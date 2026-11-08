mod cli;
mod normalize;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::LazyLock;

use anstyle::{Ansi256Color, AnsiColor, Color, Style};
use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use log::{LevelFilter, error, info, warn};
use regex::Regex;

use cli::Cli;
use normalize::{Normalizer, Options};

/// Log target for the grey informational notes.
const NOTE: &str = "difflog::note";

macro_rules! note {
    ($($arg:tt)*) => { log::info!(target: NOTE, $($arg)*) };
}

const GREY: Style = Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(245))));
const GREEN: Style = AnsiColor::Green.on_default();
const RED: Style = AnsiColor::Red.on_default();

/// The `HASH-` prefix of a store path name.
static STORE_HASH_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-df-np-sv-z]{32}-").unwrap());
static UNSAFE_FILENAME_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9._+-]").unwrap());

fn init_logging(level: LevelFilter) {
    env_logger::Builder::new()
        .filter_level(level)
        .parse_default_env()
        .format(|buf, record| {
            let style = match (record.target(), record.level()) {
                (NOTE, _) | (_, log::Level::Debug | log::Level::Trace) => GREY,
                (_, log::Level::Info) => GREEN,
                (_, log::Level::Warn | log::Level::Error) => RED,
            };
            writeln!(buf, "{style}difflog: {}{style:#}", record.args())
        })
        .init();
}

/// If `arg` is a symlink (and following is enabled), return its final target;
/// otherwise return `arg` unchanged.
fn resolve(arg: &str, follow: bool) -> Result<String> {
    let is_symlink = fs::symlink_metadata(arg).is_ok_and(|m| m.file_type().is_symlink());
    if !(follow && is_symlink) {
        return Ok(arg.to_owned());
    }
    let target = fs::canonicalize(arg)
        .with_context(|| format!("cannot resolve symlink: {arg}"))?
        .into_os_string()
        .into_string()
        .map_err(|p| anyhow!("symlink target is not valid UTF-8: {p:?}"))?;
    note!("{arg} -> {target}");
    Ok(target)
}

/// A readable file name for the diff header: the resolved store path without
/// its hash, made safe for use as a file name.
fn label(arg: &str) -> String {
    let path = fs::canonicalize(arg).unwrap_or_else(|_| PathBuf::from(arg));
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = STORE_HASH_PREFIX.replace(&base, "");
    UNSAFE_FILENAME_CHARS.replace_all(&base, "_").into_owned()
}

/// Fetch the log with `nix log`, normalize it and write it to `dest`.
fn write_log(normalizer: &Normalizer, installable: &str, dest: &Path) -> Result<()> {
    let output = Command::new("nix")
        .arg("log")
        .arg(installable)
        .stderr(Stdio::inherit())
        .output()
        .context("failed to run `nix log`")?;
    if !output.status.success() {
        bail!("`nix log {installable}` failed ({})", output.status);
    }
    fs::write(dest, normalizer.normalize(&output.stdout))
        .with_context(|| format!("failed to write {}", dest.display()))
}

/// Arguments for the final `git diff`; shared with --dry-run so they cannot drift.
fn git_diff_args(cli: &Cli, name_a: &str, name_b: &str) -> Vec<String> {
    ["diff", "--no-index", "--no-prefix"]
        .into_iter()
        .map(str::to_owned)
        .chain(cli.git_args.iter().cloned())
        .chain([format!("a/{name_a}"), format!("b/{name_b}")])
        .collect()
}

fn dry_run(cli: &Cli, a: &str, b: &str, name_a: &str, name_b: &str) -> ExitCode {
    note!("dry-run: nothing is executed and no files are created");
    for (side, installable, name) in [("a", a, name_a), ("b", b, name_b)] {
        note!(
            "dry-run: would run: nix log {} | normalize > <tmpdir>/{side}/{name}",
            shell_words::quote(installable)
        );
    }
    note!(
        "dry-run: would run in <tmpdir>: git {}",
        shell_words::join(git_diff_args(cli, name_a, name_b))
    );
    ExitCode::SUCCESS
}

fn run(cli: &Cli) -> Result<ExitCode> {
    let follow = !cli.no_follow;
    let a = resolve(&cli.first, follow)?;
    let b = resolve(&cli.second, follow)?;
    let name_a = format!("{}.log", label(&a));
    let name_b = format!("{}.log", label(&b));

    let normalizer = Normalizer::new(&Options {
        strip_timings: !cli.keep_timings,
        strip_paths: !cli.keep_paths,
        strip_jobs: !cli.keep_jobs,
        strip_kernel_version: !cli.keep_kernel_version,
        strip_progress: !cli.keep_progress,
        strip_pids: !cli.keep_pids,
        sort_lines: cli.sort_lines,
    })?;

    if cli.sort_lines {
        note!("lines are SORTED before diffing (--sort-lines); ordering differences are hidden");
    }
    if cli.dry_run {
        return Ok(dry_run(cli, &a, &b, &name_a, &name_b));
    }

    // Removed automatically when dropped, on success and on error.
    let tmp = tempfile::tempdir().context("failed to create temp dir")?;
    for side in ["a", "b"] {
        fs::create_dir(tmp.path().join(side))?;
    }
    write_log(&normalizer, &a, &tmp.path().join("a").join(&name_a))?;
    write_log(&normalizer, &b, &tmp.path().join("b").join(&name_b))?;

    // When git diff opens a pager, quitting it may make git exit non-zero,
    // which can interfere with scripts that use difflog.
    let status = Command::new("git")
        .args(git_diff_args(cli, &name_a, &name_b))
        .current_dir(tmp.path())
        .status()
        .context("failed to run `git diff`")?;
    match status.code() {
        Some(0) => {
            info!("No changes detected (Exit 0)");
            Ok(ExitCode::SUCCESS)
        }
        Some(1) => {
            warn!("Changes detected! (Exit 1)");
            Ok(ExitCode::from(1))
        }
        _ => bail!("`git diff` failed ({status})"),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose.log_level_filter());
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            error!("{err:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("difflog").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn store_hash_prefix_matches_only_valid_hashes() {
        let hash = "abcdfghijklmnpqrsvwxyz0123456789";
        assert!(STORE_HASH_PREFIX.is_match(&format!("{hash}-foo")));
        // e, o, t and u are not in the nix base32 alphabet
        assert!(!STORE_HASH_PREFIX.is_match("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-foo"));
        assert!(!STORE_HASH_PREFIX.is_match("abc-foo"));
    }

    #[test]
    fn label_strips_hash_and_sanitizes() {
        let hash = "abcdfghijklmnpqrsvwxyz0123456789";
        assert_eq!(
            label(&format!("/nix/store/{hash}-hello-2.12.drv")),
            "hello-2.12.drv"
        );
        assert_eq!(label("/nonexistent/weird name:1"), "weird_name_1");
        assert_eq!(label("nixpkgs#hello"), "nixpkgs_hello");
        assert_eq!(label("/"), "");
    }

    #[test]
    fn resolve_leaves_non_symlinks_alone() {
        assert_eq!(resolve("nixpkgs#hello", true).unwrap(), "nixpkgs#hello");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, "").unwrap();
        let file = file.to_str().unwrap();
        assert_eq!(resolve(file, true).unwrap(), file);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_follows_symlink_chains() {
        let dir = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(dir.path()).unwrap().join("real");
        fs::write(&real, "").unwrap();
        let l1 = dir.path().join("l1");
        let l2 = dir.path().join("l2");
        std::os::unix::fs::symlink(&real, &l1).unwrap();
        std::os::unix::fs::symlink(&l1, &l2).unwrap();
        let l2 = l2.to_str().unwrap();
        assert_eq!(resolve(l2, true).unwrap(), real.to_str().unwrap());
        assert_eq!(resolve(l2, false).unwrap(), l2);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_errors_on_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("missing"), &link).unwrap();
        let err = resolve(link.to_str().unwrap(), true).unwrap_err();
        assert!(err.to_string().contains("cannot resolve symlink"));
    }

    #[test]
    fn git_diff_args_order() {
        let c = cli(&["x", "y", "-U0", "--word-diff"]);
        assert_eq!(
            git_diff_args(&c, "x.log", "y.log"),
            [
                "diff",
                "--no-index",
                "--no-prefix",
                "-U0",
                "--word-diff",
                "a/x.log",
                "b/y.log"
            ]
        );
    }

    #[test]
    fn cli_parsing() {
        let c = cli(&["x", "y"]);
        assert_eq!((c.first.as_str(), c.second.as_str()), ("x", "y"));
        assert!(c.git_args.is_empty());
        assert!(!c.no_follow && !c.sort_lines && !c.dry_run);

        let c = cli(&["--sort-lines", "-n", "x", "y", "--stat", "-U3"]);
        assert!(c.sort_lines && c.dry_run);
        assert_eq!(c.git_args, ["--stat", "-U3"]);

        assert!(Cli::try_parse_from(["difflog", "only-one"]).is_err());
    }
}
