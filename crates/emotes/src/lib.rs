//! Emote providers (Twitch, BTTV, FFZ, 7TV), emote sets and resolution.
//!
//! Stub crate: real functionality arrives in later PRs (see `PLAN.md`).

/// Name of this crate, used by the placeholder test below.
pub const CRATE_NAME: &str = "gpui-chat-emotes";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_matches_package() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }
}
