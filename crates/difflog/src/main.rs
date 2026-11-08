mod cli;

use std::io::Write;
use std::process::ExitCode;

use anstyle::{Ansi256Color, AnsiColor, Color, Style};
use clap::Parser;
use log::{LevelFilter, error, info, warn};

use cli::Cli;
use difflog::{NormalizeOptions, Outcome, Request, diff_logs};
use nix_utility_common::CancelToken;
use nix_utility_common::notice::{Level, Notice};
use nix_utility_common::process::{Control, Out, ProcessPolicy};

/// Log target for the grey informational notes.
const NOTE: &str = "difflog::note";

const GREY: Style = Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(245))));
const GREEN: Style = AnsiColor::Green.on_default();
const RED: Style = AnsiColor::Red.on_default();

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

/// Print a notice the way the log filter (-q/-v) expects it.
fn print_notice(notice: Notice) {
    match notice.level {
        Level::Note | Level::Plan => log::info!(target: NOTE, "{}", notice.text),
        Level::Info => info!("{}", notice.text),
        Level::Warn => warn!("{}", notice.text),
        Level::Error => error!("{}", notice.text),
    }
}

fn request(cli: &Cli) -> Request {
    Request {
        first: cli.first.clone(),
        second: cli.second.clone(),
        git_args: cli.git_args.clone(),
        follow_symlinks: !cli.no_follow,
        normalize: NormalizeOptions {
            strip_timings: !cli.keep_timings,
            strip_paths: !cli.keep_paths,
            strip_jobs: !cli.keep_jobs,
            strip_kernel_version: !cli.keep_kernel_version,
            strip_progress: !cli.keep_progress,
            strip_pids: !cli.keep_pids,
            sort_lines: cli.sort_lines,
        },
        dry_run: cli.dry_run,
    }
}

fn run(cli: &Cli, token: &CancelToken) -> anyhow::Result<ExitCode> {
    let ctl = Control::new(token.clone(), ProcessPolicy::default());
    let outcome = diff_logs(&request(cli), &ctl, &mut print_notice, Out::Inherit)?;
    Ok(match outcome {
        Outcome::Same => {
            info!("No changes detected (Exit 0)");
            ExitCode::SUCCESS
        }
        Outcome::Different => {
            warn!("Changes detected! (Exit 1)");
            ExitCode::from(1)
        }
        Outcome::DryRun => ExitCode::SUCCESS,
        Outcome::Cancelled | Outcome::TimedOut => ExitCode::from(130),
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose.log_level_filter());

    // git runs in the foreground group, so Ctrl-C reaches it directly; the
    // token stops the steps around it (`nix log` runs in its own group).
    let token = CancelToken::new();
    let handler_token = token.clone();
    if let Err(err) = ctrlc::set_handler(move || handler_token.cancel()) {
        log::debug!("could not install the Ctrl-C handler: {err}");
    }

    match run(&cli, &token) {
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
    fn git_options_after_the_two_logs_are_collected() {
        let c = cli(&["--sort-lines", "-n", "x", "y", "--stat", "-U3"]);
        assert!(c.sort_lines && c.dry_run);
        assert_eq!((c.first.as_str(), c.second.as_str()), ("x", "y"));
        assert_eq!(c.git_args, ["--stat", "-U3"]);
    }

    #[test]
    fn keep_flags_map_to_normalizer_options() {
        let req = request(&cli(&["x", "y", "--keep-jobs", "--no-follow"]));
        assert!(!req.normalize.strip_jobs && req.normalize.strip_timings);
        assert!(!req.follow_symlinks);
    }
}
