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
//! * [`streams`]: `Get Streams` / `Get Followed Streams` and the [`streams::StreamPoller`] behind
//!   the live dot, the header stream info and the followed-live sidebar.
//! * [`history`]: [`HistoryClient`], the recent-messages service behind the history shown on join
//!   and after a reconnect (PR 5.5).
//! * [`user_emotes`]: `Get User Emotes`, the Twitch emotes the logged-in user may use (PR 3.3).

pub mod auth;
mod chat_send;
mod client;
pub mod helix;
pub mod history;
mod limits;
mod parse;
pub mod streams;
pub mod user_emotes;

pub use auth::Auth;
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
pub use parse::{ParseError, ParseOptions, map_message, parse_line, parse_line_with};
pub use user_emotes::UserEmote;

#[cfg(test)]
mod chat_tests;
