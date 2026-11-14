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

pub mod auth;
mod chat_send;
mod client;
pub mod helix;
mod limits;
mod parse;

pub use auth::Auth;
pub use chat_send::{SEND_CHAT_PATH, SendError, SentMessage, echo_message};
pub use client::{ClientConfig, Credentials, CredentialsProvider, TwitchClient, irc_login};
pub use helix::{HelixClient, HelixError, HelixResponse};
pub use limits::{Backoff, JoinRateLimit};
pub use parse::{ParseError, ParseOptions, map_message, parse_line, parse_line_with};

#[cfg(test)]
mod chat_tests;
