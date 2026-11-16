//! The pure half of the user card (PR 5.4): which messages are a user's, how old an account is,
//! what moderation the card offers, and the small date handling Helix's timestamps need.
//!
//! The card itself (a popover over the chat, `crates/app/src/chat/user_card.rs`) and the Helix
//! lookup behind it (`gpui_chat_twitch::users`) are I/O and UI; everything that can be decided
//! from data is here, so it is unit-tested without a window or a network.

use std::sync::Arc;

use crate::emote::BadgeId;
use crate::event::ChannelRole;
use crate::ids::ChannelId;
use crate::message::{Message, MessageKind};
use crate::moderation::author_is_staff;
use crate::store::MessageStore;

/// Most messages the card lists: the newest ones, so a chatty user stays cheap to draw (the
/// list is not virtualised).
pub const MAX_CARD_MESSAGES: usize = 200;

/// Whether `login` can be a Twitch login: 1 to 25 ASCII letters, digits and underscores. A
/// mention such as `@!!` or a stray `@` is not a user, and is never sent to Helix.
pub fn is_login(login: &str) -> bool {
    (1..=25).contains(&login.len())
        && login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The twitch.tv page of `login`, or `None` when it is not a login. The only URL the card opens
/// in the browser, always https.
pub fn profile_url(login: &str) -> Option<String> {
    is_login(login).then(|| format!("https://www.twitch.tv/{}", login.to_ascii_lowercase()))
}

/// The messages of `login` the store holds, newest first, at most `limit`, with their sequence
/// numbers. A message counts when a user wrote it: a chat line, a `/me` line, or the message
/// that came with a resub (an event card without text is not something they said).
pub fn user_messages(store: &MessageStore, login: &str, limit: usize) -> Vec<(u64, Arc<Message>)> {
    store
        .iter_with_seq()
        .rev()
        .filter(|(_, message)| is_by(message, login))
        .take(limit)
        .map(|(seq, message)| (seq, Arc::clone(message)))
        .collect()
}

/// One user's messages in a store, kept up to date without scanning it again: the card asks for
/// them every frame it is open, and a busy channel holds up to 100 000 messages.
///
/// It remembers which sequence numbers were theirs and how far into the store it has looked; each
/// [`CardMessages::update`] only looks at what arrived since, forgets what was evicted, and starts
/// over when the store renumbered its messages (a history merge, [`MessageStore::rebuild_serial`]).
/// The messages themselves are read from the store each time, so a removal or any other
/// replacement shows at once.
#[derive(Debug, Clone)]
pub struct CardMessages {
    login: String,
    /// Their sequence numbers, oldest first, at most [`MAX_CARD_MESSAGES`].
    seqs: Vec<u64>,
    /// The sequence number the next scan starts at.
    scanned_to: u64,
    /// The store's rebuild serial at the last scan; `None` before the first.
    rebuilds: Option<u64>,
}

impl CardMessages {
    pub fn new(login: &str) -> Self {
        Self {
            login: login.to_owned(),
            seqs: Vec::new(),
            scanned_to: 0,
            rebuilds: None,
        }
    }

    /// Their messages in `store`, newest first, at most [`MAX_CARD_MESSAGES`], with their
    /// sequence numbers; the same as [`user_messages`] would give.
    pub fn update(&mut self, store: &MessageStore) -> Vec<(u64, Arc<Message>)> {
        let serial = store.rebuild_serial();
        if self.rebuilds != Some(serial) || store.next_seq() < self.scanned_to {
            self.seqs.clear();
            self.scanned_to = store.first_seq();
            self.rebuilds = Some(serial);
        }
        self.seqs.extend(
            store
                .iter_since(self.scanned_to)
                .filter(|(_, message)| is_by(message, &self.login))
                .map(|(seq, _)| seq),
        );
        self.scanned_to = store.next_seq();
        let evicted = self.seqs.partition_point(|seq| *seq < store.first_seq());
        self.seqs.drain(..evicted);
        if self.seqs.len() > MAX_CARD_MESSAGES {
            self.seqs.drain(..self.seqs.len() - MAX_CARD_MESSAGES);
        }
        self.seqs
            .iter()
            .rev()
            .filter_map(|seq| {
                let message = store.get_by_seq(*seq)?;
                is_by(message, &self.login).then(|| (*seq, Arc::clone(message)))
            })
            .collect()
    }
}

/// Whether `message` is something `login` said (case-insensitively).
pub fn is_by(message: &Message, login: &str) -> bool {
    let wrote = match message.kind {
        MessageKind::Chat | MessageKind::Action => true,
        MessageKind::UserNotice => !message.fragments.is_empty(),
        MessageKind::Notice | MessageKind::System => false,
    };
    wrote
        && message
            .author
            .as_ref()
            .is_some_and(|author| author.login.eq_ignore_ascii_case(login))
}

/// What the store knows about one user, from the messages [`user_messages`] found: their
/// newest name, colour and badges (the newest message is the most current), and whether they are
/// only known from another room of a shared chat.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KnownUser {
    /// The store holds at least one message of theirs: their badges (and so whether they are a
    /// moderator) are known. A user only named by a mention is not seen.
    pub seen: bool,
    pub display_name: Option<String>,
    pub user_id: Option<String>,
    pub badges: Vec<BadgeId>,
    /// Every message the store has of them was sent in another channel of a shared chat session
    /// (and shown here as a copy). They are not a chatter of this room: no moderation from here.
    pub only_shared_copies: bool,
    /// The newest of their messages is a ban or a timeout (Unban makes sense).
    pub punished: bool,
}

/// Summarises `messages` (newest first, as [`user_messages`] returns them).
pub fn known_user(messages: &[(u64, Arc<Message>)]) -> KnownUser {
    let mut known = KnownUser::default();
    let Some((_, newest)) = messages.first() else {
        return known;
    };
    known.seen = true;
    if let Some(author) = &newest.author {
        known.display_name =
            Some(author.display_name.clone()).filter(|name| !name.trim().is_empty());
        known.badges = author.badges.clone();
    }
    // The newest message with an id carries it (history lines and echoes may lack one).
    known.user_id = messages
        .iter()
        .filter_map(|(_, message)| message.author.as_ref()?.user_id.clone())
        .find(|id| !id.is_empty());
    known.only_shared_copies = messages.iter().all(|(_, message)| is_shared_copy(message));
    known.punished = newest.flags.removal.is_punishment();
    known
}

/// Whether `message` is a shared-chat copy of a message sent in another room.
pub fn is_shared_copy(message: &Message) -> bool {
    message
        .shared
        .as_ref()
        .is_some_and(|shared| shared.is_copy_in(message.raw_tags.get("room-id")))
}

/// The moderation the card offers for one user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CardActions {
    pub timeout: bool,
    pub ban: bool,
    pub unban: bool,
}

impl CardActions {
    pub fn any(self) -> bool {
        self.timeout || self.ban || self.unban
    }
}

/// The moderation controls the card offers for `login` in `channel`, for a user who is `role`
/// there and whose own login is `own_login` (when known). None for viewers, for the user
/// themself, for the broadcaster, for someone known only from another room of a shared chat, for
/// someone not [seen](KnownUser::seen) (named by a mention: their badges, and so whether they are
/// a moderator or the broadcaster, are unknown, and Twitch refuses to punish those), and
/// (as for a message, [`crate::row_actions`]) none against another moderator or staff unless the
/// user is the broadcaster. Unban is always offered to someone who may moderate: the card has no
/// ban to read it off (the user's messages may have left the store), and Twitch answers
/// "not banned" to an unban of someone who is not.
pub fn card_actions(
    role: ChannelRole,
    channel: &ChannelId,
    own_login: Option<&str>,
    login: &str,
    known: &KnownUser,
) -> CardActions {
    if !role.can_moderate()
        || !is_login(login)
        || !known.seen
        || known.only_shared_copies
        || login.eq_ignore_ascii_case(channel.login())
        || own_login.is_some_and(|own| login.eq_ignore_ascii_case(own))
    {
        return CardActions::default();
    }
    let may_punish = role.is_broadcaster() || !author_is_staff(&known.badges);
    CardActions {
        timeout: may_punish,
        ban: may_punish,
        unban: may_punish,
    }
}

/// Whether the account Helix found for a login (`helix_id`) is the one that sent the messages
/// the card is about (`known_id`, the `user-id` of their message). After a rename another account
/// may own the old login; moderating "by login" would then hit a stranger. Unknown ids agree.
pub fn same_account(known_id: Option<&str>, helix_id: &str) -> bool {
    known_id.is_none_or(|known| known == helix_id)
}

/// A timeout length in words for a screen reader: `10 minutes`, `1 hour`, `2 weeks`.
pub fn duration_words(seconds: u64) -> String {
    const UNITS: [(u64, &str); 5] = [
        (604_800, "week"),
        (86_400, "day"),
        (3_600, "hour"),
        (60, "minute"),
        (1, "second"),
    ];
    for (size, name) in UNITS {
        if seconds >= size && seconds.is_multiple_of(size) {
            let count = seconds / size;
            return format!("{count} {name}{}", if count == 1 { "" } else { "s" });
        }
    }
    format!("{seconds} seconds")
}

// ---------------------------------------------------------------------------------------------
// Dates. Helix sends RFC 3339 (`2015-03-14T09:26:53Z`); there is no date library in the
// workspace and these are two small, well-known conversions.
// ---------------------------------------------------------------------------------------------

/// Days from 1970-01-01 to the civil date `year-month-day` (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The civil date `(year, month, day)` of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Unix seconds of an RFC 3339 timestamp (`2015-03-14T09:26:53Z`, `…53.123456Z`, `…53+02:00`);
/// `None` for anything else. Fractions of a second are dropped.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', 't', ' '])?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // The time and its zone: `09:26:53[.frac](Z|±hh:mm)`.
    let zone_at = rest.find(['Z', 'z', '+', '-'])?;
    let (clock, zone) = rest.split_at(zone_at);
    let mut clock = clock.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second_text = clock.next()?;
    let second: i64 = second_text.split('.').next()?.parse().ok()?;
    if clock.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let offset = match zone {
        "Z" | "z" => 0,
        zone => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (hours, minutes) = zone[1..].split_once(':')?;
            let (hours, minutes): (i64, i64) = (hours.parse().ok()?, minutes.parse().ok()?);
            sign * (hours * 3600 + minutes * 60)
        }
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `14 Mar 2015` for Unix seconds, in UTC like the chat's clock (`display::clock`, `HH:MM` UTC):
/// the workspace has no time-zone data, and the card's dates are days, not times.
pub fn format_date(unix_seconds: i64) -> String {
    let (year, month, day) = civil_from_days(unix_seconds.div_euclid(86_400));
    let month = usize::try_from(month - 1)
        .ok()
        .and_then(|index| MONTHS.get(index))
        .copied()
        .unwrap_or("?");
    format!("{day} {month} {year}")
}

/// How long ago `from` was at `now` (both Unix seconds), in the largest whole unit: `3 years`,
/// `1 year`, `5 months`, `12 days`, `4 hours`, `less than an hour`. Years and months are calendar
/// ones (14 Mar 2015 to 13 Mar 2018 is 2 years, to the 14th 3). Something in the future (a
/// clock that is behind) reads as `less than an hour`.
pub fn format_age(from: i64, now: i64) -> String {
    fn plural(count: i64, unit: &str) -> String {
        format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
    }
    let elapsed = now.saturating_sub(from).max(0);
    let (from_year, from_month, from_day) = civil_from_days(from.div_euclid(86_400));
    let (now_year, now_month, now_day) = civil_from_days(now.div_euclid(86_400));
    let mut months = (now_year - from_year) * 12 + (now_month - from_month);
    // The month is not complete until the day and the time of day have come round.
    if (now_day, now.rem_euclid(86_400)) < (from_day, from.rem_euclid(86_400)) {
        months -= 1;
    }
    if elapsed > 0 && months >= 12 {
        plural(months / 12, "year")
    } else if elapsed > 0 && months >= 1 {
        plural(months, "month")
    } else if elapsed >= 86_400 {
        plural(elapsed / 86_400, "day")
    } else if elapsed >= 3600 {
        plural(elapsed / 3600, "hour")
    } else {
        "less than an hour".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Author, Fragment, RawTags, Removal, Timestamp};
    use crate::special::SharedChat;

    fn author(login: &str, badges: &[(&str, &str)]) -> Author {
        let mut author = Author::new(login, login.to_uppercase());
        author.badges = badges
            .iter()
            .map(|(set, version)| BadgeId::new(*set, *version))
            .collect();
        author
    }

    fn chat(id: &str, who: &str, text: &str) -> Arc<Message> {
        Arc::new(Message::chat(
            id,
            Timestamp::from_unix_millis(1_000),
            author(who, &[]),
            vec![Fragment::Text(text.to_owned())],
        ))
    }

    #[test]
    fn logins_are_one_to_twenty_five_word_characters() {
        assert!(is_login("forsen"));
        assert!(is_login("Bob_1"));
        assert!(is_login(&"a".repeat(25)));
        assert!(!is_login(""));
        assert!(!is_login(&"a".repeat(26)));
        assert!(!is_login("bob!"));
        assert!(!is_login("bob smith"));
        assert!(!is_login("bób"));
    }

    #[test]
    fn the_profile_link_is_https_and_lowercase_or_nothing() {
        assert_eq!(
            profile_url("Forsen").as_deref(),
            Some("https://www.twitch.tv/forsen")
        );
        assert_eq!(profile_url("a/../b"), None);
        assert_eq!(profile_url(""), None);
    }

    #[test]
    fn a_users_messages_come_newest_first_and_case_insensitively() {
        let mut store = MessageStore::new();
        store.push(chat("1", "bob", "first"));
        store.push(chat("2", "alice", "hello"));
        store.push(chat("3", "Bob", "second"));
        store.push(chat("4", "bob", "third"));
        let messages = user_messages(&store, "BOB", 10);
        let texts: Vec<_> = messages.iter().map(|(_, m)| m.plain_text()).collect();
        assert_eq!(texts, ["third", "second", "first"]);
        assert_eq!(
            messages.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            [3, 2, 0]
        );
        // The limit keeps the newest.
        let newest = user_messages(&store, "bob", 2);
        assert_eq!(newest.len(), 2);
        assert_eq!(newest[1].1.plain_text(), "second");
        assert!(user_messages(&store, "nobody", 10).is_empty());
    }

    fn same(a: &[(u64, Arc<Message>)], b: &[(u64, Arc<Message>)]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| x.0 == y.0 && Arc::ptr_eq(&x.1, &y.1))
    }

    #[test]
    fn the_cached_messages_follow_the_store_as_it_grows_evicts_and_changes() {
        let mut store = MessageStore::with_capacity(6);
        let mut cache = CardMessages::new("Bob");
        let check = |store: &MessageStore, cache: &mut CardMessages| {
            let fresh = user_messages(store, "bob", MAX_CARD_MESSAGES);
            assert!(same(&cache.update(store), &fresh));
            fresh.len()
        };
        assert_eq!(check(&store, &mut cache), 0);
        store.push(chat("1", "bob", "a"));
        store.push(chat("2", "alice", "b"));
        assert_eq!(check(&store, &mut cache), 1);
        // Nothing new: the same answer without a scan.
        assert_eq!(check(&store, &mut cache), 1);
        store.push(chat("3", "bob", "c"));
        store.push(chat("4", "bob", "d"));
        assert_eq!(check(&store, &mut cache), 3);
        // A message changing (a removal replaces it) shows without being found again.
        store.mark_deleted("3");
        let shown = cache.update(&store);
        assert!(
            shown
                .iter()
                .any(|(_, m)| m.id.as_str() == "3" && m.flags.deleted)
        );
        // Eviction forgets the oldest.
        for n in 5..12 {
            store.push(chat(
                &n.to_string(),
                if n % 2 == 0 { "bob" } else { "alice" },
                "x",
            ));
        }
        assert_eq!(check(&store, &mut cache), 3);
        assert!(
            user_messages(&store, "bob", 10)
                .iter()
                .all(|(seq, _)| *seq >= store.first_seq())
        );
    }

    #[test]
    fn the_cache_starts_over_when_history_renumbers_the_store() {
        let stamped = |id: &str, who: &str, ts: i64| {
            Arc::new(Message::chat(
                id,
                Timestamp::from_unix_millis(ts),
                author(who, &[]),
                vec![Fragment::Text(id.to_owned())],
            ))
        };
        let mut store = MessageStore::new();
        store.push(stamped("live1", "bob", 100));
        store.push(stamped("live2", "alice", 110));
        let mut cache = CardMessages::new("bob");
        assert_eq!(cache.update(&store).len(), 1);
        let before = store.rebuild_serial();
        store.merge_history(vec![stamped("old1", "bob", 10), stamped("old2", "bob", 20)]);
        assert_ne!(store.rebuild_serial(), before, "history went before live");
        let fresh = user_messages(&store, "bob", MAX_CARD_MESSAGES);
        assert_eq!(fresh.len(), 3);
        assert!(same(&cache.update(&store), &fresh));
        // A store that was emptied and refilled is not mistaken for the old one.
        let mut other = MessageStore::new();
        other.push(stamped("n1", "bob", 1));
        assert!(same(
            &cache.update(&other),
            &user_messages(&other, "bob", MAX_CARD_MESSAGES)
        ));
    }

    #[test]
    fn the_cache_keeps_the_newest_two_hundred() {
        let mut store = MessageStore::new();
        for n in 0..MAX_CARD_MESSAGES + 30 {
            store.push(chat(&n.to_string(), "bob", "x"));
        }
        let mut cache = CardMessages::new("bob");
        let shown = cache.update(&store);
        assert_eq!(shown.len(), MAX_CARD_MESSAGES);
        assert_eq!(shown[0].1.id.as_str(), (MAX_CARD_MESSAGES + 29).to_string());
        assert!(same(
            &shown,
            &user_messages(&store, "bob", MAX_CARD_MESSAGES)
        ));
    }

    #[test]
    fn system_lines_and_textless_events_are_not_something_the_user_said() {
        let mut store = MessageStore::new();
        let mut notice = Message::chat(
            "n",
            Timestamp::from_unix_millis(1),
            author("bob", &[]),
            vec![],
        );
        notice.kind = MessageKind::UserNotice;
        store.push(Arc::new(notice));
        let mut resub = Message::chat(
            "r",
            Timestamp::from_unix_millis(2),
            author("bob", &[]),
            vec![Fragment::Text("thanks".into())],
        );
        resub.kind = MessageKind::UserNotice;
        store.push(Arc::new(resub));
        let mut system = Message::chat(
            "s",
            Timestamp::from_unix_millis(3),
            author("bob", &[]),
            vec![Fragment::Text("x".into())],
        );
        system.kind = MessageKind::System;
        store.push(Arc::new(system));
        let found = user_messages(&store, "bob", 10);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.id.as_str(), "r");
    }

    #[test]
    fn what_is_known_comes_from_the_newest_message_and_the_first_id() {
        let mut older = Message::chat(
            "1",
            Timestamp::from_unix_millis(1),
            author("bob", &[("subscriber", "3")]),
            vec![],
        );
        older.author.as_mut().unwrap().user_id = Some("77".into());
        let mut newest = Message::chat(
            "2",
            Timestamp::from_unix_millis(2),
            author("bob", &[("moderator", "1")]),
            vec![],
        );
        newest.flags.removal = Removal::Banned;
        let known = known_user(&[(1, Arc::new(newest)), (0, Arc::new(older))]);
        assert_eq!(known.display_name.as_deref(), Some("BOB"));
        assert_eq!(known.user_id.as_deref(), Some("77"));
        assert_eq!(known.badges, [BadgeId::new("moderator", "1")]);
        assert!(known.punished);
        assert!(!known.only_shared_copies);
        assert_eq!(
            known_user(&[]),
            KnownUser {
                only_shared_copies: false,
                ..KnownUser::default()
            }
        );
    }

    #[test]
    fn a_user_known_only_from_shared_copies_is_foreign() {
        let mut copy = Message::chat(
            "1",
            Timestamp::from_unix_millis(1),
            author("bob", &[]),
            vec![],
        );
        copy.shared = Some(SharedChat {
            source_room_id: "999".into(),
            source_id: None,
            source_badges: Vec::new(),
            source_login: None,
            source_name: None,
        });
        copy.raw_tags = RawTags::new([("room-id".to_owned(), "1".to_owned())]);
        assert!(is_shared_copy(&copy));
        let copy = Arc::new(copy);
        assert!(known_user(&[(0, copy.clone())]).only_shared_copies);
        let native = chat("2", "bob", "hi");
        assert!(!known_user(&[(1, native), (0, copy)]).only_shared_copies);
    }

    fn actions(
        role: ChannelRole,
        own: Option<&str>,
        login: &str,
        known: &KnownUser,
    ) -> CardActions {
        card_actions(role, &ChannelId::new("streamer"), own, login, known)
    }

    #[test]
    fn a_user_only_named_by_a_mention_gets_no_moderation() {
        // Nothing of theirs is in the store: their badges are unknown, they may be a moderator
        // or the broadcaster of another name.
        let unseen = known_user(&[]);
        assert!(!unseen.seen);
        assert!(!actions(ChannelRole::Moderator, Some("me"), "bob", &unseen).any());
        assert!(!actions(ChannelRole::Broadcaster, Some("streamer"), "bob", &unseen).any());
        // Once a message shows them, the badges decide.
        let seen = known_user(&[(0, chat("1", "bob", "hi"))]);
        assert!(seen.seen);
        assert!(actions(ChannelRole::Moderator, Some("me"), "bob", &seen).any());
    }

    #[test]
    fn an_id_that_differs_from_the_messages_is_another_account() {
        assert!(same_account(Some("77"), "77"));
        assert!(!same_account(Some("77"), "78"));
        assert!(same_account(None, "78"));
    }

    #[test]
    fn timeouts_are_spoken_in_words() {
        assert_eq!(duration_words(1), "1 second");
        assert_eq!(duration_words(60), "1 minute");
        assert_eq!(duration_words(600), "10 minutes");
        assert_eq!(duration_words(3_600), "1 hour");
        assert_eq!(duration_words(86_400), "1 day");
        assert_eq!(duration_words(604_800), "1 week");
        assert_eq!(duration_words(1_209_600), "2 weeks");
        assert_eq!(duration_words(90), "90 seconds");
    }

    #[test]
    fn viewers_get_no_moderation() {
        let known = KnownUser::default();
        assert_eq!(
            actions(ChannelRole::Viewer, None, "bob", &known),
            CardActions::default()
        );
    }

    #[test]
    fn a_moderator_may_act_on_an_ordinary_chatter() {
        let known = KnownUser {
            seen: true,
            ..KnownUser::default()
        };
        let offered = actions(ChannelRole::Moderator, Some("me"), "bob", &known);
        assert_eq!(
            offered,
            CardActions {
                timeout: true,
                ban: true,
                unban: true
            }
        );
        assert!(offered.any());
    }

    #[test]
    fn nobody_moderates_themselves_or_the_broadcaster() {
        let known = KnownUser {
            seen: true,
            ..KnownUser::default()
        };
        assert!(!actions(ChannelRole::Moderator, Some("Me"), "me", &known).any());
        assert!(!actions(ChannelRole::Moderator, Some("me"), "Streamer", &known).any());
        assert!(
            !actions(
                ChannelRole::Broadcaster,
                Some("streamer"),
                "streamer",
                &known
            )
            .any()
        );
    }

    #[test]
    fn another_moderator_is_off_limits_except_to_the_broadcaster() {
        let known = KnownUser {
            seen: true,
            badges: vec![BadgeId::new("moderator", "1")],
            ..KnownUser::default()
        };
        assert!(!actions(ChannelRole::Moderator, Some("me"), "bob", &known).any());
        assert!(actions(ChannelRole::Broadcaster, Some("streamer"), "bob", &known).any());
        let staff = KnownUser {
            seen: true,
            badges: vec![BadgeId::new("staff", "1")],
            ..KnownUser::default()
        };
        assert!(!actions(ChannelRole::Moderator, Some("me"), "bob", &staff).any());
    }

    #[test]
    fn a_user_of_another_room_or_a_non_login_gets_nothing() {
        let foreign = KnownUser {
            only_shared_copies: true,
            ..KnownUser::default()
        };
        assert!(!actions(ChannelRole::Moderator, Some("me"), "bob", &foreign).any());
        assert!(
            !actions(
                ChannelRole::Moderator,
                Some("me"),
                "b!b",
                &KnownUser::default()
            )
            .any()
        );
    }

    #[test]
    fn rfc3339_timestamps_become_unix_seconds() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2015-03-14T09:26:53Z"), Some(1_426_325_213));
        // Fractions are dropped; an offset shifts the instant.
        assert_eq!(
            parse_rfc3339("2015-03-14T09:26:53.987654Z"),
            Some(1_426_325_213)
        );
        assert_eq!(
            parse_rfc3339("2015-03-14T11:26:53+02:00"),
            Some(1_426_325_213)
        );
        assert_eq!(
            parse_rfc3339("2015-03-14T04:26:53-05:00"),
            Some(1_426_325_213)
        );
        // Leap day, and a date before the epoch.
        assert_eq!(parse_rfc3339("2020-02-29T00:00:00Z"), Some(1_582_934_400));
        assert_eq!(parse_rfc3339("1969-12-31T23:59:59Z"), Some(-1));
        for bad in [
            "",
            "yesterday",
            "2015-03-14",
            "2015-13-14T00:00:00Z",
            "2015-03-14T25:00:00Z",
            "2015-03-14T09:26Z",
            "2015-03-14T09:26:53",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn dates_read_day_month_year() {
        assert_eq!(format_date(0), "1 Jan 1970");
        assert_eq!(format_date(1_426_325_213), "14 Mar 2015");
        assert_eq!(format_date(1_582_934_400), "29 Feb 2020");
        assert_eq!(format_date(-1), "31 Dec 1969");
    }

    fn at(text: &str) -> i64 {
        parse_rfc3339(text).unwrap()
    }

    #[test]
    fn a_leap_day_account_turns_a_year_on_the_first_of_march() {
        let created = at("2020-02-29T12:00:00Z");
        assert_eq!(format_date(created), "29 Feb 2020");
        let age = |now: &str| format_age(created, at(now));
        assert_eq!(age("2021-02-28T12:00:00Z"), "11 months");
        assert_eq!(age("2021-03-01T12:00:00Z"), "1 year");
        assert_eq!(age("2024-02-29T12:00:00Z"), "4 years");
        assert_eq!(age("2024-02-28T23:59:59Z"), "3 years");
    }

    #[test]
    fn an_account_age_is_its_largest_whole_calendar_unit() {
        let created = at("2015-03-14T09:26:53Z");
        let age = |now: &str| format_age(created, at(now));
        assert_eq!(age("2018-03-14T09:26:53Z"), "3 years");
        // One second short of the third birthday.
        assert_eq!(age("2018-03-14T09:26:52Z"), "2 years");
        assert_eq!(age("2016-03-15T00:00:00Z"), "1 year");
        assert_eq!(age("2015-08-14T09:26:53Z"), "5 months");
        assert_eq!(age("2015-04-14T09:26:53Z"), "1 month");
        assert_eq!(age("2015-04-13T09:26:53Z"), "30 days");
        assert_eq!(age("2015-03-15T09:26:53Z"), "1 day");
        assert_eq!(age("2015-03-14T14:26:53Z"), "5 hours");
        assert_eq!(age("2015-03-14T09:30:00Z"), "less than an hour");
        // A clock that is behind the account's creation.
        assert_eq!(age("2015-03-14T09:00:00Z"), "less than an hour");
        assert_eq!(age("2014-01-01T00:00:00Z"), "less than an hour");
    }
}
