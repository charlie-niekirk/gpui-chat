//! Raw IRC line → [`ChatEvent`]. Pure: no network, no clock, no async runtime.
//!
//! [`parse_line`] is the entry point for anything that has a text line in hand
//! (fixtures, `--replay` files, recent-messages history). The live client feeds the
//! same mapping ([`map_message`]) with the `IRCMessage`s its transport already parsed,
//! so a fixture exercises exactly the code that runs on the wire.
//!
//! # Conventions
//!
//! * Emote ranges in the `emotes` tag are inclusive and count code points; they are
//!   relative to the text *after* the `/me` wrapper is stripped. The tokenizer in
//!   `gpui-chat-core` takes them as such.
//! * Every tag is kept in [`Message::raw_tags`], sorted by key, with values
//!   **unescaped** (`\s` is already a space), whatever the typed fields say.
//! * A missing or empty `color` tag stays `None`; the renderer picks a colour.
//! * Lines that are not chat (PING, JOIN, 001, CAP, USERSTATE, ...) map to `Ok(None)`.

use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::message::RawTags;
use gpui_chat_core::{
    Author, BadgeId, ChannelId, ChatEvent, ClearTarget, DEFAULT_CHEERMOTE_PREFIXES, FollowersOnly,
    Fragment, Message, MessageFlags, MessageId, MessageKind, ReplyRef, Rgb, RoomStateUpdate,
    Timestamp, Tokenizer, twitch_emote_ranges,
};
use twitch_irc::message::{IRCMessage, IRCParseError, IRCPrefix};

/// Why a line could not be turned into an event.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Not valid IRC at all.
    #[error("malformed IRC line: {0}")]
    Syntax(#[from] IRCParseError),
    /// A parameter every `command` carries was missing.
    #[error("{command} line is missing parameter {index}")]
    MissingParam { command: String, index: usize },
    /// The line has no `nick!user@host` prefix, which `command` needs.
    #[error("{command} line has no user prefix")]
    MissingPrefix { command: String },
    /// A tag the event cannot be built without was absent or empty.
    #[error("{command} line is missing tag `{tag}`")]
    MissingTag { command: String, tag: &'static str },
    /// A tag had a value that does not parse.
    #[error("{command} line has invalid value {value:?} for tag `{tag}`")]
    InvalidTag {
        command: String,
        tag: &'static str,
        value: String,
    },
}

/// Knobs for [`parse_line_with`]. The default is what [`parse_line`] uses.
#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    /// The local user's login, so mentions of it set [`MessageFlags::mention`]. Leave
    /// `None` for anonymous connections.
    pub own_login: Option<String>,
    /// Timestamp for lines that carry neither `tmi-sent-ts` nor `rm-received-ts`.
    /// Core never reads the clock, so the caller supplies it; the live client passes
    /// the wall clock, [`parse_line`] leaves it at zero.
    pub fallback_timestamp: Timestamp,
}

/// Parses one raw IRC line (no trailing newline needed) into a [`ChatEvent`].
///
/// * `Ok(Some(event))`: a chat event.
/// * `Ok(None)`: a valid line that is not chat (PING, JOIN, numerics, ...), a blank
///   line, or a `#` comment (which is never valid IRC, so replay files can use them).
/// * `Err(_)`: malformed IRC, or a chat command missing something essential.
///
/// Accepts the recent-messages history format too: `@historical=1;rm-received-ts=...`
/// lines map to messages with [`MessageFlags::history`] set.
///
/// ```
/// use gpui_chat_twitch::parse_line;
/// let line = "@id=1;tmi-sent-ts=5 :a!a@a.tmi.twitch.tv PRIVMSG #chan :hi";
/// assert!(parse_line(line).unwrap().is_some());
/// assert!(parse_line("PING :tmi.twitch.tv").unwrap().is_none());
/// ```
pub fn parse_line(line: &str) -> Result<Option<ChatEvent>, ParseError> {
    parse_line_with(line, &ParseOptions::default())
}

/// [`parse_line`] with explicit [`ParseOptions`].
pub fn parse_line_with(
    line: &str,
    options: &ParseOptions,
) -> Result<Option<ChatEvent>, ParseError> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    map_message(&IRCMessage::parse(line)?, options)
}

/// Maps an already-parsed IRC message. This is what [`parse_line`] and the live client
/// share.
pub fn map_message(
    msg: &IRCMessage,
    options: &ParseOptions,
) -> Result<Option<ChatEvent>, ParseError> {
    match msg.command.as_str() {
        "PRIVMSG" => privmsg(msg, options).map(Some),
        "USERNOTICE" => usernotice(msg, options).map(Some),
        "CLEARCHAT" => clearchat(msg).map(Some),
        "CLEARMSG" => clearmsg(msg).map(Some),
        "ROOMSTATE" => roomstate(msg).map(Some),
        "NOTICE" => notice(msg).map(Some),
        _ => Ok(None),
    }
}

// ---- helpers ---------------------------------------------------------------------

fn param(msg: &IRCMessage, index: usize) -> Result<&str, ParseError> {
    msg.params
        .get(index)
        .map(String::as_str)
        .ok_or_else(|| ParseError::MissingParam {
            command: msg.command.clone(),
            index,
        })
}

fn channel(msg: &IRCMessage) -> Result<ChannelId, ParseError> {
    Ok(ChannelId::new(param(msg, 0)?))
}

/// Tag value; empty counts as absent (Twitch sends `key=` for "unset").
fn tag<'a>(msg: &'a IRCMessage, key: &str) -> Option<&'a str> {
    msg.tags
        .0
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

fn required_tag<'a>(msg: &'a IRCMessage, key: &'static str) -> Result<&'a str, ParseError> {
    tag(msg, key).ok_or_else(|| ParseError::MissingTag {
        command: msg.command.clone(),
        tag: key,
    })
}

fn invalid(msg: &IRCMessage, key: &'static str, value: &str) -> ParseError {
    ParseError::InvalidTag {
        command: msg.command.clone(),
        tag: key,
        value: value.to_owned(),
    }
}

fn number<N: std::str::FromStr>(
    msg: &IRCMessage,
    key: &'static str,
) -> Result<Option<N>, ParseError> {
    tag(msg, key)
        .map(|v| v.parse().map_err(|_| invalid(msg, key, v)))
        .transpose()
}

fn flag(msg: &IRCMessage, key: &'static str) -> Result<Option<bool>, ParseError> {
    match tag(msg, key) {
        None => Ok(None),
        Some("1") => Ok(Some(true)),
        Some("0") => Ok(Some(false)),
        Some(other) => Err(invalid(msg, key, other)),
    }
}

fn nick(msg: &IRCMessage) -> Result<&str, ParseError> {
    match &msg.prefix {
        Some(IRCPrefix::Full { nick, .. }) => Ok(nick),
        _ => Err(ParseError::MissingPrefix {
            command: msg.command.clone(),
        }),
    }
}

fn timestamp(msg: &IRCMessage, options: &ParseOptions) -> Timestamp {
    ["tmi-sent-ts", "rm-received-ts"]
        .into_iter()
        .find_map(|k| tag(msg, k)?.parse().ok())
        .map_or(options.fallback_timestamp, Timestamp::from_unix_millis)
}

fn raw_tags(msg: &IRCMessage) -> RawTags {
    let mut pairs: Vec<(String, String)> = msg
        .tags
        .0
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    pairs.sort();
    RawTags::new(pairs)
}

/// `moderator/1,subscriber/12` → badge ids. Entries without a version are skipped.
fn badges(msg: &IRCMessage) -> Vec<BadgeId> {
    tag(msg, "badges")
        .into_iter()
        .flat_map(|v| v.split(','))
        .filter_map(|entry| {
            let (set, version) = entry.split_once('/')?;
            (!set.is_empty()).then(|| BadgeId::new(set, version))
        })
        .collect()
}

fn author(msg: &IRCMessage, login: &str) -> Author {
    let login = login.to_owned();
    let display_name = tag(msg, "display-name").map_or_else(|| login.clone(), str::to_owned);
    Author {
        login,
        display_name,
        user_id: tag(msg, "user-id").map(str::to_owned),
        color: tag(msg, "color").and_then(Rgb::from_hex),
        badges: badges(msg),
    }
}

fn reply(msg: &IRCMessage) -> Option<ReplyRef> {
    let parent_id = tag(msg, "reply-parent-msg-id")?;
    let text = |key| tag(msg, key).unwrap_or_default().to_owned();
    Some(ReplyRef {
        parent_id: MessageId::from(parent_id),
        parent_login: text("reply-parent-user-login"),
        parent_display_name: text("reply-parent-display-name"),
        parent_body: text("reply-parent-msg-body"),
        thread_parent_id: tag(msg, "reply-thread-parent-msg-id").map(MessageId::from),
    })
}

/// Tokenizes a message body with the `emotes` tag of `msg`.
fn fragments(
    msg: &IRCMessage,
    text: &str,
    has_bits: bool,
    options: &ParseOptions,
) -> (Vec<Fragment>, bool) {
    let ranges = tag(msg, "emotes").map_or_else(Vec::new, |t| twitch_emote_ranges(t, text));
    let mut tokenizer = Tokenizer::new();
    if let Some(login) = options.own_login.as_deref() {
        tokenizer = tokenizer.with_own_login(login);
    }
    if has_bits {
        tokenizer = tokenizer.with_cheermote_prefixes(DEFAULT_CHEERMOTE_PREFIXES);
    }
    let out = tokenizer.tokenize(text, &ranges);
    (out.fragments, out.mentions_self)
}

// ---- commands --------------------------------------------------------------------

fn privmsg(msg: &IRCMessage, options: &ParseOptions) -> Result<ChatEvent, ParseError> {
    let channel = channel(msg)?;
    let mut text = param(msg, 1)?;
    let mut kind = MessageKind::Chat;
    if let Some(inner) = text
        .strip_prefix("\u{1}ACTION ")
        .and_then(|t| t.strip_suffix('\u{1}'))
    {
        text = inner;
        kind = MessageKind::Action;
    }
    let id = MessageId::from(required_tag(msg, "id")?);
    let bits: Option<u32> = number(msg, "bits")?;
    let (fragments, mentions_self) = fragments(msg, text, bits.is_some(), options);

    let message = Message {
        id,
        timestamp: timestamp(msg, options),
        kind,
        author: Some(author(msg, nick(msg)?)),
        fragments,
        system_text: None,
        notice_id: None,
        bits,
        raw_tags: raw_tags(msg),
        flags: MessageFlags {
            mention: mentions_self,
            first_message: flag(msg, "first-msg")?.unwrap_or(false),
            highlighted: tag(msg, "msg-id") == Some("highlighted-message"),
            deleted: false,
            history: tag(msg, "historical") == Some("1"),
        },
        reply: reply(msg),
    };
    Ok(ChatEvent::Message {
        channel,
        message: Arc::new(message),
    })
}

/// Sub, resub, subgift, raid, announcement, ...: `notice_id` is `msg-id`, `system_text`
/// is `system-msg`, `fragments` is whatever the user typed (often nothing).
fn usernotice(msg: &IRCMessage, options: &ParseOptions) -> Result<ChatEvent, ParseError> {
    let channel = channel(msg)?;
    let text = msg.params.get(1).map_or("", String::as_str);
    let bits: Option<u32> = number(msg, "bits")?;
    let (fragments, mentions_self) = fragments(msg, text, bits.is_some(), options);

    let message = Message {
        id: MessageId::from(required_tag(msg, "id")?),
        timestamp: timestamp(msg, options),
        kind: MessageKind::UserNotice,
        author: tag(msg, "login").map(|login| author(msg, login)),
        fragments,
        system_text: tag(msg, "system-msg").map(str::to_owned),
        notice_id: tag(msg, "msg-id").map(str::to_owned),
        bits,
        raw_tags: raw_tags(msg),
        flags: MessageFlags {
            mention: mentions_self,
            history: tag(msg, "historical") == Some("1"),
            ..MessageFlags::default()
        },
        reply: reply(msg),
    };
    Ok(ChatEvent::Message {
        channel,
        message: Arc::new(message),
    })
}

fn clearchat(msg: &IRCMessage) -> Result<ChatEvent, ParseError> {
    let channel = channel(msg)?;
    let target = match msg.params.get(1) {
        None => ClearTarget::Channel,
        Some(login) => ClearTarget::User {
            login: login.clone(),
            duration: number::<u64>(msg, "ban-duration")?.map(Duration::from_secs),
        },
    };
    Ok(ChatEvent::ClearChat { channel, target })
}

fn clearmsg(msg: &IRCMessage) -> Result<ChatEvent, ParseError> {
    Ok(ChatEvent::DeleteMessage {
        channel: channel(msg)?,
        message_id: MessageId::from(required_tag(msg, "target-msg-id")?),
        login: tag(msg, "login").map(str::to_owned),
    })
}

fn roomstate(msg: &IRCMessage) -> Result<ChatEvent, ParseError> {
    let followers_only = match number::<i64>(msg, "followers-only")? {
        None => None,
        Some(n) if n < 0 => Some(FollowersOnly::Off),
        Some(0) => Some(FollowersOnly::All),
        Some(n) => {
            Some(FollowersOnly::Minutes(u32::try_from(n).map_err(|_| {
                invalid(msg, "followers-only", &n.to_string())
            })?))
        }
    };
    Ok(ChatEvent::RoomState {
        channel: channel(msg)?,
        update: RoomStateUpdate {
            room_id: tag(msg, "room-id").map(str::to_owned),
            emote_only: flag(msg, "emote-only")?,
            followers_only,
            unique_chat: flag(msg, "r9k")?,
            slow_mode: number::<u64>(msg, "slow")?.map(Duration::from_secs),
            subscribers_only: flag(msg, "subs-only")?,
        },
    })
}

fn notice(msg: &IRCMessage) -> Result<ChatEvent, ParseError> {
    // `NOTICE * :Login authentication failed`: the target is `*`, not a channel.
    let channel = param(msg, 0)?
        .starts_with('#')
        .then(|| channel(msg))
        .transpose()?;
    Ok(ChatEvent::Notice {
        channel,
        msg_id: tag(msg, "msg-id").map(str::to_owned),
        text: param(msg, 1)?.to_owned(),
    })
}

#[cfg(test)]
mod tests;
