//! The whole comparison: evaluate both sides, find changed outputs, build
//! them, diff build logs and outputs, save what differs.

use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use difflog::{NormalizeOptions, Outcome as DifflogOutcome};
use diffoscope_nix::Outcome as DiffoscopeOutcome;
use nix_utility_common::notice::Notice;
use nix_utility_common::process::{Cmd, Control, Out, Termination, run, run_captured};
use thiserror::Error;

use crate::attr::{self, AttrError};
use crate::build::InternalJson;
use crate::event::{Collected, Emitter, Event, Flow, Side, Stage, Tool, ToolStatus};
use crate::jobs::{Changed, Evaluation, changed_outputs, installables};
use crate::save::Saver;
use crate::text::{LineBuffer, nix_string, url_path};
use crate::tools::Preflight;

const TOOL: &str = "diff-nixpkgs-attr";

/// What to compare and how.
#[derive(Debug, Clone)]
pub struct Config {
    /// Any directory inside the nixpkgs checkout. Nothing changes the
    /// process's own working directory.
    pub repo: PathBuf,
    /// Attributes of `pkgs`: package sets or single packages.
    pub attrs: Vec<String>,
    /// Revision to compare against; the merge base of `master` and `HEAD`
    /// when unset.
    pub base: Option<String>,
    pub system: String,
    /// `nix-eval-jobs --workers`. Each worker is a full evaluator.
    pub eval_workers: u32,
    /// `nix-eval-jobs --max-memory-size` in MiB: a worker over it restarts.
    pub eval_max_memory_mib: Option<u64>,
    /// Evaluate and plan, but build nothing and run no diff tools.
    pub dry_run: bool,
    /// Where the `differences-…` directory goes. Always explicit.
    pub output_dir: PathBuf,
    /// Ask git for colour in difflog output (it is a pipe, not a terminal).
    pub color: bool,
}

impl Config {
    pub fn new(
        repo: impl Into<PathBuf>,
        attrs: Vec<String>,
        output_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            repo: repo.into(),
            attrs,
            base: None,
            system: "x86_64-linux".to_owned(),
            eval_workers: 2,
            eval_max_memory_mib: Some(4096),
            dry_run: false,
            output_dir: output_dir.into(),
            color: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Everything was compared.
    Completed,
    /// Nothing changed (or nothing could be compared).
    NothingChanged,
    DryRun,
    Cancelled,
    TimedOut,
}

/// Where differences were found for one attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryEntry {
    pub attr: String,
    pub difflog: bool,
    pub diffoscope_outputs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub status: Status,
    /// Attributes with differences, in the order they were found.
    pub entries: Vec<SummaryEntry>,
    /// The directory results went to, when any were saved.
    pub saved_to: Option<PathBuf>,
    /// The handler stopped the run before every package was compared.
    pub stopped_early: bool,
}

impl Report {
    fn empty(status: Status) -> Self {
        Self {
            status,
            entries: Vec::new(),
            saved_to: None,
            stopped_early: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Attr(#[from] AttrError),
    #[error("required program not found in PATH: {}", .0.join(", "))]
    MissingTools(Vec<&'static str>),
    #[error("`{command}` failed: {message}")]
    Git { command: String, message: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Why the run ended before the end.
enum Stop {
    Cancelled,
    TimedOut,
    Failed(Error),
}

impl From<Error> for Stop {
    fn from(err: Error) -> Self {
        Stop::Failed(err)
    }
}

impl From<io::Error> for Stop {
    fn from(err: io::Error) -> Self {
        Stop::Failed(Error::Other(err.into()))
    }
}

/// Turn cancellation and timeouts into early exits.
fn completed(termination: Termination) -> Result<Termination, Stop> {
    match termination {
        Termination::Cancelled => Err(Stop::Cancelled),
        Termination::TimedOut => Err(Stop::TimedOut),
        exited => Ok(exited),
    }
}

struct Cx<'a> {
    cfg: &'a Config,
    ctl: &'a Control,
    em: &'a Emitter<'a>,
    toplevel: PathBuf,
    work: &'a Path,
}

#[derive(Default)]
struct Results {
    entries: Vec<SummaryEntry>,
    stopped: bool,
}

impl Results {
    fn entry(&mut self, attr: &str) -> &mut SummaryEntry {
        let index = self.entries.iter().position(|e| e.attr == attr);
        let index = index.unwrap_or_else(|| {
            self.entries.push(SummaryEntry {
                attr: attr.to_owned(),
                difflog: false,
                diffoscope_outputs: Vec::new(),
            });
            self.entries.len() - 1
        });
        &mut self.entries[index]
    }
}

/// Run the whole comparison, reporting progress to `on_event`.
///
/// Cancelling `ctl`'s token stops the running child and returns a report
/// with [`Status::Cancelled`] and whatever was found so far.
pub fn run_pipeline(
    cfg: &Config,
    ctl: &Control,
    on_event: &mut dyn FnMut(Event) -> Flow,
) -> Result<Report, Error> {
    let em = Emitter::new(on_event);
    let attrs = attr::prepare(&cfg.attrs)?;
    attr::validate_system(&cfg.system)?;
    let missing = Preflight::check().missing(cfg.dry_run);
    if !missing.is_empty() {
        return Err(Error::MissingTools(missing));
    }

    let repo_info = match repo_info(ctl, cfg) {
        Ok(info) => info,
        Err(Stop::Cancelled) => return Ok(Report::empty(Status::Cancelled)),
        Err(Stop::TimedOut) => return Ok(Report::empty(Status::TimedOut)),
        Err(Stop::Failed(err)) => return Err(err),
    };
    let RepoInfo {
        toplevel,
        branch,
        short,
        base_rev,
    } = repo_info;

    let dir_name = format!(
        "differences-{}-{}-{short}",
        attr::sanitize(&attrs.join("+")),
        attr::sanitize(if branch.is_empty() {
            "detached"
        } else {
            &branch
        }),
    );
    let mut saver = Saver::new(cfg.output_dir.join(dir_name));
    let work = tempfile::tempdir().map_err(|e| Error::Other(e.into()))?;
    let cx = Cx {
        cfg,
        ctl,
        em: &em,
        toplevel,
        work: work.path(),
    };

    let mut results = Results::default();
    let status = match execute(&cx, &attrs, &base_rev, &mut saver, &mut results) {
        Ok(status) => status,
        Err(Stop::Cancelled) => Status::Cancelled,
        Err(Stop::TimedOut) => Status::TimedOut,
        Err(Stop::Failed(err)) => return Err(err),
    };
    Ok(Report {
        status,
        entries: results.entries,
        saved_to: saver.used().then(|| saver.dir().to_owned()),
        stopped_early: results.stopped,
    })
}

struct RepoInfo {
    toplevel: PathBuf,
    branch: String,
    short: String,
    base_rev: String,
}

fn repo_info(ctl: &Control, cfg: &Config) -> Result<RepoInfo, Stop> {
    let toplevel = PathBuf::from(git(ctl, &cfg.repo, &["rev-parse", "--show-toplevel"])?);
    let branch = git(ctl, &toplevel, &["branch", "--show-current"])?;
    let short = git(ctl, &toplevel, &["rev-parse", "--short", "HEAD"])?;
    let base = match &cfg.base {
        Some(base) => base.clone(),
        None => git(ctl, &toplevel, &["merge-base", "master", "HEAD"])?,
    };
    let base_rev = git(ctl, &toplevel, &["rev-parse", &base])?;
    Ok(RepoInfo {
        toplevel,
        branch,
        short,
        base_rev,
    })
}

fn git(ctl: &Control, dir: &Path, args: &[&str]) -> Result<String, Stop> {
    let captured = run_captured(&Cmd::new("git").args(args).current_dir(dir), ctl)
        .map_err(|e| Error::Other(anyhow!("running git: {e}")))?;
    completed(captured.termination)?;
    if captured.termination.success() {
        Ok(captured.stdout_lossy().trim().to_owned())
    } else {
        Err(Error::Git {
            command: format!("git {}", args.join(" ")),
            message: captured.stderr_lossy().trim().to_owned(),
        }
        .into())
    }
}

#[derive(Default)]
struct Plan {
    changed: Vec<Changed>,
    old_installables: BTreeSet<String>,
    new_installables: BTreeSet<String>,
}

fn execute(
    cx: &Cx<'_>,
    attrs: &[String],
    base_rev: &str,
    saver: &mut Saver,
    results: &mut Results,
) -> Result<Status, Stop> {
    let mut plan = Plan::default();
    for attr in attrs {
        collect_attr(cx, attr, base_rev, &mut plan)?;
    }

    // Overlapping attributes (a set and one of its members) yield identical
    // rows; keep the first so the order follows the command line.
    let mut seen = HashSet::new();
    let changed: Vec<Changed> = plan
        .changed
        .into_iter()
        .filter(|row| seen.insert(row.clone()))
        .collect();
    if changed.is_empty() {
        return Ok(Status::NothingChanged);
    }
    let packages = changed
        .iter()
        .map(|c| &c.attr)
        .collect::<HashSet<_>>()
        .len();
    cx.em.emit(Event::Total {
        attrs: attrs.len(),
        outputs: changed.len(),
        packages,
    });

    build_side(cx, Side::Base, &plan.old_installables)?;
    build_side(cx, Side::Head, &plan.new_installables)?;

    if cx.cfg.dry_run {
        for row in changed {
            cx.em.emit(Event::WouldDiff {
                attr: row.attr,
                output: row.output,
                old_path: row.old_path,
                new_path: row.new_path,
            });
        }
        return Ok(Status::DryRun);
    }

    show_diffs(cx, &changed, packages, saver, results)?;
    Ok(Status::Completed)
}

fn flake_ref(toplevel: &Path, rev: Option<&str>) -> String {
    let path = url_path(&toplevel.to_string_lossy());
    match rev {
        Some(rev) => format!("git+file://{path}?rev={rev}"),
        None => format!("git+file://{path}"),
    }
}

/// Evaluate `attr` of nixpkgs at `flake_ref` with aliases disabled. `attr`
/// may be a set (one job per package) or a single derivation (one job with
/// an empty attr). legacyPackages can't take a config, so the flake's source
/// is imported directly.
fn eval_side(cx: &Cx<'_>, flake_ref: &str, attr: &str, side: Side) -> Result<Evaluation, Stop> {
    let expr = format!(
        "\nlet\n  flake = builtins.getFlake \"{}\";\n  pkgs = import flake.outPath {{\n    system = \"{}\";\n    config.allowAliases = false;\n  }};\nin pkgs.{attr}\n",
        nix_string(flake_ref),
        cx.cfg.system,
    );
    let mut cmd = if which::which("nix-eval-jobs").is_ok() {
        Cmd::new("nix-eval-jobs")
    } else {
        Cmd::new("nix").args(["run", "nixpkgs#nix-eval-jobs", "--"])
    };
    cmd = cmd
        .args(["--impure", "--force-recurse", "--workers"])
        .arg(cx.cfg.eval_workers.max(1).to_string());
    if let Some(mib) = cx.cfg.eval_max_memory_mib {
        cmd = cmd.arg("--max-memory-size").arg(mib.to_string());
    }
    let cmd = cmd.arg("--expr").arg(expr).current_dir(&cx.toplevel);

    let mut evaluation = Evaluation::default();
    let (mut out_lines, mut err_lines) = (LineBuffer::default(), LineBuffer::default());
    let on_job = |line: String, evaluation: &mut Evaluation| {
        evaluation.push_line(&line);
        if evaluation.jobs.len().is_multiple_of(100) && !line.trim().is_empty() {
            cx.em.emit(Event::EvalProgress {
                attr: attr.to_owned(),
                side,
                jobs: evaluation.jobs.len(),
            });
        }
    };
    let on_stderr = |line: String| {
        cx.em.emit(Event::ToolLine {
            tool: "nix-eval-jobs",
            line,
        })
    };
    let termination = run(
        &cmd,
        cx.ctl,
        Out::Sink(&mut |chunk| out_lines.push(chunk, |l| on_job(l, &mut evaluation))),
        Out::Sink(&mut |chunk| err_lines.push(chunk, on_stderr)),
    )?;
    out_lines.finish(|l| on_job(l, &mut evaluation));
    err_lines.finish(on_stderr);

    // A failed evaluation (e.g. the attribute doesn't exist on the base
    // revision) must not abort the run: it is reported and left to the
    // job counts.
    if !completed(termination)?.success() {
        cx.em.notice(Notice::warn(
            TOOL,
            format!("evaluating {attr} on the {} side failed", side.label()),
        ));
    }
    Ok(evaluation)
}

fn show_errors(cx: &Cx<'_>, evaluation: &Evaluation) {
    for error in evaluation.first_errors(3) {
        cx.em.notice(Notice::note(TOOL, error));
    }
}

/// Evaluate `attr` on both sides and add what changed to the plan.
fn collect_attr(cx: &Cx<'_>, attr: &str, base_rev: &str, plan: &mut Plan) -> Result<(), Stop> {
    cx.em.emit(Event::Stage(Stage::Evaluating {
        attr: attr.to_owned(),
        base_rev: base_rev.to_owned(),
    }));
    let old = eval_side(
        cx,
        &flake_ref(&cx.toplevel, Some(base_rev)),
        attr,
        Side::Base,
    )?;
    let new = eval_side(cx, &flake_ref(&cx.toplevel, None), attr, Side::Head)?;

    for (name, evaluation) in [("old", &old), ("new", &new)] {
        let errors = evaluation.error_count();
        if errors > 0 {
            cx.em.notice(Notice::note(
                TOOL,
                format!("{errors} eval errors on the {name} side (ignored)"),
            ));
        }
        if evaluation.unparsable_lines > 0 {
            cx.em.notice(Notice::warn(
                TOOL,
                format!(
                    "ignored {} unreadable lines of evaluation output on the {name} side",
                    evaluation.unparsable_lines
                ),
            ));
        }
    }

    let nothing = |side, text: &str, evaluation: &Evaluation| {
        cx.em.notice(Notice::warn(
            TOOL,
            format!("{attr} did not evaluate on {text}; nothing to compare"),
        ));
        show_errors(cx, evaluation);
        cx.em.emit(Event::Collected {
            attr: attr.to_owned(),
            result: Collected::NotEvaluated(side),
        });
    };
    if new.job_count() == 0 {
        nothing(Side::Head, "the working tree", &new);
        return Ok(());
    }
    if old.job_count() == 0 {
        nothing(Side::Base, "the base revision (new attribute?)", &old);
        return Ok(());
    }

    let changed = changed_outputs(&old.jobs, &new.jobs, attr);
    if changed.is_empty() {
        cx.em.emit(Event::Collected {
            attr: attr.to_owned(),
            result: Collected::NothingChanged,
        });
        return Ok(());
    }
    let wanted: HashSet<&str> = changed.iter().map(|c| c.attr.as_str()).collect();
    cx.em.emit(Event::Collected {
        attr: attr.to_owned(),
        result: Collected::Changed {
            outputs: changed.len(),
            packages: wanted.len(),
        },
    });
    plan.old_installables
        .extend(installables(&old.jobs, &wanted, attr));
    plan.new_installables
        .extend(installables(&new.jobs, &wanted, attr));
    plan.changed.extend(changed);
    Ok(())
}

/// One `nix build` for everything on one side.
fn build_side(cx: &Cx<'_>, side: Side, installables: &BTreeSet<String>) -> Result<(), Stop> {
    // With nothing on stdin, `nix build` would build the default installable.
    if installables.is_empty() {
        return Ok(());
    }
    cx.em.emit(Event::Stage(Stage::Building {
        side,
        derivations: installables.len(),
    }));
    let out_link = cx.work.join(match side {
        Side::Base => "old",
        Side::Head => "new",
    });
    let mut cmd = Cmd::new("nix")
        .args([
            "build",
            "--stdin",
            "--keep-going",
            "--log-format",
            "internal-json",
            "--out-link",
        ])
        .arg(out_link)
        .current_dir(&cx.toplevel)
        .stdin_data(
            installables
                .iter()
                .map(|i| format!("{i}\n"))
                .collect::<String>(),
        );
    if cx.cfg.dry_run {
        cmd = cmd.arg("--dry-run");
    }

    let mut parser = InternalJson::default();
    let mut lines = LineBuffer::default();
    let mut on_line = |line: String| {
        if let Some(event) = parser.feed(&line) {
            cx.em.emit(Event::Build { side, event });
        }
    };
    let termination = run(
        &cmd,
        cx.ctl,
        Out::Null,
        Out::Sink(&mut |chunk| lines.push(chunk, &mut on_line)),
    )
    .map_err(|e| Error::Other(anyhow!("running nix build: {e}")))?;
    lines.finish(&mut on_line);

    if !completed(termination)?.success() {
        cx.em.notice(Notice::warn(
            TOOL,
            format!(
                "some {} builds failed; affected outputs will be skipped",
                match side {
                    Side::Base => "old",
                    Side::Head => "new",
                }
            ),
        ));
    }
    Ok(())
}

fn show_diffs(
    cx: &Cx<'_>,
    changed: &[Changed],
    packages: usize,
    saver: &mut Saver,
    results: &mut Results,
) -> Result<(), Stop> {
    let mut previous: Option<&str> = None;
    let mut index = 0;

    // All outputs of a derivation share one build log, so difflog runs once
    // per attribute (rows are grouped by attribute).
    for row in changed {
        let attr = row.attr.as_str();
        if previous != Some(attr) {
            cx.em.emit(Event::PackageStarting {
                attr: attr.to_owned(),
                index,
                total: packages,
            });
            if cx.em.stopped() {
                results.stopped = true;
                break;
            }
            index += 1;
            previous = Some(attr);
            if let Err(err) = saver.reset(attr) {
                cx.em.notice(Notice::warn(
                    TOOL,
                    format!("clearing old results of {attr}: {err}"),
                ));
            }
            run_difflog(cx, row, saver, results)?;
        }

        if !Path::new(&row.old_path).exists() || !Path::new(&row.new_path).exists() {
            cx.em.emit(Event::Skipped {
                attr: row.attr.clone(),
                output: row.output.clone(),
                old_path: row.old_path.clone(),
                new_path: row.new_path.clone(),
            });
            continue;
        }
        run_diffoscope(cx, row, saver, results)?;
        if cx.em.stopped() {
            results.stopped = true;
            break;
        }
    }
    Ok(())
}

fn difflog_request(row: &Changed, color: &str) -> difflog::Request {
    let mut req = difflog::Request::new(&row.old_path, &row.new_path);
    req.normalize = NormalizeOptions {
        sort_lines: true,
        ..NormalizeOptions::default()
    };
    req.git_args = vec!["-U0".to_owned(), color.to_owned()];
    req
}

fn run_difflog(
    cx: &Cx<'_>,
    row: &Changed,
    saver: &mut Saver,
    results: &mut Results,
) -> Result<(), Stop> {
    let attr = row.attr.as_str();
    let finish = |status| {
        cx.em.emit(Event::ToolFinished {
            attr: attr.to_owned(),
            tool: Tool::Difflog,
            output: None,
            status,
        })
    };
    cx.em.emit(Event::ToolStarted {
        attr: attr.to_owned(),
        tool: Tool::Difflog,
        output: None,
    });

    let color = if cx.cfg.color {
        "--color=always"
    } else {
        "--no-color"
    };
    let mut shown = Vec::new();
    let outcome = difflog::diff_logs(
        &difflog_request(row, color),
        cx.ctl,
        &mut |n| cx.em.notice(n),
        Out::Sink(&mut |chunk| {
            shown.extend_from_slice(chunk);
            cx.em.emit(Event::Output {
                tool: Tool::Difflog,
                data: chunk.to_vec(),
            });
        }),
    );
    match outcome {
        Ok(DifflogOutcome::Cancelled) => Err(Stop::Cancelled),
        Ok(DifflogOutcome::TimedOut) => Err(Stop::TimedOut),
        Ok(DifflogOutcome::Different) => {
            results.entry(attr).difflog = true;
            finish(ToolStatus::Different);
            // The saved copy never has colour.
            let mut plain = Vec::new();
            let saved = if cx.cfg.color {
                match difflog::diff_logs(
                    &difflog_request(row, "--no-color"),
                    cx.ctl,
                    &mut |_| {},
                    Out::Sink(&mut |chunk| plain.extend_from_slice(chunk)),
                ) {
                    Ok(_) => &plain,
                    Err(_) => &shown,
                }
            } else {
                &shown
            };
            let result = saver.difflog(attr, saved);
            save(cx, saver, attr, result);
            Ok(())
        }
        Ok(DifflogOutcome::Same | DifflogOutcome::DryRun) => {
            finish(ToolStatus::Same);
            Ok(())
        }
        Err(err) => {
            cx.em.notice(Notice::warn(
                TOOL,
                format!("difflog failed for {attr}: {err:#}"),
            ));
            finish(ToolStatus::Failed(None));
            Ok(())
        }
    }
}

/// Report a saving problem and carry on.
fn save(cx: &Cx<'_>, saver: &Saver, attr: &str, result: io::Result<()>) {
    match result {
        Ok(()) => {}
        Err(err) => cx.em.notice(Notice::warn(
            TOOL,
            format!(
                "saving results for {attr} under {}: {err}",
                saver.dir().display()
            ),
        )),
    }
}

fn run_diffoscope(
    cx: &Cx<'_>,
    row: &Changed,
    saver: &mut Saver,
    results: &mut Results,
) -> Result<(), Stop> {
    let attr = row.attr.as_str();
    let finish = |status| {
        cx.em.emit(Event::ToolFinished {
            attr: attr.to_owned(),
            tool: Tool::Diffoscope,
            output: Some(row.output.clone()),
            status,
        })
    };
    cx.em.emit(Event::ToolStarted {
        attr: attr.to_owned(),
        tool: Tool::Diffoscope,
        output: Some(row.output.clone()),
    });

    // The whole output is kept on disk for saving, whatever the frontend
    // does with the events.
    let raw_path = cx.work.join("diffoscope.raw");
    let mut raw = BufWriter::new(File::create(&raw_path)?);
    let mut raw_error: Option<io::Error> = None;
    let mut stderr_lines = LineBuffer::default();
    let on_stderr = |line: String| {
        cx.em.emit(Event::ToolLine {
            tool: "diffoscope",
            line,
        })
    };

    let outcome = diffoscope_nix::run(
        &diffoscope_nix::Request::new(&row.old_path, &row.new_path),
        cx.ctl,
        &mut |n| cx.em.notice(n),
        Out::Sink(&mut |chunk| {
            if raw_error.is_none() {
                raw_error = raw.write_all(chunk).err();
            }
            cx.em.emit(Event::Output {
                tool: Tool::Diffoscope,
                data: chunk.to_vec(),
            });
        }),
        Out::Sink(&mut |chunk| stderr_lines.push(chunk, on_stderr)),
    );
    stderr_lines.finish(on_stderr);
    let flushed = raw.flush();

    let termination = match outcome {
        Ok(DiffoscopeOutcome::Ran(termination)) => completed(termination)?,
        Ok(DiffoscopeOutcome::DryRun) => return Ok(()),
        Err(err) => {
            cx.em.notice(Notice::warn(
                TOOL,
                format!("diffoscope-nix failed for {attr} [{}]: {err}", row.output),
            ));
            finish(ToolStatus::Failed(None));
            return Ok(());
        }
    };
    match termination.code() {
        Some(0) => finish(ToolStatus::Same),
        Some(1) => {
            results
                .entry(attr)
                .diffoscope_outputs
                .push(row.output.clone());
            finish(ToolStatus::Different);
            let result = match (raw_error, flushed) {
                (Some(err), _) | (None, Err(err)) => Err(err),
                (None, Ok(())) => saver.diffoscope(attr, &row.output, &raw_path),
            };
            save(cx, saver, attr, result);
        }
        _ if termination.killed_by_memory_limit(cx.ctl) => finish(ToolStatus::MemoryLimit),
        code => finish(ToolStatus::Failed(code)),
    }
    Ok(())
}
