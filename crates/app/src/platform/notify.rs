//! System notifications (PR 5.3): the one place that talks to the operating system's notification
//! centre, through GPUI's `App::show_system_notification` (macOS: `UNUserNotificationCenter`).
//!
//! GPUI does the platform work lazily: nothing touches the notification centre until the first
//! notification is posted or a response handler is registered, and the *authorisation prompt* is
//! raised by the first `show`, so the user is asked when a notification is first needed and never
//! at startup. In a binary that is not inside an app bundle (`cargo run`) macOS cannot deliver
//! notifications at all; GPUI then logs that and every call is a no-op.

use gpui_kit::{App, SystemNotification};

/// A notification to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Its identity: posting another with the same tag replaces it, and it is handed back to
    /// [`on_activate`] when the user clicks it.
    pub tag: String,
    pub title: String,
    pub body: String,
}

/// Posts `notification`.
pub fn show(notification: Notification, cx: &App) {
    cx.show_system_notification(SystemNotification {
        tag: notification.tag.into(),
        title: notification.title.into(),
        body: notification.body.into(),
        actions: Vec::new(),
    });
}

/// Takes back the notification with this tag (delivered or pending), if it is still there.
pub fn dismiss(tag: &str, cx: &App) {
    cx.dismiss_system_notification(tag);
}

/// Runs `handler` with the tag of a notification the user clicked (on the main thread). One handler
/// for the app; registering again replaces it.
pub fn on_activate(cx: &App, mut handler: impl FnMut(&str, &mut App) + 'static) {
    cx.on_system_notification_response(move |response, cx| {
        handler(&response.tag, cx);
    });
}
