//! Terminal output: the colours, wording and layout of the old
//! `diff-nixpkgs-attr` script, rendered from the core's events.

use std::io::{self, IsTerminal, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use diff_nixpkgs_attr_core::build::BuildEvent;
use diff_nixpkgs_attr_core::{Collected, Event, Flow, Level, Report, Stage, Tool, ToolStatus};

const S_STAGE: &str = "1;36"; // >>> progress lines
const S_SET: &str = "1;35"; // attribute names inside progress lines
const S_NUM: &str = "1;97"; // counts
const S_PKG: &str = "1;97;44"; // package banner above each diff
const S_OUT: &str = "1;33"; // --- attr header
const S_OUTNAME: &str = "36"; // [output]
const S_OLD: &str = "1;31";
const S_NEW: &str = "1;32";
const S_OK: &str = "1;32";
const S_WARN: &str = "1;33";
const S_ERR: &str = "1;31";
const S_DIM: &str = "2";

#[derive(Debug, Clone, Copy)]
pub struct Colors {
    pub stdout: bool,
    pub stderr: bool,
}

impl Colors {
    /// Colour each stream that is a terminal, unless `NO_COLOR` is set.
    pub fn detect() -> Self {
        let wanted = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
            && std::env::var("TERM").is_ok_and(|t| t != "dumb");
        Self {
            stdout: wanted && io::stdout().is_terminal(),
            stderr: wanted && io::stderr().is_terminal(),
        }
    }
}

fn paint(on: bool, style: &str, text: &str) -> String {
    if on {
        format!("\x1b[{style}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

pub struct Options {
    pub pager: Option<Vec<String>>,
    pub pause: bool,
    pub colors: Colors,
}

/// Where the diff tools' output goes: straight to stdout, or through a pager.
enum Sink {
    Direct,
    Pager {
        child: Child,
        stdin: Option<ChildStdin>,
    },
}

pub struct Ui {
    opts: Options,
    sink: Option<Sink>,
    /// A transient progress line is showing on stderr.
    status_shown: bool,
    last_status: Option<String>,
}

impl Ui {
    pub fn new(opts: Options) -> Self {
        Self {
            opts,
            sink: None,
            status_shown: false,
            last_status: None,
        }
    }

    fn out(&self, style: &str, text: &str) -> String {
        paint(self.opts.colors.stdout, style, text)
    }

    fn err(&self, style: &str, text: &str) -> String {
        paint(self.opts.colors.stderr, style, text)
    }

    /// Print to stderr, clearing any progress line first.
    fn eprint(&mut self, text: &str) {
        self.clear_status();
        let mut stderr = io::stderr().lock();
        let _ = stderr.write_all(text.as_bytes());
        let _ = stderr.flush();
    }

    fn print(&mut self, text: &str) {
        self.clear_status();
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    }

    fn clear_status(&mut self) {
        if self.status_shown {
            let _ = io::stderr().write_all(b"\r\x1b[2K");
            self.status_shown = false;
        }
    }

    /// A progress line that the next message replaces; terminals only.
    fn status(&mut self, text: String) {
        if !self.opts.colors.stderr || self.last_status.as_deref() == Some(&text) {
            return;
        }
        let mut stderr = io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[2K    {}", self.err(S_DIM, &text));
        let _ = stderr.flush();
        self.status_shown = true;
        self.last_status = Some(text);
    }

    fn stage(&mut self, text: &str) {
        let line = format!("{} {text}\n", self.err(S_STAGE, ">>>"));
        self.eprint(&line);
    }

    fn note(&mut self, text: &str) {
        let line = format!("    {}\n", self.err(S_DIM, text));
        self.eprint(&line);
    }

    pub fn warn(&mut self, text: &str) {
        let line = format!("{} {text}\n", self.err(S_WARN, "warning:"));
        self.eprint(&line);
    }

    /// The old and new path, coloured for the stream it goes to.
    fn pair(&self, colour: bool, old: &str, new: &str) -> String {
        format!(
            "    {} {old}\n    {} {new}\n",
            paint(colour, S_OLD, "-"),
            paint(colour, S_NEW, "+"),
        )
    }

    fn output_header(&mut self, attr: &str, output: &str) {
        let line = format!(
            "{} {}\n",
            self.out(S_OUT, &format!("--- {attr}")),
            self.out(S_OUTNAME, &format!("[{output}]")),
        );
        self.print(&line);
    }

    /// Whether the user wants to go on to the next package.
    fn pause(&mut self) -> bool {
        if !self.opts.pause || !io::stdin().is_terminal() {
            return true;
        }
        let prompt = self.err(S_DIM, "--- Enter: next package, q: quit ");
        self.eprint(&prompt);
        let mut reply = String::new();
        match io::stdin().read_line(&mut reply) {
            Ok(0) | Err(_) => true,
            Ok(_) => reply.trim() != "q",
        }
    }

    fn open_sink(&mut self) {
        if self.sink.is_some() {
            return;
        }
        self.sink = Some(match self.spawn_pager() {
            Some((child, stdin)) => Sink::Pager {
                child,
                stdin: Some(stdin),
            },
            None => Sink::Direct,
        });
    }

    fn spawn_pager(&mut self) -> Option<(Child, ChildStdin)> {
        let command = self.opts.pager.clone()?;
        let (program, args) = command.split_first()?;
        match Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                let stdin = child.stdin.take()?;
                Some((child, stdin))
            }
            Err(err) => {
                self.warn(&format!("could not run the pager `{program}`: {err}"));
                self.opts.pager = None;
                None
            }
        }
    }

    fn write_output(&mut self, data: &[u8]) {
        self.clear_status();
        self.open_sink();
        match self.sink.as_mut() {
            Some(Sink::Direct) => {
                let mut stdout = io::stdout().lock();
                let _ = stdout.write_all(data);
                let _ = stdout.flush();
            }
            // A pager the user quit stops reading; the run goes on regardless
            // and everything is saved, so the write error is ignored.
            Some(Sink::Pager { stdin, .. }) => {
                if let Some(pipe) = stdin
                    && pipe.write_all(data).is_err()
                {
                    *stdin = None;
                }
            }
            None => {}
        }
    }

    /// Close the current output (waits for the pager to be quit).
    fn close_sink(&mut self) {
        if let Some(Sink::Pager { mut child, stdin }) = self.sink.take() {
            drop(stdin);
            let _ = child.wait();
        }
    }

    pub fn handle(&mut self, event: Event) -> Flow {
        match event {
            Event::Stage(Stage::Evaluating { attr, base_rev }) => {
                let rev: String = base_rev.chars().take(12).collect();
                self.stage(&format!(
                    "{}: evaluating base {} and {}",
                    self.err(S_SET, &attr),
                    self.err(S_OLD, &rev),
                    self.err(S_NEW, "working tree"),
                ));
            }
            Event::Stage(Stage::Building { side, derivations }) => {
                self.stage(&format!(
                    "building the {} side ({} derivations)",
                    side.short_label(),
                    self.err(S_NUM, &derivations.to_string()),
                ));
            }
            Event::Notice(notice) => match notice.level {
                Level::Note => self.note(&notice.text),
                Level::Info => self.stage(&notice.text),
                Level::Plan => {
                    let line = format!("{}\n", notice.text);
                    self.print(&line);
                }
                Level::Warn => self.warn(&notice.text),
                Level::Error => {
                    let line = format!("{} {}\n", self.err(S_ERR, "error:"), notice.text);
                    self.eprint(&line);
                }
            },
            Event::ToolLine { line, .. } => self.eprint(&format!("{line}\n")),
            Event::EvalProgress { attr, side, jobs } => {
                self.status(format!("evaluating {attr} ({}): {jobs} jobs", side.label()));
            }
            Event::Collected { attr, result } => match result {
                Collected::NothingChanged => self.stage(&format!(
                    "{}: {}",
                    self.err(S_SET, &attr),
                    self.err(S_OK, "no changed non-debug outputs"),
                )),
                Collected::Changed { outputs, packages } => self.stage(&format!(
                    "{}: {} changed non-debug outputs ({} package(s))",
                    self.err(S_SET, &attr),
                    self.err(S_NUM, &outputs.to_string()),
                    self.err(S_NUM, &packages.to_string()),
                )),
                // The core has already warned about it.
                Collected::NotEvaluated(_) => {}
            },
            Event::Total {
                attrs,
                outputs,
                packages,
            } => {
                if attrs > 1 {
                    self.stage(&format!(
                        "in total {} changed non-debug outputs ({} package(s))",
                        self.err(S_NUM, &outputs.to_string()),
                        self.err(S_NUM, &packages.to_string()),
                    ));
                }
            }
            Event::Build { event, .. } => match event {
                BuildEvent::Progress {
                    done,
                    expected,
                    running,
                    failed,
                } => {
                    let mut text = format!("built {done}/{expected}, {running} running");
                    if failed > 0 {
                        text.push_str(&format!(", {failed} failed"));
                    }
                    self.status(text);
                }
                BuildEvent::Message { text, .. } | BuildEvent::Raw(text) => {
                    self.eprint(&format!("{text}\n"));
                }
                BuildEvent::Started { .. }
                | BuildEvent::Log { .. }
                | BuildEvent::Finished { .. } => {}
            },
            Event::PackageStarting { attr, index, .. } => {
                if index > 0 && !self.pause() {
                    return Flow::Stop;
                }
                let banner = format!("\n{}\n", self.out(S_PKG, &format!(" {attr} ")));
                self.print(&banner);
            }
            Event::ToolStarted {
                attr,
                tool: Tool::Diffoscope,
                output: Some(output),
            } => self.output_header(&attr, &output),
            Event::ToolStarted { .. } => {}
            Event::Output { data, .. } => self.write_output(&data),
            Event::ToolFinished {
                attr,
                tool,
                output,
                status,
            } => {
                self.close_sink();
                let name = tool.name();
                let what = match &output {
                    Some(output) => format!("{attr} [{output}]"),
                    None => attr,
                };
                match status {
                    ToolStatus::Same | ToolStatus::Different | ToolStatus::Failed(None) => {}
                    ToolStatus::Failed(Some(code)) => {
                        self.warn(&format!("{name} exited with status {code} for {what}"));
                    }
                    ToolStatus::MemoryLimit => {
                        self.warn(&format!("{name} went over its memory limit for {what}"));
                    }
                }
            }
            Event::Skipped {
                attr,
                output,
                old_path,
                new_path,
            } => {
                self.output_header(&attr, &output);
                self.warn("skipped: build output missing");
                let pair = self.pair(self.opts.colors.stderr, &old_path, &new_path);
                self.eprint(&pair);
            }
            Event::WouldDiff {
                attr,
                output,
                old_path,
                new_path,
            } => {
                let line = format!(
                    "{} {attr} {}\n",
                    self.out(S_OK, "would diff"),
                    self.out(S_OUTNAME, &format!("[{output}]")),
                );
                self.print(&line);
                let pair = self.pair(self.opts.colors.stdout, &old_path, &new_path);
                self.print(&pair);
            }
            Event::Saved(_) => {}
        }
        Flow::Continue
    }

    pub fn summary(&mut self, report: &Report) {
        self.close_sink();
        let mut text = String::from("\n");
        if report.entries.is_empty() {
            text += &self.out(S_OK, "No differences reported by difflog or diffoscope-nix");
            text.push('\n');
        } else {
            text += &self.out(
                S_WARN,
                &format!(
                    "Differences found in {} attribute(s):",
                    report.entries.len()
                ),
            );
            text.push('\n');
            let width = report
                .entries
                .iter()
                .map(|e| e.attr.chars().count())
                .max()
                .unwrap_or(0);
            for entry in &report.entries {
                let mut parts = Vec::new();
                if entry.difflog {
                    parts.push("difflog".to_owned());
                }
                if !entry.diffoscope_outputs.is_empty() {
                    parts.push(format!(
                        "diffoscope-nix [{}]",
                        entry.diffoscope_outputs.join(",")
                    ));
                }
                text += &format!(
                    "  {}  {}\n",
                    self.out(S_OUT, &format!("{:<width$}", entry.attr)),
                    self.out(S_OUTNAME, &parts.join(", ")),
                );
            }
        }
        if let Some(dir) = &report.saved_to {
            text += &format!(
                "{} {}\n",
                self.out(S_OK, "Differences saved to:"),
                dir.display()
            );
        }
        if report.stopped_early {
            text += &self.out(S_DIM, "(stopped early: later packages were not compared)");
            text.push('\n');
        }
        self.print(&text);
    }

    pub fn error(&mut self, message: &str) {
        let line = format!("{} {message}\n", self.err(S_ERR, "error:"));
        self.eprint(&line);
    }
}

impl Drop for Ui {
    /// Leave the terminal tidy if the run ended mid-tool (e.g. cancelled).
    fn drop(&mut self) {
        self.close_sink();
        self.clear_status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_only_when_enabled() {
        assert_eq!(paint(false, "1", "x"), "x");
        assert_eq!(paint(true, "1;31", "x"), "\x1b[1;31mx\x1b[0m");
    }
}
