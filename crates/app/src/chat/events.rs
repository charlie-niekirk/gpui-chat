//! Event rows (PR 5.1): what a sub, gift, raid, announcement, cheer, highlight, first message or
//! shared-chat message adds to a row. Kept out of [`super::element`] and [`super::display`] so the
//! element only asks three things of this module: the *header* (the system line drawn above the
//! user's message, pure text, see [`header`]), the *look* (an accent bar and a tint behind the row,
//! [`look`] and [`Look::paint`]) and the shared-chat *source label* ([`source_label`]).
//!
//! The header is part of the row's display text (so it is selected and copied like any text, see
//! [`notice_line`]) and the user's message follows it on the next line, through a hard break
//! ([`super::flow::AtomKind::Break`]). Colours come from the theme tokens ([`Colors`]); nothing
//! here is shaped, so a theme change re-lays out nothing.

use gpui_chat_core::{AnnouncementColor, EventKind, Message, MessageKind, SharedChat, SubPlan};
use gpui_kit::{Bounds, Hsla, Pixels, Window, fill, hsla, px, size};

use super::colors;
use super::display::author_name;
use crate::theme::Tokens;

/// Which colour a row's accent bar uses. Decided by the kind of event, never by the theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accent {
    /// Subs, gifts, upgrades and the rest of what a viewer gives the channel.
    Support,
    /// A raid.
    Raid,
    /// Cheered bits.
    Bits,
    /// A channel-points highlight or custom reward.
    Points,
    /// A viewer's first message.
    First,
    /// An `/announce`, in the colour its sender chose.
    Announcement(AnnouncementColor),
}

/// How an event row is dressed: a bar on its left edge and a faint tint behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
    pub accent: Accent,
    /// One of the many lines of a mystery gift: no bar and no tint, muted text.
    pub quiet: bool,
}

/// The theme colours rows of this kind use, taken from [`Tokens`] once per render.
#[derive(Clone, Copy, Debug)]
pub struct Colors {
    accent: Hsla,
    link: Hsla,
    success: Hsla,
    warning: Hsla,
    /// Announcement blue and purple have no token: fixed hues (readability is adjusted at use).
    blue: Hsla,
    purple: Hsla,
}

impl Colors {
    pub fn from_tokens(tokens: &Tokens) -> Self {
        Self {
            accent: tokens.accent,
            link: tokens.accent_link,
            success: tokens.success,
            warning: tokens.warning,
            blue: hsla(212.0 / 360.0, 0.80, 0.58, 1.0),
            purple: hsla(268.0 / 360.0, 0.75, 0.66, 1.0),
        }
    }

    /// The colour of `accent`.
    pub fn of(&self, accent: Accent) -> Hsla {
        match accent {
            Accent::Support => self.accent,
            Accent::Raid => self.warning,
            Accent::Bits => self.success,
            Accent::Points => self.link,
            Accent::First => self.success,
            Accent::Announcement(AnnouncementColor::Primary) => self.accent,
            Accent::Announcement(AnnouncementColor::Blue) => self.blue,
            Accent::Announcement(AnnouncementColor::Green) => self.success,
            Accent::Announcement(AnnouncementColor::Orange) => self.warning,
            Accent::Announcement(AnnouncementColor::Purple) => self.purple,
        }
    }

    /// The header text of `accent`: its colour, moved just far enough from `background` to read.
    pub fn header_text(&self, accent: Accent, background: Hsla) -> Hsla {
        colors::readable_hsla(self.of(accent), background)
    }
}

/// Width of the accent bar.
const BAR_WIDTH: f32 = 3.0;
/// Opacity of the tint behind an event row.
const TINT: f32 = 0.08;

impl Look {
    /// Draws the tint (unless `tint` is off) and the bar into `bounds` (the whole row). The text
    /// is laid out as for any other row, so nothing moves: the bar sits in the row's left padding.
    pub fn paint(&self, bounds: Bounds<Pixels>, colors: &Colors, tint: bool, window: &mut Window) {
        if self.quiet {
            return;
        }
        let color = colors.of(self.accent);
        if tint {
            window.paint_quad(fill(bounds, color.opacity(TINT)));
        }
        let bar = Bounds::new(bounds.origin, size(px(BAR_WIDTH), bounds.size.height));
        window.paint_quad(fill(bar, color.opacity(0.9)));
    }
}

/// How `message` is dressed, if it is an event row.
pub fn look(message: &Message) -> Option<Look> {
    if let Some(event) = message
        .event
        .as_ref()
        .filter(|_| message.kind == MessageKind::UserNotice)
    {
        let accent = match event {
            EventKind::Raid { .. } => Accent::Raid,
            EventKind::Announcement { color } => Accent::Announcement(*color),
            EventKind::NewChatter => Accent::First,
            EventKind::BitsBadgeTier { .. } => Accent::Bits,
            EventKind::Other { .. } => Accent::Support,
            _ => Accent::Support,
        };
        // Other kinds of notice with nothing to say have no header and so no row to dress.
        header(message)?;
        return Some(Look {
            accent,
            quiet: event.is_community_gift(),
        });
    }
    if !matches!(message.kind, MessageKind::Chat | MessageKind::Action) {
        return None;
    }
    let accent = if message.reward_id.is_some() || message.flags.highlighted {
        Accent::Points
    } else if message.bits.is_some() {
        Accent::Bits
    } else if message.flags.first_message {
        Accent::First
    } else {
        return None;
    };
    Some(Look {
        accent,
        quiet: false,
    })
}

/// `1 raider`, `5 raiders`.
fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `3 months`, `1 month`.
fn months(n: u32) -> String {
    plural(n, "month", "months")
}

/// `at Tier 1`, `with Prime`.
fn plan_how(plan: &SubPlan) -> String {
    match plan {
        SubPlan::Prime => "with Prime".to_owned(),
        other => format!("at {}", other.label()),
    }
}

fn plan_phrase(plan: &SubPlan) -> String {
    match plan {
        SubPlan::Prime => "Prime".to_owned(),
        other => other.label().to_owned(),
    }
}

/// The system line of a `USERNOTICE`, in one line of plain text; `None` when there is nothing to
/// say (an unknown notice that carries no `system-msg`). Our own wording where the tags give
/// enough to build it, Twitch's `system-msg` for the kinds this build does not know.
pub fn notice_line(message: &Message) -> Option<String> {
    let event = message.event.as_ref()?;
    let name = message
        .author
        .as_ref()
        .map_or("Someone", |author| author_name(author));
    let line = match event {
        EventKind::Sub { plan, months: m } => {
            if *m > 1 {
                format!("{name} subscribed {} for {}", plan_how(plan), months(*m))
            } else {
                format!("{name} subscribed {}", plan_how(plan))
            }
        }
        EventKind::Resub {
            plan,
            months: m,
            streak,
        } => {
            let mut line = format!("{name} resubscribed {} for {}", plan_how(plan), months(*m));
            if let Some(streak) = streak {
                line.push_str(&format!(", {streak} month streak"));
            }
            line
        }
        EventKind::Gift {
            plan,
            recipient,
            months: m,
            gifter_total,
            anonymous,
            ..
        } => {
            let gifter = if *anonymous {
                "An anonymous user"
            } else {
                name
            };
            let what = if *m > 1 {
                format!("{} of {}", months(*m), plan_phrase(plan))
            } else {
                format!("a {} sub", plan_phrase(plan))
            };
            let mut line = format!("{gifter} gifted {what} to {}", recipient.name());
            if let Some(total) = gifter_total {
                line.push_str(&format!(" ({total} gifted in total)"));
            }
            line
        }
        EventKind::MysteryGift {
            plan,
            count,
            gifter_total,
            anonymous,
        } => {
            let gifter = if *anonymous {
                "An anonymous user"
            } else {
                name
            };
            let mut line = format!(
                "{gifter} is gifting {count} {} {} to the community",
                plan_phrase(plan),
                if *count == 1 { "sub" } else { "subs" }
            );
            if let Some(total) = gifter_total {
                line.push_str(&format!(" ({total} gifted in total)"));
            }
            line
        }
        EventKind::GiftPaidUpgrade { gifter, .. } => match gifter {
            Some(gifter) => format!("{name} is continuing the gift sub from {gifter}"),
            None => format!("{name} is continuing the gift sub from an anonymous user"),
        },
        EventKind::PrimePaidUpgrade { plan } => {
            format!("{name} converted from a Prime sub to {}", plan_phrase(plan))
        }
        EventKind::Raid {
            from_name, viewers, ..
        } => format!(
            "{} from {from_name} {} joined",
            plural(*viewers, "raider", "raiders"),
            if *viewers == 1 { "has" } else { "have" }
        ),
        EventKind::Announcement { .. } => "Announcement".to_owned(),
        EventKind::NewChatter => format!("{name} is new here"),
        EventKind::BitsBadgeTier { tier } => format!("{name} earned the {tier} bits badge"),
        EventKind::Other { .. } => one_line(message.system_text.as_deref()?),
    };
    let line = line.trim().to_owned();
    (!line.is_empty()).then_some(line)
}

/// What the menu's *Copy message* puts on the clipboard: the message's own text, and for an event
/// notice its line first (`Alice subscribed at Tier 1: thanks all`), so copying a sub that came
/// with no text of its own still copies something.
pub fn copy_text(message: &Message) -> String {
    let body = one_line(&message.plain_text());
    match (message.kind, notice_line(message)) {
        (MessageKind::UserNotice, Some(line)) if body.is_empty() => line,
        (MessageKind::UserNotice, Some(line)) => format!("{line}: {body}"),
        _ => body,
    }
}

/// `text` on one line: control characters and the U+E0000 tag become spaces or nothing.
fn one_line(text: &str) -> String {
    text.chars()
        .filter(|&c| c != '\u{E0000}')
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The label above a chat line that carries something extra: `First message · Cheered 100 bits`.
fn marker_line(message: &Message) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if message.reward_id.is_some() {
        parts.push("Redeemed a reward".to_owned());
    }
    if message.flags.highlighted {
        parts.push("Highlighted message".to_owned());
    }
    if message.flags.first_message {
        parts.push("First message".to_owned());
    }
    if let Some(bits) = message.bits {
        parts.push(format!("Cheered {}", plural(bits, "bit", "bits")));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The line drawn above the row's message, if it has one. A notice's carries the timestamp (it is
/// the row's first line); a chat line's does not (the timestamp stays on the message).
pub fn header(message: &Message) -> Option<Header> {
    match message.kind {
        MessageKind::UserNotice => {
            let text = notice_line(message)?;
            Some(Header {
                text,
                is_notice: true,
            })
        }
        MessageKind::Chat | MessageKind::Action => marker_line(message).map(|text| Header {
            text,
            is_notice: false,
        }),
        MessageKind::Notice | MessageKind::System => None,
    }
}

/// See [`header`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub text: String,
    /// The line of a `USERNOTICE`: it is the row's first line and carries the timestamp.
    pub is_notice: bool,
}

/// The text before the name of a message that was sent in another channel of a shared chat
/// session: that channel's name in brackets (`[shared chat]` while it is not known). `None` for every other
/// message, and for one that was sent in this very channel.
pub fn source_label(message: &Message) -> Option<String> {
    if !matches!(message.kind, MessageKind::Chat | MessageKind::Action) {
        return None;
    }
    let shared: &SharedChat = message.shared.as_ref()?;
    if !shared.is_copy_in(message.raw_tags.get("room-id")) {
        return None;
    }
    let name = shared
        .source_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    Some(match name {
        Some(name) => format!("[{name}]"),
        None => "[shared chat]".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::message::RawTags;
    use gpui_chat_core::{Author, Fragment, MessageId, Recipient, SharedChat, Timestamp};

    use super::*;

    fn notice(event: EventKind, system: Option<&str>) -> Message {
        let mut message = Message::chat(
            "n1",
            Timestamp(0),
            Author::new("alice", "Alice"),
            Vec::new(),
        );
        message.kind = MessageKind::UserNotice;
        message.system_text = system.map(str::to_owned);
        message.event = Some(event);
        message
    }

    fn line(event: EventKind) -> String {
        notice_line(&notice(event, None)).expect("a line")
    }

    fn recipient() -> Recipient {
        Recipient {
            login: "lucky".into(),
            display_name: "Lucky".into(),
            user_id: None,
        }
    }

    #[test]
    fn subs_say_the_plan_and_the_months() {
        let sub = |plan, months| EventKind::Sub { plan, months };
        assert_eq!(line(sub(SubPlan::Tier1, 1)), "Alice subscribed at Tier 1");
        assert_eq!(line(sub(SubPlan::Prime, 1)), "Alice subscribed with Prime");
        assert_eq!(
            line(sub(SubPlan::Tier2, 3)),
            "Alice subscribed at Tier 2 for 3 months"
        );
        assert_eq!(
            line(EventKind::Resub {
                plan: SubPlan::Tier1,
                months: 12,
                streak: Some(12)
            }),
            "Alice resubscribed at Tier 1 for 12 months, 12 month streak"
        );
        assert_eq!(
            line(EventKind::Resub {
                plan: SubPlan::Prime,
                months: 15,
                streak: None
            }),
            "Alice resubscribed with Prime for 15 months"
        );
        assert_eq!(
            line(EventKind::Resub {
                plan: SubPlan::Tier3,
                months: 1,
                streak: None
            }),
            "Alice resubscribed at Tier 3 for 1 month"
        );
    }

    #[test]
    fn gifts_name_gifter_recipient_and_totals() {
        let gift = |anonymous, months, gifter_total| EventKind::Gift {
            plan: SubPlan::Tier1,
            recipient: recipient(),
            months,
            gifter_total,
            anonymous,
            community: false,
        };
        assert_eq!(
            line(gift(false, 1, Some(50))),
            "Alice gifted a Tier 1 sub to Lucky (50 gifted in total)"
        );
        assert_eq!(
            line(gift(true, 1, None)),
            "An anonymous user gifted a Tier 1 sub to Lucky"
        );
        assert_eq!(
            line(gift(false, 3, None)),
            "Alice gifted 3 months of Tier 1 to Lucky"
        );
        let mystery = |count, anonymous, total| EventKind::MysteryGift {
            plan: SubPlan::Tier1,
            count,
            gifter_total: total,
            anonymous,
        };
        assert_eq!(
            line(mystery(5, false, Some(103))),
            "Alice is gifting 5 Tier 1 subs to the community (103 gifted in total)"
        );
        assert_eq!(
            line(mystery(1, true, None)),
            "An anonymous user is gifting 1 Tier 1 sub to the community"
        );
        assert_eq!(
            line(EventKind::GiftPaidUpgrade {
                gifter: Some("Bob".into()),
                promo: None
            }),
            "Alice is continuing the gift sub from Bob"
        );
        assert_eq!(
            line(EventKind::GiftPaidUpgrade {
                gifter: None,
                promo: None
            }),
            "Alice is continuing the gift sub from an anonymous user"
        );
        assert_eq!(
            line(EventKind::PrimePaidUpgrade {
                plan: SubPlan::Tier1
            }),
            "Alice converted from a Prime sub to Tier 1"
        );
    }

    #[test]
    fn raids_announcements_and_the_rest() {
        let raid = |viewers| EventKind::Raid {
            from_login: "x".into(),
            from_name: "RaidingChannel".into(),
            viewers,
            profile_image: None,
        };
        assert_eq!(
            line(raid(1234)),
            "1234 raiders from RaidingChannel have joined"
        );
        assert_eq!(line(raid(1)), "1 raider from RaidingChannel has joined");
        assert_eq!(
            line(EventKind::Announcement {
                color: AnnouncementColor::Blue
            }),
            "Announcement"
        );
        assert_eq!(line(EventKind::NewChatter), "Alice is new here");
        assert_eq!(
            line(EventKind::BitsBadgeTier { tier: 1000 }),
            "Alice earned the 1000 bits badge"
        );
        // Unknown kinds are Twitch's own line, flattened; none at all means no row.
        let other = EventKind::Other {
            msg_id: "unraid".into(),
        };
        assert_eq!(
            notice_line(&notice(
                other.clone(),
                Some("The raid\nwas cancelled.\u{E0000}")
            ))
            .as_deref(),
            Some("The raid was cancelled.")
        );
        assert_eq!(notice_line(&notice(other, None)), None);
    }

    #[test]
    fn copy_message_gives_the_event_line_and_the_users_text() {
        let mut message = notice(
            EventKind::Sub {
                plan: SubPlan::Tier1,
                months: 1,
            },
            None,
        );
        assert_eq!(copy_text(&message), "Alice subscribed at Tier 1");
        message.fragments = vec![Fragment::Text("thanks\nall".into())];
        assert_eq!(
            copy_text(&message),
            "Alice subscribed at Tier 1: thanks all"
        );
        assert_eq!(copy_text(&chat()), "hi");
    }

    #[test]
    fn a_notice_without_an_author_is_someone() {
        let mut message = notice(EventKind::NewChatter, None);
        message.author = None;
        assert_eq!(
            notice_line(&message).as_deref(),
            Some("Someone is new here")
        );
    }

    fn chat() -> Message {
        Message::chat(
            "c1",
            Timestamp(0),
            Author::new("alice", "Alice"),
            vec![Fragment::Text("hi".into())],
        )
    }

    #[test]
    fn a_plain_chat_line_has_no_header_look_or_label() {
        let message = chat();
        assert_eq!(header(&message), None);
        assert_eq!(look(&message), None);
        assert_eq!(source_label(&message), None);
    }

    #[test]
    fn chat_lines_with_something_extra_get_a_marker_line_and_an_accent() {
        let mut message = chat();
        message.flags.first_message = true;
        assert_eq!(header(&message).unwrap().text, "First message");
        assert_eq!(look(&message).unwrap().accent, Accent::First);
        assert!(!header(&message).unwrap().is_notice);

        message.bits = Some(100);
        assert_eq!(
            header(&message).unwrap().text,
            "First message · Cheered 100 bits"
        );
        assert_eq!(look(&message).unwrap().accent, Accent::Bits);

        message.bits = Some(1);
        message.flags.highlighted = true;
        message.reward_id = Some("r".into());
        assert_eq!(
            header(&message).unwrap().text,
            "Redeemed a reward · Highlighted message · First message · Cheered 1 bit"
        );
        assert_eq!(look(&message).unwrap().accent, Accent::Points);

        // A /me line is dressed the same; notices and system lines never are by these flags.
        message.kind = MessageKind::Action;
        assert!(look(&message).is_some());
        message.kind = MessageKind::System;
        assert_eq!((header(&message), look(&message)), (None, None));
    }

    #[test]
    fn notices_get_a_look_by_kind_and_gift_lines_of_a_mystery_gift_are_quiet() {
        let look_of = |event| look(&notice(event, Some("x"))).unwrap();
        assert_eq!(
            look_of(EventKind::Raid {
                from_login: "x".into(),
                from_name: "X".into(),
                viewers: 3,
                profile_image: None
            })
            .accent,
            Accent::Raid
        );
        assert_eq!(
            look_of(EventKind::Announcement {
                color: AnnouncementColor::Green
            })
            .accent,
            Accent::Announcement(AnnouncementColor::Green)
        );
        assert_eq!(look_of(EventKind::NewChatter).accent, Accent::First);
        assert_eq!(
            look_of(EventKind::Sub {
                plan: SubPlan::Tier1,
                months: 1
            }),
            Look {
                accent: Accent::Support,
                quiet: false
            }
        );
        let community = |community| EventKind::Gift {
            plan: SubPlan::Tier1,
            recipient: recipient(),
            months: 1,
            gifter_total: None,
            anonymous: false,
            community,
        };
        assert!(look_of(community(true)).quiet);
        assert!(!look_of(community(false)).quiet);
        // An unknown notice with no text has no row to dress.
        assert_eq!(
            look(&notice(EventKind::Other { msg_id: "x".into() }, None)),
            None
        );
    }

    fn shared(name: Option<&str>, source: &str, room: &str) -> Message {
        let mut message = chat();
        message.raw_tags = RawTags::new([("room-id".to_owned(), room.to_owned())]);
        message.shared = Some(SharedChat {
            source_room_id: source.into(),
            source_id: Some(MessageId::from("orig")),
            source_badges: Vec::new(),
            source_name: name.map(str::to_owned),
        });
        message
    }

    #[test]
    fn the_source_of_a_shared_message_is_named_when_it_came_from_elsewhere() {
        assert_eq!(
            source_label(&shared(Some("OtherChan"), "2000", "1000")).as_deref(),
            Some("[OtherChan]")
        );
        assert_eq!(
            source_label(&shared(None, "2000", "1000")).as_deref(),
            Some("[shared chat]")
        );
        assert_eq!(
            source_label(&shared(Some("  "), "2000", "1000")).as_deref(),
            Some("[shared chat]")
        );
        // Sent in this channel: nothing to say about where it came from.
        assert_eq!(source_label(&shared(Some("Me"), "1000", "1000")), None);
        // No room id tag (a synthetic message): nothing either.
        let mut message = shared(Some("X"), "2000", "1000");
        message.raw_tags = RawTags::default();
        assert_eq!(source_label(&message), None);
    }

    #[test]
    fn every_accent_has_a_colour_distinct_enough_to_tell_apart() {
        let colors =
            Colors::from_tokens(&Tokens::for_variant(gpui_chat_settings::ThemeVariant::Dark));
        let announcements = [
            AnnouncementColor::Blue,
            AnnouncementColor::Purple,
            AnnouncementColor::Primary,
        ]
        .map(|c| colors.of(Accent::Announcement(c)));
        assert_ne!(announcements[0], announcements[1]);
        assert_eq!(
            colors.of(Accent::Announcement(AnnouncementColor::Green)),
            colors.of(Accent::Bits)
        );
    }
}
