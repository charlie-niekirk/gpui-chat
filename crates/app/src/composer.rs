//! The input box under a pane's chat (PR 3.2): type, Enter to send, ↑/↓ to recall what was sent.
//!
//! The rules live in `gpui_chat_core::compose` (command parsing, the length limit, the history) and
//! in [`plan_submit`] / [`Mode`] here, so they are unit-tested; [`Composer`] is the view over them:
//!
//! * a **single-line** Kit input, so Enter always sends and a pasted block can never send several
//!   messages: a multi-line paste is flattened to one line (line breaks → spaces) and waits for
//!   Enter like anything else typed;
//! * sending goes through [`LiveSource::send`] (Helix *Send Chat Message*). The box is cleared at
//!   once and the line is remembered in the history; if Twitch refuses or drops the message, the
//!   reason appears in a strip above the box and the text comes back (unless the user already
//!   typed something new; ↑ finds it either way);
//! * a successful send is echoed into the chat by the live source;
//! * while nobody is logged in (or the login lacks the permission to chat) the box is disabled and
//!   says why, with a button to log in.
//!
//! Only the live source has composers: the offline sources have nobody to send to.

use gpui_chat_core::{
    ChannelId, Input as Parsed, InputHistory, Outgoing, parse_input, sanitize_line,
};
use gpui_chat_twitch::SendError;
use gpui_chat_twitch::auth::AuthState;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Window, div, px,
};
use tokio::sync::oneshot;

use crate::account::AccountHub;
use crate::actions::{COMPOSER_CONTEXT, ComposerHistoryNext, ComposerHistoryPrevious};
use crate::channels::ChannelStore;
use crate::live::{LiveSource, SendRequest};
use crate::sign_in;
use crate::theme::Tokens;

/// The scope Send Chat Message needs.
const WRITE_SCOPE: &str = "user:write:chat";

/// Whether (and why not) the box accepts a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The stored login is still being read.
    Restoring,
    LoggedOut,
    /// Logged in, but the login predates `user:write:chat`.
    NeedsRelogin,
    Ready,
}

impl Mode {
    pub fn of(state: &AuthState) -> Self {
        match state {
            AuthState::Restoring => Self::Restoring,
            AuthState::LoggedOut { .. } => Self::LoggedOut,
            AuthState::LoggedIn(account)
                if account
                    .missing_scopes
                    .iter()
                    .any(|scope| scope == WRITE_SCOPE) =>
            {
                Self::NeedsRelogin
            }
            AuthState::LoggedIn(_) => Self::Ready,
        }
    }

    pub fn can_send(self) -> bool {
        self == Self::Ready
    }

    /// What the empty box says.
    pub fn placeholder(self) -> &'static str {
        match self {
            Self::Restoring => "Checking your Twitch login…",
            Self::LoggedOut => "Log in to chat",
            Self::NeedsRelogin => "Log in again to chat",
            Self::Ready => "Send a message",
        }
    }

    /// The label of the log-in button, when logging in is the way forward.
    fn action(self) -> Option<&'static str> {
        match self {
            Self::LoggedOut => Some("Log in"),
            Self::NeedsRelogin => Some("Log in again"),
            Self::Restoring | Self::Ready => None,
        }
    }
}

/// What pressing Enter should do with the box's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Blank: nothing happens (the box keeps what it has).
    Nothing,
    /// Not sent; the reason goes in the error strip and the text stays.
    Refuse(String),
    /// Send `outgoing` to the room.
    Send { room_id: String, outgoing: Outgoing },
}

/// Decides what to do with `text` typed in `channel`, whose Twitch room id is `room_id` once its
/// `ROOMSTATE` has arrived.
pub fn plan_submit(text: &str, channel: &ChannelId, room_id: Option<&str>) -> Plan {
    match parse_input(text) {
        Parsed::Empty => Plan::Nothing,
        Parsed::Invalid(error) => Plan::Refuse(error.to_string()),
        Parsed::Send(outgoing) => match room_id {
            Some(room_id) => Plan::Send {
                room_id: room_id.to_owned(),
                outgoing,
            },
            None => Plan::Refuse(format!(
                "Not connected to #{} yet. Try again in a moment.",
                channel.login()
            )),
        },
    }
}

/// A multi-line clipboard as the single line the box takes; `None` when the text needs no change
/// (the box pastes it itself).
pub fn flatten_paste(text: &str) -> Option<String> {
    match sanitize_line(text) {
        std::borrow::Cow::Borrowed(_) => None,
        std::borrow::Cow::Owned(flat) => Some(flat),
    }
}

/// The input box of one pane.
pub struct Composer {
    store: Entity<ChannelStore>,
    live: LiveSource,
    account: Entity<AccountHub>,
    /// Created by the first render (it needs the window).
    input: Option<Entity<InputState>>,
    history: InputHistory,
    /// The reason the last send failed, until the next edit, send or dismissal.
    error: Option<SharedString>,
    /// The mode the box was last set up for (placeholder), so render only touches it on a change.
    shown: Option<Mode>,
    _subscriptions: Vec<Subscription>,
    _input_subscription: Option<Subscription>,
}

impl Composer {
    pub fn new(
        store: Entity<ChannelStore>,
        live: LiveSource,
        account: Entity<AccountHub>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The login state decides whether the box is usable.
        let subscriptions = vec![cx.observe(&account, |_, _, cx| cx.notify())];
        Self {
            store,
            live,
            account,
            input: None,
            history: InputHistory::new(),
            error: None,
            shown: None,
            _subscriptions: subscriptions,
            _input_subscription: None,
        }
    }

    fn mode(&self, cx: &gpui_kit::App) -> Mode {
        Mode::of(self.account.read(cx).state())
    }

    /// The input, created (and given focus) on first use.
    fn input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = &self.input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        self._input_subscription = Some(cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                // Edited by the user (programmatic `set_value` does not emit): the recalled entry
                // is now the user's own text, and an old error no longer applies.
                InputEvent::Change => {
                    this.history.reset();
                    if this.error.take().is_some() {
                        cx.notify();
                    }
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        ));
        if self.mode(cx).can_send() {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        self.input = Some(input.clone());
        input
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.input.clone() else {
            return;
        };
        if !self.mode(cx).can_send() {
            return;
        }
        let typed = input.read(cx).value();
        let (channel, room_id) = {
            let store = self.store.read(cx);
            (store.channel().clone(), store.room().room_id.clone())
        };
        match plan_submit(&typed, &channel, room_id.as_deref()) {
            Plan::Nothing => {}
            Plan::Refuse(reason) => {
                self.error = Some(reason.into());
                cx.notify();
            }
            Plan::Send { room_id, outgoing } => {
                let request = SendRequest {
                    channel,
                    room_id,
                    outgoing,
                };
                let Some(result) = self.live.send(request, cx) else {
                    return;
                };
                self.history.push(&typed);
                input.update(cx, |input, cx| input.set_value("", window, cx));
                self.error = None;
                cx.notify();
                self.await_result(typed.to_string(), result, window, cx);
            }
        }
    }

    /// Waits for the outcome of a send and shows a failure.
    fn await_result(
        &mut self,
        typed: String,
        result: oneshot::Receiver<Result<(), SendError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            // A closed channel means the runtime is stopping; nothing to show.
            let Ok(Err(error)) = result.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.send_failed(&typed, &error, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn send_failed(
        &mut self,
        typed: &str,
        error: &SendError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(%error, "message not sent");
        self.error = Some(error.to_string().into());
        // Give the text back for editing, unless the user has already started something else.
        if let Some(input) = &self.input {
            input.update(cx, |input, cx| {
                if input.value().is_empty() {
                    input.set_value(typed, window, cx);
                }
            });
        }
        cx.notify();
    }

    /// ↑
    fn history_previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.input.clone() else {
            return;
        };
        let current = input.read(cx).value();
        if let Some(entry) = self.history.older(&current).map(str::to_owned) {
            input.update(cx, |input, cx| input.set_value(entry, window, cx));
        }
    }

    /// ↓
    fn history_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.input.clone() else {
            return;
        };
        if let Some(entry) = self.history.newer().map(str::to_owned) {
            input.update(cx, |input, cx| input.set_value(entry, window, cx));
        }
    }

    fn error_strip(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let error = self.error.clone()?;
        Some(
            h_flex()
                .id("composer-error")
                .w_full()
                .flex_none()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .bg(tokens.danger.opacity(0.14))
                .border_t_1()
                .border_color(tokens.danger)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(tokens.danger)
                        .child(error),
                )
                .child(
                    Button::new("composer-error-dismiss")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Close))
                        .accessibility_label("Dismiss error")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                ),
        )
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        let input = self.input(window, cx);
        let mode = self.mode(cx);
        if self.shown != Some(mode) {
            self.shown = Some(mode);
            input.update(cx, |input, cx| {
                input.set_placeholder(mode.placeholder(), window, cx);
            });
        }
        let editor = input.clone();
        let field = Input::new(&input).disabled(!mode.can_send()).on_paste(
            move |clipboard: &ClipboardItem, window, cx| {
                // A multi-line paste becomes one line instead of being glued together (the box's
                // own handling drops the line breaks, fusing the last word of a line with the
                // first of the next).
                let Some(flat) = clipboard.text().as_deref().and_then(flatten_paste) else {
                    return false;
                };
                editor.update(cx, |input, cx| {
                    if input.is_editable() {
                        input.insert(flat, window, cx);
                    }
                });
                true
            },
        );
        let login = mode.action().map(|label| {
            let account = self.account.clone();
            Button::new("composer-log-in")
                .small()
                .label(label)
                .on_click(move |_, window, cx| sign_in::open(account.clone(), window, cx))
        });

        v_flex()
            .id("composer")
            .key_context(COMPOSER_CONTEXT)
            .on_action(
                cx.listener(|this, _: &ComposerHistoryPrevious, window, cx| {
                    this.history_previous(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &ComposerHistoryNext, window, cx| {
                this.history_next(window, cx);
            }))
            .w_full()
            .flex_none()
            .border_t_1()
            .border_color(tokens.border)
            .children(self.error_strip(&tokens, cx))
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .min_h(px(44.0))
                    .child(div().flex_1().min_w_0().child(field))
                    .children(login),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_twitch::auth::Account;

    use super::*;

    fn account(missing: &[&str]) -> AuthState {
        AuthState::LoggedIn(Account {
            login: "viewer".into(),
            user_id: "42".into(),
            missing_scopes: missing.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    #[test]
    fn the_box_is_usable_only_with_a_login_that_may_chat() {
        assert_eq!(Mode::of(&AuthState::Restoring), Mode::Restoring);
        assert_eq!(
            Mode::of(&AuthState::LoggedOut { notice: None }),
            Mode::LoggedOut
        );
        assert_eq!(Mode::of(&account(&[])), Mode::Ready);
        assert_eq!(Mode::of(&account(&["user:write:chat"])), Mode::NeedsRelogin);
        // Missing some other permission does not stop chatting.
        assert_eq!(Mode::of(&account(&["user:read:follows"])), Mode::Ready);
        assert!(Mode::Ready.can_send());
        for mode in [Mode::Restoring, Mode::LoggedOut, Mode::NeedsRelogin] {
            assert!(!mode.can_send());
        }
    }

    #[test]
    fn logging_in_is_offered_when_it_is_the_way_forward() {
        assert_eq!(Mode::LoggedOut.action(), Some("Log in"));
        assert_eq!(Mode::NeedsRelogin.action(), Some("Log in again"));
        assert_eq!(Mode::Ready.action(), None);
        assert_eq!(Mode::Restoring.action(), None);
        assert_eq!(Mode::LoggedOut.placeholder(), "Log in to chat");
    }

    #[test]
    fn enter_sends_to_the_room_once_it_is_known() {
        let channel = ChannelId::new("forsen");
        assert_eq!(
            plan_submit("hello", &channel, Some("22484632")),
            Plan::Send {
                room_id: "22484632".into(),
                outgoing: Outgoing {
                    text: "hello".into(),
                    action: false
                }
            }
        );
        let Plan::Send { outgoing, .. } = plan_submit("/me waves", &channel, Some("1")) else {
            panic!("an action is sent")
        };
        assert!(outgoing.action);
        assert_eq!(outgoing.wire_text(), "/me waves");
    }

    #[test]
    fn enter_on_a_blank_box_does_nothing() {
        let channel = ChannelId::new("forsen");
        assert_eq!(plan_submit("   ", &channel, Some("1")), Plan::Nothing);
        assert_eq!(plan_submit("", &channel, None), Plan::Nothing);
    }

    #[test]
    fn a_channel_that_is_not_joined_yet_is_not_sent_to() {
        let channel = ChannelId::new("forsen");
        let Plan::Refuse(reason) = plan_submit("hello", &channel, None) else {
            panic!("no room id, no send")
        };
        assert!(reason.contains("#forsen"), "{reason}");
    }

    #[test]
    fn bad_input_is_refused_with_the_reason() {
        let channel = ChannelId::new("forsen");
        let Plan::Refuse(reason) = plan_submit("/ban x", &channel, Some("1")) else {
            panic!("unknown command")
        };
        assert!(reason.contains("Unknown command /ban"), "{reason}");
        let long = "a".repeat(501);
        let Plan::Refuse(reason) = plan_submit(&long, &channel, Some("1")) else {
            panic!("too long")
        };
        assert!(reason.contains("501/500"), "{reason}");
    }

    #[test]
    fn a_multi_line_paste_is_flattened_and_never_sends() {
        assert_eq!(flatten_paste("one line"), None);
        assert_eq!(
            flatten_paste("first\nsecond\r\nthird").as_deref(),
            Some("first second third")
        );
        // Flattened, it is one message for the user to confirm.
        let channel = ChannelId::new("forsen");
        let flat = flatten_paste("a\nb\nc").unwrap();
        let Plan::Send { outgoing, .. } = plan_submit(&flat, &channel, Some("1")) else {
            panic!("one message")
        };
        assert_eq!(outgoing.text, "a b c");
    }
}
