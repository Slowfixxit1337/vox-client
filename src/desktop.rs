use gpui_kit::{
    Context, IntoElement, Render, TitlebarOptions, Window, WindowOptions, div, prelude::*,
};

pub fn show(authority: &str, status: String) {
    let title = format!("Vox DEV | {authority}");
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
            gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| StatusView(status)))
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
