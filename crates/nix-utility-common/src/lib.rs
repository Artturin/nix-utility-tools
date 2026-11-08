//! Building blocks shared by the tool crates and the programs built on them.
//!
//! - [`notice`]: the one channel for messages meant for the user. The `log`
//!   crate is only for diagnostics.
//! - [`process`]: running child processes so that they can be cancelled,
//!   time limited and memory capped, taking all their descendants with them.

pub mod notice;
pub mod process;

/// Cooperative cancellation shared between a front end and the work it started.
pub use tokio_util::sync::CancellationToken as CancelToken;
