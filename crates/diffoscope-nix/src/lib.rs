//! Run diffoscope on two Nix store paths after masking differences that are
//! only due to differing store path hashes and GNU build IDs.
//!
//! The library reports through values: remarks go to a [`NoticeSink`],
//! diffoscope's output to [`Out`]s, and the result is an [`Outcome`]. The
//! `diffoscope-nix` binary renders all of that for a terminal.

mod elf;
mod normalize;
mod rewrite;
mod scratch;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, anyhow};
use regex::Regex;

use nix_utility_common::notice::{Notice, NoticeSink};
use nix_utility_common::process::{Cmd, Control, Out, Termination, run as run_process};

use crate::normalize::{Options, normalize};
use crate::scratch::Scratch;

const TOOL: &str = "diffoscope-nix";

/// What to compare and how.
#[derive(Debug, Clone)]
pub struct Request {
    pub path1: PathBuf,
    pub path2: PathBuf,
    /// Don't zero the GNU build ID of ELF files.
    pub keep_build_id: bool,
    /// Only describe what would be done.
    pub dry_run: bool,
    /// Passed to diffoscope unchanged, after `--text-color always`.
    pub diffoscope_args: Vec<OsString>,
}

impl Request {
    pub fn new(path1: impl Into<PathBuf>, path2: impl Into<PathBuf>) -> Self {
        Self {
            path1: path1.into(),
            path2: path2.into(),
            keep_build_id: false,
            dry_run: false,
            diffoscope_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `dry_run` was set and nothing was run.
    DryRun,
    /// diffoscope ran (or was stopped); its exit status 0 means no
    /// differences and 1 means differences.
    Ran(Termination),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("diffoscope not found in PATH")]
    DiffoscopeNotFound,
    #[error("no such path: {}", .0.display())]
    NoSuchPath(PathBuf),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Why a step stopped the run early.
enum Stop {
    Interrupted(Termination),
    Failed(Error),
}

impl<E: Into<anyhow::Error>> From<E> for Stop {
    fn from(err: E) -> Self {
        Stop::Failed(Error::Other(err.into()))
    }
}

/// Run diffoscope on the two paths.
///
/// Its standard output and error go to `stdout` and `stderr`. The output is
/// coloured (`--text-color always`) even when it is not a terminal.
pub fn run(
    req: &Request,
    ctl: &Control,
    notices: NoticeSink<'_>,
    stdout: Out<'_>,
    stderr: Out<'_>,
) -> Result<Outcome, Error> {
    match run_inner(req, ctl, notices, stdout, stderr) {
        Ok(outcome) => Ok(outcome),
        Err(Stop::Interrupted(termination)) => Ok(Outcome::Ran(termination)),
        Err(Stop::Failed(err)) => Err(err),
    }
}

fn run_inner(
    req: &Request,
    ctl: &Control,
    notices: NoticeSink<'_>,
    stdout: Out<'_>,
    stderr: Out<'_>,
) -> Result<Outcome, Stop> {
    let diffoscope =
        which::which("diffoscope").map_err(|_| Stop::Failed(Error::DiffoscopeNotFound))?;
    for path in [&req.path1, &req.path2] {
        if !path.exists() {
            return Err(Stop::Failed(Error::NoSuchPath(path.clone())));
        }
    }

    let opts = Options {
        strip_build_id: !req.keep_build_id,
        list_only: req.dry_run,
    };
    let name_a = store_name(&req.path1)?;
    let name_b = store_name(&req.path2)?;

    if req.dry_run {
        dry_run(req, &name_a, &name_b, opts, ctl, notices)?;
        return Ok(Outcome::DryRun);
    }

    let scratch = Scratch::new()?;
    let a = copy_into(&scratch.path().join("a"), &name_a, &req.path1, ctl)?;
    let b = copy_into(&scratch.path().join("b"), &name_b, &req.path2, ctl)?;
    for copy in [&a, &b] {
        if !normalize(copy, opts, &ctl.token, &mut |_| {})? {
            return Err(Stop::Interrupted(Termination::Cancelled));
        }
    }

    // Emitted just before the diff, so it comes after any earlier output and
    // is only there for the non-default case (build IDs kept).
    if req.keep_build_id {
        notices(Notice::warn(TOOL, BUILD_ID_BANNER));
    }
    // --text-color always keeps colors even when piped (e.g. into a pager);
    // it comes first so a --text-color given after the paths overrides it.
    let cmd = Cmd::new(diffoscope)
        .args(["--text-color", "always"])
        .args(&req.diffoscope_args)
        .arg(&a)
        .arg(&b)
        .memory_limited();
    let termination = run_process(&cmd, ctl, stdout, stderr).context("running diffoscope")?;
    // `scratch` is dropped (and removed) on every path out of here.
    Ok(Outcome::Ran(termination))
}

const BUILD_ID_BANNER: &str = "GNU build IDs are KEPT (build ID differences will show)";

fn dry_run(
    req: &Request,
    name_a: &str,
    name_b: &str,
    opts: Options,
    ctl: &Control,
    notices: NoticeSink<'_>,
) -> Result<(), Stop> {
    let mut plan = |text: String| notices(Notice::plan(TOOL, text));
    let (a, b) = (format!("<tmp>/a/{name_a}"), format!("<tmp>/b/{name_b}"));
    plan("[dry-run] would create a temp dir and copy:".to_owned());
    plan(format!("[dry-run]   {} -> {a}", req.path1.display()));
    plan(format!("[dry-run]   {} -> {b}", req.path2.display()));
    let what = if opts.strip_build_id {
        "store hashes and GNU build IDs"
    } else {
        "store hashes"
    };
    for path in [&req.path1, &req.path2] {
        plan(format!(
            "[dry-run] would rewrite {what} in (from {}):",
            path.display()
        ));
        if !normalize(path, opts, &ctl.token, &mut plan)? {
            return Err(Stop::Interrupted(Termination::Cancelled));
        }
    }
    let extra: String = req
        .diffoscope_args
        .iter()
        .map(|arg| format!("{} ", arg.to_string_lossy()))
        .collect();
    plan(format!(
        "[dry-run] would run: diffoscope --text-color always {extra}{a} {b}"
    ));
    if req.keep_build_id {
        plan("[dry-run] would print this banner first:".to_owned());
        notices(Notice::warn(TOOL, BUILD_ID_BANNER));
    }
    Ok(())
}

/// /nix/store/<hash>-name -> name, so both copies get identical paths
fn store_name(path: &Path) -> anyhow::Result<String> {
    static HASH_PREFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("^[0-9a-df-np-sv-z]{32}-").expect("valid regex"));

    let name = match path.file_name() {
        Some(name) => name.to_owned(),
        None => path
            .canonicalize()?
            .file_name()
            .map_or_else(|| OsString::from("root"), ToOwned::to_owned),
    };
    let name = name.to_string_lossy();
    Ok(HASH_PREFIX.replace(&name, "").into_owned())
}

/// `cp -a` keeps modes, mtimes, ownership, hard links and extended attributes;
/// -H follows a symlink given on the command line (e.g. ./result).
fn copy_into(dir: &Path, name: &str, src: &Path, ctl: &Control) -> Result<PathBuf, Stop> {
    std::fs::create_dir(dir)?;
    let dest = dir.join(name);
    let cp = Cmd::new("cp").arg("-aH").arg("--").arg(src).arg(&dest);
    let termination = run_process(&cp, ctl, Out::Null, Out::Inherit).context("running cp")?;
    match termination {
        Termination::Cancelled | Termination::TimedOut => Err(Stop::Interrupted(termination)),
        Termination::Exited(status) if !status.success() => {
            Err(anyhow!("copying {} failed: {status}", src.display()).into())
        }
        Termination::Exited(_) => Ok(dest),
    }
}
