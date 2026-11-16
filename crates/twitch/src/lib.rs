//! Twitch: chat transport (IRC over TLS), login (OAuth device flow, tokens) and Helix.
//!
//! * [`parse_line`] / [`parse_line_with`]: pure mapping of one raw IRC line to a
//!   [`gpui_chat_core::ChatEvent`]. No network, no runtime; used by fixtures,
//!   `--replay`, and later by recent-messages history.
//! * [`TwitchClient`]: the live connection (anonymous, or logged in through a [`CredentialsProvider`]) that
//!   feeds the same mapping, with reconnect, backoff and join pacing.
//! * [`auth`]: device-code login, the [`auth::TokenStore`] seam and [`auth::Auth`], the single
//!   owner of the tokens (refresh, validation, logout).
//! * [`helix`]: [`HelixClient`], authenticated Helix GETs and JSON POSTs (refresh-and-retry on 401,
//!   rate limits); [`HelixClient::send_chat_message`] sends chat (PR 3.2).
//! * [`blocks`]: the Twitch block list (`GET` / `PUT` / `DELETE users/blocks`, PR 5.7).
//! * [`moderation`]: the moderation commands as Helix requests ([`HelixClient::moderate`], PR 5.2).
//! * [`streams`]: `Get Streams` / `Get Followed Streams` and the [`streams::StreamPoller`] behind
//!   the live dot, the header stream info and the followed-live sidebar.
//! * [`history`]: [`HistoryClient`], the recent-messages service behind the history shown on join
//!   and after a reconnect (PR 5.5).
//! * [`cheermotes`]: `Get Cheermotes`, a channel's cheermote set, for bits messages (PR 5.1).
//! * [`users`]: `Get Users` and `Get Channel Followers`, behind the user card (PR 5.4).
//! * [`user_emotes`]: `Get User Emotes`, the Twitch emotes the logged-in user may use (PR 3.3).

pub mod auth;
pub mod blocks;
mod chat_send;
pub mod cheermotes;
mod client;
pub mod helix;
pub mod history;
mod limits;
pub mod moderation;
mod parse;
pub mod room_names;
pub mod streams;
pub mod user_emotes;
pub mod users;

pub use auth::Auth;
pub use blocks::{BlockError, BlockedUser};
pub use chat_send::{SEND_CHAT_PATH, SendError, SentMessage, echo_message};
pub use client::{
    ClientConfig, Credentials, CredentialsProvider, TwitchClient, irc_login, settled_state,
};
pub use helix::{HelixClient, HelixError, HelixListError, HelixResponse};
pub use history::{
    Fetch, History, HistoryClient, HistoryError, HistoryRequest, HistoryTracker, gap_start,
    parse_history,
};
pub use limits::{Backoff, JoinRateLimit};
pub use moderation::ModError;
pub use parse::{ParseError, ParseOptions, map_message, parse_line, parse_line_with};
pub use room_names::{RoomName, RoomNames};
pub use user_emotes::UserEmote;
pub use users::{UserProfile, UserQuery};

#[cfg(test)]
mod chat_tests;
