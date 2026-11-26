use gpui_kit::{
    Context, IntoElement, Render, TitlebarOptions, Window, WindowOptions, div, prelude::*,
};

pub fn show(authority: &str, status: String) {
    show_view(format!("Vox DEV | {authority}"), move |_, _| {
        StatusView(status)
    });
}

fn show_view<V: Render + 'static>(
    title: String,
    view: impl FnOnce(&mut Window, &mut Context<V>) -> V + 'static,
) {
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        if let Err(error) =
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| view(window, cx)))
        {
            eprintln!("Не удалось открыть окно Vox: {error}");
            cx.quit();
        }
    });
}

struct StatusView(String);

impl Render for StatusView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().p_4().child(self.0.clone())
    }
}

#[cfg(feature = "lab")]
pub fn capture_preview() {
    show_view("Vox DEV | Предпросмотр".into(), |_, _| {
        preview::Preview::default()
    });
}

#[cfg(all(feature = "webrtc", feature = "lab"))]
pub fn show_share(
    authority: &str,
    feed: crate::video::share::DesktopFeed,
    audio: Option<crate::game_audio::Controls>,
) {
    show_view(
        format!("Vox DEV | Демонстрация | {authority}"),
        move |window, cx| preview::Preview::sharing(window, cx, feed, audio),
    );
}

#[cfg(feature = "lab")]
mod preview {
    #[cfg(feature = "webrtc")]
    use crate::video::share::DesktopFeed;
    use crate::window_capture::{self, Source};
    #[cfg(feature = "webrtc")]
    use crate::{audio_targets, game_audio};
    use anyhow::Context as _;
    use gpui_kit::{
        Context, IntoElement, ObjectFit, Render, RenderImage, Task, Window,
        component::{Disableable, button::Button},
        div, img,
        prelude::*,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::Arc;
    #[cfg(feature = "webrtc")]
    use tokio::sync::watch;
    use windows::Win32::Foundation::HWND;

    #[derive(Default)]
    pub(super) struct Preview {
        image: Option<Arc<RenderImage>>,
        task: Option<Task<()>>,
        choosing: bool,
        monitors: Vec<String>,
        status: String,
        #[cfg(feature = "webrtc")]
        peer: Option<PeerPreview>,
        #[cfg(feature = "webrtc")]
        audio: Option<GameAudioPreview>,
    }

    #[cfg(feature = "webrtc")]
    struct PeerPreview {
        capture: watch::Sender<Option<Arc<window_capture::Frame>>>,
        image: Option<Arc<RenderImage>>,
        disconnected: bool,
        _viewer: Task<()>,
    }

    #[cfg(feature = "webrtc")]
    struct GameAudioPreview {
        target: watch::Sender<Option<u32>>,
        listen: watch::Sender<bool>,
        status: String,
        choices: Vec<audio_targets::Application>,
        error: Option<String>,
        _observer: Task<()>,
    }

    enum Selection {
        Window { owner: HWND, borderless: bool },
        Monitor(String),
    }

    impl Preview {
        #[cfg(feature = "webrtc")]
        pub(super) fn sharing(
            window: &Window,
            cx: &mut Context<Self>,
            feed: DesktopFeed,
            audio: Option<game_audio::Controls>,
        ) -> Self {
            let DesktopFeed {
                capture,
                mut received,
            } = feed;
            let viewer = cx.spawn_in(window, async move |view, cx| {
                while received.changed().await.is_ok() {
                    let frame = received.borrow_and_update().clone();
                    let image = frame.and_then(|frame| {
                        render_image(frame.width, frame.height, frame.bgra.clone())
                    });
                    if view
                        .update_in(cx, |view, window, cx| {
                            if let Some(peer) = view.peer.as_mut() {
                                replace_image(&mut peer.image, image, window);
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = view.update_in(cx, |view, window, cx| {
                    view.stop_capture(window);
                    view.choosing = false;
                    view.monitors.clear();
                    view.status.clear();
                    if let Some(peer) = view.peer.as_mut() {
                        peer.disconnected = true;
                        replace_image(&mut peer.image, None, window);
                    }
                    cx.notify();
                });
            });
            let audio = audio.map(
                |game_audio::Controls {
                     target,
                     listen,
                     mut status,
                 }| {
                    let initial = status.borrow_and_update().clone();
                    let observer = cx.spawn_in(window, async move |view, cx| {
                        while status.changed().await.is_ok() {
                            let text = status.borrow_and_update().clone();
                            if view
                                .update_in(cx, |view, _, cx| {
                                    if let Some(audio) = &mut view.audio {
                                        audio.status = text;
                                        cx.notify();
                                    }
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                    });
                    GameAudioPreview {
                        target,
                        listen,
                        status: initial,
                        choices: vec![],
                        error: None,
                        _observer: observer,
                    }
                },
            );
            Self {
                peer: Some(PeerPreview {
                    capture,
                    image: None,
                    disconnected: false,
                    _viewer: viewer,
                }),
                audio,
                ..Self::default()
            }
        }

        fn start_window(&mut self, borderless: bool, window: &mut Window, cx: &mut Context<Self>) {
            let hwnd = match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
                Ok(RawWindowHandle::Win32(handle)) => HWND(handle.hwnd.get() as _),
                _ => {
                    self.status = "Не удалось получить дескриптор окна Vox".into();
                    cx.notify();
                    return;
                }
            };
            self.choosing = true;
            self.status = "Выберите окно в системном диалоге".into();
            self.start(
                Selection::Window {
                    owner: hwnd,
                    borderless,
                },
                window,
                cx,
            );
        }

        fn start(&mut self, selection: Selection, window: &Window, cx: &mut Context<Self>) {
            self.monitors.clear();
            #[cfg(feature = "webrtc")]
            let outgoing = self.peer.as_ref().map(|peer| peer.capture.clone());
            self.task = Some(cx.spawn_in(window, async move |view, cx| {
                let result = async {
                    let (source, label) = match selection {
                        Selection::Window { owner, borderless } => {
                            if borderless {
                                window_capture::request_borderless_window().await?;
                            }
                            let Some(item) = window_capture::pick(owner).await? else {
                                return Ok("Выбор отменён");
                            };
                            (Source::Window { item, borderless }, "Захват окна")
                        }
                        Selection::Monitor(name) => {
                            (Source::Monitor(name), "Захват всего монитора")
                        }
                    };
                    let frames = window_capture::frames(source)?;
                    view.update_in(cx, |view, _, cx| {
                        view.choosing = false;
                        view.status = format!("{label} · ожидаем кадр");
                        cx.notify();
                    })?;
                    while let Ok(frame) = frames.recv().await {
                        let frame = frame?;
                        let (width, height) = (frame.width, frame.height);
                        #[cfg(feature = "webrtc")]
                        let pixels = if let Some(outgoing) = &outgoing {
                            let pixels = frame.bgra.clone();
                            outgoing.send_replace(Some(Arc::new(frame)));
                            pixels
                        } else {
                            frame.bgra
                        };
                        #[cfg(not(feature = "webrtc"))]
                        let pixels = frame.bgra;
                        let image =
                            render_image(width, height, pixels).context("неверный размер кадра")?;
                        view.update_in(cx, |view, window, cx| {
                            view.status = format!("{label} · {width} × {height}");
                            replace_image(&mut view.image, Some(image), window);
                            cx.notify();
                        })?;
                    }
                    Ok::<_, anyhow::Error>("Захват завершён")
                }
                .await;
                let _ = view.update_in(cx, |view, window, cx| {
                    view.stop_capture(window);
                    view.choosing = false;
                    view.status = match result {
                        Ok(message) => message.into(),
                        Err(error) => format!("Не удалось захватить источник. {error}"),
                    };
                    cx.notify();
                });
            }));
            cx.notify();
        }

        fn clear_image(&mut self, window: &mut Window) {
            replace_image(&mut self.image, None, window);
        }

        fn stop_capture(&mut self, window: &mut Window) {
            self.task = None;
            self.clear_image(window);
            #[cfg(feature = "webrtc")]
            if let Some(peer) = &self.peer {
                peer.capture.send_replace(None);
            }
            #[cfg(feature = "webrtc")]
            if let Some(audio) = &mut self.audio {
                audio.target.send_replace(None);
                audio.choices.clear();
                audio.error = None;
            }
        }

        #[cfg(feature = "webrtc")]
        fn render_audio(&self, audio: &GameAudioPreview, cx: &Context<Self>) -> impl IntoElement {
            let sharing = self.task.is_some() && self.image.is_some();
            let listening = *audio.listen.borrow();
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .when(sharing, |row| {
                            row.child(
                                Button::new("game-audio-source")
                                    .label("Приложение для звука")
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        if let Some(audio) = &mut view.audio {
                                            audio.choices.clear();
                                            audio.error = match audio_targets::applications() {
                                                Ok(apps) if apps.is_empty() => Some(
                                                    "Приложения с видимым окном не найдены".into(),
                                                ),
                                                Ok(apps) => {
                                                    audio.choices = apps;
                                                    None
                                                }
                                                Err(error) => Some(format!(
                                                    "Не удалось получить список приложений. {error}"
                                                )),
                                            };
                                            cx.notify();
                                        }
                                    })),
                            )
                            .when(
                                audio.target.borrow().is_some(),
                                |row| {
                                    row.child(
                                        Button::new("game-audio-stop")
                                            .label("Показ без звука")
                                            .on_click(cx.listener(|view, _, _, cx| {
                                                if let Some(audio) = &mut view.audio {
                                                    audio.target.send_replace(None);
                                                    audio.choices.clear();
                                                    cx.notify();
                                                }
                                            })),
                                    )
                                },
                            )
                        })
                        .child(
                            Button::new("game-audio-listen")
                                .label(if listening {
                                    "Заглушить звук демки"
                                } else {
                                    "Слушать звук демки"
                                })
                                .on_click(cx.listener(|view, _, _, cx| {
                                    if let Some(audio) = &view.audio {
                                        audio.listen.send_modify(|enabled| *enabled = !*enabled);
                                        cx.notify();
                                    }
                                })),
                        ),
                )
                .when(sharing, |column| column.child(audio.status.clone()))
                .when_some(audio.error.clone(), |column, error| column.child(error))
                .children(audio.choices.iter().map(|app| {
                    let pid = app.pid;
                    Button::new(("game-audio-app", pid as usize))
                        .label(app.label.clone())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if view.task.is_some()
                                && view.image.is_some()
                                && let Some(audio) = &mut view.audio
                            {
                                audio.target.send_replace(Some(pid));
                                audio.choices.clear();
                                audio.error = None;
                                cx.notify();
                            }
                        }))
                }))
        }
    }

    fn render_image(width: u32, height: u32, bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
        let pixels = image::RgbaImage::from_raw(width, height, bgra)?;
        Some(Arc::new(RenderImage::new([image::Frame::new(pixels)])))
    }

    fn replace_image(
        slot: &mut Option<Arc<RenderImage>>,
        frame: Option<Arc<RenderImage>>,
        window: &mut Window,
    ) {
        if let Some(image) = std::mem::replace(slot, frame) {
            let _ = window.drop_image(image);
        }
    }

    impl Render for Preview {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let active = self.task.is_some();
            #[cfg(feature = "webrtc")]
            let controls = self.peer.as_ref().is_none_or(|peer| !peer.disconnected);
            #[cfg(not(feature = "webrtc"))]
            let controls = true;
            let view = div()
                .size_full()
                .p_4()
                .flex()
                .flex_col()
                .gap_3()
                .when(active, |element| {
                    element.child(
                        Button::new("capture-stop")
                            .label("Остановить")
                            .disabled(self.choosing)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.stop_capture(window);
                                view.status = "Захват остановлен".into();
                                cx.notify();
                            })),
                    )
                })
                .when(!active && controls, |element| {
                    element.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                Button::new("capture-window-borderless")
                                    .label("Окно без рамки")
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.start_window(true, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("capture-window")
                                    .label("Окно с рамкой Windows")
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.start_window(false, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("capture-monitor")
                                    .label("Монитор без рамки")
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        match window_capture::monitors() {
                                            Ok(monitors) if monitors.is_empty() => {
                                                view.status = "Активные мониторы не найдены".into()
                                            }
                                            Ok(monitors) => {
                                                view.monitors = monitors;
                                                view.status =
                                                    "Выберите монитор. Будет виден весь экран."
                                                        .into();
                                            }
                                            Err(error) => {
                                                view.status = format!(
                                                    "Не удалось получить список мониторов. {error}"
                                                )
                                            }
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                })
                .when(!self.status.is_empty(), |element| {
                    element.child(self.status.clone())
                })
                .children(
                    self.monitors
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(|(index, monitor)| {
                            Button::new(("monitor", index))
                                .label(monitor.clone())
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.start(Selection::Monitor(monitor.clone()), window, cx);
                                }))
                        }),
                )
                .when_some(self.image.clone(), |element, frame| {
                    element.child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .child(img(frame).size_full().object_fit(ObjectFit::Contain)),
                    )
                });
            #[cfg(feature = "webrtc")]
            let view = view.when_some(
                self.audio.as_ref().filter(|_| controls),
                |element, audio| element.child(self.render_audio(audio, cx)),
            );
            #[cfg(feature = "webrtc")]
            let view = view.when_some(self.peer.as_ref(), |element, peer| {
                element.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(if peer.disconnected {
                            "Соединение закрыто"
                        } else if peer.image.is_some() {
                            "Демонстрация собеседника · E2EE"
                        } else {
                            "Ожидаем демонстрацию собеседника"
                        })
                        .when_some(peer.image.clone(), |pane, frame| {
                            pane.child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .child(img(frame).size_full().object_fit(ObjectFit::Contain)),
                            )
                        }),
                )
            });
            view
        }
    }
}
