//! The run's progress as the user sees it: log lines and a progress bar.

use diff_nixpkgs_attr_core::build::BuildEvent;
use diff_nixpkgs_attr_core::{Collected, Event, Level, Stage, ToolStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Stage,
    Note,
    Warn,
    Error,
    Plain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    pub text: String,
}

fn line(kind: Kind, text: impl Into<String>) -> Option<Line> {
    Some(Line {
        kind,
        text: text.into(),
    })
}

/// The log line for an event, when it has one.
pub fn describe(event: &Event) -> Option<Line> {
    match event {
        Event::Stage(Stage::Evaluating { attr, base_rev }) => {
            let rev: String = base_rev.chars().take(12).collect();
            line(
                Kind::Stage,
                format!("{attr}: evaluating base {rev} and the working tree"),
            )
        }
        Event::Stage(Stage::Building { side, derivations }) => line(
            Kind::Stage,
            format!(
                "Building the {} side ({derivations} derivations)",
                side.short_label()
            ),
        ),
        Event::Notice(notice) => {
            let kind = match notice.level {
                Level::Note => Kind::Note,
                Level::Info => Kind::Stage,
                Level::Plan => Kind::Plain,
                Level::Warn => Kind::Warn,
                Level::Error => Kind::Error,
            };
            line(kind, notice.text.clone())
        }
        Event::ToolLine { line: text, .. } => line(Kind::Plain, text.clone()),
        Event::Collected { attr, result } => match result {
            Collected::NothingChanged => {
                line(Kind::Stage, format!("{attr}: no changed non-debug outputs"))
            }
            Collected::Changed { outputs, packages } => line(
                Kind::Stage,
                format!("{attr}: {outputs} changed non-debug outputs ({packages} package(s))"),
            ),
            Collected::NotEvaluated(_) => None,
        },
        Event::Total {
            attrs,
            outputs,
            packages,
        } if *attrs > 1 => line(
            Kind::Stage,
            format!("In total {outputs} changed non-debug outputs ({packages} package(s))"),
        ),
        Event::Build {
            event: BuildEvent::Message { text, .. } | BuildEvent::Raw(text),
            ..
        } => line(Kind::Plain, text.clone()),
        Event::PackageStarting { attr, index, total } => line(
            Kind::Stage,
            format!("Comparing {attr} ({} of {total})", index + 1),
        ),
        Event::ToolFinished {
            attr,
            tool,
            output,
            status,
        } => {
            let name = tool.name();
            let what = match output {
                Some(output) => format!("{attr} [{output}]"),
                None => attr.clone(),
            };
            match status {
                ToolStatus::Same | ToolStatus::Different => None,
                ToolStatus::Failed(Some(code)) => line(
                    Kind::Warn,
                    format!("{name} exited with status {code} for {what}"),
                ),
                ToolStatus::Failed(None) => line(Kind::Warn, format!("{name} failed for {what}")),
                ToolStatus::MemoryLimit => line(
                    Kind::Warn,
                    format!("{name} went over its memory limit for {what}"),
                ),
            }
        }
        Event::Skipped { attr, output, .. } => line(
            Kind::Warn,
            format!("{attr} [{output}]: skipped, build output missing"),
        ),
        Event::WouldDiff {
            attr,
            output,
            old_path,
            new_path,
        } => line(
            Kind::Plain,
            format!("would diff {attr} [{output}]\n  - {old_path}\n  + {new_path}"),
        ),
        _ => None,
    }
}

/// The one-line status and the progress bar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Activity {
    pub status: String,
    /// 0 to 1; `None` when unknown (shown as a pulse).
    pub fraction: Option<f64>,
}

impl Activity {
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Stage(Stage::Evaluating { attr, .. }) => {
                self.status = format!("Evaluating {attr}");
                self.fraction = None;
            }
            Event::EvalProgress { attr, side, jobs } => {
                self.status = format!("Evaluating {attr} ({}): {jobs} jobs", side.label());
                self.fraction = None;
            }
            Event::Stage(Stage::Building { side, .. }) => {
                self.status = format!("Building the {} side", side.short_label());
                self.fraction = None;
            }
            Event::Build {
                side,
                event:
                    BuildEvent::Progress {
                        done,
                        expected,
                        running,
                        failed,
                    },
            } => {
                self.status = format!(
                    "Building the {} side: {done} of {expected} done, {running} running{}",
                    side.short_label(),
                    if *failed > 0 {
                        format!(", {failed} failed")
                    } else {
                        String::new()
                    }
                );
                self.fraction = (*expected > 0).then(|| (*done as f64 / *expected as f64).min(1.0));
            }
            Event::PackageStarting { attr, index, total } => {
                self.status = format!("Comparing {attr} ({} of {total})", index + 1);
                self.fraction = (*total > 0).then(|| *index as f64 / *total as f64);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff_nixpkgs_attr_core::{Notice, Side, Tool};

    #[test]
    fn notices_keep_their_severity() {
        let warn = Event::Notice(Notice::warn("t", "careful"));
        assert_eq!(
            describe(&warn),
            Some(Line {
                kind: Kind::Warn,
                text: "careful".into()
            })
        );
        let note = Event::Notice(Notice::note("t", "fyi"));
        assert_eq!(describe(&note).unwrap().kind, Kind::Note);
    }

    #[test]
    fn quiet_events_have_no_line() {
        assert_eq!(
            describe(&Event::Output {
                tool: Tool::Difflog,
                data: vec![]
            }),
            None
        );
        assert_eq!(
            describe(&Event::ToolFinished {
                attr: "a".into(),
                tool: Tool::Difflog,
                output: None,
                status: ToolStatus::Different
            }),
            None
        );
    }

    #[test]
    fn failures_are_reported() {
        let line = describe(&Event::ToolFinished {
            attr: "a".into(),
            tool: Tool::Diffoscope,
            output: Some("out".into()),
            status: ToolStatus::Failed(Some(2)),
        })
        .unwrap();
        assert_eq!(line.kind, Kind::Warn);
        assert_eq!(line.text, "diffoscope-nix exited with status 2 for a [out]");
    }

    #[test]
    fn build_progress_sets_the_bar() {
        let mut activity = Activity::default();
        activity.apply(&Event::Build {
            side: Side::Base,
            event: BuildEvent::Progress {
                done: 1,
                expected: 4,
                running: 2,
                failed: 0,
            },
        });
        assert_eq!(activity.fraction, Some(0.25));
        assert_eq!(
            activity.status,
            "Building the old side: 1 of 4 done, 2 running"
        );
        activity.apply(&Event::EvalProgress {
            attr: "x".into(),
            side: Side::Head,
            jobs: 300,
        });
        assert_eq!(activity.fraction, None);
        assert!(activity.status.contains("300 jobs"));
    }
}
