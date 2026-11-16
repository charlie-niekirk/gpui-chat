//! Message history for the live source (PR 5.5): recent messages when a channel is joined and the
//! gap after a reconnect, from the public recent-messages service.
//!
//! The decisions (when to fetch, what to ask for, whether an answer is still wanted) are
//! `gpui_chat_twitch::HistoryTracker`'s and are tested there; this is the glue.
//!
//! * Every event on its way from the Twitch client to the UI passes [`HistoryService::observe`]
//!   (on the backend thread, like [`crate::emote_service::EmoteService::process`]). A channel's
//!   join confirmation (`Connected`, which a reconnect repeats for every channel) starts a fetch on
//!   the backend runtime; it never delays the join or any other event.
//! * The answer goes into the bridge as one [`ChatEvent::History`]: the channel store merges it by
//!   time and message id (`MessageStore::merge_history`), so live messages that arrived meanwhile
//!   keep their place and nothing shows twice. Third-party emotes are resolved on the way, as for
//!   live messages (sets that have not loaded yet leave words as text, as they do for the first
//!   live messages).
//! * A failure (timeout, HTTP error, the service refusing) is a `warn` in the log and one quiet
//!   system line in the channel; chat itself is unaffected.
//! * `chat.history = false` ([`HistoryService::set_enabled`]) makes joins fetch nothing: the
//!   channel name is not sent to the service at all. The setting applies to the next join or
//!   reconnect.
//!
//! Offline sources (`--firehose`, `--replay`) have no service: nothing is fetched.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use gpui_chat_core::{ChannelId, ChatEvent, Message, MessageKind, Timestamp};
use gpui_chat_twitch::{Fetch, HistoryClient, HistoryTracker};
use gpui_kit::App;
use tokio::runtime::Handle;

use crate::bridge::BridgeSender;
use crate::emote_service::EmoteService;
use crate::live::now;
use crate::runtime;

struct Inner {
    client: HistoryClient,
    handle: Handle,
    tracker: Mutex<HistoryTracker>,
    sender: BridgeSender<ChatEvent>,
    emotes: Option<EmoteService>,
}

impl Inner {
    fn tracker(&self) -> MutexGuard<'_, HistoryTracker> {
        self.tracker.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Cheap to clone; all clones share one tracker.
#[derive(Clone)]
pub struct HistoryService {
    inner: Arc<Inner>,
}

impl HistoryService {
    /// Starts the service on the backend runtime. Fetches nothing until a channel is joined
    /// (and never, while `enabled` is off). `None` if the HTTP client cannot be built.
    pub fn start(
        enabled: bool,
        sender: BridgeSender<ChatEvent>,
        emotes: Option<EmoteService>,
        cx: &App,
    ) -> Option<Self> {
        let client = match runtime::enter(cx, HistoryClient::new) {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(%error, "message history unavailable");
                return None;
            }
        };
        Some(Self {
            inner: Arc::new(Inner {
                client,
                handle: cx.global::<runtime::Backend>().handle(),
                tracker: Mutex::new(HistoryTracker::new(enabled)),
                sender,
                emotes,
            }),
        })
    }

    /// Applies `chat.history`: whether the joins that follow fetch anything.
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.tracker().set_enabled(enabled);
    }

    /// Looks at an event on its way to the UI; a channel's join confirmation starts its fetch.
    pub fn observe(&self, event: &ChatEvent) {
        let fetch = self.inner.tracker().observe(event);
        if let Some(fetch) = fetch {
            self.spawn(fetch);
        }
    }

    /// A channel was closed: forget it, and discard a fetch still running for it.
    pub fn forget(&self, channel: &ChannelId) {
        self.inner.tracker().forget(channel);
    }

    fn spawn(&self, fetch: Fetch) {
        let inner = self.inner.clone();
        drop(self.inner.handle.spawn(async move {
            let Fetch {
                channel,
                token,
                request,
            } = fetch;
            let outcome = inner.client.fetch(&channel, request).await;
            let wanted = inner.tracker().complete(
                &channel,
                token,
                outcome
                    .as_ref()
                    .ok()
                    .map(|history| history.messages.as_slice()),
            );
            if !wanted {
                return;
            }
            let event = match outcome {
                Ok(history) => {
                    let missing = history.may_be_missing_messages(&request);
                    if history.messages.is_empty() && history.moderation.is_empty() {
                        return;
                    }
                    tracing::debug!(
                        %channel,
                        count = history.messages.len(),
                        moderation = history.moderation.len(),
                        missing,
                        "recent messages loaded"
                    );
                    let mut messages: Vec<Arc<Message>> = history
                        .messages
                        .into_iter()
                        .map(|message| resolve(inner.emotes.as_ref(), &channel, message))
                        .collect();
                    if missing && let Some(oldest) = messages.first() {
                        // Right before the oldest message the answer reached back to: where the
                        // hole is.
                        let notice = possibly_missing(token, oldest.timestamp);
                        messages.insert(0, Arc::new(notice));
                    }
                    ChatEvent::History {
                        messages,
                        moderation: history.moderation,
                        channel,
                    }
                }
                Err(error) => {
                    tracing::warn!(%channel, %error, "could not load recent messages");
                    unavailable(channel, token, request.after.is_some())
                }
            };
            // A closed bridge means the UI is gone.
            let _ = inner.sender.send(event).await;
        }));
    }
}

/// `message` with the channel's third-party emotes resolved, as a live message would be.
fn resolve(
    emotes: Option<&EmoteService>,
    channel: &ChannelId,
    message: Arc<Message>,
) -> Arc<Message> {
    let Some(emotes) = emotes else {
        return message;
    };
    match emotes.process(ChatEvent::Message {
        channel: channel.clone(),
        message,
    }) {
        ChatEvent::Message { message, .. } => message,
        // `process` hands a message event back as a message event.
        _ => unreachable!("EmoteService::process keeps a message a message"),
    }
}

/// The quiet one-line notice that history could not be loaded (`gap`: it was the messages missed
/// during a reconnect, not the history of a fresh join).
fn unavailable(channel: ChannelId, token: u64, gap: bool) -> ChatEvent {
    let text = if gap {
        "Messages from while you were disconnected could not be loaded."
    } else {
        "Recent messages could not be loaded."
    };
    ChatEvent::Message {
        channel,
        message: Arc::new(Message::plain(
            format!("history-unavailable-{token}"),
            now(),
            MessageKind::System,
            text,
        )),
    }
}

/// The quiet line in a reconnect's history that marks a hole: the service had more messages for
/// the time we were away than one answer can hold, so the earliest of them are not shown.
/// Stamped just before the oldest message that was returned, so it lands where the hole is.
fn possibly_missing(token: u64, oldest: Timestamp) -> Message {
    let mut line = Message::plain(
        format!("history-gap-{token}"),
        Timestamp::from_unix_millis(oldest.unix_millis().saturating_sub(1)),
        MessageKind::System,
        "Some messages may be missing here.",
    );
    line.flags.history = true;
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_is_a_quiet_system_line_that_says_which_kind_of_history_failed() {
        let ChatEvent::Message { channel, message } = unavailable(ChannelId::new("a"), 3, false)
        else {
            panic!("a message")
        };
        assert_eq!(channel, ChannelId::new("a"));
        assert_eq!(message.kind, MessageKind::System);
        assert_eq!(message.id.as_str(), "history-unavailable-3");
        assert!(message.plain_text().starts_with("Recent messages"));
        assert!(!message.flags.mention && !message.flags.history);
        let ChatEvent::Message { message: gap, .. } = unavailable(ChannelId::new("a"), 4, true)
        else {
            panic!("a message")
        };
        assert!(gap.plain_text().contains("disconnected"));
        assert_ne!(gap.id, message.id, "one line per failure");
    }

    #[test]
    fn the_hole_marker_sorts_just_before_the_oldest_returned_message() {
        let line = possibly_missing(2, Timestamp(50_000));
        assert_eq!(line.timestamp, Timestamp(49_999));
        assert!(line.flags.history, "dimmed, and skipped by notifications");
        assert_eq!(line.kind, MessageKind::System);
        assert_eq!(line.id.as_str(), "history-gap-2");
    }

    #[test]
    fn without_an_emote_service_a_message_comes_back_unchanged() {
        let message = Arc::new(Message::plain(
            "1",
            Default::default(),
            MessageKind::Chat,
            "Kappa",
        ));
        let same = resolve(None, &ChannelId::new("a"), message.clone());
        assert!(Arc::ptr_eq(&same, &message));
    }
}
