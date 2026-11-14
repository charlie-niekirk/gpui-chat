use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gpui_chat_core::ImageUrls;
use gpui_chat_media::{DecodeError, DecodeLimits, FetchFuture, Fetcher, RetryPolicy, decode};
use gpui_kit::{
    AppContext as _, Bounds, Context, Corners, IntoElement, ParentElement, Pixels, Render, Styled,
    TestAppContext, WindowHandle, canvas, div,
};

use super::*;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../fixtures/images/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn frames_follow_the_global_clock() {
    // The GIF fixture: delays 100, 50, 0 (clamped to 100) and 30 ms; loop 280 ms.
    let image = EmoteImage::from_decoded(
        decode(&fixture("animated.gif"), &DecodeLimits::default()).unwrap(),
    );
    assert!(image.is_animated() && image.is_drawable());
    assert_eq!(image.pixel_size(), (28, 28));
    assert_eq!(image.render_image().frame_count(), 4);
    assert_eq!(image.frame(Some(ms(0))), (0, Some(ms(100))));
    assert_eq!(image.frame(Some(ms(120))), (1, Some(ms(30))));
    assert_eq!(image.frame(Some(ms(200))), (2, Some(ms(50))));
    assert_eq!(image.frame(Some(ms(260))), (3, Some(ms(20))));
    // Looping: 290 ms is 10 ms into the second loop.
    assert_eq!(image.frame(Some(ms(290))), (0, Some(ms(90))));
    // Animation off: the first frame, and nothing to wake for.
    assert_eq!(image.frame(None), (0, None));
    // Two copies of the same emote asked at the same clock agree (they animate in sync).
    let twin = EmoteImage::from_decoded(
        decode(&fixture("animated.gif"), &DecodeLimits::default()).unwrap(),
    );
    for t in (0..600).step_by(7) {
        assert_eq!(image.frame(Some(ms(t))), twin.frame(Some(ms(t))));
    }

    let still =
        EmoteImage::from_decoded(decode(&fixture("static.png"), &DecodeLimits::default()).unwrap());
    assert!(!still.is_animated());
    assert_eq!(still.frame(Some(ms(1234))), (0, None));
    // BGRA, straight alpha: the fixture's pixel (0,0) is RGBA(200, 100, 50, 128).
    assert_eq!(
        &still.render_image().as_bytes(0).unwrap()[..4],
        [50, 100, 200, 128]
    );
}

#[test]
fn animation_wakes_only_when_animating_visible_and_active() {
    assert_eq!(animation_wake(Some(ms(30)), true, true), Some(ms(30)));
    assert_eq!(
        animation_wake(Some(ms(30)), false, true),
        None,
        "animation off"
    );
    assert_eq!(
        animation_wake(Some(ms(30)), true, false),
        None,
        "window inactive"
    );
    assert_eq!(
        animation_wake(None, true, true),
        None,
        "nothing animated painted"
    );
    assert_eq!(
        animation_wake(Some(Duration::from_nanos(5)), true, true),
        Some(MIN_ANIMATION_WAKE)
    );
}

#[test]
fn placeholder_sizes_and_themed_urls() {
    let twitch = Emote::twitch("25", "Kappa");
    assert_eq!(assumed_size(&twitch, 1.0), (28.0, 28.0));
    assert_eq!(assumed_size(&twitch, 1.5), (42.0, 42.0));
    let wide = Emote {
        width: Some(64),
        height: Some(32),
        ..Emote::twitch("1", "Wide")
    };
    assert_eq!(assumed_size(&wide, 1.0), (64.0, 32.0));

    let url = twitch.urls.get(ImageScale::X2).unwrap();
    assert_eq!(themed_url(url, false), url);
    assert_eq!(
        themed_url(url, true),
        "https://static-cdn.jtvnw.net/emoticons/v2/25/default/light/2.0"
    );
    assert_eq!(
        themed_url("https://cdn.7tv.app/dark/1x.webp", true),
        "https://cdn.7tv.app/dark/1x.webp"
    );
}

type Routes = Arc<Mutex<HashMap<&'static str, Result<Vec<u8>, FetchError>>>>;

/// Serves fixtures (or errors) by URL, 404 for anything else, and counts requests.
struct FakeFetcher {
    routes: Routes,
    requests: Arc<AtomicUsize>,
}

impl Fetcher for FakeFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let result = self
            .routes
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .unwrap_or(Err(FetchError::NotFound(404)));
        Box::pin(async move { result })
    }
}

/// How long the test cache remembers a transient failure.
const BACKOFF: Duration = Duration::from_millis(50);

struct Harness {
    _runtime: tokio::runtime::Runtime,
    requests: Arc<AtomicUsize>,
    routes: Routes,
}

impl Harness {
    /// Installs an image store serving `routes`, with a memory budget of `budget` bytes.
    fn new(routes: &[(&'static str, &str)], budget: usize, cx: &mut TestAppContext) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let routes: Routes = Arc::new(Mutex::new(
            routes
                .iter()
                .map(|(url, file)| (*url, Ok(fixture(file))))
                .collect(),
        ));
        let fetcher = FakeFetcher {
            routes: routes.clone(),
            requests: requests.clone(),
        };
        let builder = ImageCache::builder(runtime.handle().clone())
            .config(ImageCacheConfig {
                memory_budget_bytes: budget,
                // One attempt, and transient failures forgotten quickly, to test retries.
                retry: RetryPolicy {
                    max_attempts: 1,
                    ..RetryPolicy::default()
                },
                failure_backoff: BACKOFF,
                ..ImageCacheConfig::default()
            })
            .fetcher(Arc::new(fetcher));
        cx.update(|cx| Images::install_with(builder, false, cx));
        Self {
            _runtime: runtime,
            requests,
            routes,
        }
    }

    /// What `url` serves from now on.
    fn route(&self, url: &'static str, result: Result<Vec<u8>, FetchError>) {
        self.routes.lock().unwrap().insert(url, result);
    }

    fn cache(&self, cx: &mut TestAppContext) -> ImageCache<EmoteImage> {
        cx.read(|cx| cx.global::<Images>().cache().unwrap().clone())
    }

    /// Waits for `url`'s load (started by the view or `emote_box`), then lets the UI apply it
    /// (the pump runs on the UI thread). Never starts a load itself.
    fn load(&self, url: &str, cx: &mut TestAppContext) {
        let cache = self.cache(cx);
        for _ in 0..500 {
            if !matches!(cache.status(url), ImageStatus::Loading) {
                break;
            }
            std::thread::sleep(ms(2));
        }
        // `on_ready` fires just after the status changes: wait for it to land (it may already
        // have been pumped by an earlier call, hence the cap).
        let shared = cx.read(|cx| cx.global::<Images>().shared.clone());
        for _ in 0..100 {
            let pending = shared
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if pending.ready.iter().any(|key| key.url() == url) {
                break;
            }
            drop(pending);
            std::thread::sleep(ms(2));
        }
        self.pump(cx);
    }

    /// Applies what media reported (the app's pump task does this on its own).
    fn pump(&self, cx: &mut TestAppContext) {
        cx.update(Images::pump);
        cx.run_until_parked();
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// Paints the images at `urls` (while `show` is set) through a [`FrameImages`], like the chat view.
struct ImageView {
    urls: Vec<ImageKey>,
    show: Rc<Cell<bool>>,
    frame: Rc<RefCell<FrameImages>>,
    /// Images drawn in the last paint.
    drawn: Rc<RefCell<Vec<Arc<EmoteImage>>>>,
}

impl Render for ImageView {
    fn render(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.frame.borrow_mut().begin_frame();
        Images::sweep(window, cx);
        let (urls, frame, drawn) = (self.urls.clone(), self.frame.clone(), self.drawn.clone());
        let entity = cx.entity_id();
        let show = self.show.get();
        div().size_full().child(
            canvas(
                |_, _, _| {},
                move |bounds: Bounds<Pixels>, (), window, cx| {
                    drawn.borrow_mut().clear();
                    if !show {
                        return;
                    }
                    for url in &urls {
                        let lookup =
                            cx.global_mut::<Images>()
                                .lookup(url, &mut frame.borrow_mut(), entity);
                        if let Lookup::Ready(image) = lookup {
                            window
                                .paint_image(
                                    bounds,
                                    bounds,
                                    Corners::default(),
                                    image.render_image().clone(),
                                    0,
                                    false,
                                )
                                .unwrap();
                            drawn.borrow_mut().push(image);
                        }
                    }
                    Images::arm_retry(cx);
                },
            )
            .size_full(),
        )
    }
}

struct Window1 {
    handle: WindowHandle<ImageView>,
    show: Rc<Cell<bool>>,
    drawn: Rc<RefCell<Vec<Arc<EmoteImage>>>>,
}

impl Window1 {
    fn open(urls: &[&str], cx: &mut TestAppContext) -> Self {
        let show = Rc::new(Cell::new(true));
        let drawn = Rc::<RefCell<Vec<Arc<EmoteImage>>>>::default();
        let (s, d) = (show.clone(), drawn.clone());
        let urls: Vec<ImageKey> = urls.iter().map(|u| ImageKey::new(*u)).collect();
        let handle = cx.add_window(move |_, _| ImageView {
            urls,
            show: s,
            frame: Rc::default(),
            drawn: d,
        });
        cx.run_until_parked();
        Self {
            handle,
            show,
            drawn,
        }
    }

    fn redraw(&self, cx: &mut TestAppContext) {
        self.handle.update(cx, |_, _, cx| cx.notify()).unwrap();
        cx.run_until_parked();
    }

    fn in_atlas(&self, image: &RenderImage, cx: &mut TestAppContext) -> bool {
        self.handle
            .update(cx, |_, window, _| window.has_image_atlas_entry(image))
            .unwrap()
    }

    fn drawn(&self) -> usize {
        self.drawn.borrow().len()
    }
}

#[gpui_kit::test]
fn a_waiting_view_is_woken_when_its_image_arrives(cx: &mut TestAppContext) {
    let h = Harness::new(&[("a", "static.png")], 1 << 30, cx);
    let w = Window1::open(&["a"], cx);
    assert_eq!(w.drawn(), 0, "still loading: a placeholder");
    h.load("a", cx);
    // Nobody asked for a redraw except the image store's wake-up.
    assert_eq!(w.drawn(), 1);
    assert_eq!(h.requests(), 1);
}

#[gpui_kit::test]
fn textures_are_released_on_eviction_once_nothing_shows_them(cx: &mut TestAppContext) {
    let h = Harness::new(&[("a", "static.png")], 1 << 30, cx);
    let w = Window1::open(&["a"], cx);
    h.load("a", cx);
    let image = w.drawn.borrow()[0].clone();
    let render = image.render_image().clone();
    drop(image);
    assert!(w.in_atlas(&render, cx), "painted: uploaded");

    // Evicted while on screen: still drawn from the view's pin, texture kept, nothing refetched.
    h.cache(cx).clear_memory();
    h.pump(cx);
    w.redraw(cx);
    assert_eq!(w.drawn(), 1);
    assert!(w.in_atlas(&render, cx));
    assert_eq!(h.requests(), 1);

    // Off screen: the pin goes with the next frame, and the texture with it.
    w.show.set(false);
    w.redraw(cx);
    w.redraw(cx);
    assert!(!w.in_atlas(&render, cx), "texture released");
}

#[gpui_kit::test]
fn an_evicted_image_nothing_shows_is_released_by_the_pump(cx: &mut TestAppContext) {
    let h = Harness::new(&[("a", "static.png")], 1 << 30, cx);
    let w = Window1::open(&["a"], cx);
    h.load("a", cx);
    let render = w.drawn.borrow()[0].render_image().clone();
    w.show.set(false);
    w.redraw(cx);
    w.redraw(cx);
    // Unpinned but still cached: the texture stays with the cache entry.
    assert!(w.in_atlas(&render, cx));
    h.cache(cx).clear_memory();
    h.pump(cx);
    assert!(!w.in_atlas(&render, cx));
}

#[gpui_kit::test]
fn visible_images_over_budget_do_not_thrash(cx: &mut TestAppContext) {
    // A one-byte budget: each insert evicts the previous image.
    let h = Harness::new(&[("a", "static.png"), ("b", "animated.gif")], 1, cx);
    let w = Window1::open(&["a", "b"], cx);
    h.load("a", cx);
    h.load("b", cx);
    // Both loaded at once, so `b`'s insert may have evicted `a` before it was ever drawn (not
    // pinned yet): that costs at most one refetch, then both are pinned.
    for _ in 0..3 {
        w.redraw(cx);
        h.load("a", cx);
        h.load("b", cx);
    }
    let settled = h.requests();
    assert!(settled <= 3, "{settled} requests");
    for _ in 0..5 {
        w.redraw(cx);
        assert_eq!(w.drawn(), 2, "both stay on screen");
    }
    assert_eq!(h.requests(), settled, "no refetch loop");
}

fn emote_at(url: &'static str) -> Emote {
    Emote {
        urls: ImageUrls::new().with(ImageScale::X1, url),
        ..Emote::twitch("1", "E")
    }
}

#[gpui_kit::test]
fn boxes_keep_their_size_and_only_wrong_ones_are_logged_by_url(cx: &mut TestAppContext) {
    let h = Harness::new(
        &[("ok", "static.png"), ("big", "animated-112.gif")],
        1 << 30,
        cx,
    );
    let boxed = |url: &'static str, cx: &mut TestAppContext| {
        cx.update(|cx| {
            cx.global_mut::<Images>()
                .emote_box(&emote_at(url), 1.0, 1.0, false)
        })
    };
    let version = |cx: &mut TestAppContext| cx.read(|cx| cx.global::<Images>().layout_version());
    let changes = |since: u64, cx: &mut TestAppContext| {
        cx.read(|cx| cx.global::<Images>().changes_since(since))
    };
    let urls =
        |names: &[&str]| LayoutChanges::Urls(names.iter().map(|n| ImageKey::new(*n)).collect());

    // Unknown size before loading: Twitch's 28 px square.
    let placeholder = EmoteBox::Image {
        url: ImageKey::new("ok"),
        size: (28.0, 28.0),
    };
    assert_eq!(boxed("ok", cx), placeholder);
    h.load("ok", cx);
    assert_eq!(version(cx), 0, "28 px as assumed: nothing to re-shape");
    assert_eq!(changes(0, cx), LayoutChanges::None);
    assert_eq!(boxed("ok", cx), placeholder);

    // A 112 px image laid out as 28 px: re-shape, and from now on it is 112 px.
    boxed("big", cx);
    h.load("big", cx);
    assert_eq!(version(cx), 1);
    // Only that URL: "ok" (right as assumed) is not named, so its messages are left alone.
    assert_eq!(changes(0, cx), urls(&["big"]));
    assert_eq!(
        boxed("big", cx),
        EmoteBox::Image {
            url: ImageKey::new("big"),
            size: (112.0, 112.0)
        }
    );

    // A 404 falls back to text.
    boxed("missing", cx);
    h.load("missing", cx);
    assert_eq!(version(cx), 2);
    assert_eq!(changes(0, cx), urls(&["big", "missing"]));
    // A view that already saw "big" is told about "missing" only.
    assert_eq!(changes(1, cx), urls(&["missing"]));
    assert_eq!(changes(2, cx), LayoutChanges::None);
    assert_eq!(boxed("missing", cx), EmoteBox::Text);
    // An emote without URLs is text from the start.
    let none = Emote {
        urls: ImageUrls::new(),
        ..Emote::twitch("2", "None")
    };
    assert_eq!(
        cx.update(|cx| cx.global_mut::<Images>().emote_box(&none, 1.0, 1.0, false)),
        EmoteBox::Text
    );
}

#[gpui_kit::test]
fn animation_wake_is_scheduled_and_cancelled(cx: &mut TestAppContext) {
    let frame = Rc::new(RefCell::new(FrameImages::default()));
    let entity = cx.update(|cx| cx.new(|_| ()).entity_id());
    cx.update(|cx| {
        let mut f = frame.borrow_mut();
        f.animate_in(ms(40));
        f.animate_in(ms(30));
        f.schedule_wake(entity, true, true, cx);
        assert!(f.wake_pending());
        // Next frame: nothing animated was painted, so the wake is cancelled.
        f.begin_frame();
        f.schedule_wake(entity, true, true, cx);
        assert!(!f.wake_pending());
        // Animated, but the window is inactive (or animation is off): no wake.
        f.animate_in(ms(30));
        f.schedule_wake(entity, true, false, cx);
        assert!(!f.wake_pending());
        f.animate_in(ms(30));
        f.schedule_wake(entity, false, true, cx);
        assert!(!f.wake_pending());
    });
}

#[test]
fn only_permanent_failures_become_text() {
    use gpui_chat_media::MediaError as E;
    for permanent in [
        E::Fetch(FetchError::NotFound(404)),
        E::Fetch(FetchError::NotFound(410)),
        E::Fetch(FetchError::Request("bad url".into())),
        E::Fetch(FetchError::TooLarge { limit: 1 }),
        E::Decode(DecodeError::Malformed("truncated".into())),
        E::Decode(DecodeError::UnknownFormat),
    ] {
        assert!(is_permanent(&permanent), "{permanent:?}");
    }
    for transient in [
        E::Fetch(FetchError::Timeout),
        E::Fetch(FetchError::Network("reset".into())),
        E::Fetch(FetchError::Status(503)),
        E::Fetch(FetchError::Status(429)),
        E::Fetch(FetchError::Status(403)),
        E::Cancelled,
    ] {
        assert!(!is_permanent(&transient), "{transient:?}");
    }
}

#[gpui_kit::test]
fn a_transient_failure_keeps_the_placeholder_and_is_retried(cx: &mut TestAppContext) {
    let h = Harness::new(&[], 1 << 30, cx);
    h.route("flaky", Err(FetchError::Status(503)));
    let placeholder = EmoteBox::Image {
        url: ImageKey::new("flaky"),
        size: (28.0, 28.0),
    };
    let boxed = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            cx.global_mut::<Images>()
                .emote_box(&emote_at("flaky"), 1.0, 1.0, false)
        })
    };
    assert_eq!(boxed(cx), placeholder);
    h.load("flaky", cx);
    let epoch = |cx: &mut TestAppContext| cx.read(|cx| cx.global::<Images>().layout_version());
    assert_eq!(epoch(cx), 0, "a 503 does not re-shape");
    assert_eq!(boxed(cx), placeholder, "still a placeholder, not text");

    // The view paints a placeholder and is woken after RETRY_POLL; by then the CDN recovered
    // and media's back-off has passed, so the retry loads the image.
    let w = Window1::open(&["flaky"], cx);
    assert_eq!(w.drawn(), 0);
    h.route("flaky", Ok(fixture("static.png")));
    std::thread::sleep(BACKOFF * 2);
    cx.executor().advance_clock(RETRY_POLL);
    cx.run_until_parked();
    h.load("flaky", cx);
    assert_eq!(w.drawn(), 1, "retried and drawn");
    assert_eq!(epoch(cx), 0, "28 px as assumed");
}

fn badge_at(url: &'static str) -> gpui_chat_core::Badge {
    gpui_chat_core::Badge {
        id: gpui_chat_core::BadgeId::new("subscriber", "0"),
        title: "Subscriber".into(),
        urls: ImageUrls::new().with(ImageScale::X1, url),
    }
}

#[gpui_kit::test]
fn badges_have_a_fixed_box_and_only_a_permanent_failure_changes_the_layout(
    cx: &mut TestAppContext,
) {
    // `big` is 112 px, far from the 18 px badge box.
    let h = Harness::new(&[("big", "animated-112.gif")], 1 << 30, cx);
    let boxed = |url: &'static str, cx: &mut TestAppContext| {
        cx.update(|cx| {
            cx.global_mut::<Images>()
                .badge_box(&badge_at(url), 18.0, 1.0)
        })
    };
    let version = |cx: &mut TestAppContext| cx.read(|cx| cx.global::<Images>().layout_version());

    assert_eq!(boxed("big", cx), Some(ImageKey::new("big")));
    h.load("big", cx);
    // The image's own size does not matter: nothing to lay out again, unlike an emote.
    assert_eq!(version(cx), 0);
    assert_eq!(boxed("big", cx), Some(ImageKey::new("big")));

    // A 404 drops the badge: the URL is logged once, and shaping again leaves the badge out.
    assert_eq!(boxed("missing", cx), Some(ImageKey::new("missing")));
    h.load("missing", cx);
    assert_eq!(version(cx), 1);
    assert_eq!(
        cx.read(|cx| cx.global::<Images>().changes_since(0)),
        LayoutChanges::Urls(vec![ImageKey::new("missing")])
    );
    assert_eq!(boxed("missing", cx), None);

    // No URLs at all: no badge.
    let none = gpui_chat_core::Badge {
        urls: ImageUrls::new(),
        ..badge_at("x")
    };
    assert_eq!(
        cx.update(|cx| cx.global_mut::<Images>().badge_box(&none, 18.0, 1.0)),
        None
    );
}

#[gpui_kit::test]
fn the_badge_variant_follows_the_box_and_display_scale_and_the_preview_is_the_densest(
    cx: &mut TestAppContext,
) {
    let _h = Harness::new(&[], 1 << 30, cx);
    let badge = gpui_chat_core::Badge {
        urls: ImageUrls::new()
            .with(ImageScale::X1, "b/1")
            .with(ImageScale::X2, "b/2")
            .with(ImageScale::X4, "b/3"),
        ..badge_at("x")
    };
    // The tooltip preview: the 72 px variant, one image pixel per device pixel.
    let preview = |scale: f32, cx: &mut TestAppContext| {
        cx.update(|cx| cx.global_mut::<Images>().badge_preview(&badge, scale))
    };
    assert_eq!(preview(2.0, cx), Some((ImageKey::new("b/3"), (36.0, 36.0))));
    assert_eq!(preview(1.0, cx), Some((ImageKey::new("b/3"), (72.0, 72.0))));
    let url = |size: f32, scale: f32, cx: &mut TestAppContext| {
        cx.update(|cx| {
            cx.global_mut::<Images>()
                .badge_box(&badge, size, scale)
                .map(|key| key.url().to_owned())
        })
    };
    assert_eq!(url(18.0, 1.0, cx).as_deref(), Some("b/1"));
    assert_eq!(url(18.0, 2.0, cx).as_deref(), Some("b/2"));
    // A bigger box (bigger font) wants a denser image.
    assert_eq!(url(36.0, 2.0, cx).as_deref(), Some("b/3"));
}

#[gpui_kit::test]
fn an_evicted_image_that_fails_to_reload_becomes_text(cx: &mut TestAppContext) {
    let h = Harness::new(&[("gone", "static.png")], 1 << 30, cx);
    let boxed = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            cx.global_mut::<Images>()
                .emote_box(&emote_at("gone"), 1.0, 1.0, false)
        })
    };
    boxed(cx);
    h.load("gone", cx);
    // Shaped while ready: an image box from the image itself (no load expected any more).
    assert!(matches!(boxed(cx), EmoteBox::Image { .. }));
    let epoch = cx.read(|cx| cx.global::<Images>().layout_version());

    // Evicted while off screen, and the CDN now answers 404.
    h.cache(cx).clear_memory();
    h.pump(cx);
    h.route("gone", Err(FetchError::NotFound(404)));
    let w = Window1::open(&["gone"], cx);
    h.load("gone", cx);
    w.redraw(cx);
    assert_eq!(w.drawn(), 0);
    let after = cx.read(|cx| cx.global::<Images>().layout_version());
    assert_eq!(after, epoch + 1, "the box is re-laid out once");
    w.redraw(cx);
    assert_eq!(
        cx.read(|cx| cx.global::<Images>().layout_version()),
        after,
        "not on every frame"
    );
    assert_eq!(boxed(cx), EmoteBox::Text);
}

#[test]
fn the_change_log_names_urls_once_and_gives_up_when_a_view_is_too_far_behind() {
    let mut log = ChangeLog::default();
    assert_eq!(log.since(0), LayoutChanges::None);
    for url in ["a", "b", "a", "c"] {
        log.push(&ImageKey::new(url));
    }
    let keys =
        |names: &[&str]| LayoutChanges::Urls(names.iter().map(|n| ImageKey::new(*n)).collect());
    // Repeats are listed once.
    assert_eq!(log.since(0), keys(&["a", "b", "c"]));
    assert_eq!(log.since(2), keys(&["a", "c"]));
    assert_eq!(log.since(4), LayoutChanges::None);
    // A view cannot be ahead of the log (a fresh app with an older view is a no-op, not a panic).
    assert_eq!(log.since(99), LayoutChanges::None);

    // Past the capacity the oldest entries go; whoever still needed them re-shapes everything.
    for n in 0..CHANGE_LOG_CAPACITY {
        log.push(&ImageKey::new(format!("u{n}")));
    }
    assert_eq!(log.entries.len(), CHANGE_LOG_CAPACITY);
    assert_eq!(log.since(0), LayoutChanges::Everything);
    assert_eq!(log.since(3), LayoutChanges::Everything, "entry 4 is gone");
    // A view that saw all but the newest two entries is still served exactly.
    let caught_up = log.version - 2;
    assert_eq!(
        log.since(caught_up),
        keys(&[
            &format!("u{}", CHANGE_LOG_CAPACITY - 2),
            &format!("u{}", CHANGE_LOG_CAPACITY - 1)
        ])
    );
}
