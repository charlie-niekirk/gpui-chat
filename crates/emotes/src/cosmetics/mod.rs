//! Third-party cosmetics: badges next to a name and 7TV name paints.
//!
//! * 7TV: a user's *style* (`badge_id`, `paint_id`) comes from the GraphQL API ([`lookup`], one
//!   batched request for many chatters) and, live, from EventAPI `entitlement.*` events; the
//!   definitions come from the same API and `cosmetic.create`.
//! * FFZ (`api.frankerfacez.com/v1/badges/ids`, users by Twitch id) and Chatterino
//!   (`api.chatterino.com/badges`, users by Twitch id) publish one list of badge holders each,
//!   loaded once ([`bulk`]).
//! * BTTV's badges are SVG only and the image pipeline cannot draw SVG, so there are none.
//!
//! [`Cosmetics`] holds it all. A badge shows up in a message as an extra
//! [`gpui_chat_core::BadgeId`] (set `seventv` / `ffz` / `chatterino`) after Twitch's own, so
//! rendering, tooltips and copy are the ones every badge already has; the definitions live in the
//! [`crate::BadgeRegistry`]'s third-party layer, where a provider switched off hides them. Paints
//! are looked up by the chat view when it paints a name.

pub mod bulk;
pub mod lookup;
pub mod paint;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use gpui_chat_core::{Badge, BadgeId, ImageScale, ImageUrls};
use serde::Deserialize;
use serde_json::Value;

pub use paint::{Paint, PaintColor, PaintFill, Shadow, Stop, contrast, luminance, parse_paint};

use crate::badges::BadgeRegistry;
use crate::lenient::{null_default, parse_entries};
use crate::{absolute_url, seventv};

/// The badge set names of the third-party providers.
pub mod sets {
    pub const SEVEN_TV: &str = "seventv";
    pub const FFZ: &str = "ffz";
    pub const CHATTERINO: &str = "chatterino";
    /// All of them, in the order their badges are shown.
    pub const ALL: [&str; 3] = [SEVEN_TV, FFZ, CHATTERINO];
}

/// How long a chatter's 7TV style is trusted before it is looked up again.
pub const STYLE_TTL: Duration = Duration::from_secs(30 * 60);
/// The most styles kept. A busy channel shows tens of thousands of distinct chatters an hour, and
/// the EventAPI adds users who are in no chat of ours; past this the oldest go.
pub const STYLE_CAP: usize = 20_000;
/// The most paint definitions kept (7TV has a few hundred).
pub const PAINT_CAP: usize = 4_096;

/// Which cosmetics are shown (the `[badges]` settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CosmeticOptions {
    pub seventv_badges: bool,
    pub ffz_badges: bool,
    pub chatterino_badges: bool,
    pub paints: bool,
}

impl Default for CosmeticOptions {
    fn default() -> Self {
        Self {
            seventv_badges: true,
            ffz_badges: true,
            chatterino_badges: true,
            paints: true,
        }
    }
}

impl CosmeticOptions {
    /// Whether a source needs 7TV's per-user data at all.
    pub fn wants_seventv(self) -> bool {
        self.seventv_badges || self.paints
    }
}

/// What 7TV says a user wears.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserStyle {
    pub paint_id: Option<Arc<str>>,
    pub badge_id: Option<Arc<str>>,
}

struct StyleEntry {
    style: UserStyle,
    /// When it was looked up (or last changed by an event).
    at: Instant,
    /// Looked up in full. An entry made by an EventAPI entitlement knows one part only and does
    /// not stop the lookup.
    complete: bool,
}

#[derive(Default)]
struct Inner {
    paints: HashMap<Arc<str>, Arc<Paint>>,
    /// 7TV style by Twitch user id.
    styles: HashMap<Arc<str>, StyleEntry>,
    /// FFZ badge versions by Twitch user id.
    ffz: HashMap<Arc<str>, Vec<Arc<str>>>,
    /// Chatterino badge versions by Twitch user id.
    chatterino: HashMap<Arc<str>, Vec<Arc<str>>>,
}

impl Inner {
    fn prune(&mut self, now: Instant) {
        self.styles
            .retain(|_, entry| now.saturating_duration_since(entry.at) <= STYLE_TTL);
        if self.styles.len() > STYLE_CAP {
            let keep = STYLE_CAP - STYLE_CAP / 4;
            let mut by_age: Vec<(Arc<str>, Instant)> = self
                .styles
                .iter()
                .map(|(id, e)| (id.clone(), e.at))
                .collect();
            by_age.sort_by_key(|(_, at)| *at);
            let drop_count = by_age.len() - keep;
            for (id, _) in by_age.into_iter().take(drop_count) {
                self.styles.remove(&id);
            }
        }
    }
}

/// The cosmetics of every user seen so far.
pub struct Cosmetics {
    inner: RwLock<Inner>,
    badges: Arc<BadgeRegistry>,
    seventv_badges: AtomicBool,
    ffz_badges: AtomicBool,
    chatterino_badges: AtomicBool,
    paints_on: AtomicBool,
    /// Bumped by every store that can change what a name looks like.
    version: AtomicU64,
}

impl Cosmetics {
    pub fn new(badges: Arc<BadgeRegistry>, options: CosmeticOptions) -> Self {
        let this = Self {
            inner: RwLock::default(),
            badges,
            seventv_badges: AtomicBool::new(true),
            ffz_badges: AtomicBool::new(true),
            chatterino_badges: AtomicBool::new(true),
            paints_on: AtomicBool::new(true),
            version: AtomicU64::new(0),
        };
        this.set_options(options);
        this
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// A number that moves whenever a name's paint may have changed.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn options(&self) -> CosmeticOptions {
        CosmeticOptions {
            seventv_badges: self.seventv_badges.load(Ordering::Relaxed),
            ffz_badges: self.ffz_badges.load(Ordering::Relaxed),
            chatterino_badges: self.chatterino_badges.load(Ordering::Relaxed),
            paints: self.paints_on.load(Ordering::Relaxed),
        }
    }

    /// Applies the switches. A provider's badges are hidden (not forgotten) by the badge
    /// registry, so turning them back on needs no request.
    pub fn set_options(&self, options: CosmeticOptions) {
        if options == self.options() {
            return;
        }
        self.seventv_badges
            .store(options.seventv_badges, Ordering::Relaxed);
        self.ffz_badges.store(options.ffz_badges, Ordering::Relaxed);
        self.chatterino_badges
            .store(options.chatterino_badges, Ordering::Relaxed);
        self.paints_on.store(options.paints, Ordering::Relaxed);
        self.badges
            .set_extra_hidden(sets::SEVEN_TV, !options.seventv_badges);
        self.badges.set_extra_hidden(sets::FFZ, !options.ffz_badges);
        self.badges
            .set_extra_hidden(sets::CHATTERINO, !options.chatterino_badges);
        self.bump();
    }

    // --- 7TV -------------------------------------------------------------------------------

    /// Records a paint definition.
    pub fn store_paint(&self, paint: Paint) {
        let paint = Arc::new(paint);
        let mut inner = self.write();
        if inner.paints.len() >= PAINT_CAP && !inner.paints.contains_key(&paint.id) {
            return;
        }
        let changed = inner
            .paints
            .insert(paint.id.clone(), paint.clone())
            .is_none_or(|old| *old != *paint);
        drop(inner);
        if changed {
            self.bump();
        }
    }

    /// Records a 7TV badge definition in the badge registry.
    pub fn store_seventv_badge(&self, badge: Badge) {
        self.badges.store_extra([badge]);
    }

    /// Records what 7TV says `twitch_id` wears (looked up at `now`).
    pub fn set_style(&self, twitch_id: &str, style: UserStyle, now: Instant) {
        let entry = StyleEntry {
            style: style.clone(),
            at: now,
            complete: true,
        };
        let mut inner = self.write();
        let old = inner.styles.insert(Arc::from(twitch_id), entry);
        if inner.styles.len() > STYLE_CAP {
            inner.prune(now);
        }
        drop(inner);
        if old.is_none_or(|old| old.style.paint_id != style.paint_id) {
            self.bump();
        }
    }

    /// Drops styles past [`STYLE_TTL`] and, if the cache is still over [`STYLE_CAP`], the oldest
    /// ones until a quarter of the room is free again (so the work is not done at every insert).
    pub fn prune(&self, now: Instant) {
        self.write().prune(now);
    }

    /// How many styles are kept.
    pub fn style_count(&self) -> usize {
        self.read().styles.len()
    }

    /// An EventAPI entitlement: `twitch_id` got (`granted`) or lost the paint or badge `ref_id`.
    /// Losing one that is not the one worn changes nothing.
    pub fn entitle(&self, twitch_id: &str, paint: bool, ref_id: &str, granted: bool, now: Instant) {
        let mut inner = self.write();
        // Losing something nobody is known to wear is no news (and makes no entry).
        if !granted && !inner.styles.contains_key(twitch_id) {
            return;
        }
        let entry = inner
            .styles
            .entry(Arc::from(twitch_id))
            .or_insert_with(|| StyleEntry {
                style: UserStyle::default(),
                at: now,
                complete: false,
            });
        let slot = if paint {
            &mut entry.style.paint_id
        } else {
            &mut entry.style.badge_id
        };
        if granted {
            *slot = Some(Arc::from(ref_id));
        } else if slot.as_deref() == Some(ref_id) {
            *slot = None;
        } else {
            return;
        }
        if inner.styles.len() > STYLE_CAP {
            inner.prune(now);
        }
        drop(inner);
        self.bump();
    }

    /// Whether the paint or 7TV badge `id` is already defined.
    pub fn knows_cosmetic(&self, id: &str) -> bool {
        self.read().paints.contains_key(id) || self.badges.has_extra(sets::SEVEN_TV, id)
    }

    /// `true` if `twitch_id`'s style was never looked up, or not within [`STYLE_TTL`].
    pub fn needs_lookup(&self, twitch_id: &str, now: Instant) -> bool {
        match self.read().styles.get(twitch_id) {
            Some(entry) => !entry.complete || now.saturating_duration_since(entry.at) > STYLE_TTL,
            None => true,
        }
    }

    /// The paint `twitch_id`'s name is drawn with: only when paints are on, the user has one
    /// and its definition is known and can be drawn.
    pub fn paint(&self, twitch_id: &str) -> Option<Arc<Paint>> {
        if !self.paints_on.load(Ordering::Relaxed) {
            return None;
        }
        let inner = self.read();
        let id = inner.styles.get(twitch_id)?.style.paint_id.as_ref()?;
        inner
            .paints
            .get(id)
            .filter(|paint| paint.is_drawable())
            .cloned()
    }

    // --- FFZ and Chatterino ------------------------------------------------------------------

    /// Records FFZ's badges (definitions) and who has which (Twitch user id → badge versions).
    pub fn store_ffz(&self, badges: Vec<Badge>, holders: HashMap<Arc<str>, Vec<Arc<str>>>) {
        self.badges.store_extra(badges);
        self.write().ffz = holders;
    }

    /// Records Chatterino's badges and who has which (`holders`: Twitch user id → versions).
    pub fn store_chatterino(&self, badges: Vec<Badge>, holders: HashMap<Arc<str>, Vec<Arc<str>>>) {
        self.badges.store_extra(badges);
        self.write().chatterino = holders;
    }

    // --- Messages ----------------------------------------------------------------------------

    /// The third-party badges of a chatter (Twitch user id), in display order (7TV, FFZ,
    /// Chatterino). Providers that are off are still listed: the badge registry hides their
    /// definitions, so a switch takes effect on messages already on screen.
    pub fn badge_ids(&self, twitch_id: &str) -> Vec<BadgeId> {
        let inner = self.read();
        let mut ids = Vec::new();
        if let Some(entry) = inner.styles.get(twitch_id)
            && let Some(badge) = &entry.style.badge_id
        {
            ids.push(BadgeId::new(sets::SEVEN_TV, badge.clone()));
        }
        for version in inner.ffz.get(twitch_id).into_iter().flatten() {
            ids.push(BadgeId::new(sets::FFZ, version.clone()));
        }
        for version in inner.chatterino.get(twitch_id).into_iter().flatten() {
            ids.push(BadgeId::new(sets::CHATTERINO, version.clone()));
        }
        ids
    }
}

// --- parsing 7TV badges ----------------------------------------------------------------------

#[derive(Deserialize)]
struct RawFile {
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    format: String,
}

#[derive(Deserialize)]
struct RawHost {
    url: String,
    #[serde(default, deserialize_with = "null_default")]
    files: Vec<Value>,
}

#[derive(Deserialize)]
struct RawBadge {
    id: String,
    #[serde(default, deserialize_with = "null_default")]
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    tooltip: String,
    host: Option<RawHost>,
}

/// A 7TV badge (GraphQL `Badge` / EventAPI cosmetic data) as a [`Badge`] of set `seventv`, whose
/// title names the provider. `None` when it has no usable image.
pub fn parse_seventv_badge(value: &Value) -> Option<Badge> {
    let raw: RawBadge = match serde_json::from_value(value.clone()) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::debug!(%error, "skipping malformed 7TV badge");
            return None;
        }
    };
    let host = raw.host?;
    let base = absolute_url(&host.url);
    if !crate::trusted_image_url(&base) {
        return None;
    }
    let mut best: [Option<(u8, String)>; 4] = [None, None, None, None];
    for file in parse_entries::<RawFile>("7TV badge file", host.files) {
        let (Some(scale), Some(rank)) = (
            seventv::file_scale(&file.name),
            seventv::format_rank(&file.format),
        ) else {
            continue;
        };
        let slot = &mut best[usize::from(scale.factor()) - 1];
        if slot.as_ref().is_none_or(|(r, _)| rank < *r) {
            *slot = Some((rank, file.name));
        }
    }
    let mut urls = ImageUrls::new();
    for scale in ImageScale::ALL {
        if let Some((_, name)) = &best[usize::from(scale.factor()) - 1] {
            urls = urls.with(scale, format!("{base}/{name}"));
        }
    }
    if urls.is_empty() {
        return None;
    }
    let what = if raw.tooltip.is_empty() {
        raw.name
    } else {
        raw.tooltip
    };
    Some(Badge {
        id: BadgeId::new(sets::SEVEN_TV, raw.id),
        title: format!("7TV: {what}"),
        urls,
    })
}

#[cfg(test)]
mod tests;
