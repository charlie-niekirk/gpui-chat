//! Building the command line.

use std::ffi::OsString;
use std::path::PathBuf;

/// Quality used when nothing is configured.
pub const DEFAULT_QUALITY: &str = "best";

/// Qualities offered by the quality menu, best first. Twitch names them like this; a channel
/// that does not offer one makes streamlink fall back with an error that is shown in the header.
pub const QUALITIES: &[&str] = &[
    "best",
    "1080p60",
    "720p60",
    "720p",
    "480p",
    "360p",
    "160p",
    "audio_only",
    "worst",
];

/// Everything needed to start one stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Channel login (lowercase, as `ChannelId::login`).
    pub channel: String,
    /// Quality selector as streamlink understands it (`best`, `720p60`, `720p,480p`, …).
    pub quality: String,
    /// `streamlink` executable from settings; `None` searches for it.
    pub binary_override: Option<PathBuf>,
    /// Player executable or command (`--player`); `None` leaves streamlink's own default.
    pub player: Option<String>,
    /// Extra arguments, passed through before the URL, one per entry.
    pub extra_args: Vec<String>,
}

impl LaunchSpec {
    pub fn new(channel: impl Into<String>, quality: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            quality: quality.into(),
            binary_override: None,
            player: None,
            extra_args: Vec::new(),
        }
    }
}

/// The page streamlink is pointed at.
pub fn stream_url(channel: &str) -> String {
    format!("twitch.tv/{channel}")
}

/// A quality is non-empty, has no whitespace and cannot be mistaken for an option.
pub fn valid_quality(quality: &str) -> bool {
    !quality.is_empty() && !quality.starts_with('-') && !quality.chars().any(char::is_whitespace)
}

/// `[--player <player>] <extra…> twitch.tv/<channel> <quality>`: options first, then the two
/// positionals. A blank player or blank extra argument is dropped.
pub fn build_args(spec: &LaunchSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    if let Some(player) = spec
        .player
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        args.push("--player".into());
        args.push(player.into());
    }
    args.extend(
        spec.extra_args
            .iter()
            .filter(|arg| !arg.trim().is_empty())
            .map(OsString::from),
    );
    args.push(stream_url(&spec.channel).into());
    args.push(spec.quality.as_str().into());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn minimal_command_line() {
        let spec = LaunchSpec::new("forsen", "best");
        assert_eq!(strings(build_args(&spec)), ["twitch.tv/forsen", "best"]);
    }

    #[test]
    fn player_and_extra_args_come_before_the_url() {
        let mut spec = LaunchSpec::new("forsen", "720p60");
        spec.player = Some("mpv".into());
        spec.extra_args = vec!["--twitch-low-latency".into(), "--retry-open=3".into()];
        assert_eq!(
            strings(build_args(&spec)),
            [
                "--player",
                "mpv",
                "--twitch-low-latency",
                "--retry-open=3",
                "twitch.tv/forsen",
                "720p60"
            ]
        );
    }

    #[test]
    fn a_player_with_spaces_stays_one_argument() {
        let mut spec = LaunchSpec::new("a_b", "best");
        spec.player = Some("/Applications/VLC.app/Contents/MacOS/VLC".into());
        let args = strings(build_args(&spec));
        assert_eq!(args[1], "/Applications/VLC.app/Contents/MacOS/VLC");
        spec.player = Some("/Applications/My Player/mpv".into());
        assert_eq!(strings(build_args(&spec))[1], "/Applications/My Player/mpv");
    }

    #[test]
    fn blank_player_and_blank_extras_are_dropped() {
        let mut spec = LaunchSpec::new("forsen", "best");
        spec.player = Some("  ".into());
        spec.extra_args = vec![String::new(), "  ".into(), "--hls-live-edge=2".into()];
        assert_eq!(
            strings(build_args(&spec)),
            ["--hls-live-edge=2", "twitch.tv/forsen", "best"]
        );
    }

    #[test]
    fn quality_may_be_a_fallback_list() {
        let spec = LaunchSpec::new("forsen", "720p60,720p,best");
        assert_eq!(
            strings(build_args(&spec)).last().unwrap(),
            "720p60,720p,best"
        );
    }

    #[test]
    fn qualities_are_valid_selectors() {
        assert!(QUALITIES.iter().all(|q| valid_quality(q)));
        assert!(valid_quality(DEFAULT_QUALITY));
        assert!(!valid_quality(""));
        assert!(!valid_quality("--player=evil"));
        assert!(!valid_quality("best 720p"));
    }
}
