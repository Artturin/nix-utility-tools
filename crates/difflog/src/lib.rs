//! Compare two Nix build logs after normalizing out boring differences.
//!
//! The library does the work and reports through values: user-facing remarks
//! go to a [`NoticeSink`], the diff itself to an [`Out`], and the result is an
//! [`Outcome`]. The `difflog` binary renders all of that for a terminal.

pub mod normalize;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow};
use regex::Regex;

use nix_utility_common::notice::{Notice, NoticeSink};
use nix_utility_common::process::{Cmd, Control, Out, Termination, run};
use nix_utility_common::store::hash_pattern;

pub use normalize::{Normalizer, Options as NormalizeOptions};

const TOOL: &str = "difflog";

/// The `HASH-` prefix of a store path name.
static STORE_HASH_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^{}-", hash_pattern())).unwrap());
static UNSAFE_FILENAME_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9._+-]").unwrap());

/// What to compare and how.
#[derive(Debug, Clone)]
pub struct Request {
    /// A store path of an output or derivation (anything `nix log` accepts).
    pub first: String,
    pub second: String,
    /// Extra options for `git diff --no-index`.
    pub git_args: Vec<String>,
    /// Resolve symlinks (`./result`) before handing the arguments to `nix log`.
    pub follow_symlinks: bool,
    pub normalize: NormalizeOptions,
    /// Only describe what would be done.
    pub dry_run: bool,
}

impl Request {
    pub fn new(first: impl Into<String>, second: impl Into<String>) -> Self {
        Self {
            first: first.into(),
            second: second.into(),
            git_args: Vec::new(),
            follow_symlinks: true,
            normalize: NormalizeOptions::default(),
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The normalized logs are identical.
    Same,
    /// They differ; the diff went to the output.
    Different,
    /// `dry_run` was set and nothing was run.
    DryRun,
    Cancelled,
    TimedOut,
}

/// Why a step stopped the comparison early.
enum Stop {
    Interrupted(Outcome),
    Failed(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Stop {
    fn from(err: E) -> Self {
        Stop::Failed(err.into())
    }
}

fn interrupted(termination: Termination) -> Option<Stop> {
    match termination {
        Termination::Cancelled => Some(Stop::Interrupted(Outcome::Cancelled)),
        Termination::TimedOut => Some(Stop::Interrupted(Outcome::TimedOut)),
        Termination::Exited(_) => None,
    }
}

/// Compare the logs of `req.first` and `req.second`.
///
/// The diff (as `git diff --no-index` prints it) is written to `diff_out`.
/// With [`Out::Inherit`] git writes to the terminal itself and may use a pager.
pub fn diff_logs(
    req: &Request,
    ctl: &Control,
    notices: NoticeSink<'_>,
    diff_out: Out<'_>,
) -> Result<Outcome> {
    match compare(req, ctl, notices, diff_out) {
        Ok(outcome) => Ok(outcome),
        Err(Stop::Interrupted(outcome)) => Ok(outcome),
        Err(Stop::Failed(err)) => Err(err),
    }
}

fn compare(
    req: &Request,
    ctl: &Control,
    notices: NoticeSink<'_>,
    diff_out: Out<'_>,
) -> Result<Outcome, Stop> {
    let a = resolve(&req.first, req.follow_symlinks, notices)?;
    let b = resolve(&req.second, req.follow_symlinks, notices)?;
    let name_a = format!("{}.log", label(&a));
    let name_b = format!("{}.log", label(&b));
    let normalizer = Normalizer::new(&req.normalize)?;

    if req.normalize.sort_lines {
        notices(Notice::note(
            TOOL,
            "lines are SORTED before diffing (--sort-lines); ordering differences are hidden",
        ));
    }
    if req.dry_run {
        dry_run(req, &a, &b, &name_a, &name_b, notices);
        return Ok(Outcome::DryRun);
    }

    // Removed automatically when dropped, on success, error and cancellation.
    let tmp = tempfile::tempdir().context("failed to create temp dir")?;
    for side in ["a", "b"] {
        fs::create_dir(tmp.path().join(side))?;
    }
    write_log(&normalizer, &a, &tmp.path().join("a").join(&name_a), ctl)?;
    write_log(&normalizer, &b, &tmp.path().join("b").join(&name_b), ctl)?;

    let mut git = Cmd::new("git")
        .args(git_diff_args(&req.git_args, &name_a, &name_b))
        .current_dir(tmp.path());
    let to_terminal = matches!(diff_out, Out::Inherit);
    if to_terminal {
        git = git.foreground();
    }
    let termination = run(&git, ctl, diff_out, Out::Inherit).context("failed to run `git diff`")?;
    if let Some(stop) = interrupted(termination) {
        return Err(stop);
    }
    // Ctrl-C reaches a foreground git directly, so it may end by signal first.
    if ctl.token.is_cancelled() {
        return Err(Stop::Interrupted(Outcome::Cancelled));
    }
    match termination.code() {
        Some(0) => Ok(Outcome::Same),
        Some(1) => Ok(Outcome::Different),
        // Quitting git's pager before the end kills git with SIGPIPE. The
        // pager only fails to take everything when there was a diff to show.
        _ if to_terminal && termination.broken_pipe() => Ok(Outcome::Different),
        _ => Err(anyhow!("`git diff` failed ({})", describe(termination)).into()),
    }
}

fn describe(termination: Termination) -> String {
    match termination {
        Termination::Exited(status) => status.to_string(),
        Termination::Cancelled => "cancelled".to_owned(),
        Termination::TimedOut => "timed out".to_owned(),
    }
}

/// If `arg` is a symlink (and following is enabled), return its final target;
/// otherwise return `arg` unchanged.
pub fn resolve(arg: &str, follow: bool, notices: NoticeSink<'_>) -> Result<String> {
    let is_symlink = fs::symlink_metadata(arg).is_ok_and(|m| m.file_type().is_symlink());
    if !(follow && is_symlink) {
        return Ok(arg.to_owned());
    }
    let target = fs::canonicalize(arg)
        .with_context(|| format!("cannot resolve symlink: {arg}"))?
        .into_os_string()
        .into_string()
        .map_err(|p| anyhow!("symlink target is not valid UTF-8: {p:?}"))?;
    notices(Notice::note(TOOL, format!("{arg} -> {target}")));
    Ok(target)
}

/// A readable file name for the diff header: the resolved store path without
/// its hash, made safe for use as a file name.
pub fn label(arg: &str) -> String {
    let path = fs::canonicalize(arg).unwrap_or_else(|_| PathBuf::from(arg));
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = STORE_HASH_PREFIX.replace(&base, "");
    UNSAFE_FILENAME_CHARS.replace_all(&base, "_").into_owned()
}

/// Fetch the log with `nix log`, normalize it and write it to `dest`.
fn write_log(
    normalizer: &Normalizer,
    installable: &str,
    dest: &Path,
    ctl: &Control,
) -> Result<(), Stop> {
    let nix_log = Cmd::new("nix").arg("log").arg(installable);
    let mut log = Vec::new();
    let termination = run(
        &nix_log,
        ctl,
        Out::Sink(&mut |chunk| log.extend_from_slice(chunk)),
        Out::Inherit,
    )
    .context("failed to run `nix log`")?;
    if let Some(stop) = interrupted(termination) {
        return Err(stop);
    }
    if !termination.success() {
        return Err(anyhow!("`nix log {installable}` failed ({})", describe(termination)).into());
    }
    fs::write(dest, normalizer.normalize(&log))
        .with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(())
}

/// Arguments for the final `git diff`; shared with the dry run so they cannot drift.
pub fn git_diff_args(git_args: &[String], name_a: &str, name_b: &str) -> Vec<String> {
    ["diff", "--no-index", "--no-prefix"]
        .into_iter()
        .map(str::to_owned)
        .chain(git_args.iter().cloned())
        .chain([format!("a/{name_a}"), format!("b/{name_b}")])
        .collect()
}

fn dry_run(req: &Request, a: &str, b: &str, name_a: &str, name_b: &str, notices: NoticeSink<'_>) {
    let mut note = |text: String| notices(Notice::plan(TOOL, text));
    note("dry-run: nothing is executed and no files are created".to_owned());
    for (side, installable, name) in [("a", a, name_a), ("b", b, name_b)] {
        note(format!(
            "dry-run: would run: nix log {} | normalize > <tmpdir>/{side}/{name}",
            shell_words::quote(installable)
        ));
    }
    note(format!(
        "dry-run: would run in <tmpdir>: git {}",
        shell_words::join(git_diff_args(&req.git_args, name_a, name_b))
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(arg: &str, follow: bool) -> Result<String> {
        super::resolve(arg, follow, &mut |_| {})
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
    fn resolve_reports_the_link_it_followed() {
        let dir = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(dir.path()).unwrap().join("real");
        fs::write(&real, "").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut seen = Vec::new();
        super::resolve(link.to_str().unwrap(), true, &mut |n| seen.push(n)).unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].text.contains("link ->"), "{:?}", seen[0]);
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
        let git_args = ["-U0".to_owned(), "--word-diff".to_owned()];
        assert_eq!(
            git_diff_args(&git_args, "x.log", "y.log"),
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
}
