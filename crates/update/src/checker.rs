//! The check itself: one request to GitHub's "list releases" endpoint, made politely.
//!
//! * **At most once a day** ([`UpdateChecker::check_if_due`]): within [`CHECK_INTERVAL`] of the last
//!   answer nothing is requested and the remembered answer is used. After a failure the automatic
//!   check waits [`FAILURE_PAUSE`] before asking again.
//! * **Conditional**: the `ETag` of the last answer is sent as `If-None-Match`; a `304` costs no body.
//! * **Rate limits**: a `403` / `429` that says the limit is used up (or carries `Retry-After`) is
//!   remembered until the time GitHub names (at most a day), and neither the automatic nor the
//!   manual check asks before then.
//! * **Bounded**: a request timeout, and a cap on the size of the body read.
//!
//! Nothing is downloaded or installed: the result is only an [`Outcome`] the app turns into a notice.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_chat_core::APP_NAME;
use reqwest::header::{ACCEPT, ETAG, IF_NONE_MATCH};
use semver::Version;

use crate::cache::{self, Record};
use crate::release::{self, Release};

/// Automatic checks ask GitHub at most this often (seconds).
pub const CHECK_INTERVAL: u64 = 24 * 60 * 60;
/// After a failed request the automatic check waits this long (seconds).
pub const FAILURE_PAUSE: u64 = 60 * 60;
/// The longest a rate limit is honoured (seconds), whatever GitHub says.
const MAX_RATE_WAIT: u64 = 24 * 60 * 60;
/// The shortest wait between two automatic checks (seconds): a guard against a busy loop.
const MIN_WAIT: u64 = 60;
/// The most of a response that is read; a list of releases is a few tens of kilobytes.
const MAX_BODY: usize = 2 * 1024 * 1024;
/// How many releases one request asks for.
const PER_PAGE: u32 = 20;

/// The GitHub REST API.
pub const DEFAULT_API_BASE: &str = "https://api.github.com";
/// Environment variable that points the check at another server speaking the same API. It is for
/// trying the notice out against a local copy of a fixture; nothing else reads it.
pub const API_BASE_ENV: &str = "GPUI_CHAT_UPDATE_API";
/// How long a request may take in all.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// What the check found, compared with the running version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A published release is newer than the running version.
    UpdateAvailable(Release),
    /// The newest published release is this version or older.
    UpToDate { latest: Version },
    /// There is no usable release: none published yet, or only drafts, prereleases and tags that
    /// are not semver.
    NoRelease,
}

/// Why a check gave no answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    #[error("could not reach GitHub: {0}")]
    Network(String),
    #[error("GitHub answered with status {0}")]
    Status(u16),
    #[error("GitHub's answer could not be read: {0}")]
    Malformed(String),
    #[error("GitHub's answer was larger than expected")]
    TooLarge,
    /// GitHub's rate limit is used up until `until` (Unix seconds).
    #[error("GitHub's rate limit is used up for now")]
    RateLimited { until: u64 },
    /// The automatic check is pausing after a failure until `until` (Unix seconds).
    #[error("pausing after a failed check")]
    BackingOff { until: u64 },
    #[error("{0:?} is not a version")]
    BadVersion(String),
    #[error("could not set up the HTTP client: {0}")]
    Client(String),
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Checks the GitHub Releases of one repository against one running version.
pub struct UpdateChecker {
    client: reqwest::Client,
    api_base: String,
    /// `owner/name`.
    repo: String,
    current: Version,
    cache_file: Option<PathBuf>,
    /// The truth while the app runs; the file only carries it across runs.
    state: Mutex<Record>,
    clock: Clock,
}

impl UpdateChecker {
    /// A checker for the running version `current` (`CARGO_PKG_VERSION`) of this repository
    /// (`CARGO_PKG_REPOSITORY`), remembering its state in `cache_file` when given. The API base is
    /// [`DEFAULT_API_BASE`] unless [`API_BASE_ENV`] is set.
    pub fn new(current: &str, cache_file: Option<PathBuf>) -> Result<Self, CheckError> {
        let current =
            Version::parse(current).map_err(|_| CheckError::BadVersion(current.to_owned()))?;
        let api_base = std::env::var(API_BASE_ENV)
            .ok()
            .filter(|base| !base.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API_BASE.to_owned());
        let record = cache_file.as_deref().map(cache::load).unwrap_or_default();
        Ok(Self {
            client: client(REQUEST_TIMEOUT)?,
            api_base: api_base.trim_end_matches('/').to_owned(),
            repo: repo_from_url(env!("CARGO_PKG_REPOSITORY")).to_owned(),
            current,
            cache_file,
            state: Mutex::new(record),
            clock: Arc::new(unix_now),
        })
    }

    /// Asks `base` (no trailing `/`) instead of the GitHub API.
    #[must_use]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_owned();
        self
    }

    /// Looks at `owner/name` instead of this repository.
    #[must_use]
    pub fn with_repo(mut self, repo: impl Into<String>) -> Self {
        self.repo = repo.into();
        self
    }

    /// Gives a request `timeout` in all instead of [`REQUEST_TIMEOUT`].
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, CheckError> {
        self.client = client(timeout)?;
        Ok(self)
    }

    /// Reads "now" (Unix seconds) from `clock`, for tests.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// The version this checker compares against.
    pub fn current(&self) -> &Version {
        &self.current
    }

    /// The automatic check: asks GitHub only when the last answer is older than
    /// [`CHECK_INTERVAL`] and no rate limit or failure pause is running, else answers from what it
    /// remembers (an error if it remembers nothing and may not ask).
    pub async fn check_if_due(&self) -> Result<Outcome, CheckError> {
        let now = self.now();
        let record = self.snapshot();
        let known = record.checked_at.is_some();
        let remembered = || self.evaluate(record.latest_tag.as_deref());
        if let Some(until) = record.rate_limited_until.filter(|until| *until > now) {
            return if known {
                Ok(remembered())
            } else {
                Err(CheckError::RateLimited { until })
            };
        }
        if let Some(until) = record.retry_after.filter(|until| *until > now) {
            return if known {
                Ok(remembered())
            } else {
                Err(CheckError::BackingOff { until })
            };
        }
        if record
            .checked_at
            .is_some_and(|checked| now.abs_diff(checked) < CHECK_INTERVAL)
        {
            return Ok(remembered());
        }
        self.fetch(now).await
    }

    /// The manual check ("Check for Updates…"): asks GitHub whatever the last answer's age, but
    /// not while a rate limit runs.
    pub async fn check_now(&self) -> Result<Outcome, CheckError> {
        let now = self.now();
        if let Some(until) = self
            .snapshot()
            .rate_limited_until
            .filter(|until| *until > now)
        {
            return Err(CheckError::RateLimited { until });
        }
        self.fetch(now).await
    }

    /// How long until the automatic check may ask again (at least a minute, at most a day).
    pub fn next_check_in(&self) -> Duration {
        let now = self.now();
        let record = self.snapshot();
        let mut due = record
            .checked_at
            .map_or(now, |checked| checked.saturating_add(CHECK_INTERVAL));
        due = due
            .max(record.rate_limited_until.unwrap_or(0))
            .max(record.retry_after.unwrap_or(0));
        Duration::from_secs(due.saturating_sub(now).clamp(MIN_WAIT, CHECK_INTERVAL))
    }

    /// The user dismissed the notice for `release`: [`is_dismissed`](Self::is_dismissed) says so
    /// from now on, across runs.
    pub fn dismiss(&self, release: &Release) {
        self.update(|record| record.dismissed_tag = Some(release.tag.clone()));
    }

    /// Whether the notice for `release` was dismissed.
    pub fn is_dismissed(&self, release: &Release) -> bool {
        self.snapshot().dismissed_tag.as_deref() == Some(release.tag.as_str())
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn snapshot(&self) -> Record {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Changes the remembered state and writes it out.
    fn update(&self, change: impl FnOnce(&mut Record)) {
        let record = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            change(&mut state);
            state.clone()
        };
        if let Some(path) = &self.cache_file {
            cache::save(path, &record);
        }
    }

    /// The outcome for the newest counting tag (`None`: there is none).
    fn evaluate(&self, latest_tag: Option<&str>) -> Outcome {
        match latest_tag.and_then(|tag| release::release_for_tag(&self.repo, tag)) {
            None => Outcome::NoRelease,
            Some(release) if release.version.cmp_precedence(&self.current).is_gt() => {
                Outcome::UpdateAvailable(release)
            }
            Some(release) => Outcome::UpToDate {
                latest: release.version,
            },
        }
    }

    /// One request; a failure that is not GitHub's rate limit starts the failure pause.
    async fn fetch(&self, now: u64) -> Result<Outcome, CheckError> {
        let result = self.request(now).await;
        if let Err(error) = &result {
            tracing::debug!(%error, "update check failed");
            if !matches!(error, CheckError::RateLimited { .. }) {
                self.update(|record| record.retry_after = Some(now.saturating_add(FAILURE_PAUSE)));
            }
        }
        result
    }

    async fn request(&self, now: u64) -> Result<Outcome, CheckError> {
        let url = format!(
            "{}/repos/{}/releases?per_page={PER_PAGE}",
            self.api_base, self.repo
        );
        let record = self.snapshot();
        let mut request = self
            .client
            .get(url)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        // Only ever send an ETag that goes with an answer we still remember.
        let etag = record
            .etag
            .as_deref()
            .filter(|_| record.checked_at.is_some());
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let mut response = request.send().await.map_err(network)?;
        let status = response.status().as_u16();
        match status {
            200 => {
                let etag = response
                    .headers()
                    .get(ETAG)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let body = read_capped(&mut response).await?;
                let latest = release::latest(&body, &self.repo)
                    .map_err(|error| CheckError::Malformed(error.0))?;
                let tag = latest.map(|release| release.tag);
                let outcome = self.evaluate(tag.as_deref());
                self.update(|record| {
                    record.checked_at = Some(now);
                    record.etag = etag;
                    record.latest_tag = tag;
                    record.rate_limited_until = None;
                    record.retry_after = None;
                });
                Ok(outcome)
            }
            304 if record.checked_at.is_some() => {
                self.update(|record| {
                    record.checked_at = Some(now);
                    record.rate_limited_until = None;
                    record.retry_after = None;
                });
                Ok(self.evaluate(record.latest_tag.as_deref()))
            }
            // A repository with no releases answers `[]`; a missing or private one answers 404,
            // which to anybody without access is the same thing.
            404 => {
                self.update(|record| {
                    record.checked_at = Some(now);
                    record.etag = None;
                    record.latest_tag = None;
                    record.rate_limited_until = None;
                    record.retry_after = None;
                });
                Ok(Outcome::NoRelease)
            }
            403 | 429 => {
                let header = |name: &str| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .map(str::trim)
                        .map(str::to_owned)
                };
                let number = |name: &str| header(name).and_then(|value| value.parse::<u64>().ok());
                let retry_after = number("retry-after");
                let exhausted = header("x-ratelimit-remaining").as_deref() == Some("0");
                if status == 429 || retry_after.is_some() || exhausted {
                    let until = retry_after
                        .map(|seconds| now.saturating_add(seconds))
                        .or_else(|| number("x-ratelimit-reset"))
                        .unwrap_or_else(|| now.saturating_add(FAILURE_PAUSE))
                        .clamp(now + MIN_WAIT, now + MAX_RATE_WAIT);
                    self.update(|record| record.rate_limited_until = Some(until));
                    Err(CheckError::RateLimited { until })
                } else {
                    Err(CheckError::Status(status))
                }
            }
            other => Err(CheckError::Status(other)),
        }
    }
}

/// `owner/name` of a GitHub URL (`https://github.com/owner/name`, with or without `.git`).
pub fn repo_from_url(url: &str) -> &str {
    let url = url.trim_end_matches('/').trim_end_matches(".git");
    url.strip_prefix("https://github.com/").unwrap_or(url)
}

fn client(timeout: Duration) -> Result<reqwest::Client, CheckError> {
    reqwest::Client::builder()
        .user_agent(format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .map_err(|error| CheckError::Client(error.without_url().to_string()))
}

fn network(error: reqwest::Error) -> CheckError {
    CheckError::Network(error.without_url().to_string())
}

/// The body of `response`, refusing more than [`MAX_BODY`] bytes.
async fn read_capped(response: &mut reqwest::Response) -> Result<Vec<u8>, CheckError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY as u64)
    {
        return Err(CheckError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(CheckError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
