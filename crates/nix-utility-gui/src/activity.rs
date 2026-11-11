//! The run's progress as the user sees it: log lines and a progress bar.

use diff_nixpkgs_attr_core::build::BuildEvent;
use diff_nixpkgs_attr_core::{Collected, Event, Level, Side, Stage, ToolStatus, strip_sgr};

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

/// Length of the hash part of a store path name.
const HASH_LEN: usize = 32;

/// A derivation's name without the store directory, hash and `.drv`, e.g.
/// `hello-2.12` for `/nix/store/<hash>-hello-2.12.drv`.
pub fn drv_name(drv: &str) -> &str {
    let base = drv.rsplit('/').next().unwrap_or(drv);
    let base = base.strip_suffix(".drv").unwrap_or(base);
    match base.get(HASH_LEN..) {
        Some(rest) if rest.starts_with('-') && rest.len() > 1 => &rest[1..],
        _ => base,
    }
}

/// Running derivations shown in the build header before "+N more".
const SHOWN_BUILDS: usize = 3;

/// What the builders are doing: their output and which are running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Builds {
    side: Option<Side>,
    /// Names of the running derivations, in start order.
    running: Vec<String>,
}

impl Builds {
    /// Track the event; returns the line it adds to the build log, if any.
    pub fn apply(&mut self, event: &Event) -> Option<Line> {
        let Event::Build { side, event } = event else {
            return None;
        };
        match event {
            BuildEvent::Started { drv } => {
                self.side = Some(*side);
                self.running.push(drv_name(drv).to_owned());
                None
            }
            BuildEvent::Finished { drv } => {
                let name = drv_name(drv);
                if let Some(i) = self.running.iter().position(|n| n == name) {
                    self.running.remove(i);
                }
                None
            }
            BuildEvent::Log { drv, line: text } => line(
                Kind::Plain,
                format!("{}> {}", drv_name(drv), strip_sgr(text)),
            ),
            // Build failures, next to the output that led to them.
            BuildEvent::Message { level: 0, text } => line(Kind::Error, text.clone()),
            _ => None,
        }
    }

    /// Nothing is building any more (the run ended).
    pub fn stop(&mut self) {
        self.running.clear();
    }

    /// The build pane's header.
    pub fn summary(&self) -> String {
        let (Some(side), false) = (self.side, self.running.is_empty()) else {
            return "Build log".to_owned();
        };
        let shown = self.running[..self.running.len().min(SHOWN_BUILDS)].join(", ");
        let more = self.running.len().saturating_sub(SHOWN_BUILDS);
        let more = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        format!("Building the {} side: {shown}{more}", side.short_label())
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
    use diff_nixpkgs_attr_core::{Notice, Tool};

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
    const DRV: &str = "/nix/store/zwkkghilp2s5g1ds47gams5f8j2klqy6-hello-2.12.drv";

    fn build(event: BuildEvent) -> Event {
        Event::Build {
            side: Side::Head,
            event,
        }
    }

    #[test]
    fn derivation_names_lose_the_store_path() {
        assert_eq!(drv_name(DRV), "hello-2.12");
        assert_eq!(drv_name("not-a-store-path"), "not-a-store-path");
        assert_eq!(drv_name(""), "");
    }

    #[test]
    fn builder_output_is_prefixed_with_the_derivation() {
        let mut builds = Builds::default();
        let line = builds
            .apply(&build(BuildEvent::Log {
                drv: DRV.into(),
                line: "\x1b[1mcompiling\x1b[0m main.c".into(),
            }))
            .unwrap();
        assert_eq!(line.text, "hello-2.12> compiling main.c");
        assert_eq!(line.kind, Kind::Plain);
        let error = builds.apply(&build(BuildEvent::Message {
            level: 0,
            text: "builder failed".into(),
        }));
        assert_eq!(error.unwrap().kind, Kind::Error);
        assert_eq!(
            builds.apply(&build(BuildEvent::Progress {
                done: 0,
                expected: 1,
                running: 1,
                failed: 0
            })),
            None
        );
        assert_eq!(
            builds.apply(&build(BuildEvent::Started { drv: DRV.into() })),
            None
        );
    }

    #[test]
    fn the_header_lists_running_builds() {
        let mut builds = Builds::default();
        assert_eq!(builds.summary(), "Build log");
        for name in ["a", "b"] {
            builds.apply(&build(BuildEvent::Started { drv: name.into() }));
        }
        assert_eq!(builds.summary(), "Building the new side: a, b");
        for name in ["c", "d", "e"] {
            builds.apply(&build(BuildEvent::Started { drv: name.into() }));
        }
        assert_eq!(builds.summary(), "Building the new side: a, b, c (+2 more)");
        builds.apply(&build(BuildEvent::Finished { drv: "a".into() }));
        assert_eq!(builds.summary(), "Building the new side: b, c, d (+1 more)");
        builds.stop();
        assert_eq!(builds.summary(), "Build log");
    }
}
