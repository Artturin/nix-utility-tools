//! What a run reports. Frontends (the CLI, the GUI) render these; nothing
//! here is formatted for a particular display.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

pub use nix_utility_common::notice::{Level, Notice};

use crate::build::BuildEvent;

/// The two sides of a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The base revision.
    Base,
    /// The working tree.
    Head,
}

impl Side {
    pub fn label(self) -> &'static str {
        match self {
            Side::Base => "base",
            Side::Head => "working tree",
        }
    }

    /// "old" or "new", as the build steps call the sides.
    pub fn short_label(self) -> &'static str {
        match self {
            Side::Base => "old",
            Side::Head => "new",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Difflog,
    Diffoscope,
}

impl Tool {
    /// The tool's name as the user knows it.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Difflog => "difflog",
            Tool::Diffoscope => "diffoscope-nix",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Evaluating { attr: String, base_rev: String },
    Building { side: Side, derivations: usize },
}

/// What evaluating one attribute on both sides led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collected {
    NothingChanged,
    Changed {
        outputs: usize,
        packages: usize,
    },
    /// Evaluation produced no derivation on this side.
    NotEvaluated(Side),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Same,
    Different,
    /// The tool failed; `None` when it could not be run at all.
    Failed(Option<i32>),
    /// Killed because it went over its memory limit.
    MemoryLimit,
}

#[derive(Debug, Clone)]
pub enum Event {
    Stage(Stage),
    Notice(Notice),
    /// A line a tool printed on stderr (nix-eval-jobs, diffoscope).
    ToolLine {
        tool: &'static str,
        line: String,
    },
    EvalProgress {
        attr: String,
        side: Side,
        jobs: usize,
    },
    Collected {
        attr: String,
        result: Collected,
    },
    /// Everything to compare, after de-duplication.
    Total {
        attrs: usize,
        outputs: usize,
        packages: usize,
    },
    Build {
        side: Side,
        event: BuildEvent,
    },
    /// About to diff the next package. Returning [`Flow::Stop`] ends the run
    /// here (e.g. the user quit between packages).
    PackageStarting {
        attr: String,
        index: usize,
        total: usize,
    },
    /// A diff tool starts; its output follows as [`Event::Output`]. For
    /// diffoscope `output` names the output being compared.
    ToolStarted {
        attr: String,
        tool: Tool,
        output: Option<String>,
    },
    /// A chunk of the diff tool's stdout (bytes: may end mid-character).
    Output {
        tool: Tool,
        data: Vec<u8>,
    },
    ToolFinished {
        attr: String,
        tool: Tool,
        output: Option<String>,
        status: ToolStatus,
    },
    /// A changed output was not compared because a build output is missing.
    Skipped {
        attr: String,
        output: String,
        old_path: String,
        new_path: String,
    },
    /// Dry run: this is what would be diffed.
    WouldDiff {
        attr: String,
        output: String,
        old_path: String,
        new_path: String,
    },
    /// Where results are being saved (reported once, when the first is).
    Saved(PathBuf),
}

/// What the event handler wants to happen next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Shares one handler between the several callbacks a step needs.
pub(crate) struct Emitter<'a> {
    handler: RefCell<&'a mut dyn FnMut(Event) -> Flow>,
    stopped: Cell<bool>,
}

impl<'a> Emitter<'a> {
    pub fn new(handler: &'a mut dyn FnMut(Event) -> Flow) -> Self {
        Self {
            handler: RefCell::new(handler),
            stopped: Cell::new(false),
        }
    }

    pub fn emit(&self, event: Event) {
        if (self.handler.borrow_mut())(event) == Flow::Stop {
            self.stopped.set(true);
        }
    }

    pub fn notice(&self, notice: Notice) {
        self.emit(Event::Notice(notice));
    }

    pub fn stopped(&self) -> bool {
        self.stopped.get()
    }
}
