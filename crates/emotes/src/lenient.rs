//! Tolerant JSON helpers: one bad entry must not reject a whole emote set.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// `#[serde(default, deserialize_with = "null_default")]`: `null` and a missing field both
/// give `T::default()`.
pub(crate) fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Deserialises every entry on its own; entries that do not fit `T` are skipped with a
/// warning (`what` names the list, e.g. `"7TV channel emote"`).
pub(crate) fn parse_entries<T: DeserializeOwned>(what: &str, values: Vec<Value>) -> Vec<T> {
    values
        .into_iter()
        .enumerate()
        .filter_map(|(index, value)| match serde_json::from_value(value) {
            Ok(entry) => Some(entry),
            Err(error) => {
                tracing::warn!(what, index, %error, "skipping malformed emote entry");
                None
            }
        })
        .collect()
}
