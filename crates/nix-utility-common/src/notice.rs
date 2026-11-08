//! Messages for the user. Libraries hand these to a caller-supplied sink
//! instead of printing; a terminal front end prints them, a GUI shows them its
//! own way.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Background information such as which symlink was followed.
    Note,
    Info,
    /// A line of a dry run's plan: the requested output of the run, not a
    /// remark about it.
    Plan,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub level: Level,
    /// The tool the notice comes from, e.g. `difflog`.
    pub tool: &'static str,
    pub text: String,
}

impl Notice {
    pub fn new(level: Level, tool: &'static str, text: impl Into<String>) -> Self {
        Self {
            level,
            tool,
            text: text.into(),
        }
    }

    pub fn note(tool: &'static str, text: impl Into<String>) -> Self {
        Self::new(Level::Note, tool, text)
    }

    pub fn info(tool: &'static str, text: impl Into<String>) -> Self {
        Self::new(Level::Info, tool, text)
    }

    pub fn plan(tool: &'static str, text: impl Into<String>) -> Self {
        Self::new(Level::Plan, tool, text)
    }

    pub fn warn(tool: &'static str, text: impl Into<String>) -> Self {
        Self::new(Level::Warn, tool, text)
    }

    pub fn error(tool: &'static str, text: impl Into<String>) -> Self {
        Self::new(Level::Error, tool, text)
    }
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.tool, self.text)
    }
}

/// Where a library sends its notices.
pub type NoticeSink<'a> = &'a mut dyn FnMut(Notice);
