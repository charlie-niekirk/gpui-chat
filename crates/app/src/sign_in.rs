//! The login sheet (PR 3.1): Twitch's device-code flow as a Kit dialog.
//!
//! Opening it starts a sign-in on the [`AccountHub`]; the sheet shows the user code with a copy
//! button and "Open browser" (Twitch's activation link, which carries the code), and waits.
//! Approving on Twitch closes it; closing it any other way (Cancel, Escape, the close button)
//! cancels the sign-in. An expired or declined code shows why, with "Try again".

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, FontWeight, IntoElement, ParentElement,
    Render, Styled, Subscription, Window, div, px,
};

use crate::account::{AccountHub, SignIn};

/// What the sheet shows for the activation link (the real link carries the code as well).
const ACTIVATE_LABEL: &str = "twitch.tv/activate";
/// Opened instead of a `verification_uri` that is not an `https` Twitch link.
const ACTIVATE_FALLBACK: &str = "https://www.twitch.tv/activate";

/// The link "Open browser" opens: Twitch's `verification_uri` when it is `https` on `twitch.tv` or
/// a subdomain of it (no user info, no port), else the plain activation page. The browser is never
/// sent anywhere a response could redirect it to.
fn activation_link(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("https://") else {
        return ACTIVATE_FALLBACK.to_owned();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.to_ascii_lowercase();
    let is_twitch = host == "twitch.tv" || host.ends_with(".twitch.tv");
    let clean = !authority.contains(['@', ':', '\\']) && !uri.contains(char::is_whitespace);
    if is_twitch && clean {
        uri.to_owned()
    } else {
        ACTIVATE_FALLBACK.to_owned()
    }
}

struct SignInSheet {
    hub: Entity<AccountHub>,
    /// The code currently on the clipboard, to say "Copied".
    copied: Option<String>,
    /// The dialog was closed by us (on success); never close twice (that would close what is below).
    closed: bool,
    _observe: Subscription,
}

impl SignInSheet {
    fn new(hub: Entity<AccountHub>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe_in(&hub, window, |sheet, hub, window, cx| {
            if !sheet.closed && hub.read(cx).sign_in() == Some(&SignIn::Done) {
                sheet.closed = true;
                window.close_dialog(cx);
            }
            cx.notify();
        });
        // However the dialog goes away, a sign-in still waiting for approval stops with it.
        let weak = hub.downgrade();
        cx.on_release(move |_, cx| {
            weak.update(cx, |hub, cx| hub.cancel_sign_in(cx)).ok();
        })
        .detach();
        Self {
            hub,
            copied: None,
            closed: false,
            _observe: observe,
        }
    }

    fn waiting(&self, code: &str, link: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let copied = self.copied.as_deref() == Some(code);
        let (copy_code, open_link) = (code.to_owned(), activation_link(link));
        v_flex()
            .gap_3()
            .child(div().text_sm().child(format!(
                "Open {ACTIVATE_LABEL} in a browser and enter this code:"
            )))
            .child(
                div()
                    .w_full()
                    .py_3()
                    .flex()
                    .justify_center()
                    .rounded(px(6.))
                    .bg(cx.theme().muted)
                    .text_size(px(28.))
                    .font_weight(FontWeight::BOLD)
                    .child(code.to_owned()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("sign-in-copy")
                            .small()
                            .icon(Icon::new(if copied {
                                IconName::Check
                            } else {
                                IconName::Copy
                            }))
                            .label(if copied { "Copied" } else { "Copy code" })
                            .on_click(cx.listener(move |sheet, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_code.clone()));
                                sheet.copied = Some(copy_code.clone());
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("sign-in-open")
                            .small()
                            .primary()
                            .icon(Icon::new(IconName::ExternalLink))
                            .label("Open browser")
                            .on_click(move |_, _, cx| cx.open_url(&open_link)),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Waiting for you to approve the login on Twitch…"),
            )
    }

    fn failed(&self, message: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let danger = cx.theme().danger;
        let hub = self.hub.clone();
        v_flex()
            .gap_3()
            .child(div().text_sm().text_color(danger).child(message.to_owned()))
            .child(
                h_flex().child(
                    Button::new("sign-in-retry")
                        .small()
                        .primary()
                        .label("Try again")
                        .on_click(move |_, _, cx| {
                            hub.update(cx, |hub, cx| hub.begin_sign_in(cx));
                        }),
                ),
            )
    }
}

impl Render for SignInSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = self.hub.read(cx).sign_in().cloned();
        let muted = cx.theme().muted_foreground;
        let body = match progress {
            Some(SignIn::Waiting(code)) => self
                .waiting(&code.user_code, &code.verification_uri, cx)
                .into_any_element(),
            Some(SignIn::Failed(message)) => self.failed(&message, cx).into_any_element(),
            Some(SignIn::Done) => div().text_sm().child("Logged in.").into_any_element(),
            Some(SignIn::Requesting) | None => div()
                .text_sm()
                .text_color(muted)
                .child("Asking Twitch for a login code…")
                .into_any_element(),
        };
        v_flex().gap_2().child(body)
    }
}

/// Opens the login sheet and starts a sign-in. Does nothing while another dialog is open.
pub fn open(hub: Entity<AccountHub>, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    hub.update(cx, |hub, cx| hub.begin_sign_in(cx));
    let sheet = cx.new(|cx| SignInSheet::new(hub, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Log in to Twitch")
            .w(px(420.))
            .child(sheet.clone())
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().trigger(|button| button.label("Cancel"))),
            )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_twitch_links_are_opened() {
        for good in [
            "https://www.twitch.tv/activate?public=true&device-code=ABCDEFGH",
            "https://twitch.tv/activate",
            "https://WWW.Twitch.TV/activate",
            "https://id.twitch.tv/device",
        ] {
            assert_eq!(activation_link(good), good);
        }
        for bad in [
            "http://www.twitch.tv/activate",
            "https://evil.example/activate",
            "https://twitch.tv.evil.example/activate",
            "https://eviltwitch.tv/activate",
            "https://www.twitch.tv@evil.example/activate",
            "https://www.twitch.tv:8443/activate",
            "https://www.twitch.tv\\@evil.example/",
            "//www.twitch.tv/activate",
            "javascript:alert(1)",
            "https://www.twitch.tv/activate x",
            "",
        ] {
            assert_eq!(activation_link(bad), ACTIVATE_FALLBACK, "{bad:?}");
        }
    }
}
