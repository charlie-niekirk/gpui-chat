//! `gpui-chat` binary: the only crate that depends on GPUI (via GPUI Kit).
//!
//! GPUI Kit API usage here was verified against the pinned `gpui-kit 0.9.1`
//! source (`~/.cargo/registry/src/*/gpui-kit-0.9.1/src/lib.rs`).

use gpui_chat_core::{APP_ID, APP_NAME};
use gpui_kit::*;

/// Root view of the hello-world window.
struct Hello;

impl Render for Hello {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(div().text_xl().child(format!("Hello from {APP_NAME}")))
            .child(div().text_sm().child(APP_ID))
    }
}

fn main() {
    gpui_kit::application().run(|cx: &mut App| {
        gpui_kit::init(cx);

        // Quit when the last window closes (the app has no other UI yet).
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = WindowBounds::centered(size(px(900.0), px(600.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(bounds),
            titlebar: Some(TitlebarOptions {
                title: Some(APP_NAME.into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| Hello))
            .expect("failed to open the main window");
        cx.activate(true);
    });
}
