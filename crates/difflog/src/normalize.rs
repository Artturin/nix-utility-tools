//! Log normalization: the Rust equivalent of the `perl -0pe` filter in the
//! original script.
//!
//! Perl's `\K` and look-around are rewritten as capture groups (`${1}`) so the
//! plain, linear-time `regex` crate can be used.

use std::borrow::Cow;

use anyhow::{Context, Result};
use regex::{Captures, Regex};

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub strip_timings: bool,
    pub strip_paths: bool,
    pub strip_jobs: bool,
    pub strip_kernel_version: bool,
    pub strip_progress: bool,
    pub strip_pids: bool,
    /// Sort all lines of the normalized log (like `LC_ALL=C sort`).
    pub sort_lines: bool,
}

struct Rule {
    re: Regex,
    replacement: &'static str,
}

pub struct Normalizer {
    rules: Vec<Rule>,
    run_sorters: Vec<Regex>,
    sort_lines: bool,
}

/// Runs of adjacent lines of these kinds get sorted, to reduce
/// nondeterminism from concurrency.
const SORTED_RUNS: [&str; 5] = [
    r"(?m)(?:^(?:libtool: compile: |/nix/store/\S+/bin/bash \S+/libtool ).*\n)+",
    r"(?m)(?:^(?:libtool: link: |/nix/store/\S+/bin/bash \S+/libtool ).*\n)+",
    r"(?m)(?:^shrinking /nix/store/.*\n)+",
    r"(?m)(?:^Installing /nix/store/.*\n)+",
    r"(?m)(?:^/nix/store/\S+: interpreter directive changed .*\n)+",
];

/// (enabled, pattern, replacement), applied in order.
type Row = (bool, &'static str, &'static str);

fn rule_table(o: &Options) -> Vec<Row> {
    let always = true;
    vec![
        // Normalize all store hashes.
        (always, r"/nix/store/[a-z0-9]{32}-", "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-"),
        // Seen locally but not in cached builds.
        (always, r"(?m)^@nix \{.*\n", ""),
        // ECMA-48 control sequences (colors in gcc warnings, ...) and other
        // ECMA-48-style terminal magic.
        (always, r"\x1b\[.*?[\x40-\x7e]", ""),
        (always, r"\x1b[\x20-\x2f].", ""),
        (always, r"\x1b[\x30-\x5f]", ""),
        // Dates that sneak in impurely, then timestamps and times.
        (always, r"(?m)^(configure: autobuild timestamp\.\.\. )\d+-\d+", "${1}20200102-030405"),
        (always, r"(?m)^(\d?) ?\d:\d\d\.\d+ ", "${1} 0:00.00 "),
        (always, r"(?i)\b(time: |elapsed: |in )\d+(?:\.\d+)?s", "${1}0.00s"),
        // CTest durations; the padding before the number varies with its
        // width, so it is swallowed too. Must run before the "N/N" counter rule.
        (
            o.strip_timings,
            r"(Total Test time \(\w+\) =)[ ]+\d+(?:\.\d+)?[ ]+sec",
            "${1} N sec",
        ),
        (
            o.strip_timings,
            r"(?m)^((?:[\w-]+: )?[ ]*\d+/\d+ Test[ ]+#\d+: [^\n]*?)[ ]+\d+(?:\.\d+)?[ ]+sec$",
            "${1} N sec",
        ),
        // stdenv phase footers are only printed for phases that took 30s or
        // more, so drop the whole line rather than normalizing the duration.
        (
            o.strip_timings,
            r"(?m)^\S+ completed in (?:\d+ (?:hours|minutes|seconds)[ ]?)+\n",
            "",
        ),
        // Random D-Bus session addresses.
        (o.strip_paths, r"(/tmp/dbus-)[A-Za-z0-9]+", "${1}RANDOM"),
        (o.strip_paths, r"(,guid=)[0-9a-f]{32}\b", "${1}GUID"),
        // CMake step timings: "-- Configuring done (14.6s)".
        (o.strip_timings, r"(\bdone \()\d+(?:\.\d+)?(s\))", "${1}N${2}"),
        // Host kernel version printed by CMake.
        (
            o.strip_kernel_version,
            r#"(\bCMAKE_(?:HOST_)?SYSTEM: "Linux-)[^"]+""#,
            "${1}KERNEL_VERSION\"",
        ),
        (
            o.strip_kernel_version,
            r#"(\bCMAKE_HOST_SYSTEM_VERSION: ")[^"]+""#,
            "${1}KERNEL_VERSION\"",
        ),
        // GN completion timings.
        (o.strip_timings, r"(?m)^(-- GN Done\. Made .+ in )\d+ms\b", "${1}Nms"),
        // Build IDs, and the file names derived from them.
        (o.strip_paths, r"(?i)(\bbuild id )[0-9a-f]{16,}\b", "${1}BUILD_ID"),
        (o.strip_paths, r"(/\.build-id/)[0-9a-f]{2}/[0-9a-f]{14,}", "${1}XX/BUILD_ID"),
        // GLib log lines and "Started with PID: 1766".
        (o.strip_pids, r"(\(process:)\d+(\):)", "${1}N${2}"),
        (o.strip_pids, r"(\bPID: )\d+\b", "${1}N"),
        // Random temp dirs ("/build/qsb-RoAGjy/qsb_spv_temp") and GLib timestamps.
        (o.strip_paths, r"((?:/build|/tmp)/[A-Za-z]\w*-)[A-Za-z]{6}/", "${1}XXXXXX/"),
        (o.strip_timings, r"(\*\*: )\d\d:\d\d:\d\d\.\d+:", "${1}HH:MM:SS.mmm:"),
        // Qt Test totals.
        (o.strip_timings, r"(\bTotals: [^\n]*?, )\d+(?:\.\d+)?ms\b", "${1}Nms"),
        // Job counts and thread/worker counts depend on the build machine.
        (o.strip_jobs, r"(^|[^\w-])(-j ?)\d+\b", "${1}${2}N"),
        (o.strip_jobs, r"(--jobs[= ])\d+\b", "${1}N"),
        (
            o.strip_jobs,
            r"\b\d+ parallel (threads?|jobs|workers|processes)\b",
            "N parallel ${1}",
        ),
        // Progress counters at the start of a line (or after a \r):
        // ninja "[13/17] ", cmake/make "[ 45%] ", CTest " 9/86 Test  #7: ".
        (o.strip_progress, r"(?m)(^|\r)\[ *\d+/\d+\]", "${1}[N/N]"),
        (o.strip_progress, r"(?m)(^|\r)\[ *\d+(%\])", "${1}[N${2}"),
        (
            o.strip_progress,
            r"(?m)^((?:[\w-]+: )?) *\d+/\d+( Test +#\d+: )",
            "${1}N/N${2}",
        ),
    ]
}

impl Normalizer {
    pub fn new(opts: &Options) -> Result<Self> {
        let rules = rule_table(opts)
            .into_iter()
            .filter(|(enabled, ..)| *enabled)
            .map(|(_, pattern, replacement)| {
                let re = Regex::new(pattern)
                    .with_context(|| format!("invalid built-in pattern: {pattern}"))?;
                Ok(Rule { re, replacement })
            })
            .collect::<Result<Vec<_>>>()?;
        let run_sorters = SORTED_RUNS
            .iter()
            .map(|p| Regex::new(p).with_context(|| format!("invalid built-in pattern: {p}")))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { rules, run_sorters, sort_lines: opts.sort_lines })
    }

    /// Normalize a raw log. Invalid UTF-8 is replaced with U+FFFD, which hits
    /// both logs of a comparison the same way.
    pub fn normalize(&self, raw: &[u8]) -> String {
        let mut text = expand_tabs(&String::from_utf8_lossy(raw));

        for rule in &self.rules {
            let replaced = match rule.re.replace_all(&text, rule.replacement) {
                Cow::Owned(s) => s,
                Cow::Borrowed(_) => continue,
            };
            text = replaced;
        }
        for re in &self.run_sorters {
            let replaced = match re.replace_all(&text, |c: &Captures| sort_run(&c[0])) {
                Cow::Owned(s) => s,
                Cow::Borrowed(_) => continue,
            };
            text = replaced;
        }
        if self.sort_lines {
            text = sort_lines(&text);
        }
        text
    }
}

/// Sort the lines of a run (each line keeps its trailing newline).
fn sort_run(run: &str) -> String {
    let mut lines: Vec<&str> = run.split_inclusive('\n').collect();
    lines.sort_unstable();
    lines.concat()
}

/// Like `LC_ALL=C sort`: byte order, duplicates kept, output newline-terminated.
fn sort_lines(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut lines: Vec<&str> = body.split('\n').collect();
    lines.sort_unstable();
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Like `expand(1)`: tabs to spaces, tab stops every 8 columns. This matters
/// because `make` trace continuation lines are indented with tabs locally but
/// spaces in cached builds.
fn expand_tabs(input: &str) -> String {
    const TAB_WIDTH: usize = 8;
    let mut out = String::with_capacity(input.len());
    let mut col = 0usize;
    for c in input.chars() {
        match c {
            '\t' => {
                let n = TAB_WIDTH - col % TAB_WIDTH;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            '\n' => {
                out.push(c);
                col = 0;
            }
            '\x08' => {
                out.push(c);
                col = col.saturating_sub(1);
            }
            _ => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options {
            strip_timings: true,
            strip_paths: true,
            strip_jobs: true,
            strip_kernel_version: true,
            strip_progress: true,
            strip_pids: true,
            sort_lines: false,
        }
    }

    fn norm_with(o: Options, s: &str) -> String {
        Normalizer::new(&o).unwrap().normalize(s.as_bytes())
    }

    fn norm(s: &str) -> String {
        norm_with(opts(), s)
    }

    #[test]
    fn store_hash_is_32_chars() {
        let (_, _, rep) = rule_table(&opts())[0];
        assert_eq!(rep, format!("/nix/store/{}-", "e".repeat(32)));
    }

    #[test]
    fn store_hashes_and_colors() {
        assert_eq!(
            norm("\x1b[1;31mwarn\x1b[0m /nix/store/abcdfghijklmnpqrsvwxyz0123456789-foo\n"),
            "warn /nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-foo\n"
        );
    }

    #[test]
    fn timings_and_counters() {
        assert_eq!(norm("-- Configuring done (14.6s)\n"), "-- Configuring done (Ns)\n");
        assert_eq!(norm("[13/17] CC foo.o\n[ 45%] Built\n"), "[N/N] CC foo.o\n[N%] Built\n");
        assert_eq!(
            norm(" 9/86 Test  #7: foo ....   Passed    0.02 sec\n"),
            "N/N Test  #7: foo ....   Passed N sec\n"
        );
        assert_eq!(norm("buildPhase completed in 1 minutes 5 seconds\nnext\n"), "next\n");
    }

    #[test]
    fn paths_pids_jobs_kernel() {
        assert_eq!(norm("/build/qsb-RoAGjy/x\n"), "/build/qsb-XXXXXX/x\n");
        assert_eq!(norm("(process:842): Gtk-CRITICAL **: 07:21:29.789: x\n"),
                   "(process:N): Gtk-CRITICAL **: HH:MM:SS.mmm: x\n");
        assert_eq!(norm("make -j23 --jobs=4\n"), "make -jN --jobs=N\n");
        assert_eq!(norm("using 97 parallel threads\n"), "using N parallel threads\n");
        assert_eq!(norm("-- CMAKE_HOST_SYSTEM: \"Linux-6.18.57\"\n"),
                   "-- CMAKE_HOST_SYSTEM: \"Linux-KERNEL_VERSION\"\n");
    }

    #[test]
    fn keep_flags_disable_rules() {
        let o = Options { strip_jobs: false, strip_progress: false, ..opts() };
        assert_eq!(norm_with(o, "make -j23\n[13/17] x\n"), "make -j23\n[13/17] x\n");
    }

    #[test]
    fn sorts_runs_and_lines() {
        assert_eq!(
            norm("Installing /nix/store/b\nInstalling /nix/store/a\nother\nInstalling /nix/store/d\nInstalling /nix/store/c\n"),
            "Installing /nix/store/a\nInstalling /nix/store/b\nother\nInstalling /nix/store/c\nInstalling /nix/store/d\n"
        );
        assert_eq!(sort_lines("b\na\nb"), "a\nb\nb\n");
    }

    #[test]
    fn expands_tabs() {
        assert_eq!(expand_tabs("a\tb\n\tc"), "a       b\n        c");
    }
}
