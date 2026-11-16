use gpui_chat_core::{Author, Fragment};

use super::*;

fn request(command: ModCommand) -> ModRequest {
    ModRequest {
        channel: ChannelId::new("streamer"),
        room_id: "1000".into(),
        command,
        target_id: None,
    }
}

fn chat() -> Message {
    Message::chat(
        "msg-1",
        Timestamp(1),
        Author::new("alice", "Alice"),
        vec![Fragment::Text("hello".into())],
    )
}

#[test]
fn a_click_on_a_message_builds_the_same_command_as_typing_it() {
    let message = chat();
    assert_eq!(
        RowAction::Delete.command(&message),
        Some(ModCommand::DeleteMessage {
            message_id: "msg-1".into()
        })
    );
    assert_eq!(
        RowAction::Timeout(600).command(&message),
        Some(ModCommand::Timeout {
            user: "alice".into(),
            seconds: 600,
            reason: None
        })
    );
    assert_eq!(
        RowAction::Ban.command(&message),
        Some(ModCommand::Ban {
            user: "alice".into(),
            reason: None
        })
    );
    assert_eq!(
        RowAction::Unban.command(&message),
        Some(ModCommand::Unban {
            user: "alice".into()
        })
    );
    // The same values the parser makes.
    assert_eq!(
        gpui_chat_core::parse_input("/timeout alice 10m"),
        gpui_chat_core::Input::Command(RowAction::Timeout(600).command(&message).unwrap())
    );
    // Nobody to punish on a line nobody wrote; the message itself can still be deleted.
    let system = Message::plain("s", Timestamp(0), MessageKind::System, "x");
    assert_eq!(RowAction::Ban.command(&system), None);
    assert!(RowAction::Delete.command(&system).is_some());
}

#[test]
fn only_a_ban_asks_for_confirmation() {
    let message = chat();
    let (title, body) = confirmation(&RowAction::Ban.command(&message).unwrap()).unwrap();
    assert_eq!(title, "Ban alice?");
    assert!(body.contains("alice"));
    for action in [RowAction::Delete, RowAction::Timeout(600), RowAction::Unban] {
        assert_eq!(confirmation(&action.command(&message).unwrap()), None);
    }
    assert_eq!(confirmation(&ModCommand::ClearChat), None);
}

#[test]
fn the_labels_read_as_actions() {
    assert_eq!(RowAction::Delete.label(), "Delete message");
    assert_eq!(RowAction::Timeout(600).label(), "Timeout 10m");
    assert_eq!(RowAction::Timeout(86_400).label(), "Timeout 1d");
    assert_eq!(RowAction::Ban.label(), "Ban");
}

#[test]
fn removed_messages_collapse_only_when_asked_and_not_revealed() {
    let live = chat();
    let removed = chat().with_deleted();
    for mode in [DeletedMessages::Strikethrough, DeletedMessages::Collapse] {
        assert!(!is_collapsed(&live, mode, false), "{mode:?}");
    }
    assert!(!is_collapsed(
        &removed,
        DeletedMessages::Strikethrough,
        false
    ));
    assert!(is_collapsed(&removed, DeletedMessages::Collapse, false));
    assert!(
        !is_collapsed(&removed, DeletedMessages::Collapse, true),
        "revealed"
    );
    // A system line is never collapsed, whatever its flag says.
    let mut line = Message::plain("s", Timestamp(0), MessageKind::System, "x").with_deleted();
    line.flags.deleted = true;
    assert!(!is_collapsed(&line, DeletedMessages::Collapse, false));
}

#[test]
fn the_loopback_turns_a_command_into_the_event_the_server_would_send() {
    let at = Timestamp(9_000);
    let events = |command| loopback_events(&request(command), at);
    assert_eq!(
        events(ModCommand::Timeout {
            user: "alice".into(),
            seconds: 600,
            reason: None
        }),
        [ChatEvent::ClearChat {
            channel: ChannelId::new("streamer"),
            target: ClearTarget::User {
                login: "alice".into(),
                duration: Some(Duration::from_secs(600))
            },
            at: Some(at),
        }]
    );
    assert_eq!(
        events(ModCommand::Ban {
            user: "alice".into(),
            reason: None
        }),
        [ChatEvent::ClearChat {
            channel: ChannelId::new("streamer"),
            target: ClearTarget::User {
                login: "alice".into(),
                duration: None
            },
            at: Some(at),
        }]
    );
    assert_eq!(
        events(ModCommand::DeleteMessage {
            message_id: "m1".into()
        }),
        [ChatEvent::DeleteMessage {
            channel: ChannelId::new("streamer"),
            message_id: "m1".into(),
            login: None
        }]
    );
    assert!(matches!(
        events(ModCommand::ClearChat).as_slice(),
        [ChatEvent::ClearChat {
            target: ClearTarget::Channel,
            ..
        }]
    ));
}

#[test]
fn the_loopback_updates_the_room_like_a_roomstate() {
    let update = |change| {
        let events = loopback_events(&request(ModCommand::Room(change)), Timestamp(1));
        let [ChatEvent::RoomState { update, .. }] = events.as_slice() else {
            panic!("one ROOMSTATE, got {events:?}")
        };
        update.clone()
    };
    assert_eq!(
        update(RoomChange::Slow(Some(30))).slow_mode,
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        update(RoomChange::Slow(None)).slow_mode,
        Some(Duration::ZERO)
    );
    assert_eq!(
        update(RoomChange::Followers(Some(0))).followers_only,
        Some(FollowersOnly::All)
    );
    assert_eq!(
        update(RoomChange::Followers(Some(1440))).followers_only,
        Some(FollowersOnly::Minutes(1440))
    );
    assert_eq!(
        update(RoomChange::Followers(None)).followers_only,
        Some(FollowersOnly::Off)
    );
    assert_eq!(update(RoomChange::EmoteOnly(true)).emote_only, Some(true));
    assert_eq!(
        update(RoomChange::Subscribers(false)).subscribers_only,
        Some(false)
    );
    assert_eq!(update(RoomChange::Unique(true)).unique_chat, Some(true));
}

#[test]
fn the_loopback_says_what_a_command_without_an_echo_did() {
    let events = loopback_events(
        &request(ModCommand::AddMod {
            user: "alice".into(),
        }),
        Timestamp(5),
    );
    let [ChatEvent::Message { message, .. }] = events.as_slice() else {
        panic!("one line, got {events:?}")
    };
    assert_eq!(message.kind, MessageKind::System);
    assert_eq!(message.plain_text(), "alice is now a moderator.");
    // An unban has no echo in chat and nothing to confirm.
    assert!(
        loopback_events(
            &request(ModCommand::Unban { user: "a".into() }),
            Timestamp(5)
        )
        .is_empty()
    );
}

#[test]
fn an_event_card_is_never_collapsed() {
    let mut card = chat().with_deleted();
    card.kind = MessageKind::UserNotice;
    assert!(!is_collapsed(&card, DeletedMessages::Collapse, false));
    let mut action = chat().with_deleted();
    action.kind = MessageKind::Action;
    assert!(is_collapsed(&action, DeletedMessages::Collapse, false));
}

#[test]
fn failures_say_what_to_do_about_them() {
    let missing = describe(&ModError::MissingScope {
        scope: "moderator:manage:banned_users".into(),
    });
    assert_eq!(missing.kind, FailureKind::MissingScope);
    assert!(
        missing
            .message
            .starts_with("Log in again to enable moderation")
    );
    let forbidden = describe(&ModError::Rejected {
        status: 403,
        message: "You are not a moderator.".into(),
    });
    assert_eq!(forbidden.kind, FailureKind::Forbidden);
    assert!(forbidden.message.starts_with("You are not a moderator."));
    assert!(
        forbidden.message.contains("hidden"),
        "{}",
        forbidden.message
    );
    let other = describe(&ModError::Rejected {
        status: 400,
        message: "nope".into(),
    });
    assert_eq!(
        (other.kind, other.message.as_str()),
        (FailureKind::Other, "nope")
    );
    assert_eq!(describe(&ModError::RateLimited).kind, FailureKind::Other);
}
