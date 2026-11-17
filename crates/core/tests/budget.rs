//! The part of the memory and ingest budgets (`docs/ARCHITECTURE.md`, PR 6.1) that this crate
//! owns, as counts rather than timings so it can gate CI on any machine: ten channels of 1 000
//! messages (the default scrollback) — tokenized, stored and indexed for search — must fit in
//! a few megabytes of heap, and taking in a message once the stores are full must stay a
//! handful of allocations.
//!
//! The rest of the 300 MB (layouts, images, GPU) is measured by `scripts/bench.sh` in the app.
//! A separate test binary because `dhat` replaces the global allocator.

use std::sync::Arc;

use gpui_chat_core::{
    Author, BadgeId, Message, MessageStore, Rgb, SearchIndex, Timestamp, Tokenizer,
    twitch_emote_ranges,
};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const CHANNELS: usize = 10;
const SCROLLBACK: usize = 1_000;

/// Heap the ten full channels may hold: stores plus search indexes. Measured 17.1 MB when
/// written (~1.6 KB per message, nearly all of it the message itself: author strings, badge ids,
/// fragments, a fresh `Emote` with three URL strings per Twitch emote; the store and the index
/// add ~120 bytes). The margin is for honest growth, not for a doubling.
const MAX_HEAP_BYTES: usize = 24 * 1024 * 1024;
/// Allocations per message taken in once the stores are full (building the line and its author
/// here, tokenizing, storing, evicting, indexing): measured 45 when written.
const MAX_ALLOCATIONS_PER_MESSAGE: f64 = 64.0;

const WORDS: &[&str] = &[
    "the",
    "stream",
    "is",
    "so",
    "good",
    "today",
    "lol",
    "what",
    "did",
    "he",
    "just",
    "do",
    "chat",
    "is",
    "this",
    "real",
    "no",
    "way",
    "pog",
    "gg",
    "nice",
    "play",
    "clip",
    "it",
    "überraschung",
    "日本語",
    "😂",
    "👍🏽",
    "again",
    "first",
    "time",
    "here",
];
const EMOTES: &[(&str, &str)] = &[
    ("25", "Kappa"),
    ("305954156", "PogChamp"),
    ("88", "PogChamp"),
    ("354", "4Head"),
    ("1902", "Keepo"),
];

/// A deterministic chat line: words, now and then an emote, a mention or a link.
fn message(n: usize) -> Arc<Message> {
    let mut text = String::new();
    let mut emotes: Vec<(&str, usize, usize)> = Vec::new();
    let words = 3 + (n * 7) % 14;
    for w in 0..words {
        if !text.is_empty() {
            text.push(' ');
        }
        let pick = (n * 31 + w * 17) % 41;
        if pick < 5 {
            let (id, name) = EMOTES[pick];
            let start = text.chars().count();
            text.push_str(name);
            emotes.push((id, start, start + name.chars().count() - 1));
        } else if pick == 5 {
            text.push_str(&format!("@user{}", (n + w) % 300));
        } else if pick == 6 {
            text.push_str("https://clips.twitch.tv/SomeClip");
        } else {
            text.push_str(WORDS[pick % WORDS.len()]);
        }
    }
    let tag = emotes
        .iter()
        .map(|(id, a, b)| format!("{id}:{a}-{b}"))
        .collect::<Vec<_>>()
        .join("/");
    let tokenized = Tokenizer::new()
        .with_own_login("viewer")
        .tokenize(&text, &twitch_emote_ranges(&tag, &text));
    let user = n % 400;
    let mut author = Author::new(format!("user{user}"), format!("User{user}"));
    author.user_id = Some(format!("{}", 100_000 + user));
    author.color = (!user.is_multiple_of(3)).then_some(Rgb::new(200, (user % 255) as u8, 90));
    author.badges.push(BadgeId::new("subscriber", "12"));
    if user.is_multiple_of(10) {
        author.badges.push(BadgeId::new("moderator", "1"));
    }
    let mut message = Message::chat(
        format!("{n:08x}-0000-4000-8000-000000000000"),
        Timestamp::from_unix_millis(1_700_000_000_000 + n as i64 * 10),
        author,
        tokenized.fragments,
    );
    message.flags.mention = tokenized.mentions_self;
    Arc::new(message)
}

#[test]
fn ten_full_channels_fit_their_share_of_the_memory_budget_and_ingest_allocates_little() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let start = dhat::HeapStats::get();

    let mut stores: Vec<MessageStore> = (0..CHANNELS)
        .map(|_| MessageStore::with_capacity(SCROLLBACK))
        .collect();
    let mut indexes: Vec<SearchIndex> = (0..CHANNELS).map(|_| SearchIndex::new()).collect();
    let mut n = 0;
    for _ in 0..CHANNELS * SCROLLBACK {
        stores[n % CHANNELS].push(message(n));
        n += 1;
    }
    for (index, store) in indexes.iter_mut().zip(&stores) {
        index.sync(store);
    }
    let full = dhat::HeapStats::get();
    let held = full.curr_bytes - start.curr_bytes;
    assert!(
        stores.iter().all(|s| s.len() == SCROLLBACK),
        "every channel is full"
    );
    assert!(
        held <= MAX_HEAP_BYTES,
        "10 × 1 000 messages hold {held} bytes of heap (budget {MAX_HEAP_BYTES})"
    );

    // Steady state: every new message evicts the oldest one; the index follows.
    let before = dhat::HeapStats::get();
    let more = CHANNELS * SCROLLBACK;
    for _ in 0..more {
        let channel = n % CHANNELS;
        stores[channel].push(message(n));
        indexes[channel].sync(&stores[channel]);
        n += 1;
    }
    let after = dhat::HeapStats::get();
    let per_message = (after.total_blocks - before.total_blocks) as f64 / more as f64;
    assert!(
        per_message <= MAX_ALLOCATIONS_PER_MESSAGE,
        "{per_message:.1} allocations per message (budget {MAX_ALLOCATIONS_PER_MESSAGE})"
    );
    // Eviction gives the memory back: the full stores do not grow.
    let grown = after.curr_bytes.saturating_sub(full.curr_bytes);
    assert!(
        grown <= held / 10,
        "full stores grew by {grown} bytes while messages were evicted"
    );
}
