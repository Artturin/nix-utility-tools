//! Compare nixpkgs attributes between a base revision and the working tree.
//!
//! Every non-debug output whose store path changed is built on both sides
//! (one `nix build` per side for all attributes together) and run through
//! `difflog` and `diffoscope-nix`, in-process. [`run_pipeline`] does it all
//! and reports through [`Event`]s, so a terminal and a GUI can share it.

pub mod attr;
pub mod build;
pub mod event;
pub mod jobs;
pub mod tools;

mod pipeline;
mod save;
mod text;

pub use event::{Collected, Event, Flow, Level, Notice, Side, Stage, Tool, ToolStatus};
pub use nix_utility_common::CancelToken;
pub use nix_utility_common::process::{Control, ProcessPolicy, SystemdRun};
pub use pipeline::{Config, Error, Report, Status, SummaryEntry, run_pipeline};
pub use text::{LineBuffer, strip_sgr};
