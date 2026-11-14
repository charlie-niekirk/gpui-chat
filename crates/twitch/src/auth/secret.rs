//! A string that must never be logged: access, refresh and device tokens.

use serde::{Deserialize, Serialize};

/// A token. `Debug` prints `[redacted]`, there is no `Display`, and the value is only reachable
/// through [`Secret::expose`], so it cannot end up in a log line by accident.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value, for a request header or form field, or the token store.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}
