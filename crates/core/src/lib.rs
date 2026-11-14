//! Domain types, message store and tokenizer. No I/O, no GPUI.
//!
//! * [`Message`] / [`Fragment`]: the immutable chat message (shared as `Arc<Message>`)
//!   and its tokenized body.
//! * [`MessageStore`]: per-channel ring buffer with lookup by id and change tracking.
//! * [`compose`]: what the input box does with typed text (commands, limits, history).
//! * [`tokenizer`]: text → fragments (emote ranges, links, mentions, emoji, cheers).
//! * [`ChatEvent`]: what network backends send to the UI.
//! * [`APP_NAME`] / [`APP_ID`]: the one place the application identity lives.

pub mod compose;
pub mod emote;
pub mod event;
pub mod ids;
pub mod message;
pub mod store;
pub mod tokenizer;

pub use compose::{
    HISTORY_CAPACITY, Input, InputError, InputHistory, MAX_MESSAGE_CHARS, Outgoing, parse_input,
    sanitize_line,
};
pub use emote::{Badge, BadgeId, Emote, EmoteProvider, ImageScale, ImageUrls};
pub use event::{
    ChatEvent, ClearTarget, ConnectionState, FollowersOnly, RoomState, RoomStateUpdate,
};
pub use ids::{ChannelId, EmoteId, MessageId};
pub use message::{Author, Fragment, Message, MessageFlags, MessageKind, ReplyRef, Rgb, Timestamp};
pub use store::{DEFAULT_CAPACITY, MessageStore};
pub use tokenizer::{
    DEFAULT_CHEERMOTE_PREFIXES, EmoteRange, NoResolver, RawEmoteRange, Tokenized, Tokenizer,
    WordResolver, parse_emotes_tag, resolve_fragments, twitch_emote_ranges,
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
