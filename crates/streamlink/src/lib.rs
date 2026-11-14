//! Locate, launch and track streamlink child processes.
//!
//! GPUI-free and I/O only through `std::process` and plain threads, so it builds and tests on
//! any OS. The pieces:
//!
//! * [`resolve`]: find the binary (settings override, login-shell `command -v`, well-known
//!   directories). The environment is a trait ([`Environment`]), so the order is unit-tested
//!   without touching the machine.
//! * [`args`]: `streamlink [--player P] [extra…] twitch.tv/<channel> <quality>`.
//! * [`classify`]: turns streamlink's stderr into a typed [`StreamError`] (channel offline,
//!   no player, other).
//! * [`Streams`]: spawns and tracks one child per channel and reports [`StreamEvent`]s to a
//!   sink the caller provides. Dropping or [`Streams::stop_all`] ends every child.

mod args;
mod classify;
mod resolve;
mod streams;

pub use args::{DEFAULT_QUALITY, LaunchSpec, QUALITIES, build_args, stream_url, valid_quality};
pub use classify::{StreamError, classify_line};
pub use resolve::{
    Environment, FALLBACK_DIRS, ResolveError, SystemEnvironment, parse_which_output, resolve,
};
pub use streams::{EventSink, SessionId, StreamEvent, StreamEventKind, Streams};
