//! The chat view (PR 1.4): a virtualised list of [`element::MessageElement`]s.
//!
//! * [`display`] — a message as one line of text with semantic span roles (pure).
//! * [`flow`] — wrapping atoms into lines, plus hit-testing (pure).
//! * [`layout`] — shaping with the window text system into grapheme atoms; the per-message
//!   layout cache.
//! * [`colors`] — username fallback colours and readability adjustment (pure).
//! * [`element`] — the custom GPUI `Element` that measures and paints one message.
//! * [`list_sync`] — incremental list splices from store sequence numbers (pure).
//! * [`view`] — the `ChatView` entity: list, follow-bottom, "new messages" pill, live settings.

pub mod colors;
pub mod display;
pub mod element;
pub mod flow;
pub mod layout;
pub mod list_sync;
pub mod view;

pub use view::ChatView;
