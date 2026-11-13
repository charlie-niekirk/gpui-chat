//! The HTTP seam. Clients only need "GET this URL, give me status and body", which keeps
//! them testable without a network.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use gpui_chat_core::APP_NAME;
use thiserror::Error;

use crate::error::EmoteError;

/// A boxed future (object-safe trait methods cannot be `async fn`).
pub type HttpFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HttpResponse, HttpError>> + Send + 'a>>;

/// A completed HTTP exchange (any status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Transport-level failure: no complete response.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HttpError {
    #[error("request timed out")]
    Timeout,
    #[error("network error: {0}")]
    Network(String),
    #[error("response body larger than {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("invalid request: {0}")]
    Request(String),
}

/// Fetches a URL with `GET`. One call is one attempt; non-2xx statuses are returned as
/// responses, not errors.
pub trait HttpClient: Send + Sync + 'static {
    fn get<'a>(&'a self, url: &'a str) -> HttpFuture<'a>;
}

/// `User-Agent` sent with every request: `<APP_NAME>/<version>`.
pub fn user_agent() -> String {
    format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION"))
}

/// Settings for [`ReqwestHttp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpConfig {
    /// Whole-request timeout (connect + headers + body).
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    /// Bodies larger than this are abandoned with [`HttpError::TooLarge`].
    pub max_body_bytes: u64,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            // 7TV's channel document for a big channel is a few MB and takes seconds.
            request_timeout: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(5),
            max_body_bytes: 32 * 1024 * 1024,
        }
    }
}

/// `reqwest` (rustls) implementation of [`HttpClient`].
pub struct ReqwestHttp {
    client: reqwest::Client,
    max_body_bytes: u64,
}

impl ReqwestHttp {
    pub fn new() -> Result<Self, HttpError> {
        Self::with_config(HttpConfig::default())
    }

    pub fn with_config(config: HttpConfig) -> Result<Self, HttpError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent())
            .timeout(config.request_timeout)
            .connect_timeout(config.connect_timeout)
            .build()
            .map_err(|e| HttpError::Request(e.to_string()))?;
        Ok(Self {
            client,
            max_body_bytes: config.max_body_bytes,
        })
    }

    async fn fetch(&self, url: &str) -> Result<HttpResponse, HttpError> {
        let mut response = self.client.get(url).send().await.map_err(map_err)?;
        let status = response.status().as_u16();
        let limit = self.max_body_bytes;
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(HttpError::TooLarge { limit });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(map_err)? {
            if body.len() as u64 + chunk.len() as u64 > limit {
                return Err(HttpError::TooLarge { limit });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse { status, body })
    }
}

impl HttpClient for ReqwestHttp {
    fn get<'a>(&'a self, url: &'a str) -> HttpFuture<'a> {
        Box::pin(self.fetch(url))
    }
}

fn map_err(err: reqwest::Error) -> HttpError {
    if err.is_timeout() {
        HttpError::Timeout
    } else if err.is_builder() || err.is_redirect() {
        HttpError::Request(err.to_string())
    } else {
        HttpError::Network(err.to_string())
    }
}

/// GETs `url` and returns the body of a 2xx response; any other status is
/// [`EmoteError::Status`].
pub(crate) async fn get_ok(http: &dyn HttpClient, url: &str) -> Result<Vec<u8>, EmoteError> {
    let response = http.get(url).await?;
    match response.status {
        200..=299 => Ok(response.body),
        status => Err(EmoteError::Status(status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serves `response` (raw HTTP) to every connection and sends each request head
    /// through the returned channel. Waits `delay` before answering.
    async fn serve(
        response: &'static str,
        delay: Duration,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                    tokio::time::sleep(delay).await;
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn sends_user_agent_and_returns_status_and_body() {
        let (base, mut requests) = serve(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope",
            Duration::ZERO,
        )
        .await;
        let http = ReqwestHttp::new().unwrap();
        let response = http.get(&format!("{base}/x")).await.unwrap();
        assert_eq!(response.status, 503);
        assert_eq!(response.body, b"nope");
        let head = requests.recv().await.unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /x "), "{head}");
        assert!(head.contains(&format!(
            "user-agent: {}",
            user_agent().to_ascii_lowercase()
        )));
        assert!(user_agent().starts_with(&format!("{APP_NAME}/")));
    }

    #[tokio::test]
    async fn slow_server_times_out() {
        let (base, _requests) = serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Duration::from_secs(5),
        )
        .await;
        let http = ReqwestHttp::with_config(HttpConfig {
            request_timeout: Duration::from_millis(150),
            ..HttpConfig::default()
        })
        .unwrap();
        assert_eq!(http.get(&base).await.unwrap_err(), HttpError::Timeout);
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let (base, _requests) = serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            Duration::ZERO,
        )
        .await;
        let http = ReqwestHttp::with_config(HttpConfig {
            max_body_bytes: 4,
            ..HttpConfig::default()
        })
        .unwrap();
        assert_eq!(
            http.get(&base).await.unwrap_err(),
            HttpError::TooLarge { limit: 4 }
        );
    }

    #[tokio::test]
    async fn refused_connection_is_a_network_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let http = ReqwestHttp::new().unwrap();
        assert!(matches!(
            http.get(&format!("http://{addr}")).await,
            Err(HttpError::Network(_))
        ));
    }
}
