//! Test helpers: a fake [`HttpClient`] and fixture access.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::http::{HttpClient, HttpError, HttpFuture, HttpResponse};

/// Reads `fixtures/emotes/<name>` at the repo root.
pub(crate) fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/emotes")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
}

/// Reads `fixtures/badges/<name>` at the repo root.
pub(crate) fn badge_fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/badges")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
}

/// What the fake answers for a URL.
#[derive(Clone)]
pub(crate) enum Reply {
    Ok(u16, Vec<u8>),
    Err(HttpError),
    /// Never answers (to test client-side timeouts).
    Hang,
}

/// Answers from a URL → reply map; unknown URLs are a network error. Records requests.
#[derive(Default)]
pub(crate) struct FakeHttp {
    replies: HashMap<String, Reply>,
    pub(crate) requests: Mutex<Vec<String>>,
}

impl FakeHttp {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn json(mut self, url: &str, fixture_name: &str) -> Self {
        self.replies
            .insert(url.to_owned(), Reply::Ok(200, fixture(fixture_name)));
        self
    }

    pub(crate) fn body(mut self, url: &str, status: u16, body: Vec<u8>) -> Self {
        self.replies.insert(url.to_owned(), Reply::Ok(status, body));
        self
    }

    pub(crate) fn status(mut self, url: &str, status: u16, body: &str) -> Self {
        self.replies
            .insert(url.to_owned(), Reply::Ok(status, body.as_bytes().to_vec()));
        self
    }

    pub(crate) fn fail(mut self, url: &str, err: HttpError) -> Self {
        self.replies.insert(url.to_owned(), Reply::Err(err));
        self
    }

    pub(crate) fn hang(mut self, url: &str) -> Self {
        self.replies.insert(url.to_owned(), Reply::Hang);
        self
    }

    pub(crate) fn requested(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl HttpClient for FakeHttp {
    fn get<'a>(&'a self, url: &'a str) -> HttpFuture<'a> {
        self.requests.lock().unwrap().push(url.to_owned());
        let reply = self.replies.get(url).cloned();
        Box::pin(async move {
            match reply {
                Some(Reply::Ok(status, body)) => Ok(HttpResponse { status, body }),
                Some(Reply::Err(e)) => Err(e),
                Some(Reply::Hang) => std::future::pending().await,
                None => Err(HttpError::Network(format!("no fake reply for {url}"))),
            }
        })
    }
}
