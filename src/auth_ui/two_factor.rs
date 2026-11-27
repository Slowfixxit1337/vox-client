use super::*;

impl AuthView {
    pub(super) fn submit_two_factor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (request, operation) = match &self.screen {
            Screen::FactorSettings(session) => {
                let password = self.current_password.read(cx).value().to_string();
                if password.is_empty() {
                    self.status = Some("Enter your current password.");
                    cx.notify();
                    return;
                }
                (
                    Request::BeginTwoFactor,
                    Operation::BeginTwoFactor {
                        session: session.token.clone(),
                        password,
                    },
                )
            }
            Screen::FactorSetup(..) | Screen::FactorSignIn(_) => {
                let code = self.factor_code.read(cx).value().trim().to_owned();
                let authenticator_code =
                    code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit());
                let is_sign_in = matches!(self.screen, Screen::FactorSignIn(_));
                let backup_code = is_sign_in
                    && code.len() == 20
                    && code.bytes().all(|byte| byte.is_ascii_hexdigit());
                if !authenticator_code && !backup_code {
                    self.status = Some(if is_sign_in {
                        "Enter an authenticator code or a backup code."
                    } else {
                        "Enter the six-digit authenticator code."
                    });
                    cx.notify();
                    return;
                }
                match &self.screen {
                    Screen::FactorSetup(session, _) => (
                        Request::ConfirmTwoFactor,
                        Operation::ConfirmTwoFactor {
                            session: session.token.clone(),
                            code,
                        },
                    ),
                    Screen::FactorSignIn(challenge) => (
                        Request::CompleteTwoFactor,
                        Operation::CompleteTwoFactorSignIn {
                            challenge: challenge.clone(),
                            code,
                        },
                    ),
                    _ => return,
                }
            }
            _ => return,
        };
        self.send(request, operation, window, cx);
    }

    pub(super) fn render_two_factor(&self, cx: &mut Context<Self>) -> AnyElement {
        let input = || {
            field(
                &self.factor_code,
                "Security code",
                InputContentType::Password,
                PASSWORD_ICON,
                self.busy,
            )
        };
        let (title, hint, label, body) = match &self.screen {
            Screen::FactorSettings(_) => (
                "Two-factor authentication",
                "Confirm your password to start setup.",
                "Set up authenticator",
                div().child(field(
                    &self.current_password,
                    "Current password",
                    InputContentType::Password,
                    PASSWORD_ICON,
                    self.busy,
                )),
            ),
            Screen::FactorSetup(_, secret) => (
                "Connect authenticator",
                "Add a time-based account in your authenticator and enter this key.",
                "Enable 2FA",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(
                        div()
                            .font_family("Consolas")
                            .text_center()
                            .child(secret.clone()),
                    )
                    .child(
                        link("copy-authenticator-key", "Copy setup key", self.busy).on_click(
                            cx.listener(|view, _, _, cx| {
                                if let Screen::FactorSetup(_, secret) = &view.screen {
                                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                        secret.clone(),
                                    ));
                                }
                            }),
                        ),
                    )
                    .child(input()),
            ),
            Screen::FactorBackupCodes(_, codes) => (
                "Backup codes",
                "Save these codes now. Each can be used once.",
                "Copy backup codes",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .text_center()
                    .font_family("Consolas")
                    .children(codes.iter().map(|code| div().child(code.clone()))),
            ),
            Screen::FactorSignIn(_) => (
                "Two-factor sign-in",
                "Use your authenticator app or a saved backup code.",
                "Sign in",
                div().child(input()),
            ),
            _ => return div().into_any_element(),
        };
        let footer = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(12.))
            .child(
                link("two-factor-action", label, self.busy).on_click(cx.listener(
                    |view, _, window, cx| {
                        if let Screen::FactorBackupCodes(_, codes) = &view.screen {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                codes.join("\n"),
                            ));
                            view.status = Some("Backup codes copied.");
                            cx.notify();
                        } else {
                            view.submit(window, cx);
                        }
                    },
                )),
            )
            .child(
                link(
                    "two-factor-back",
                    if matches!(self.screen, Screen::FactorBackupCodes(..)) {
                        "Done"
                    } else {
                        "Back"
                    },
                    self.busy,
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    let screen = match &view.screen {
                        Screen::FactorSignIn(_) => Screen::SignIn,
                        Screen::FactorSetup(session, _) => Screen::FactorSettings(session.clone()),
                        Screen::FactorSettings(session) | Screen::FactorBackupCodes(session, _) => {
                            Screen::AccountSettings(session.clone())
                        }
                        _ => return,
                    };
                    view.switch(screen, window, cx);
                })),
            );
        self.layout(
            title,
            LOGO,
            body.mt(px(22.)).into_any_element(),
            footer.into_any_element(),
            Some(hint.into()),
        )
    }
}
