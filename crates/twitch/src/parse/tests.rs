//! Fixture-driven tests. The raw lines live in `fixtures/irc/`; tests pick messages by
//! the tail of their `id` tag (`...0001` and so on) so the files stay readable.

use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::{
    ChannelId, ChatEvent, ClearTarget, FollowersOnly, Fragment, Message, MessageKind, Rgb,
    RoomState, Timestamp,
};

use super::*;

fn fixture_lines(name: &str) -> Vec<String> {
    let path = format!("{}/../../fixtures/irc/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {path}: {e}"))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Events of every chat line in a fixture file, in order.
fn events(name: &str) -> Vec<ChatEvent> {
    fixture_lines(name)
        .iter()
        .filter_map(|l| parse_line(l).unwrap_or_else(|e| panic!("{name}: {e}: {l}")))
        .collect()
}

/// The message whose `id` tag ends with `id_tail`, from `privmsg.irc` or `usernotice.irc`.
fn message(file: &str, id_tail: &str) -> (ChannelId, Arc<Message>) {
    message_with(file, id_tail, &ParseOptions::default())
}

fn message_with(file: &str, id_tail: &str, options: &ParseOptions) -> (ChannelId, Arc<Message>) {
    let needle = format!("{id_tail};");
    let line = fixture_lines(file)
        .into_iter()
        .find(|l| l.contains(&format!("id={needle}")) || l.contains(&format!(";id={needle}")))
        .or_else(|| {
            // `id` may be the last tag.
            fixture_lines(file)
                .into_iter()
                .find(|l| l.contains(id_tail))
        })
        .unwrap_or_else(|| panic!("no line with id {id_tail} in {file}"));
    match parse_line_with(&line, options).unwrap().unwrap() {
        ChatEvent::Message { channel, message } => (channel, message),
        other => panic!("expected a message, got {other:?}"),
    }
}

fn emotes(m: &Message) -> Vec<(&str, &str)> {
    m.fragments
        .iter()
        .filter_map(|f| match f {
            Fragment::Emote { emote, .. } => Some((emote.id.as_str(), &*emote.name)),
            _ => None,
        })
        .collect()
}

fn privmsg(id_tail: &str) -> Arc<Message> {
    message("privmsg.irc", &format!("0000-4000-8000-{id_tail}")).1
}

fn usernotice(id_tail: &str) -> Arc<Message> {
    message("usernotice.irc", &format!("0000-4000-8000-{id_tail}")).1
}

// ---- whole files -------------------------------------------------------------------------

#[test]
fn every_fixture_line_parses() {
    for name in [
        "privmsg.irc",
        "usernotice.irc",
        "clearchat.irc",
        "clearmsg.irc",
        "roomstate.irc",
        "notice.irc",
    ] {
        let lines = fixture_lines(name);
        let chat = lines
            .iter()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .count();
        assert_eq!(events(name).len(), chat, "{name}: every line is an event");
    }
}

#[test]
fn non_chat_lines_are_none() {
    let lines = fixture_lines("other.irc");
    assert!(lines.len() > 8);
    for line in lines {
        assert_eq!(parse_line(&line), Ok(None), "{line:?}");
    }
}

#[test]
fn blank_lines_comments_and_crlf() {
    assert_eq!(parse_line(""), Ok(None));
    assert_eq!(parse_line("   "), Ok(None));
    assert_eq!(parse_line("# a comment"), Ok(None));
    assert_eq!(parse_line("PING :tmi.twitch.tv\r\n"), Ok(None));
    let with_crlf = "@id=1;tmi-sent-ts=5 :a!a@a.tmi.twitch.tv PRIVMSG #c :hi\r\n";
    assert!(matches!(parse_line(with_crlf), Ok(Some(_))));
}

// ---- PRIVMSG -----------------------------------------------------------------------------

#[test]
fn plain_message() {
    let (channel, m) = message("privmsg.irc", "000000000001");
    assert_eq!(channel, ChannelId::new("testchan"));
    assert_eq!(m.id.as_str(), "11111111-0000-4000-8000-000000000001");
    assert_eq!(m.kind, MessageKind::Chat);
    assert_eq!(m.timestamp, Timestamp::from_unix_millis(1_700_000_000_001));
    let a = m.author.as_ref().unwrap();
    assert_eq!(
        (a.login.as_str(), a.display_name.as_str()),
        ("plain_user", "Plain_User")
    );
    assert_eq!(a.user_id.as_deref(), Some("1001"));
    assert_eq!(a.color, Some(Rgb::new(0, 0, 0xff)));
    assert!(a.badges.is_empty());
    assert_eq!(
        m.plain_text(),
        "hello world, see https://example.com or @someone"
    );
    assert!(
        m.fragments
            .iter()
            .any(|f| matches!(f, Fragment::Link { url, .. } if url == "https://example.com"))
    );
    assert!(m.fragments.iter().any(
        |f| matches!(f, Fragment::Mention { login, is_self: false, .. } if login == "someone")
    ));
    assert_eq!(m.flags, Default::default());
    assert!(m.reply.is_none() && m.bits.is_none() && m.notice_id.is_none());
}

#[test]
fn raw_tags_are_complete_sorted_and_unescaped() {
    let m = privmsg("000000000007");
    let keys: Vec<_> = m.raw_tags.iter().map(|(k, _)| k).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted);
    assert_eq!(m.raw_tags.get("room-id"), Some("1000"));
    assert_eq!(m.raw_tags.get("user-type"), Some(""), "empty tags are kept");
    assert_eq!(m.raw_tags.get("flags"), Some(""));
    assert_eq!(
        m.raw_tags.get("reply-parent-msg-body"),
        Some("what is the game; this\\that")
    );
}

#[test]
fn emote_range_after_emoji_counts_code_points() {
    let m = privmsg("000000000002");
    assert_eq!(m.plain_text(), "😀 Kappa");
    assert_eq!(emotes(&m), [("25", "Kappa")]);
    assert!(matches!(&m.fragments[0], Fragment::Emoji { text } if text == "😀"));
}

#[test]
fn emote_range_after_zwj_sequence_and_repeats() {
    let m = privmsg("000000000003");
    assert_eq!(m.plain_text(), "👨‍👩‍👧 Kappa Kappa");
    assert_eq!(emotes(&m), [("25", "Kappa"), ("25", "Kappa")]);
    let emoji: Vec<_> = m
        .fragments
        .iter()
        .filter(|f| matches!(f, Fragment::Emoji { .. }))
        .collect();
    assert_eq!(emoji.len(), 1, "the family stays one grapheme: {emoji:?}");
}

#[test]
fn missing_colour_stays_none() {
    for tail in ["000000000004", "000000000005"] {
        assert_eq!(privmsg(tail).author.as_ref().unwrap().color, None, "{tail}");
    }
    // Empty display-name falls back to the login.
    let bare = privmsg("000000000005");
    let a = bare.author.as_ref().unwrap();
    assert_eq!(
        (a.login.as_str(), a.display_name.as_str()),
        ("bare_user", "bare_user")
    );
    assert_eq!(a.user_id.as_deref(), Some("1004"));
}

#[test]
fn action_message() {
    let m = privmsg("000000000006");
    assert_eq!(m.kind, MessageKind::Action);
    assert_eq!(m.plain_text(), "wave Kappa", "no \\x01ACTION wrapper");
    assert_eq!(
        emotes(&m),
        [("25", "Kappa")],
        "range is relative to the inner text"
    );
}

#[test]
fn reply_tags() {
    let m = privmsg("000000000007");
    let r = m.reply.as_ref().expect("reply");
    assert_eq!(r.parent_id.as_str(), "11111111-0000-4000-8000-000000000001");
    assert_eq!(r.parent_login, "plain_user");
    assert_eq!(r.parent_display_name, "Parent");
    assert_eq!(r.parent_body, "what is the game; this\\that");
    assert_eq!(
        r.thread_parent_id.as_ref().map(|i| i.as_str()),
        Some("11111111-0000-4000-8000-0000000000aa")
    );
    // The body is left alone: the leading @mention stays a fragment.
    assert!(matches!(&m.fragments[0], Fragment::Mention { login, .. } if login == "parent"));
}

#[test]
fn first_message_bits_highlight_history() {
    assert!(privmsg("000000000008").flags.first_message);
    assert!(!privmsg("000000000001").flags.first_message);

    let cheer = privmsg("000000000009");
    assert_eq!(cheer.bits, Some(100));
    assert!(matches!(
        &cheer.fragments[0],
        Fragment::Cheer { prefix, bits: 100 } if prefix == "Cheer"
    ));

    assert!(privmsg("00000000000a").flags.highlighted);

    let h = privmsg("00000000000b");
    assert!(h.flags.history);
    assert_eq!(
        h.timestamp,
        Timestamp::from_unix_millis(1_700_000_100_000),
        "tmi-sent-ts wins"
    );
    assert_eq!(h.raw_tags.get("rm-received-ts"), Some("1700000100100"));
    assert!(!privmsg("000000000001").flags.history);
}

#[test]
fn timestamp_fallbacks() {
    let options = ParseOptions {
        fallback_timestamp: Timestamp::from_unix_millis(42),
        ..ParseOptions::default()
    };
    let (_, m) = message_with("privmsg.irc", "0000-4000-8000-00000000000c", &options);
    assert_eq!(
        m.timestamp,
        Timestamp::from_unix_millis(1_700_000_200_200),
        "rm-received-ts"
    );

    let bare = "@id=x :a!a@a.tmi.twitch.tv PRIVMSG #c :hi";
    let Some(ChatEvent::Message { message, .. }) = parse_line_with(bare, &options).unwrap() else {
        panic!()
    };
    assert_eq!(message.timestamp, Timestamp::from_unix_millis(42));
    let Some(ChatEvent::Message { message, .. }) = parse_line(bare).unwrap() else {
        panic!()
    };
    assert_eq!(message.timestamp, Timestamp::default());
}

#[test]
fn mentions_of_the_local_user() {
    let id = "0000-4000-8000-00000000000d";
    let (_, anon) = message("privmsg.irc", id);
    assert!(!anon.flags.mention);
    let own = ParseOptions {
        own_login: Some("viewer".into()),
        ..ParseOptions::default()
    };
    let (_, m) = message_with("privmsg.irc", id, &own);
    assert!(m.flags.mention);
    assert!(
        m.fragments
            .iter()
            .any(|f| matches!(f, Fragment::Mention { is_self: true, .. }))
    );
}

#[test]
fn ignored_character_does_not_shift_emote_ranges() {
    let m = privmsg("00000000000e");
    assert_eq!(emotes(&m), [("25", "Kappa")]);
    assert_eq!(m.plain_text().trim_end(), "Kappa");
    assert!(!m.plain_text().contains('\u{e0000}'));
}

#[test]
fn recorded_lines() {
    let lines = fixture_lines("privmsg.irc");
    let real: Vec<Arc<Message>> = lines
        .iter()
        .filter(|l| {
            l.contains("PRIVMSG #xqc")
                || l.contains("PRIVMSG #jynxzi")
                || l.contains("PRIVMSG #hasanabi")
        })
        .map(|l| match parse_line(l).unwrap().unwrap() {
            ChatEvent::Message { message, .. } => message,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(real.len(), 4);

    // xqc: non-numeric emote id, badges, colour.
    assert_eq!(
        emotes(&real[0]),
        [("emotesv2_70f4fd96a64e4ec0af542ec3c79b4d27", "meiyaTweak")]
    );
    assert_eq!(real[0].plain_text(), "meiyaTweak water");
    let badges: Vec<_> = real[0]
        .author
        .as_ref()
        .unwrap()
        .badges
        .iter()
        .map(|b| (&*b.set, &*b.version))
        .collect();
    assert_eq!(badges, [("subscriber", "30"), ("mod-founder", "1")]);
    assert_eq!(
        real[0].author.as_ref().unwrap().color,
        Rgb::from_hex("#47135F")
    );

    // jynxzi: reply, empty colour and badges.
    assert!(real[1].reply.is_some());
    assert_eq!(real[1].author.as_ref().unwrap().color, None);
    assert!(real[1].author.as_ref().unwrap().badges.is_empty());

    // hasanabi: three ranges of the same emote, escaped spaces in the parent body.
    assert_eq!(
        real[2].plain_text(),
        "@errehpe orpheu3467 orpheu3467 orpheu3467"
    );
    assert_eq!(emotes(&real[2]).len(), 3);
    assert_eq!(
        real[2].reply.as_ref().unwrap().parent_body,
        "6 Shrug 7 months @tackycardia"
    );

    // hasanabi bot line: emoji with a `flags` range tag we do not model.
    assert!(
        real[3]
            .fragments
            .iter()
            .any(|f| matches!(f, Fragment::Emoji { .. }))
    );
    assert!(real[3].raw_tags.get("flags").is_some());
}

// ---- USERNOTICE --------------------------------------------------------------------------

#[test]
fn sub_and_resub() {
    let sub = usernotice("000000000001");
    assert_eq!(sub.kind, MessageKind::UserNotice);
    assert_eq!(sub.notice_id.as_deref(), Some("sub"));
    assert_eq!(
        sub.system_text.as_deref(),
        Some("NewSub subscribed at Tier 1.")
    );
    assert!(sub.fragments.is_empty(), "no user message");
    let a = sub.author.as_ref().unwrap();
    assert_eq!(
        (a.login.as_str(), a.display_name.as_str()),
        ("newsub", "NewSub")
    );
    assert_eq!(
        sub.raw_tags.get("msg-param-sub-plan-name"),
        Some("Channel Subscription")
    );

    let resub = usernotice("000000000002");
    assert_eq!(resub.notice_id.as_deref(), Some("resub"));
    assert_eq!(
        resub.system_text.as_deref(),
        Some(
            "LongSub subscribed at Tier 1. They've subscribed for 12 months, currently on a 12 month streak!"
        )
    );
    assert_eq!(resub.plain_text(), "a year Kappa");
    assert_eq!(emotes(&resub), [("25", "Kappa")]);
    assert_eq!(
        resub.raw_tags.get("msg-param-cumulative-months"),
        Some("12")
    );
}

#[test]
fn subgift_raid_announcement_ritual() {
    let gift = usernotice("000000000003");
    assert_eq!(gift.notice_id.as_deref(), Some("subgift"));
    assert_eq!(gift.author.as_ref().unwrap().login, "gifter");
    assert_eq!(
        gift.raw_tags.get("msg-param-recipient-user-name"),
        Some("lucky")
    );
    assert_eq!(gift.raw_tags.get("msg-param-origin-id"), Some("da 39 a3"));

    let raid = usernotice("000000000004");
    assert_eq!(raid.notice_id.as_deref(), Some("raid"));
    assert_eq!(raid.author.as_ref().unwrap().login, "raidingchannel");
    assert_eq!(
        raid.system_text.as_deref(),
        Some("1234 raiders from RaidingChannel have joined!")
    );
    assert_eq!(raid.raw_tags.get("msg-param-viewerCount"), Some("1234"));

    let ann = usernotice("000000000005");
    assert_eq!(ann.notice_id.as_deref(), Some("announcement"));
    assert_eq!(ann.system_text, None, "empty system-msg is absent");
    assert_eq!(ann.plain_text(), "Stream starts in five minutes");
    assert_eq!(ann.raw_tags.get("msg-param-color"), Some("PRIMARY"));
    assert_eq!(
        ann.author.as_ref().unwrap().color,
        Some(Rgb::new(0, 0xff, 0x7f))
    );

    let ritual = usernotice("000000000006");
    assert_eq!(ritual.notice_id.as_deref(), Some("ritual"));
    assert_eq!(ritual.author.as_ref().unwrap().color, None);
}

// ---- moderation, room state, notices -------------------------------------------------------

#[test]
fn clearchat_timeout_ban_and_channel() {
    let evs = events("clearchat.irc");
    let targets: Vec<_> = evs
        .iter()
        .map(|e| match e {
            ChatEvent::ClearChat { channel, target } => {
                assert_eq!(channel.login(), "testchan");
                target.clone()
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        targets,
        [
            ClearTarget::User {
                login: "timeduser".into(),
                duration: Some(Duration::from_secs(600))
            },
            ClearTarget::User {
                login: "banneduser".into(),
                duration: None
            },
            ClearTarget::Channel,
        ]
    );
}

#[test]
fn clearmsg() {
    let evs = events("clearmsg.irc");
    assert_eq!(
        evs[0],
        ChatEvent::DeleteMessage {
            channel: ChannelId::new("testchan"),
            message_id: "33333333-0000-4000-8000-000000000001".into(),
            login: Some("spammer".into()),
        }
    );
    assert!(matches!(
        &evs[1],
        ChatEvent::DeleteMessage { login: None, .. }
    ));
}

#[test]
fn roomstate_full_then_partial() {
    let updates: Vec<_> = events("roomstate.irc")
        .into_iter()
        .map(|e| match e {
            ChatEvent::RoomState { update, .. } => update,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(updates.len(), 10);

    // Full state on join (real xqc line).
    let full = &updates[0];
    assert_eq!(full.room_id.as_deref(), Some("71092938"));
    assert_eq!(full.followers_only, Some(FollowersOnly::Minutes(1440)));
    assert_eq!(
        (
            full.emote_only,
            full.unique_chat,
            full.subscribers_only,
            full.slow_mode
        ),
        (Some(false), Some(false), Some(false), Some(Duration::ZERO))
    );
    assert_eq!(updates[1].followers_only, Some(FollowersOnly::Minutes(10)));

    // Partial updates leave everything else `None`.
    assert_eq!(updates[2].slow_mode, Some(Duration::from_secs(30)));
    assert_eq!(updates[2].emote_only, None);
    assert_eq!(updates[2].followers_only, None);
    assert_eq!(updates[3].slow_mode, Some(Duration::ZERO));
    assert_eq!(updates[4].followers_only, Some(FollowersOnly::Off));
    assert_eq!(updates[5].followers_only, Some(FollowersOnly::All));
    assert_eq!(updates[6].followers_only, Some(FollowersOnly::Minutes(15)));
    assert_eq!(updates[7].emote_only, Some(true));
    assert_eq!(updates[8].unique_chat, Some(true));
    assert_eq!(updates[9].subscribers_only, Some(true));

    // ... and merge into the room state the way the UI will.
    let mut state = RoomState::default();
    for u in &updates[..3] {
        state.apply(u);
    }
    assert_eq!(state.followers_only, FollowersOnly::Minutes(10));
    assert_eq!(state.slow_mode, Duration::from_secs(30));
    assert_eq!(state.room_id.as_deref(), Some("1000"));
}

#[test]
fn notices() {
    let evs = events("notice.irc");
    assert_eq!(
        evs[0],
        ChatEvent::Notice {
            channel: Some(ChannelId::new("testchan")),
            msg_id: Some("slow_off".into()),
            text: "This room is no longer in slow mode.".into(),
        }
    );
    assert!(matches!(&evs[1], ChatEvent::Notice { msg_id: Some(id), .. } if id == "msg_banned"));
    assert_eq!(
        evs[2],
        ChatEvent::Notice {
            channel: None,
            msg_id: None,
            text: "Login authentication failed".into(),
        }
    );
}

// ---- errors ---------------------------------------------------------------------------

#[test]
fn errors_are_reported_not_panicked() {
    assert!(matches!(parse_line("@"), Err(ParseError::Syntax(_))));
    assert!(matches!(parse_line("@a=b"), Err(ParseError::Syntax(_))));
    assert!(matches!(parse_line(":prefix"), Err(ParseError::Syntax(_))));
    assert!(matches!(
        parse_line("@id=1 :a!a@a PRIVMSG"),
        Err(ParseError::MissingParam { index: 0, .. })
    ));
    assert!(matches!(
        parse_line("@id=1 :a!a@a PRIVMSG #c"),
        Err(ParseError::MissingParam { index: 1, .. })
    ));
    assert!(matches!(
        parse_line("@tmi-sent-ts=1 :a!a@a PRIVMSG #c :hi"),
        Err(ParseError::MissingTag { tag: "id", .. })
    ));
    assert!(matches!(
        parse_line("@id=1 PRIVMSG #c :hi"),
        Err(ParseError::MissingPrefix { .. })
    ));
    assert!(matches!(
        parse_line("@slow=fast :tmi.twitch.tv ROOMSTATE #c"),
        Err(ParseError::InvalidTag { tag: "slow", .. })
    ));
    assert!(matches!(
        parse_line("@emote-only=yes :tmi.twitch.tv ROOMSTATE #c"),
        Err(ParseError::InvalidTag {
            tag: "emote-only",
            ..
        })
    ));
    assert!(matches!(
        parse_line("@bits=lots;id=1 :a!a@a PRIVMSG #c :hi"),
        Err(ParseError::InvalidTag { tag: "bits", .. })
    ));
    assert!(matches!(
        parse_line("@target-msg-id= :tmi.twitch.tv CLEARMSG #c :x"),
        Err(ParseError::MissingTag {
            tag: "target-msg-id",
            ..
        })
    ));
}

#[test]
fn bad_emote_ranges_do_not_break_the_message() {
    let line = "@id=1;emotes=25:40-50,x:1-2;tmi-sent-ts=1 :a!a@a.tmi.twitch.tv PRIVMSG #c :hi";
    let Some(ChatEvent::Message { message, .. }) = parse_line(line).unwrap() else {
        panic!()
    };
    assert_eq!(message.plain_text(), "hi");
}
