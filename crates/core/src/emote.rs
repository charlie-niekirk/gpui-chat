//! Emote and badge data. Data only: nothing here fetches or decodes anything.

use std::sync::Arc;

use crate::ids::EmoteId;

/// Where an emote comes from. Decides tooltip text and resolution precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmoteProvider {
    Twitch,
    Bttv,
    Ffz,
    SevenTv,
}

impl EmoteProvider {
    /// Human-facing provider name (for tooltips).
    pub fn name(self) -> &'static str {
        match self {
            Self::Twitch => "Twitch",
            Self::Bttv => "BTTV",
            Self::Ffz => "FFZ",
            Self::SevenTv => "7TV",
        }
    }
}

/// Image scale factor. `X1` is the provider's base size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImageScale {
    X1,
    X2,
    X3,
    X4,
}

impl ImageScale {
    pub const ALL: [ImageScale; 4] = [Self::X1, Self::X2, Self::X3, Self::X4];

    /// The multiplier (1–4).
    pub fn factor(self) -> u8 {
        match self {
            Self::X1 => 1,
            Self::X2 => 2,
            Self::X3 => 3,
            Self::X4 => 4,
        }
    }

    fn index(self) -> usize {
        self.factor() as usize - 1
    }
}

/// Image URLs keyed by scale. Providers rarely offer all four scales.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageUrls {
    urls: [Option<Arc<str>>; 4],
}

impl ImageUrls {
    /// No URLs yet; add some with [`ImageUrls::with`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `self` with the URL for `scale` set.
    #[must_use]
    pub fn with(mut self, scale: ImageScale, url: impl Into<Arc<str>>) -> Self {
        self.urls[scale.index()] = Some(url.into());
        self
    }

    /// The URL for exactly this scale, if the provider has one.
    pub fn get(&self, scale: ImageScale) -> Option<&str> {
        self.urls[scale.index()].as_deref()
    }

    /// The best URL for a wanted scale: the smallest available scale that is at
    /// least `wanted` (sharp when downscaled), otherwise the largest one below it.
    pub fn best_for(&self, wanted: ImageScale) -> Option<(ImageScale, &str)> {
        let at_least = ImageScale::ALL.into_iter().filter(|s| *s >= wanted);
        let below = ImageScale::ALL.into_iter().rev().filter(|s| *s < wanted);
        at_least
            .chain(below)
            .find_map(|s| self.get(s).map(|url| (s, url)))
    }

    /// `true` if no scale has a URL.
    pub fn is_empty(&self) -> bool {
        self.urls.iter().all(Option::is_none)
    }
}

/// One emote, from any provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emote {
    pub id: EmoteId,
    /// The word users type to get it (`Kappa`, `monkaS`).
    pub name: Arc<str>,
    pub provider: EmoteProvider,
    pub urls: ImageUrls,
    /// Known to be animated before loading. Twitch emotes parsed from IRC tags
    /// cannot say, so they are `false`; the `default` CDN format serves the
    /// animated version when one exists, and the app learns it from the decoded
    /// image (more than one frame).
    pub animated: bool,
    /// Overlay emote: stacks on the preceding emote instead of taking its own slot.
    pub zero_width: bool,
    /// Base-scale pixel size when the provider reports it (for placeholder boxes).
    pub width: Option<u16>,
    pub height: Option<u16>,
}

impl Emote {
    /// A Twitch emote built from its id and name, with the standard CDN URLs
    /// (`1.0`, `2.0`, `3.0` map to scales 1x, 2x, 4x).
    pub fn twitch(id: impl Into<EmoteId>, name: impl Into<Arc<str>>) -> Self {
        let id: EmoteId = id.into();
        let base = format!(
            "https://static-cdn.jtvnw.net/emoticons/v2/{}/default/dark",
            id.as_str()
        );
        let urls = ImageUrls::new()
            .with(ImageScale::X1, format!("{base}/1.0"))
            .with(ImageScale::X2, format!("{base}/2.0"))
            .with(ImageScale::X4, format!("{base}/3.0"));
        Self {
            id,
            name: name.into(),
            provider: EmoteProvider::Twitch,
            urls,
            animated: false,
            zero_width: false,
            width: None,
            height: None,
        }
    }
}

/// A badge as it appears in IRC tags: just the set and version, no images.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BadgeId {
    /// Badge set (`subscriber`, `moderator`, `bits`, ...).
    pub set: Arc<str>,
    /// Version within the set (`0`, `12`, `1000`, ...).
    pub version: Arc<str>,
}

impl BadgeId {
    pub fn new(set: impl Into<Arc<str>>, version: impl Into<Arc<str>>) -> Self {
        Self {
            set: set.into(),
            version: version.into(),
        }
    }
}

/// A resolved badge definition (what a `BadgeSource` returns for a [`BadgeId`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub id: BadgeId,
    /// Tooltip title (`Moderator`, `Subscriber`).
    pub title: String,
    pub urls: ImageUrls,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_for_prefers_at_least_then_below() {
        let urls = ImageUrls::new()
            .with(ImageScale::X1, "a")
            .with(ImageScale::X4, "d");
        assert_eq!(urls.best_for(ImageScale::X1), Some((ImageScale::X1, "a")));
        assert_eq!(urls.best_for(ImageScale::X2), Some((ImageScale::X4, "d")));
        let only_low = ImageUrls::new().with(ImageScale::X1, "a");
        assert_eq!(
            only_low.best_for(ImageScale::X4),
            Some((ImageScale::X1, "a"))
        );
        assert_eq!(ImageUrls::new().best_for(ImageScale::X1), None);
        assert!(ImageUrls::new().is_empty());
    }

    #[test]
    fn twitch_emote_has_cdn_urls() {
        let e = Emote::twitch("25", "Kappa");
        assert_eq!(e.provider, EmoteProvider::Twitch);
        assert_eq!(
            e.urls.get(ImageScale::X1),
            Some("https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/1.0")
        );
        assert_eq!(e.urls.get(ImageScale::X3), None);
    }
}
