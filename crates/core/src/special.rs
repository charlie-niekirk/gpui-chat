//! What a *special* message is (PR 5.1): the `USERNOTICE` variants (subs, gifts, raids,
//! announcements, ...) and the extras a `PRIVMSG` can carry (channel-point rewards, shared chat).
//! Plain data filled in by the IRC parser; the app turns it into rows. No I/O.
//!
//! Everything here is *also* in [`crate::Message::raw_tags`]; these types exist so the UI does
//! not parse tags and so a tag Twitch changes is one parser edit.

use crate::emote::BadgeId;
use crate::ids::MessageId;

/// The plan of a subscription (`msg-param-sub-plan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubPlan {
    Prime,
    Tier1,
    Tier2,
    Tier3,
    /// A plan this build does not know, as sent.
    Other(String),
}

impl SubPlan {
    /// From the tag value: `Prime`, `1000`, `2000`, `3000`. Anything else is [`SubPlan::Other`].
    pub fn from_tag(value: &str) -> Self {
        match value {
            "Prime" => Self::Prime,
            "1000" => Self::Tier1,
            "2000" => Self::Tier2,
            "3000" => Self::Tier3,
            other => Self::Other(other.to_owned()),
        }
    }

    /// `Prime`, `Tier 1`, ...
    pub fn label(&self) -> &str {
        match self {
            Self::Prime => "Prime",
            Self::Tier1 => "Tier 1",
            Self::Tier2 => "Tier 2",
            Self::Tier3 => "Tier 3",
            Self::Other(other) => other,
        }
    }
}

/// The user a sub was gifted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    pub login: String,
    pub display_name: String,
    pub user_id: Option<String>,
}

impl Recipient {
    /// The name to show.
    pub fn name(&self) -> &str {
        if self.display_name.trim().is_empty() {
            &self.login
        } else {
            &self.display_name
        }
    }
}

/// The colour of an `/announce` (`msg-param-color`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnnouncementColor {
    /// The channel's own accent (also what an unknown or missing value means).
    #[default]
    Primary,
    Blue,
    Green,
    Orange,
    Purple,
}

impl AnnouncementColor {
    /// From the tag value (`PRIMARY`, `BLUE`, ... in any case); anything else is `Primary`.
    pub fn from_tag(value: &str) -> Self {
        match value.to_ascii_uppercase().as_str() {
            "BLUE" => Self::Blue,
            "GREEN" => Self::Green,
            "ORANGE" => Self::Orange,
            "PURPLE" => Self::Purple,
            _ => Self::Primary,
        }
    }
}

/// What a `USERNOTICE` announced (its `msg-id`). The author of the message is the user the notice
/// is about (the subscriber, the gifter, the raiding channel, the announcer); the text the user
/// typed with it, if any, is the message's fragments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// `sub`: a first subscription.
    Sub {
        plan: SubPlan,
        /// Months subscribed in total (1 for a first sub).
        months: u32,
    },
    /// `resub`.
    Resub {
        plan: SubPlan,
        months: u32,
        /// Consecutive months, when the user shared their streak.
        streak: Option<u32>,
    },
    /// `subgift` / `anonsubgift`: one sub gifted to `recipient`.
    Gift {
        plan: SubPlan,
        recipient: Recipient,
        /// Months the gift covers.
        months: u32,
        /// Subs the gifter has gifted in this channel in total.
        gifter_total: Option<u32>,
        /// The gifter chose to stay anonymous (the message's author is then the anonymous
        /// gifter user, `ananonymousgifter`).
        anonymous: bool,
        /// One of the gifts of a mystery (community) gift: Twitch sends one line per recipient
        /// after the `submysterygift` one, and the UI shows those quietly.
        community: bool,
    },
    /// `submysterygift` / `anonsubmysterygift`: a number of random viewers got a sub.
    MysteryGift {
        plan: SubPlan,
        count: u32,
        gifter_total: Option<u32>,
        anonymous: bool,
    },
    /// `giftpaidupgrade` / `anongiftpaidupgrade`: continuing a gifted sub.
    GiftPaidUpgrade {
        /// Who gifted the sub they are continuing, `None` for an anonymous gifter.
        gifter: Option<String>,
        promo: Option<String>,
    },
    /// `primepaidupgrade`: Prime to a paid sub.
    PrimePaidUpgrade { plan: SubPlan },
    /// `raid`: `from` is the raiding channel.
    Raid {
        from_login: String,
        from_name: String,
        viewers: u32,
        profile_image: Option<String>,
    },
    /// `announcement`.
    Announcement { color: AnnouncementColor },
    /// `ritual` with `new_chatter`: someone's first message in the channel.
    NewChatter,
    /// `bitsbadgetier`: a new bits badge.
    BitsBadgeTier { tier: u32 },
    /// Any other `msg-id` (`unraid`, `standardpayforward`, ...): drawn from `system-msg`.
    Other { msg_id: String },
}

impl EventKind {
    /// A gift that belongs to a mystery gift (drawn quietly).
    pub fn is_community_gift(&self) -> bool {
        matches!(
            self,
            Self::Gift {
                community: true,
                ..
            }
        )
    }
}

/// Shared chat: where a message was really sent. Twitch delivers the message to every channel in
/// the session, with the room it came from in `source-room-id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedChat {
    /// The room the message was sent in (`source-room-id`).
    pub source_room_id: String,
    /// The id of the message in that room (`source-id`).
    pub source_id: Option<MessageId>,
    /// The author's badges in that room (`source-badges`).
    pub source_badges: Vec<BadgeId>,
    /// The source channel's display name, when known. The parser cannot know it (the tags only
    /// carry the room id); the app fills it in from the channels it has joined, or by lookup.
    pub source_name: Option<String>,
}

impl SharedChat {
    /// Whether this is the copy of a message sent in another channel: `room_id` (the room this
    /// message was delivered in, `room-id`) is not the room it came from. A message sent in the
    /// channel itself carries its own room as source (or no `room_id` is known): not a copy.
    pub fn is_copy_in(&self, room_id: Option<&str>) -> bool {
        room_id.is_some_and(|room| room != self.source_room_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_plans_and_labels() {
        assert_eq!(SubPlan::from_tag("1000"), SubPlan::Tier1);
        assert_eq!(SubPlan::from_tag("2000").label(), "Tier 2");
        assert_eq!(SubPlan::from_tag("Prime").label(), "Prime");
        assert_eq!(SubPlan::from_tag("4000").label(), "4000");
    }

    #[test]
    fn announcement_colours_ignore_case_and_unknown_means_primary() {
        assert_eq!(AnnouncementColor::from_tag("blue"), AnnouncementColor::Blue);
        assert_eq!(
            AnnouncementColor::from_tag("ORANGE"),
            AnnouncementColor::Orange
        );
        assert_eq!(
            AnnouncementColor::from_tag("PRIMARY"),
            AnnouncementColor::Primary
        );
        assert_eq!(
            AnnouncementColor::from_tag("pink"),
            AnnouncementColor::Primary
        );
    }

    #[test]
    fn a_shared_message_is_a_copy_only_in_another_room() {
        let shared = SharedChat {
            source_room_id: "10".into(),
            source_id: None,
            source_badges: Vec::new(),
            source_name: None,
        };
        assert!(shared.is_copy_in(Some("20")));
        assert!(!shared.is_copy_in(Some("10")));
        assert!(!shared.is_copy_in(None));
    }

    #[test]
    fn recipients_fall_back_to_the_login() {
        let mut r = Recipient {
            login: "lucky".into(),
            display_name: "Lucky".into(),
            user_id: None,
        };
        assert_eq!(r.name(), "Lucky");
        r.display_name = " ".into();
        assert_eq!(r.name(), "lucky");
    }
}
