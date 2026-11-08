//! The jobs logic was ported from `jq` filters. This runs the original
//! filters and the Rust port on the same generated evaluations and compares
//! their results. Skipped when `jq` is not installed.

use std::collections::HashSet;
use std::fs;
use std::process::Command;

use diff_nixpkgs_attr_core::jobs::{Evaluation, changed_outputs, installables};

const CHANGED_FILTER: &str = r#"
    def full: if .attr == "" then $set else $set + "." + .attr end;
    ($old | map(select(.drvPath != null) | {key: full, value: (.outputs // {})})
          | from_entries) as $o
    | $new[]
    | select(.drvPath != null)
    | full as $attr
    | (.outputs // {}) | to_entries[]
    | select(.key != "debug" and .value != null)
    | select(($o[$attr][.key] // .value) != .value)
    | [$attr, .key, $o[$attr][.key], .value] | join("|")
"#;

const INSTALLABLES_FILTER: &str = r#"
    def full: if .attr == "" then $set else $set + "." + .attr end;
    ($want | split("\n")) as $w
    | select(.drvPath != null and (full | IN($w[])))
    | ((.outputs // {} | keys) - ["debug"]) as $outs
    | select(($outs | length) > 0)
    | .drvPath + "^" + ($outs | join(","))
"#;

fn have_jq() -> bool {
    Command::new("jq").arg("--version").output().is_ok()
}

fn jq(args: &[&str]) -> String {
    let output = Command::new("jq").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Small deterministic generator, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

fn path(rng: &mut Rng, output: &str) -> String {
    // Few distinct values, so sides often agree.
    format!(
        "\"/nix/store/{}-p-{output}\"",
        rng.pick(&["h1", "h2", "h3"])
    )
}

fn job(rng: &mut Rng, attr: &str) -> String {
    let attr_json = format!("\"{attr}\"");
    match rng.below(10) {
        0 => return format!(r#"{{"attr":{attr_json},"error":"boom"}}"#),
        1 => return format!(r#"{{"attr":{attr_json},"drvPath":null,"outputs":{{"out":"/x"}}}}"#),
        _ => {}
    }
    let mut outputs = Vec::new();
    for name in ["out", "dev", "debug", "man"] {
        match rng.below(6) {
            0 => {}
            1 => outputs.push(format!("\"{name}\":null")),
            _ => outputs.push(format!("\"{name}\":{}", path(rng, name))),
        }
    }
    let drv = format!("\"/nix/store/{}-d.drv\"", rng.pick(&["d1", "d2", "d3"]));
    let outputs = if rng.below(12) == 0 {
        String::new() // no outputs key at all
    } else {
        format!(r#","outputs":{{{}}}"#, outputs.join(","))
    };
    format!(r#"{{"attr":{attr_json},"drvPath":{drv}{outputs}}}"#)
}

fn evaluation(rng: &mut Rng, single: bool) -> String {
    if single {
        return job(rng, "");
    }
    let mut lines = Vec::new();
    for attr in ["a", "b", "c", "d", "e", "a"] {
        // "a" twice: the last duplicate wins on the old side
        if rng.below(5) != 0 {
            lines.push(job(rng, attr));
        }
    }
    lines.join("\n")
}

#[test]
fn rust_port_matches_the_original_jq_filters() {
    if !have_jq() {
        eprintln!("jq not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (old_file, new_file, want_file) = (
        dir.path().join("old.jsonl"),
        dir.path().join("new.jsonl"),
        dir.path().join("want"),
    );
    let mut rng = Rng(42);
    let mut compared = 0;
    let mut total_changes = 0;

    for case in 0..400 {
        let single = case % 5 == 0;
        let set = "pkgs-set";
        let (old, new) = (evaluation(&mut rng, single), evaluation(&mut rng, single));
        fs::write(&old_file, format!("{old}\n")).unwrap();
        fs::write(&new_file, format!("{new}\n")).unwrap();
        let (old_eval, new_eval) = (Evaluation::parse(&old), Evaluation::parse(&new));

        let expected = jq(&[
            "-n",
            "-r",
            "--arg",
            "set",
            set,
            "--slurpfile",
            "old",
            old_file.to_str().unwrap(),
            "--slurpfile",
            "new",
            new_file.to_str().unwrap(),
            CHANGED_FILTER,
        ]);
        let actual: String = changed_outputs(&old_eval.jobs, &new_eval.jobs, set)
            .iter()
            .map(|c| format!("{}|{}|{}|{}\n", c.attr, c.output, c.old_path, c.new_path))
            .collect();
        assert_eq!(
            actual, expected,
            "changed outputs, case {case}\nold:\n{old}\nnew:\n{new}"
        );
        total_changes += actual.lines().count();

        // As in the script: the attrs file lists the changed attributes.
        let mut attrs: Vec<&str> = actual.lines().filter_map(|l| l.split('|').next()).collect();
        attrs.sort_unstable();
        attrs.dedup();
        fs::write(
            &want_file,
            attrs.iter().map(|a| format!("{a}\n")).collect::<String>(),
        )
        .unwrap();
        let wanted: HashSet<&str> = attrs.iter().copied().collect();
        for (side, file, eval) in [("old", &old_file, &old_eval), ("new", &new_file, &new_eval)] {
            let raw = jq(&[
                "-r",
                "--arg",
                "set",
                set,
                "--rawfile",
                "want",
                want_file.to_str().unwrap(),
                INSTALLABLES_FILTER,
                file.to_str().unwrap(),
            ]);
            let mut expected: Vec<&str> = raw.lines().collect();
            expected.sort_unstable();
            expected.dedup();
            assert_eq!(
                installables(&eval.jobs, &wanted, set),
                expected,
                "installables ({side}), case {case}\n{old}\n---\n{new}"
            );
        }
        compared += 1;
    }
    assert_eq!(compared, 400);
    assert!(
        total_changes > 100,
        "the generator should produce changes ({total_changes})"
    );
}
