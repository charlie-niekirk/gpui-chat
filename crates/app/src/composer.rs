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
//! * **reply mode** (PR 3.4, [`crate::reply`]): choosing Reply on a message shows a strip above the
//!   box naming it, with a cancel button (and Esc); the next message goes out as a reply to it and
//!   the mode ends once it is on its way (a failed send puts both the text and the mode back);
//! * while nobody is logged in (or the login lacks the permission to chat) the box is disabled and
//!   says why, with a button to log in.
//!
//! Autocomplete (`:` / `@` / `/` / Tab) and the emote picker (PR 3.3) live in [`emotes`], with the
//! lists in `crate::completion` and the picker in `crate::emote_picker`.
//!
//! Only the live source has composers: the offline sources have nobody to send to.

use std::sync::Arc;

use gpui_chat_core::{
    ChannelId, ChannelRole, CommandInfo, Input as Parsed, InputHistory, Message, ModCommand,
    Outgoing, ReplyRef, command_hint, parse_input, sanitize_line,
};
use gpui_chat_twitch::SendError;
use gpui_chat_twitch::auth::AuthState;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{
    AppContext as _, Bounds, ClipboardItem, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, SharedString, Styled, Subscription, Window, div, px,
};
use tokio::sync::oneshot;

mod emotes;

use crate::account::AccountHub;
use crate::actions::{COMPOSER_CONTEXT, ComposerHistoryNext, ComposerHistoryPrevious};
use crate::channels::ChannelStore;
use crate::completion::{
    Autocomplete, ComposerComplete, ComposerCompletePrevious, ComposerCompletionDismiss,
    ComposerToggleEmotePicker,
};
use crate::emote_picker::EmotePicker;
use crate::emote_sources::EmoteSources;
use crate::live::{LiveSource, SendRequest};
use crate::moderation::{ModRequest, Moderator};
use crate::reply::{ReplyTarget, restored};
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
    /// Send `outgoing` to the room, as a reply when `reply` is set.
    Send {
        room_id: String,
        outgoing: Outgoing,
        reply: Option<ReplyRef>,
    },
    /// Run a moderation command in the room (PR 5.2).
    Moderate {
        room_id: String,
        command: ModCommand,
    },
}

/// The command name as typed (`ban` for `/Ban bob`), for the sentence refusing it.
fn typed_name(text: &str) -> String {
    text.trim_start()
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase()
}

/// Decides what to do with `text` typed in `channel`, whose Twitch room id is `room_id` once its
/// `ROOMSTATE` has arrived, by a user who is `role` there.
pub fn plan_submit(
    text: &str,
    channel: &ChannelId,
    room_id: Option<&str>,
    role: ChannelRole,
    reply: Option<&ReplyTarget>,
) -> Plan {
    let not_connected = || {
        Plan::Refuse(format!(
            "Not connected to #{} yet. Try again in a moment.",
            channel.login()
        ))
    };
    match parse_input(text) {
        Parsed::Empty => Plan::Nothing,
        Parsed::Invalid(error) => Plan::Refuse(error.to_string()),
        Parsed::Command(command) => {
            let allowed = if command.needs_broadcaster() {
                role.is_broadcaster()
            } else {
                role.can_moderate()
            };
            let name = typed_name(text);
            if !allowed {
                return Plan::Refuse(if command.needs_broadcaster() {
                    format!(
                        "Only the broadcaster can use /{name} in #{}.",
                        channel.login()
                    )
                } else {
                    format!(
                        "You need to be a moderator of #{} to use /{name}.",
                        channel.login()
                    )
                });
            }
            match room_id {
                Some(room_id) => Plan::Moderate {
                    room_id: room_id.to_owned(),
                    command,
                },
                None => not_connected(),
            }
        }
        Parsed::Send(outgoing) => match room_id {
            Some(room_id) => Plan::Send {
                room_id: room_id.to_owned(),
                outgoing,
                reply: reply.map(|target| target.reference.clone()),
            },
            None => not_connected(),
        },
    }
}

/// The command whose usage to show under the box for `text`: the one it starts with, when the
/// user may use it.
pub fn hint_for(text: &str, role: ChannelRole) -> Option<&'static CommandInfo> {
    command_hint(text).filter(|command| command.role.allows(role))
}

/// What Esc does in the box, in this order: close the completion list if one is open, else leave
/// reply mode, else let the key go on (to the input's own handling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    CloseList,
    CancelReply,
    Pass,
}

impl Escape {
    pub fn of(list_open: bool, replying: bool) -> Self {
        if list_open {
            Self::CloseList
        } else if replying {
            Self::CancelReply
        } else {
            Self::Pass
        }
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
    /// The message the next send replies to, while in reply mode.
    reply: Option<ReplyTarget>,
    /// A Reply that arrived from outside the composer, started by the next render (which has the
    /// window the box needs to take the text and focus).
    queued_reply: Option<Arc<Message>>,
    /// The reason the last send failed, until the next edit, send or dismissal.
    error: Option<SharedString>,
    /// The mode the box was last set up for (placeholder), so render only touches it on a change.
    shown: Option<Mode>,
    /// The `:` / `@` / `/` / Tab completion list (PR 3.3).
    completion: Autocomplete,
    /// The user's own Twitch emotes and the merged emote catalog (PR 3.3).
    emotes: EmoteSources,
    /// The open emote picker, if any (PR 3.3).
    picker: Option<Entity<EmotePicker>>,
    /// Where the picker's button is, so a press on it does not dismiss the picker as an outside click.
    picker_button_bounds: Rc<Cell<Bounds<Pixels>>>,
    _subscriptions: Vec<Subscription>,
    _input_subscription: Option<Subscription>,
    _picker_subscription: Option<Subscription>,
}

impl Composer {
    pub fn new(
        store: Entity<ChannelStore>,
        live: LiveSource,
        account: Entity<AccountHub>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The login state decides whether the box is usable.
        let subscriptions = vec![
            cx.observe(&account, |_, _, cx| cx.notify()),
            // The emote button's label names its shortcut: follow a keymap reload.
            cx.observe_global::<crate::keymap::KeymapState>(|_, cx| cx.notify()),
        ];
        Self {
            store,
            live,
            account,
            input: None,
            history: InputHistory::new(),
            reply: None,
            queued_reply: None,
            error: None,
            shown: None,
            completion: Autocomplete::default(),
            emotes: EmoteSources::default(),
            picker: None,
            picker_button_bounds: Rc::default(),
            _subscriptions: subscriptions,
            _input_subscription: None,
            _picker_subscription: None,
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
                    this.refresh_completion(false, window, cx);
                    if this.error.take().is_some() {
                        cx.notify();
                    }
                }
                InputEvent::Blur => this.input_blurred(cx),
                InputEvent::Focus => {}
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
        // Enter takes the highlighted candidate once the user has chosen from the list (moved in it,
        // or asked for it with Tab); a list that just opened as they typed does not capture it.
        if self.completion.engaged() {
            self.accept_completion(None, window, cx);
            return;
        }
        let typed = input.read(cx).value();
        let (channel, room_id, role) = {
            let store = self.store.read(cx);
            (
                store.channel().clone(),
                store.room().room_id.clone(),
                store.role(),
            )
        };
        match plan_submit(
            &typed,
            &channel,
            room_id.as_deref(),
            role,
            self.reply.as_ref(),
        ) {
            Plan::Nothing => {}
            Plan::Moderate { room_id, command } => {
                let request = ModRequest {
                    channel,
                    room_id,
                    command,
                    target_id: None,
                };
                let Some(result) = Moderator::Live(self.live.clone()).run(request, cx) else {
                    return;
                };
                self.history.push(&typed);
                input.update(cx, |input, cx| input.set_value("", window, cx));
                self.completion.close();
                self.error = None;
                cx.notify();
                self.await_command(typed.to_string(), result, window, cx);
            }
            Plan::Refuse(reason) => {
                self.error = Some(reason.into());
                cx.notify();
            }
            Plan::Send {
                room_id,
                outgoing,
                reply,
            } => {
                let request = SendRequest {
                    channel,
                    room_id,
                    outgoing,
                    reply,
                };
                let Some(result) = self.live.send(request, cx) else {
                    return;
                };
                self.history.push(&typed);
                input.update(cx, |input, cx| input.set_value("", window, cx));
                // `set_value` emits no change, so a list over the old text is closed here.
                self.completion.close();
                self.error = None;
                // Reply mode ends with the send; a failure brings it back.
                let reply = self.reply.take();
                cx.notify();
                self.await_result(typed.to_string(), reply, result, window, cx);
            }
        }
    }

    /// Waits for the outcome of a send and shows a failure.
    fn await_result(
        &mut self,
        typed: String,
        reply: Option<ReplyTarget>,
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
                this.send_failed(&typed, reply, &error, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Waits for the outcome of a moderation command; a refusal shows under the box and the text
    /// comes back for editing, as for a message that was not sent.
    fn await_command(
        &mut self,
        typed: String,
        result: oneshot::Receiver<Result<(), String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Err(reason)) = result.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.error = Some(reason.into());
                if let Some(input) = &this.input {
                    input.update(cx, |input, cx| {
                        if input.value().is_empty() {
                            input.set_value(typed, window, cx);
                        }
                    });
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn send_failed(
        &mut self,
        typed: &str,
        reply: Option<ReplyTarget>,
        error: &SendError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(%error, "message not sent");
        self.error = Some(error.to_string().into());
        // Still a reply, unless the user already chose another message to reply to.
        self.reply = restored(self.reply.take(), reply);
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

    /// Gives the input box keyboard focus (creating it if it was never drawn); `false` when it
    /// cannot take text (logged out), and so is left alone.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.mode(cx).can_send() {
            return false;
        }
        let input = self.input(window, cx);
        input.update(cx, |input, cx| input.focus(window, cx));
        true
    }

    /// Reply was chosen on `parent` in the chat above: start it on the next render.
    pub fn queue_reply(&mut self, parent: Arc<Message>, cx: &mut Context<Self>) {
        self.queued_reply = Some(parent);
        cx.notify();
    }

    /// Puts the box in reply mode for `parent` (Reply on a row, or the context menu): shows the
    /// strip and focuses the box. The text is left alone (Twitch puts the `@parent ` mention in
    /// front of the reply itself). Does nothing for a line that cannot be replied to.
    pub fn start_reply(
        &mut self,
        parent: &Arc<Message>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = ReplyTarget::new(parent) else {
            return;
        };
        let input = self.input(window, cx);
        input.update(cx, |input, cx| input.focus(window, cx));
        self.reply = Some(target);
        self.history.reset();
        cx.notify();
    }

    /// Leaves reply mode; what the user typed stays. `false` when there was no reply to cancel.
    fn cancel_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.reply.take().is_none() {
            return false;
        }
        if let Some(input) = &self.input {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
        true
    }

    /// The strip naming the message being replied to, with its cancel button.
    fn reply_strip(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let label = self.reply.as_ref()?.label();
        Some(
            h_flex()
                .id("composer-reply")
                .w_full()
                .flex_none()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .bg(tokens.accent.opacity(0.12))
                .border_t_1()
                .border_color(tokens.accent.opacity(0.5))
                .child(
                    Icon::new(gpui_kit::assets::IconName::Undo2)
                        .size_4()
                        .text_color(tokens.accent_link),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(tokens.text_muted)
                        .child(label),
                )
                .child(
                    Button::new("composer-reply-cancel")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Close))
                        .tooltip("Cancel reply (Esc)")
                        .accessibility_label("Cancel reply")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cancel_reply(window, cx);
                        })),
                ),
        )
    }

    /// ↑
    fn history_previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // With the completion list open the arrows choose from it.
        if self.move_completion(-1, cx) {
            return;
        }
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
        if self.move_completion(1, cx) {
            return;
        }
        let Some(input) = self.input.clone() else {
            return;
        };
        if let Some(entry) = self.history.newer().map(str::to_owned) {
            input.update(cx, |input, cx| input.set_value(entry, window, cx));
        }
    }

    /// The usage of the command being typed (`/timeout <user> [duration] [reason]`) under the box,
    /// once its name is complete and the user may run it (PR 5.2).
    fn hint_strip(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let typed = self.input.as_ref()?.read(cx).value();
        let command = hint_for(&typed, self.store.read(cx).role())?;
        Some(
            h_flex()
                .id("composer-hint")
                .w_full()
                .flex_none()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_color(tokens.border)
                .text_sm()
                .child(
                    div()
                        .flex_none()
                        .text_color(tokens.text)
                        .child(command.usage),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(tokens.text_muted)
                        .child(command.summary),
                ),
        )
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
                        .text_color(tokens.danger_text)
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
        if let Some(parent) = self.queued_reply.take() {
            self.start_reply(&parent, window, cx);
        }
        let mode = self.mode(cx);
        if !mode.can_send() && self.picker.is_some() {
            self.close_picker(window, cx);
        }
        self.request_own_emotes(window, cx);
        self.sync_completion(cx);
        let overlays = self.overlays(&tokens, window, cx);
        let emotes_button = self.picker_button(cx);
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
                        // Replaces the selection, like a plain paste does.
                        input.replace(flat, window, cx);
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
            // Tab and Shift-Tab only act while there is something to complete; otherwise they
            // propagate, so the input's own handling still sees the key. Esc is one chain: close
            // the completion list, else cancel reply mode, else propagate.
            .on_action(cx.listener(|this, _: &ComposerComplete, window, cx| {
                if !this.complete(window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &ComposerCompletePrevious, _, cx| {
                if !this.complete_previous(cx) {
                    cx.propagate();
                }
            }))
            .on_action(
                cx.listener(|this, _: &ComposerCompletionDismiss, window, cx| {
                    match Escape::of(this.completion_is_open(), this.reply.is_some()) {
                        Escape::CloseList => {
                            this.dismiss_completion(cx);
                        }
                        Escape::CancelReply => {
                            this.cancel_reply(window, cx);
                        }
                        Escape::Pass => cx.propagate(),
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &ComposerToggleEmotePicker, window, cx| {
                    this.toggle_picker(window, cx);
                }),
            )
            .w_full()
            .flex_none()
            .border_t_1()
            .border_color(tokens.border)
            .children(self.error_strip(&tokens, cx))
            .children(self.hint_strip(&tokens, cx))
            // The completion list / emote picker hang off this zero-height line, above the reply
            // strip and the box.
            .child(div().w_full().h(px(0.0)).children(overlays))
            .children(self.reply_strip(&tokens, cx))
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .min_h(px(44.0))
                    .child(div().flex_1().min_w_0().child(field))
                    .children(emotes_button)
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
            plan_submit(
                "hello",
                &channel,
                Some("22484632"),
                ChannelRole::Viewer,
                None
            ),
            Plan::Send {
                room_id: "22484632".into(),
                outgoing: Outgoing {
                    text: "hello".into(),
                    action: false
                },
                reply: None,
            }
        );
        let Plan::Send { outgoing, .. } =
            plan_submit("/me waves", &channel, Some("1"), ChannelRole::Viewer, None)
        else {
            panic!("an action is sent")
        };
        assert!(outgoing.action);
        assert_eq!(outgoing.wire_text(), "/me waves");
    }

    #[test]
    fn a_message_typed_in_reply_mode_is_sent_as_a_reply_to_that_message() {
        use gpui_chat_core::{Author, Fragment, Message, Timestamp};
        let channel = ChannelId::new("forsen");
        let parent = Message::chat(
            "parent-1",
            Timestamp(0),
            Author::new("alice", "Alice"),
            vec![Fragment::Text("what is the game".into())],
        );
        let target = ReplyTarget::new(&parent).unwrap();
        let Plan::Send {
            outgoing, reply, ..
        } = plan_submit(
            "chess",
            &channel,
            Some("1"),
            ChannelRole::Viewer,
            Some(&target),
        )
        else {
            panic!("a reply is sent")
        };
        assert_eq!(outgoing.text, "chess");
        let reply = reply.expect("the request carries the reply");
        assert_eq!(reply.parent_id.as_str(), "parent-1");
        assert_eq!(reply.parent_body, "what is the game");
        // Without reply mode there is none, and a refused message carries nothing.
        let Plan::Send { reply, .. } =
            plan_submit("chess", &channel, Some("1"), ChannelRole::Viewer, None)
        else {
            panic!()
        };
        assert_eq!(reply, None);
        assert!(matches!(
            plan_submit(
                "/ban x",
                &channel,
                Some("1"),
                ChannelRole::Viewer,
                Some(&target)
            ),
            Plan::Refuse(_)
        ));
    }

    #[test]
    fn enter_on_a_blank_box_does_nothing() {
        let channel = ChannelId::new("forsen");
        assert_eq!(
            plan_submit("   ", &channel, Some("1"), ChannelRole::Viewer, None),
            Plan::Nothing
        );
        assert_eq!(
            plan_submit("", &channel, None, ChannelRole::Viewer, None),
            Plan::Nothing
        );
    }

    #[test]
    fn a_channel_that_is_not_joined_yet_is_not_sent_to() {
        let channel = ChannelId::new("forsen");
        let Plan::Refuse(reason) = plan_submit("hello", &channel, None, ChannelRole::Viewer, None)
        else {
            panic!("no room id, no send")
        };
        assert!(reason.contains("#forsen"), "{reason}");
    }

    #[test]
    fn bad_input_is_refused_with_the_reason() {
        let channel = ChannelId::new("forsen");
        let Plan::Refuse(reason) =
            plan_submit("/w x", &channel, Some("1"), ChannelRole::Viewer, None)
        else {
            panic!("unknown command")
        };
        assert!(reason.contains("Unknown command /w"), "{reason}");
        let long = "a".repeat(501);
        let Plan::Refuse(reason) =
            plan_submit(&long, &channel, Some("1"), ChannelRole::Viewer, None)
        else {
            panic!("too long")
        };
        assert!(reason.contains("501/500"), "{reason}");
    }

    #[test]
    fn moderation_commands_run_only_for_those_who_may_use_them() {
        let channel = ChannelId::new("forsen");
        let ban = || ModCommand::Ban {
            user: "bob".into(),
            reason: None,
        };
        let run = |text, role| plan_submit(text, &channel, Some("22"), role, None);
        // A viewer is told why, in terms of the channel and the command they typed.
        let Plan::Refuse(reason) = run("/Ban bob", ChannelRole::Viewer) else {
            panic!("a viewer cannot ban")
        };
        assert_eq!(reason, "You need to be a moderator of #forsen to use /ban.");
        assert_eq!(
            run("/ban bob", ChannelRole::Moderator),
            Plan::Moderate {
                room_id: "22".into(),
                command: ban()
            }
        );
        assert!(matches!(
            run("/ban bob", ChannelRole::Broadcaster),
            Plan::Moderate { .. }
        ));
        // Mods and raids are the broadcaster's.
        let Plan::Refuse(reason) = run("/mod bob", ChannelRole::Moderator) else {
            panic!("a moderator cannot make moderators")
        };
        assert_eq!(reason, "Only the broadcaster can use /mod in #forsen.");
        assert!(matches!(
            run("/mod bob", ChannelRole::Broadcaster),
            Plan::Moderate { .. }
        ));
        // Bad arguments are the parser's sentence, whoever types them.
        let Plan::Refuse(reason) = run("/timeout", ChannelRole::Moderator) else {
            panic!("a timeout needs a user")
        };
        assert!(reason.starts_with("Usage: /timeout"), "{reason}");
        // Not joined yet: no room to act in.
        let Plan::Refuse(reason) =
            plan_submit("/ban bob", &channel, None, ChannelRole::Moderator, None)
        else {
            panic!("no room id")
        };
        assert!(reason.contains("#forsen"), "{reason}");
    }

    #[test]
    fn the_usage_shows_once_the_command_is_named_and_allowed() {
        assert_eq!(
            hint_for("/timeout ", ChannelRole::Moderator).map(|c| c.name),
            Some("timeout")
        );
        assert!(hint_for("/timeout ", ChannelRole::Viewer).is_none());
        assert!(hint_for("/timeout", ChannelRole::Moderator).is_none());
        assert_eq!(
            hint_for("/me ", ChannelRole::Viewer).map(|c| c.name),
            Some("me")
        );
        assert!(hint_for("hello /ban x", ChannelRole::Moderator).is_none());
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
        let Plan::Send { outgoing, .. } =
            plan_submit(&flat, &channel, Some("1"), ChannelRole::Viewer, None)
        else {
            panic!("one message")
        };
        assert_eq!(outgoing.text, "a b c");
    }
}
