//! Identifier newtypes. All are cheap to clone (`Arc<str>`).

use std::borrow::Borrow;
use std::fmt;
use std::sync::Arc;

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Arc<str>);

        impl $name {
            /// The identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:?})"), &*self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        // `Borrow<str>` lets hash maps keyed by this type be queried with `&str`.
        // Sound because the derived `Hash`/`Eq` delegate to `Arc<str>`, which
        // hashes and compares exactly like `str`.
        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(Arc::from(value))
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(Arc::from(value))
            }
        }
    };
}

string_id! {
    /// Unique id of a message (the IRC `id` tag, a UUID on Twitch). Synthetic
    /// messages (notices, system lines) get ids minted by whoever creates them.
    MessageId
}

string_id! {
    /// Provider-specific emote id (Twitch numeric id, BTTV hash, 7TV id, ...).
    EmoteId
}

/// A chat channel, identified by its lowercase login without the leading `#`.
///
/// The Twitch numeric room id is not part of the identity: it only becomes known
/// after `ROOMSTATE` (see [`crate::RoomState::room_id`]).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(Arc<str>);

impl ChannelId {
    /// Normalises `login`: strips a leading `#` and lowercases.
    pub fn new(login: &str) -> Self {
        let login = login.trim().trim_start_matches('#');
        Self(Arc::from(login.to_ascii_lowercase()))
    }

    /// The lowercase login, without `#`.
    pub fn login(&self) -> &str {
        &self.0
    }

    /// The IRC channel name, `#login`.
    pub fn irc_name(&self) -> String {
        format!("#{}", self.0)
    }
}

impl fmt::Debug for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChannelId({:?})", &*self.0)
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for ChannelId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ChannelId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn channel_id_is_normalised() {
        assert_eq!(ChannelId::new("#Forsen"), ChannelId::new("forsen"));
        assert_eq!(ChannelId::new(" #Forsen ").login(), "forsen");
        assert_eq!(ChannelId::new("forsen").irc_name(), "#forsen");
    }

    #[test]
    fn ids_can_be_looked_up_by_str() {
        let mut map: HashMap<MessageId, u32> = HashMap::new();
        map.insert(MessageId::from("abc"), 7);
        assert_eq!(map.get("abc"), Some(&7));
        assert_eq!(map.get("nope"), None);
    }
}
