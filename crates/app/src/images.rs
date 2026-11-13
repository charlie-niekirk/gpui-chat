//! The app side of the image pipeline: `gpui-chat-media`'s cache holding GPUI images, the global
//! animation clock, and GPU texture release.
//!
//! * **Pixels → GPUI.** [`EmoteImage`] is media's [`CacheValue`]: right after decoding (on the
//!   blocking pool) the BGRA, straight-alpha frames are moved without a copy into an
//!   `image::Frame` each and wrapped in one GPUI [`RenderImage`], which is what `paint_image`
//!   draws (`gpui-pre 0.3.12` keeps `RenderImage` frames as straight-alpha BGRA too). The pixels
//!   are held once, by the cache. The GPU texture (a sprite-atlas tile per frame) is created
//!   lazily on the UI thread by the first `Window::paint_image` of each frame.
//! * **Textures are released on eviction.** Media's `on_evict` hands evicted images over; they wait
//!   in a graveyard until nothing on screen holds them any more and then `App::drop_image` removes
//!   their tiles from every window's atlas.
//! * **Visible images are pinned.** Each view keeps the images it painted in its last frame
//!   ([`FrameImages`]); paint looks there before asking the cache, so an image the byte budget
//!   evicted while on screen keeps drawing (and keeps its texture) instead of being refetched
//!   every frame. Painted images are also touched in the LRU, so off-screen ones go first.
//! * **Waking views.** A view that painted a placeholder registers as a waiter; when loads
//!   finish, one task on the UI thread notifies every waiter once for the whole batch.
//! * **Layout stability.** Emote boxes are sized before their image arrives (the provider's
//!   reported size, or Twitch's 28 px square), so lines do not jump when it does. If a finished
//!   image turns out to have another size, or fails and falls back to text, [`Images::epoch`]
//!   moves on and views re-shape their messages.
//! * **One animation clock.** Every animated image picks its frame from the time since
//!   [`Images`] was created, so identical emotes animate in sync wherever they are.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpui_chat_core::{Emote, ImageScale};
use gpui_chat_media::scale::{self, logical_size};
use gpui_chat_media::{
    CacheValue, DecodedImage, ImageCache, ImageCacheBuilder, ImageCacheConfig, ImageKey,
    ImageStatus, Timeline,
};
use gpui_kit::{App, EntityId, Global, RenderImage, Task, Window};
use tokio::sync::Notify;

/// Base size (logical px at emote scale 1) of an emote whose provider does not report one:
/// Twitch emotes are 28 × 28 at 1x.
pub const DEFAULT_EMOTE_SIZE: f32 = 28.0;

/// A size change smaller than this (logical px) does not re-shape anything.
const SIZE_SLACK: f32 = 0.5;

/// Shortest wait between animation wake-ups, so a frame change due "now" cannot spin.
const MIN_ANIMATION_WAKE: Duration = Duration::from_millis(4);

/// A decoded image as GPUI draws it: all frames in one [`RenderImage`], plus their timing.
pub struct EmoteImage {
    image: Arc<RenderImage>,
    timeline: Timeline,
    pixel_size: (u32, u32),
    bytes: usize,
}

impl CacheValue for EmoteImage {
    fn from_decoded(decoded: DecodedImage) -> Self {
        let (width, height) = (decoded.width(), decoded.height());
        let bytes = decoded.byte_size();
        let timeline = decoded.timeline().clone();
        let frames: Vec<image::Frame> = decoded
            .into_frames()
            .into_iter()
            .filter_map(|frame| {
                let delay = image::Delay::from_saturating_duration(frame.delay());
                // `DecodedImage` guarantees `width * height * 4` bytes per frame.
                image::RgbaImage::from_raw(width, height, frame.into_pixels())
                    .map(|buffer| image::Frame::from_parts(buffer, 0, 0, delay))
            })
            .collect();
        Self {
            image: Arc::new(RenderImage::new(frames)),
            timeline,
            pixel_size: (width, height),
            bytes,
        }
    }

    fn byte_size(&self) -> usize {
        self.bytes
    }
}

impl EmoteImage {
    pub fn render_image(&self) -> &Arc<RenderImage> {
        &self.image
    }

    /// Size in device pixels of the variant that was downloaded.
    pub fn pixel_size(&self) -> (u32, u32) {
        self.pixel_size
    }

    /// More than one frame.
    pub fn is_animated(&self) -> bool {
        self.timeline.len() > 1
    }

    /// Whether `paint_image` can draw it (a frame survived conversion).
    pub fn is_drawable(&self) -> bool {
        self.image.frame_count() > 0
    }

    /// The frame to draw and how long until it changes. See [`frame_for`].
    pub fn frame(&self, clock: Option<Duration>) -> (usize, Option<Duration>) {
        let (index, next) = frame_for(&self.timeline, clock);
        (index.min(self.image.frame_count().saturating_sub(1)), next)
    }
}

/// The frame of `timeline` to show at `clock` (time since the global clock started), and how
/// long until the next frame change. `None` means animation is off: the first frame, forever.
/// Every image asked with the same clock shows the frame a browser that started all of them at
/// time zero would show, so identical emotes stay in sync.
pub fn frame_for(timeline: &Timeline, clock: Option<Duration>) -> (usize, Option<Duration>) {
    match clock {
        Some(elapsed) => timeline.frame_at(elapsed),
        None => (0, None),
    }
}

/// When a view should be woken for its next animation frame: only while animation is on, the
/// window is active and something animated was painted.
pub fn animation_wake(
    next_change: Option<Duration>,
    animate: bool,
    active: bool,
) -> Option<Duration> {
    next_change
        .filter(|_| animate && active)
        .map(|delay| delay.max(MIN_ANIMATION_WAKE))
}

/// The URL `url` for the light theme (Twitch's CDN serves `light` and `dark` variants of a few
/// emotes); other providers' URLs are unchanged.
pub fn themed_url(url: &str, light: bool) -> std::borrow::Cow<'_, str> {
    const TWITCH_CDN: &str = "https://static-cdn.jtvnw.net/emoticons/";
    if light && url.starts_with(TWITCH_CDN) && url.contains("/dark/") {
        std::borrow::Cow::Owned(url.replacen("/dark/", "/light/", 1))
    } else {
        std::borrow::Cow::Borrowed(url)
    }
}

/// How an emote is drawn, decided when its message is shaped.
#[derive(Clone, Debug, PartialEq)]
pub enum EmoteBox {
    /// An image box of this logical size; `url` is the variant to draw.
    Image { url: ImageKey, size: (f32, f32) },
    /// No image (no URLs, or it failed to load): draw the emote's name as text.
    Text,
}

/// What paint has for an image right now.
pub enum Lookup {
    Ready(Arc<EmoteImage>),
    Loading,
    Failed,
}

/// The images one view painted, so they stay pinned while on screen, plus its animation timer.
#[derive(Default)]
pub struct FrameImages {
    /// Painted in the last finished frame (what is on screen).
    shown: HashMap<ImageKey, Arc<EmoteImage>>,
    /// Painted so far in the frame being drawn.
    painting: HashMap<ImageKey, Arc<EmoteImage>>,
    /// Soonest frame change among animated images painted this frame.
    next_change: Option<Duration>,
    wake: Option<(Instant, Task<()>)>,
}

impl FrameImages {
    /// Call when the view renders, before its elements paint: the last frame's images become the
    /// pinned (shown) set and the previous pins are let go.
    pub fn begin_frame(&mut self) {
        self.shown = std::mem::take(&mut self.painting);
        self.next_change = None;
    }

    fn get(&self, key: &str) -> Option<&Arc<EmoteImage>> {
        self.painting.get(key).or_else(|| self.shown.get(key))
    }

    fn record(&mut self, key: &ImageKey, image: &Arc<EmoteImage>) {
        if !self.painting.contains_key(key.url()) {
            self.painting.insert(key.clone(), image.clone());
        }
    }

    /// Notes that an animated image painted this frame changes frame after `delay`.
    pub fn animate_in(&mut self, delay: Duration) {
        self.next_change = Some(self.next_change.map_or(delay, |d| d.min(delay)));
    }

    /// After painting: wakes `entity` when the soonest animation frame is due, if `animate` and
    /// the window is `active`; otherwise cancels any pending wake. An earlier pending wake is
    /// kept rather than replaced.
    pub fn schedule_wake(&mut self, entity: EntityId, animate: bool, active: bool, cx: &App) {
        let Some(delay) = animation_wake(self.next_change, animate, active) else {
            self.wake = None;
            return;
        };
        let now = Instant::now();
        let deadline = now + delay;
        if let Some((at, _)) = &self.wake
            && *at > now
            && *at <= deadline
        {
            return;
        }
        let timer = cx.background_executor().timer(delay);
        let task = cx.spawn(async move |cx| {
            timer.await;
            cx.update(|cx| cx.notify(entity));
        });
        self.wake = Some((deadline, task));
    }

    /// Whether an animation wake is scheduled.
    #[cfg(test)]
    pub fn wake_pending(&self) -> bool {
        self.wake.is_some()
    }
}

/// What a load in flight was assumed to look like when its message was shaped.
struct Expected {
    size: (f32, f32),
    variant: ImageScale,
    emote_scale: f32,
}

/// Events from media's callbacks (tokio threads) for the UI thread.
#[derive(Default)]
struct Pending {
    ready: Vec<ImageKey>,
    evicted: Vec<Arc<EmoteImage>>,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<Pending>,
    notify: Notify,
}

impl Shared {
    fn push(&self, f: impl FnOnce(&mut Pending)) {
        f(&mut self.pending.lock().unwrap_or_else(PoisonError::into_inner));
        self.notify.notify_one();
    }

    fn take(&self) -> Pending {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The app's image store. A GPUI global; see the module docs.
pub struct Images {
    /// `None` when there is no backend (headless tests): every image stays a placeholder.
    cache: Option<ImageCache<EmoteImage>>,
    shared: Arc<Shared>,
    clock_start: Instant,
    waiters: HashSet<EntityId>,
    expected: HashMap<ImageKey, Expected>,
    epoch: u64,
    /// Evicted images still held by a view (on screen); their textures go once they are not.
    graveyard: Vec<Arc<EmoteImage>>,
    _pump: Option<Task<()>>,
}

impl Global for Images {}

impl Images {
    fn new(cache: Option<ImageCache<EmoteImage>>, shared: Arc<Shared>) -> Self {
        Self {
            cache,
            shared,
            clock_start: Instant::now(),
            waiters: HashSet::new(),
            expected: HashMap::new(),
            epoch: 0,
            graveyard: Vec::new(),
            _pump: None,
        }
    }

    /// Builds the cache from `builder` (runtime, config, fetcher) and installs the global, with
    /// the task that applies finished loads and evictions on the UI thread.
    pub fn install(builder: ImageCacheBuilder<EmoteImage>, cx: &mut App) {
        Self::install_with(builder, true, cx);
    }

    /// [`Self::install`]; without `pump` nothing applies finished loads until [`Self::pump`] is
    /// called (tests: GPUI's test scheduler rejects wake-ups from tokio threads).
    fn install_with(builder: ImageCacheBuilder<EmoteImage>, pump: bool, cx: &mut App) {
        let shared = Arc::new(Shared::default());
        let cache = {
            let (ready, evicted) = (shared.clone(), shared.clone());
            builder
                .on_ready(move |key| ready.push(|p| p.ready.push(key.clone())))
                .on_evict(move |_, image| evicted.push(|p| p.evicted.push(image.clone())))
                .build()
        };
        let cache = match cache {
            Ok(cache) => Some(cache),
            Err(error) => {
                tracing::warn!(%error, "image cache unavailable; emotes are drawn as text");
                None
            }
        };
        let mut images = Self::new(cache, shared.clone());
        if pump {
            images._pump = Some(cx.spawn(async move |cx| {
                loop {
                    shared.notify.notified().await;
                    cx.update(Self::pump);
                }
            }));
        }
        cx.set_global(images);
    }

    /// The media cache, if there is one.
    #[cfg(test)]
    pub fn cache(&self) -> Option<&ImageCache<EmoteImage>> {
        self.cache.as_ref()
    }

    /// Time on the global animation clock.
    pub fn clock(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.clock_start)
    }

    /// Moves on whenever an emote's box changed after it was laid out (a different size, or a
    /// failure that falls back to text); views re-shape when it does.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Decides how `emote` is drawn and starts loading its image. Called when a message is
    /// shaped, so loads start for rows about to be shown.
    pub fn emote_box(
        &mut self,
        emote: &Emote,
        display_scale: f32,
        emote_scale: f32,
        light: bool,
    ) -> EmoteBox {
        let Some(selected) = scale::select(&emote.urls, display_scale, emote_scale) else {
            return EmoteBox::Text;
        };
        let url = ImageKey::new(themed_url(selected.url, light).as_ref());
        let assumed = assumed_size(emote, emote_scale);
        let Some(cache) = &self.cache else {
            return EmoteBox::Image { url, size: assumed };
        };
        match cache.status(url.url()) {
            ImageStatus::Ready(image) => EmoteBox::Image {
                size: logical_size(image.pixel_size(), selected.scale, emote_scale),
                url,
            },
            ImageStatus::Failed(_) => EmoteBox::Text,
            status => {
                if matches!(status, ImageStatus::Absent) {
                    drop(cache.request(url.url()));
                }
                self.expected.insert(
                    url.clone(),
                    Expected {
                        size: assumed,
                        variant: selected.scale,
                        emote_scale,
                    },
                );
                EmoteBox::Image { url, size: assumed }
            }
        }
    }

    /// The URL and logical size of a larger preview of `emote` for the tooltip: its densest
    /// variant (up to 4x), drawn one image pixel per device pixel so it stays sharp (a Twitch
    /// emote is 112 px: 56 pt on a Retina display, twice its chat size). `None` if it has no
    /// image. Does not affect chat layout.
    pub fn preview(
        &mut self,
        emote: &Emote,
        display_scale: f32,
        light: bool,
    ) -> Option<(ImageKey, (f32, f32))> {
        let selected = scale::select(&emote.urls, 1.0, f32::from(ImageScale::X4.factor()))?;
        let url = ImageKey::new(themed_url(selected.url, light).as_ref());
        // `logical_size(pixels, variant, s)` is `pixels / variant * s`: one pixel per device pixel.
        let scale = f32::from(selected.scale.factor()) / display_scale.max(1.0);
        let size = match self.cache.as_ref().map(|cache| cache.status(url.url())) {
            Some(ImageStatus::Ready(image)) => {
                logical_size(image.pixel_size(), selected.scale, scale)
            }
            Some(ImageStatus::Failed(_)) => return None,
            _ => assumed_size(emote, scale),
        };
        Some((url, size))
    }

    /// Whether the image at `url` is loaded and animated. This is how Twitch emotes, whose IRC
    /// tags cannot say, are known to be animated: the `default` CDN format serves a GIF for them.
    pub fn is_animated(&self, url: &ImageKey) -> bool {
        self.cache.as_ref().is_some_and(|cache| {
            matches!(cache.status(url.url()), ImageStatus::Ready(image) if image.is_animated())
        })
    }

    /// The image for `url` to paint now: pinned in `frame` or ready in the cache, else its load
    /// is started or joined and `waiter` is notified when it finishes. Marks it painted.
    pub fn lookup(&mut self, url: &ImageKey, frame: &mut FrameImages, waiter: EntityId) -> Lookup {
        let Some(cache) = &self.cache else {
            return Lookup::Loading;
        };
        // Touch the LRU first so on-screen images are the most recently used.
        let image = match cache.get_if_ready(url.url()) {
            Some(image) => image,
            None => match frame.get(url.url()) {
                Some(image) => image.clone(),
                None => match cache.request(url.url()).try_get() {
                    Some(Ok(image)) => image,
                    Some(Err(_)) => return Lookup::Failed,
                    None => {
                        self.waiters.insert(waiter);
                        return Lookup::Loading;
                    }
                },
            },
        };
        frame.record(url, &image);
        Lookup::Ready(image)
    }

    /// Releases the textures of evicted images nothing on screen holds any more. Call from a
    /// view's render (with its window, which GPUI has taken out of the app while drawing).
    pub fn sweep(window: &mut Window, cx: &mut App) {
        let released = match cx.try_global::<Self>() {
            Some(images) if !images.graveyard.is_empty() => cx.global_mut::<Self>().collect(),
            _ => return,
        };
        for image in released {
            cx.drop_image(image, Some(&mut *window));
        }
    }

    /// Takes the evicted images that only the graveyard still holds.
    fn collect(&mut self) -> Vec<Arc<RenderImage>> {
        let mut released = Vec::new();
        self.graveyard.retain(|image| {
            let held = Arc::strong_count(image) > 1;
            if !held {
                released.push(image.image.clone());
            }
            held
        });
        released
    }

    /// Applies finished loads and evictions (UI thread, once per batch).
    fn pump(cx: &mut App) {
        let Some(images) = cx.try_global::<Self>() else {
            return;
        };
        let Pending { ready, evicted } = images.shared.take();
        let images = cx.global_mut::<Self>();
        for key in &ready {
            images.settle(key);
        }
        images.graveyard.extend(evicted);
        let released = images.collect();
        let waiters: Vec<EntityId> = if ready.is_empty() {
            Vec::new()
        } else {
            images.waiters.drain().collect()
        };
        for image in released {
            cx.drop_image(image, None);
        }
        for entity in waiters {
            cx.notify(entity);
        }
    }

    /// A load finished: if its box was laid out with a size it does not have (or it failed and
    /// becomes text), move the epoch on so views re-shape.
    fn settle(&mut self, key: &ImageKey) {
        let (Some(expected), Some(cache)) = (self.expected.remove(key), &self.cache) else {
            return;
        };
        let changed = match cache.status(key.url()) {
            ImageStatus::Ready(image) => {
                let (w, h) =
                    logical_size(image.pixel_size(), expected.variant, expected.emote_scale);
                (w - expected.size.0).abs() > SIZE_SLACK || (h - expected.size.1).abs() > SIZE_SLACK
            }
            ImageStatus::Failed(_) => true,
            // Evicted again already, or a cancelled load: laid out as assumed, nothing to do.
            ImageStatus::Loading | ImageStatus::Absent => false,
        };
        if changed {
            self.epoch += 1;
        }
    }
}

/// Starts the image store on the backend runtime, with the disk cache under the app's cache dir
/// (`<cache>/images`; none if the cache dir is unknown).
pub fn init(cx: &mut App) {
    let disk_dir = match gpui_chat_settings::SettingsPaths::from_env() {
        Ok(paths) => Some(paths.cache_dir().join("images")),
        Err(error) => {
            tracing::warn!(%error, "no cache directory; images are not cached on disk");
            None
        }
    };
    let config = ImageCacheConfig {
        disk_dir,
        ..ImageCacheConfig::default()
    };
    let runtime = cx.global::<crate::runtime::Backend>().handle();
    Images::install(ImageCache::builder(runtime).config(config), cx);
}

/// The box size of `emote` before its image is known: the provider's base size (or Twitch's
/// 28 px square) times the emote scale.
pub fn assumed_size(emote: &Emote, emote_scale: f32) -> (f32, f32) {
    let base = match (emote.width, emote.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (f32::from(w), f32::from(h)),
        _ => (DEFAULT_EMOTE_SIZE, DEFAULT_EMOTE_SIZE),
    };
    (base.0 * emote_scale, base.1 * emote_scale)
}

#[cfg(test)]
mod tests;
