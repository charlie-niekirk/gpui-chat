//! Highlights, unread dots, the Mentions log and notifications (PR 5.3): the app-side half of
//! `gpui_chat_core::highlight`.
//!
//! Every chat event passes [`Attention::process`] on its way into the channel stores
//! ([`crate::channels::ChannelHub::apply_batch`]): a message the rules hit is stamped
//! (`flags.highlight`, and `flags.mention` for the user's own name, which also covers history,
//! whose lines are parsed without knowing the user), counted as unread / an unseen mention when
//! its channel is not being watched, logged for the Mentions view and, when a rule asks for it,
//! raised as a system notification. [`Core`] is all of that without GPUI, so it is tested without
//! a window; [`Attention`] is the entity around it (settings, the account's login, the timer of the
//! notification summary, what a click on a notification does).
//!
//! Rules are applied to messages **as they arrive**: editing the rules does not recolour what is
//! already on screen, like a theme change does not rewrite history either.

use std::sync::Arc;
use std::time::Instant;

use gpui_chat_core::{
    ChannelId, ChatEvent, ClearTarget, HighlightConfig, Highlights, Hit, Ignores, Message, Own,
};
use gpui_kit::{App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task};

use crate::activity::{Activity, is_countable};
use crate::config;
use crate::notifications::{self, Coalescer, Target, Verdict};
use crate::platform::notify::{self, Notification};

/// The login the offline sources (`--firehose`, `--replay`) act as, so mentions can be tried
/// without an account.
pub const OFFLINE_LOGIN: &str = crate::firehose::OWN_LOGIN;

/// What [`Core::process`] wants done besides changing the events.
#[derive(Debug, Default)]
pub struct Processed {
    /// What the sidebar or the Mentions view shows changed.
    pub changed: bool,
    /// Notifications to post now.
    pub notifications: Vec<Notification>,
    /// A hit was held back: a summary is due at [`Coalescer::summary_due`].
    pub held: bool,
    /// Tags of notifications to take back (their message was removed by a moderator).
    pub dismiss: Vec<String>,
}

/// The state and rules of [`Attention`], with no GPUI.
pub struct Core {
    engine: Highlights,
    config: HighlightConfig,
    pub activity: Activity,
    pub coalescer: Coalescer,
    /// The user's display name as Twitch writes it, learned from their own messages (the login
    /// alone is known from the account).
    display_name: Option<String>,
    /// What is ignored (PR 5.7): such a message is drawn by the chat view as the user decided, but
    /// counts for no dot, no Mentions entry and no notification.
    ignores: Arc<Ignores>,
}

impl Core {
    pub fn new(config: HighlightConfig) -> Self {
        let (engine, _) = Highlights::compile(&config);
        Self {
            engine,
            config,
            activity: Activity::default(),
            coalescer: Coalescer::default(),
            display_name: None,
            ignores: Arc::default(),
        }
    }

    /// Takes what is ignored now. Whether the Mentions log changed because of it (entries of
    /// users who are ignored now leave it).
    pub fn set_ignores(&mut self, ignores: Arc<Ignores>) -> bool {
        let dropped = self.activity.drop_ignored(&ignores);
        self.ignores = ignores;
        dropped
    }

    /// Takes new rules (what `[highlights]` says now). Whether they differ from the ones in force.
    pub fn set_config(&mut self, config: &HighlightConfig) -> bool {
        if &self.config == config {
            return false;
        }
        let own = self.engine.own().cloned();
        let (mut engine, _) = Highlights::compile(config);
        engine.set_own(own);
        self.engine = engine;
        self.config = config.clone();
        if !config.notifications {
            // Nothing held may fire after the user switched notifications off.
            self.coalescer.clear();
        }
        true
    }

    /// Stamps the rules' verdict on the messages of `events`, counts activity, logs hits and
    /// decides on notifications. `own` is the user's login (`None`: anonymous). `is_copy` says a
    /// message is the copy of one that a channel the user has open shows too (shared chat): it is
    /// drawn like any other, but counts for nothing, so one line is one hit.
    pub fn process(
        &mut self,
        own: Option<&str>,
        events: &mut [ChatEvent],
        is_copy: &dyn Fn(&Message) -> bool,
        now: Instant,
    ) -> Processed {
        let mut out = Processed::default();
        self.engine
            .set_own(own.map(|login| Own::new(login, self.display_name.clone())));
        for event in events {
            match event {
                ChatEvent::Message { channel, message } => {
                    self.learn_name(own, message);
                    let hit = self.engine.evaluate(message);
                    let live = is_countable(message);
                    stamp(message, hit, &self.engine);
                    if !live
                        || self.engine.is_own(message)
                        || is_copy(message)
                        || self.ignores.is_ignored(message)
                    {
                        continue;
                    }
                    out.changed |= self.activity.note_message(channel);
                    let Some(hit) = hit else { continue };
                    let color = hit.color;
                    out.changed |= self.activity.note_hit(channel, message.clone(), color);
                    if hit.notify && self.config.notifications && !self.activity.is_watched(channel)
                    {
                        match self.coalescer.offer(now, channel, &message.id) {
                            Verdict::Send => {
                                let (title, body) = notifications::describe(
                                    message,
                                    channel,
                                    hit.reason,
                                    self.config.notification_previews,
                                );
                                out.notifications.push(Notification {
                                    tag: notifications::message_tag(channel, &message.id),
                                    title,
                                    body,
                                });
                            }
                            Verdict::Held => out.held = true,
                        }
                    }
                }
                // Recent messages are only drawn: stamped (so the user's name stands out in them
                // too) but never counted or notified.
                ChatEvent::History { messages, .. } => {
                    for message in messages {
                        let hit = self.engine.evaluate(message);
                        stamp(message, hit, &self.engine);
                    }
                }
                ChatEvent::DeleteMessage {
                    channel,
                    message_id,
                    ..
                } => {
                    let gone = self.activity.message_removed(channel, message_id);
                    out.changed |= !gone.is_empty();
                    // Whatever was said about it is taken back, logged or not.
                    self.coalescer.forget(channel, message_id);
                    out.dismiss
                        .push(notifications::message_tag(channel, message_id));
                }
                ChatEvent::ClearChat {
                    channel, target, ..
                } => {
                    let login = match target {
                        ClearTarget::Channel => None,
                        ClearTarget::User { login, .. } => Some(login.as_str()),
                    };
                    let gone = self.activity.user_removed(channel, login);
                    out.changed |= !gone.is_empty();
                    for id in gone {
                        self.coalescer.forget(channel, &id);
                        out.dismiss.push(notifications::message_tag(channel, &id));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// The summary of held notifications, once it is due.
    pub fn summary(&mut self, now: Instant) -> Option<Notification> {
        if !self.config.notifications {
            self.coalescer.clear();
            return None;
        }
        let (count, channels) = self.coalescer.next_summary(now)?;
        let (title, body) = notifications::summary_text(count, &channels);
        Some(Notification {
            tag: notifications::summary_tag(),
            title,
            body,
        })
    }

    fn learn_name(&mut self, own: Option<&str>, message: &Message) {
        let (Some(own), Some(author)) = (own, &message.author) else {
            return;
        };
        if author.login.eq_ignore_ascii_case(own)
            && !author.display_name.trim().is_empty()
            && self.display_name.as_deref() != Some(author.display_name.as_str())
        {
            self.display_name = Some(author.display_name.clone());
            self.engine
                .set_own(Some(Own::new(own, self.display_name.clone())));
        }
    }
}

/// Puts the verdict on the message: the rule colour, and whether it names the user (so the parser's
/// guess, which knows only the login, gives way). Messages the rules do not apply to (the user's
/// own, lines nobody wrote) are left as they are.
fn stamp(message: &mut Arc<Message>, hit: Option<Hit>, engine: &Highlights) {
    let applies = matches!(
        message.kind,
        gpui_chat_core::MessageKind::Chat
            | gpui_chat_core::MessageKind::Action
            | gpui_chat_core::MessageKind::UserNotice
    ) && !engine.is_own(message);
    if !applies {
        return;
    }
    let (color, mention) = hit.map_or((None, false), |hit| (Some(hit.color), hit.mention));
    if message.flags.highlight == color && message.flags.mention == mention {
        return;
    }
    let mut stamped = (**message).clone();
    stamped.flags.highlight = color;
    stamped.flags.mention = mention;
    *message = Arc::new(stamped);
}

/// What the app should show after a notification was clicked (or any other request to jump).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionEvent {
    Show(Target),
}

impl EventEmitter<AttentionEvent> for Attention {}

/// The entity every window reads the sidebar dots and the Mentions log from.
pub struct Attention {
    core: Core,
    /// The offline sources have no account; they act as [`OFFLINE_LOGIN`].
    offline: bool,
    summary: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Attention {
    /// Creates the entity and hooks up notification clicks. `offline`: `--firehose` / `--replay`.
    pub fn start(offline: bool, cx: &mut App) -> Entity<Self> {
        let entity = cx.new(|cx| {
            let subscription = cx.observe(&config::store(cx), |this: &mut Self, _, cx| {
                if this
                    .core
                    .set_config(&config::current(cx).highlights.clone())
                {
                    cx.notify();
                }
            });
            let mut subscriptions = vec![subscription];
            let mut core = Core::new(config::current(cx).highlights.clone());
            // What is ignored (PR 5.7): counted for nothing, and out of the Mentions log.
            if let Some(hub) = crate::ignores::current(cx) {
                core.set_ignores(hub.read(cx).engine());
                subscriptions.push(cx.observe(&hub, |this: &mut Self, hub, cx| {
                    if this.core.set_ignores(hub.read(cx).engine()) {
                        cx.notify();
                    }
                }));
            }
            Self {
                core,
                offline,
                summary: None,
                _subscriptions: subscriptions,
            }
        });
        let weak = entity.downgrade();
        notify::on_activate(cx, move |tag, cx| {
            let Some(target) = notifications::parse_tag(tag) else {
                return;
            };
            cx.activate(true);
            weak.update(cx, |_, cx| cx.emit(AttentionEvent::Show(target)))
                .ok();
        });
        entity
    }

    pub fn activity(&self) -> &Activity {
        &self.core.activity
    }

    /// The user the rules act for: the logged-in account, or the offline stand-in.
    fn own_login(&self, cx: &App) -> Option<String> {
        if self.offline {
            Some(OFFLINE_LOGIN.to_owned())
        } else {
            crate::account::own_login(cx)
        }
    }

    /// Runs `events` (one frame's batch, before the stores take it) through the rules; see the
    /// module docs.
    pub fn process(
        &mut self,
        events: &mut [ChatEvent],
        is_copy: &dyn Fn(&Message) -> bool,
        cx: &mut Context<Self>,
    ) {
        let own = self.own_login(cx);
        let out = self
            .core
            .process(own.as_deref(), events, is_copy, Instant::now());
        for notification in out.notifications {
            notify::show(notification, cx);
        }
        for tag in &out.dismiss {
            notify::dismiss(tag, cx);
        }
        if out.held {
            self.schedule_summary(cx);
        }
        if out.changed {
            cx.notify();
        }
    }

    /// What is on screen: the channels shown by a pane, the focused one and whether the window is
    /// the active one. Clears what the user is now looking at.
    pub fn set_view(
        &mut self,
        visible: Vec<ChannelId>,
        focused: Option<ChannelId>,
        window_active: bool,
        cx: &mut Context<Self>,
    ) {
        if self.core.activity.set_view(visible, focused, window_active) {
            cx.notify();
        }
    }

    /// The user looked at `channel`'s messages (jumped to one): its dots go.
    pub fn mark_seen(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        if self.core.activity.mark_seen(channel) {
            cx.notify();
        }
    }

    /// The channel was closed.
    pub fn forget(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        if self.core.activity.forget(channel) {
            cx.notify();
        }
    }

    fn schedule_summary(&mut self, cx: &mut Context<Self>) {
        if self.summary.is_some() {
            return;
        }
        let Some(due) = self.core.coalescer.summary_due() else {
            return;
        };
        let delay = due.saturating_duration_since(Instant::now());
        self.summary = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                this.summary = None;
                match this.core.summary(Instant::now()) {
                    Some(notification) => notify::show(notification, cx),
                    // The timer fired a hair early: wait for the rest.
                    None => this.schedule_summary(cx),
                }
            })
            .ok();
        }));
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{
        Author, Fragment, HighlightColor, HighlightRule, HighlightUser, MessageId, Timestamp,
    };

    use super::*;
    use crate::notifications::MIN_GAP;

    fn chat(channel: &str, id: &str, login: &str, text: &str) -> ChatEvent {
        ChatEvent::Message {
            channel: ChannelId::new(channel),
            message: Arc::new(Message::chat(
                id,
                Timestamp(0),
                Author::new(login, login),
                vec![Fragment::Text(text.into())],
            )),
        }
    }

    fn history(channel: &str, login: &str, text: &str) -> ChatEvent {
        let mut message = Message::chat(
            "h",
            Timestamp(0),
            Author::new(login, login),
            vec![Fragment::Text(text.into())],
        );
        message.flags.history = true;
        ChatEvent::History {
            channel: ChannelId::new(channel),
            messages: vec![Arc::new(message)],
            moderation: Vec::new(),
        }
    }

    fn flags(event: &ChatEvent) -> gpui_chat_core::MessageFlags {
        match event {
            ChatEvent::Message { message, .. } => message.flags,
            ChatEvent::History { messages, .. } => messages[0].flags,
            other => panic!("{other:?}"),
        }
    }

    fn run(core: &mut Core, events: &mut [ChatEvent]) -> Processed {
        core.process(Some("viewer"), events, &|_| false, Instant::now())
    }

    fn core_watching(config: HighlightConfig, watching: &str) -> Core {
        let mut core = Core::new(config);
        core.activity.set_view(
            [ChannelId::new(watching)],
            Some(ChannelId::new(watching)),
            true,
        );
        core
    }

    #[test]
    fn a_mention_is_stamped_counted_logged_and_notified_when_the_channel_is_not_watched() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let mut events = [chat("b", "1", "alice", "hello @Viewer!")];
        let out = run(&mut core, &mut events);
        let f = flags(&events[0]);
        assert!(f.mention);
        assert_eq!(f.highlight, Some(HighlightColor::Accent));
        assert!(out.changed);
        assert_eq!(out.notifications.len(), 1);
        assert_eq!(out.notifications[0].title, "alice mentioned you in #b");
        assert_eq!(out.notifications[0].tag, "hit:b:1");
        let b = ChannelId::new("b");
        assert_eq!(core.activity.unread(&b), 1);
        assert_eq!(core.activity.unseen_mentions(&b), 1);
    }

    #[test]
    fn a_watched_channel_counts_for_nothing_and_raises_no_notification() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let mut events = [chat("a", "1", "alice", "hi viewer")];
        let out = run(&mut core, &mut events);
        assert!(flags(&events[0]).mention, "still drawn as a mention");
        assert!(out.notifications.is_empty());
        let a = ChannelId::new("a");
        assert_eq!(
            (core.activity.unread(&a), core.activity.unseen_mentions(&a)),
            (0, 0)
        );
        assert_eq!(
            core.activity.mention_count(),
            1,
            "but it is in the Mentions log"
        );
    }

    #[test]
    fn the_users_own_messages_never_count_or_match() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let mut events = [chat("b", "1", "Viewer", "my own words, viewer")];
        let out = run(&mut core, &mut events);
        assert!(!out.changed && out.notifications.is_empty());
        assert_eq!(core.activity.unread(&ChannelId::new("b")), 0);
        assert_eq!(flags(&events[0]).highlight, None);
    }

    fn ignoring(users: &[&str], phrases: &[&str]) -> Arc<Ignores> {
        use gpui_chat_core::{IgnoreConfig, IgnoreRule};
        let (engine, problems) = Ignores::compile(&IgnoreConfig {
            users: users.iter().map(|u| (*u).to_owned()).collect(),
            rules: phrases
                .iter()
                .map(|pattern| IgnoreRule {
                    pattern: (*pattern).to_owned(),
                    ..IgnoreRule::default()
                })
                .collect(),
            ..IgnoreConfig::default()
        });
        assert!(problems.is_empty());
        Arc::new(engine)
    }

    #[test]
    fn an_ignored_message_counts_for_no_dot_no_log_entry_and_no_notification() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        core.set_ignores(ignoring(&["spammer"], &["buy followers"]));
        let b = ChannelId::new("b");
        for (id, login, text) in [
            ("1", "spammer", "hey @viewer"),
            ("2", "alice", "viewer: buy followers now"),
        ] {
            let out = run(&mut core, &mut [chat("b", id, login, text)]);
            assert!(!out.changed && out.notifications.is_empty(), "{id}");
        }
        assert_eq!(core.activity.unread(&b), 0);
        assert_eq!(core.activity.mention_count(), 0);
        // Anyone else still counts.
        let out = run(&mut core, &mut [chat("b", "3", "alice", "hello viewer")]);
        assert!(out.changed);
        assert_eq!(core.activity.unseen_mentions(&b), 1);
    }

    #[test]
    fn ignoring_someone_takes_their_earlier_hits_out_of_the_log() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        run(&mut core, &mut [chat("b", "1", "spammer", "hi viewer")]);
        run(&mut core, &mut [chat("b", "2", "alice", "hi viewer")]);
        let b = ChannelId::new("b");
        assert_eq!(core.activity.unseen_mentions(&b), 2);
        assert!(core.set_ignores(ignoring(&["spammer"], &[])));
        assert_eq!(core.activity.unseen_mentions(&b), 1);
        assert_eq!(core.activity.mention_count(), 1);
        assert!(
            !core.set_ignores(ignoring(&["spammer"], &[])),
            "nothing more to drop"
        );
    }

    #[test]
    fn history_is_stamped_for_display_but_never_counts_or_notifies() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let mut events = [history("b", "alice", "hey viewer")];
        let out = run(&mut core, &mut events);
        assert!(flags(&events[0]).mention && flags(&events[0]).history);
        assert!(!out.changed && out.notifications.is_empty());
        let b = ChannelId::new("b");
        assert_eq!(
            (core.activity.unread(&b), core.activity.mention_count()),
            (0, 0)
        );
        // A live message already flagged as history (a replayed copy) does not count either.
        let mut live = chat("b", "2", "alice", "viewer");
        if let ChatEvent::Message { message, .. } = &mut live {
            Arc::make_mut(message).flags.history = true;
        }
        assert!(!run(&mut core, &mut [live]).changed);
    }

    #[test]
    fn removed_messages_do_not_count() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let mut removed = chat("b", "1", "alice", "viewer");
        if let ChatEvent::Message { message, .. } = &mut removed {
            Arc::make_mut(message).flags.deleted = true;
        }
        assert!(!run(&mut core, &mut [removed]).changed);
        // One that is removed afterwards leaves the log.
        run(&mut core, &mut [chat("b", "2", "alice", "viewer")]);
        let b = ChannelId::new("b");
        assert_eq!(core.activity.unseen_mentions(&b), 1);
        let out = run(
            &mut core,
            &mut [ChatEvent::DeleteMessage {
                channel: b.clone(),
                message_id: MessageId::from("2"),
                login: None,
            }],
        );
        assert!(out.changed);
        assert_eq!(core.activity.unseen_mentions(&b), 0);
    }

    #[test]
    fn plain_messages_only_make_the_unread_dot() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let mut events = [chat("b", "1", "alice", "just chatting")];
        let out = run(&mut core, &mut events);
        assert!(out.changed && out.notifications.is_empty());
        assert_eq!(flags(&events[0]).highlight, None);
        let b = ChannelId::new("b");
        assert_eq!(
            (core.activity.unread(&b), core.activity.mention_count()),
            (1, 0)
        );
        // The second message changes no dot.
        assert!(!run(&mut core, &mut [chat("b", "2", "alice", "more")]).changed);
    }

    #[test]
    fn the_parsers_mention_flag_gives_way_to_the_rules() {
        let off = HighlightConfig {
            mentions: false,
            ..HighlightConfig::default()
        };
        let mut core = core_watching(off, "a");
        let mut event = chat("a", "1", "alice", "viewer");
        if let ChatEvent::Message { message, .. } = &mut event {
            Arc::make_mut(message).flags.mention = true; // what the parser found
        }
        let mut events = [event];
        run(&mut core, &mut events);
        assert!(!flags(&events[0]).mention);
    }

    #[test]
    fn a_rule_without_notify_highlights_quietly_and_a_user_rule_colours_the_row() {
        let config = HighlightConfig {
            rules: vec![HighlightRule {
                pattern: "hype".into(),
                color: HighlightColor::Yellow,
                ..HighlightRule::default()
            }],
            users: vec![HighlightUser {
                login: "Streamer".into(),
                color: HighlightColor::Green,
                notify: true,
            }],
            ..HighlightConfig::default()
        };
        let mut core = core_watching(config, "a");
        let mut events = [
            chat("b", "1", "alice", "so much hype"),
            chat("b", "2", "streamer", "hello"),
        ];
        let out = run(&mut core, &mut events);
        assert_eq!(flags(&events[0]).highlight, Some(HighlightColor::Yellow));
        assert_eq!(flags(&events[1]).highlight, Some(HighlightColor::Green));
        assert_eq!(
            out.notifications.len(),
            1,
            "only the user rule asked to notify"
        );
        assert_eq!(out.notifications[0].title, "streamer in #b");
    }

    #[test]
    fn notifications_can_be_switched_off_and_the_highlight_stays() {
        let config = HighlightConfig {
            notifications: false,
            ..HighlightConfig::default()
        };
        let mut core = core_watching(config, "a");
        let mut events = [chat("b", "1", "alice", "viewer")];
        let out = run(&mut core, &mut events);
        assert!(out.notifications.is_empty() && !out.held);
        assert!(flags(&events[0]).mention);
        assert_eq!(core.activity.unseen_mentions(&ChannelId::new("b")), 1);
    }

    #[test]
    fn a_burst_of_hits_is_one_notification_and_a_summary() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let t0 = Instant::now();
        let mut first = [chat("b", "1", "alice", "viewer")];
        let out = core.process(Some("viewer"), &mut first, &|_| false, t0);
        assert_eq!(out.notifications.len(), 1);
        let mut more = [
            chat("b", "2", "alice", "viewer"),
            chat("c", "3", "alice", "viewer"),
        ];
        let out = core.process(Some("viewer"), &mut more, &|_| false, t0);
        assert!(out.notifications.is_empty() && out.held);
        assert!(core.summary(t0).is_none(), "not yet");
        let summary = core.summary(t0 + MIN_GAP).unwrap();
        assert_eq!(summary.title, "2 more highlights");
        assert_eq!(summary.body, "in #b, #c");
        assert_eq!(summary.tag, "summary");
    }

    #[test]
    fn the_display_name_is_learned_from_the_users_own_messages() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let mut own = ChatEvent::Message {
            channel: ChannelId::new("a"),
            message: Arc::new(Message::chat(
                "0",
                Timestamp(0),
                Author::new("viewer", "Vïewer"),
                vec![Fragment::Text("hi".into())],
            )),
        };
        run(&mut core, std::slice::from_mut(&mut own));
        let mut events = [chat("b", "1", "alice", "hey VÏEWER")];
        run(&mut core, &mut events);
        assert!(flags(&events[0]).mention);
    }

    #[test]
    fn nobody_is_mentioned_without_a_login_and_new_rules_apply_to_new_messages() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let mut events = [chat("b", "1", "alice", "viewer")];
        core.process(None, &mut events, &|_| false, Instant::now());
        assert_eq!(flags(&events[0]).highlight, None);
        assert!(!core.set_config(&HighlightConfig::default()), "unchanged");
        let config = HighlightConfig {
            rules: vec![HighlightRule {
                pattern: "newrule".into(),
                ..HighlightRule::default()
            }],
            ..HighlightConfig::default()
        };
        assert!(core.set_config(&config));
        let mut events = [chat("b", "2", "alice", "a newrule")];
        run(&mut core, &mut events);
        assert!(flags(&events[0]).highlight.is_some());
    }

    #[test]
    fn a_timeout_takes_the_users_hits_out_of_the_log() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        run(&mut core, &mut [chat("b", "1", "troll", "viewer")]);
        let out = run(
            &mut core,
            &mut [ChatEvent::ClearChat {
                channel: ChannelId::new("b"),
                target: ClearTarget::User {
                    login: "troll".into(),
                    duration: None,
                },
                at: None,
            }],
        );
        assert!(out.changed);
        assert_eq!(core.activity.mention_count(), 0);
    }

    #[test]
    fn a_removed_message_takes_its_banner_and_its_place_in_the_summary_with_it() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let t0 = Instant::now();
        let mut events = [
            chat("b", "1", "alice", "viewer"),
            chat("b", "2", "alice", "viewer again"),
            chat("c", "3", "bob", "viewer too"),
        ];
        let out = core.process(Some("viewer"), &mut events, &|_| false, t0);
        assert_eq!(
            out.notifications.len(),
            1,
            "the first is sent, two are held"
        );
        let (b, c) = (ChannelId::new("b"), ChannelId::new("c"));
        // Deleting a held message: its banner (if any) is dismissed, and it is not summarised.
        let out = core.process(
            Some("viewer"),
            &mut [ChatEvent::DeleteMessage {
                channel: b.clone(),
                message_id: MessageId::from("2"),
                login: None,
            }],
            &|_| false,
            t0,
        );
        assert_eq!(out.dismiss, ["hit:b:2"]);
        // Deleting the one that was sent dismisses its banner too (logged or not).
        let out = core.process(
            Some("viewer"),
            &mut [ChatEvent::DeleteMessage {
                channel: b.clone(),
                message_id: MessageId::from("1"),
                login: None,
            }],
            &|_| false,
            t0,
        );
        assert_eq!(out.dismiss, ["hit:b:1"]);
        let summary = core.summary(t0 + MIN_GAP).unwrap();
        assert_eq!(summary.title, "1 more highlight");
        assert_eq!(summary.body, "in #c");
        // A timeout dismisses every banner of that user's logged hits.
        let out = core.process(
            Some("viewer"),
            &mut [ChatEvent::ClearChat {
                channel: c,
                target: ClearTarget::User {
                    login: "bob".into(),
                    duration: None,
                },
                at: None,
            }],
            &|_| false,
            t0 + MIN_GAP,
        );
        assert_eq!(out.dismiss, ["hit:c:3"]);
    }

    #[test]
    fn a_held_summary_does_not_fire_after_notifications_are_switched_off() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let t0 = Instant::now();
        let mut events = [
            chat("b", "1", "alice", "viewer"),
            chat("b", "2", "alice", "viewer"),
        ];
        core.process(Some("viewer"), &mut events, &|_| false, t0);
        assert!(core.coalescer.summary_due().is_some());
        let off = HighlightConfig {
            notifications: false,
            ..HighlightConfig::default()
        };
        assert!(core.set_config(&off));
        assert_eq!(core.summary(t0 + MIN_GAP), None);
        assert!(
            core.coalescer.summary_due().is_none(),
            "nothing is held any more"
        );
    }

    #[test]
    fn previews_off_keeps_the_text_and_the_author_out_of_the_notification() {
        let config = HighlightConfig {
            notification_previews: false,
            ..HighlightConfig::default()
        };
        let mut core = core_watching(config, "a");
        let mut events = [chat("b", "1", "alice", "my secret, viewer")];
        let out = run(&mut core, &mut events);
        assert_eq!(out.notifications[0].title, "New highlight in #b");
        assert_eq!(out.notifications[0].body, "");
        // The row itself and the Mentions log are unaffected.
        assert!(flags(&events[0]).mention);
        assert_eq!(core.activity.mention_count(), 1);
    }

    #[test]
    fn the_copy_of_a_line_an_open_channel_shows_is_drawn_but_not_counted() {
        let mut core = core_watching(HighlightConfig::default(), "a");
        let mut events = [chat("b", "1", "alice", "hi viewer")];
        let out = core.process(Some("viewer"), &mut events, &|_| true, Instant::now());
        assert!(flags(&events[0]).mention, "still a mention on screen");
        assert!(!out.changed && out.notifications.is_empty());
        assert_eq!(core.activity.mention_count(), 0);
        assert_eq!(core.activity.unread(&ChannelId::new("b")), 0);
    }
}
