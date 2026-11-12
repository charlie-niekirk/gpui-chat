//! On-disk cache of downloaded (still encoded) image bytes, keyed by a hash of the URL.
//!
//! All methods block; [`crate::ImageCache`] calls them from tokio's blocking pool.
//!
//! Layout under the caller's directory:
//!
//! ```text
//! <dir>/v1/<h0h1>/<32 hex chars of SHA-256(url)>   entries
//! <dir>/v1/tmp/                                    in-progress writes
//! ```
//!
//! Each entry stores a small header (magic, the full URL, body length) followed by the body.
//! Writes go to a uniquely named temp file that is then renamed over the entry, so readers
//! (in this or another process) see either the old or the new complete file, never a partial
//! one. Entries are not fsynced; a file torn by power loss fails the header/length check on
//! read and is deleted, which turns it into an ordinary miss.
//!
//! The size cap is enforced against an index built lazily on first use from a directory scan
//! (recency = file mtime, refreshed at most hourly on reads). The index only sees this
//! process's writes; another process sharing the directory makes the total an estimate until
//! the next start.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"GCMI";
const FORMAT_VERSION: u8 = 1;
const LAYOUT_DIR: &str = "v1";
const TMP_DIR: &str = "tmp";
/// Temp files older than this are leftovers from a crash and are deleted on startup.
const STALE_TMP: Duration = Duration::from_secs(60 * 60);
/// Reads refresh an entry's mtime only when it is older than this, to avoid a write per read.
const TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Eviction frees down to this fraction of the cap so it does not run on every write.
const EVICT_TO_PERCENT: u64 = 90;

#[derive(Debug, Clone, Copy)]
struct Meta {
    size: u64,
    used: SystemTime,
}

#[derive(Default)]
struct Index {
    entries: HashMap<String, Meta>,
    total: u64,
}

impl Index {
    fn put(&mut self, name: String, meta: Meta) {
        if let Some(old) = self.entries.insert(name, meta) {
            self.total -= old.size;
        }
        self.total += meta.size;
    }

    fn take(&mut self, name: &str) -> Option<Meta> {
        let meta = self.entries.remove(name)?;
        self.total -= meta.size;
        Some(meta)
    }
}

/// Disk cache with a byte cap. Cheap to construct; touches the disk on first use.
pub struct DiskCache {
    root: PathBuf,
    max_bytes: u64,
    index: Mutex<Option<Index>>,
    tmp_counter: AtomicU64,
}

impl DiskCache {
    /// A cache in `dir` (created on first use) holding at most `max_bytes` of entries.
    pub fn new(dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self {
            root: dir.into().join(LAYOUT_DIR),
            max_bytes,
            index: Mutex::new(None),
            tmp_counter: AtomicU64::new(0),
        }
    }

    /// The stored bytes for `url`, or `None` on a miss. A corrupt or truncated entry is
    /// deleted and reported as a miss.
    pub fn read(&self, url: &str) -> io::Result<Option<Vec<u8>>> {
        self.ensure_index()?;
        let name = file_name(url);
        let path = self.entry_path(&name);
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.with_index(|ix| ix.take(&name));
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let Some(body) = parse_entry(&data, url) else {
            tracing::debug!(url, "discarding corrupt disk cache entry");
            self.remove(url)?;
            return Ok(None);
        };
        let body = body.to_vec();
        let now = SystemTime::now();
        let stale = self.with_index(|ix| {
            let last_used = match ix.entries.get_mut(&name) {
                Some(meta) => std::mem::replace(&mut meta.used, now),
                // Written by another process since the scan.
                None => {
                    let size = data.len() as u64;
                    ix.put(name, Meta { size, used: now });
                    SystemTime::UNIX_EPOCH
                }
            };
            now.duration_since(last_used).unwrap_or_default() > TOUCH_INTERVAL
        });
        if stale {
            // Persist recency for the next start; failure only costs eviction accuracy.
            let _ = OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(now));
        }
        Ok(Some(body))
    }

    /// Stores `body` for `url` atomically, then evicts old entries if over the cap.
    pub fn write(&self, url: &str, body: &[u8]) -> io::Result<()> {
        self.ensure_index()?;
        let name = file_name(url);
        let path = self.entry_path(&name);
        let tmp = self.root.join(TMP_DIR).join(format!(
            "{name}.{}.{}.tmp",
            std::process::id(),
            self.tmp_counter.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = File::create(&tmp)?;
            file.write_all(MAGIC)?;
            file.write_all(&[FORMAT_VERSION])?;
            file.write_all(&(url.len() as u32).to_le_bytes())?;
            file.write_all(url.as_bytes())?;
            file.write_all(&(body.len() as u64).to_le_bytes())?;
            file.write_all(body)?;
            drop(file);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&tmp, &path)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        let size = header_len(url) + body.len() as u64;
        let over = self.with_index(|ix| {
            ix.put(
                name,
                Meta {
                    size,
                    used: SystemTime::now(),
                },
            );
            ix.total > self.max_bytes
        });
        if over {
            self.evict();
        }
        Ok(())
    }

    /// Deletes the entry for `url`, if any.
    pub fn remove(&self, url: &str) -> io::Result<()> {
        let name = file_name(url);
        self.with_index(|ix| ix.take(&name));
        match fs::remove_file(self.entry_path(&name)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Bytes on disk according to the index, or `None` before first use.
    pub fn total_bytes(&self) -> Option<u64> {
        self.index
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|ix| ix.total)
    }

    /// The directory entries live in (`<dir>/v1`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry_path(&self, name: &str) -> PathBuf {
        self.root.join(&name[..2]).join(name)
    }

    fn with_index<R>(&self, f: impl FnOnce(&mut Index) -> R) -> R {
        let mut guard = self.index.lock().unwrap_or_else(PoisonError::into_inner);
        f(guard.get_or_insert_with(Index::default))
    }

    fn ensure_index(&self) -> io::Result<()> {
        let mut guard = self.index.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(self.scan()?);
        }
        Ok(())
    }

    /// Creates the layout, drops stale temp files and indexes existing entries.
    fn scan(&self) -> io::Result<Index> {
        let tmp_dir = self.root.join(TMP_DIR);
        fs::create_dir_all(&tmp_dir)?;
        let now = SystemTime::now();
        for entry in fs::read_dir(&tmp_dir)?.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| now.duration_since(t).unwrap_or_default() > STALE_TMP)
                .unwrap_or(true);
            if old {
                let _ = fs::remove_file(entry.path());
            }
        }
        let mut index = Index::default();
        for shard in fs::read_dir(&self.root)?.flatten() {
            if shard.file_name() == TMP_DIR || !shard.path().is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())?.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !meta.is_file() || !is_entry_name(&name) {
                    continue;
                }
                let used = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                index.put(
                    name,
                    Meta {
                        size: meta.len(),
                        used,
                    },
                );
            }
        }
        tracing::debug!(
            entries = index.entries.len(),
            bytes = index.total,
            dir = %self.root.display(),
            "disk image cache indexed"
        );
        Ok(index)
    }

    /// Deletes least recently used entries until under [`EVICT_TO_PERCENT`] of the cap.
    fn evict(&self) {
        let target = self.max_bytes / 100 * EVICT_TO_PERCENT;
        let victims: Vec<String> = self.with_index(|ix| {
            let mut by_age: Vec<(&String, &Meta)> = ix.entries.iter().collect();
            by_age.sort_by_key(|(_, m)| m.used);
            let mut total = ix.total;
            let mut victims = Vec::new();
            for (name, meta) in by_age {
                if total <= target {
                    break;
                }
                total -= meta.size;
                victims.push(name.clone());
            }
            for name in &victims {
                ix.take(name);
            }
            victims
        });
        for name in &victims {
            if let Err(e) = fs::remove_file(self.entry_path(name))
                && e.kind() != io::ErrorKind::NotFound
            {
                tracing::debug!(%e, name, "could not evict disk cache entry");
            }
        }
        if !victims.is_empty() {
            tracing::debug!(evicted = victims.len(), "disk image cache over cap");
        }
    }
}

/// 128 bits of SHA-256(url) as lowercase hex.
fn file_name(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

fn is_entry_name(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn header_len(url: &str) -> u64 {
    (MAGIC.len() + 1 + 4 + url.len() + 8) as u64
}

/// The body if `data` is a complete entry for exactly `url`.
fn parse_entry<'a>(data: &'a [u8], url: &str) -> Option<&'a [u8]> {
    let rest = data.strip_prefix(MAGIC)?;
    let (&version, rest) = rest.split_first()?;
    if version != FORMAT_VERSION {
        return None;
    }
    let (len, rest) = rest.split_at_checked(4)?;
    let url_len = u32::from_le_bytes(len.try_into().ok()?) as usize;
    let (stored_url, rest) = rest.split_at_checked(url_len)?;
    if stored_url != url.as_bytes() {
        return None;
    }
    let (len, body) = rest.split_at_checked(8)?;
    let body_len = u64::from_le_bytes(len.try_into().ok()?);
    (body.len() as u64 == body_len).then_some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://cdn.example/emote/1/2x.webp";

    #[test]
    fn round_trip_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path(), 1 << 20);
        assert_eq!(cache.read(URL).unwrap(), None);
        cache.write(URL, b"hello").unwrap();
        assert_eq!(cache.read(URL).unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(cache.total_bytes(), Some(header_len(URL) + 5));

        // A new instance (an app restart) indexes and reads the same entry.
        let reopened = DiskCache::new(dir.path(), 1 << 20);
        assert_eq!(reopened.total_bytes(), None);
        assert_eq!(reopened.read(URL).unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(reopened.total_bytes(), Some(header_len(URL) + 5));

        reopened.remove(URL).unwrap();
        assert_eq!(reopened.read(URL).unwrap(), None);
        assert_eq!(reopened.total_bytes(), Some(0));
    }

    #[test]
    fn truncated_or_foreign_entries_are_misses_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path(), 1 << 20);
        cache.write(URL, b"0123456789").unwrap();
        let path = cache.entry_path(&file_name(URL));
        let full = fs::read(&path).unwrap();
        fs::write(&path, &full[..full.len() - 3]).unwrap();
        assert_eq!(cache.read(URL).unwrap(), None);
        assert!(!path.exists(), "corrupt entry deleted");

        // An entry whose header names a different URL (hash collision / tampering).
        cache.write("https://other", b"x").unwrap();
        let other = fs::read(cache.entry_path(&file_name("https://other"))).unwrap();
        fs::write(&path, other).unwrap();
        assert_eq!(cache.read(URL).unwrap(), None);

        // Garbage and an empty file.
        fs::write(&path, b"garbage").unwrap();
        assert_eq!(cache.read(URL).unwrap(), None);
        fs::write(&path, b"").unwrap();
        assert_eq!(cache.read(URL).unwrap(), None);
    }

    #[test]
    fn evicts_oldest_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        let entry = header_len("u0") + 1000;
        let cache = DiskCache::new(dir.path(), entry * 3);
        let body = vec![7u8; 1000];
        for i in 0..3 {
            cache.write(&format!("u{i}"), &body).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        // Use u0 so u1 is the oldest.
        assert!(cache.read("u0").unwrap().is_some());
        cache.write("u3", &body).unwrap();
        // Evicts down to 90 % of the cap: two entries go (u1, u2).
        assert!(cache.read("u1").unwrap().is_none());
        assert!(cache.read("u2").unwrap().is_none());
        assert!(cache.read("u0").unwrap().is_some());
        assert!(cache.read("u3").unwrap().is_some());
        assert!(cache.total_bytes().unwrap() <= entry * 3);
    }

    #[test]
    fn concurrent_writers_never_expose_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = std::sync::Arc::new(DiskCache::new(dir.path(), 1 << 30));
        let bodies: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 200_000]).collect();
        std::thread::scope(|s| {
            for body in &bodies {
                let cache = cache.clone();
                s.spawn(move || {
                    for _ in 0..5 {
                        cache.write(URL, body).unwrap();
                    }
                });
            }
            for _ in 0..4 {
                let cache = cache.clone();
                s.spawn(move || {
                    for _ in 0..50 {
                        if let Some(got) = cache.read(URL).unwrap() {
                            assert_eq!(got.len(), 200_000);
                            assert!(got.iter().all(|b| *b == got[0]), "mixed writers");
                        }
                    }
                });
            }
        });
        assert_eq!(cache.read(URL).unwrap().unwrap().len(), 200_000);
        // No temp files left behind.
        let tmp = fs::read_dir(cache.root().join(TMP_DIR)).unwrap().count();
        assert_eq!(tmp, 0);
    }

    #[test]
    fn stale_temp_files_are_cleaned_on_first_use() {
        let dir = tempfile::tempdir().unwrap();
        let tmp_dir = dir.path().join(LAYOUT_DIR).join(TMP_DIR);
        fs::create_dir_all(&tmp_dir).unwrap();
        let stale = tmp_dir.join("old.tmp");
        let fresh = tmp_dir.join("fresh.tmp");
        fs::write(&stale, b"x").unwrap();
        fs::write(&fresh, b"x").unwrap();
        File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 60 * 60))
            .unwrap();
        let cache = DiskCache::new(dir.path(), 1 << 20);
        assert_eq!(cache.read(URL).unwrap(), None);
        assert!(!stale.exists());
        assert!(
            fresh.exists(),
            "another writer's in-progress file is left alone"
        );
    }
}
