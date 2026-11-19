//! The update check (PR 7.2): is a newer release published on GitHub? No GPUI, no UI.
//!
//! [`UpdateChecker`] compares the running version with the newest **published, non-draft,
//! non-prerelease** release of this repository whose tag is semver (`v0.1.0`). It never downloads or
//! installs anything: the app shows [`Outcome::UpdateAvailable`] as a notice with a link to
//! [`Release::url`].
//!
//! * [`release`]: reads GitHub's "list releases" response (drafts, prereleases and tags that are not
//!   semver are skipped; a response that is not a list is an error).
//! * [`UpdateChecker`] (in `checker`): the request, politely: at most once a day, `ETag` / `304`,
//!   rate limits honoured, a timeout and a size cap; it remembers its state in a small file in the
//!   cache directory (`cache`).
//! * [`run_automatic`]: the background task. It asks only while the "check automatically" setting
//!   is on; with it off it makes no request at all.
//!
//! Tests run against recorded JSON in `fixtures/updates/` served by a local mock server, never
//! the live API.

mod auto;
mod cache;
mod checker;
pub mod release;

pub use auto::run_automatic;
pub use cache::FILE_NAME as CACHE_FILE_NAME;
pub use checker::{
    API_BASE_ENV, CHECK_INTERVAL, CheckError, DEFAULT_API_BASE, FAILURE_PAUSE, OVERRIDE_CACHE_FILE,
    Outcome, REQUEST_TIMEOUT, UpdateChecker, repo_from_url,
};
pub use release::Release;
pub use semver::Version;

#[cfg(test)]
mod tests;
