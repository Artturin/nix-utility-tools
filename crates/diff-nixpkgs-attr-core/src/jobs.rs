//! The nix-eval-jobs output and what to make of it: which outputs changed
//! between two evaluations, and what to build to compare them.
//!
//! This is a port of two `jq` filters from the original shell script, so the
//! semantics (including the odd corners) are deliberate; see the tests, which
//! also run the original filters when `jq` is installed.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::Value;

/// A JSON object whose key order is kept, as jq does.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outputs(Vec<(String, Value)>);

impl Outputs {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, v)| v)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }
}

impl<'de> Deserialize<'de> for Outputs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries;

        impl<'de> Visitor<'de> for Entries {
            type Value = Outputs;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Outputs, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Outputs(entries))
            }
        }

        deserializer.deserialize_map(Entries)
    }
}

/// One line of nix-eval-jobs output.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Job {
    /// Attribute path inside the evaluated set; empty when the evaluated
    /// attribute is itself a derivation.
    #[serde(default)]
    pub attr: String,
    #[serde(rename = "drvPath", default)]
    pub drv_path: Option<String>,
    /// Output name to store path (`null` when unknown).
    #[serde(default)]
    pub outputs: Option<Outputs>,
    #[serde(default)]
    pub error: Option<Value>,
}

impl Job {
    /// The attribute path relative to `pkgs`, given the evaluated `set`.
    pub fn full_attr(&self, set: &str) -> String {
        if self.attr.is_empty() {
            set.to_owned()
        } else {
            format!("{set}.{}", self.attr)
        }
    }

    pub fn has_drv(&self) -> bool {
        self.drv_path.is_some()
    }

    pub fn error_text(&self) -> Option<String> {
        match self.error.as_ref()? {
            Value::Null => None,
            Value::String(text) => Some(text.clone()),
            other => Some(other.to_string()),
        }
    }
}

/// Everything one evaluation produced.
#[derive(Debug, Default)]
pub struct Evaluation {
    pub jobs: Vec<Job>,
    /// Lines that were not valid job JSON (e.g. cut off by a killed worker).
    pub unparsable_lines: usize,
}

impl Evaluation {
    /// Add one output line; blank lines are ignored.
    pub fn push_line(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        match serde_json::from_str(line) {
            Ok(job) => self.jobs.push(job),
            Err(_) => self.unparsable_lines += 1,
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut evaluation = Self::default();
        text.lines().for_each(|line| evaluation.push_line(line));
        evaluation
    }

    /// Jobs that have a derivation.
    pub fn job_count(&self) -> usize {
        self.jobs.iter().filter(|job| job.has_drv()).count()
    }

    pub fn error_count(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| job.error_text().is_some())
            .count()
    }

    /// The first few error messages, for showing why nothing evaluated.
    pub fn first_errors(&self, n: usize) -> Vec<String> {
        self.jobs
            .iter()
            .filter_map(Job::error_text)
            .take(n)
            .collect()
    }
}

/// A non-debug output whose store path differs between the two sides.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Changed {
    /// Full attribute path relative to `pkgs`.
    pub attr: String,
    pub output: String,
    pub old_path: String,
    pub new_path: String,
}

/// Outputs that exist on both sides with different store paths.
///
/// Mirrors the jq filter: an output that is missing (or null) on the old side
/// counts as unchanged, as does one that is null on the new side, and
/// `debug` outputs are never compared. Results follow the new side's order
/// (nix-eval-jobs prints outputs sorted by name).
pub fn changed_outputs(old: &[Job], new: &[Job], set: &str) -> Vec<Changed> {
    // Like jq's from_entries, a later job with the same attribute wins.
    let old_outputs: HashMap<String, Option<&Outputs>> = old
        .iter()
        .filter(|job| job.has_drv())
        .map(|job| (job.full_attr(set), job.outputs.as_ref()))
        .collect();

    let mut changed = Vec::new();
    for job in new.iter().filter(|job| job.has_drv()) {
        let attr = job.full_attr(set);
        let Some(outputs) = &job.outputs else {
            continue;
        };
        for (name, new_path) in outputs.iter() {
            if name == "debug" || new_path.is_null() {
                continue;
            }
            // jq's `//` treats null and false as absent.
            let Some(old_path) = old_outputs
                .get(&attr)
                .copied()
                .flatten()
                .and_then(|outputs| outputs.get(name))
                .filter(|path| !matches!(path, Value::Null | Value::Bool(false)))
            else {
                continue;
            };
            if old_path != new_path {
                changed.push(Changed {
                    attr: attr.clone(),
                    output: name.to_owned(),
                    old_path: display(old_path),
                    new_path: display(new_path),
                });
            }
        }
    }
    changed
}

/// What jq's `join` makes of a value.
fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `drv^out1,out2` installables for the jobs whose full attribute is in
/// `wanted`, sorted and without duplicates. All non-debug outputs of a
/// derivation are included, changed or not; jobs with no such output are
/// left out.
pub fn installables(jobs: &[Job], wanted: &HashSet<&str>, set: &str) -> Vec<String> {
    let mut installables = BTreeSet::new();
    for job in jobs {
        let Some(drv) = &job.drv_path else {
            continue;
        };
        if !wanted.contains(job.full_attr(set).as_str()) {
            continue;
        }
        let mut outputs: Vec<&str> = job
            .outputs
            .iter()
            .flat_map(Outputs::iter)
            .map(|(name, _)| name)
            .filter(|name| *name != "debug")
            .collect();
        outputs.sort_unstable(); // jq's `keys` is sorted
        outputs.dedup();
        if !outputs.is_empty() {
            installables.insert(format!("{drv}^{}", outputs.join(",")));
        }
    }
    installables.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRV: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv";

    fn evaluation(lines: &[&str]) -> Evaluation {
        Evaluation::parse(&lines.join("\n"))
    }

    fn changed_lines(old: &[&str], new: &[&str], set: &str) -> Vec<String> {
        changed_outputs(&evaluation(old).jobs, &evaluation(new).jobs, set)
            .into_iter()
            .map(|c| format!("{}|{}|{}|{}", c.attr, c.output, c.old_path, c.new_path))
            .collect()
    }

    #[test]
    fn single_derivation_has_an_empty_attr() {
        let old = [r#"{"attr":"","drvPath":"d1","outputs":{"out":"/o1"}}"#];
        let new = [r#"{"attr":"","drvPath":"d2","outputs":{"out":"/o2"}}"#];
        assert_eq!(changed_lines(&old, &new, "hello"), ["hello|out|/o1|/o2"]);
    }

    #[test]
    fn set_members_get_the_set_prefix() {
        let old = [r#"{"attr":"a","drvPath":"d","outputs":{"out":"/1"}}"#];
        let new = [r#"{"attr":"a","drvPath":"d","outputs":{"out":"/2"}}"#];
        assert_eq!(changed_lines(&old, &new, "set"), ["set.a|out|/1|/2"]);
    }

    #[test]
    fn identical_debug_new_and_removed_outputs_are_not_changes() {
        let old = [
            r#"{"attr":"same","drvPath":"d","outputs":{"out":"/s"}}"#,
            r#"{"attr":"dbg","drvPath":"d","outputs":{"out":"/a","debug":"/d1"}}"#,
            r#"{"attr":"gone","drvPath":"d","outputs":{"out":"/g"}}"#,
            r#"{"attr":"fewer","drvPath":"d","outputs":{"out":"/f","dev":"/fd"}}"#,
            r#"{"attr":"nulled","drvPath":"d","outputs":{"out":null}}"#,
        ];
        let new = [
            r#"{"attr":"same","drvPath":"d2","outputs":{"out":"/s"}}"#,
            r#"{"attr":"dbg","drvPath":"d2","outputs":{"out":"/a","debug":"/d2"}}"#,
            r#"{"attr":"added","drvPath":"d2","outputs":{"out":"/n"}}"#,
            r#"{"attr":"fewer","drvPath":"d2","outputs":{"out":"/f","dev":"/fd","doc":"/new"}}"#,
            r#"{"attr":"nulled","drvPath":"d2","outputs":{"out":"/x"}}"#,
        ];
        assert_eq!(changed_lines(&old, &new, "s"), Vec::<String>::new());
    }

    #[test]
    fn several_outputs_are_reported_in_the_order_given() {
        let old = [r#"{"attr":"a","drvPath":"d","outputs":{"out":"/1","dev":"/1d","man":"/1m"}}"#];
        let new = [r#"{"attr":"a","drvPath":"d2","outputs":{"out":"/2","dev":"/1d","man":"/2m"}}"#];
        assert_eq!(
            changed_lines(&old, &new, "s"),
            ["s.a|out|/1|/2", "s.a|man|/1m|/2m"]
        );
    }

    #[test]
    fn jobs_without_a_derivation_and_errors_are_ignored() {
        let old = [
            r#"{"attr":"a","drvPath":null,"outputs":{"out":"/1"}}"#,
            r#"{"attr":"b","error":"boom"}"#,
            r#"{"attr":"c","drvPath":"d","outputs":{"out":"/1"}}"#,
        ];
        let new = [
            r#"{"attr":"a","drvPath":"d","outputs":{"out":"/2"}}"#,
            r#"{"attr":"b","drvPath":"d","outputs":{"out":"/2"}}"#,
            r#"{"attr":"c","drvPath":null,"outputs":{"out":"/2"}}"#,
            r#"{"attr":"e","error":"boom"}"#,
        ];
        assert_eq!(changed_lines(&old, &new, "s"), Vec::<String>::new());
    }

    #[test]
    fn counts_errors_and_derivations() {
        let eval = evaluation(&[
            r#"{"attr":"a","drvPath":"d","outputs":{}}"#,
            r#"{"attr":"b","drvPath":null,"error":"first"}"#,
            r#"{"attr":"c","error":"second"}"#,
            r#"{"attr":"d","error":null,"drvPath":"d"}"#,
            "",
            "not json",
        ]);
        assert_eq!((eval.job_count(), eval.error_count()), (2, 2));
        assert_eq!(eval.first_errors(1), ["first"]);
        assert_eq!(eval.unparsable_lines, 1);
    }

    #[test]
    fn installables_cover_all_non_debug_outputs_of_wanted_jobs() {
        let eval = evaluation(&[
            &format!(
                r#"{{"attr":"a","drvPath":"{DRV}","outputs":{{"out":"/1","dev":"/d","debug":"/g"}}}}"#
            ),
            r#"{"attr":"b","drvPath":"/other.drv","outputs":{"out":"/2"}}"#,
            r#"{"attr":"dbgonly","drvPath":"/dbg.drv","outputs":{"debug":"/g"}}"#,
            r#"{"attr":"nodrv","drvPath":null,"outputs":{"out":"/3"}}"#,
        ]);
        let wanted: HashSet<&str> = ["s.a", "s.dbgonly", "s.nodrv"].into();
        assert_eq!(
            installables(&eval.jobs, &wanted, "s"),
            [format!("{DRV}^dev,out")]
        );
    }

    #[test]
    fn installables_are_sorted_and_unique() {
        let eval = evaluation(&[
            r#"{"attr":"z","drvPath":"/z.drv","outputs":{"out":"/1"}}"#,
            r#"{"attr":"a","drvPath":"/a.drv","outputs":{"out":"/1"}}"#,
            r#"{"attr":"a2","drvPath":"/a.drv","outputs":{"out":"/1"}}"#,
        ]);
        let wanted: HashSet<&str> = ["s.z", "s.a", "s.a2"].into();
        assert_eq!(
            installables(&eval.jobs, &wanted, "s"),
            ["/a.drv^out", "/z.drv^out"]
        );
    }
}
