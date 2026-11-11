//! Domain types, message store and tokenizer. No I/O.
//!
//! Stub crate: the domain model arrives in PR 1.1 (see `PLAN.md`). For now this
//! crate holds the application identity constants so that every crate (and the
//! eventual rename PR) has exactly one place to look.

/// Name of this crate, used by the placeholder test below.
pub const CRATE_NAME: &str = "gpui-chat-core";

/// Human-facing application name. Working title; renaming the app starts here.
pub const APP_NAME: &str = "gpui-chat";

/// Reverse-DNS application identifier (bundle ID, keychain service, config dir
/// namespace). Renaming the app starts here.
pub const APP_ID: &str = "dev.gpui-chat.app";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_matches_package() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }

    #[test]
    fn app_identity_is_set() {
        assert!(!APP_NAME.is_empty());
        assert!(APP_ID.ends_with(".app"));
        assert!(APP_ID.contains('.'));
    }
}
