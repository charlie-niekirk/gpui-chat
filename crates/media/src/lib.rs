//! Image fetch, disk cache, decoding and memory budget for emotes, badges and emoji.
//!
//! GPUI-free: the app's `ImageCache` adapter (PR 2.2) turns [`DecodedImage`] frames into GPUI
//! `RenderImage`s. Entry point: [`ImageCache`].
//!
//! * [`ImageCache::request`] / [`ImageCache::load`]: memory LRU → disk → network, one load per
//!   URL however many callers ask ([in-flight de-duplication](ImageCache)), bounded network
//!   and decode concurrency, timeouts, retries for transient errors, remembered failures
//!   (404s for [`ImageCacheConfig::not_found_ttl`]).
//! * [`ImageCache::get_if_ready`]: the synchronous render-path lookup.
//! * [`decode()`]: PNG/APNG, GIF and (animated) WebP into BGRA8 straight-alpha frames with
//!   browser-style delay clamping, under [`DecodeLimits`]. See the [`decode`](mod@decode)
//!   module docs for why that pixel format.
//! * [`DiskCache`]: URL-hash keyed, size-capped, atomic writes.
//! * [`scale`]: which 1x–4x variant to fetch for a display scale × emote scale.

mod cache;
pub mod decode;
mod disk;
mod error;
mod fetch;
mod memory;
pub mod scale;

#[cfg(test)]
mod test_support;

pub use cache::{
    CacheStats, CacheValue, ImageCache, ImageCacheBuilder, ImageCacheConfig, ImageHandle,
    ImageStatus,
};
pub use decode::{DecodeLimits, DecodedFrame, DecodedImage, SourceFormat, Timeline, decode};
pub use disk::DiskCache;
pub use error::{BuildError, DecodeError, FetchError, MediaError};
pub use fetch::{FetchFuture, Fetcher, HttpConfig, HttpFetcher, RetryPolicy, user_agent};
pub use memory::ImageKey;
