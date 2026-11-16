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
pub mod catalog;
pub mod cosmetics;
mod error;
pub mod ffz;
mod http;
mod lenient;
pub mod live;
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
pub use catalog::EmoteCatalog;
pub use cosmetics::{CosmeticOptions, Cosmetics, Paint, PaintColor, PaintFill, Shadow, Stop};
pub use error::EmoteError;
pub use ffz::FfzClient;
pub use http::{
    HttpClient, HttpConfig, HttpError, HttpFuture, HttpResponse, ReqwestHttp, user_agent,
};
pub use live::{Backoff, Endpoints, LiveEmotes, LiveSink};
pub use loader::{EmoteLoader, ProviderResults};
pub use resolver::{EmoteRegistry, EmoteResolver, Providers};
pub use set::{EmoteChange, EmoteScope, EmoteSet, SetEmote, SetOrigin};
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

/// The hosts image URLs from the providers may point to (and their subdomains): the providers'
/// own CDNs, Chatterino's badge host and Twitch's CDN. Images come from provider APIs and live
/// sockets, which are untrusted: an `http` URL, a bare IP or any other host is refused rather
/// than fetched.
pub const TRUSTED_IMAGE_HOSTS: [&str; 7] = [
    "7tv.app",
    "7tv.io",
    "betterttv.net",
    "frankerfacez.com",
    "chatterino.com",
    // Where Chatterino's badge list actually serves its images from.
    "fourtf.com",
    "jtvnw.net",
];

/// Whether `url` is an `https` URL on one of [`TRUSTED_IMAGE_HOSTS`] (protocol-relative `//host`
/// URLs are made `https` first, as [`absolute_url`] does).
pub fn trusted_image_url(url: &str) -> bool {
    let absolute = absolute_url(url);
    let Ok(parsed) = reqwest::Url::parse(&absolute) else {
        return false;
    };
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    TRUSTED_IMAGE_HOSTS
        .iter()
        .any(|trusted| host == *trusted || host.ends_with(&format!(".{trusted}")))
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
    fn only_https_on_the_providers_hosts_is_trusted() {
        for good in [
            "https://cdn.7tv.app/emote/A/1x.webp",
            "//cdn.7tv.app/emote/A",
            "https://cdn.betterttv.net/emote/x/1x.webp",
            "https://cdn.frankerfacez.com/badge/2/1",
            "https://fourtf.com/chatterino/badges/topd.png",
            "https://static-cdn.jtvnw.net/emoticons/v2/1/default/dark/1.0",
        ] {
            assert!(trusted_image_url(good), "{good}");
        }
        for bad in [
            "",
            "http://cdn.7tv.app/emote/A",
            "https://evil.example/7tv.app",
            "https://7tv.app.evil.example/x",
            "https://not7tv.app/x",
            "https://user@cdn.7tv.app/x",
            "https://127.0.0.1/x",
            "ftp://cdn.7tv.app/x",
            "javascript:alert(1)",
        ] {
            assert!(!trusted_image_url(bad), "{bad}");
        }
    }

    #[test]
    fn absolute_urls() {
        assert_eq!(absolute_url("//cdn.7tv.app/x"), "https://cdn.7tv.app/x");
        assert_eq!(absolute_url("https://a/b"), "https://a/b");
    }
}
