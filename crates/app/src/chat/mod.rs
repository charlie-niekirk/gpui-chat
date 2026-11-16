//! The chat view (PR 1.4): a virtualised list of [`element::MessageElement`]s.
//!
//! * [`display`] — a message as one line of text with semantic span roles (pure).
//! * [`flow`] — wrapping atoms into lines, plus hit-testing (pure).
//! * [`layout`] — shaping with the window text system into grapheme atoms; the per-message
//!   layout cache.
//! * [`colors`] — username fallback colours and readability adjustment (pure).
//! * [`element`] — the custom GPUI `Element` that measures and paints one message.
//! * [`list_sync`] — incremental list splices from store sequence numbers (pure).
//! * [`row`] — a row with its reply button and context menu; [`thread`] — the thread popover.
//! * [`selection`] — selecting text across messages, and what copying it gives (pure);
//!   [`selection_layer`] — the mouse, the highlight, links and autoscroll in the window.
//! * [`events`] — what event rows (subs, raids, cheers, ...) add: header line, accent bar, shared-chat
//!   label (PR 5.1).
//! * [`tooltip`] — emote hover tooltips (preview, name, provider).
//! * [`view`] — the `ChatView` entity: list, follow-bottom, "new messages" pill, live settings.

pub mod colors;
pub mod display;
pub mod element;
pub mod events;
pub mod flow;
pub mod layout;
pub mod list_sync;
pub mod row;
pub mod selection;
pub mod selection_layer;
pub mod thread;
pub mod tooltip;
pub mod view;

pub use view::ChatView;
