use std::sync::Mutex as StdMutex;

use super::*;
use crate::DecodeError;
use crate::fetch::FetchFuture;
use crate::test_support::{fixture, png_bytes};

/// What the fake server does for a URL.
#[derive(Clone)]
enum Route {
    Ok(Vec<u8>),
    Err(FetchError),
    /// Fails with the error `n` times, then serves the bytes.
    FailThen(usize, FetchError, Vec<u8>),
    /// Never answers.
    Hang,
}

#[derive(Default)]
struct FakeFetcher {
    routes: StdMutex<HashMap<String, Route>>,
    calls: StdMutex<HashMap<String, usize>>,
    delay: Duration,
}

impl FakeFetcher {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            delay,
            ..Self::default()
        })
    }

    fn route(self: &Arc<Self>, url: &str, route: Route) -> Arc<Self> {
        self.routes.lock().unwrap().insert(url.to_owned(), route);
        self.clone()
    }

    fn calls(&self, url: &str) -> usize {
        self.calls.lock().unwrap().get(url).copied().unwrap_or(0)
    }

    fn total_calls(&self) -> usize {
        self.calls.lock().unwrap().values().sum()
    }
}

impl Fetcher for FakeFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
        Box::pin(async move {
            let n = {
                let mut calls = self.calls.lock().unwrap();
                let n = calls.entry(url.to_owned()).or_default();
                *n += 1;
                *n
            };
            tokio::time::sleep(self.delay).await;
            let route = self.routes.lock().unwrap().get(url).cloned();
            match route {
                Some(Route::Ok(bytes)) => Ok(bytes),
                Some(Route::Err(e)) => Err(e),
                Some(Route::FailThen(fails, e, bytes)) => {
                    if n <= fails {
                        Err(e)
                    } else {
                        Ok(bytes)
                    }
                }
                Some(Route::Hang) => std::future::pending().await,
                None => Err(FetchError::NotFound(404)),
            }
        })
    }
}

fn test_config() -> ImageCacheConfig {
    ImageCacheConfig {
        request_timeout: Duration::from_millis(300),
        retry: RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        },
        ..ImageCacheConfig::default()
    }
}

fn cache_with(fetcher: Arc<FakeFetcher>, config: ImageCacheConfig) -> ImageCache {
    ImageCache::builder(Handle::current())
        .config(config)
        .fetcher(fetcher)
        .build()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_share_one_fetch() {
    const URL: &str = "https://cdn/anim.webp";
    let fetcher =
        FakeFetcher::new(Duration::from_millis(50)).route(URL, Route::Ok(fixture("animated.webp")));
    let cache = cache_with(fetcher.clone(), test_config());

    let tasks: Vec<_> = (0..64)
        .map(|_| {
            let cache = cache.clone();
            tokio::spawn(async move { cache.load(URL).await })
        })
        .collect();
    let mut images = Vec::new();
    for t in tasks {
        images.push(t.await.unwrap().unwrap());
    }

    assert_eq!(fetcher.calls(URL), 1, "one network fetch for 64 requests");
    assert!(images.iter().all(|i| Arc::ptr_eq(i, &images[0])));
    assert_eq!(images[0].frames().len(), 4);
    let stats = cache.stats();
    assert_eq!(stats.network_fetches, 1);
    assert_eq!(stats.in_flight, 0);
    // Every request either started the load, joined it, or (if it came late) hit memory.
    assert_eq!(stats.dedup_joins + stats.memory_hits + 1, 64);
    assert_eq!(stats.memory_entries, 1);

    // Afterwards it is a memory hit with no further fetch.
    assert!(Arc::ptr_eq(&cache.get_if_ready(URL).unwrap(), &images[0]));
    cache.load(URL).await.unwrap();
    assert_eq!(fetcher.calls(URL), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_if_ready_and_status_follow_the_load() {
    const URL: &str = "https://cdn/a.png";
    let fetcher =
        FakeFetcher::new(Duration::from_millis(30)).route(URL, Route::Ok(png_bytes(8, 8)));
    let ready = Arc::new(StdMutex::new(Vec::new()));
    let seen = ready.clone();
    let cache = ImageCache::<DecodedImage>::builder(Handle::current())
        .config(test_config())
        .fetcher(fetcher)
        .on_ready(move |k| seen.lock().unwrap().push(k.url().to_owned()))
        .build()
        .unwrap();

    assert!(cache.get_if_ready(URL).is_none());
    assert!(matches!(cache.status(URL), ImageStatus::Absent));
    assert!(cache.get_or_request(URL).is_none());
    assert!(matches!(cache.status(URL), ImageStatus::Loading));
    let handle = cache.request(URL);
    assert!(handle.try_get().is_none());
    let img = handle.await.unwrap();
    assert_eq!((img.width(), img.height()), (8, 8));
    assert!(matches!(cache.status(URL), ImageStatus::Ready(_)));
    assert!(cache.get_if_ready(URL).is_some());
    assert!(cache.get_or_request(URL).is_some());
    assert_eq!(*ready.lock().unwrap(), vec![URL.to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disk_cache_hit_after_restart() {
    const URL: &str = "https://cdn/anim.gif";
    let dir = tempfile::tempdir().unwrap();
    let config = ImageCacheConfig {
        disk_dir: Some(dir.path().to_owned()),
        ..test_config()
    };

    let online = FakeFetcher::new(Duration::ZERO).route(URL, Route::Ok(fixture("animated.gif")));
    let first = cache_with(online.clone(), config.clone());
    let a = first.load(URL).await.unwrap();
    assert_eq!(online.calls(URL), 1);
    assert_eq!(first.stats().disk_hits, 0);
    assert!(first.stats().disk_bytes.unwrap() > 0);
    drop(first);

    // "Restart" with the network down: served from disk, decoded again.
    let offline = FakeFetcher::new(Duration::ZERO).route(URL, Route::Err(FetchError::Timeout));
    let second = cache_with(offline.clone(), config);
    let b = second.load(URL).await.unwrap();
    assert_eq!(offline.total_calls(), 0);
    assert_eq!(second.stats().disk_hits, 1);
    assert_eq!(second.stats().network_requests, 0);
    assert_eq!(*a, *b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_disk_entry_is_refetched() {
    const URL: &str = "https://cdn/x.png";
    let dir = tempfile::tempdir().unwrap();
    // Plant valid-container but undecodable bytes for the URL.
    DiskCache::new(dir.path(), 1 << 20)
        .write(URL, &fixture("corrupt.png"))
        .unwrap();
    let fetcher = FakeFetcher::new(Duration::ZERO).route(URL, Route::Ok(png_bytes(4, 4)));
    let cache = cache_with(
        fetcher.clone(),
        ImageCacheConfig {
            disk_dir: Some(dir.path().to_owned()),
            ..test_config()
        },
    );
    assert_eq!(cache.load(URL).await.unwrap().width(), 4);
    assert_eq!(fetcher.calls(URL), 1);
    assert_eq!(cache.stats().disk_hits, 0);
    // The good bytes replaced the bad entry.
    let on_disk = DiskCache::new(dir.path(), 1 << 20)
        .read(URL)
        .unwrap()
        .unwrap();
    assert_eq!(on_disk, png_bytes(4, 4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lru_evicts_by_bytes_and_notifies() {
    // 32x32 BGRA = 4 KiB of pixels each; the budget fits two.
    let one =
        DecodedImage::byte_size(&decode(&png_bytes(32, 32), &DecodeLimits::default()).unwrap());
    let fetcher = FakeFetcher::new(Duration::ZERO)
        .route("u1", Route::Ok(png_bytes(32, 32)))
        .route("u2", Route::Ok(png_bytes(32, 32)))
        .route("u3", Route::Ok(png_bytes(32, 32)));
    let evicted = Arc::new(StdMutex::new(Vec::new()));
    let sink = evicted.clone();
    let cache = ImageCache::<DecodedImage>::builder(Handle::current())
        .config(ImageCacheConfig {
            memory_budget_bytes: one * 2 + one / 2,
            ..test_config()
        })
        .fetcher(fetcher.clone())
        .on_evict(move |k, v| sink.lock().unwrap().push((k.url().to_owned(), v.width())))
        .build()
        .unwrap();

    cache.load("u1").await.unwrap();
    cache.load("u2").await.unwrap();
    // Touch u1 so u2 is least recently used.
    assert!(cache.get_if_ready("u1").is_some());
    cache.load("u3").await.unwrap();

    assert_eq!(*evicted.lock().unwrap(), vec![("u2".to_owned(), 32)]);
    let stats = cache.stats();
    assert_eq!(stats.evictions, 1);
    assert_eq!(stats.memory_entries, 2);
    assert_eq!(stats.memory_bytes, one * 2);
    assert!(stats.memory_bytes <= stats.memory_budget);
    assert!(cache.get_if_ready("u2").is_none());
    assert!(cache.get_if_ready("u1").is_some() && cache.get_if_ready("u3").is_some());

    // Shrinking the budget and clearing also notify.
    cache.set_memory_budget(one);
    assert_eq!(cache.stats().memory_entries, 1);
    cache.clear_memory();
    assert_eq!(cache.stats().memory_entries, 0);
    assert_eq!(evicted.lock().unwrap().len(), 3);
    assert_eq!(cache.stats().evictions, 3);

    // An evicted image is reloaded on demand (no disk cache here, so from the network).
    cache.load("u2").await.unwrap();
    assert_eq!(fetcher.calls("u2"), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_found_is_negatively_cached() {
    const URL: &str = "https://cdn/missing.png";
    let fetcher =
        FakeFetcher::new(Duration::ZERO).route(URL, Route::Err(FetchError::NotFound(404)));
    let cache = cache_with(
        fetcher.clone(),
        ImageCacheConfig {
            not_found_ttl: Duration::from_millis(100),
            ..test_config()
        },
    );
    let err = MediaError::Fetch(FetchError::NotFound(404));
    assert_eq!(cache.load(URL).await.unwrap_err(), err);
    assert_eq!(cache.load(URL).await.unwrap_err(), err);
    assert!(matches!(cache.status(URL), ImageStatus::Failed(_)));
    assert_eq!(fetcher.calls(URL), 1, "404 is not retried or refetched");
    assert_eq!(cache.stats().negative_hits, 1);

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(matches!(cache.status(URL), ImageStatus::Absent));
    cache.load(URL).await.unwrap_err();
    assert_eq!(fetcher.calls(URL), 2, "retried after the TTL");

    cache.clear_failures();
    cache.load(URL).await.unwrap_err();
    assert_eq!(fetcher.calls(URL), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_errors_are_retried_permanent_ones_are_not() {
    let fetcher = FakeFetcher::new(Duration::ZERO)
        .route(
            "flaky",
            Route::FailThen(2, FetchError::Status(503), png_bytes(2, 2)),
        )
        .route("forbidden", Route::Err(FetchError::Status(403)))
        .route(
            "down",
            Route::Err(FetchError::Network("connection reset".into())),
        );
    let cache = cache_with(fetcher.clone(), test_config());

    assert!(cache.load("flaky").await.is_ok());
    assert_eq!(fetcher.calls("flaky"), 3);

    assert_eq!(
        cache.load("forbidden").await.unwrap_err(),
        MediaError::Fetch(FetchError::Status(403))
    );
    assert_eq!(fetcher.calls("forbidden"), 1);

    assert!(matches!(
        cache.load("down").await.unwrap_err(),
        MediaError::Fetch(FetchError::Network(_))
    ));
    assert_eq!(fetcher.calls("down"), 3, "max_attempts");
    // Remembered for `failure_backoff`.
    cache.load("down").await.unwrap_err();
    assert_eq!(fetcher.calls("down"), 3);
    let stats = cache.stats();
    assert_eq!(stats.network_requests, 3 + 1 + 3);
    assert_eq!(stats.fetch_failures, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hung_requests_time_out() {
    let fetcher = FakeFetcher::new(Duration::ZERO).route("hang", Route::Hang);
    let cache = cache_with(
        fetcher.clone(),
        ImageCacheConfig {
            request_timeout: Duration::from_millis(20),
            ..test_config()
        },
    );
    assert_eq!(
        cache.load("hang").await.unwrap_err(),
        MediaError::Fetch(FetchError::Timeout)
    );
    assert_eq!(fetcher.calls("hang"), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undecodable_download_fails_and_is_not_written_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = FakeFetcher::new(Duration::ZERO)
        .route("bad", Route::Ok(fixture("corrupt.png")))
        .route("huge", Route::Ok(fixture("oversized.png")));
    let cache = cache_with(
        fetcher,
        ImageCacheConfig {
            disk_dir: Some(dir.path().to_owned()),
            ..test_config()
        },
    );
    assert!(matches!(
        cache.load("bad").await.unwrap_err(),
        MediaError::Decode(DecodeError::Malformed(_))
    ));
    assert!(matches!(
        cache.load("huge").await.unwrap_err(),
        MediaError::Decode(DecodeError::TooLarge { .. })
    ));
    assert_eq!(cache.stats().decode_failures, 2);
    let disk = DiskCache::new(dir.path(), 1 << 20);
    assert_eq!(disk.read("bad").unwrap(), None);
    assert_eq!(disk.read("huge").unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_concurrency_is_bounded() {
    let fetcher = FakeFetcher::new(Duration::from_millis(40));
    for i in 0..6 {
        fetcher.route(&format!("u{i}"), Route::Ok(png_bytes(2, 2)));
    }
    let cache = cache_with(
        fetcher,
        ImageCacheConfig {
            max_concurrent_fetches: 2,
            ..test_config()
        },
    );
    let start = Instant::now();
    let handles: Vec<_> = (0..6).map(|i| cache.request(&format!("u{i}"))).collect();
    for h in handles {
        h.await.unwrap();
    }
    // 6 fetches of 40 ms, 2 at a time: at least 3 rounds.
    assert!(
        start.elapsed() >= Duration::from_millis(115),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn shutdown_cancels_waiters_and_allows_a_new_request() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let fetcher = FakeFetcher::new(Duration::ZERO).route("hang", Route::Hang);
    let cache: ImageCache = ImageCache::builder(rt.handle().clone())
        .config(test_config())
        .fetcher(fetcher)
        .build()
        .unwrap();
    let handle = cache.request("hang");
    rt.shutdown_background();

    let other = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_eq!(other.block_on(handle.wait()), Err(MediaError::Cancelled));
    assert!(matches!(cache.status("hang"), ImageStatus::Absent));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_scaled_picks_the_variant() {
    use gpui_chat_core::{Emote, ImageScale};
    let emote = Emote::twitch("25", "Kappa");
    let x2 = emote.urls.get(ImageScale::X2).unwrap();
    let fetcher = FakeFetcher::new(Duration::ZERO).route(x2, Route::Ok(png_bytes(56, 56)));
    let cache = cache_with(fetcher.clone(), test_config());
    let (selected, handle) = cache.request_scaled(&emote.urls, 2.0, 1.0).unwrap();
    assert_eq!(selected.scale, ImageScale::X2);
    let img = handle.await.unwrap();
    assert_eq!(
        selected.logical_size((img.width(), img.height()), 1.0),
        (28.0, 28.0)
    );
    assert_eq!(fetcher.total_calls(), 1);
    assert!(
        cache
            .request_scaled(&gpui_chat_core::ImageUrls::new(), 2.0, 1.0)
            .is_none()
    );
}

/// A custom value type, as PR 2.2 will use for GPUI images.
struct Wrapped {
    frames: usize,
    bytes: usize,
}

impl CacheValue for Wrapped {
    fn from_decoded(image: DecodedImage) -> Self {
        let bytes = image.byte_size();
        Self {
            frames: image.into_frames().len(),
            bytes,
        }
    }

    fn byte_size(&self) -> usize {
        self.bytes
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_cache_value() {
    let fetcher = FakeFetcher::new(Duration::ZERO).route("a", Route::Ok(fixture("animated.gif")));
    let cache = ImageCache::<Wrapped>::builder(Handle::current())
        .config(test_config())
        .fetcher(fetcher)
        .build()
        .unwrap();
    let w = cache.load("a").await.unwrap();
    assert_eq!(w.frames, 4);
    assert_eq!(cache.stats().memory_bytes, w.bytes);
}
