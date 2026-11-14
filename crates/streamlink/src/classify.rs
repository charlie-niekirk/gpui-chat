//! Reading streamlink's stderr.

use std::fmt;

/// Why a stream did not start or ended badly, as far as stderr says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// streamlink could not be found (see [`crate::resolve`]).
    NotFound {
        /// The path from settings that did not work, when that is what was searched.
        configured: Option<String>,
    },
    /// The channel is not live ("No playable streams found on this URL").
    ChannelOffline,
    /// The player is not installed or could not be started.
    NoPlayer(String),
    /// streamlink could not be started at all (permissions, a broken binary).
    Spawn(String),
    /// Any other `error:` line.
    Other(String),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { configured: None } => {
                f.write_str("streamlink not found — brew install streamlink")
            }
            Self::NotFound {
                configured: Some(path),
            } => write!(f, "streamlink not found at {path} (set in settings)"),
            Self::ChannelOffline => f.write_str("Channel is offline"),
            Self::NoPlayer(detail) => write!(f, "No player found: {detail}"),
            Self::Spawn(detail) => write!(f, "Could not start streamlink: {detail}"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

/// Classifies one stderr line. Only `error:` lines (streamlink's `[cli][error]`-style output
/// also appears as `error: …`) count; progress and info lines return `None`.
pub fn classify_line(line: &str) -> Option<StreamError> {
    let line = line.trim();
    let message = strip_error_prefix(line)?;
    let lower = message.to_ascii_lowercase();
    if lower.contains("no playable streams")
        || lower.contains("currently offline")
        || lower.contains("channel is offline")
        || lower.contains("no streams found")
    {
        return Some(StreamError::ChannelOffline);
    }
    if lower.contains("player")
        && (lower.contains("does not seem to be installed")
            || lower.contains("failed to start player")
            || lower.contains("unable to find player")
            || lower.contains("could not find player")
            || lower.contains("player not found")
            || lower.contains("no such file or directory"))
    {
        return Some(StreamError::NoPlayer(message.to_owned()));
    }
    Some(StreamError::Other(message.to_owned()))
}

/// `error: msg` and `[cli][error] msg` → `msg`.
fn strip_error_prefix(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("error:") {
        return Some(rest.trim_start());
    }
    let tagged = line.split_once("[error]")?;
    // Only a leading `[tag][error]` / `[error]`, not the word inside a message.
    if tagged.0.matches('[').count() == tagged.0.matches(']').count() && !tagged.0.contains(' ') {
        return Some(tagged.1.trim_start());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_channel() {
        let line = "error: No playable streams found on this URL: https://twitch.tv/nobody";
        assert_eq!(classify_line(line), Some(StreamError::ChannelOffline));
        assert_eq!(
            classify_line("[cli][error] Channel is offline"),
            Some(StreamError::ChannelOffline)
        );
    }

    #[test]
    fn missing_player() {
        let line = "error: The default player (VLC) does not seem to be installed. \
                    You must specify the path to a player executable with --player.";
        assert!(matches!(
            classify_line(line),
            Some(StreamError::NoPlayer(_))
        ));
        let line = "error: Failed to start player: mpv (No such file or directory)";
        assert!(matches!(
            classify_line(line),
            Some(StreamError::NoPlayer(_))
        ));
    }

    #[test]
    fn other_errors_keep_their_message() {
        assert_eq!(
            classify_line("error: Unable to open URL: https://x (403 Client Error)"),
            Some(StreamError::Other(
                "Unable to open URL: https://x (403 Client Error)".into()
            ))
        );
        assert_eq!(
            classify_line("[cli][error] Boom"),
            Some(StreamError::Other("Boom".into()))
        );
    }

    #[test]
    fn info_lines_are_not_errors() {
        assert_eq!(
            classify_line("[cli][info] Found matching plugin twitch"),
            None
        );
        assert_eq!(
            classify_line("[cli][info] Opening stream: 720p60 (hls)"),
            None
        );
        assert_eq!(
            classify_line("[plugin.twitch][info] some [error] word"),
            None
        );
        assert_eq!(classify_line(""), None);
    }

    #[test]
    fn display_hint_for_missing_binary() {
        assert_eq!(
            StreamError::NotFound { configured: None }.to_string(),
            "streamlink not found — brew install streamlink"
        );
        assert!(
            StreamError::NotFound {
                configured: Some("/x/streamlink".into())
            }
            .to_string()
            .contains("/x/streamlink")
        );
    }
}
