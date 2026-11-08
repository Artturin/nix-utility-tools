mod cli;
mod ui;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use diff_nixpkgs_attr_core::{
    CancelToken, Config, Control, Error, ProcessPolicy, Status, SystemdRun, run_pipeline,
};

use crate::cli::Cli;
use crate::ui::{Colors, Options, Ui};

fn main() -> ExitCode {
    let cli = Cli::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let colors = Colors::detect();
    let mut ui = Ui::new(Options {
        pager: pager_command(&cli),
        pause: cli.pause,
        colors,
    });

    let token = CancelToken::new();
    let handler_token = token.clone();
    if let Err(err) = ctrlc::set_handler(move || handler_token.cancel()) {
        log::debug!("could not install the Ctrl-C handler: {err}");
    }

    let ctl = Control::new(
        token,
        ProcessPolicy {
            memory_max: cli.memory_max,
            timeout: cli.timeout.0,
            systemd_run: if cli.no_systemd_run {
                SystemdRun::Off
            } else {
                SystemdRun::Auto
            },
        },
    );
    if cli.memory_max.is_some() && !ctl.memory_cap_enforceable() {
        ui.warn(
            "the diffoscope memory limit can't be enforced here (it needs `systemd-run --user`); \
             running without it",
        );
    }

    let output_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            ui.error(&format!("cannot determine the current directory: {err}"));
            return ExitCode::FAILURE;
        }
    };
    let mut cfg = Config::new(".", cli.attrs, output_dir);
    cfg.base = cli.base;
    cfg.system = cli.system;
    cfg.eval_workers = cli.workers;
    cfg.eval_max_memory_mib = (cli.max_eval_memory > 0).then_some(cli.max_eval_memory);
    cfg.dry_run = cli.dry_run;
    cfg.color = colors.stdout;

    let result = run_pipeline(&cfg, &ctl, &mut |event| ui.handle(event));
    match result {
        Ok(report) => {
            match report.status {
                Status::Completed => ui.summary(&report),
                Status::NothingChanged | Status::DryRun => {}
                Status::Cancelled | Status::TimedOut => {
                    if !report.entries.is_empty() {
                        ui.summary(&report);
                    }
                }
            }
            match report.status {
                Status::Cancelled => {
                    ui.warn("cancelled");
                    ExitCode::from(130)
                }
                Status::TimedOut => {
                    ui.warn("a command timed out");
                    ExitCode::from(124)
                }
                Status::Completed | Status::NothingChanged | Status::DryRun => ExitCode::SUCCESS,
            }
        }
        Err(err) => {
            ui.error(&err.to_string());
            match err {
                Error::MissingTools(_) => ExitCode::from(127),
                _ => ExitCode::FAILURE,
            }
        }
    }
}

/// The pager for diff output, if there should be one.
fn pager_command(cli: &Cli) -> Option<Vec<String>> {
    if cli.no_pager || !std::io::stdout().is_terminal() {
        return None;
    }
    let command = cli.diff_pager.as_deref().unwrap_or("less -FR");
    match shell_words::split(command) {
        Ok(words) if !words.is_empty() => Some(words),
        _ => None,
    }
}
