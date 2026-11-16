//! "You appear to be offline" (PR 6.3): one cause, one strip.
//!
//! When the network is down every source fails at once: the chat connection, the emote providers,
//! the badge loads, the stream poll. Each used to file its own strip. They now report here as
//! *sources* of one `network` problem, which shows while any source is failing and goes when the
//! last one recovers. The rules:
//!
//! * [`is_transport_error`] tells a transport failure (timeout, refused, DNS, nothing answered)
//!   from a service that answered badly (a 5xx, a body that does not parse). Only transport
//!   failures can mean "offline".
//! * The chat connection is the witness. While it is `Connected` a provider that times out is
//!   just that provider being down, not the network, so [`Network::failure`] declines
//!   (returns `false`) and the caller files its own strip; when it is not connected (or not yet),
//!   a transport failure is offline. The chat itself reports through [`Network::chat_outage`]
//!   once it has failed twice (a single reconnect is a blip the pane header already shows).
//! * The chat connecting drops every provider's entry: the network is evidently up, and the
//!   providers are asked again on `Connected`.
//!
//! [`Network`] is plain data (and carries the tests); the functions at the bottom wrap one in a
//! process-wide mutex so the backend runtime and the UI thread can both report.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use super::{Problem, clear_from_anywhere, report_from_anywhere};

/// The words of the strip. Every source files the same ones, so the strip reads the same whoever
/// reported last (and a dismissal sticks).
fn strip(details: Option<String>) -> Problem {
    let problem = Problem::warning("You appear to be offline, or Twitch can't be reached")
        .line("Check your internet connection. Chat, emotes and badges keep trying by themselves.");
    match details {
        Some(details) => problem.details(details),
        None => problem,
    }
}

/// Whether `error` (the text of a failed request) is a transport failure: no answer came at all.
pub fn is_transport_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "network error",
        "error sending request",
        "connection refused",
        "connection reset",
        "connection closed",
        "connect failed",
        "could not connect",
        "failed to connect",
        "dns",
        "failed to lookup",
        "no route to host",
        "network is unreachable",
        "unreachable",
        "not connected to the internet",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

/// The sources reporting a transport failure, and whether the chat connection is up.
#[derive(Debug, Default, Clone)]
pub struct Network {
    sources: BTreeMap<&'static str, String>,
    chat_up: bool,
}

impl Network {
    /// What the strip should be now: `None` for no strip.
    fn current(&self) -> Option<Problem> {
        if self.sources.is_empty() {
            return None;
        }
        let details = self
            .sources
            .iter()
            .map(|(source, error)| format!("{source}: {error}"))
            .collect::<Vec<_>>()
            .join("\n");
        Some(strip(Some(details)))
    }

    /// A provider's request failed at the transport level. `false`: the chat connection is up, so
    /// this is that provider being down, not the network; the caller reports it itself.
    pub fn failure(&mut self, source: &'static str, error: &str) -> bool {
        if self.chat_up {
            self.sources.remove(source);
            return false;
        }
        self.sources.insert(source, error.to_owned());
        true
    }

    /// The chat connection failed repeatedly (or gave up): offline, whatever the providers say.
    pub fn chat_outage(&mut self, error: &str) {
        self.chat_up = false;
        self.sources.insert("chat", error.to_owned());
    }

    /// `source` worked (or stopped failing at the transport level).
    pub fn recovered(&mut self, source: &'static str) {
        self.sources.remove(source);
    }

    /// The chat connection is up (`true`) or not (`false`). Up clears every entry: the network is
    /// evidently there.
    pub fn chat_up(&mut self, up: bool) {
        self.chat_up = up;
        if up {
            self.sources.clear();
        }
    }
}

struct Shared {
    network: Network,
    /// What was last sent to the registry (`Some(None)` = cleared), so an unchanged state sends
    /// nothing.
    sent: Option<Option<Problem>>,
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);

fn with(f: impl FnOnce(&mut Network)) {
    let mut guard = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
    let shared = guard.get_or_insert_with(|| Shared {
        network: Network::default(),
        sent: None,
    });
    f(&mut shared.network);
    let now = shared.network.current();
    if shared.sent.as_ref() == Some(&now) {
        return;
    }
    shared.sent = Some(now.clone());
    match now {
        Some(problem) => report_from_anywhere("network", problem),
        None => clear_from_anywhere("network"),
    }
}

/// [`Network::failure`] on the shared state; `false` means "file your own strip".
pub fn failure(source: &'static str, error: &str) -> bool {
    let mut filed = false;
    with(|network| filed = network.failure(source, error));
    filed
}

/// [`Network::recovered`].
pub fn recovered(source: &'static str) {
    with(|network| network.recovered(source));
}

/// [`Network::chat_outage`].
pub fn chat_outage(error: &str) {
    with(|network| network.chat_outage(error));
}

/// [`Network::chat_up`].
pub fn chat_up(up: bool) {
    with(|network| network.chat_up(up));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_failures_are_told_from_services_that_answered() {
        for error in [
            "request timed out",
            "network error: error sending request for url (https://x)",
            "connect timed out",
            "tcp connect error: Connection refused (os error 61)",
            "dns error: failed to lookup address information",
        ] {
            assert!(is_transport_error(error), "{error}");
        }
        for error in [
            "unexpected HTTP status 503",
            "malformed response: expected value at line 1",
            "Twitch answered 429.",
            "invalid Twitch user id \"x\"",
        ] {
            assert!(!is_transport_error(error), "{error}");
        }
    }

    #[test]
    fn many_failing_sources_are_one_problem_with_the_same_words() {
        let mut network = Network::default();
        assert!(network.current().is_none());
        assert!(network.failure("emotes", "request timed out"));
        let one = network.current().unwrap();
        assert!(network.failure("streams", "network error: x"));
        network.chat_outage("connect timed out");
        let three = network.current().unwrap();
        assert_eq!(one.title, three.title);
        assert_eq!(one.lines, three.lines, "so a dismissal sticks");
        let details = three.details.unwrap();
        assert!(
            details.contains("emotes:")
                && details.contains("streams:")
                && details.contains("chat:")
        );
    }

    #[test]
    fn the_strip_goes_when_the_last_source_recovers() {
        let mut network = Network::default();
        network.failure("emotes", "request timed out");
        network.chat_outage("connect timed out");
        network.recovered("emotes");
        assert!(network.current().is_some(), "the chat is still out");
        network.recovered("chat");
        assert!(network.current().is_none());
    }

    #[test]
    fn a_provider_timing_out_while_chat_is_up_is_not_an_outage() {
        let mut network = Network::default();
        network.chat_up(true);
        assert!(
            !network.failure("emotes", "request timed out"),
            "caller files its own"
        );
        assert!(network.current().is_none());
    }

    #[test]
    fn chat_connecting_clears_what_the_providers_said() {
        let mut network = Network::default();
        network.failure("emotes", "request timed out");
        network.failure("streams", "request timed out");
        network.chat_up(true);
        assert!(network.current().is_none());
        // And a fresh outage is possible again once chat is lost.
        network.chat_outage("connect timed out");
        assert!(network.current().is_some());
    }
}
