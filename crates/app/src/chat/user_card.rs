//! The user card (PR 5.4): a card over the chat, anchored where a name, a mention or the context
//! menu was clicked, with the user's avatar, names, badges in this channel, account age, how long
//! they have followed (when Twitch lets us know), what they said in this channel this session
//! (history marked) and what can be done with them.
//!
//! * **Opening.** A click on an author's name or an `@mention` (see [`super::selection_layer`]),
//!   *View user* in the right-click menu, or the `OpenUserCard` action on the selected message.
//!   It is dismissed by Esc (the chat view has focus while it is open, so Esc reaches it before
//!   the input box's own chain: close the completion list, else cancel the reply, else go on,
//!   which is what Esc does again once the card is gone) or a press anywhere outside it.
//! * **Content.** What the store knows (name, badges, messages) is shown at once and needs no
//!   login. Account details come from Helix through [`crate::user_directory`] (one lookup per user
//!   per session, off the UI thread); logged out the card says "Log in to see account details".
//! * **Actions.** Copy username, Mention, Reply to the latest message, open the twitch.tv profile
//!   (https only), and for a moderator Timeout (the presets), Ban (asks first) and Unban through
//!   the same path as the buttons on a message ([`crate::moderation`], the pane runs them): the
//!   missing-scope answer therefore ends in the same "Enable moderation tools" strip. Every action
//!   closes the card.
//!
//! [`CardModel`] is everything the card shows, decided from data (the store, the directory's
//! answer, the login state) and unit-tested; [`render`] only draws it.

use std::sync::Arc;

use gpui_chat_core::{
    BadgeId, CardActions, CardMessages, ChannelId, ChannelRole, KnownUser, MAX_CARD_MESSAGES,
    Message, MessageKind, TIMEOUT_PRESETS, card_actions, duration_words, format_age, format_date,
    format_seconds, known_user, profile_url, same_account,
};
use gpui_chat_settings::DeletedMessages;
use gpui_chat_twitch::UserProfile;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Anchor, AnyElement, ClipboardItem, Context, EntityId, InteractiveElement as _,
    IntoElement as _, MouseButton, ParentElement as _, Pixels, Point, SharedString,
    StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, anchored, canvas, deferred,
    div, point, px, uniform_list,
};

use super::display;
use super::selection_layer::Surface;
use super::view::ChatView;
use crate::emote_thumb;
use crate::images::{FrameImages, Images};
use crate::moderation::{RowAction, is_collapsed};
use crate::reply::can_reply_to;
use crate::theme::Tokens;
use crate::user_directory::{Follow, Profile};

use std::cell::RefCell;
use std::rc::Rc;

const WIDTH: f32 = 340.0;
const AVATAR: f32 = 56.0;
const BADGE: f32 = 18.0;
/// The message list scrolls beyond this height. Its rows have one fixed height (two lines of
/// text), so only the visible ones are built however long the list is.
const LIST_HEIGHT: f32 = 190.0;
const ROW_HEIGHT: f32 = 46.0;
/// Where the card sits relative to the pointer that opened it.
const OFFSET: (f32, f32) = (6.0, 10.0);

/// The open card: whose it is and where it hangs.
#[derive(Debug, Clone)]
pub struct UserCard {
    /// Lowercase login.
    pub login: Arc<str>,
    /// The Twitch id of the account whose message (name) the card was opened from: that, and not
    /// whoever owns the login now, is who moderation is aimed at. `None` for a mention, a
    /// reply line or a user with no message.
    pub origin_id: Option<Arc<str>>,
    /// Window coordinates.
    pub anchor: Point<Pixels>,
    /// Their messages in the store, kept up to date without scanning it each frame.
    pub messages: CardMessages,
}

impl UserCard {
    pub fn new(login: &str, origin_id: Option<&str>, anchor: Point<Pixels>) -> Self {
        Self {
            login: Arc::from(login),
            origin_id: origin_id.filter(|id| !id.is_empty()).map(Arc::from),
            anchor,
            messages: CardMessages::new(login),
        }
    }
}

/// What the account part of the card says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Details {
    /// Nobody is logged in: Helix cannot be asked.
    LoggedOut,
    Loading,
    /// Twitch has no account with this login (it was renamed, suspended or deleted).
    Missing,
    Failed(String),
    /// One line each: when the account was created, how long they have followed.
    Found(Vec<String>),
}

/// Everything the card shows.
#[derive(Debug, Clone)]
pub struct CardModel {
    /// Lowercase login.
    pub login: String,
    /// The name to show: Twitch's display name, else the one on their last message, else the login.
    pub title: String,
    /// The avatar's URL, when Helix gave one.
    pub avatar: Option<String>,
    pub badges: Vec<BadgeId>,
    /// Their messages in this channel, newest first.
    pub messages: Rc<Vec<(u64, Arc<Message>)>>,
    pub details: Details,
    /// The account that owns this login now is not the one that sent the messages (a rename: the
    /// old name went to someone else). The card says so, and offers no moderation.
    pub conflict: bool,
    pub actions: CardActions,
    /// The newest message that can be replied to, when replying is possible.
    pub reply_to: Option<Arc<Message>>,
    /// The id moderation is aimed at (and the follow lookup uses): the sender's, from the message
    /// the card was opened from or their newest one, else Helix's for the login.
    pub user_id: Option<String>,
}

/// What a card is built from besides the store.
#[derive(Debug, Clone, Copy)]
pub struct CardInputs<'a> {
    pub role: ChannelRole,
    pub own_login: Option<&'a str>,
    /// The pane can run moderation commands.
    pub moderation: bool,
    /// The pane has an input box to reply with.
    pub can_reply: bool,
    /// Someone is logged in (Helix can be asked).
    pub logged_in: bool,
    /// Now, in Unix seconds (core never reads the clock).
    pub now: i64,
}

/// The lines of the account part: creation date and age, follow date and age.
pub fn detail_lines(profile: &UserProfile, follow: Option<Follow>, now: i64) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(created) = profile.created_at {
        lines.push(format!(
            "Account created {} ({} old)",
            format_date(created),
            format_age(created, now)
        ));
    }
    if let Some(Follow::Since(Some(since))) = follow {
        lines.push(format!(
            "Following since {} ({})",
            format_date(since),
            format_age(since, now)
        ));
    }
    lines
}

/// What the avatar box shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarShow {
    /// Their image (a placeholder while it loads).
    Image,
    /// A circle with the name's initial: logged out (no URL), no avatar, or the image failed for
    /// good (not found, not an image, undecodable).
    Initial,
}

/// Maps what is known about the avatar to what is drawn.
pub fn avatar_show(url: Option<&str>, failed_for_good: bool) -> AvatarShow {
    match url {
        Some(_) if !failed_for_good => AvatarShow::Image,
        _ => AvatarShow::Initial,
    }
}

/// The first character of `name`, uppercased, for the avatar placeholder.
pub fn initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default()
}

/// The header of the message list: `N messages this session`.
pub fn messages_title(count: usize) -> String {
    match count {
        0 => "No messages this session".to_owned(),
        1 => "1 message this session".to_owned(),
        n if n >= MAX_CARD_MESSAGES => format!("Latest {MAX_CARD_MESSAGES} messages this session"),
        n => format!("{n} messages this session"),
    }
}

impl CardModel {
    /// Builds the card for `login`: `messages` are theirs (newest first,
    /// [`CardMessages::update`]), `origin_id` the id of the sender of the message the card was
    /// opened from, `profile` / `follow` the directory's answers and `inputs` the rest.
    pub fn build(
        login: &str,
        origin_id: Option<&str>,
        messages: Vec<(u64, Arc<Message>)>,
        channel: &ChannelId,
        profile: Option<&Profile>,
        follow: &dyn Fn(&str) -> Option<Follow>,
        inputs: CardInputs<'_>,
    ) -> Self {
        let known = known_user(&messages);
        let found = match profile {
            Some(Profile::Found(profile)) => Some(profile),
            _ => None,
        };
        // Who sent the messages: the message the card was opened from, else their newest.
        let sender_id = origin_id.or(known.user_id.as_deref());
        let conflict = found.is_some_and(|profile| !same_account(sender_id, &profile.id));
        let user_id = sender_id
            .map(str::to_owned)
            .or_else(|| found.map(|profile| profile.id.clone()));
        let details = match profile {
            Some(Profile::Found(profile)) => {
                let follow = user_id.as_deref().filter(|_| !conflict).and_then(follow);
                Details::Found(detail_lines(profile, follow, inputs.now))
            }
            Some(Profile::Missing) => Details::Missing,
            Some(Profile::Failed(why)) => Details::Failed(why.clone()),
            Some(Profile::Loading) => Details::Loading,
            // Not asked: logged out, or about to be asked by the view.
            None if inputs.logged_in => Details::Loading,
            None => Details::LoggedOut,
        };
        let actions = if inputs.moderation && !conflict {
            card_actions(inputs.role, channel, inputs.own_login, login, &known)
        } else {
            CardActions::default()
        };
        Self {
            login: login.to_ascii_lowercase(),
            title: title(login, found.map(|p| p.display_name.as_str()), &known),
            avatar: found.and_then(|profile| profile.avatar_url.clone()),
            badges: known.badges.clone(),
            reply_to: inputs
                .can_reply
                .then(|| {
                    messages
                        .iter()
                        .map(|(_, message)| message)
                        .find(|message| can_reply_to(message))
                        .cloned()
                })
                .flatten(),
            messages: Rc::new(messages),
            details,
            conflict,
            actions,
            user_id,
        }
    }

    /// The follow lookup to start: only a moderator of the channel may read a user's follow, and
    /// both ids are needed.
    pub fn wants_follow(
        &self,
        role: ChannelRole,
        room_id: Option<&str>,
    ) -> Option<(String, String)> {
        if !role.can_moderate() || self.conflict || !matches!(self.details, Details::Found(_)) {
            return None;
        }
        Some((room_id?.to_owned(), self.user_id.clone()?))
    }
}

fn title(login: &str, display_name: Option<&str>, known: &KnownUser) -> String {
    display_name
        .filter(|name| !name.trim().is_empty())
        .or(known.display_name.as_deref())
        .map_or_else(|| login.to_owned(), str::to_owned)
}

/// How a message in the list reads: its text, or a stand-in for a message that was removed and
/// is collapsed (`chat.deleted_messages`, as in the chat itself).
pub fn row_text(message: &Message, deleted: DeletedMessages) -> String {
    if is_collapsed(message, deleted, false) {
        return "<message deleted>".to_owned();
    }
    let text = crate::reply::single_line(&message.plain_text());
    if text.is_empty() && message.kind == MessageKind::UserNotice {
        return message.system_text.clone().unwrap_or_default();
    }
    text
}

/// What [`render`] needs from the chat view.
pub struct Env<'a> {
    pub tokens: Tokens,
    pub view: WeakEntity<ChatView>,
    pub owner: EntityId,
    pub images: Rc<RefCell<FrameImages>>,
    pub deleted: DeletedMessages,
    pub model: &'a CardModel,
    pub channel: ChannelId,
    pub anchor: Point<Pixels>,
    pub can_login: bool,
    /// The selection surfaces the card sits over: a press inside the card must not clear their
    /// selection, so they are told where the card is.
    pub surfaces: Vec<Rc<Surface>>,
}

/// The card, positioned at its anchor (and kept inside the window).
pub fn render(env: Env<'_>, window: &mut Window, cx: &mut Context<ChatView>) -> AnyElement {
    let Env {
        tokens,
        view,
        owner,
        images,
        deleted,
        model,
        channel,
        anchor,
        can_login,
        surfaces,
    } = env;
    let muted = tokens.text_muted;

    // --- header: avatar, names, close ---
    // The avatar once Helix gave its URL; before that (and logged out) a circle with the initial.
    let avatar_key = model
        .avatar
        .as_ref()
        .map(|url| gpui_chat_media::ImageKey::new(url.as_str()));
    let failed = avatar_key
        .as_ref()
        .is_some_and(|key| Images::failed_for_good(key, cx));
    let avatar = match (avatar_show(model.avatar.as_deref(), failed), avatar_key) {
        (AvatarShow::Image, Some(key)) => emote_thumb::image_with(
            Some(key),
            AVATAR,
            AVATAR / 2.0,
            tokens.border,
            images.clone(),
            owner,
            true,
        )
        .into_any_element(),
        _ => div()
            .size(px(AVATAR))
            .rounded_full()
            .bg(tokens.border)
            .flex()
            .items_center()
            .justify_center()
            .text_xl()
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .text_color(muted)
            .child(initial(&model.title))
            .into_any_element(),
    };
    let avatar = div()
        .id("user-card-avatar")
        .flex_none()
        .aria_label(format!("Avatar of {}", model.title))
        .child(avatar);
    let login_line = if model.title.eq_ignore_ascii_case(&model.login) {
        None
    } else {
        Some(format!("@{}", model.login))
    };
    let close_view = view.clone();
    let header = h_flex()
        .gap_3()
        .items_center()
        .child(avatar)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_base()
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .truncate()
                        .child(model.title.clone()),
                )
                .children(
                    login_line.map(|line| div().text_sm().text_color(muted).truncate().child(line)),
                ),
        )
        .child(
            Button::new("user-card-close")
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::Close))
                .tooltip("Close (Esc)")
                .accessibility_label("Close user card")
                .on_click(move |_, _, cx| {
                    close_view
                        .update(cx, |view, cx| view.close_user_card(cx))
                        .ok();
                }),
        );

    // --- badges in this channel ---
    let badges = badge_chips(model, &channel, &tokens, &images, owner, window, cx);

    // --- account details ---
    let details = match &model.details {
        Details::Found(lines) => v_flex()
            .gap_0p5()
            .children(lines.iter().map(|line| div().text_sm().child(line.clone())))
            .into_any_element(),
        Details::Loading => muted_line("Loading account details…", muted),
        Details::Missing => muted_line("No such account (renamed or suspended?)", muted),
        Details::Failed(why) => {
            let retry_view = view.clone();
            let login = model.login.clone();
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(tokens.danger_text)
                        .child(why.clone()),
                )
                .child(
                    Button::new("user-card-retry")
                        .ghost()
                        .xsmall()
                        .label("Retry")
                        .on_click(move |_, _, cx| {
                            retry_view
                                .update(cx, |view, cx| view.retry_user_lookup(&login, cx))
                                .ok();
                        }),
                )
                .into_any_element()
        }
        Details::LoggedOut => h_flex()
            .gap_2()
            .items_center()
            .child(muted_line("Log in to see account details", muted))
            .children(can_login.then(|| {
                Button::new("user-card-login")
                    .ghost()
                    .xsmall()
                    .label("Log in")
                    .on_click(|_, window, cx| {
                        if let Some(hub) = crate::account::current(cx) {
                            crate::sign_in::open(hub, window, cx);
                        }
                    })
            }))
            .into_any_element(),
    };

    // After a rename the login can belong to someone else: say so, and moderation is off.
    let conflict = model.conflict.then(|| {
        div()
            .text_sm()
            .text_color(tokens.danger_text)
            .child("This name now belongs to a different account than the one who sent these messages (renamed?). Moderation is off.")
    });

    // --- actions ---
    let ignoring = ignore_buttons(model, &view, cx);
    let actions = actions_row(model, &view, ignoring);
    let moderation = moderation_rows(model, &view, &tokens);

    // --- messages ---
    let list = message_list(model, deleted, &tokens, &view);

    let close_on_outside = view.clone();
    // Where the card is, for the surfaces under it: a press inside it keeps their selection.
    let place = canvas(
        move |bounds, _, _| {
            for surface in &surfaces {
                surface.set_overlay(Some(bounds));
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .size_full();
    // Below the middle of the window the card opens upward from the pointer: the newest lines
    // (the ones people click) are at the bottom, and a card over the pointer would take the second
    // press of a double-click.
    let upward = anchor.y > window.viewport_size().height / 2.0;
    let card = v_flex()
        .id("user-card")
        .relative()
        .occlude()
        .w(px(WIDTH))
        .max_w_full()
        .gap_2()
        .p_3()
        .bg(tokens.surface)
        .text_color(tokens.text)
        .border_1()
        .border_color(tokens.border)
        .rounded_lg()
        .shadow_lg()
        // A press anywhere else closes it (the press itself still reaches what it was on).
        .on_mouse_down_out(move |_, _, cx| {
            close_on_outside
                .update(cx, |view, cx| view.close_user_card(cx))
                .ok();
        })
        // A press on the card is not a press on the chat under it.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(header)
        .children(badges)
        .child(details)
        .children(conflict)
        .child(actions)
        .children(moderation)
        .child(list)
        .child(place);

    deferred(
        anchored()
            .anchor(if upward {
                Anchor::BottomLeft
            } else {
                Anchor::TopLeft
            })
            .position(anchor)
            .offset(point(
                px(OFFSET.0),
                px(if upward { -OFFSET.1 } else { OFFSET.1 }),
            ))
            .child(card),
    )
    .with_priority(2)
    .into_any_element()
}

fn muted_line(text: &str, color: gpui_kit::Hsla) -> AnyElement {
    div()
        .text_sm()
        .text_color(color)
        .child(text.to_owned())
        .into_any_element()
}

/// The badges the user has in this channel, each its image and title.
fn badge_chips(
    model: &CardModel,
    channel: &ChannelId,
    tokens: &Tokens,
    images: &Rc<RefCell<FrameImages>>,
    owner: EntityId,
    window: &Window,
    cx: &mut Context<ChatView>,
) -> Option<AnyElement> {
    if model.badges.is_empty() {
        return None;
    }
    let registry = cx
        .try_global::<crate::badges::Badges>()
        .map(|badges| badges.registry().clone());
    let scale = window.scale_factor();
    let mut chips = h_flex().flex_wrap().gap_1();
    let mut any = false;
    for id in &model.badges {
        let Some(badge) = registry
            .as_ref()
            .and_then(|registry| registry.lookup(channel, id))
        else {
            continue;
        };
        let key = cx
            .has_global::<Images>()
            .then(|| cx.global_mut::<Images>().badge_box(&badge, BADGE, scale))
            .flatten();
        any = true;
        chips = chips.child(
            h_flex()
                .gap_1()
                .items_center()
                .px_1p5()
                .py_0p5()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .text_xs()
                .child(emote_thumb::image(
                    key,
                    BADGE,
                    2.0,
                    tokens.border,
                    images.clone(),
                    owner,
                ))
                .child(badge.title.clone()),
        );
    }
    any.then(|| chips.into_any_element())
}

/// *Ignore* / *Unignore* and, with a login, *Block on Twitch…* / *Unblock* (PR 5.7): none for the user's
/// own card. Every one closes the card.
fn ignore_buttons(
    model: &CardModel,
    view: &WeakEntity<ChatView>,
    cx: &mut Context<ChatView>,
) -> Vec<AnyElement> {
    let login = model.login.clone();
    let own = crate::account::own_login(cx);
    if own
        .as_deref()
        .is_some_and(|own| own.eq_ignore_ascii_case(&login))
    {
        return Vec::new();
    }
    let Some(hub) = crate::ignores::current(cx) else {
        return Vec::new();
    };
    let (listed, blocked) = {
        let hub = hub.read(cx);
        (hub.is_listed(&login), hub.is_blocked(&login))
    };
    let mut buttons = Vec::new();
    let ignore_view = view.clone();
    let ignore_login = login.clone();
    buttons.push(
        Button::new("user-card-ignore")
            .ghost()
            .xsmall()
            .label(if listed { "Unignore" } else { "Ignore" })
            .tooltip(if listed {
                "Show their messages again"
            } else {
                "Hide their messages everywhere"
            })
            .on_click(move |_, _, cx| {
                let login = ignore_login.clone();
                ignore_view
                    .update(cx, |view, cx| {
                        if listed {
                            view.unignore_user(&login, cx);
                        } else {
                            view.ignore_user(&login, cx);
                        }
                        view.close_user_card(cx);
                    })
                    .ok();
            })
            .into_any_element(),
    );
    if own.is_some() {
        let block_view = view.clone();
        let user_id = model.user_id.clone();
        buttons.push(
            Button::new("user-card-block")
                .ghost()
                .xsmall()
                .label(if blocked { "Unblock" } else { "Block…" })
                .tooltip(if blocked {
                    "Unblock on Twitch"
                } else {
                    "Block on Twitch"
                })
                .on_click(move |_, _, cx| {
                    let (login, user_id) = (login.clone(), user_id.clone());
                    block_view
                        .update(cx, |view, cx| {
                            view.close_user_card(cx);
                            view.request_block(login, user_id, !blocked, cx);
                        })
                        .ok();
                })
                .into_any_element(),
        );
    }
    buttons
}

/// Copy username, Mention, Reply, Profile (and what `ignoring` adds).
fn actions_row(
    model: &CardModel,
    view: &WeakEntity<ChatView>,
    ignoring: Vec<AnyElement>,
) -> AnyElement {
    let login = model.login.clone();
    let copy_login = login.clone();
    let copy_view = view.clone();
    let mention_view = view.clone();
    let mention_login = login.clone();
    let reply_view = view.clone();
    let reply_to = model.reply_to.clone();
    let profile = profile_url(&login);
    let profile_view = view.clone();
    h_flex()
        .gap_1()
        .flex_wrap()
        .child(
            Button::new("user-card-copy")
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::Copy))
                .label("Copy name")
                .tooltip("Copy username")
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_login.clone()));
                    copy_view
                        .update(cx, |view, cx| view.close_user_card(cx))
                        .ok();
                }),
        )
        .child(
            Button::new("user-card-mention")
                .ghost()
                .xsmall()
                .label("Mention")
                .tooltip("Put @name in the input box")
                .on_click(move |_, _, cx| {
                    let login = mention_login.clone();
                    mention_view
                        .update(cx, |view, cx| view.mention(login, cx))
                        .ok();
                }),
        )
        .child(
            Button::new("user-card-reply")
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::Undo2))
                .label("Reply")
                .tooltip(if reply_to.is_some() {
                    "Reply to their latest message"
                } else {
                    "Nothing of theirs to reply to"
                })
                .disabled(reply_to.is_none())
                .on_click(move |_, _, cx| {
                    if let Some(message) = reply_to.clone() {
                        reply_view
                            .update(cx, |view, cx| view.reply_from_card(message, cx))
                            .ok();
                    }
                }),
        )
        .child(
            Button::new("user-card-profile")
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::ExternalLink))
                .label("Profile")
                .tooltip("Open twitch.tv profile")
                .disabled(profile.is_none())
                .on_click(move |_, _, cx| {
                    if let Some(url) = &profile {
                        cx.open_url(url);
                    }
                    profile_view
                        .update(cx, |view, cx| view.close_user_card(cx))
                        .ok();
                }),
        )
        .children(ignoring)
        .into_any_element()
}

/// Timeout presets, Ban and Unban, for a moderator who may use them on this user.
fn moderation_rows(
    model: &CardModel,
    view: &WeakEntity<ChatView>,
    tokens: &Tokens,
) -> Option<AnyElement> {
    let actions = model.actions;
    if !actions.any() {
        return None;
    }
    let run = {
        let (view, login, user_id) = (view.clone(), model.login.clone(), model.user_id.clone());
        move |action: RowAction, cx: &mut gpui_kit::App| {
            let (login, user_id) = (login.clone(), user_id.clone());
            view.update(cx, |view, cx| {
                view.moderate_user(login, user_id, action, cx)
            })
            .ok();
        }
    };
    let mut timeouts = h_flex().gap_0p5().flex_wrap();
    for seconds in TIMEOUT_PRESETS {
        let run = run.clone();
        timeouts = timeouts.child(
            Button::new(SharedString::from(format!("user-card-timeout-{seconds}")))
                .ghost()
                .xsmall()
                .label(format_seconds(seconds))
                .tooltip(RowAction::Timeout(seconds).label())
                .accessibility_label(format!("Time out for {}", duration_words(seconds)))
                .disabled(!actions.timeout)
                .on_click(move |_, _, cx| run(RowAction::Timeout(seconds), cx)),
        );
    }
    let (ban, unban) = (run.clone(), run);
    Some(
        v_flex()
            .gap_1()
            .pt_1()
            .border_t_1()
            .border_color(tokens.border)
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(tokens.text_muted)
                            .child("Timeout"),
                    )
                    .child(timeouts),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("user-card-ban")
                            .danger()
                            .xsmall()
                            .icon(Icon::new(IconName::Ban))
                            .label("Ban…")
                            .disabled(!actions.ban)
                            .on_click(move |_, _, cx| ban(RowAction::Ban, cx)),
                    )
                    .child(
                        Button::new("user-card-unban")
                            .ghost()
                            .xsmall()
                            .label("Unban")
                            .disabled(!actions.unban)
                            .on_click(move |_, _, cx| unban(RowAction::Unban, cx)),
                    ),
            )
            .into_any_element(),
    )
}

/// The user's messages, newest first, each one a click that jumps to it in the chat. A list of
/// fixed-height rows: only the visible ones are built.
fn message_list(
    model: &CardModel,
    deleted: DeletedMessages,
    tokens: &Tokens,
    view: &WeakEntity<ChatView>,
) -> AnyElement {
    let messages = model.messages.clone();
    let count = messages.len();
    let (tokens_for_rows, view) = (*tokens, view.clone());
    let list = uniform_list("user-card-messages", count, move |range, _, _| {
        range
            .filter_map(|ix| messages.get(ix))
            .map(|(seq, message)| message_row(*seq, message, deleted, &tokens_for_rows, &view))
            .collect::<Vec<_>>()
    })
    .w_full()
    .h(px((count as f32 * ROW_HEIGHT).min(LIST_HEIGHT)));
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(tokens.text_muted)
                .child(messages_title(count)),
        )
        .child(list)
        .into_any_element()
}

/// One row of the list: time, text (two lines at most), a "history" mark for a message from
/// recent-messages history; struck through (or collapsed) when it was removed.
fn message_row(
    seq: u64,
    message: &Arc<Message>,
    deleted: DeletedMessages,
    tokens: &Tokens,
    view: &WeakEntity<ChatView>,
) -> AnyElement {
    let jump_view = view.clone();
    let removed = message.flags.deleted;
    let collapsed = is_collapsed(message, deleted, false);
    let text = row_text(message, deleted);
    let time = display::clock(message.timestamp.unix_millis());
    let label = format!(
        "{time}{}: {text}. Jump to this message",
        if message.flags.history {
            ", from history"
        } else {
            ""
        }
    );
    let mut body = div().flex_1().min_w_0().text_sm().line_clamp(2);
    if removed || collapsed {
        body = body.text_color(tokens.text_muted);
        // A struck-through message keeps its text; a collapsed one has none to strike.
        if !collapsed {
            body = body.line_through();
        }
    }
    let body = body.child(text);
    let mut row = h_flex()
        .id(SharedString::from(format!("user-card-row-{seq}")))
        .w_full()
        .h(px(ROW_HEIGHT))
        .gap_2()
        .items_start()
        .px_1()
        .py_0p5()
        .rounded_md()
        .cursor_pointer()
        .hover(|row| row.bg(tokens.border.opacity(0.5)))
        .aria_label(label)
        .on_click(move |_, _, cx| {
            jump_view
                .update(cx, |view, cx| view.jump_from_card(seq, cx))
                .ok();
        })
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(tokens.text_muted)
                .child(time),
        );
    if message.flags.history {
        row = row
            .child(
                div()
                    .flex_none()
                    .px_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(tokens.border)
                    .text_xs()
                    .text_color(tokens.text_muted)
                    .child("history"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .opacity(tokens.history_opacity)
                    .child(body),
            );
    } else {
        row = row.child(body);
    }
    row.into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{Author, ChannelId, ChatEvent, Fragment, Removal, Timestamp};

    use crate::channels::ChannelStore;

    use super::*;

    fn profile() -> UserProfile {
        UserProfile {
            id: "77".into(),
            login: "bob".into(),
            display_name: "Bob".into(),
            avatar_url: Some("https://example.com/a.png".into()),
            created_at: gpui_chat_core::parse_rfc3339("2015-03-14T09:26:53Z"),
        }
    }

    fn now() -> i64 {
        gpui_chat_core::parse_rfc3339("2018-03-14T10:00:00Z").unwrap()
    }

    fn chat(id: &str, login: &str, text: &str, ts: i64) -> ChatEvent {
        ChatEvent::Message {
            channel: ChannelId::new("streamer"),
            message: Arc::new(Message::chat(
                id,
                Timestamp::from_unix_millis(ts),
                {
                    let mut author = Author::new(login, login);
                    author.user_id = Some("77".into());
                    author
                },
                vec![Fragment::Text(text.to_owned())],
            )),
        }
    }

    /// The card for `login` from `store`, opened from nothing in particular.
    fn build_for(
        login: &str,
        store: &ChannelStore,
        profile: Option<&Profile>,
        follow: &dyn Fn(&str) -> Option<Follow>,
        inputs: CardInputs<'_>,
    ) -> CardModel {
        build_from(login, None, store, profile, follow, inputs)
    }

    fn build_from(
        login: &str,
        origin_id: Option<&str>,
        store: &ChannelStore,
        profile: Option<&Profile>,
        follow: &dyn Fn(&str) -> Option<Follow>,
        inputs: CardInputs<'_>,
    ) -> CardModel {
        let messages = CardMessages::new(login).update(store.messages());
        CardModel::build(
            login,
            origin_id,
            messages,
            store.channel(),
            profile,
            follow,
            inputs,
        )
    }

    fn store(events: impl IntoIterator<Item = ChatEvent>) -> ChannelStore {
        let mut store = ChannelStore::new(ChannelId::new("streamer"), 100);
        store.apply_all(events);
        store
    }

    fn inputs(logged_in: bool, role: ChannelRole) -> CardInputs<'static> {
        CardInputs {
            role,
            own_login: Some("me"),
            moderation: true,
            can_reply: true,
            logged_in,
            now: now(),
        }
    }

    #[test]
    fn the_account_lines_give_the_date_and_the_age() {
        let lines = detail_lines(&profile(), None, now());
        assert_eq!(lines, ["Account created 14 Mar 2015 (3 years old)"]);
        let followed = gpui_chat_core::parse_rfc3339("2017-03-14T09:26:53Z").unwrap();
        let lines = detail_lines(&profile(), Some(Follow::Since(Some(followed))), now());
        assert_eq!(
            lines,
            [
                "Account created 14 Mar 2015 (3 years old)",
                "Following since 14 Mar 2017 (1 year)"
            ]
        );
        // A follow that is unknown, absent or unreadable adds no line.
        for follow in [Follow::Unavailable, Follow::Since(None), Follow::Loading] {
            assert_eq!(detail_lines(&profile(), Some(follow), now()).len(), 1);
        }
        let undated = UserProfile {
            created_at: None,
            ..profile()
        };
        assert!(detail_lines(&undated, None, now()).is_empty());
    }

    #[test]
    fn the_avatar_is_its_image_unless_there_is_none_or_it_failed_for_good() {
        let url = Some("https://static-cdn.jtvnw.net/x-profile_image-600x600.jpeg");
        assert_eq!(avatar_show(url, false), AvatarShow::Image);
        // Not found, not an image, undecodable: the same initial as the logged-out card.
        assert_eq!(avatar_show(url, true), AvatarShow::Initial);
        assert_eq!(avatar_show(None, false), AvatarShow::Initial);
        assert_eq!(avatar_show(None, true), AvatarShow::Initial);
    }

    #[test]
    fn the_avatar_placeholder_is_the_uppercased_first_letter() {
        assert_eq!(initial("alice"), "A");
        assert_eq!(initial("Émile"), "É");
        assert_eq!(initial("ßeta"), "SS");
        assert_eq!(initial(""), "");
    }

    #[test]
    fn the_message_list_is_titled_by_its_length() {
        assert_eq!(messages_title(0), "No messages this session");
        assert_eq!(messages_title(1), "1 message this session");
        assert_eq!(messages_title(7), "7 messages this session");
        assert_eq!(
            messages_title(MAX_CARD_MESSAGES),
            "Latest 200 messages this session"
        );
    }

    #[test]
    fn logged_out_the_card_has_the_store_and_a_login_hint() {
        let store = store([
            chat("1", "bob", "hello", 1_000),
            chat("2", "alice", "hi", 2_000),
            chat("3", "Bob", "again", 3_000),
        ]);
        let model = build_for(
            "Bob",
            &store,
            None,
            &|_| None,
            inputs(false, ChannelRole::Viewer),
        );
        assert_eq!(model.details, Details::LoggedOut);
        // The newest message's own capitalisation.
        assert_eq!(model.title, "Bob");
        assert_eq!(model.login, "bob");
        assert_eq!(model.avatar, None);
        assert_eq!(model.messages.len(), 2);
        assert_eq!(model.messages[0].1.id.as_str(), "3");
        assert_eq!(model.reply_to.as_ref().unwrap().id.as_str(), "3");
        assert_eq!(model.user_id.as_deref(), Some("77"));
        assert!(!model.actions.any());
    }

    #[test]
    fn a_found_profile_gives_the_avatar_the_name_and_the_account_lines() {
        let store = store([chat("1", "bob", "hello", 1_000)]);
        let found = Profile::Found(Arc::new(profile()));
        let model = build_for(
            "bob",
            &store,
            Some(&found),
            &|_| None,
            inputs(true, ChannelRole::Viewer),
        );
        assert_eq!(model.title, "Bob");
        assert_eq!(model.avatar.as_deref(), Some("https://example.com/a.png"));
        assert_eq!(
            model.details,
            Details::Found(vec!["Account created 14 Mar 2015 (3 years old)".into()])
        );
    }

    #[test]
    fn each_lookup_state_has_its_own_text() {
        let store = store([]);
        let build = |profile: Option<&Profile>, logged_in| {
            build_for(
                "bob",
                &store,
                profile,
                &|_| None,
                inputs(logged_in, ChannelRole::Viewer),
            )
            .details
        };
        assert_eq!(build(Some(&Profile::Loading), true), Details::Loading);
        assert_eq!(build(None, true), Details::Loading);
        assert_eq!(build(Some(&Profile::Missing), true), Details::Missing);
        assert_eq!(
            build(Some(&Profile::Failed("no".into())), true),
            Details::Failed("no".into())
        );
        assert_eq!(build(None, false), Details::LoggedOut);
    }

    #[test]
    fn a_user_with_no_messages_still_gets_a_card_and_nothing_to_reply_to() {
        let store = store([chat("1", "alice", "hi", 1_000)]);
        let model = build_for(
            "bob",
            &store,
            None,
            &|_| None,
            inputs(false, ChannelRole::Viewer),
        );
        assert!(model.messages.is_empty());
        assert!(model.reply_to.is_none());
        assert_eq!(model.title, "bob");
    }

    #[test]
    fn replying_needs_a_box_and_a_message_that_can_be_replied_to() {
        let mut store = store([
            chat("1", "bob", "kept", 1_000),
            chat("2", "bob", "gone", 2_000),
        ]);
        store.apply(ChatEvent::DeleteMessage {
            channel: ChannelId::new("streamer"),
            message_id: "2".into(),
            login: None,
        });
        let model = build_for(
            "bob",
            &store,
            None,
            &|_| None,
            inputs(false, ChannelRole::Viewer),
        );
        // The newest is deleted: the one before it is what Reply goes to.
        assert_eq!(model.reply_to.as_ref().unwrap().id.as_str(), "1");
        assert!(model.messages[0].1.flags.removal == Removal::Deleted);
        let no_box = CardInputs {
            can_reply: false,
            ..inputs(false, ChannelRole::Viewer)
        };
        assert!(
            build_for("bob", &store, None, &|_| None, no_box)
                .reply_to
                .is_none()
        );
    }

    #[test]
    fn moderation_follows_the_role_the_flag_and_who_the_user_is() {
        let store = store([chat("1", "bob", "hello", 1_000)]);
        let build = |login: &str, role, moderation| {
            let inputs = CardInputs {
                moderation,
                ..inputs(true, role)
            };
            build_for(login, &store, None, &|_| None, inputs).actions
        };
        assert!(build("bob", ChannelRole::Moderator, true).any());
        assert!(!build("bob", ChannelRole::Viewer, true).any());
        // The pane cannot run commands (an offline run without the loopback).
        assert!(!build("bob", ChannelRole::Moderator, false).any());
        // Not on themselves, not on the broadcaster.
        assert!(!build("me", ChannelRole::Moderator, true).any());
        assert!(!build("streamer", ChannelRole::Moderator, true).any());
    }

    #[test]
    fn a_user_named_only_by_a_mention_gets_no_moderation_until_a_message_shows_them() {
        let none = store([chat("1", "alice", "hi", 1_000)]);
        let inputs = inputs(true, ChannelRole::Moderator);
        assert!(
            !build_for("bob", &none, None, &|_| None, inputs)
                .actions
                .any()
        );
        let some = store([chat("1", "bob", "hi", 1_000)]);
        assert!(
            build_for("bob", &some, None, &|_| None, inputs)
                .actions
                .any()
        );
    }

    #[test]
    fn an_account_that_is_not_the_sender_is_a_conflict_and_gets_no_moderation() {
        // The messages are from account 77 (see `chat`); Helix says the login is account 99 now.
        let store = store([chat("1", "bob", "hi", 1_000)]);
        let inputs = inputs(true, ChannelRole::Moderator);
        let other = Profile::Found(Arc::new(UserProfile {
            id: "99".into(),
            ..profile()
        }));
        let model = build_for("bob", &store, Some(&other), &|_| None, inputs);
        assert!(model.conflict);
        assert!(!model.actions.any());
        assert_eq!(
            model.user_id.as_deref(),
            Some("77"),
            "still aimed at the sender"
        );
        assert_eq!(
            model.wants_follow(ChannelRole::Moderator, Some("1000")),
            None
        );
        // The same account: no conflict, moderation as usual.
        let same = Profile::Found(Arc::new(profile()));
        let model = build_for("bob", &store, Some(&same), &|_| None, inputs);
        assert!(!model.conflict);
        assert!(model.actions.any());
        // Opened from a message by another id than the newest one: that message's sender counts.
        let model = build_from("bob", Some("55"), &store, Some(&same), &|_| None, inputs);
        assert!(model.conflict, "77 is not who sent the clicked message");
        assert_eq!(model.user_id.as_deref(), Some("55"));
        assert!(!model.actions.any());
        // With nothing to compare against, Helix's id is used and nothing conflicts.
        let empty = self::store([]);
        let model = build_for("bob", &empty, Some(&same), &|_| None, inputs);
        assert!(!model.conflict);
        assert_eq!(model.user_id.as_deref(), Some("77"));
    }

    #[test]
    fn the_follow_lookup_is_for_a_moderator_who_has_both_ids() {
        let store = store([chat("1", "bob", "hello", 1_000)]);
        let found = Profile::Found(Arc::new(profile()));
        let model = |role| build_for("bob", &store, Some(&found), &|_| None, inputs(true, role));
        assert_eq!(
            model(ChannelRole::Moderator).wants_follow(ChannelRole::Moderator, Some("1000")),
            Some(("1000".to_owned(), "77".to_owned()))
        );
        assert_eq!(
            model(ChannelRole::Viewer).wants_follow(ChannelRole::Viewer, Some("1000")),
            None
        );
        assert_eq!(
            model(ChannelRole::Moderator).wants_follow(ChannelRole::Moderator, None),
            None
        );
        // Nothing to ask while the account is not known.
        let loading = build_for(
            "bob",
            &store,
            None,
            &|_| None,
            inputs(true, ChannelRole::Moderator),
        );
        assert_eq!(
            loading.wants_follow(ChannelRole::Moderator, Some("1000")),
            None
        );
    }

    #[test]
    fn a_removed_message_reads_as_the_chat_draws_it() {
        let store = {
            let mut store = store([chat("1", "bob", "hello   there", 1_000)]);
            store.apply(ChatEvent::DeleteMessage {
                channel: ChannelId::new("streamer"),
                message_id: "1".into(),
                login: None,
            });
            store
        };
        let message = store.messages().get("1").unwrap();
        assert_eq!(
            row_text(message, DeletedMessages::Strikethrough),
            "hello there"
        );
        assert_eq!(
            row_text(message, DeletedMessages::Collapse),
            "<message deleted>"
        );
    }
}
