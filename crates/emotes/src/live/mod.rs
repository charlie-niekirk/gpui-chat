//! Live emote updates: 7TV's EventAPI and BetterTTV's socket.
//!
//! [`LiveEmotes`] keeps at most two connections (one per provider, shared by every open channel)
//! and only while there is something to listen to:
//!
//! * 7TV ([`seventv`]): per channel, `emote_set.update` of its active set and `user.update` of the
//!   owning user (a switch of the active set loads the new one), and — when paints or 7TV badges
//!   are on — the channel's `cosmetic.create` / `entitlement.*` events.
//! * BTTV ([`bttv`]): `join_channel twitch:<room id>`, then `emote_create` / `emote_update` /
//!   `emote_delete`.
//!
//! Changes are applied to the channel's set in the [`EmoteRegistry`] (so the next message
//! resolves against them) and reported as a quiet line, coalesced ([`notices`]), to the
//! [`LiveSink`]. Connections are re-made with exponential backoff ([`socket::Backoff`]) and
//! resubscribe everything; a channel that closes unsubscribes; a provider switched off in the
//! settings holds no connection.
//!
//! Everything protocol-shaped is parsed by pure functions in [`seventv`] and [`bttv`]; the I/O is
//! the small runner in [`socket`].

pub mod bttv;
mod manager;
pub mod notices;
mod sessions;
pub mod seventv;
pub mod socket;
#[cfg(test)]
mod testserver;

pub use manager::{Endpoints, LiveEmotes, LiveSink};
pub use socket::Backoff;
