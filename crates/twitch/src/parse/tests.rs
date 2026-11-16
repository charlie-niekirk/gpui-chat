//! Fixture-driven tests. The raw lines live in `fixtures/irc/`; tests pick messages by
//! the tail of their `id` tag (`...0001` and so on) so the files stay readable.

use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::{
    AnnouncementColor, ChannelId, ChatEvent, ClearTarget, EventKind, FollowersOnly, Fragment,
    Message, MessageKind, Rgb, RoomState, SubPlan, Timestamp,
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
        "events.irc",
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
fn userstate_is_the_local_users_role_in_the_channel() {
    let roles: Vec<(String, ChannelRole)> = fixture_lines("userstate.irc")
        .iter()
        .filter(|line| !line.starts_with('#'))
        .map(|line| match parse_line(line) {
            Ok(Some(ChatEvent::UserState { channel, role })) => (channel.login().to_owned(), role),
            other => panic!("{line:?} parsed as {other:?}"),
        })
        .collect();
    assert_eq!(
        roles,
        [
            ("testchan".to_owned(), ChannelRole::Viewer),
            ("subchan".to_owned(), ChannelRole::Viewer),
            ("modchan".to_owned(), ChannelRole::Moderator),
            ("tagchan".to_owned(), ChannelRole::Moderator),
            ("ownchan".to_owned(), ChannelRole::Broadcaster),
            ("staffchan".to_owned(), ChannelRole::Viewer),
        ]
    );
    // Without a channel there is nothing to say; GLOBALUSERSTATE is not about a channel.
    assert!(parse_line("@mod=1 :tmi.twitch.tv USERSTATE").is_err());
    assert_eq!(
        parse_line("@mod=1 :tmi.twitch.tv GLOBALUSERSTATE"),
        Ok(None)
    );
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
        Fragment::Cheer { prefix, bits: 100, .. } if prefix == "Cheer"
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
            ChatEvent::ClearChat {
                channel, target, ..
            } => {
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

#[test]
fn malformed_optional_tags_are_treated_as_absent() {
    let Some(ChatEvent::Message { message: m, .. }) = parse_line(
        "@id=1;bits=-5;first-msg=true;tmi-sent-ts=1 :a!a@a.tmi.twitch.tv PRIVMSG #c :Cheer100 hi",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(m.bits, None);
    assert!(!m.flags.first_message);
    assert_eq!(m.plain_text(), "Cheer100 hi", "no cheermote without bits");
    assert_eq!(
        m.raw_tags.get("bits"),
        Some("-5"),
        "the raw tag is still kept"
    );

    let Some(ChatEvent::RoomState { update, .. }) = parse_line(
        "@slow=fast;emote-only=yes;followers-only=99999999999;r9k=1;room-id=5 :tmi.twitch.tv ROOMSTATE #c",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(update.slow_mode, None);
    assert_eq!(update.emote_only, None);
    assert_eq!(update.followers_only, None);
    assert_eq!(update.unique_chat, Some(true), "good tags still apply");

    let Some(ChatEvent::ClearChat { target, .. }) =
        parse_line("@ban-duration=soon :tmi.twitch.tv CLEARCHAT #c :someone").unwrap()
    else {
        panic!()
    };
    assert_eq!(
        target,
        ClearTarget::User {
            login: "someone".into(),
            duration: None
        }
    );
}

#[test]
fn action_without_closing_marker_is_still_an_action() {
    let line =
        "@id=1;emotes=25:5-9;tmi-sent-ts=1 :a!a@a.tmi.twitch.tv PRIVMSG #c :\u{1}ACTION wave Kappa";
    let Some(ChatEvent::Message { message: m, .. }) = parse_line(line).unwrap() else {
        panic!()
    };
    assert_eq!(m.kind, MessageKind::Action);
    assert_eq!(m.plain_text(), "wave Kappa");
    assert_eq!(emotes(&m), [("25", "Kappa")]);
}

// ---- replies (PR 3.4, fixtures/irc/replies.irc) -----------------------------------------

fn reply_message(id_tail: &str) -> Arc<Message> {
    message("replies.irc", &format!("33333333-0000-4000-8000-{id_tail}")).1
}

#[test]
fn a_thread_root_has_no_reply_tags() {
    let root = reply_message("000000000001");
    assert!(root.reply.is_none());
    assert_eq!(root.thread_root().as_str(), root.id.as_str());
}

#[test]
fn a_reply_to_the_root_names_it_as_parent_and_thread() {
    let reply = reply_message("000000000002");
    let r = reply.reply.as_ref().expect("reply");
    assert_eq!(r.parent_id.as_str(), "33333333-0000-4000-8000-000000000001");
    assert_eq!(r.parent_login, "alice");
    assert_eq!(r.parent_display_name, "Alice");
    assert_eq!(
        r.parent_body, "what is the best opening for a quick game?",
        "the escaped spaces are plain spaces"
    );
    assert_eq!(r.root_id(), &r.parent_id);
    assert_eq!(reply.thread_root(), &r.parent_id);
}

#[test]
fn a_reply_to_a_reply_keeps_the_thread_root_apart_from_its_parent() {
    let reply = reply_message("000000000003");
    let r = reply.reply.as_ref().expect("reply");
    assert_eq!(r.parent_id.as_str(), "33333333-0000-4000-8000-000000000002");
    assert_eq!(r.parent_login, "bob");
    assert_eq!(r.parent_body, "@Alice the Italian is solid");
    assert_eq!(
        reply.thread_root().as_str(),
        "33333333-0000-4000-8000-000000000001"
    );
    assert_ne!(r.root_id(), &r.parent_id);
}

#[test]
fn a_reply_without_a_body_or_thread_tag_still_has_a_parent_and_root() {
    let reply = reply_message("000000000007");
    let r = reply.reply.as_ref().expect("reply");
    assert_eq!(r.parent_id.as_str(), "33333333-0000-4000-8000-0000000000f1");
    assert_eq!(r.parent_name(), "Hank");
    assert_eq!(r.parent_body, "", "no body tag: empty, not an error");
    assert_eq!(r.thread_parent_id, None);
    assert_eq!(reply.thread_root(), &r.parent_id);
}

#[test]
fn the_reply_tags_stay_in_the_raw_tags_too() {
    let reply = reply_message("000000000006");
    assert_eq!(
        reply.raw_tags.get("reply-thread-parent-user-login"),
        Some("frank")
    );
}

#[test]
fn a_replayed_conversation_forms_one_thread_in_a_store() {
    let mut store = gpui_chat_core::MessageStore::new();
    for event in events("replies.irc") {
        if let ChatEvent::Message { message, .. } = event {
            store.push(message);
        }
    }
    let thread = store.thread("33333333-0000-4000-8000-000000000001");
    assert_eq!(thread.root.unwrap().author.as_ref().unwrap().login, "alice");
    let replies: Vec<_> = thread
        .replies
        .iter()
        .map(|m| m.author.as_ref().unwrap().login.as_str())
        .collect();
    assert_eq!(replies, ["bob", "carol", "alice"]);
    // A reply to a message that is not in the log is a thread of its own, without a root.
    let orphan = store.thread("33333333-0000-4000-8000-0000000000f0");
    assert!(orphan.root.is_none());
    assert_eq!(orphan.replies.len(), 1);
}

#[test]
fn clearchat_carries_its_time_when_the_line_has_one() {
    let at = |line: &str| match parse_line(line).unwrap() {
        Some(ChatEvent::ClearChat { at, .. }) => at,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        at("@ban-duration=600;tmi-sent-ts=1794826007000 :tmi.twitch.tv CLEARCHAT #c :someone"),
        Some(Timestamp(1_794_826_007_000))
    );
    assert_eq!(at(":tmi.twitch.tv CLEARCHAT #c"), None);
    assert_eq!(at("@tmi-sent-ts=oops :tmi.twitch.tv CLEARCHAT #c"), None);
}

// ---- event messages (PR 5.1, fixtures/irc/events.irc) ----------------------------------------

fn event_message(n: &str) -> Arc<Message> {
    message("events.irc", &format!("0000-4000-8000-{n:0>12}")).1
}

fn kind(n: &str) -> EventKind {
    event_message(n).event.clone().expect("a user notice")
}

#[test]
fn subs_and_resubs() {
    assert_eq!(
        kind("10"),
        EventKind::Sub {
            plan: SubPlan::Tier1,
            months: 1
        }
    );
    let prime = event_message("11");
    assert_eq!(
        prime.event,
        Some(EventKind::Sub {
            plan: SubPlan::Prime,
            months: 1
        })
    );
    assert_eq!(prime.plain_text(), "hello Kappa first month");
    assert_eq!(
        kind("12"),
        EventKind::Resub {
            plan: SubPlan::Tier1,
            months: 12,
            streak: Some(12)
        }
    );
    let long = event_message("12");
    assert!(
        long.fragments
            .iter()
            .any(|f| matches!(f, Fragment::Link { url, .. } if url == "https://example.com/art")),
        "the user's message is tokenized"
    );
    // The streak is only reported when the user shared it.
    assert_eq!(
        kind("13"),
        EventKind::Resub {
            plan: SubPlan::Tier3,
            months: 27,
            streak: None
        }
    );
}

#[test]
fn gifts() {
    let EventKind::Gift {
        plan,
        recipient,
        months,
        gifter_total,
        anonymous,
        community_gift_id,
    } = kind("20")
    else {
        panic!("a gift")
    };
    assert_eq!(plan, SubPlan::Tier1);
    assert_eq!(
        (
            recipient.login.as_str(),
            recipient.name(),
            recipient.user_id.as_deref()
        ),
        ("lucky", "Lucky", Some("2010"))
    );
    assert_eq!(
        (months, gifter_total, anonymous, community_gift_id),
        (1, Some(50), false, None)
    );

    let EventKind::Gift {
        plan,
        anonymous,
        gifter_total,
        ..
    } = kind("21")
    else {
        panic!("an anonymous gift")
    };
    assert_eq!(
        (plan, anonymous, gifter_total),
        (SubPlan::Tier2, true, None)
    );

    assert_eq!(
        kind("22"),
        EventKind::MysteryGift {
            plan: SubPlan::Tier1,
            count: 3,
            gifter_total: Some(103),
            anonymous: false,
            origin_id: Some("11 22 33".into()),
            recipients: Vec::new(),
        }
    );
    // The per-recipient lines of a mystery gift name it (`community-gift-id` = its origin id) and
    // carry no running total (Twitch sends 0 there).
    for n in ["23", "24", "25"] {
        assert!(kind(n).is_community_gift(), "{n}");
        assert_eq!(kind(n).community_gift_id(), Some("11 22 33"));
        assert!(matches!(
            kind(n),
            EventKind::Gift {
                gifter_total: None,
                ..
            }
        ));
    }
    assert!(!kind("20").is_community_gift());
    assert_eq!(
        kind("26"),
        EventKind::MysteryGift {
            plan: SubPlan::Tier1,
            count: 20,
            gifter_total: None,
            anonymous: true,
            origin_id: Some("44 55 66".into()),
            recipients: Vec::new(),
        }
    );
    assert_eq!(
        kind("27"),
        EventKind::GiftPaidUpgrade {
            gifter: Some("Generous".into()),
            promo: None
        }
    );
    assert_eq!(
        kind("28"),
        EventKind::GiftPaidUpgrade {
            gifter: None,
            promo: None
        }
    );
    assert_eq!(
        kind("29"),
        EventKind::PrimePaidUpgrade {
            plan: SubPlan::Tier1
        }
    );
}

#[test]
fn raids_announcements_and_the_rest() {
    assert_eq!(
        kind("30"),
        EventKind::Raid {
            from_login: "raidingchannel".into(),
            from_name: "RaidingChannel".into(),
            viewers: 1234,
            profile_image: Some(
                "https://static-cdn.jtvnw.net/jtv_user_pictures/x-profile_image-70x70.png".into()
            )
        }
    );
    assert!(matches!(
        kind("31"),
        EventKind::Raid {
            viewers: 1,
            profile_image: None,
            ..
        }
    ));
    let colours: Vec<_> = ["40", "41", "42", "43", "44"]
        .map(|n| match kind(n) {
            EventKind::Announcement { color } => color,
            other => panic!("{other:?}"),
        })
        .into();
    assert_eq!(
        colours,
        [
            AnnouncementColor::Primary,
            AnnouncementColor::Blue,
            AnnouncementColor::Green,
            AnnouncementColor::Orange,
            AnnouncementColor::Purple
        ]
    );
    // The announcement's text is the user's message; there is no system-msg.
    let announcement = event_message("42");
    assert_eq!(announcement.system_text, None);
    assert_eq!(emotes(&announcement), [("25", "Kappa")]);
    assert_eq!(kind("50"), EventKind::NewChatter);
    assert_eq!(kind("51"), EventKind::BitsBadgeTier { tier: 1000 });
    // Unknown msg-ids keep Twitch's own line.
    assert_eq!(
        kind("52"),
        EventKind::Other {
            msg_id: "standardpayforward".into()
        }
    );
    assert!(
        event_message("52")
            .system_text
            .as_deref()
            .is_some_and(|text| text.starts_with("Payer is paying forward"))
    );
    assert_eq!(
        kind("53"),
        EventKind::Other {
            msg_id: "unraid".into()
        }
    );
}

#[test]
fn an_incomplete_notice_falls_back_to_its_system_line() {
    let parse = |tags: &str| {
        let line = format!(
            "@id=1;login=x;system-msg=hello;tmi-sent-ts=5;{tags} :tmi.twitch.tv USERNOTICE #c"
        );
        match parse_line(&line).unwrap().unwrap() {
            ChatEvent::Message { message, .. } => message,
            other => panic!("{other:?}"),
        }
    };
    let other = |m: &Message| matches!(m.event, Some(EventKind::Other { .. }));
    // A subgift with no recipient, a mystery gift with no count, a raid with no viewer count.
    assert!(other(&parse("msg-id=subgift")));
    assert!(other(&parse("msg-id=submysterygift")));
    assert!(other(&parse(
        "msg-id=submysterygift;msg-param-mass-gift-count=0"
    )));
    assert!(other(&parse("msg-id=raid;msg-param-login=x")));
    assert!(other(&parse("msg-id=ritual;msg-param-ritual-name=other")));
    assert!(other(&parse("msg-id=")));
    assert_eq!(
        parse("msg-id=raid;msg-param-login=x")
            .system_text
            .as_deref(),
        Some("hello")
    );
    // Missing numbers do not fail a sub.
    assert_eq!(
        parse("msg-id=sub").event,
        Some(EventKind::Sub {
            plan: SubPlan::Tier1,
            months: 1
        })
    );
    // An unknown plan is kept as sent.
    assert_eq!(
        parse("msg-id=sub;msg-param-sub-plan=9000").event,
        Some(EventKind::Sub {
            plan: SubPlan::Other("9000".into()),
            months: 1
        })
    );
}

#[test]
fn chat_lines_have_no_event_kind() {
    assert_eq!(privmsg("000000000001").event, None);
    assert_eq!(event_message("1").event, None);
}

#[test]
fn bits_cheermotes_highlights_rewards_and_first_messages() {
    let cheer = event_message("60");
    assert_eq!(cheer.bits, Some(100));
    assert!(matches!(
        &cheer.fragments[0],
        Fragment::Cheer { prefix, bits: 100, image: None } if prefix == "Cheer"
    ));
    // Every cheermote of the message is found, in any case, with its own amount.
    let big = event_message("61");
    assert_eq!(big.bits, Some(1750));
    let cheers: Vec<_> = big
        .fragments
        .iter()
        .filter_map(|f| match f {
            Fragment::Cheer { prefix, bits, .. } => Some((prefix.as_str(), *bits)),
            _ => None,
        })
        .collect();
    assert_eq!(cheers, [("cheer", 1000), ("Kappa", 500), ("cheer", 250)]);
    // A prefix this build does not know is text until the channel's cheermotes are known.
    let odd = event_message("62");
    assert_eq!(odd.bits, Some(50));
    assert!(
        !odd.fragments
            .iter()
            .any(|f| matches!(f, Fragment::Cheer { .. }))
    );

    let highlighted = event_message("70");
    assert!(highlighted.flags.highlighted);
    assert_eq!(highlighted.reward_id, None);
    let reward = event_message("71");
    assert_eq!(
        reward.reward_id.as_deref(),
        Some("a1b2c3d4-0000-4000-8000-00000000aaaa")
    );
    assert!(!reward.flags.highlighted);
    assert_eq!(emotes(&reward), [("25", "Kappa")]);
    let first = event_message("72");
    assert!(first.flags.first_message);
    assert_eq!(first.kind, MessageKind::Chat);
    let first_action = event_message("73");
    assert!(first_action.flags.first_message);
    assert_eq!(first_action.kind, MessageKind::Action);
    assert_eq!(first_action.plain_text(), "waves hello");
}

#[test]
fn shared_chat_tags() {
    let own = event_message("80");
    let shared = own.shared.as_ref().expect("shared");
    assert_eq!(shared.source_room_id, "2000");
    assert_eq!(
        shared.source_id.as_ref().map(|id| id.as_str()),
        Some(own.id.as_str())
    );
    assert!(!shared.is_copy_in(own.raw_tags.get("room-id")));

    // The copy in the other channel: another id, the original's in `source-id`.
    let copy = event_message("81");
    let shared = copy.shared.as_ref().expect("shared");
    assert!(shared.is_copy_in(copy.raw_tags.get("room-id")));
    assert_ne!(copy.id, own.id);
    assert_eq!(
        shared.source_id.as_ref(),
        Some(&own.id),
        "`source-id` names the original"
    );
    assert_eq!(
        (&shared.source_login, &shared.source_name),
        (&None, &None),
        "the parser cannot know the name"
    );

    let visitor = event_message("82");
    let shared = visitor.shared.as_ref().unwrap();
    assert_eq!(shared.source_room_id, "3000");
    let badges: Vec<_> = shared
        .source_badges
        .iter()
        .map(|b| (&*b.set, &*b.version))
        .collect();
    assert_eq!(badges, [("moderator", "1"), ("subscriber", "12")]);
    // The author's own badges are the ones of the room the line was delivered in.
    assert!(visitor.author.as_ref().unwrap().badges.is_empty());

    // Sent in this room itself: not a copy.
    let host = event_message("83");
    assert!(
        !host
            .shared
            .as_ref()
            .unwrap()
            .is_copy_in(host.raw_tags.get("room-id"))
    );
    // Outside shared chat there is nothing.
    assert_eq!(event_message("1").shared, None);
}

#[test]
fn history_events_are_flagged() {
    let old = event_message("90");
    assert!(old.flags.history);
    assert!(matches!(old.event, Some(EventKind::Sub { .. })));
    assert_eq!(old.timestamp, Timestamp(1_700_000_099_000));
}

#[test]
fn a_notice_of_another_shared_channel_is_dispatched_on_its_source_kind() {
    // msg-id=sharedchatnotice, the real kind in source-msg-id, with that kind's msg-param-*.
    let raid = event_message("100");
    assert_eq!(raid.notice_id.as_deref(), Some("sharedchatnotice"));
    assert_eq!(
        raid.event,
        Some(EventKind::Raid {
            from_login: "bigraider".into(),
            from_name: "BigRaider".into(),
            viewers: 2500,
            profile_image: Some(
                "https://static-cdn.jtvnw.net/jtv_user_pictures/x-profile_image-70x70.png".into()
            )
        })
    );
    // It is marked as coming from the other room, so it is drawn as that channel's raid.
    let shared = raid.shared.as_ref().expect("shared");
    assert_eq!(shared.source_room_id, "2000");
    assert!(shared.is_copy_in(raid.raw_tags.get("room-id")));

    let announcement = event_message("101");
    assert_eq!(
        announcement.event,
        Some(EventKind::Announcement {
            color: AnnouncementColor::Blue
        })
    );
    assert_eq!(announcement.system_text, None, "an empty system-msg");
    assert_eq!(
        announcement.plain_text(),
        "Both chats: raffle in ten minutes"
    );
    assert_eq!(
        announcement.shared.as_ref().unwrap().source_badges[0]
            .set
            .as_ref(),
        "moderator"
    );

    assert_eq!(
        event_message("102").event,
        Some(EventKind::Resub {
            plan: SubPlan::Tier1,
            months: 6,
            streak: None
        })
    );
    assert_eq!(
        event_message("102").shared.as_ref().unwrap().source_room_id,
        "3000"
    );

    // Without a source kind there is nothing to dispatch on.
    let parse = |tags: &str| {
        let line = format!("@id=1;login=x;tmi-sent-ts=5;{tags} :tmi.twitch.tv USERNOTICE #c");
        match parse_line(&line).unwrap().unwrap() {
            ChatEvent::Message { message, .. } => message,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        parse("msg-id=sharedchatnotice").event,
        Some(EventKind::Other {
            msg_id: String::new()
        })
    );
}

#[test]
fn the_gift_bomb_fixture_has_one_row_and_a_line_per_recipient() {
    let mystery = event_message("103");
    let Some(EventKind::MysteryGift {
        count, origin_id, ..
    }) = &mystery.event
    else {
        panic!("a mystery gift")
    };
    assert_eq!((*count, origin_id.as_deref()), (12, Some("77 88 99")));
    let lines: Vec<_> = (104..116)
        .map(|n| match kind(&n.to_string()) {
            EventKind::Gift {
                recipient,
                community_gift_id,
                gifter_total,
                ..
            } => (recipient.name().to_owned(), community_gift_id, gifter_total),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(lines.len(), 12);
    assert!(
        lines
            .iter()
            .all(|(_, id, total)| id.as_deref() == Some("77 88 99") && total.is_none())
    );
    assert_eq!((lines[0].0.as_str(), lines[11].0.as_str()), ("Ann", "Lea"));
}
