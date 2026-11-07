mod cli;
mod elf;
mod logging;
mod normalize;
mod rewrite;
mod scratch;

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use clap::Parser;
use regex::Regex;

use crate::cli::Cli;
use crate::normalize::{Options, normalize};
use crate::scratch::Scratch;

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging::init();

    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            log::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode> {
    let Ok(diffoscope) = which::which("diffoscope") else {
        log::error!("diffoscope not found in PATH");
        return Ok(ExitCode::from(127));
    };
    for path in [&cli.path1, &cli.path2] {
        if !path.exists() {
            log::error!("no such path: {}", path.display());
            return Ok(ExitCode::from(2));
        }
    }

    let opts = Options {
        strip_build_id: !cli.keep_build_id,
        list_only: cli.dry_run,
    };
    let name_a = store_name(&cli.path1)?;
    let name_b = store_name(&cli.path2)?;

    if cli.dry_run {
        dry_run(cli, &name_a, &name_b, opts)?;
        return Ok(ExitCode::SUCCESS);
    }

    let scratch = Scratch::new()?;
    let a = copy_into(&scratch.path().join("a"), &name_a, &cli.path1)?;
    let b = copy_into(&scratch.path().join("b"), &name_b, &cli.path2)?;
    normalize(&a, opts)?;
    normalize(&b, opts)?;

    // Logged to stderr just before the diff, so it doesn't end up in
    // diffoscope's own output (e.g. when that is piped, or --json/--html -).
    // Only for the non-default case (build IDs kept).
    if cli.keep_build_id {
        log::warn!("GNU build IDs are KEPT (build ID differences will show)");
    }
    // --text-color always keeps colors even when piped (e.g. into a pager);
    // it comes first so a --text-color given after the paths overrides it.
    let status = Command::new(diffoscope)
        .args(["--text-color", "always"])
        .args(&cli.diffoscope_args)
        .arg(&a)
        .arg(&b)
        .status()
        .context("running diffoscope")?;
    Ok(exit_code(status))
}

fn dry_run(cli: &Cli, name_a: &str, name_b: &str, opts: Options) -> Result<()> {
    let (a, b) = (format!("<tmp>/a/{name_a}"), format!("<tmp>/b/{name_b}"));
    println!("[dry-run] would create a temp dir and copy:");
    println!("[dry-run]   {} -> {a}", cli.path1.display());
    println!("[dry-run]   {} -> {b}", cli.path2.display());
    let what = if opts.strip_build_id {
        "store hashes and GNU build IDs"
    } else {
        "store hashes"
    };
    for path in [&cli.path1, &cli.path2] {
        println!(
            "[dry-run] would rewrite {what} in (from {}):",
            path.display()
        );
        normalize(path, opts)?;
    }
    let extra: String = cli
        .diffoscope_args
        .iter()
        .map(|arg| format!("{} ", arg.to_string_lossy()))
        .collect();
    println!("[dry-run] would run: diffoscope --text-color always {extra}{a} {b}");
    if cli.keep_build_id {
        println!("[dry-run] would print this banner first:");
        log::warn!("GNU build IDs are KEPT (build ID differences will show)");
    }
    Ok(())
}

/// /nix/store/<hash>-name -> name, so both copies get identical paths
fn store_name(path: &Path) -> Result<String> {
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
fn copy_into(dir: &Path, name: &str, src: &Path) -> Result<PathBuf> {
    std::fs::create_dir(dir)?;
    let dest = dir.join(name);
    let status = Command::new("cp")
        .arg("-aH")
        .arg("--")
        .arg(src)
        .arg(&dest)
        .status()
        .context("running cp")?;
    if !status.success() {
        bail!("copying {} failed: {status}", src.display());
    }
    Ok(dest)
}

fn exit_code(status: ExitStatus) -> ExitCode {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitCode::from(code as u8),
        (None, Some(signal)) => ExitCode::from(128 + signal as u8),
        (None, None) => ExitCode::FAILURE,
    }
}
