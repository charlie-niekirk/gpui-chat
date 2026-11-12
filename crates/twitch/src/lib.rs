//! Twitch chat transport: IRC over TLS (this PR), plus Helix and auth (later PRs).
//!
//! * [`parse_line`] / [`parse_line_with`]: pure mapping of one raw IRC line to a
//!   [`gpui_chat_core::ChatEvent`]. No network, no runtime; used by fixtures,
//!   `--replay`, and later by recent-messages history.
//! * [`TwitchClient`]: the live anonymous (or, later, authenticated) connection that
//!   feeds the same mapping, with reconnect, backoff and join pacing.

mod client;
mod limits;
mod parse;

pub use client::{ClientConfig, Credentials, TwitchClient};
pub use limits::{Backoff, JoinRateLimit};
pub use parse::{ParseError, ParseOptions, map_message, parse_line, parse_line_with};
