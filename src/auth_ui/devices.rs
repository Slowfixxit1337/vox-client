use super::*;
use crate::auth_net::{DeviceSnapshot, TrustState};

#[derive(Default)]
pub(super) struct DevicePanel {
    snapshot: Option<DeviceSnapshot>,
    selected: usize,
    error: Option<String>,
    confirm_revoke: bool,
}

pub(super) enum DeviceAction {
    Refresh,
    Verify([u8; 16], String),
    Revoke([u8; 16]),
}

impl AuthView {
    pub(super) fn load_devices(
        &mut self,
        action: DeviceAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Screen::Devices(session, panel) = &mut self.screen else {
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        panel.error = None;
        panel.confirm_revoke = false;
        let session = session.clone();
        let client = self.client.clone();
        let task = self.runtime.spawn(async move {
            match action {
                DeviceAction::Refresh => {
                    client
                        .device_catalog(&session.username, &session.token)
                        .await
                }
                DeviceAction::Verify(id, fingerprint) => {
                    client
                        .verify_device(&session.username, &session.token, id, &fingerprint)
                        .await
                }
                DeviceAction::Revoke(id) => {
                    client
                        .revoke_device(&session.username, &session.token, id)
                        .await
                }
            }
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.busy = false;
                let Screen::Devices(_, panel) = &mut view.screen else {
                    return;
                };
                match result {
                    Ok(Ok(snapshot)) => {
                        let previous = panel
                            .snapshot
                            .as_ref()
                            .and_then(|old| old.entries.get(panel.selected))
                            .map(|entry| entry.id);
                        panel.selected = previous
                            .and_then(|id| snapshot.entries.iter().position(|entry| entry.id == id))
                            .unwrap_or(0);
                        panel.snapshot = Some(snapshot);
                        view.device_fingerprint
                            .update(cx, |value, cx| value.set_value("", window, cx));
                    }
                    Ok(Err(error)) => {
                        panel.snapshot = None;
                        panel.error = Some(error.to_string());
                    }
                    Err(_) => {
                        panel.snapshot = None;
                        panel.error = Some("Could not load Vox devices.".into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn select_device(&mut self, next: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Screen::Devices(_, panel) = &mut self.screen else {
            return;
        };
        let count = panel
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.entries.len());
        panel.selected = if next {
            (panel.selected + 1).min(count.saturating_sub(1))
        } else {
            panel.selected.saturating_sub(1)
        };
        panel.confirm_revoke = false;
        self.device_fingerprint
            .update(cx, |value, cx| value.set_value("", window, cx));
        cx.notify();
    }

    pub(super) fn device_panel(&self, panel: &DevicePanel, cx: &mut Context<Self>) -> AnyElement {
        let mut body = div().mt(px(18.)).flex().flex_col().gap(px(10.));
        let selected = panel
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.entries.get(panel.selected));
        if let Some(entry) = selected {
            let id = entry.id;
            let state = entry.state;
            let count = panel
                .snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.entries.len());
            let label = match state {
                TrustState::Local => "This PC",
                TrustState::Unverified => "Unverified device",
                TrustState::Verified => "Fingerprint verified on this PC",
                TrustState::Changed => "Key changed. Verification blocked.",
                TrustState::Revoked => "Device revoked",
                TrustState::Missing => "Verified device missing from the directory",
            };
            body = body
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            link("devices-prev", "Previous", self.busy || panel.selected == 0)
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.select_device(false, window, cx)
                                })),
                        )
                        .child(format!("{} / {}", panel.selected + 1, count))
                        .child(
                            link(
                                "devices-next",
                                "Next",
                                self.busy || panel.selected + 1 >= count,
                            )
                            .on_click(cx.listener(
                                |view, _, window, cx| view.select_device(true, window, cx),
                            )),
                        ),
                )
                .child(div().text_center().child(label))
                .child(
                    div()
                        .text_center()
                        .font_family("Consolas")
                        .child(format!("ID {}", hex::encode(id))),
                );
            match state {
                TrustState::Local => {
                    let (first, second) = entry.fingerprint.split_at(35);
                    let fingerprint = entry.fingerprint.clone();
                    body = body
                        .child(
                            div()
                                .font_family("Consolas")
                                .text_center()
                                .child(div().child(first.to_owned()))
                                .child(div().child(second.trim_start().to_owned())),
                        )
                        .child(
                            link("devices-copy", "Copy this PC’s fingerprint", self.busy).on_click(
                                cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                        fingerprint.clone(),
                                    ));
                                }),
                            ),
                        );
                }
                TrustState::Unverified => {
                    body = body
                        .child(div().text_size(px(13.)).child(
                            "Get the fingerprint directly from the other device, outside Vox.",
                        ))
                        .child(field(
                            &self.device_fingerprint,
                            "Independently verified fingerprint",
                            InputContentType::Password,
                            PASSWORD_ICON,
                            self.busy,
                        ))
                        .child(
                            link("devices-verify", "Verify fingerprint", self.busy).on_click(
                                cx.listener(move |view, _, window, cx| {
                                    let supplied =
                                        view.device_fingerprint.read(cx).value().to_string();
                                    view.load_devices(
                                        DeviceAction::Verify(id, supplied),
                                        window,
                                        cx,
                                    );
                                }),
                            ),
                        );
                }
                _ => {}
            }
            if matches!(
                state,
                TrustState::Unverified | TrustState::Verified | TrustState::Changed
            ) {
                body = body.child(
                    link(
                        "devices-revoke",
                        if panel.confirm_revoke {
                            "Confirm revocation"
                        } else {
                            "Revoke this device"
                        },
                        self.busy,
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        if let Screen::Devices(_, panel) = &mut view.screen {
                            if panel.confirm_revoke {
                                view.load_devices(DeviceAction::Revoke(id), window, cx);
                            } else {
                                panel.confirm_revoke = true;
                                cx.notify();
                            }
                        }
                    })),
                );
            }
        } else if self.busy {
            body = body.child("Loading devices…");
        } else if let Some(error) = &panel.error {
            body = body.child(div().text_color(rgb(ERROR)).child(error.clone()));
        } else {
            body = body.child("No devices in the account directory.");
        }
        let footer = div()
            .flex()
            .justify_between()
            .child(
                link("devices-back", "Back to account", self.busy).on_click(cx.listener(
                    |view, _, window, cx| {
                        if let Screen::Devices(session, _) = &view.screen {
                            view.switch(Screen::AccountSettings(session.clone()), window, cx);
                        }
                    },
                )),
            )
            .child(
                link("devices-refresh", "Refresh", self.busy).on_click(cx.listener(
                    |view, _, window, cx| view.load_devices(DeviceAction::Refresh, window, cx),
                )),
            );
        self.layout(
            "Devices",
            LOGO,
            body.into_any_element(),
            footer.into_any_element(),
            None,
        )
    }
}
