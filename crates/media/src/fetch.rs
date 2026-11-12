//! Downloading image bytes: the [`Fetcher`] seam, the real HTTP implementation and the retry
//! policy applied around it by [`crate::ImageCache`].

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use gpui_chat_core::APP_NAME;

use crate::error::{BuildError, FetchError};

/// A boxed fetch future (object-safe trait methods cannot be `async fn`).
pub type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, FetchError>> + Send + 'a>>;

/// Downloads one URL. One call is one attempt: retries, timeouts, concurrency limits and
/// de-duplication are applied by [`crate::ImageCache`]. Tests substitute a fake.
pub trait Fetcher: Send + Sync + 'static {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a>;
}

/// `User-Agent` sent with every request: `<APP_NAME>/<version>`.
pub fn user_agent() -> String {
    format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION"))
}

/// HTTP settings for [`HttpFetcher`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpConfig {
    /// Whole-request timeout (connect + headers + body).
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    /// Bodies larger than this are abandoned with [`FetchError::TooLarge`].
    pub max_body_bytes: u64,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(15),
            connect_timeout: Duration::from_secs(5),
            max_body_bytes: 8 * 1024 * 1024,
        }
    }
}

/// `reqwest` (rustls) implementation of [`Fetcher`].
pub struct HttpFetcher {
    client: reqwest::Client,
    max_body_bytes: u64,
}

impl HttpFetcher {
    pub fn new(config: HttpConfig) -> Result<Self, BuildError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent())
            .timeout(config.request_timeout)
            .connect_timeout(config.connect_timeout)
            .build()
            .map_err(|e| BuildError::Http(e.to_string()))?;
        Ok(Self {
            client,
            max_body_bytes: config.max_body_bytes,
        })
    }

    async fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        let mut response = self.client.get(url).send().await.map_err(map_err)?;
        let status = response.status().as_u16();
        match status {
            200..=299 => {}
            404 | 410 => return Err(FetchError::NotFound(status)),
            _ => return Err(FetchError::Status(status)),
        }
        let limit = self.max_body_bytes;
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(FetchError::TooLarge { limit });
        }
        let mut body = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
        while let Some(chunk) = response.chunk().await.map_err(map_err)? {
            if body.len() as u64 + chunk.len() as u64 > limit {
                return Err(FetchError::TooLarge { limit });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

impl Fetcher for HttpFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
        Box::pin(self.get(url))
    }
}

fn map_err(err: reqwest::Error) -> FetchError {
    if err.is_timeout() {
        FetchError::Timeout
    } else if let Some(status) = err.status() {
        FetchError::Status(status.as_u16())
    } else if err.is_builder() || err.is_redirect() || err.is_decode() {
        // Retrying cannot fix an invalid URL, a redirect loop or a bad content encoding.
        FetchError::Request(err.to_string().into())
    } else {
        // Connect, send and body errors: the connection failed and may work next time.
        FetchError::Network(err.to_string().into())
    }
}

/// Exponential backoff for transient failures ([`FetchError::is_transient`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts including the first (1 = no retries).
    pub max_attempts: u32,
    /// Delay before the first retry; doubles per retry.
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(4),
        }
    }
}

impl RetryPolicy {
    /// Delay after failed attempt number `attempt` (1-based).
    pub fn delay_after(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        self.base_delay
            .saturating_mul(1u32 << shift)
            .min(self.max_delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn transient_classification() {
        assert!(FetchError::Timeout.is_transient());
        assert!(FetchError::Network("reset".into()).is_transient());
        assert!(FetchError::Status(503).is_transient());
        assert!(FetchError::Status(429).is_transient());
        assert!(!FetchError::Status(403).is_transient());
        assert!(!FetchError::NotFound(404).is_transient());
        assert!(!FetchError::TooLarge { limit: 1 }.is_transient());
        assert!(!FetchError::Request("bad url".into()).is_transient());
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(500),
        };
        assert_eq!(p.delay_after(1), Duration::from_millis(100));
        assert_eq!(p.delay_after(2), Duration::from_millis(200));
        assert_eq!(p.delay_after(3), Duration::from_millis(400));
        assert_eq!(p.delay_after(4), Duration::from_millis(500));
        assert_eq!(p.delay_after(40), Duration::from_millis(500));
    }

    #[test]
    fn user_agent_names_the_app() {
        assert_eq!(
            user_agent(),
            format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    /// A minimal HTTP/1.1 server on localhost: one response per connection.
    async fn serve(seen_agents: Arc<Mutex<Vec<String>>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen_agents.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let mut len = 0;
                    while !buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = sock.read(&mut buf[len..]).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        len += n;
                    }
                    let req = String::from_utf8_lossy(&buf[..len]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    if let Some(ua) = req
                        .lines()
                        .find_map(|l| l.strip_prefix("user-agent: "))
                        .or_else(|| req.lines().find_map(|l| l.strip_prefix("User-Agent: ")))
                    {
                        seen.lock().unwrap().push(ua.to_owned());
                    }
                    let (status, body): (&str, Vec<u8>) = match path.as_str() {
                        "/ok" => ("200 OK", b"image-bytes".to_vec()),
                        "/missing" => ("404 Not Found", Vec::new()),
                        "/gone" => ("410 Gone", Vec::new()),
                        "/error" => ("503 Service Unavailable", Vec::new()),
                        "/big" => ("200 OK", vec![0u8; 64 * 1024]),
                        "/slow" => {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            ("200 OK", Vec::new())
                        }
                        _ => ("400 Bad Request", Vec::new()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn http_fetcher_against_local_server() {
        let agents = Arc::new(Mutex::new(Vec::new()));
        let base = serve(agents.clone()).await;
        let fetcher = HttpFetcher::new(HttpConfig {
            request_timeout: Duration::from_millis(500),
            connect_timeout: Duration::from_millis(500),
            max_body_bytes: 1024,
        })
        .unwrap();
        let get = |p: &str| format!("{base}{p}");
        assert_eq!(
            fetcher.fetch(&get("/ok")).await.unwrap(),
            b"image-bytes".to_vec()
        );
        assert_eq!(
            fetcher.fetch(&get("/missing")).await,
            Err(FetchError::NotFound(404))
        );
        assert_eq!(
            fetcher.fetch(&get("/gone")).await,
            Err(FetchError::NotFound(410))
        );
        assert_eq!(
            fetcher.fetch(&get("/error")).await,
            Err(FetchError::Status(503))
        );
        assert_eq!(
            fetcher.fetch(&get("/big")).await,
            Err(FetchError::TooLarge { limit: 1024 })
        );
        assert_eq!(fetcher.fetch(&get("/slow")).await, Err(FetchError::Timeout));
        // Not a URL: a permanent request error, not retried.
        let invalid = fetcher.fetch("not a url").await.unwrap_err();
        assert!(matches!(invalid, FetchError::Request(_)), "{invalid:?}");
        assert!(!invalid.is_transient());
        // Nothing listens on port 9 of localhost: a (transient) network error.
        assert!(matches!(
            fetcher.fetch("http://127.0.0.1:9/x").await,
            Err(FetchError::Network(_))
        ));
        let agents = agents.lock().unwrap();
        assert!(!agents.is_empty());
        assert!(agents.iter().all(|ua| *ua == user_agent()), "{agents:?}");
    }
}
