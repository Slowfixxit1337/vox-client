use gpui_kit::{
    Context, IntoElement, Render, TitlebarOptions, Window, WindowOptions, div, prelude::*,
};

pub fn show(authority: &str, status: String) {
    show_view(format!("Vox DEV | {authority}"), StatusView(status));
}

fn show_view<V: Render + 'static>(title: String, view: V) {
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        if let Err(error) = gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| view)) {
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
    show_view("Vox DEV | Предпросмотр".into(), preview::Preview::default());
}

#[cfg(feature = "lab")]
mod preview {
    use crate::window_capture::{self, Source};
    use anyhow::Context as _;
    use gpui_kit::{
        Context, IntoElement, ObjectFit, Render, RenderImage, Task, Window,
        component::{Disableable, button::Button},
        div, img,
        prelude::*,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::Arc;
    use windows::Win32::Foundation::HWND;

    #[derive(Default)]
    pub(super) struct Preview {
        image: Option<Arc<RenderImage>>,
        task: Option<Task<()>>,
        choosing: bool,
        monitors: Vec<String>,
        status: String,
    }

    enum Selection {
        Window { owner: HWND, borderless: bool },
        Monitor(String),
    }

    impl Preview {
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
                        let pixels =
                            image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra)
                                .context("неверный размер кадра")?;
                        let image = Arc::new(RenderImage::new([image::Frame::new(pixels)]));
                        view.update_in(cx, |view, window, cx| {
                            view.status = format!("{label} · {} × {}", frame.width, frame.height);
                            if let Some(old) = view.image.replace(image) {
                                let _ = window.drop_image(old);
                            }
                            cx.notify();
                        })?;
                    }
                    Ok::<_, anyhow::Error>("Захват завершён")
                }
                .await;
                let _ = view.update_in(cx, |view, window, cx| {
                    view.clear_image(window);
                    view.choosing = false;
                    view.status = match result {
                        Ok(message) => message.into(),
                        Err(error) => format!("Не удалось захватить источник. {error}"),
                    };
                    view.task = None;
                    cx.notify();
                });
            }));
            cx.notify();
        }

        fn clear_image(&mut self, window: &mut Window) {
            if let Some(image) = self.image.take() {
                let _ = window.drop_image(image);
            }
        }
    }

    impl Render for Preview {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let active = self.task.is_some();
            div()
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
                                view.task = None;
                                view.clear_image(window);
                                view.status = "Захват остановлен".into();
                                cx.notify();
                            })),
                    )
                })
                .when(!active, |element| {
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
                })
        }
    }
}
