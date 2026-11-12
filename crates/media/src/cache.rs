//! [`ImageCache`]: memory LRU → disk cache → network, with in-flight de-duplication.

use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use gpui_chat_core::ImageUrls;
use tokio::runtime::Handle;
use tokio::sync::{Semaphore, watch};

use crate::decode::{DecodeLimits, DecodedImage, decode};
use crate::disk::DiskCache;
use crate::error::{BuildError, DecodeError, FetchError, MediaError};
use crate::fetch::{Fetcher, HttpConfig, HttpFetcher, RetryPolicy};
use crate::memory::{ByteLru, ImageKey};
use crate::scale::{self, Selected};

/// What the memory cache stores. [`DecodedImage`] is the default; the app's GPUI adapter
/// (PR 2.2) can store its own type (e.g. a wrapper around `Arc<RenderImage>`) so the pixels
/// are held once, not once here and once in GPUI.
pub trait CacheValue: Send + Sync + 'static {
    /// Converts a freshly decoded image. Runs on the blocking pool, right after decoding.
    fn from_decoded(image: DecodedImage) -> Self;
    /// Bytes charged against the memory budget.
    fn byte_size(&self) -> usize;
}

impl CacheValue for DecodedImage {
    fn from_decoded(image: DecodedImage) -> Self {
        image
    }

    fn byte_size(&self) -> usize {
        DecodedImage::byte_size(self)
    }
}

/// Settings for [`ImageCache`]. `Default` gives the production values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCacheConfig {
    /// Directory for the disk cache (the caller's platform cache dir); `None` disables it.
    pub disk_dir: Option<PathBuf>,
    /// Size cap of the disk cache.
    pub disk_max_bytes: u64,
    /// Byte budget of decoded images held in memory.
    pub memory_budget_bytes: usize,
    /// Network requests in flight at once.
    pub max_concurrent_fetches: usize,
    /// Decodes running at once on the blocking pool.
    pub max_concurrent_decodes: usize,
    /// Per-attempt timeout (also passed to the HTTP client).
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    pub max_download_bytes: u64,
    pub retry: RetryPolicy,
    /// How long a 404/410 or undecodable image is remembered before it is tried again.
    pub not_found_ttl: Duration,
    /// How long other failures (retries exhausted) are remembered.
    pub failure_backoff: Duration,
    pub decode_limits: DecodeLimits,
}

impl Default for ImageCacheConfig {
    fn default() -> Self {
        let http = HttpConfig::default();
        let cpus = std::thread::available_parallelism().map_or(2, usize::from);
        Self {
            disk_dir: None,
            disk_max_bytes: 256 * 1024 * 1024,
            memory_budget_bytes: 256 * 1024 * 1024,
            max_concurrent_fetches: 8,
            max_concurrent_decodes: (cpus / 2).clamp(1, 4),
            request_timeout: http.request_timeout,
            connect_timeout: http.connect_timeout,
            max_download_bytes: http.max_body_bytes,
            retry: RetryPolicy::default(),
            not_found_ttl: Duration::from_secs(10 * 60),
            failure_backoff: Duration::from_secs(30),
            decode_limits: DecodeLimits::default(),
        }
    }
}

/// Counters since the cache was built, plus current sizes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups ([`ImageCache::get_if_ready`], [`ImageCache::request`]) answered from memory.
    pub memory_hits: u64,
    /// Lookups that were not in memory.
    pub memory_misses: u64,
    /// Requests that joined a load already in flight instead of starting one.
    pub dedup_joins: u64,
    /// Requests answered from the remembered-failure (negative) cache.
    pub negative_hits: u64,
    /// Loads satisfied from the disk cache.
    pub disk_hits: u64,
    /// Network attempts, including retries.
    pub network_requests: u64,
    /// Successful downloads.
    pub network_fetches: u64,
    pub fetch_failures: u64,
    pub decode_failures: u64,
    pub evictions: u64,
    /// Current decoded bytes in memory.
    pub memory_bytes: usize,
    pub memory_entries: usize,
    pub memory_budget: usize,
    /// Loads currently running.
    pub in_flight: usize,
    /// Disk cache size, once it has been indexed.
    pub disk_bytes: Option<u64>,
}

#[derive(Default)]
struct Counters {
    memory_hits: AtomicU64,
    memory_misses: AtomicU64,
    dedup_joins: AtomicU64,
    negative_hits: AtomicU64,
    disk_hits: AtomicU64,
    network_requests: AtomicU64,
    network_fetches: AtomicU64,
    fetch_failures: AtomicU64,
    decode_failures: AtomicU64,
    evictions: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

type LoadResult<V> = Result<Arc<V>, MediaError>;
/// Downloaded, still-encoded bytes shared between decode and the disk write.
type Bytes = Arc<Vec<u8>>;
type Slot<V> = Option<LoadResult<V>>;
type EvictFn<V> = Arc<dyn Fn(&ImageKey, &Arc<V>) + Send + Sync>;
type ReadyFn = Arc<dyn Fn(&ImageKey) + Send + Sync>;

struct Failure {
    error: MediaError,
    until: Instant,
}

struct State<V> {
    lru: ByteLru<V>,
    in_flight: HashMap<ImageKey, watch::Sender<Slot<V>>>,
    failures: HashMap<ImageKey, Failure>,
}

struct Inner<V: CacheValue> {
    config: ImageCacheConfig,
    fetcher: Arc<dyn Fetcher>,
    disk: Option<Arc<DiskCache>>,
    runtime: Handle,
    state: Mutex<State<V>>,
    fetch_permits: Semaphore,
    decode_permits: Arc<Semaphore>,
    counters: Counters,
    on_evict: Option<EvictFn<V>>,
    on_ready: Option<ReadyFn>,
}

/// Fetches, caches and decodes images by URL. Cheap to clone (shared state).
///
/// Lookup order: memory LRU → in-flight load (joined) → remembered failure → new load task on
/// the tokio runtime (disk cache → network with retries → decode on the blocking pool →
/// disk write → memory insert). Every request for the same URL while a load runs shares that
/// one load.
pub struct ImageCache<V: CacheValue = DecodedImage> {
    inner: Arc<Inner<V>>,
}

impl<V: CacheValue> Clone for ImageCache<V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Builds an [`ImageCache`].
pub struct ImageCacheBuilder<V: CacheValue = DecodedImage> {
    runtime: Handle,
    config: ImageCacheConfig,
    fetcher: Option<Arc<dyn Fetcher>>,
    on_evict: Option<EvictFn<V>>,
    on_ready: Option<ReadyFn>,
}

impl<V: CacheValue> ImageCacheBuilder<V> {
    #[must_use]
    pub fn config(mut self, config: ImageCacheConfig) -> Self {
        self.config = config;
        self
    }

    /// Replaces the HTTP client (tests, or a different transport).
    #[must_use]
    pub fn fetcher(mut self, fetcher: Arc<dyn Fetcher>) -> Self {
        self.fetcher = Some(fetcher);
        self
    }

    /// Called for every entry that leaves the memory cache (budget eviction, replacement,
    /// [`ImageCache::clear_memory`], [`ImageCache::set_memory_budget`]). Runs on whichever
    /// thread caused it (usually a tokio worker), never with internal locks held. Keep it
    /// cheap: e.g. push the key onto a channel the UI drains to release GPU textures.
    #[must_use]
    pub fn on_evict(mut self, f: impl Fn(&ImageKey, &Arc<V>) + Send + Sync + 'static) -> Self {
        self.on_evict = Some(Arc::new(f));
        self
    }

    /// Called when a load finishes, successfully or not (so the UI can repaint). Same threading
    /// rules as [`Self::on_evict`].
    #[must_use]
    pub fn on_ready(mut self, f: impl Fn(&ImageKey) + Send + Sync + 'static) -> Self {
        self.on_ready = Some(Arc::new(f));
        self
    }

    pub fn build(self) -> Result<ImageCache<V>, BuildError> {
        let config = self.config;
        let fetcher = match self.fetcher {
            Some(f) => f,
            None => Arc::new(HttpFetcher::new(HttpConfig {
                request_timeout: config.request_timeout,
                connect_timeout: config.connect_timeout,
                max_body_bytes: config.max_download_bytes,
            })?),
        };
        let disk = config
            .disk_dir
            .as_ref()
            .map(|dir| Arc::new(DiskCache::new(dir, config.disk_max_bytes)));
        let inner = Inner {
            fetch_permits: Semaphore::new(config.max_concurrent_fetches.max(1)),
            decode_permits: Arc::new(Semaphore::new(config.max_concurrent_decodes.max(1))),
            state: Mutex::new(State {
                lru: ByteLru::new(config.memory_budget_bytes),
                in_flight: HashMap::new(),
                failures: HashMap::new(),
            }),
            config,
            fetcher,
            disk,
            runtime: self.runtime,
            counters: Counters::default(),
            on_evict: self.on_evict,
            on_ready: self.on_ready,
        };
        Ok(ImageCache {
            inner: Arc::new(inner),
        })
    }
}

/// Where a URL stands, without starting anything.
#[derive(Debug, Clone)]
pub enum ImageStatus<V> {
    Ready(Arc<V>),
    Loading,
    /// A remembered failure; it is retried after its TTL.
    Failed(MediaError),
    /// Never requested, evicted, or its failure expired.
    Absent,
}

impl<V: CacheValue> ImageCache<V> {
    /// A builder whose load tasks run on `runtime` (the app's backend runtime).
    pub fn builder(runtime: Handle) -> ImageCacheBuilder<V> {
        ImageCacheBuilder {
            runtime,
            config: ImageCacheConfig::default(),
            fetcher: None,
            on_evict: None,
            on_ready: None,
        }
    }

    fn state(&self) -> MutexGuard<'_, State<V>> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The image if it is in memory, else `None`. Never blocks on I/O and never starts a
    /// load: this is the render-path call. Marks the entry recently used.
    pub fn get_if_ready(&self, url: &str) -> Option<Arc<V>> {
        let hit = self.state().lru.get(url);
        bump(if hit.is_some() {
            &self.inner.counters.memory_hits
        } else {
            &self.inner.counters.memory_misses
        });
        hit
    }

    /// Where `url` stands, without starting a load or changing recency.
    pub fn status(&self, url: &str) -> ImageStatus<V> {
        let state = self.state();
        if let Some(v) = state.lru.peek(url) {
            ImageStatus::Ready(v.clone())
        } else if state.in_flight.contains_key(url) {
            ImageStatus::Loading
        } else {
            match state.failures.get(url) {
                Some(f) if f.until > Instant::now() => ImageStatus::Failed(f.error.clone()),
                _ => ImageStatus::Absent,
            }
        }
    }

    /// Starts loading `url` unless it is in memory, already loading or recently failed, and
    /// returns a handle to the result. Non-blocking; the load continues if the handle is
    /// dropped (warming the cache).
    pub fn request(&self, url: &str) -> ImageHandle<V> {
        let counters = &self.inner.counters;
        let mut state = self.state();
        if let Some(v) = state.lru.get(url) {
            bump(&counters.memory_hits);
            return ImageHandle::ready(Ok(v));
        }
        bump(&counters.memory_misses);
        if let Some(tx) = state.in_flight.get(url) {
            bump(&counters.dedup_joins);
            return ImageHandle::pending(tx.subscribe());
        }
        match state.failures.get(url) {
            Some(f) if f.until > Instant::now() => {
                bump(&counters.negative_hits);
                return ImageHandle::ready(Err(f.error.clone()));
            }
            Some(_) => {
                state.failures.remove(url);
            }
            None => {}
        }
        let key = ImageKey::new(url);
        let (tx, rx) = watch::channel(None);
        state.in_flight.insert(key.clone(), tx);
        drop(state);

        let guard = LoadGuard {
            cache: self.clone(),
            key,
            finished: false,
        };
        self.inner.runtime.spawn(guard.run());
        ImageHandle::pending(rx)
    }

    /// [`Self::request`] and wait for the result.
    pub async fn load(&self, url: &str) -> Result<Arc<V>, MediaError> {
        self.request(url).await
    }

    /// For the render path: the image if ready, otherwise starts (or joins) its load and
    /// returns `None`. Use [`ImageCacheBuilder::on_ready`] to learn when to repaint.
    pub fn get_or_request(&self, url: &str) -> Option<Arc<V>> {
        match self.request(url).try_get() {
            Some(Ok(v)) => Some(v),
            _ => None,
        }
    }

    /// Picks the variant of `urls` for this display and emote scale (see [`crate::scale`]) and
    /// requests it. `None` if `urls` is empty.
    pub fn request_scaled<'u>(
        &self,
        urls: &'u ImageUrls,
        display_scale: f32,
        emote_scale: f32,
    ) -> Option<(Selected<'u>, ImageHandle<V>)> {
        let selected = scale::select(urls, display_scale, emote_scale)?;
        Some((selected, self.request(selected.url)))
    }

    /// Changes the memory budget, evicting as needed.
    pub fn set_memory_budget(&self, bytes: usize) {
        let evicted = self.state().lru.set_budget(bytes);
        self.notify_evicted(evicted);
    }

    /// Drops every decoded image from memory (disk cache untouched).
    pub fn clear_memory(&self) {
        let evicted = self.state().lru.clear();
        self.notify_evicted(evicted);
    }

    /// Forgets remembered failures so the next request retries immediately.
    pub fn clear_failures(&self) {
        self.state().failures.clear();
    }

    pub fn stats(&self) -> CacheStats {
        let c = &self.inner.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        // Read the disk index first: it has its own lock, which must never be taken while the
        // state lock is held (the render path takes the state lock).
        let disk_bytes = self.inner.disk.as_ref().and_then(|d| d.total_bytes());
        let state = self.state();
        CacheStats {
            memory_hits: get(&c.memory_hits),
            memory_misses: get(&c.memory_misses),
            dedup_joins: get(&c.dedup_joins),
            negative_hits: get(&c.negative_hits),
            disk_hits: get(&c.disk_hits),
            network_requests: get(&c.network_requests),
            network_fetches: get(&c.network_fetches),
            fetch_failures: get(&c.fetch_failures),
            decode_failures: get(&c.decode_failures),
            evictions: get(&c.evictions),
            memory_bytes: state.lru.bytes(),
            memory_entries: state.lru.len(),
            memory_budget: state.lru.budget(),
            in_flight: state.in_flight.len(),
            disk_bytes,
        }
    }

    fn notify_evicted(&self, evicted: Vec<(ImageKey, Arc<V>)>) {
        for (key, value) in &evicted {
            bump(&self.inner.counters.evictions);
            if let Some(f) = &self.inner.on_evict {
                f(key, value);
            }
        }
    }

    /// The load pipeline for one URL. Runs on the tokio runtime. On success also returns the
    /// downloaded bytes when they still need writing to the disk cache, which the caller does
    /// after publishing the image.
    async fn load_uncached(&self, key: &ImageKey) -> Result<(Arc<V>, Option<Bytes>), MediaError> {
        let url = key.url();
        if let Some(disk) = &self.inner.disk {
            let (disk, u) = (disk.clone(), url.to_owned());
            let cached = match tokio::task::spawn_blocking(move || disk.read(&u)).await {
                Ok(read) => read,
                Err(e) if e.is_panic() => Err(std::io::Error::other("disk read panicked")),
                Err(_) => return Err(MediaError::Cancelled),
            };
            match cached {
                Ok(Some(bytes)) => match self.decode(Arc::new(bytes)).await {
                    Ok(v) => {
                        bump(&self.inner.counters.disk_hits);
                        return Ok((v, None));
                    }
                    Err(MediaError::Cancelled) => return Err(MediaError::Cancelled),
                    Err(err) => {
                        tracing::debug!(url, %err, "disk cache entry does not decode; refetching");
                        self.disk_op(move |d, u| d.remove(u), url).await;
                    }
                },
                Ok(None) => {}
                Err(err) => tracing::debug!(url, %err, "disk cache read failed"),
            }
        }

        let bytes = Arc::new(self.fetch_with_retry(url).await.inspect_err(|_| {
            bump(&self.inner.counters.fetch_failures);
        })?);
        bump(&self.inner.counters.network_fetches);
        let value = self.decode(bytes.clone()).await.inspect_err(|_| {
            bump(&self.inner.counters.decode_failures);
        })?;
        // Only bytes that decoded are worth keeping.
        Ok((value, Some(bytes)))
    }

    async fn disk_op(
        &self,
        op: impl FnOnce(&DiskCache, &str) -> std::io::Result<()> + Send + 'static,
        url: &str,
    ) {
        let Some(disk) = self.inner.disk.clone() else {
            return;
        };
        let u = url.to_owned();
        match tokio::task::spawn_blocking(move || op(&disk, &u)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::debug!(url, %err, "disk cache write failed"),
            Err(_) => {}
        }
    }

    async fn fetch_with_retry(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        let policy = self.inner.config.retry;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = {
                let _permit = self
                    .inner
                    .fetch_permits
                    .acquire()
                    .await
                    .expect("semaphore never closed");
                bump(&self.inner.counters.network_requests);
                tokio::time::timeout(
                    self.inner.config.request_timeout,
                    self.inner.fetcher.fetch(url),
                )
                .await
                .unwrap_or(Err(FetchError::Timeout))
            };
            match result {
                Ok(bytes) => return Ok(bytes),
                Err(err) if err.is_transient() && attempt < policy.max_attempts.max(1) => {
                    let delay = policy.delay_after(attempt);
                    tracing::debug!(url, %err, attempt, ?delay, "retrying image fetch");
                    tokio::time::sleep(delay).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Decodes and converts on the blocking pool, at most `max_concurrent_decodes` at a time.
    async fn decode(&self, bytes: Bytes) -> LoadResult<V> {
        let permit = self
            .inner
            .decode_permits
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed");
        let limits = self.inner.config.decode_limits;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            decode(&bytes, &limits).map(|img| Arc::new(V::from_decoded(img)))
        })
        .await
        .map_err(|e| {
            if e.is_panic() {
                // Remembered like any undecodable image, so a bad file is not refetched and
                // re-panicked on every repaint.
                tracing::warn!("image decoder panicked");
                MediaError::Decode(DecodeError::Malformed("decoder panicked".into()))
            } else {
                MediaError::Cancelled
            }
        })?
        .map_err(MediaError::from)
    }

    /// Publishes a finished load to the memory cache, waiters and callbacks.
    fn finish(&self, key: &ImageKey, result: LoadResult<V>) {
        let mut state = self.state();
        let evicted = match &result {
            Ok(v) => state.lru.insert(key.clone(), v.clone(), v.byte_size()),
            Err(err) => {
                if let Some(ttl) = self.failure_ttl(err) {
                    state.failures.insert(
                        key.clone(),
                        Failure {
                            error: err.clone(),
                            until: Instant::now() + ttl,
                        },
                    );
                    if state.failures.len() > 4096 {
                        let now = Instant::now();
                        state.failures.retain(|_, f| f.until > now);
                    }
                }
                Vec::new()
            }
        };
        if let Some(tx) = state.in_flight.remove(key) {
            tx.send_replace(Some(result));
        }
        drop(state);
        self.notify_evicted(evicted);
        if let Some(f) = &self.inner.on_ready {
            f(key);
        }
    }

    fn failure_ttl(&self, err: &MediaError) -> Option<Duration> {
        let config = &self.inner.config;
        match err {
            MediaError::Cancelled => None,
            MediaError::Fetch(e) if e.is_transient() => Some(config.failure_backoff),
            MediaError::Fetch(FetchError::Status(_)) => Some(config.failure_backoff),
            MediaError::Fetch(_) | MediaError::Decode(_) => Some(config.not_found_ttl),
        }
    }
}

/// Owns an in-flight entry. If the load future is dropped unfinished (runtime shutdown), waiters get [`MediaError::Cancelled`] instead of hanging and the URL can be
/// requested again.
struct LoadGuard<V: CacheValue> {
    cache: ImageCache<V>,
    key: ImageKey,
    finished: bool,
}

impl<V: CacheValue> LoadGuard<V> {
    async fn run(mut self) {
        let (result, to_write) = match self.cache.load_uncached(&self.key).await {
            Ok((value, to_write)) => (Ok(value), to_write),
            Err(err) => {
                tracing::debug!(url = %self.key, %err, "image load failed");
                (Err(err), None)
            }
        };
        self.finished = true;
        // Waiters get the image first; persisting it (and disk eviction) happens afterwards.
        self.cache.finish(&self.key, result);
        if let Some(bytes) = to_write {
            self.cache
                .disk_op(move |d, u| d.write(u, &bytes), self.key.url())
                .await;
        }
    }
}

impl<V: CacheValue> Drop for LoadGuard<V> {
    fn drop(&mut self) {
        if !self.finished {
            let tx = self.cache.state().in_flight.remove(&self.key);
            if let Some(tx) = tx {
                tx.send_replace(Some(Err(MediaError::Cancelled)));
            }
        }
    }
}

/// A pending or finished image load. `.await` it (or call [`Self::wait`]) for the result, or
/// poll [`Self::try_get`] without blocking. Dropping it does not cancel the load.
pub struct ImageHandle<V> {
    inner: HandleInner<V>,
}

enum HandleInner<V> {
    Ready(LoadResult<V>),
    Pending(watch::Receiver<Slot<V>>),
}

impl<V> std::fmt::Debug for ImageHandle<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.inner {
            HandleInner::Ready(Ok(_)) => "ready",
            HandleInner::Ready(Err(_)) => "failed",
            HandleInner::Pending(_) => "pending",
        };
        f.debug_struct("ImageHandle")
            .field("state", &state)
            .finish()
    }
}

impl<V> ImageHandle<V> {
    fn ready(result: LoadResult<V>) -> Self {
        Self {
            inner: HandleInner::Ready(result),
        }
    }

    fn pending(rx: watch::Receiver<Slot<V>>) -> Self {
        Self {
            inner: HandleInner::Pending(rx),
        }
    }

    /// The result if the load has finished.
    pub fn try_get(&self) -> Option<Result<Arc<V>, MediaError>> {
        match &self.inner {
            HandleInner::Ready(r) => Some(r.clone()),
            HandleInner::Pending(rx) => rx.borrow().clone(),
        }
    }

    /// Waits for the load to finish.
    pub async fn wait(self) -> Result<Arc<V>, MediaError> {
        match self.inner {
            HandleInner::Ready(r) => r,
            HandleInner::Pending(mut rx) => match rx.wait_for(Option::is_some).await {
                Ok(slot) => slot.clone().unwrap_or(Err(MediaError::Cancelled)),
                Err(_) => Err(MediaError::Cancelled),
            },
        }
    }
}

impl<V: Send + Sync + 'static> IntoFuture for ImageHandle<V> {
    type Output = Result<Arc<V>, MediaError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.wait())
    }
}

#[cfg(test)]
mod tests;
