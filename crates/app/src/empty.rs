//! Empty states and the first-run welcome (PR 6.3).
//!
//! Every "there is nothing here yet" in the app is drawn by [`empty_state`] (or, for a single line
//! inside a list, [`empty_line`]) with the same wording rules: a short title saying what is empty,
//! then one line saying what makes something appear or what to do. Colours are theme tokens only:
//! `text_muted` is checked at 4.5:1 on every surface of every bundled theme.
//!
//! [`welcome`] is the first-run view of the main pane: what the app is, and the three things to
//! do first.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AnyElement, ClickEvent, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Role, SharedString, StatefulInteractiveElement, Styled, div, px,
};

use crate::problems::Callback;
use crate::theme::Tokens;

/// A centred empty state: `title`, and under it `hint` (what to do next), if any.
pub fn empty_state(
    id: &'static str,
    title: impl Into<SharedString>,
    hint: Option<SharedString>,
    tokens: &Tokens,
) -> AnyElement {
    let title: SharedString = title.into();
    v_flex()
        .id(id)
        .role(Role::Status)
        .aria_label(title.clone())
        .items_center()
        .justify_center()
        .gap_1()
        .p_3()
        .text_color(tokens.text_muted)
        .child(div().text_sm().child(title))
        .children(hint.map(|hint| div().text_xs().max_w(px(420.0)).text_center().child(hint)))
        .into_any_element()
}

/// What the three welcome buttons do. The shell implements them (it owns the dialogs and windows).
pub struct WelcomeActions {
    pub add_channel: Callback,
    /// `None` when there is nothing to log in to (already logged in, or an offline source).
    pub log_in: Option<Callback>,
    pub choose_theme: Callback,
}

/// The first-run view: the app in a few lines, then *Add a channel*, *Log in to Twitch* and
/// *Choose a theme*. `add_focus` is the first button's focus handle, so the shell can put keyboard
/// focus on it (Tab reaches the others; Enter or Space presses).
///
/// There are no suggested channels: "who is live" needs a Twitch token even for an app, and the
/// first run has none (chat itself is read anonymously), so a list would have to be made up.
pub fn welcome(
    app_name: &str,
    shortcut_hint: String,
    add_shortcut: Option<String>,
    add_focus: &FocusHandle,
    actions: WelcomeActions,
    tokens: &Tokens,
) -> AnyElement {
    let WelcomeActions {
        add_channel,
        log_in,
        choose_theme,
    } = actions;
    let add_label = match add_shortcut {
        Some(keys) => format!("Add a channel  {keys}"),
        None => "Add a channel".to_owned(),
    };
    let logged_out = log_in.is_some();
    let mut buttons = h_flex().gap_2().flex_wrap().justify_center().child(
        Button::new("welcome-add-channel")
            .primary()
            .label(add_label)
            .track_focus(add_focus)
            .on_click(move |_: &ClickEvent, window, cx| add_channel(window, cx)),
    );
    if let Some(log_in) = log_in {
        buttons = buttons.child(
            Button::new("welcome-log-in")
                .outline()
                .label("Log in to Twitch")
                .on_click(move |_: &ClickEvent, window, cx| log_in(window, cx)),
        );
    }
    buttons = buttons.child(
        Button::new("welcome-choose-theme")
            .outline()
            .label("Choose a theme")
            .on_click(move |_: &ClickEvent, window, cx| choose_theme(window, cx)),
    );
    let muted = tokens.text_muted;
    v_flex()
        .id("welcome")
        .role(Role::Group)
        .aria_label(SharedString::from(format!("Welcome to {app_name}")))
        .size_full()
        .items_center()
        .justify_center()
        .gap_4()
        .p_6()
        .child(
            v_flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .child(format!("Welcome to {app_name}")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("A fast, native Twitch chat client."),
                ),
        )
        .child(
            v_flex()
                .items_center()
                .gap_1()
                .max_w(px(460.0))
                .text_sm()
                .text_center()
                .child(
                    div().child(
                        "Add a channel to read its chat. You do not need an account for that.",
                    ),
                )
                .child(div().text_color(muted).child(if logged_out {
                    "Log in only if you want to send messages, see who you follow, or moderate."
                } else {
                    "You are logged in, so you can send messages too."
                })),
        )
        .child(buttons)
        .children(
            (!shortcut_hint.is_empty())
                .then(|| div().text_xs().text_color(muted).child(shortcut_hint)),
        )
        .into_any_element()
}
