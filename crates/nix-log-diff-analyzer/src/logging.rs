//! Coloured logging on top of the `log` crate.
//!
//! The `log` crate has no "success" level, so successes are emitted at
//! `Info` with a dedicated target and rendered as `[INCONSEQUENTIAL]`.

use std::io::Write;

use anstyle::{Ansi256Color, AnsiColor, Color, Style};
use log::Level;

pub const SUCCESS_TARGET: &str = "success";

pub const RED: Style = AnsiColor::Red.on_default();
pub const GREEN: Style = AnsiColor::Green.on_default();
pub const YELLOW: Style = AnsiColor::Yellow.on_default();
pub const CYAN: Style = AnsiColor::Cyan.on_default();
/// Reliable 256-colour mid-grey.
pub const GREY: Style = Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(245))));

macro_rules! success {
    ($($arg:tt)+) => {
        ::log::info!(target: $crate::logging::SUCCESS_TARGET, $($arg)+)
    };
}
pub(crate) use success;

pub fn init() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            let (style, label) = if record.target() == SUCCESS_TARGET {
                (GREEN, "INCONSEQUENTIAL")
            } else {
                match record.level() {
                    Level::Error => (RED, "ERROR"),
                    Level::Warn => (YELLOW, "WARNING"),
                    Level::Info => (CYAN, "INFO"),
                    Level::Debug => (GREY, "DEBUG"),
                    Level::Trace => (GREY, "TRACE"),
                }
            };
            writeln!(buf, "{style}[{label}]{style:#} {}", record.args())
        })
        .init();
}
