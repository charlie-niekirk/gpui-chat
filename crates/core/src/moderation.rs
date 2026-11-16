//! Moderation (PR 5.2), pure and I/O free: the slash commands, what they ask Twitch for, the lines
//! chat shows when someone was timed out, and which controls a message offers.
//!
//! * [`parse_mod_command`] turns `/timeout user 10m spam` into a [`ModCommand`], or says why not
//!   (the sentence is the usage / validation message shown under the input).
//! * [`ModCommand`] is what the twitch crate turns into a Helix request; the UI builds the same
//!   values for its buttons, menu items and room-mode toggles, so a click and a typed command are
//!   one code path.
//! * [`clear_line`] is the system line for a `CLEARCHAT` ("alice was timed out for 10m"), with an
//!   id derived from what happened so the live copy and the recent-messages copy are one line.
//! * [`row_actions`] decides which moderation controls a message gets, for a given role.

use std::fmt;

use crate::event::{ChannelRole, ClearTarget, FollowersOnly, RoomState};
use crate::ids::ChannelId;
use crate::message::{Message, MessageKind, Timestamp};

/// Longest timeout Twitch accepts: two weeks.
pub const MAX_TIMEOUT_SECS: u64 = 1_209_600;
/// What `/timeout user` and the hover button use: ten minutes.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
/// Slow-mode delays Twitch accepts, in seconds.
pub const SLOW_RANGE: (u32, u32) = (3, 120);
/// What `/slow` without a number uses.
pub const DEFAULT_SLOW_SECS: u32 = 30;
/// Longest followers-only requirement Twitch accepts: three months, in minutes.
pub const MAX_FOLLOWER_MINUTES: u32 = 129_600;
/// Longest reason Twitch accepts for a ban or timeout, in characters.
pub const MAX_REASON_CHARS: usize = 500;
/// Longest announcement, in characters.
pub const MAX_ANNOUNCEMENT_CHARS: usize = 500;

/// Timeout lengths the Timeout menu offers, in seconds.
pub const TIMEOUT_PRESETS: [u64; 6] = [60, 600, 3_600, 86_400, 604_800, MAX_TIMEOUT_SECS];
/// Slow-mode delays the room-mode menu offers, in seconds.
pub const SLOW_PRESETS: [u32; 5] = [5, 10, 30, 60, 120];
/// Followers-only requirements the room-mode menu offers, in minutes (0 is every follower).
pub const FOLLOWER_PRESETS: [u32; 8] = [0, 10, 30, 60, 1_440, 10_080, 43_200, MAX_FOLLOWER_MINUTES];

/// A change to one of the room's chat modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomChange {
    /// Slow mode with this delay in seconds, or off.
    Slow(Option<u32>),
    /// Followers-only with this requirement in minutes (0: every follower), or off.
    Followers(Option<u32>),
    Subscribers(bool),
    EmoteOnly(bool),
    Unique(bool),
}

/// Something a moderator can do through Helix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModCommand {
    /// Mute `user` for `seconds` (1 to [`MAX_TIMEOUT_SECS`]).
    Timeout {
        user: String,
        seconds: u64,
        reason: Option<String>,
    },
    Ban {
        user: String,
        reason: Option<String>,
    },
    /// Lifts a ban or a timeout.
    Unban {
        user: String,
    },
    /// Deletes one message by its id.
    DeleteMessage {
        message_id: String,
    },
    ClearChat,
    Room(RoomChange),
    AddMod {
        user: String,
    },
    RemoveMod {
        user: String,
    },
    AddVip {
        user: String,
    },
    RemoveVip {
        user: String,
    },
    Announce {
        text: String,
    },
    /// Starts a raid to `user`'s channel.
    Raid {
        user: String,
    },
    Unraid,
    Shoutout {
        user: String,
    },
}

impl ModCommand {
    /// The login the command is about, when it names one.
    pub fn target(&self) -> Option<&str> {
        match self {
            Self::Timeout { user, .. }
            | Self::Ban { user, .. }
            | Self::Unban { user }
            | Self::AddMod { user }
            | Self::RemoveMod { user }
            | Self::AddVip { user }
            | Self::RemoveVip { user }
            | Self::Raid { user }
            | Self::Shoutout { user } => Some(user),
            Self::DeleteMessage { .. }
            | Self::ClearChat
            | Self::Room(_)
            | Self::Announce { .. }
            | Self::Unraid => None,
        }
    }

    /// Who may do it: mods, moderators and the broadcaster, or the broadcaster alone.
    pub fn needs_broadcaster(&self) -> bool {
        matches!(
            self,
            Self::AddMod { .. }
                | Self::RemoveMod { .. }
                | Self::AddVip { .. }
                | Self::RemoveVip { .. }
                | Self::Raid { .. }
                | Self::Unraid
        )
    }

    /// The line to show in chat once Twitch accepted the command, for the ones whose effect chat
    /// does not report by itself (a ban or a room-mode change arrives as `CLEARCHAT` /
    /// `ROOMSTATE`; making someone a moderator does not).
    pub fn confirmation(&self) -> Option<String> {
        Some(match self {
            Self::AddMod { user } => format!("{user} is now a moderator."),
            Self::RemoveMod { user } => format!("{user} is no longer a moderator."),
            Self::AddVip { user } => format!("{user} is now a VIP."),
            Self::RemoveVip { user } => format!("{user} is no longer a VIP."),
            Self::Raid { user } => format!("Raiding {user} in about 90 s. /unraid to cancel."),
            Self::Unraid => "The raid was cancelled.".to_owned(),
            Self::Shoutout { user } => format!("Gave {user} a shoutout."),
            _ => return None,
        })
    }
}

/// Why a typed command was not understood. `Display` is the sentence shown under the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError(String);

impl CommandError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CommandError {}

/// Who may use a command, for the completion list and the "you are not a moderator" refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRole {
    Everyone,
    Moderator,
    Broadcaster,
}

impl CommandRole {
    /// Whether `role` may use a command that needs this.
    pub fn allows(self, role: ChannelRole) -> bool {
        match self {
            Self::Everyone => true,
            Self::Moderator => role.can_moderate(),
            Self::Broadcaster => role.is_broadcaster(),
        }
    }
}

/// A slash command the input box understands (the completion list and the argument hint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandInfo {
    /// Without the slash.
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
    pub role: CommandRole,
}

const fn info(
    name: &'static str,
    usage: &'static str,
    summary: &'static str,
    role: CommandRole,
) -> CommandInfo {
    CommandInfo {
        name,
        usage,
        summary,
        role,
    }
}

use CommandRole::{Broadcaster, Everyone, Moderator};

/// The commands [`crate::parse_input`] accepts (a test keeps the two in step).
pub const COMMANDS: &[CommandInfo] = &[
    info(
        "me",
        "/me <message>",
        "Send a message as an action",
        Everyone,
    ),
    info(
        "timeout",
        "/timeout <user> [duration] [reason]",
        "Time a user out (default 10m)",
        Moderator,
    ),
    info("ban", "/ban <user> [reason]", "Ban a user", Moderator),
    info("unban", "/unban <user>", "Lift a ban or timeout", Moderator),
    info(
        "untimeout",
        "/untimeout <user>",
        "Lift a timeout (same as /unban)",
        Moderator,
    ),
    info(
        "delete",
        "/delete <message id>",
        "Delete one message",
        Moderator,
    ),
    info("clear", "/clear", "Clear the chat for everyone", Moderator),
    info(
        "slow",
        "/slow [seconds]",
        "Slow mode (3 to 120 s, default 30)",
        Moderator,
    ),
    info("slowoff", "/slowoff", "Turn slow mode off", Moderator),
    info(
        "followers",
        "/followers [duration]",
        "Followers-only chat (e.g. 30m, 1d; none: all followers)",
        Moderator,
    ),
    info(
        "followersoff",
        "/followersoff",
        "Turn followers-only chat off",
        Moderator,
    ),
    info(
        "subscribers",
        "/subscribers",
        "Subscribers-only chat",
        Moderator,
    ),
    info(
        "subscribersoff",
        "/subscribersoff",
        "Turn subscribers-only chat off",
        Moderator,
    ),
    info("emoteonly", "/emoteonly", "Emote-only chat", Moderator),
    info(
        "emoteonlyoff",
        "/emoteonlyoff",
        "Turn emote-only chat off",
        Moderator,
    ),
    info(
        "uniquechat",
        "/uniquechat",
        "Unique chat (no repeated messages)",
        Moderator,
    ),
    info(
        "uniquechatoff",
        "/uniquechatoff",
        "Turn unique chat off",
        Moderator,
    ),
    info(
        "announce",
        "/announce <message>",
        "Post a highlighted announcement",
        Moderator,
    ),
    info(
        "shoutout",
        "/shoutout <user>",
        "Give a channel a shoutout",
        Moderator,
    ),
    info("mod", "/mod <user>", "Make a user a moderator", Broadcaster),
    info("unmod", "/unmod <user>", "Remove a moderator", Broadcaster),
    info("vip", "/vip <user>", "Make a user a VIP", Broadcaster),
    info("unvip", "/unvip <user>", "Remove a VIP", Broadcaster),
    info("raid", "/raid <user>", "Raid another channel", Broadcaster),
    info("unraid", "/unraid", "Cancel the raid", Broadcaster),
];

/// The listed command called `name` (case-insensitive, no slash).
pub fn command_info(name: &str) -> Option<&'static CommandInfo> {
    let name = name.to_ascii_lowercase();
    COMMANDS.iter().find(|command| command.name == name)
}

/// The command `line` starts with (`/timeout bob`), for the argument hint under the box: only
/// once the name is complete and followed by a space, `None` for `//text` and plain messages.
pub fn command_hint(line: &str) -> Option<&'static CommandInfo> {
    let rest = line.trim_start().strip_prefix('/')?;
    let (name, _) = rest.split_once(char::is_whitespace)?;
    command_info(name)
}

/// Whether `name` (lowercase, no slash) is one of the moderation commands this module parses.
pub fn is_mod_command(name: &str) -> bool {
    name != "me" && command_info(name).is_some()
}

fn usage_of(name: &str) -> &'static str {
    command_info(name).map_or("", |command| command.usage)
}

/// Parses the command `name` (lowercase, no slash) with its `args` (the rest of the line, trimmed).
/// `None` when `name` is not a moderation command. Whether the user is allowed to run it is the
/// caller's question ([`CommandInfo::role`]).
pub fn parse_mod_command(name: &str, args: &str) -> Option<Result<ModCommand, CommandError>> {
    if !is_mod_command(name) {
        return None;
    }
    Some(parse(name, args.trim()))
}

fn parse(name: &str, args: &str) -> Result<ModCommand, CommandError> {
    let usage = || CommandError::new(format!("Usage: {}", usage_of(name)));
    let mut words = args.split_whitespace();
    let no_args = |command: ModCommand| {
        if args.is_empty() {
            Ok(command)
        } else {
            Err(usage())
        }
    };
    let user = |word: Option<&str>| -> Result<String, CommandError> {
        let word = word.ok_or_else(usage)?;
        login_of(word).ok_or_else(|| {
            CommandError::new(format!(
                "“{word}” is not a Twitch username. {}",
                usage_line(name)
            ))
        })
    };
    let only_user = |mut words: std::str::SplitWhitespace<'_>| -> Result<String, CommandError> {
        let found = user(words.next())?;
        if words.next().is_some() {
            return Err(usage());
        }
        Ok(found)
    };
    match name {
        "timeout" => {
            let target = user(words.next())?;
            let rest = rest_after(args, 1);
            let (seconds, reason) = match words.next() {
                Some(word) if word.starts_with(|c: char| c.is_ascii_digit()) => {
                    let seconds = parse_duration(word, DurationUnit::Seconds)
                        .filter(|seconds| (1..=MAX_TIMEOUT_SECS).contains(seconds))
                        .ok_or_else(|| {
                            CommandError::new(format!(
                                "A timeout lasts from 1s to 14d (like 90, 10m, 2h or 1w). {}",
                                usage_line(name)
                            ))
                        })?;
                    (seconds, rest_after(args, 2))
                }
                _ => (DEFAULT_TIMEOUT_SECS, rest),
            };
            Ok(ModCommand::Timeout {
                user: target,
                seconds,
                reason: reason_of(reason)?,
            })
        }
        "ban" => {
            let target = user(words.next())?;
            Ok(ModCommand::Ban {
                user: target,
                reason: reason_of(rest_after(args, 1))?,
            })
        }
        "unban" | "untimeout" => only_user(words).map(|user| ModCommand::Unban { user }),
        "delete" => {
            let id = words.next().ok_or_else(usage)?;
            if words.next().is_some() {
                return Err(usage());
            }
            Ok(ModCommand::DeleteMessage {
                message_id: id.to_owned(),
            })
        }
        "clear" => no_args(ModCommand::ClearChat),
        "slow" => {
            let seconds = match words.next() {
                None => DEFAULT_SLOW_SECS,
                Some(word) => {
                    if words.next().is_some() {
                        return Err(usage());
                    }
                    parse_duration(word, DurationUnit::Seconds)
                        .and_then(|seconds| u32::try_from(seconds).ok())
                        .filter(|seconds| (SLOW_RANGE.0..=SLOW_RANGE.1).contains(seconds))
                        .ok_or_else(|| {
                            CommandError::new(format!(
                                "Slow mode takes {} to {} seconds. {}",
                                SLOW_RANGE.0,
                                SLOW_RANGE.1,
                                usage_line(name)
                            ))
                        })?
                }
            };
            Ok(ModCommand::Room(RoomChange::Slow(Some(seconds))))
        }
        "slowoff" => no_args(ModCommand::Room(RoomChange::Slow(None))),
        "followers" => {
            let minutes = match words.next() {
                None => 0,
                Some(word) => {
                    if words.next().is_some() {
                        return Err(usage());
                    }
                    parse_duration(word, DurationUnit::Minutes)
                        // Whole minutes only: 90s must not silently become 1m (or 30s "everyone").
                        .filter(|seconds| seconds % 60 == 0)
                        .map(|seconds| seconds / 60)
                        .and_then(|minutes| u32::try_from(minutes).ok())
                        .filter(|minutes| *minutes <= MAX_FOLLOWER_MINUTES)
                        .ok_or_else(|| {
                            CommandError::new(format!(
                                "Followers-only takes whole minutes, up to 3 months (like 30m, 1d, 2w or 3mo). {}",
                                usage_line(name)
                            ))
                        })?
                }
            };
            Ok(ModCommand::Room(RoomChange::Followers(Some(minutes))))
        }
        "followersoff" => no_args(ModCommand::Room(RoomChange::Followers(None))),
        "subscribers" => no_args(ModCommand::Room(RoomChange::Subscribers(true))),
        "subscribersoff" => no_args(ModCommand::Room(RoomChange::Subscribers(false))),
        "emoteonly" => no_args(ModCommand::Room(RoomChange::EmoteOnly(true))),
        "emoteonlyoff" => no_args(ModCommand::Room(RoomChange::EmoteOnly(false))),
        "uniquechat" => no_args(ModCommand::Room(RoomChange::Unique(true))),
        "uniquechatoff" => no_args(ModCommand::Room(RoomChange::Unique(false))),
        "mod" => only_user(words).map(|user| ModCommand::AddMod { user }),
        "unmod" => only_user(words).map(|user| ModCommand::RemoveMod { user }),
        "vip" => only_user(words).map(|user| ModCommand::AddVip { user }),
        "unvip" => only_user(words).map(|user| ModCommand::RemoveVip { user }),
        "raid" => only_user(words).map(|user| ModCommand::Raid { user }),
        "unraid" => no_args(ModCommand::Unraid),
        "shoutout" => only_user(words).map(|user| ModCommand::Shoutout { user }),
        "announce" => {
            if args.is_empty() {
                return Err(usage());
            }
            let chars = args.chars().count();
            if chars > MAX_ANNOUNCEMENT_CHARS {
                return Err(CommandError::new(format!(
                    "The announcement is too long ({chars}/{MAX_ANNOUNCEMENT_CHARS} characters)."
                )));
            }
            Ok(ModCommand::Announce {
                text: args.to_owned(),
            })
        }
        other => Err(CommandError::new(format!("Unknown command /{other}."))),
    }
}

fn usage_line(name: &str) -> String {
    format!("Usage: {}", usage_of(name))
}

/// The text after the first `skip` whitespace-separated words (trimmed), or `""`.
fn rest_after(args: &str, skip: usize) -> &str {
    let mut rest = args.trim_start();
    for _ in 0..skip {
        rest = match rest.split_once(char::is_whitespace) {
            Some((_, tail)) => tail.trim_start(),
            None => "",
        };
    }
    rest.trim()
}

fn reason_of(text: &str) -> Result<Option<String>, CommandError> {
    let chars = text.chars().count();
    if chars > MAX_REASON_CHARS {
        return Err(CommandError::new(format!(
            "The reason is too long ({chars}/{MAX_REASON_CHARS} characters)."
        )));
    }
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

/// A username as typed (`Bob`, `@bob`) as a lowercase login, or `None` when it cannot be one
/// (1 to 25 of `a-z`, `0-9`, `_`; Twitch's longest, and the characters its logins use).
pub fn login_of(text: &str) -> Option<String> {
    let login = text.strip_prefix('@').unwrap_or(text).to_ascii_lowercase();
    let valid = (1..=25).contains(&login.len())
        && login
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    valid.then_some(login)
}

/// What a bare number in a duration means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationUnit {
    Seconds,
    Minutes,
}

/// Parses `90`, `10m`, `2h`, `1d`, `1w`, `3mo` or a mix (`1h30m`) into seconds. A bare number
/// counts in `bare` units. `None` for anything else, a zero-length result excepted: `0` is `0`.
pub fn parse_duration(text: &str, bare: DurationUnit) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return None;
        }
        let amount: u64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let letters = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let unit = match &rest[..letters] {
            // A bare number is the whole duration, not the tail of `1m5`.
            "" if total > 0 => return None,
            "" => match bare {
                DurationUnit::Seconds => 1,
                DurationUnit::Minutes => 60,
            },
            "s" | "sec" | "secs" => 1,
            "m" | "min" | "mins" => 60,
            "h" | "hr" | "hrs" => 3_600,
            "d" | "day" | "days" => 86_400,
            "w" | "wk" | "wks" => 604_800,
            "mo" | "mos" => 2_592_000,
            _ => return None,
        };
        rest = &rest[letters..];
        total = total.checked_add(amount.checked_mul(unit)?)?;
        // A bare number can only be the last (and only) part.
        if letters == 0 && !rest.is_empty() {
            return None;
        }
    }
    Some(total)
}

/// `600` → `10m`, `5400` → `1h 30m`, `90` → `1m 30s`: at most the two largest non-zero units.
pub fn format_seconds(seconds: u64) -> String {
    let (weeks, days, hours, minutes, secs) = (
        seconds / 604_800,
        seconds % 604_800 / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
        seconds % 60,
    );
    let parts: Vec<String> = [
        (weeks, 'w'),
        (days, 'd'),
        (hours, 'h'),
        (minutes, 'm'),
        (secs, 's'),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .take(2)
    .map(|(n, unit)| format!("{n}{unit}"))
    .collect();
    if parts.is_empty() {
        "0s".to_owned()
    } else {
        parts.join(" ")
    }
}

/// The room's chat modes, as the room-mode menu lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomMode {
    EmoteOnly,
    Followers,
    Slow,
    Subscribers,
    Unique,
}

impl RoomMode {
    pub const ALL: [Self; 5] = [
        Self::EmoteOnly,
        Self::Followers,
        Self::Slow,
        Self::Subscribers,
        Self::Unique,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::EmoteOnly => "Emote-only",
            Self::Followers => "Followers-only",
            Self::Slow => "Slow mode",
            Self::Subscribers => "Subscribers-only",
            Self::Unique => "Unique chat",
        }
    }

    /// Whether the mode is on in `room`.
    pub fn is_on(self, room: &RoomState) -> bool {
        match self {
            Self::EmoteOnly => room.emote_only,
            Self::Followers => room.followers_only != FollowersOnly::Off,
            Self::Slow => !room.slow_mode.is_zero(),
            Self::Subscribers => room.subscribers_only,
            Self::Unique => room.unique_chat,
        }
    }

    /// The change that flips the mode: off when it is on, otherwise on with the default setting
    /// (every follower for followers-only, 30 s for slow mode).
    pub fn toggled(self, room: &RoomState) -> RoomChange {
        let on = !self.is_on(room);
        match self {
            Self::EmoteOnly => RoomChange::EmoteOnly(on),
            Self::Followers => RoomChange::Followers(on.then_some(0)),
            Self::Slow => RoomChange::Slow(on.then_some(DEFAULT_SLOW_SECS)),
            Self::Subscribers => RoomChange::Subscribers(on),
            Self::Unique => RoomChange::Unique(on),
        }
    }
}

/// The system line for a `CLEARCHAT` that happened at `at`: who was timed out or banned, or that
/// chat was cleared. Its id is made from what happened and when (the `tmi-sent-ts` of the event),
/// so the same event seen live and in recent-messages history is one line, not two.
pub fn clear_line(target: &ClearTarget, at: Timestamp, history: bool) -> Message {
    let (key, text) = match target {
        ClearTarget::Channel => (
            "chat".to_owned(),
            "Chat was cleared by a moderator.".to_owned(),
        ),
        ClearTarget::User {
            login,
            duration: Some(duration),
        } => (
            format!("timeout:{}", login.to_ascii_lowercase()),
            format!(
                "{login} was timed out for {}.",
                format_seconds(duration.as_secs())
            ),
        ),
        ClearTarget::User {
            login,
            duration: None,
        } => (
            format!("ban:{}", login.to_ascii_lowercase()),
            format!("{login} was banned."),
        ),
    };
    let mut message = Message::plain(
        format!("moderation:{key}:{}", at.unix_millis()),
        at,
        MessageKind::System,
        text,
    );
    message.flags.history = history;
    message
}

/// Which moderation controls one message offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RowActions {
    pub delete: bool,
    pub timeout: bool,
    pub ban: bool,
    pub unban: bool,
}

impl RowActions {
    pub fn any(self) -> bool {
        self.delete || self.timeout || self.ban || self.unban
    }
}

/// The moderation controls `message` gets in `channel` for a user who is `role` there and whose
/// own login is `own_login` (when known): none for viewers, for lines nobody wrote, for
/// events (a `USERNOTICE` card is not a chat line, whatever user text it carries: Twitch does
/// not document deleting those, so there is no Delete for them), for shared-chat copies of a
/// message sent in another channel, for the user's own messages and for the
/// broadcaster's; no timeout, ban or unban of another moderator unless the user is the
/// broadcaster (Twitch refuses it). Delete only for a chat line that is not yet removed; a
/// timeout or ban stays possible after a delete (the usual order); Unban only for a message
/// removed by a timeout or ban (Twitch answers "not banned" to an unban after a plain delete).
pub fn row_actions(
    role: ChannelRole,
    channel: &ChannelId,
    own_login: Option<&str>,
    message: &Message,
) -> RowActions {
    if !role.can_moderate() || message.flags.echo {
        return RowActions::default();
    }
    let Some(author) = &message.author else {
        return RowActions::default();
    };
    // A shared-chat copy was sent in another room (`source-room-id`): the user is a moderator of
    // this room, not of that one, and the message there is not this room's to delete.
    let shared_copy = message
        .shared
        .as_ref()
        .is_some_and(|shared| shared.is_copy_in(message.raw_tags.get("room-id")));
    if shared_copy
        || author.login.is_empty()
        || author.login.eq_ignore_ascii_case(channel.login())
        || own_login.is_some_and(|own| author.login.eq_ignore_ascii_case(own))
        || !matches!(message.kind, MessageKind::Chat | MessageKind::Action)
    {
        return RowActions::default();
    }
    let author_is_staff = author.badges.iter().any(|badge| {
        matches!(
            &*badge.set,
            "moderator" | "broadcaster" | "staff" | "admin" | "global_mod"
        )
    });
    let may_punish = role.is_broadcaster() || !author_is_staff;
    RowActions {
        delete: may_punish && !message.flags.deleted,
        timeout: may_punish,
        ban: may_punish,
        unban: may_punish && message.flags.removal.is_punishment(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::emote::BadgeId;
    use crate::message::Author;

    fn parsed(line: &str) -> Result<ModCommand, String> {
        let rest = line.strip_prefix('/').expect("a command");
        let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
        parse_mod_command(name, args)
            .unwrap_or_else(|| panic!("/{name} is not a moderation command"))
            .map_err(|error| error.to_string())
    }

    fn timeout(user: &str, seconds: u64, reason: Option<&str>) -> ModCommand {
        ModCommand::Timeout {
            user: user.into(),
            seconds,
            reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn timeout_takes_a_duration_and_a_reason() {
        assert_eq!(parsed("/timeout bob"), Ok(timeout("bob", 600, None)));
        assert_eq!(parsed("/timeout @Bob 90"), Ok(timeout("bob", 90, None)));
        assert_eq!(parsed("/timeout bob 10m"), Ok(timeout("bob", 600, None)));
        assert_eq!(
            parsed("/timeout bob 1h30m too   loud"),
            Ok(timeout("bob", 5400, Some("too   loud")))
        );
        // Not a duration: the rest is the reason, with the default length.
        assert_eq!(
            parsed("/timeout bob spamming links"),
            Ok(timeout("bob", 600, Some("spamming links")))
        );
        assert_eq!(parsed("/timeout bob 1w"), Ok(timeout("bob", 604_800, None)));
    }

    #[test]
    fn a_timeout_out_of_range_or_without_a_user_says_what_is_wrong() {
        for line in [
            "/timeout bob 0",
            "/timeout bob 15d",
            "/timeout bob 99999999999999999999",
        ] {
            let error = parsed(line).unwrap_err();
            assert!(error.contains("1s to 14d"), "{line}: {error}");
            assert!(error.contains("Usage: /timeout"), "{line}: {error}");
        }
        assert_eq!(
            parsed("/timeout").unwrap_err(),
            "Usage: /timeout <user> [duration] [reason]"
        );
        let error = parsed("/timeout b@d").unwrap_err();
        assert!(error.contains("not a Twitch username"), "{error}");
        let long = format!("/timeout bob 5m {}", "x".repeat(501));
        assert!(parsed(&long).unwrap_err().contains("501/500"));
    }

    #[test]
    fn ban_unban_and_delete() {
        assert_eq!(
            parsed("/ban Bob being rude"),
            Ok(ModCommand::Ban {
                user: "bob".into(),
                reason: Some("being rude".into())
            })
        );
        assert_eq!(
            parsed("/ban bob"),
            Ok(ModCommand::Ban {
                user: "bob".into(),
                reason: None
            })
        );
        assert_eq!(
            parsed("/unban bob"),
            Ok(ModCommand::Unban { user: "bob".into() })
        );
        assert_eq!(
            parsed("/untimeout @bob"),
            Ok(ModCommand::Unban { user: "bob".into() })
        );
        assert!(parsed("/unban bob extra").is_err());
        assert_eq!(
            parsed("/delete 6d3b-c1"),
            Ok(ModCommand::DeleteMessage {
                message_id: "6d3b-c1".into()
            })
        );
        assert_eq!(
            parsed("/delete").unwrap_err(),
            "Usage: /delete <message id>"
        );
    }

    #[test]
    fn room_modes() {
        let room = |c| Ok(ModCommand::Room(c));
        assert_eq!(parsed("/slow"), room(RoomChange::Slow(Some(30))));
        assert_eq!(parsed("/slow 10"), room(RoomChange::Slow(Some(10))));
        assert_eq!(parsed("/slow 2m"), room(RoomChange::Slow(Some(120))));
        for line in ["/slow 2", "/slow 121", "/slow x", "/slow 5 6"] {
            assert!(parsed(line).is_err(), "{line}");
        }
        assert_eq!(parsed("/slowoff"), room(RoomChange::Slow(None)));
        assert_eq!(parsed("/followers"), room(RoomChange::Followers(Some(0))));
        assert_eq!(
            parsed("/followers 30"),
            room(RoomChange::Followers(Some(30)))
        );
        assert_eq!(
            parsed("/followers 30m"),
            room(RoomChange::Followers(Some(30)))
        );
        assert_eq!(
            parsed("/followers 1d"),
            room(RoomChange::Followers(Some(1440)))
        );
        assert_eq!(
            parsed("/followers 2w"),
            room(RoomChange::Followers(Some(20_160)))
        );
        assert_eq!(
            parsed("/followers 3mo"),
            room(RoomChange::Followers(Some(129_600)))
        );
        assert_eq!(parsed("/followers 0"), room(RoomChange::Followers(Some(0))));
        assert!(parsed("/followers 4mo").is_err());
        assert!(parsed("/followers 30s").is_err());
        assert_eq!(parsed("/followersoff"), room(RoomChange::Followers(None)));
        assert_eq!(parsed("/subscribers"), room(RoomChange::Subscribers(true)));
        assert_eq!(
            parsed("/subscribersoff"),
            room(RoomChange::Subscribers(false))
        );
        assert_eq!(parsed("/emoteonly"), room(RoomChange::EmoteOnly(true)));
        assert_eq!(parsed("/emoteonlyoff"), room(RoomChange::EmoteOnly(false)));
        assert_eq!(parsed("/uniquechat"), room(RoomChange::Unique(true)));
        assert_eq!(parsed("/uniquechatoff"), room(RoomChange::Unique(false)));
        assert_eq!(parsed("/clear"), Ok(ModCommand::ClearChat));
        assert_eq!(parsed("/clear now").unwrap_err(), "Usage: /clear");
        assert_eq!(parsed("/slowoff 5").unwrap_err(), "Usage: /slowoff");
    }

    #[test]
    fn people_and_channels() {
        let user = || "bob".to_owned();
        assert_eq!(parsed("/mod bob"), Ok(ModCommand::AddMod { user: user() }));
        assert_eq!(
            parsed("/unmod bob"),
            Ok(ModCommand::RemoveMod { user: user() })
        );
        assert_eq!(parsed("/vip @BOB"), Ok(ModCommand::AddVip { user: user() }));
        assert_eq!(
            parsed("/unvip bob"),
            Ok(ModCommand::RemoveVip { user: user() })
        );
        assert_eq!(parsed("/raid bob"), Ok(ModCommand::Raid { user: user() }));
        assert_eq!(parsed("/unraid"), Ok(ModCommand::Unraid));
        assert_eq!(
            parsed("/shoutout bob"),
            Ok(ModCommand::Shoutout { user: user() })
        );
        assert_eq!(parsed("/mod").unwrap_err(), "Usage: /mod <user>");
        assert_eq!(
            parsed("/announce Starting in  5 minutes"),
            Ok(ModCommand::Announce {
                text: "Starting in  5 minutes".into()
            })
        );
        assert_eq!(
            parsed("/announce").unwrap_err(),
            "Usage: /announce <message>"
        );
        assert!(parsed(&format!("/announce {}", "a".repeat(501))).is_err());
    }

    #[test]
    fn other_names_are_not_moderation_commands() {
        assert!(parse_mod_command("me", "hi").is_none());
        assert!(parse_mod_command("w", "bob hi").is_none());
        assert!(parse_mod_command("nope", "").is_none());
    }

    #[test]
    fn every_listed_command_has_a_parser_and_usage_that_names_it() {
        for command in COMMANDS {
            assert!(command.usage.starts_with(&format!("/{}", command.name)));
            if command.name != "me" {
                assert!(
                    parse_mod_command(command.name, "").is_some(),
                    "/{} is listed but not parsed",
                    command.name
                );
            }
        }
    }

    #[test]
    fn commands_know_who_may_use_them() {
        assert!(ModCommand::AddMod { user: "a".into() }.needs_broadcaster());
        assert!(ModCommand::Unraid.needs_broadcaster());
        assert!(!ModCommand::ClearChat.needs_broadcaster());
        let ban = command_info("BAN").unwrap();
        assert!(!ban.role.allows(ChannelRole::Viewer));
        assert!(ban.role.allows(ChannelRole::Moderator));
        let raid = command_info("raid").unwrap();
        assert!(!raid.role.allows(ChannelRole::Moderator));
        assert!(raid.role.allows(ChannelRole::Broadcaster));
        assert!(command_info("me").unwrap().role.allows(ChannelRole::Viewer));
        for name in ["mod", "unmod", "vip", "unvip", "raid", "unraid"] {
            assert_eq!(command_info(name).unwrap().role, CommandRole::Broadcaster);
        }
        for name in [
            "timeout", "ban", "unban", "clear", "slow", "announce", "shoutout",
        ] {
            assert_eq!(command_info(name).unwrap().role, CommandRole::Moderator);
        }
    }

    #[test]
    fn the_hint_follows_a_complete_command_name() {
        assert_eq!(command_hint("/timeout bob").unwrap().name, "timeout");
        assert_eq!(command_hint("/TIMEOUT ").unwrap().name, "timeout");
        assert!(command_hint("/timeout").is_none(), "still being typed");
        assert!(command_hint("//timeout bob").is_none());
        assert!(command_hint("timeout bob").is_none());
        assert!(command_hint("/nope bob").is_none());
    }

    #[test]
    fn durations() {
        use DurationUnit::{Minutes, Seconds};
        assert_eq!(parse_duration("90", Seconds), Some(90));
        assert_eq!(parse_duration("90", Minutes), Some(5400));
        assert_eq!(parse_duration("10M", Seconds), Some(600));
        assert_eq!(parse_duration("1h30m", Seconds), Some(5400));
        assert_eq!(parse_duration("2d", Seconds), Some(172_800));
        assert_eq!(parse_duration("1mo", Seconds), Some(2_592_000));
        assert_eq!(parse_duration("0", Seconds), Some(0));
        for bad in [
            "",
            "m",
            "-5",
            "5x",
            "1m5",
            "10 m",
            "1.5h",
            "99999999999999999999d",
        ] {
            assert_eq!(parse_duration(bad, Seconds), None, "{bad:?}");
        }
    }

    #[test]
    fn durations_read_as_two_units() {
        assert_eq!(format_seconds(0), "0s");
        assert_eq!(format_seconds(45), "45s");
        assert_eq!(format_seconds(600), "10m");
        assert_eq!(format_seconds(5400), "1h 30m");
        assert_eq!(format_seconds(90), "1m 30s");
        assert_eq!(format_seconds(86_400), "1d");
        assert_eq!(format_seconds(MAX_TIMEOUT_SECS), "2w");
    }

    #[test]
    fn toggling_a_mode_flips_it_with_the_default_setting() {
        let mut room = RoomState::default();
        assert_eq!(
            RoomMode::EmoteOnly.toggled(&room),
            RoomChange::EmoteOnly(true)
        );
        assert_eq!(RoomMode::Slow.toggled(&room), RoomChange::Slow(Some(30)));
        assert_eq!(
            RoomMode::Followers.toggled(&room),
            RoomChange::Followers(Some(0))
        );
        room.emote_only = true;
        room.slow_mode = Duration::from_secs(10);
        room.followers_only = FollowersOnly::Minutes(1440);
        room.unique_chat = true;
        room.subscribers_only = true;
        for mode in RoomMode::ALL {
            assert!(mode.is_on(&room), "{mode:?}");
        }
        assert_eq!(
            RoomMode::EmoteOnly.toggled(&room),
            RoomChange::EmoteOnly(false)
        );
        assert_eq!(RoomMode::Slow.toggled(&room), RoomChange::Slow(None));
        assert_eq!(
            RoomMode::Followers.toggled(&room),
            RoomChange::Followers(None)
        );
        assert_eq!(
            RoomMode::Subscribers.toggled(&room),
            RoomChange::Subscribers(false)
        );
        assert_eq!(RoomMode::Unique.toggled(&room), RoomChange::Unique(false));
        // Followers-only "all followers" is on too.
        room.followers_only = FollowersOnly::All;
        assert!(RoomMode::Followers.is_on(&room));
    }

    #[test]
    fn the_clear_line_says_what_happened_and_has_a_stable_id() {
        let timed_out = ClearTarget::User {
            login: "alice".into(),
            duration: Some(Duration::from_secs(600)),
        };
        let line = clear_line(&timed_out, Timestamp(5_000), false);
        assert_eq!(line.kind, MessageKind::System);
        assert_eq!(line.plain_text(), "alice was timed out for 10m.");
        assert_eq!(line.timestamp, Timestamp(5_000));
        assert!(!line.flags.history);
        // The same event from history is the same line.
        let again = clear_line(&timed_out, Timestamp(5_000), true);
        assert_eq!(again.id, line.id);
        assert!(again.flags.history);
        assert_ne!(clear_line(&timed_out, Timestamp(5_001), false).id, line.id);
        let banned = ClearTarget::User {
            login: "alice".into(),
            duration: None,
        };
        let ban = clear_line(&banned, Timestamp(5_000), false);
        assert_eq!(ban.plain_text(), "alice was banned.");
        assert_ne!(
            ban.id, line.id,
            "a ban and a timeout at one instant are two lines"
        );
        assert_eq!(
            clear_line(&ClearTarget::Channel, Timestamp(1), false).plain_text(),
            "Chat was cleared by a moderator."
        );
    }

    fn message(login: &str, badges: &[&str]) -> Message {
        let mut author = Author::new(login, login);
        author.badges = badges.iter().map(|set| BadgeId::new(*set, "1")).collect();
        Message::chat("m1", Timestamp(0), author, Vec::new())
    }

    use crate::message::Removal;

    fn actions(role: ChannelRole, message: &Message) -> RowActions {
        row_actions(role, &ChannelId::new("streamer"), Some("me"), message)
    }

    const ALL_LIVE: RowActions = RowActions {
        delete: true,
        timeout: true,
        ban: true,
        unban: false,
    };

    #[test]
    fn viewers_and_lines_nobody_wrote_get_no_controls() {
        assert!(!actions(ChannelRole::Viewer, &message("alice", &[])).any());
        let notice = Message::plain("n", Timestamp(0), MessageKind::System, "hi");
        assert!(!actions(ChannelRole::Moderator, &notice).any());
        let mut echo = message("alice", &[]);
        echo.flags.echo = true;
        assert!(!actions(ChannelRole::Moderator, &echo).any());
    }

    #[test]
    fn a_moderator_may_act_on_viewers_not_on_the_broadcaster_themselves_or_other_staff() {
        assert_eq!(
            actions(ChannelRole::Moderator, &message("alice", &["subscriber"])),
            ALL_LIVE
        );
        assert!(!actions(ChannelRole::Moderator, &message("streamer", &[])).any());
        assert!(!actions(ChannelRole::Broadcaster, &message("Streamer", &[])).any());
        // Your own messages (from IRC, not only the echo) get no controls.
        assert!(!actions(ChannelRole::Moderator, &message("Me", &[])).any());
        assert!(!actions(ChannelRole::Broadcaster, &message("me", &["moderator"])).any());
        let other_mod = message("modguy", &["moderator"]);
        assert!(!actions(ChannelRole::Moderator, &other_mod).any());
        assert_eq!(actions(ChannelRole::Broadcaster, &other_mod), ALL_LIVE);
        // Without knowing who you are, the rest still applies.
        assert_eq!(
            row_actions(
                ChannelRole::Moderator,
                &ChannelId::new("streamer"),
                None,
                &message("alice", &[])
            ),
            ALL_LIVE
        );
    }

    #[test]
    fn after_a_delete_the_author_can_still_be_timed_out_or_banned_but_not_unbanned() {
        let deleted = message("alice", &[]).with_removal(Removal::Deleted);
        assert_eq!(
            actions(ChannelRole::Moderator, &deleted),
            RowActions {
                delete: false,
                timeout: true,
                ban: true,
                unban: false
            }
        );
        let cleared = message("alice", &[]).with_removal(Removal::Cleared);
        assert!(!actions(ChannelRole::Moderator, &cleared).unban);
    }

    #[test]
    fn unban_is_offered_for_what_a_timeout_or_ban_removed() {
        for removal in [Removal::TimedOut, Removal::Banned] {
            let removed = message("alice", &[]).with_removal(removal);
            assert_eq!(
                actions(ChannelRole::Moderator, &removed),
                RowActions {
                    delete: false,
                    timeout: true,
                    ban: true,
                    unban: true
                },
                "{removal:?}"
            );
        }
    }

    #[test]
    fn an_event_card_is_not_a_chat_line_it_gets_no_controls() {
        let mut notice = message("alice", &[]);
        notice.kind = MessageKind::UserNotice;
        assert!(!actions(ChannelRole::Moderator, &notice).any());
        assert!(!actions(ChannelRole::Broadcaster, &notice).any());
    }

    fn shared(source_room: &str, delivered_in: &str) -> Message {
        use crate::message::RawTags;
        use crate::special::SharedChat;
        let mut m = message("alice", &[]);
        m.shared = Some(SharedChat {
            source_room_id: source_room.into(),
            source_id: None,
            source_badges: Vec::new(),
            source_login: Some("otherchan".into()),
            source_name: None,
        });
        m.raw_tags = RawTags::new([("room-id".to_owned(), delivered_in.to_owned())]);
        m
    }

    #[test]
    fn a_shared_chat_copy_from_another_channel_gets_no_controls_here() {
        // Sent in room 20, delivered in room 10 (this one): not ours to moderate.
        let copy = shared("20", "10");
        assert!(!actions(ChannelRole::Moderator, &copy).any());
        assert!(!actions(ChannelRole::Broadcaster, &copy).any());
        // A message sent in this very room carries it as its source: a normal message.
        assert_eq!(
            actions(ChannelRole::Moderator, &shared("10", "10")),
            ALL_LIVE
        );
        // Removed or not, a copy stays without controls (no Unban of someone else's room either).
        let removed = shared("20", "10").with_removal(Removal::Banned);
        assert!(!actions(ChannelRole::Broadcaster, &removed).any());
    }

    #[test]
    fn an_event_card_with_user_text_has_no_controls_either() {
        let mut resub = message("alice", &[]);
        resub.kind = MessageKind::UserNotice;
        resub.system_text = Some("alice subscribed for 12 months".into());
        resub.notice_id = Some("resub".into());
        assert!(!resub.fragments.is_empty() || resub.system_text.is_some());
        assert!(!actions(ChannelRole::Broadcaster, &resub).any());
        // Not even once removed.
        assert!(
            !actions(
                ChannelRole::Broadcaster,
                &resub.with_removal(Removal::Banned)
            )
            .any()
        );
    }

    #[test]
    fn a_followers_requirement_is_whole_minutes() {
        assert!(parsed("/followers 90s").is_err());
        assert!(parsed("/followers 30s").is_err());
        assert_eq!(
            parsed("/followers 120s"),
            Ok(ModCommand::Room(RoomChange::Followers(Some(2))))
        );
        let error = parsed("/followers 90s").unwrap_err();
        assert!(error.contains("whole minutes"), "{error}");
    }

    #[test]
    fn untimeout_is_listed_and_means_unban() {
        assert_eq!(
            command_info("untimeout").unwrap().usage,
            "/untimeout <user>"
        );
        assert_eq!(command_hint("/untimeout bob").unwrap().name, "untimeout");
        assert_eq!(
            parsed("/untimeout bob"),
            Ok(ModCommand::Unban { user: "bob".into() })
        );
        assert_eq!(
            parsed("/untimeout").unwrap_err(),
            "Usage: /untimeout <user>"
        );
    }

    #[test]
    fn the_raid_says_it_starts_a_countdown() {
        let line = ModCommand::Raid { user: "bob".into() }
            .confirmation()
            .unwrap();
        assert!(line.contains("90 s") && line.contains("/unraid"), "{line}");
    }
}
