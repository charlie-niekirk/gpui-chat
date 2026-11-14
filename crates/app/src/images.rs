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
//!   image turns out to have another size, or fails for good and falls back to text, its URL goes
//!   into the layout change log ([`Images::changes_since`]) and each view re-shapes only the
//!   cached messages that contain that URL. BTTV often omits sizes, so this happens for many
//!   third-party emotes; one URL must never re-shape the whole list.
//! * **Badges** use the same store ([`Images::badge_box`]): a fixed square box, so only a permanent
//!   failure (the badge is then left out of messages) goes through the change log, never a size.
//! * **Failures.** Only permanent ones ([`is_permanent`]: 404/410, undecodable, refused) turn an
//!   emote into its name as text. A transient one (timeout, network error, 5xx) keeps the
//!   placeholder, and views that painted it are woken every [`RETRY_POLL`] to ask again once
//!   media's failure back-off has passed.
//! * **One animation clock.** Every animated image picks its frame from the time since
//!   [`Images`] was created, so identical emotes animate in sync wherever they are.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpui_chat_core::{Badge, Emote, ImageScale};
use gpui_chat_media::scale::{self, logical_size};
use gpui_chat_media::{
    CacheValue, DecodedImage, FetchError, ImageCache, ImageCacheBuilder, ImageCacheConfig,
    ImageKey, ImageStatus, MediaError, Timeline,
};
use gpui_kit::{App, EntityId, Global, RenderImage, Task, Window};
use tokio::sync::Notify;

/// Base size (logical px at emote scale 1) of an emote whose provider does not report one:
/// Twitch emotes are 28 × 28 at 1x.
pub const DEFAULT_EMOTE_SIZE: f32 = 28.0;

/// Twitch's 1x badge is this many pixels square; a badge drawn at another logical size wants a
/// proportionally denser variant.
pub const BADGE_BASE_SIZE: f32 = 18.0;

/// A size change smaller than this (logical px) does not re-shape anything.
const SIZE_SLACK: f32 = 0.5;

/// How many layout changes the log remembers. A view that was away (a hidden pane) for longer
/// than that re-shapes everything instead of asking which URLs changed.
const CHANGE_LOG_CAPACITY: usize = 1024;

/// How often a view showing a transiently failed image asks for it again (media remembers the
/// failure for its back-off, 30 s by default, and answers from memory until then).
pub const RETRY_POLL: Duration = Duration::from_secs(5);

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
    /// A transient failure: keep the placeholder; the view is woken to ask again.
    Retrying,
    /// A permanent failure. `relayout`: the box was laid out as an image (it was ready when the
    /// message was shaped, then evicted and failed to reload) and the URL was just logged as
    /// changed so it becomes text; the caller should make the view render again.
    Failed {
        relayout: bool,
    },
}

/// Whether a failed load will fail again however often it is retried: the image is gone
/// (404/410), the request is refused as such, the body is over the size limit, or it does not
/// decode. Timeouts, network errors and other HTTP statuses (5xx, 429, 403...) may pass.
pub fn is_permanent(error: &MediaError) -> bool {
    match error {
        MediaError::Fetch(
            FetchError::NotFound(_) | FetchError::Request(_) | FetchError::TooLarge { .. },
        )
        | MediaError::Decode(_) => true,
        MediaError::Fetch(FetchError::Status(_) | FetchError::Timeout | FetchError::Network(_))
        | MediaError::Cancelled => false,
    }
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
        // Swap rather than take, so both maps keep their capacity from frame to frame.
        std::mem::swap(&mut self.shown, &mut self.painting);
        self.painting.clear();
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

/// Which image URLs changed what their emote's box looks like (another size, or text instead of
/// an image) since a view last looked. See [`Images::changes_since`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutChanges {
    /// Nothing changed.
    None,
    /// Only messages containing one of these URLs need to be laid out again.
    Urls(Vec<ImageKey>),
    /// The log no longer reaches back that far: lay everything out again.
    Everything,
}

/// A bounded, numbered log of URLs whose box changed.
#[derive(Default)]
struct ChangeLog {
    /// Number of the newest entry; the next one is `version + 1`.
    version: u64,
    /// `(number, url)`, oldest first, numbers consecutive.
    entries: VecDeque<(u64, ImageKey)>,
}

impl ChangeLog {
    fn push(&mut self, url: &ImageKey) {
        self.version += 1;
        self.entries.push_back((self.version, url.clone()));
        if self.entries.len() > CHANGE_LOG_CAPACITY {
            self.entries.pop_front();
        }
    }

    fn since(&self, seen: u64) -> LayoutChanges {
        if seen >= self.version {
            return LayoutChanges::None;
        }
        // Entries `seen + 1 ..= version` are needed; the oldest kept must not be newer than that.
        if self
            .entries
            .front()
            .is_none_or(|(first, _)| *first > seen + 1)
        {
            return LayoutChanges::Everything;
        }
        // Each URL once, in log order.
        let mut seen_urls: HashSet<&str> = HashSet::new();
        let urls: Vec<ImageKey> = self
            .entries
            .iter()
            .filter(|(n, _)| *n > seen)
            .filter(|(_, url)| seen_urls.insert(url.url()))
            .map(|(_, url)| url.clone())
            .collect();
        LayoutChanges::Urls(urls)
    }
}

/// What a load in flight was assumed to look like when its message was shaped.
struct Expected {
    size: (f32, f32),
    variant: ImageScale,
    emote_scale: f32,
    /// The box does not depend on the image's own size (a badge is always square), so only a
    /// failure for good changes the layout, never a size.
    fixed: bool,
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
    changes: ChangeLog,
    /// Evicted images still held by a view (on screen); their textures go once they are not.
    graveyard: Vec<Arc<EmoteImage>>,
    /// Permanently failed URLs already logged as changed, until a re-shape makes them text.
    to_text: HashSet<ImageKey>,
    /// Views that painted a transiently failed image, woken by `retry`.
    retry_waiters: HashSet<EntityId>,
    retry: Option<Task<()>>,
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
            changes: ChangeLog::default(),
            graveyard: Vec::new(),
            to_text: HashSet::new(),
            retry_waiters: HashSet::new(),
            retry: None,
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

    /// Number of the newest entry in the layout change log; a view remembers it and later asks
    /// [`Self::changes_since`].
    pub fn layout_version(&self) -> u64 {
        self.changes.version
    }

    /// The URLs whose emote box changed after a message was laid out with it (a different size
    /// than assumed, or a failure that falls back to text) since the log was at `seen`. A view
    /// re-shapes only the messages containing them.
    pub fn changes_since(&self, seen: u64) -> LayoutChanges {
        self.changes.since(seen)
    }

    /// Logs a box change by hand (tests of the views that consume the log).
    #[cfg(test)]
    pub fn record_layout_change(&mut self, url: &ImageKey) {
        self.changes.push(url);
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
            ImageStatus::Failed(error) if is_permanent(&error) => {
                // Shaped as text now; a later image box for it is a new change.
                self.to_text.remove(url.url());
                EmoteBox::Text
            }
            // Not ready, or failed for now (media retries once its back-off has passed): a
            // placeholder of the assumed size.
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
                        fixed: false,
                    },
                );
                EmoteBox::Image { url, size: assumed }
            }
        }
    }

    /// The image to draw `badge` from in a square box of `size` logical pixels, starting its
    /// load; `None` if it has no image or it failed for good (the badge is then left out of the
    /// message). Called when a message is shaped. The box never depends on the image, so a
    /// finished load changes nothing; a permanent failure after the message was shaped is logged
    /// like an emote's (the view shapes its messages again, now without the badge).
    pub fn badge_box(&mut self, badge: &Badge, size: f32, display_scale: f32) -> Option<ImageKey> {
        let selected = scale::select(&badge.urls, display_scale, size / BADGE_BASE_SIZE)?;
        let url = ImageKey::new(selected.url);
        let Some(cache) = &self.cache else {
            return Some(url);
        };
        match cache.status(url.url()) {
            ImageStatus::Failed(error) if is_permanent(&error) => {
                self.to_text.remove(url.url());
                None
            }
            ImageStatus::Ready(_) => Some(url),
            status => {
                if matches!(status, ImageStatus::Absent) {
                    drop(cache.request(url.url()));
                }
                self.expected.insert(
                    url.clone(),
                    Expected {
                        size: (size, size),
                        variant: selected.scale,
                        emote_scale: 1.0,
                        fixed: true,
                    },
                );
                Some(url)
            }
        }
    }

    /// The URL and logical size of a larger preview of `badge` for the tooltip: its densest
    /// variant, one image pixel per device pixel. `None` if it has no image.
    pub fn badge_preview(
        &mut self,
        badge: &Badge,
        display_scale: f32,
    ) -> Option<(ImageKey, (f32, f32))> {
        let selected = scale::select(&badge.urls, 1.0, f32::from(ImageScale::X4.factor()))?;
        let url = ImageKey::new(selected.url);
        if let Some(cache) = &self.cache
            && let ImageStatus::Failed(error) = cache.status(url.url())
            && is_permanent(&error)
        {
            return None;
        }
        // 18 px at 1x, so the variant's pixels are `18 × factor`: one per device pixel.
        let side = BADGE_BASE_SIZE * f32::from(selected.scale.factor()) / display_scale.max(1.0);
        Some((url, (side, side)))
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
            Some(ImageStatus::Failed(error)) if is_permanent(&error) => return None,
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
    /// is started or joined and `waiter` is notified when it finishes (or, after a transient
    /// failure, woken to retry: call [`Self::arm_retry`] after painting). Marks it painted.
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
                    Some(Err(error)) if is_permanent(&error) => {
                        let relayout = self.to_text.insert(url.clone());
                        if relayout {
                            self.changes.push(url);
                        }
                        return Lookup::Failed { relayout };
                    }
                    Some(Err(_)) => {
                        self.retry_waiters.insert(waiter);
                        return Lookup::Retrying;
                    }
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

    /// Wakes the views that painted a transiently failed image after [`RETRY_POLL`], so they ask
    /// for it again. Call after painting; one timer serves every view.
    pub fn arm_retry(cx: &mut App) {
        match cx.try_global::<Self>() {
            Some(images) if !images.retry_waiters.is_empty() && images.retry.is_none() => {}
            _ => return,
        }
        let timer = cx.background_executor().timer(RETRY_POLL);
        let task = cx.spawn(async move |cx| {
            timer.await;
            cx.update(|cx| {
                let images = cx.global_mut::<Self>();
                images.retry = None;
                let waiters: Vec<EntityId> = images.retry_waiters.drain().collect();
                for entity in waiters {
                    cx.notify(entity);
                }
            });
        });
        cx.global_mut::<Self>().retry = Some(task);
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

    /// A load finished: if its box was laid out with a size it does not have (or it failed for
    /// good and becomes text), log the URL so views re-shape the messages that contain it. After
    /// a transient failure the box stays a placeholder and is checked again when a retry
    /// finishes.
    fn settle(&mut self, key: &ImageKey) {
        let (Some(expected), Some(cache)) = (self.expected.remove(key), &self.cache) else {
            return;
        };
        let changed = match cache.status(key.url()) {
            ImageStatus::Ready(_) if expected.fixed => false,
            ImageStatus::Ready(image) => {
                let (w, h) =
                    logical_size(image.pixel_size(), expected.variant, expected.emote_scale);
                (w - expected.size.0).abs() > SIZE_SLACK || (h - expected.size.1).abs() > SIZE_SLACK
            }
            ImageStatus::Failed(error) if is_permanent(&error) => {
                self.to_text.insert(key.clone());
                true
            }
            ImageStatus::Failed(_) => {
                self.expected.insert(key.clone(), expected);
                false
            }
            // Evicted again already, or a cancelled load: laid out as assumed, nothing to do.
            ImageStatus::Loading | ImageStatus::Absent => false,
        };
        if changed {
            self.changes.push(key);
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
