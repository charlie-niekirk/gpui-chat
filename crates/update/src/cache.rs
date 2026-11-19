//! What the check remembers between runs: when it last asked, GitHub's `ETag` for the answer, the
//! tag it found, when not to ask again, and the release the user dismissed.
//!
//! One small JSON file in the cache directory. A missing, unreadable or corrupt file is the same
//! as an empty one (the check then asks again): nothing here is worth failing for.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// File name inside the cache directory.
pub const FILE_NAME: &str = "update-check.json";

/// The remembered state. Times are Unix seconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    /// When GitHub last answered (200 or 304).
    pub checked_at: Option<u64>,
    /// The `ETag` of that answer, sent back as `If-None-Match`.
    pub etag: Option<String>,
    /// The tag of the newest counting release in that answer; `None` when there was none.
    pub latest_tag: Option<String>,
    /// Do not ask before this: the end of GitHub's rate limit window.
    pub rate_limited_until: Option<u64>,
    /// Do not ask automatically before this: a short pause after a failure.
    pub retry_after: Option<u64>,
    /// The tag whose notice the user dismissed; its automatic notice stays quiet.
    pub dismissed_tag: Option<String>,
}

/// The record in `path`, or an empty one.
pub fn load(path: &Path) -> Record {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::debug!(%error, path = %path.display(), "ignoring a corrupt update-check cache");
            Record::default()
        }),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(%error, path = %path.display(), "could not read the update-check cache");
            }
            Record::default()
        }
    }
}

/// Writes the record, atomically. Failures are logged and otherwise ignored.
pub fn save(path: &Path, record: &Record) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    };
    if let Err(error) = write() {
        tracing::debug!(%error, path = %path.display(), "could not save the update-check cache");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let record = Record {
            checked_at: Some(100),
            etag: Some("\"abc\"".into()),
            latest_tag: Some("v1.0.0".into()),
            rate_limited_until: Some(500),
            retry_after: None,
            dismissed_tag: Some("v1.0.0".into()),
        };
        save(&path, &record);
        assert_eq!(load(&path), record);
    }

    #[test]
    fn missing_and_corrupt_files_read_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(load(&path), Record::default());
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&path), Record::default());
        std::fs::write(&path, r#"{"checked_at": "yesterday"}"#).unwrap();
        assert_eq!(load(&path), Record::default());
    }

    #[test]
    fn unknown_keys_are_ignored_and_missing_ones_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, r#"{"checked_at": 7, "from_the_future": true}"#).unwrap();
        let record = load(&path);
        assert_eq!(record.checked_at, Some(7));
        assert_eq!(record.etag, None);
    }

    #[test]
    fn saving_into_a_missing_directory_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b").join(FILE_NAME);
        save(&path, &Record::default());
        assert!(path.exists());
    }
}
