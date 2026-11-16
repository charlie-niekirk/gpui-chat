//! Where the app's log goes (PR 5.3): `tracing` events *and* the `log` records the libraries emit.
//!
//! GPUI reports through the `log` crate (that notifications are disabled because the binary is not
//! inside an app bundle, that the user denied the authorisation, a failed delivery), and until now
//! nothing listened to either facade. [`init`] installs one subscriber on stderr; with
//! `tracing-subscriber`'s `tracing-log` feature (a default one) installing it also routes `log`
//! records into `tracing`, so both show up here. `RUST_LOG` overrides the default filter.

use tracing_subscriber::EnvFilter;

/// Warnings from everything; the app's own crates and GPUI's macOS platform layer (which says why a
/// notification could not be shown) at info.
const DEFAULT_FILTER: &str = "warn,gpui_chat=info,gpui_macos=info";

/// Installs the subscriber. Does nothing if one is installed already.
pub fn init() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_default_filter_parses() {
        let _ = tracing_subscriber::EnvFilter::new(super::DEFAULT_FILTER);
    }
}
