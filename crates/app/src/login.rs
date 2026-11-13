//! Turns what a user types into the "add channel" dialog into a channel login.
//!
//! Accepted forms (case-insensitive, surrounding whitespace ignored):
//!
//! * `name`, `#name`, `@name`
//! * `twitch.tv/name`, `www.twitch.tv/name`, `m.twitch.tv/name`, `go.twitch.tv/name`, with or
//!   without `http(s)://`, a trailing `/`, a query string or a fragment
//! * `twitch.tv/popout/name/chat`, `twitch.tv/moderator/name` and `twitch.tv/embed/name/chat`
//!   (the URLs Twitch itself hands out for the chat window)
//!
//! A login is 3–25 characters of `a-z`, `0-9` and `_`. Twitch's own rule is 4–25, but some old
//! accounts are shorter, so the lower bound only catches obvious typos.

use gpui_chat_core::ChannelId;

/// Shortest login accepted.
pub const MIN_LOGIN_LEN: usize = 3;
/// Longest login Twitch allows.
pub const MAX_LOGIN_LEN: usize = 25;

/// Why input was not accepted as a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginError {
    Empty,
    NotAChannelUrl,
    Length,
    Characters,
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("Enter a channel name"),
            Self::NotAChannelUrl => f.write_str("That URL is not a twitch.tv channel"),
            Self::Length => write!(
                f,
                "Channel names are {MIN_LOGIN_LEN}–{MAX_LOGIN_LEN} characters"
            ),
            Self::Characters => {
                f.write_str("Channel names use only letters, numbers and underscores")
            }
        }
    }
}

impl std::error::Error for LoginError {}

/// Hosts whose first path segment is a channel. Other `*.twitch.tv` hosts (`clips.`, `dev.`,
/// `help.`, ...) are different sites.
const CHANNEL_HOSTS: [&str; 4] = ["twitch.tv", "www.twitch.tv", "m.twitch.tv", "go.twitch.tv"];

/// First path segments of twitch.tv that are pages, not channels.
const RESERVED_SEGMENTS: [&str; 15] = [
    "directory",
    "videos",
    "settings",
    "search",
    "downloads",
    "p",
    "jobs",
    "subscriptions",
    "inventory",
    "wallet",
    "drops",
    "turbo",
    "prime",
    "friends",
    "messages",
];

/// Path segments of twitch.tv URLs that come before the channel name.
const PATH_PREFIXES: [&str; 3] = ["popout", "moderator", "embed"];

/// Normalises `input` to a channel, or says why it is not one.
pub fn normalise_login(input: &str) -> Result<ChannelId, LoginError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(LoginError::Empty);
    }
    let name = if input.contains('/') || input.contains('.') {
        name_from_url(input)?
    } else {
        input
    };
    let name = name.trim_start_matches(['#', '@']);
    if !(MIN_LOGIN_LEN..=MAX_LOGIN_LEN).contains(&name.chars().count()) {
        return Err(if name.is_empty() {
            LoginError::Empty
        } else {
            LoginError::Length
        });
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(LoginError::Characters);
    }
    Ok(ChannelId::new(name))
}

/// The channel segment of a `twitch.tv` URL (scheme optional).
fn name_from_url(input: &str) -> Result<&str, LoginError> {
    let without_scheme = input.split_once("://").map_or(input, |(_, rest)| rest);
    // Query and fragment are never part of the name.
    let without_query = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let mut segments = without_query.split('/').filter(|s| !s.is_empty());
    let host = segments.next().ok_or(LoginError::Empty)?;
    // `user@host` and `host:port` forms do not occur for twitch.tv links.
    let host = host.to_ascii_lowercase();
    if !CHANNEL_HOSTS.contains(&host.as_str()) {
        return Err(LoginError::NotAChannelUrl);
    }
    let mut segments = segments.peekable();
    if segments
        .peek()
        .is_some_and(|s| PATH_PREFIXES.contains(&s.to_ascii_lowercase().as_str()))
    {
        segments.next();
    }
    let name = segments.next().ok_or(LoginError::NotAChannelUrl)?;
    if RESERVED_SEGMENTS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(LoginError::NotAChannelUrl);
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(input: &str) -> String {
        normalise_login(input)
            .unwrap_or_else(|error| panic!("{input:?} rejected: {error}"))
            .login()
            .to_owned()
    }

    #[test]
    fn plain_names_are_lowercased_and_trimmed() {
        assert_eq!(ok("Forsen"), "forsen");
        assert_eq!(ok("  xQc  "), "xqc");
        assert_eq!(ok("some_one_99"), "some_one_99");
    }

    #[test]
    fn hash_and_at_prefixes_are_stripped() {
        assert_eq!(ok("#Forsen"), "forsen");
        assert_eq!(ok("@Forsen"), "forsen");
        assert_eq!(ok("##forsen"), "forsen");
    }

    #[test]
    fn twitch_urls_yield_the_channel() {
        for url in [
            "twitch.tv/Forsen",
            "www.twitch.tv/forsen",
            "m.twitch.tv/forsen",
            "https://twitch.tv/forsen",
            "http://www.twitch.tv/forsen/",
            "https://www.twitch.tv/FORSEN?sr=a&foo=bar",
            "https://www.twitch.tv/forsen#chat",
            "TWITCH.TV/forsen",
            "https://www.twitch.tv/popout/forsen/chat",
            "https://www.twitch.tv/popout/forsen/chat?popout=",
            "https://www.twitch.tv/moderator/forsen",
            "https://www.twitch.tv/embed/forsen/chat",
            "https://www.twitch.tv/forsen/videos",
            "https://go.twitch.tv/forsen",
            "https://m.twitch.tv/Forsen/",
        ] {
            assert_eq!(ok(url), "forsen", "{url}");
        }
    }

    #[test]
    fn other_hosts_and_bare_urls_are_rejected() {
        for url in [
            "https://example.com/forsen",
            "https://nottwitch.tv/forsen",
            "twitch.tv.evil.com/forsen",
            "https://clips.twitch.tv/SomeClipSlug",
            "https://dev.twitch.tv/forsen",
            "https://help.twitch.tv/forsen",
            "https://twitch.tv/directory",
            "https://www.twitch.tv/directory/game/Just%20Chatting",
            "twitch.tv/videos/123456",
            "https://www.twitch.tv/settings",
            "twitch.tv/Search?term=x",
            "https://www.twitch.tv/popout/directory/chat",
            "twitch.tv/p/en/about",
            "https://www.twitch.tv/",
            "twitch.tv",
            "https://www.twitch.tv/popout",
        ] {
            assert!(
                matches!(
                    normalise_login(url),
                    Err(LoginError::NotAChannelUrl | LoginError::Empty)
                ),
                "{url} -> {:?}",
                normalise_login(url)
            );
        }
    }

    #[test]
    fn empty_and_invalid_names_are_rejected() {
        assert_eq!(normalise_login(""), Err(LoginError::Empty));
        assert_eq!(normalise_login("   "), Err(LoginError::Empty));
        assert_eq!(normalise_login("#"), Err(LoginError::Empty));
        assert_eq!(normalise_login("ab"), Err(LoginError::Length));
        assert_eq!(
            normalise_login(&"a".repeat(MAX_LOGIN_LEN + 1)),
            Err(LoginError::Length)
        );
        assert!(normalise_login(&"a".repeat(MAX_LOGIN_LEN)).is_ok());
        assert_eq!(normalise_login("two words"), Err(LoginError::Characters));
        assert_eq!(normalise_login("na-me"), Err(LoginError::Characters));
        assert_eq!(normalise_login("nämé"), Err(LoginError::Characters));
        assert_eq!(normalise_login("#na me"), Err(LoginError::Characters));
    }
}
