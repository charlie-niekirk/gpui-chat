//! Chat filters (PR 5.7): which rows a channel's chat shows.
//!
//! [`Filters`] is what the find bar toggles (only one user's messages, only mentions and
//! highlights, hide bots, only links). [`visibility`] combines them with the ignores
//! ([`crate::ignore`]) into what is drawn for one message: a normal row, a collapsed
//! "1 ignored message" line, or nothing. Everything here is pure and cheap per message (no
//! allocation unless an ignore phrase has to read the text), so a filter change can be applied to a
//! whole scrollback at once.
//!
//! [`FilterConfig`] is the `[filters]` section of `settings.toml`: the list of bots *Hide bots*
//! hides.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::highlight::{login_problem, normalise_user};
use crate::ignore::Ignores;
use crate::message::{Fragment, Message, MessageKind};

/// The bots *Hide bots* starts with: the chat bots most channels run.
pub const DEFAULT_BOTS: &[&str] = &[
    "nightbot",
    "streamelements",
    "streamlabs",
    "moobot",
    "fossabot",
    "wizebot",
    "sery_bot",
];

/// Most bots the list may hold.
pub const MAX_BOTS: usize = 500;

/// `[filters]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    /// Logins *Hide bots* hides (an `@` in front and capitals are ignored).
    pub bots: Vec<String>,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            bots: DEFAULT_BOTS.iter().map(|bot| (*bot).to_owned()).collect(),
        }
    }
}

impl FilterConfig {
    /// The first entry that is not a login, with the reason.
    pub fn first_problem(&self) -> Option<String> {
        if self.bots.len() > MAX_BOTS {
            return Some(format!("more than {MAX_BOTS} bots"));
        }
        self.bots.iter().enumerate().find_map(|(index, bot)| {
            login_problem(&normalise_user(bot)).map(|reason| format!("bot {}: {reason}", index + 1))
        })
    }
}

/// The bots to hide, by lowercase login.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BotList(HashSet<String>);

impl BotList {
    /// The logins that are valid; the rest are skipped (the settings check reports them).
    pub fn new(config: &FilterConfig) -> Self {
        Self(
            config
                .bots
                .iter()
                .take(MAX_BOTS)
                .map(|bot| normalise_user(bot))
                .filter(|login| login_problem(login).is_none())
                .collect(),
        )
    }

    pub fn contains(&self, login: &str) -> bool {
        self.0.contains(&*crate::highlight::lowercase(login))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The filters the find bar toggles. All of them together: a message has to pass each one that is on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    /// Only messages from this user (lowercase login).
    pub from: Option<String>,
    /// Only messages that mention the user or that a highlight rule hit.
    pub mentions: bool,
    /// Only messages with a link.
    pub links: bool,
    /// Hide the messages of the bots in the list.
    pub hide_bots: bool,
}

impl Filters {
    /// Whether any filter is on.
    pub fn is_active(&self) -> bool {
        self.from.is_some() || self.mentions || self.links || self.hide_bots
    }

    /// How many filters are on.
    pub fn count(&self) -> usize {
        usize::from(self.from.is_some())
            + usize::from(self.mentions)
            + usize::from(self.links)
            + usize::from(self.hide_bots)
    }

    /// Switches the user filter to `login` (`None`, or a name that is not a login, turns it off).
    pub fn set_from(&mut self, login: Option<&str>) {
        self.from = login
            .map(normalise_user)
            .filter(|login| login_problem(login).is_none());
    }

    /// What the filters say, for the pill: `from alice · mentions · links · no bots`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(from) = &self.from {
            parts.push(format!("from {from}"));
        }
        if self.mentions {
            parts.push("mentions".to_owned());
        }
        if self.links {
            parts.push("links".to_owned());
        }
        if self.hide_bots {
            parts.push("no bots".to_owned());
        }
        parts.join(" · ")
    }

    /// Whether `message` passes every filter that is on. Lines nobody wrote (notices, system
    /// lines) and event cards pass only *Hide bots*: they are not from, a mention of, or a link
    /// by anyone, so any other filter hides them.
    pub fn matches(&self, message: &Message, bots: &BotList) -> bool {
        if !self.is_active() {
            return true;
        }
        let said = matches!(message.kind, MessageKind::Chat | MessageKind::Action);
        if self.hide_bots
            && said
            && message
                .author
                .as_ref()
                .is_some_and(|author| bots.contains(&author.login))
        {
            return false;
        }
        if let Some(from) = &self.from {
            let from_them = message
                .author
                .as_ref()
                .is_some_and(|author| *crate::highlight::lowercase(&author.login) == **from);
            if !from_them {
                return false;
            }
        }
        if self.mentions && !(message.flags.mention || message.flags.highlight.is_some()) {
            return false;
        }
        if self.links
            && !message
                .fragments
                .iter()
                .any(|fragment| matches!(fragment, Fragment::Link { .. }))
        {
            return false;
        }
        true
    }
}

/// What the chat draws for one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// A normal row.
    Shown,
    /// An ignored message drawn as one collapsed line (`ignores.show_collapsed`).
    Collapsed,
    /// Nothing.
    Hidden,
}

impl Visibility {
    pub fn is_visible(self) -> bool {
        self != Self::Hidden
    }
}

/// Whether and how `message` is drawn, given the ignores and the filters. A filter on hides
/// ignored messages even when they would be collapsed: the filtered list is a list of what matched,
/// and a placeholder for what did not is just noise.
pub fn visibility(
    message: &Message,
    ignores: &Ignores,
    filters: &Filters,
    bots: &BotList,
) -> Visibility {
    visibility_with(
        message,
        || ignores.is_ignored(message),
        ignores.show_collapsed(),
        filters,
        bots,
    )
}

/// [`visibility`] with the (expensive) ignore verdict supplied by the caller, who may have it
/// cached. The filters are checked first: they are a few comparisons, while `ignored` may read the
/// text and run regular expressions, and a message a filter hides needs no verdict at all.
pub fn visibility_with(
    message: &Message,
    ignored: impl FnOnce() -> bool,
    show_collapsed: bool,
    filters: &Filters,
    bots: &BotList,
) -> Visibility {
    if filters.is_active() {
        if !filters.matches(message, bots) || ignored() {
            return Visibility::Hidden;
        }
        return Visibility::Shown;
    }
    if ignored() {
        return if show_collapsed {
            Visibility::Collapsed
        } else {
            Visibility::Hidden
        };
    }
    Visibility::Shown
}

/// The ignore verdict of each message of a store, remembered by sequence number so that a filter
/// change (typing `from:`, toggling *Links*) never runs the ignore phrases and regular expressions
/// again over the whole scrollback. The owner forgets what is stale: everything when the ignores
/// change or the store is renumbered ([`IgnoreCache::clear`]), single messages that were replaced
/// ([`IgnoreCache::invalidate`]), and the evicted ones ([`IgnoreCache::trim`]).
#[derive(Debug, Default)]
pub struct IgnoreCache {
    first: u64,
    /// `0` not asked yet, `1` not ignored, `2` ignored; item `i` is message `first + i`.
    verdicts: std::collections::VecDeque<u8>,
}

impl IgnoreCache {
    /// Whether `message` (number `seq`) is ignored under `ignores`, asking only the first time.
    pub fn ignored(&mut self, seq: u64, message: &Message, ignores: &Ignores) -> bool {
        if seq < self.first {
            return ignores.is_ignored(message);
        }
        let at = (seq - self.first) as usize;
        if self.verdicts.len() <= at {
            self.verdicts.resize(at + 1, 0);
        }
        match self.verdicts[at] {
            0 => {
                let ignored = ignores.is_ignored(message);
                self.verdicts[at] = if ignored { 2 } else { 1 };
                ignored
            }
            known => known == 2,
        }
    }

    /// Forgets everything (the ignores changed, or the numbers did).
    pub fn clear(&mut self) {
        self.verdicts.clear();
    }

    /// Forgets the verdict of message `seq` (it was replaced by another message).
    pub fn invalidate(&mut self, seq: u64) {
        if let Some(at) = seq.checked_sub(self.first)
            && let Some(slot) = self.verdicts.get_mut(at as usize)
        {
            *slot = 0;
        }
    }

    /// Drops the verdicts of messages before `first` (evicted from the store).
    pub fn trim(&mut self, first: u64) {
        while self.first < first && !self.verdicts.is_empty() {
            self.verdicts.pop_front();
            self.first += 1;
        }
        self.first = self.first.max(first);
    }

    /// How many verdicts are remembered (for tests).
    pub fn len(&self) -> usize {
        self.verdicts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.verdicts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::HighlightColor;
    use crate::ignore::IgnoreConfig;
    use crate::message::{Author, Timestamp};

    fn said(login: &str, text: &str) -> Message {
        Message::chat(
            "id",
            Timestamp(0),
            Author::new(login, login),
            vec![Fragment::Text(text.into())],
        )
    }

    fn with_link(login: &str) -> Message {
        Message::chat(
            "id",
            Timestamp(0),
            Author::new(login, login),
            vec![
                Fragment::Text("look ".into()),
                Fragment::Link {
                    text: "example.com".into(),
                    url: "https://example.com".into(),
                },
            ],
        )
    }

    fn bots() -> BotList {
        BotList::new(&FilterConfig::default())
    }

    #[test]
    fn no_filter_passes_everything() {
        let filters = Filters::default();
        assert!(!filters.is_active());
        let mut system = said("x", "y");
        system.kind = MessageKind::System;
        system.author = None;
        assert!(filters.matches(&system, &bots()));
        assert_eq!(filters.describe(), "");
    }

    #[test]
    fn from_keeps_one_users_messages_whatever_the_case() {
        let mut filters = Filters::default();
        filters.set_from(Some("@Alice"));
        assert_eq!(filters.from.as_deref(), Some("alice"));
        assert!(filters.matches(&said("ALICE", "hi"), &bots()));
        assert!(!filters.matches(&said("bob", "hi"), &bots()));
        filters.set_from(Some("not a login"));
        assert_eq!(filters.from, None);
        filters.set_from(Some(""));
        assert_eq!(filters.from, None);
    }

    #[test]
    fn mentions_keeps_mentions_and_highlights() {
        let filters = Filters {
            mentions: true,
            ..Filters::default()
        };
        let mut mention = said("a", "hey viewer");
        mention.flags.mention = true;
        let mut highlight = said("a", "giveaway");
        highlight.flags.highlight = Some(HighlightColor::Green);
        assert!(filters.matches(&mention, &bots()));
        assert!(filters.matches(&highlight, &bots()));
        assert!(!filters.matches(&said("a", "nothing"), &bots()));
    }

    #[test]
    fn links_keeps_messages_with_a_link() {
        let filters = Filters {
            links: true,
            ..Filters::default()
        };
        assert!(filters.matches(&with_link("a"), &bots()));
        assert!(!filters.matches(&said("a", "example.com"), &bots()));
    }

    #[test]
    fn hide_bots_hides_the_listed_bots_chat_but_not_events() {
        let filters = Filters {
            hide_bots: true,
            ..Filters::default()
        };
        assert!(!filters.matches(&said("NightBot", "!rules"), &bots()));
        assert!(filters.matches(&said("alice", "hi"), &bots()));
        let mut event = said("nightbot", "");
        event.kind = MessageKind::UserNotice;
        assert!(
            filters.matches(&event, &bots()),
            "only what bots *say* is hidden"
        );
        let custom = BotList::new(&FilterConfig {
            bots: vec!["@MyBot".into(), "bad login".into()],
        });
        assert!(custom.contains("mybot"));
        assert!(!custom.contains("nightbot"), "the list is the user's own");
    }

    #[test]
    fn filters_combine_and_other_lines_fail_all_but_hide_bots() {
        let mut filters = Filters {
            links: true,
            hide_bots: true,
            ..Filters::default()
        };
        filters.set_from(Some("alice"));
        assert_eq!(filters.count(), 3);
        assert_eq!(filters.describe(), "from alice · links · no bots");
        assert!(filters.matches(&with_link("alice"), &bots()));
        assert!(!filters.matches(&with_link("bob"), &bots()));
        assert!(!filters.matches(&said("alice", "no link"), &bots()));
        let mut system = said("x", "y");
        system.kind = MessageKind::System;
        system.author = None;
        assert!(!filters.matches(&system, &bots()));
        let only_bots = Filters {
            hide_bots: true,
            ..Filters::default()
        };
        assert!(only_bots.matches(&system, &bots()));
    }

    #[test]
    fn ignored_messages_are_hidden_or_collapsed_and_filters_decide_the_rest() {
        let config = |show| IgnoreConfig {
            users: vec!["spam".into()],
            show_collapsed: show,
            ..IgnoreConfig::default()
        };
        let (hide, _) = Ignores::compile(&config(false));
        let (collapse, _) = Ignores::compile(&config(true));
        let none = Filters::default();
        let spam = said("spam", "x");
        let fine = said("alice", "x");
        assert_eq!(visibility(&spam, &hide, &none, &bots()), Visibility::Hidden);
        assert_eq!(
            visibility(&spam, &collapse, &none, &bots()),
            Visibility::Collapsed
        );
        assert_eq!(
            visibility(&fine, &collapse, &none, &bots()),
            Visibility::Shown
        );
        let links = Filters {
            links: true,
            ..Filters::default()
        };
        assert_eq!(
            visibility(&spam, &collapse, &links, &bots()),
            Visibility::Hidden,
            "no placeholders in a filtered list"
        );
        assert_eq!(
            visibility(&fine, &collapse, &links, &bots()),
            Visibility::Hidden
        );
        assert_eq!(
            visibility(&with_link("alice"), &collapse, &links, &bots()),
            Visibility::Shown
        );
        assert!(Visibility::Collapsed.is_visible() && !Visibility::Hidden.is_visible());
    }

    #[test]
    fn the_bot_list_is_checked() {
        assert!(FilterConfig::default().first_problem().is_none());
        let bad = FilterConfig {
            bots: vec!["fine".into(), "no spaces".into()],
        };
        assert!(bad.first_problem().unwrap().starts_with("bot 2: "));
    }

    #[test]
    fn the_ignore_cache_asks_once_and_forgets_what_is_stale() {
        let (ignores, _) = Ignores::compile(&IgnoreConfig {
            users: vec!["spam".into()],
            ..IgnoreConfig::default()
        });
        let mut cache = IgnoreCache::default();
        let spam = said("spam", "x");
        let fine = said("alice", "x");
        assert!(cache.ignored(5, &spam, &ignores));
        assert!(!cache.ignored(6, &fine, &ignores));
        // Remembered: the answer does not come from the message this time.
        assert!(cache.ignored(5, &fine, &ignores));
        cache.invalidate(5);
        assert!(!cache.ignored(5, &fine, &ignores));
        // A message before the first remembered one is answered but not remembered.
        cache.trim(6);
        assert!(!cache.ignored(2, &fine, &ignores));
        assert!(cache.ignored(2, &spam, &ignores));
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn filters_are_checked_before_the_ignore_verdict_is_asked() {
        let asked = std::cell::Cell::new(0);
        let ask = || {
            asked.set(asked.get() + 1);
            false
        };
        let links = Filters {
            links: true,
            ..Filters::default()
        };
        let bots = bots();
        assert_eq!(
            visibility_with(&said("a", "plain"), ask, false, &links, &bots),
            Visibility::Hidden
        );
        assert_eq!(
            asked.get(),
            0,
            "a message the filter hides needs no verdict"
        );
        assert_eq!(
            visibility_with(&with_link("a"), ask, false, &links, &bots),
            Visibility::Shown
        );
        assert_eq!(asked.get(), 1);
        assert_eq!(
            visibility_with(&said("a", "x"), || true, true, &Filters::default(), &bots),
            Visibility::Collapsed
        );
    }
}
