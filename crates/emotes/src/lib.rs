//! Third-party emote providers (BTTV, FFZ, 7TV): typed async clients that turn each
//! provider's public JSON API into [`EmoteSet`]s of core [`gpui_chat_core::Emote`]s.
//!
//! * [`EmoteLoader`] is the entry point: [`EmoteLoader::load_global`] and
//!   [`EmoteLoader::load_channel`] query all three providers concurrently and return a
//!   [`ProviderResults`] with one `Result` per provider, so one provider failing (timeout,
//!   5xx, bad JSON) never blocks the others. A channel with no account at a provider
//!   (HTTP 404) is an *empty set*, not an error.
//! * [`BttvClient`], [`FfzClient`] and [`SevenTvClient`] are the per-provider clients.
//! * HTTP goes through the small [`HttpClient`] trait ([`ReqwestHttp`] is the real thing,
//!   tests substitute a fake), so everything is testable offline.
//!
//! Twitch's own emotes come from IRC tags and are not handled here. [`EmoteRegistry`] keeps the
//! loaded sets (channel and global) and hands out an [`EmoteResolver`] per channel, the
//! [`gpui_chat_core::WordResolver`] for the tokenizer: channel 7TV → BTTV → FFZ, then global 7TV →
//! BTTV → FFZ, with per-provider switches ([`Providers`]).
//!
//! Twitch badges live in [`badges`]: the [`BadgeSource`] trait, the anonymous source (bundled global
//! snapshot + `api.ivr.fi` for a channel's), the Helix source used when logged in, and the
//! [`BadgeRegistry`] the chat view reads.
//!
//! See the module docs of [`bttv`], [`ffz`] and [`seventv`] for endpoint and mapping details.

pub mod badges;
pub mod bttv;
mod error;
pub mod ffz;
mod http;
mod lenient;
mod loader;
mod resolver;
mod set;
pub mod seventv;
#[cfg(test)]
mod testutil;

pub use badges::{
    AnonymousBadgeSource, BadgeError, BadgeFuture, BadgeRegistry, BadgeSet, BadgeSource,
    HelixBadgeSource,
};
pub use bttv::BttvClient;
pub use error::EmoteError;
pub use ffz::FfzClient;
pub use http::{
    HttpClient, HttpConfig, HttpError, HttpFuture, HttpResponse, ReqwestHttp, user_agent,
};
pub use loader::{EmoteLoader, ProviderResults};
pub use resolver::{EmoteRegistry, EmoteResolver, Providers};
pub use set::{EmoteScope, EmoteSet, SetEmote};
pub use seventv::SevenTvClient;

/// Checks that `id` is a plain Twitch user id (ASCII digits), so it can be put in a URL
/// path without escaping.
pub(crate) fn validate_user_id(id: &str) -> Result<&str, EmoteError> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        Ok(id)
    } else {
        Err(EmoteError::InvalidUserId(id.to_owned()))
    }
}

/// Makes a provider URL absolute: protocol-relative (`//cdn...`) gets `https:`.
pub(crate) fn absolute_url(url: &str) -> String {
    match url.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_id_validation() {
        assert!(validate_user_id("11148817").is_ok());
        for bad in ["", "abc", "12/34", "1 2", "-1", "１２"] {
            assert!(validate_user_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn absolute_urls() {
        assert_eq!(absolute_url("//cdn.7tv.app/x"), "https://cdn.7tv.app/x");
        assert_eq!(absolute_url("https://a/b"), "https://a/b");
    }
}
