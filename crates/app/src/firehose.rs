//! `--firehose <msgs/sec>`: a synthetic, deterministic chat source for offline runs.
//!
//! [`Generator`] makes a realistic stream of [`ChatEvent::Message`]s: short and long lines,
//! emoji (including ZWJ sequences and flags), non-ASCII text, `@mentions`, links, very long
//! words, users with and without name colours, `/me` actions and Twitch-style emote ranges
//! (generated as an IRC `emotes` tag and run through the real tokenizer, like a backend would).
//! The stream depends only on the seed and the message index, never on the clock or on timing.
//!
//! [`run`] paces it on the tokio runtime. Messages are *due* at `i / rate` seconds after the
//! start; every wake-up sends everything that is due by then in one go (a batch per tick rather
//! than a timer per message) and the next wake-up targets the next due message, never earlier
//! than [`MIN_TICK`] ahead. Because the count is derived from elapsed time, timer granularity and
//! scheduling jitter delay a message by at most [`MIN_TICK`] plus the timer's own 1 ms rounding
//! (2 ms worst case, measured in the pacing tests) but never change the rate and never accumulate
//! drift. The UI applies events once per frame (8 ms or more), so that is below what it can see.
//!
//! Each wake-up is capped ([`WorkLimits`]): a rate the generator cannot sustain makes it fall
//! behind, reported as `behind` on the HUD, instead of monopolising a tokio worker.
//!
//! Sending uses `try_send`: a real-time source cannot wait for a slow UI, so events that do not
//! fit the bridge queue are discarded and counted (`dropped` on the HUD).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui_chat_core::{
    Author, BadgeId, ChannelId, ChatEvent, Message, MessageKind, Rgb, Timestamp, Tokenizer,
    twitch_emote_ranges,
};
use tokio::time::{Instant, sleep_until};

use crate::bridge::{BridgeSender, TrySendError};
use crate::diagnostics::IngestCounters;

/// Shortest pause between two wake-ups. Messages due within it are sent together.
pub const MIN_TICK: Duration = Duration::from_millis(1);

/// Twitch rejects messages longer than this many characters.
const MAX_MESSAGE_CHARS: usize = 500;

/// The viewer the synthetic chat sometimes mentions (drives `flags.mention`).
pub const OWN_LOGIN: &str = "viewer";

const USER_COUNT: usize = 400;

pub struct FirehoseConfig {
    /// Messages per second across all channels.
    pub rate: f64,
    pub seed: u64,
    pub channels: Vec<ChannelId>,
    /// Timestamp (Unix ms) of message 0; later messages are spaced `1000 / rate` ms apart.
    pub start_ts_ms: i64,
}

/// `channel-1 .. channel-n` style names for the synthetic channels (`firehose` when there is one).
pub fn channel_names(count: usize) -> Vec<ChannelId> {
    if count <= 1 {
        return vec![ChannelId::new("firehose")];
    }
    (1..=count)
        .map(|n| ChannelId::new(&format!("firehose{n}")))
        .collect()
}

// ---- random numbers ----------------------------------------------------------------------

/// SplitMix64: tiny, fast, and the same on every platform, which is all this needs.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0.0..1.0`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// Uniform in `0..n` (`n > 0`).
    fn below(&mut self, n: usize) -> usize {
        ((self.unit() * n as f64) as usize).min(n - 1)
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

// ---- vocabulary --------------------------------------------------------------------------

const WORDS: &[&str] = &[
    "the", "this", "that", "game", "chat", "stream", "play", "good", "bad", "nice", "wow", "lol",
    "gg", "clip", "that", "was", "so", "just", "really", "can", "you", "see", "what", "happened",
    "he", "she", "they", "we", "going", "to", "win", "lose", "again", "first", "last", "boss",
    "run", "speedrun", "world", "record", "pb", "split", "gold", "route", "skip", "glitch", "luck",
    "no", "way", "actually", "insane", "clean", "sweaty", "tilted", "cracked", "hype", "poggers",
    "monkaS", "copium", "based", "cringe", "W", "L", "ratio", "mods", "ban", "timeout", "sub",
    "gifted", "raid", "hello", "hi", "bye", "love", "hate", "music", "song", "volume", "mic",
    "lag", "fps", "ping", "server", "patch", "nerf", "buff", "meta", "build", "item", "level",
    "death", "again", "never", "always", "maybe", "why", "how", "when", "who", "where", "ok",
];

/// Twitch emotes (id, name) with their real ids.
const EMOTES: &[(&str, &str)] = &[
    ("25", "Kappa"),
    ("425618", "LUL"),
    ("354", "4Head"),
    ("86", "BibleThump"),
    ("41", "Kreygasm"),
    ("245", "ResidentSleeper"),
    ("64138", "SeemsGood"),
    ("65", "FrankerZ"),
    ("33", "DansGame"),
    ("1902", "Keepo"),
    ("28087", "WutFace"),
    ("81274", "VoHiYo"),
    ("120232", "TriHard"),
    ("114836", "Jebaited"),
];

const EMOJI: &[&str] = &[
    "😂",
    "🔥",
    "👍",
    "❤️",
    "😭",
    "🤔",
    "🎉",
    "💀",
    "👀",
    "🙏",
    "👨‍👩‍👧‍👦",
    "🏳️‍🌈",
    "👍🏽",
    "🇯🇵",
    "🇬🇧",
    "1️⃣",
    "🧑‍💻",
];

const NON_ASCII_WORDS: &[&str] = &[
    "こんにちは",
    "すごい",
    "やばい",
    "ありがとう",
    "привет",
    "красиво",
    "спасибо",
    "你好",
    "太强了",
    "안녕하세요",
    "café",
    "naïve",
    "señor",
    "Łódź",
    "größe",
    "ελληνικά",
    "שלום",
    "مرحبا",
];

const LINKS: &[&str] = &[
    "https://www.twitch.tv/videos/2113456789",
    "https://clips.twitch.tv/FamousSpottedFlyPoooound-abc123XYZ",
    "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
    "http://example.com/a/very/long/path/that/keeps/going?with=query&and=more#fragment",
    "youtube.com/watch?v=abcdefghijk",
    "twitch.tv/directory",
    "github.com/zed-industries/zed",
    "https://store.steampowered.com/app/1245620/ELDEN_RING/",
    "reddit.com/r/LivestreamFail",
];

const NAME_PARTS: &[&str] = &[
    "swift", "lazy", "dark", "pixel", "turbo", "salty", "cozy", "mega", "tiny", "wild", "frost",
    "neon", "rusty", "silent", "lucky", "grumpy", "happy", "sleepy", "chaos", "ninja", "panda",
    "wolf", "fox", "ghost", "wizard", "knight", "robot", "toast", "noodle", "potato",
];

const UNICODE_DISPLAY_NAMES: &[(&str, &str)] = &[
    ("hikaru_jp", "ひかる"),
    ("lukasz_pl", "Łukasz"),
    ("sasha_ru", "Саша"),
    ("minjun_kr", "민준"),
    ("renee_fr", "Renée"),
];

const PUNCTUATION: &[&str] = &["!", "?", "...", ",", "!!", "?!", "."];

// ---- users and messages ------------------------------------------------------------------

struct User {
    author: Author,
    /// How a message addresses this user (`@Name`).
    mention: String,
}

fn make_users(rng: &mut Rng) -> Vec<User> {
    let mut users = Vec::with_capacity(USER_COUNT);
    for (login, display) in UNICODE_DISPLAY_NAMES {
        users.push(make_user(rng, (*login).to_owned(), (*display).to_owned()));
    }
    while users.len() < USER_COUNT {
        let login = format!(
            "{}_{}{}",
            rng.pick(NAME_PARTS),
            rng.pick(NAME_PARTS),
            rng.range(1, 9999)
        );
        // Twitch display names keep the user's own capitalisation.
        let display = match rng.below(3) {
            0 => login.clone(),
            1 => capitalise(&login),
            _ => login
                .split('_')
                .map(capitalise)
                .collect::<Vec<_>>()
                .join("_"),
        };
        users.push(make_user(rng, login, display));
    }
    users
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn make_user(rng: &mut Rng, login: String, display_name: String) -> User {
    let mut author = Author::new(login.clone(), display_name.clone());
    author.user_id = Some((10_000_000 + rng.range(0, 900_000_000)).to_string());
    // Roughly one in seven users never picked a colour.
    if !rng.chance(0.14) {
        author.color = Some(random_colour(rng));
    }
    let roll = rng.unit();
    if roll < 0.002 {
        author.badges.push(BadgeId::new("vip", "1"));
    } else if roll < 0.012 {
        author.badges.push(BadgeId::new("moderator", "1"));
    } else if roll < 0.15 {
        let months = *rng.pick(&["0", "1", "3", "6", "12", "24"]);
        author.badges.push(BadgeId::new("subscriber", months));
    }
    if rng.chance(0.05) {
        author.badges.push(BadgeId::new("premium", "1"));
    }
    User {
        author,
        mention: format!("@{display_name}"),
    }
}

/// A fully saturated-ish colour like Twitch's colour picker produces.
fn random_colour(rng: &mut Rng) -> Rgb {
    let hue = rng.unit() * 6.0;
    let sector = hue as u32 % 6;
    let f = hue - hue.floor();
    let (high, low) = (255.0, 90.0 + rng.unit() * 80.0);
    let (rising, falling) = (low + (high - low) * f, high - (high - low) * f);
    let (r, g, b) = match sector {
        0 => (high, rising, low),
        1 => (falling, high, low),
        2 => (low, high, rising),
        3 => (low, falling, high),
        4 => (rising, low, high),
        _ => (high, low, falling),
    };
    Rgb::new(r as u8, g as u8, b as u8)
}

/// Builds a message body and the matching IRC `emotes` tag (ranges count code points).
#[derive(Default)]
struct Body {
    text: String,
    chars: usize,
    /// (emote id, inclusive code point ranges), in order of first appearance.
    emotes: Vec<(&'static str, Vec<(usize, usize)>)>,
}

impl Body {
    fn space(&mut self) {
        if !self.text.is_empty() {
            self.text.push(' ');
            self.chars += 1;
        }
    }

    fn word(&mut self, word: &str) {
        self.space();
        self.text.push_str(word);
        self.chars += word.chars().count();
    }

    fn emote(&mut self, id: &'static str, name: &str) {
        self.space();
        let start = self.chars;
        self.text.push_str(name);
        self.chars += name.chars().count();
        let range = (start, self.chars - 1);
        match self.emotes.iter_mut().find(|(known, _)| *known == id) {
            Some((_, ranges)) => ranges.push(range),
            None => self.emotes.push((id, vec![range])),
        }
    }

    fn emotes_tag(&self) -> String {
        self.emotes
            .iter()
            .map(|(id, ranges)| {
                let ranges: Vec<String> = ranges
                    .iter()
                    .map(|(start, end)| format!("{start}-{end}"))
                    .collect();
                format!("{id}:{}", ranges.join(","))
            })
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// Deterministic stream of chat events. Message `i` depends only on the seed and `i`.
pub struct Generator {
    rng: Rng,
    users: Vec<User>,
    channels: Vec<ChannelId>,
    index: u64,
    start_ts_ms: i64,
    ms_per_message: f64,
}

impl Generator {
    pub fn new(config: &FirehoseConfig) -> Self {
        let mut rng = Rng::new(config.seed);
        let users = make_users(&mut rng);
        let channels = if config.channels.is_empty() {
            vec![ChannelId::new("firehose")]
        } else {
            config.channels.clone()
        };
        Self {
            rng,
            users,
            channels,
            index: 0,
            start_ts_ms: config.start_ts_ms,
            ms_per_message: 1000.0 / config.rate,
        }
    }

    /// How many events have been produced.
    pub fn produced(&self) -> u64 {
        self.index
    }

    pub fn next_event(&mut self) -> ChatEvent {
        let index = self.index;
        self.index += 1;

        // Busy users chat more: squaring a uniform draw skews towards the front of the list.
        let skew = self.rng.unit();
        let user_index = ((skew * skew) * self.users.len() as f64) as usize % self.users.len();
        let channel = {
            // A few channels are busier than the rest, the same skew.
            let skew = self.rng.unit();
            let i = ((skew * skew) * self.channels.len() as f64) as usize % self.channels.len();
            self.channels[i].clone()
        };

        let body = self.body(user_index);
        let tokenizer = Tokenizer::new().with_own_login(OWN_LOGIN);
        let ranges = twitch_emote_ranges(&body.emotes_tag(), &body.text);
        let tokenized = tokenizer.tokenize(&body.text, &ranges);

        let timestamp = Timestamp::from_unix_millis(
            self.start_ts_ms + (index as f64 * self.ms_per_message) as i64,
        );
        let author = self.users[user_index].author.clone();
        let mut message = Message::chat(
            format!("firehose-{index:012}"),
            timestamp,
            author,
            tokenized.fragments,
        );
        message.flags.mention = tokenized.mentions_self;
        message.flags.first_message = self.rng.chance(0.004);
        message.flags.highlighted = self.rng.chance(0.003);
        if self.rng.chance(0.03) {
            message.kind = MessageKind::Action;
        }
        ChatEvent::Message {
            channel,
            message: Arc::new(message),
        }
    }

    fn body(&mut self, author_index: usize) -> Body {
        let mut body = Body::default();
        // Emote spam: a line of nothing but emotes.
        if self.rng.chance(0.07) {
            for _ in 0..self.rng.range(1, 5) {
                let (id, name) = *self.rng.pick(EMOTES);
                body.emote(id, name);
            }
            return body;
        }
        let roll = self.rng.unit();
        let words = match roll {
            r if r < 0.55 => self.rng.range(1, 5),
            r if r < 0.85 => self.rng.range(6, 15),
            r if r < 0.97 => self.rng.range(16, 40),
            _ => self.rng.range(41, 110),
        };
        let shouting = self.rng.chance(0.03);
        for _ in 0..words {
            if body.chars > MAX_MESSAGE_CHARS - 100 {
                break;
            }
            self.push_piece(&mut body, author_index, shouting);
        }
        // Never exceed Twitch's limit, even with the last long piece.
        if body.chars > MAX_MESSAGE_CHARS {
            return self.fallback_body();
        }
        body
    }

    fn fallback_body(&mut self) -> Body {
        let mut body = Body::default();
        body.word("too long, shortened");
        body
    }

    fn push_piece(&mut self, body: &mut Body, author_index: usize, shouting: bool) {
        let roll = self.rng.unit();
        if roll < 0.13 {
            let (id, name) = *self.rng.pick(EMOTES);
            body.emote(id, name);
        } else if roll < 0.17 {
            body.word(self.rng.pick(EMOJI));
        } else if roll < 0.20 {
            body.word(self.rng.pick(NON_ASCII_WORDS));
        } else if roll < 0.24 {
            body.word(&self.mention(author_index));
        } else if roll < 0.252 {
            body.word(self.rng.pick(LINKS));
        } else if roll < 0.260 {
            // A very long unbroken token: pasted hashes, keyboard mashing, ASCII art.
            let len = self.rng.range(25, 90);
            let word: String = (0..len)
                .map(|_| {
                    let c = self.rng.below(36) as u8;
                    char::from(if c < 10 { b'0' + c } else { b'a' + c - 10 })
                })
                .collect();
            body.word(&word);
        } else {
            let word = *self.rng.pick(WORDS);
            let mut word = if shouting {
                word.to_uppercase()
            } else {
                word.to_owned()
            };
            if self.rng.chance(0.08) {
                word.push_str(self.rng.pick(PUNCTUATION));
            }
            body.word(&word);
        }
    }

    /// `@someone`, now and then the viewer (so the mention flag fires).
    fn mention(&mut self, author_index: usize) -> String {
        if self.rng.chance(0.06) {
            return format!("@{OWN_LOGIN}");
        }
        let mut target = self.rng.below(self.users.len());
        if target == author_index {
            target = (target + 1) % self.users.len();
        }
        self.users[target].mention.clone()
    }
}

// ---- pacing ------------------------------------------------------------------------------

/// How many messages are due `elapsed` after the start.
pub fn due_count(elapsed: Duration, rate: f64) -> u64 {
    (elapsed.as_secs_f64() * rate).floor() as u64
}

/// When message number `count` is due, relative to the start.
fn due_time(count: u64, rate: f64) -> Duration {
    Duration::from_secs_f64(count as f64 / rate)
}

/// Caps on the work one wake-up may do, so a rate the generator cannot sustain makes it fall
/// behind (and say so, as `behind` on the HUD) instead of monopolising a tokio worker.
#[derive(Debug, Clone, Copy)]
pub struct WorkLimits {
    /// Most events produced per wake-up.
    pub max_messages: usize,
    /// Most wall-clock time spent producing per wake-up (checked every [`CLOCK_CHECK_EVERY`]
    /// events). The source then sleeps at least [`MIN_TICK`], so it uses at most about half a
    /// worker at the default.
    pub max_time: Duration,
}

impl Default for WorkLimits {
    fn default() -> Self {
        Self {
            max_messages: 4096,
            max_time: Duration::from_millis(1),
        }
    }
}

/// How many events are produced between two looks at the clock.
const CLOCK_CHECK_EVERY: usize = 16;

/// Produces events at `config.rate` until the UI side of the bridge is gone.
pub async fn run(
    sender: BridgeSender<ChatEvent>,
    config: FirehoseConfig,
    counters: Arc<IngestCounters>,
) {
    run_with(sender, config, counters, WorkLimits::default()).await;
}

async fn run_with(
    sender: BridgeSender<ChatEvent>,
    config: FirehoseConfig,
    counters: Arc<IngestCounters>,
    limits: WorkLimits,
) {
    let rate = config.rate;
    let mut generator = Generator::new(&config);
    let start = Instant::now();
    loop {
        let now = Instant::now();
        let due = due_count(now - start, rate);
        // Real elapsed time, not tokio's (which stands still under paused test time).
        let work_started = std::time::Instant::now();
        let mut done = 0usize;
        while generator.produced() < due {
            if done >= limits.max_messages
                || (done.is_multiple_of(CLOCK_CHECK_EVERY)
                    && done > 0
                    && work_started.elapsed() >= limits.max_time)
            {
                break;
            }
            match sender.try_send(generator.next_event()) {
                Ok(()) => {
                    counters.sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    counters.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Closed(_)) => return,
            }
            done += 1;
        }
        counters
            .behind
            .store(due.saturating_sub(generator.produced()), Ordering::Relaxed);
        // The sleep also yields to other tasks when the cap cut this wake-up short.
        let next_due = start + due_time(generator.produced(), rate);
        sleep_until(next_due.max(now + MIN_TICK)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge;
    use gpui_chat_core::Fragment;

    fn config(rate: f64, seed: u64) -> FirehoseConfig {
        FirehoseConfig {
            rate,
            seed,
            channels: channel_names(3),
            start_ts_ms: 1_700_000_000_000,
        }
    }

    fn events(seed: u64, count: usize) -> Vec<ChatEvent> {
        let mut generator = Generator::new(&config(1000.0, seed));
        (0..count).map(|_| generator.next_event()).collect()
    }

    fn messages(events: &[ChatEvent]) -> impl Iterator<Item = &Message> {
        events.iter().filter_map(|event| match event {
            ChatEvent::Message { message, .. } => Some(&**message),
            _ => None,
        })
    }

    #[test]
    fn same_seed_same_stream_different_seed_different_stream() {
        assert_eq!(events(42, 500), events(42, 500));
        assert_ne!(events(42, 500), events(43, 500));
    }

    #[test]
    fn the_stream_does_not_depend_on_how_it_is_consumed() {
        let mut one = Generator::new(&config(1000.0, 9));
        let mut two = Generator::new(&config(1000.0, 9));
        let a: Vec<_> = (0..300).map(|_| one.next_event()).collect();
        let mut b = Vec::new();
        for chunk in [1, 7, 100, 192] {
            b.extend((0..chunk).map(|_| two.next_event()));
        }
        assert_eq!(a, b);
    }

    #[test]
    fn ids_are_unique_timestamps_follow_the_rate_and_channels_are_spread() {
        let evs = events(1, 2000);
        let mut ids: Vec<_> = messages(&evs).map(|m| m.id.as_str().to_owned()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 2000);

        let stamps: Vec<i64> = messages(&evs).map(|m| m.timestamp.unix_millis()).collect();
        assert_eq!(stamps[0], 1_700_000_000_000);
        assert_eq!(
            stamps[1000], 1_700_000_001_000,
            "1000/s: message 1000 is 1 s in"
        );
        assert!(stamps.windows(2).all(|w| w[0] <= w[1]));

        let channels: std::collections::HashSet<_> =
            evs.iter().filter_map(|e| e.channel().cloned()).collect();
        assert_eq!(channels.len(), 3);
    }

    #[test]
    fn the_mix_is_realistic() {
        let evs = events(5, 8000);
        let mut seen = std::collections::HashMap::<&str, usize>::new();
        let mut note = |what| *seen.entry(what).or_default() += 1;
        let mut lengths = Vec::new();
        for message in messages(&evs) {
            let text = message.plain_text();
            let chars = text.chars().count();
            assert!(chars <= MAX_MESSAGE_CHARS, "{chars} chars");
            assert!(!text.is_empty());
            lengths.push(chars);
            if !text.is_ascii() {
                note("non-ascii");
            }
            if text
                .split(' ')
                .any(|w| w.chars().count() >= 25 && !w.contains('/'))
            {
                note("long word");
            }
            if message.author.as_ref().is_some_and(|a| a.color.is_none()) {
                note("no colour");
            }
            if message.author.as_ref().is_some_and(|a| a.color.is_some()) {
                note("colour");
            }
            if message
                .author
                .as_ref()
                .is_some_and(|a| !a.badges.is_empty())
            {
                note("badges");
            }
            if message.kind == MessageKind::Action {
                note("action");
            }
            if message.flags.mention {
                note("mentions viewer");
            }
            for fragment in &message.fragments {
                match fragment {
                    Fragment::Emote { .. } => note("emote"),
                    Fragment::Emoji { .. } => note("emoji"),
                    Fragment::Mention { .. } => note("mention"),
                    Fragment::Link { .. } => note("link"),
                    _ => {}
                }
            }
        }
        for what in [
            "non-ascii",
            "long word",
            "no colour",
            "colour",
            "badges",
            "action",
            "mentions viewer",
            "emote",
            "emoji",
            "mention",
            "link",
        ] {
            assert!(
                seen.get(what).copied().unwrap_or(0) >= 20,
                "too few `{what}`: {seen:?}"
            );
        }
        lengths.sort_unstable();
        let median = lengths[lengths.len() / 2];
        assert!((4..=60).contains(&median), "median length {median}");
        assert!(*lengths.last().unwrap() > 200, "some long lines");
    }

    #[test]
    fn emote_ranges_cover_the_emote_text() {
        // The tag is built from code point indices; after tokenizing, every emote fragment
        // must still be named by the text it covers, including after multi-byte characters.
        let evs = events(11, 3000);
        let mut checked = 0;
        for message in messages(&evs) {
            for fragment in &message.fragments {
                if let Fragment::Emote { emote, .. } = fragment {
                    assert!(
                        EMOTES.iter().any(|(_, name)| **name == *emote.name),
                        "unexpected emote name {:?}",
                        emote.name
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 500);
    }

    #[test]
    fn due_count_is_exact_arithmetic_on_elapsed_time() {
        assert_eq!(due_count(Duration::ZERO, 1000.0), 0);
        assert_eq!(due_count(Duration::from_millis(1), 1000.0), 1);
        assert_eq!(due_count(Duration::from_micros(999), 1000.0), 0);
        assert_eq!(due_count(Duration::from_secs(60), 1000.0), 60_000);
        assert_eq!(due_count(Duration::from_secs(3600), 200.0), 720_000);
        assert_eq!(due_count(Duration::from_secs(10), 0.5), 5);
    }

    /// Runs the source for `seconds` of (virtual) time against an unbounded-enough queue and
    /// returns when each event was received, relative to the start.
    async fn run_for(
        rate: f64,
        seconds: u64,
        capacity: usize,
    ) -> (Vec<Duration>, Arc<IngestCounters>) {
        let (tx, mut rx) = bridge::channel::<ChatEvent>(capacity, usize::MAX);
        let counters = Arc::new(IngestCounters::default());
        let start = Instant::now();
        let source = tokio::spawn(run(tx, config(rate, 1), counters.clone()));
        let mut arrivals = Vec::new();
        let deadline = start + Duration::from_secs(seconds);
        loop {
            tokio::select! {
                () = sleep_until(deadline) => break,
                first = rx.first() => {
                    let Some(first) = first else { break };
                    let now = Instant::now() - start;
                    // Everything queued came in the same tick.
                    let batch = rx.drain(first);
                    arrivals.extend(std::iter::repeat_n(now, batch.len()));
                }
            }
        }
        source.abort();
        (arrivals, counters)
    }

    #[tokio::test(start_paused = true)]
    async fn holds_the_rate_at_1000_per_second_without_drift() {
        let (arrivals, counters) = run_for(1000.0, 10, 100_000).await;
        // 10 s at 1000/s: exactly 10_000 are due by the deadline (give or take the last tick).
        assert!(
            (9_998..=10_001).contains(&arrivals.len()),
            "{} events",
            arrivals.len()
        );
        assert_eq!(counters.dropped(), 0);

        // Each message arrives when it was due: never early, and late by at most MIN_TICK plus
        // the timer's own millisecond rounding.
        for (i, arrived) in arrivals.iter().enumerate() {
            let due = due_time(i as u64, 1000.0);
            assert!(
                *arrived + Duration::from_micros(1) >= due,
                "message {i} early"
            );
            assert!(
                *arrived <= due + MIN_TICK + Duration::from_millis(1),
                "message {i} late: due {due:?}, arrived {arrived:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn holds_the_rate_at_other_speeds() {
        for (rate, seconds, expected) in [(200.0, 10, 2000), (5000.0, 4, 20_000), (10.0, 20, 200)] {
            let (arrivals, _) = run_for(rate, seconds, 100_000).await;
            // Events due in the last tick or two before the deadline have not been sent yet.
            let lag_allowance = (rate * 0.003).ceil() as usize + 2;
            let got = arrivals.len();
            assert!(
                got + lag_allowance >= expected && got <= expected + 2,
                "{rate}/s for {seconds}s gave {got}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_consumer_makes_the_source_drop_and_count_not_wait() {
        let (tx, mut rx) = bridge::channel::<ChatEvent>(16, 16);
        let counters = Arc::new(IngestCounters::default());
        let source = tokio::spawn(run(tx, config(1000.0, 1), counters.clone()));
        sleep_until(Instant::now() + Duration::from_secs(1)).await;
        assert_eq!(counters.sent(), 16, "the queue took 16");
        assert!(counters.dropped() >= 980, "{} dropped", counters.dropped());
        // The source did not stall: it kept counting through the second.
        assert!(counters.sent() + counters.dropped() >= 998);
        // The queued events are the oldest ones, in order.
        let first = rx.first().await.unwrap();
        let ChatEvent::Message { message, .. } = first else {
            panic!("expected a message");
        };
        assert_eq!(message.id.as_str(), "firehose-000000000000");
        source.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn an_unsustainable_rate_falls_behind_and_reports_it_instead_of_hogging_the_worker() {
        let (tx, mut rx) = bridge::channel::<ChatEvent>(1_000_000, usize::MAX);
        let counters = Arc::new(IngestCounters::default());
        let limits = WorkLimits {
            max_messages: 10,
            max_time: Duration::from_secs(60),
        };
        // 100 000/s asked for, but at most 10 per wake-up and one wake-up per millisecond.
        let source = tokio::spawn(run_with(tx, config(100_000.0, 1), counters.clone(), limits));
        sleep_until(Instant::now() + Duration::from_secs(1)).await;
        let produced = counters.sent() + counters.dropped();
        assert!(
            (9_000..=10_100).contains(&produced),
            "capped at ~10 per ms, got {produced}"
        );
        // Due by the clock: ~100 000. The shortfall is exactly what was not produced.
        let behind = counters.behind();
        assert!((88_000..=91_500).contains(&behind), "behind by {behind}");
        assert!(produced + behind >= 99_000, "{produced} + {behind}");
        // What was produced is still the start of the same deterministic stream.
        let first = rx.first().await.unwrap();
        let ChatEvent::Message { message, .. } = first else {
            panic!("expected a message");
        };
        assert_eq!(message.id.as_str(), "firehose-000000000000");
        source.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_sustainable_rate_is_never_behind() {
        let (_, counters) = run_for(1000.0, 3, 100_000).await;
        assert!(counters.behind() <= 2, "behind by {}", counters.behind());
    }

    #[tokio::test(start_paused = true)]
    async fn the_source_stops_when_the_ui_is_gone() {
        let (tx, rx) = bridge::channel::<ChatEvent>(16, 16);
        let counters = Arc::new(IngestCounters::default());
        let source = tokio::spawn(run(tx, config(1000.0, 1), counters));
        drop(rx);
        tokio::time::timeout(Duration::from_secs(1), source)
            .await
            .expect("source ends")
            .unwrap();
    }
}
