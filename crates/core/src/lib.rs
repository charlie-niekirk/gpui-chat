//! Domain types, message store and tokenizer. No I/O, no GPUI.
//!
//! * [`Message`] / [`Fragment`]: the immutable chat message (shared as `Arc<Message>`)
//!   and its tokenized body.
//! * [`MessageStore`]: per-channel ring buffer with lookup by id and change tracking.
//! * [`ignore`], [`filter`], [`search`]: ignored users and phrases, the find bar's filters and the
//!   incremental in-channel search (PR 5.7).
//! * [`compose`]: what the input box does with typed text (commands, limits, history).
//! * [`tokenizer`]: text → fragments (emote ranges, links, mentions, emoji, cheers).
//! * [`ChatEvent`]: what network backends send to the UI.
//! * [`APP_NAME`] / [`APP_ID`]: the one place the application identity lives.

pub mod cheer;
pub mod complete;
pub mod compose;
pub mod emote;
pub mod event;
pub mod filter;
pub mod highlight;
pub mod ids;
pub mod ignore;
pub mod message;
pub mod moderation;
pub mod search;
pub mod special;
pub mod store;
pub mod tokenizer;
pub mod user_card;

pub use cheer::{CheerTier, Cheermote, CheermoteSet};
pub use complete::{
    Chatter, Edit, MIN_COLON_QUERY, RECENT_EMOTES, Recents, Tier, Trigger, TriggerKind, apply,
    find_trigger, match_tier, rank, recent_chatters, word_insertion,
};
pub use compose::{
    HISTORY_CAPACITY, Input, InputError, InputHistory, MAX_MESSAGE_CHARS, Outgoing, parse_input,
    sanitize_line,
};
pub use emote::{Badge, BadgeId, Emote, EmoteProvider, ImageScale, ImageUrls};
pub use event::{
    ChannelRole, ChatEvent, ClearTarget, ConnectionState, FollowersOnly, Moderation, RoomState,
    RoomStateUpdate,
};
pub use filter::{BotList, DEFAULT_BOTS, FilterConfig, Filters, MAX_BOTS, Visibility, visibility};
pub use highlight::{
    HighlightColor, HighlightConfig, HighlightRule, HighlightUser, Highlights, Hit, HitReason, Own,
    ProblemKind, RuleKind, RuleProblem, rule_problem, user_problem,
};
pub use ids::{ChannelId, EmoteId, MessageId};
pub use ignore::{IgnoreConfig, IgnoreRule, Ignores, ignore_rule_problem, ignore_user_problem};
pub use message::{
    Author, Fragment, Message, MessageFlags, MessageKind, Removal, ReplyRef, Rgb, Timestamp,
};
pub use moderation::{
    COMMANDS, CommandError, CommandInfo, CommandRole, DEFAULT_TIMEOUT_SECS, FOLLOWER_PRESETS,
    ModCommand, RoomChange, RoomMode, RowActions, SLOW_PRESETS, TIMEOUT_PRESETS, clear_line,
    command_hint, command_info, format_seconds, row_actions,
};
pub use search::{
    IndexChange, MAX_QUERY_CHARS, Needle, Query, Search, SearchIndex, find_ranges, fold,
    parse_query, search_text,
};
pub use special::{AnnouncementColor, EventKind, Recipient, SharedChat, SubPlan};
pub use store::{DEFAULT_CAPACITY, MessageStore, Rebuild, Thread};
pub use tokenizer::{
    DEFAULT_CHEERMOTE_PREFIXES, EmoteRange, NoResolver, RawEmoteRange, Tokenized, Tokenizer,
    WordResolver, parse_emotes_tag, resolve_fragments, twitch_emote_ranges,
};
pub use user_card::{
    CardActions, CardMessages, KnownUser, MAX_CARD_MESSAGES, card_actions, duration_words,
    format_age, format_date, is_login, is_shared_copy, known_user, parse_rfc3339, profile_url,
    same_account, user_messages,
};

/// Name of this crate, used by the placeholder test below.
pub const CRATE_NAME: &str = "gpui-chat-core";

/// Human-facing application name. Working title; renaming the app starts here.
pub const APP_NAME: &str = "gpui-chat";

/// Reverse-DNS application identifier (bundle ID, keychain service, config dir
/// namespace). Renaming the app starts here.
pub const APP_ID: &str = "dev.gpui-chat.app";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_matches_package() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }

    #[test]
    fn app_identity_is_set() {
        assert!(!APP_NAME.is_empty());
        assert!(APP_ID.ends_with(".app"));
        assert!(APP_ID.contains('.'));
    }
}
