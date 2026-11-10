//! Log normalization: the Rust equivalent of the `perl -0pe` filter in the
//! original script.
//!
//! Perl's `\K` and look-around are rewritten as capture groups (`${1}`) so the
//! plain, linear-time `regex` crate can be used.

use std::borrow::Cow;

use anyhow::{Context, Result};
use nix_utility_common::store::{PLACEHOLDER, hash_pattern};
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

impl Default for Options {
    /// Everything stripped, lines in their original order.
    fn default() -> Self {
        Self {
            strip_timings: true,
            strip_paths: true,
            strip_jobs: true,
            strip_kernel_version: true,
            strip_progress: true,
            strip_pids: true,
            sort_lines: false,
        }
    }
}

struct Rule {
    re: Regex,
    replacement: Cow<'static, str>,
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
type Row = (bool, Cow<'static, str>, Cow<'static, str>);

fn rule_table(o: &Options) -> Vec<Row> {
    let always = true;
    let rows: Vec<(bool, &'static str, &'static str)> = vec![
        // Seen locally but not in cached builds.
        (always, r"(?m)^@nix \{.*\n", ""),
        // ECMA-48 control sequences (colors in gcc warnings, ...) and other
        // ECMA-48-style terminal magic.
        (always, r"\x1b\[.*?[\x40-\x7e]", ""),
        (always, r"\x1b[\x20-\x2f].", ""),
        (always, r"\x1b[\x30-\x5f]", ""),
        // Dates that sneak in impurely, then timestamps and times
        // ("M:SS.ss" at the start of a line, any number of minutes).
        (
            o.strip_timings,
            r"(?m)^(configure: autobuild timestamp\.\.\. )\d+-\d+",
            "${1}20200102-030405",
        ),
        (o.strip_timings, r"(?m)^ ?\d+:\d\d\.\d+ ", "0:00.00 "),
        (
            o.strip_timings,
            r"(?i)\b(time: |elapsed: |in )\d+(?:\.\d+)?s",
            "${1}0.00s",
        ),
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
        (
            o.strip_timings,
            r"(\bdone \()\d+(?:\.\d+)?(s\))",
            "${1}N${2}",
        ),
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
        (
            o.strip_timings,
            r"(?m)^(-- GN Done\. Made .+ in )\d+ms\b",
            "${1}Nms",
        ),
        // Build IDs, and the file names derived from them.
        (
            o.strip_paths,
            r"(?i)(\bbuild id )[0-9a-f]{16,}\b",
            "${1}BUILD_ID",
        ),
        (
            o.strip_paths,
            r"(/\.build-id/)[0-9a-f]{2}/[0-9a-f]{14,}",
            "${1}XX/BUILD_ID",
        ),
        // GLib log lines and "Started with PID: 1766".
        (o.strip_pids, r"(\(process:)\d+(\):)", "${1}N${2}"),
        (o.strip_pids, r"(\bPID: )\d+\b", "${1}N"),
        // Random temp dirs ("/build/qsb-RoAGjy/qsb_spv_temp") and GLib timestamps.
        (
            o.strip_paths,
            r"((?:/build|/tmp)/[A-Za-z]\w*-)[A-Za-z]{6}/",
            "${1}XXXXXX/",
        ),
        (
            o.strip_timings,
            r"(\*\*: )\d\d:\d\d:\d\d\.\d+:",
            "${1}HH:MM:SS.mmm:",
        ),
        // Qt Test totals.
        (
            o.strip_timings,
            r"(\bTotals: [^\n]*?, )\d+(?:\.\d+)?ms\b",
            "${1}Nms",
        ),
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
    ];
    // Normalize all store hashes first.
    let store = (
        always,
        Cow::Owned(format!("/nix/store/{}-", hash_pattern())),
        Cow::Owned(format!("/nix/store/{PLACEHOLDER}-")),
    );
    std::iter::once(store)
        .chain(
            rows.into_iter()
                .map(|(on, pattern, replacement)| (on, pattern.into(), replacement.into())),
        )
        .collect()
}

impl Normalizer {
    pub fn new(opts: &Options) -> Result<Self> {
        let rules = rule_table(opts)
            .into_iter()
            .filter(|(enabled, ..)| *enabled)
            .map(|(_, pattern, replacement)| {
                let re = Regex::new(&pattern)
                    .with_context(|| format!("invalid built-in pattern: {pattern}"))?;
                Ok(Rule { re, replacement })
            })
            .collect::<Result<Vec<_>>>()?;
        let run_sorters = SORTED_RUNS
            .iter()
            .map(|p| Regex::new(p).with_context(|| format!("invalid built-in pattern: {p}")))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            rules,
            run_sorters,
            sort_lines: opts.sort_lines,
        })
    }

    /// Normalize a raw log. Invalid UTF-8 is replaced with U+FFFD, which hits
    /// both logs of a comparison the same way.
    pub fn normalize(&self, raw: &[u8]) -> String {
        let mut text = expand_tabs(&String::from_utf8_lossy(raw));

        for rule in &self.rules {
            if let Cow::Owned(replaced) = rule.re.replace_all(&text, rule.replacement.as_ref()) {
                text = replaced;
            }
        }
        for re in &self.run_sorters {
            if let Cow::Owned(replaced) = re.replace_all(&text, |c: &Captures| sort_run(&c[0])) {
                text = replaced;
            }
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
    fn store_hashes_and_colors() {
        assert_eq!(
            norm("\x1b[1;31mwarn\x1b[0m /nix/store/abcdfghijklmnpqrsvwxyz0123456789-foo\n"),
            "warn /nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-foo\n"
        );
    }

    #[test]
    fn timings_and_counters() {
        assert_eq!(
            norm("-- Configuring done (14.6s)\n"),
            "-- Configuring done (Ns)\n"
        );
        assert_eq!(
            norm("[13/17] CC foo.o\n[ 45%] Built\n"),
            "[N/N] CC foo.o\n[N%] Built\n"
        );
        assert_eq!(
            norm(" 9/86 Test  #7: foo ....   Passed    0.02 sec\n"),
            "N/N Test  #7: foo ....   Passed N sec\n"
        );
        assert_eq!(
            norm("buildPhase completed in 1 minutes 5 seconds\nnext\n"),
            "next\n"
        );
    }

    #[test]
    fn paths_pids_jobs_kernel() {
        assert_eq!(norm("/build/qsb-RoAGjy/x\n"), "/build/qsb-XXXXXX/x\n");
        assert_eq!(
            norm("(process:842): Gtk-CRITICAL **: 07:21:29.789: x\n"),
            "(process:N): Gtk-CRITICAL **: HH:MM:SS.mmm: x\n"
        );
        assert_eq!(norm("make -j23 --jobs=4\n"), "make -jN --jobs=N\n");
        assert_eq!(
            norm("using 97 parallel threads\n"),
            "using N parallel threads\n"
        );
        assert_eq!(
            norm("-- CMAKE_HOST_SYSTEM: \"Linux-6.18.57\"\n"),
            "-- CMAKE_HOST_SYSTEM: \"Linux-KERNEL_VERSION\"\n"
        );
    }

    #[test]
    fn sorts_runs_and_lines() {
        assert_eq!(
            norm(
                "Installing /nix/store/b\nInstalling /nix/store/a\nother\nInstalling /nix/store/d\nInstalling /nix/store/c\n"
            ),
            "Installing /nix/store/a\nInstalling /nix/store/b\nother\nInstalling /nix/store/c\nInstalling /nix/store/d\n"
        );
        assert_eq!(sort_lines("b\na\nb"), "a\nb\nb\n");
    }

    #[test]
    fn expands_tabs() {
        assert_eq!(expand_tabs("a\tb\n\tc"), "a       b\n        c");
    }

    #[test]
    fn empty_input() {
        assert_eq!(norm(""), "");
        assert_eq!(
            norm_with(
                Options {
                    sort_lines: true,
                    ..opts()
                },
                ""
            ),
            ""
        );
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let n = Normalizer::new(&opts()).unwrap();
        assert_eq!(n.normalize(b"a\xffb\n"), "a\u{fffd}b\n");
    }

    #[test]
    fn nix_internal_json_lines_are_dropped() {
        assert_eq!(norm("@nix {\"action\":\"msg\"}\nkeep\n"), "keep\n");
        assert_eq!(norm("not @nix {\"a\":1}\n"), "not @nix {\"a\":1}\n");
    }

    #[test]
    fn terminal_controls() {
        assert_eq!(
            norm("\x1b[01;31m\x1b[Kerror\x1b[m\x1b[K: x\n"),
            "error: x\n"
        );
        assert_eq!(norm("a\x1b(Bb\n"), "ab\n");
        assert_eq!(norm("a\x1b=b\n"), "ab\n");
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(
            norm("configure: autobuild timestamp... 20231125-081234\n"),
            "configure: autobuild timestamp... 20200102-030405\n"
        );
        assert_eq!(norm("0:01.23 foo\n"), norm("2:59.99 foo\n"));
        assert!(norm("0:01.23 foo\n").ends_with("0:00.00 foo\n"));
        // the width of the minutes must not matter
        assert_eq!(norm("9:59.99 foo\n"), norm("10:01.02 foo\n"));
        assert_eq!(norm(" 9:59.99 foo\n"), norm("123:45.67 foo\n"));
        assert_eq!(norm("Elapsed: 12.5s\n"), "Elapsed: 0.00s\n");
        assert_eq!(norm("finished in 3s\n"), "finished in 0.00s\n");
    }

    #[test]
    fn ctest_durations() {
        assert_eq!(
            norm("Total Test time (real) =   3.39 sec\n"),
            "Total Test time (real) = N sec\n"
        );
        // the padding varies with the width of the number, so it must not matter
        let narrow = norm("1: 10/86 Test #7: foo ....   Passed    0.02 sec\n");
        let wide = norm("1: 9/86 Test #7: foo ....   Passed   12.50 sec\n");
        assert_eq!(narrow, wide);
        assert!(narrow.starts_with("1: N/N Test #7: foo ...."), "{narrow}");
        assert!(narrow.ends_with("Passed N sec\n"), "{narrow}");
    }

    #[test]
    fn phase_footers_of_all_units() {
        assert_eq!(
            norm("installPhase completed in 2 hours 3 minutes 4 seconds\nx\n"),
            "x\n"
        );
        assert_eq!(norm("buildPhase completed in 45 seconds\nx\n"), "x\n");
        // not a footer: has leading text on the line
        assert_eq!(
            norm("note: buildPhase completed in 45 seconds\n"),
            "note: buildPhase completed in 45 seconds\n"
        );
    }

    #[test]
    fn dbus_addresses_and_build_ids() {
        assert_eq!(
            norm("unix:path=/tmp/dbus-ggOBj2AOpQ,guid=12ad34ef12ad34ef12ad34ef12ad34ef\n"),
            "unix:path=/tmp/dbus-RANDOM,guid=GUID\n"
        );
        assert_eq!(
            norm("note (build ID 36ad3d4f5a6b7c8d9e0f)\n"),
            "note (build ID BUILD_ID)\n"
        );
        assert_eq!(
            norm("/lib/.build-id/36/ad3d4f5a6b7c8d9e0f1a2b.debug\n"),
            "/lib/.build-id/XX/BUILD_ID.debug\n"
        );
    }

    #[test]
    fn gn_and_qt_timings() {
        assert_eq!(
            norm("-- GN Done. Made 123 targets in 20890ms\n"),
            "-- GN Done. Made 123 targets in Nms\n"
        );
        assert_eq!(
            norm("Totals: 5 passed, 0 failed, 0 skipped, 0 blacklisted, 4393ms\n"),
            "Totals: 5 passed, 0 failed, 0 skipped, 0 blacklisted, Nms\n"
        );
    }

    #[test]
    fn kernel_version_variants() {
        assert_eq!(
            norm("-- CMAKE_HOST_SYSTEM_VERSION: \"6.18.57\"\n"),
            "-- CMAKE_HOST_SYSTEM_VERSION: \"KERNEL_VERSION\"\n"
        );
        assert_eq!(
            norm("CMAKE_SYSTEM: \"Linux-6.1.0-nixos\"\n"),
            "CMAKE_SYSTEM: \"Linux-KERNEL_VERSION\"\n"
        );
    }

    #[test]
    fn pid_and_job_variants() {
        assert_eq!(norm("Started with PID: 1766\n"), "Started with PID: N\n");
        assert_eq!(norm("make -j 8 --jobs 4\n"), "make -j N --jobs N\n");
        assert_eq!(norm("-j1\n"), "-jN\n");
        assert_eq!(
            norm("using 4 parallel workers\n"),
            "using N parallel workers\n"
        );
        // dashes inside words are not job flags
        assert_eq!(norm("foo-j3\n"), "foo-j3\n");
    }

    #[test]
    fn progress_counters_only_at_line_start() {
        assert_eq!(norm("[ 3/17] x\n"), "[N/N] x\n");
        assert_eq!(norm("a\r[ 5%] x\n"), "a\r[N%] x\n");
        assert_eq!(norm("see [13/17] here\n"), "see [13/17] here\n");
        assert_eq!(norm("2: 9/86 Test #7: foo\n"), "2: N/N Test #7: foo\n");
    }

    #[test]
    fn each_keep_flag_disables_its_rules() {
        type Case = (fn(&mut Options), &'static str);
        let cases: [Case; 9] = [
            (|o| o.strip_timings = false, "-- Configuring done (14.6s)\n"),
            (|o| o.strip_timings = false, "Elapsed: 12.5s\n"),
            (|o| o.strip_timings = false, "1:02.34 foo\n"),
            (
                |o| o.strip_timings = false,
                "configure: autobuild timestamp... 20231125-081234\n",
            ),
            (|o| o.strip_paths = false, "/build/qsb-RoAGjy/x\n"),
            (|o| o.strip_jobs = false, "make -j23\n"),
            (
                |o| o.strip_kernel_version = false,
                "CMAKE_SYSTEM: \"Linux-6.1\"\n",
            ),
            (|o| o.strip_progress = false, "[13/17] x\n"),
            (|o| o.strip_pids = false, "(process:842): x\n"),
        ];
        for (tweak, input) in cases {
            let mut o = opts();
            tweak(&mut o);
            assert_eq!(norm_with(o, input), input);
            assert_ne!(norm(input), input);
        }
    }

    #[test]
    fn store_hashes_are_always_normalized() {
        let o = Options {
            strip_timings: false,
            strip_paths: false,
            strip_jobs: false,
            strip_kernel_version: false,
            strip_progress: false,
            strip_pids: false,
            sort_lines: false,
        };
        assert_eq!(
            norm_with(o, "/nix/store/abcdfghijklmnpqrsvwxyz0123456789-x\n"),
            "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-x\n"
        );
    }

    #[test]
    fn libtool_and_shrinking_runs_are_sorted() {
        assert_eq!(
            norm("libtool: compile: b\nlibtool: compile: a\nx\n"),
            "libtool: compile: a\nlibtool: compile: b\nx\n"
        );
        assert_eq!(
            norm("shrinking /nix/store/z\nshrinking /nix/store/y\n"),
            "shrinking /nix/store/y\nshrinking /nix/store/z\n"
        );
    }

    #[test]
    fn sort_lines_edge_cases() {
        assert_eq!(sort_lines(""), "");
        assert_eq!(sort_lines("\n"), "\n");
        assert_eq!(sort_lines("b\n\na\n"), "\na\nb\n");
        // byte order: uppercase before lowercase
        assert_eq!(sort_lines("a\nB\n"), "B\na\n");
    }

    #[test]
    fn expand_tabs_columns() {
        assert_eq!(expand_tabs("\t"), "        ");
        assert_eq!(expand_tabs("1234567\tx"), "1234567 x");
        assert_eq!(expand_tabs("12345678\tx"), "12345678        x");
        // backspace moves the column back
        assert_eq!(expand_tabs("ab\x08\tx"), "ab\x08       x");
    }
}
