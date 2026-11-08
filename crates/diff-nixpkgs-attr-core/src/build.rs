//! Reading `nix build --log-format internal-json`.
//!
//! nix prints one `@nix {...}` JSON object per line on stderr describing log
//! messages and activities (builds, downloads, ...). This is not a stable API,
//! so the parser keeps only what is needed, ignores what it doesn't know, and
//! hands any line it can't read back as [`BuildEvent::Raw`].

use std::collections::HashMap;

use serde_json::Value;

use crate::text::strip_sgr;

/// Activity types (`libutil/logging.hh`).
const ACT_BUILDS: u64 = 104;
const ACT_BUILD: u64 = 105;
/// Result types.
const RES_BUILD_LOG_LINE: u64 = 101;
const RES_PROGRESS: u64 = 105;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildEvent {
    /// A derivation started building.
    Started { drv: String },
    /// A line of a builder's output.
    Log { drv: String, line: String },
    /// A derivation's build activity ended (successfully or not; failures
    /// show up as messages).
    Finished { drv: String },
    /// Overall build counts.
    Progress {
        done: u64,
        expected: u64,
        running: u64,
        failed: u64,
    },
    /// A message from nix itself. `level` is nix's verbosity: 0 error,
    /// 1 warning, 2 notice, 3 info.
    Message { level: u8, text: String },
    /// Anything that was not a recognisable `@nix` line.
    Raw(String),
}

#[derive(Default)]
pub struct InternalJson {
    /// Open activities: id to (type, derivation for builds).
    activities: HashMap<u64, (u64, Option<String>)>,
    last_progress: Option<(u64, u64, u64, u64)>,
}

impl InternalJson {
    /// Interpret one line of nix's stderr.
    pub fn feed(&mut self, line: &str) -> Option<BuildEvent> {
        let Some(json) = line.strip_prefix("@nix ") else {
            return (!line.trim().is_empty()).then(|| BuildEvent::Raw(line.to_owned()));
        };
        let Ok(value) = serde_json::from_str::<Value>(json) else {
            return Some(BuildEvent::Raw(line.to_owned()));
        };
        let id = value.get("id").and_then(Value::as_u64);
        let fields = value.get("fields").and_then(Value::as_array);

        match value.get("action").and_then(Value::as_str)? {
            "msg" => {
                let level = value.get("level").and_then(Value::as_u64).unwrap_or(0);
                // Only what nix would show by default.
                if level > 3 {
                    return None;
                }
                let text = value.get("msg").and_then(Value::as_str)?;
                Some(BuildEvent::Message {
                    level: level as u8,
                    text: strip_sgr(text),
                })
            }
            "start" => {
                let kind = value.get("type").and_then(Value::as_u64)?;
                let drv = (kind == ACT_BUILD)
                    .then(|| fields?.first()?.as_str().map(str::to_owned))
                    .flatten();
                self.activities.insert(id?, (kind, drv.clone()));
                drv.map(|drv| BuildEvent::Started { drv })
            }
            "result" => {
                let kind = value.get("type").and_then(Value::as_u64)?;
                let (activity, drv) = self.activities.get(&id?)?;
                match (kind, *activity) {
                    (RES_BUILD_LOG_LINE, ACT_BUILD) => Some(BuildEvent::Log {
                        drv: drv.clone()?,
                        line: fields?.first()?.as_str()?.to_owned(),
                    }),
                    (RES_PROGRESS, ACT_BUILDS) => {
                        let number = |i: usize| fields?.get(i)?.as_u64();
                        let counts = (number(0)?, number(1)?, number(2)?, number(3)?);
                        if self.last_progress == Some(counts) {
                            return None;
                        }
                        self.last_progress = Some(counts);
                        Some(BuildEvent::Progress {
                            done: counts.0,
                            expected: counts.1,
                            running: counts.2,
                            failed: counts.3,
                        })
                    }
                    _ => None,
                }
            }
            "stop" => {
                let (kind, drv) = self.activities.remove(&id?)?;
                (kind == ACT_BUILD)
                    .then_some(drv)
                    .flatten()
                    .map(|drv| BuildEvent::Finished { drv })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(fixture: &str) -> Vec<BuildEvent> {
        let mut parser = InternalJson::default();
        fixture
            .lines()
            .filter_map(|line| parser.feed(line))
            .collect()
    }

    fn drv(events: &[BuildEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                BuildEvent::Started { drv } => Some(drv.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_real_successful_build() {
        let events = parse(include_str!("../tests/fixtures/nix-build-success.jsonl"));
        let drv_path = "/nix/store/zwkkghilp2s5g1ds47gams5f8j2klqy6-fx-a-1794106199.drv";
        assert_eq!(drv(&events), [drv_path]);
        let logs: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                BuildEvent::Log { drv, line } => Some((drv.as_str(), line.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            logs,
            [
                (drv_path, "building fx-a-1794106199"),
                (drv_path, "warn-line")
            ]
        );
        assert!(events.contains(&BuildEvent::Finished {
            drv: drv_path.to_owned()
        }));
        assert!(events.contains(&BuildEvent::Message {
            level: 3,
            text: "this derivation will be built:".to_owned()
        }));
        // Only nix's default-visible messages: the "evaluating file" chatter is dropped.
        assert!(!events.iter().any(
            |e| matches!(e, BuildEvent::Message { text, .. } if text.contains("evaluating file"))
        ));
    }

    #[test]
    fn a_real_failed_build_keeps_the_error_without_colour_codes() {
        let events = parse(include_str!("../tests/fixtures/nix-build-failure.jsonl"));
        assert_eq!(drv(&events).len(), 2);
        let error = events
            .iter()
            .find_map(|e| match e {
                BuildEvent::Message { level: 0, text } if text.contains("Cannot build") => {
                    Some(text)
                }
                _ => None,
            })
            .expect("the failure message");
        assert!(error.contains("builder failed with exit code 3"), "{error}");
        assert!(!error.contains('\x1b'), "{error:?}");
        assert!(events.iter().any(|e| matches!(
            e,
            BuildEvent::Log { line, .. } if line == "about to fail"
        )));
    }

    #[test]
    fn a_real_dry_run_only_has_messages() {
        let events = parse(include_str!("../tests/fixtures/nix-build-dry-run.jsonl"));
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                BuildEvent::Message { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts[0], "these 2 derivations will be built:");
        assert_eq!(texts.len(), 3);
        assert!(drv(&events).is_empty());
    }

    #[test]
    fn progress_is_reported_only_when_it_changes() {
        let mut parser = InternalJson::default();
        let lines = [
            r#"@nix {"action":"start","id":1,"level":0,"parent":0,"text":"","type":104}"#,
            r#"@nix {"action":"result","fields":[0,2,0,0],"id":1,"type":105}"#,
            r#"@nix {"action":"result","fields":[0,2,0,0],"id":1,"type":105}"#,
            r#"@nix {"action":"result","fields":[1,2,1,0],"id":1,"type":105}"#,
        ];
        let events: Vec<_> = lines.iter().filter_map(|l| parser.feed(l)).collect();
        assert_eq!(
            events,
            [
                BuildEvent::Progress {
                    done: 0,
                    expected: 2,
                    running: 0,
                    failed: 0
                },
                BuildEvent::Progress {
                    done: 1,
                    expected: 2,
                    running: 1,
                    failed: 0
                },
            ]
        );
    }

    #[test]
    fn unknown_and_broken_lines_never_panic() {
        let mut parser = InternalJson::default();
        assert_eq!(parser.feed(""), None);
        assert_eq!(
            parser.feed("error: something plain"),
            Some(BuildEvent::Raw("error: something plain".to_owned()))
        );
        assert_eq!(
            parser.feed("@nix {not json"),
            Some(BuildEvent::Raw("@nix {not json".to_owned()))
        );
        for line in [
            r#"@nix {"action":"future-thing","id":1}"#,
            r#"@nix {"action":"result","id":99,"type":101,"fields":["x"]}"#,
            r#"@nix {"action":"stop","id":99}"#,
            r#"@nix {"action":"start"}"#,
            r#"@nix []"#,
        ] {
            assert_eq!(parser.feed(line), None, "{line}");
        }
    }
}
