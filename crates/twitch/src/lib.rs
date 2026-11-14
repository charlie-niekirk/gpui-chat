//! Twitch: chat transport (IRC over TLS), login (OAuth device flow, tokens) and Helix.
//!
//! * [`parse_line`] / [`parse_line_with`]: pure mapping of one raw IRC line to a
//!   [`gpui_chat_core::ChatEvent`]. No network, no runtime; used by fixtures,
//!   `--replay`, and later by recent-messages history.
//! * [`TwitchClient`]: the live anonymous (or, later, authenticated) connection that
//!   feeds the same mapping, with reconnect, backoff and join pacing.
//! * [`auth`]: device-code login, the [`auth::TokenStore`] seam and [`auth::Auth`], the single
//!   owner of the tokens (refresh, validation, logout).
//! * [`helix`]: [`HelixClient`], authenticated Helix GETs (refresh-and-retry on 401, rate limits).

pub mod auth;
mod client;
pub mod helix;
mod limits;
mod parse;

pub use auth::Auth;
pub use client::{ClientConfig, Credentials, TwitchClient};
pub use helix::{HelixClient, HelixError, HelixResponse};
pub use limits::{Backoff, JoinRateLimit};
pub use parse::{ParseError, ParseOptions, map_message, parse_line, parse_line_with};
