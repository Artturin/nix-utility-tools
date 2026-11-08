mod cli;
mod logging;

use std::os::unix::process::ExitStatusExt;
use std::process::{ExitCode, ExitStatus};

use clap::Parser;

use crate::cli::Cli;
use diffoscope_nix::{Error, Outcome, Request, run};
use nix_utility_common::CancelToken;
use nix_utility_common::notice::{Level, Notice};
use nix_utility_common::process::{Control, Out, Termination};

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging::init();

    // diffoscope runs in its own process group, so the token is what stops it;
    // the library then removes its scratch directory on the way out.
    let token = CancelToken::new();
    let handler_token = token.clone();
    if let Err(err) = ctrlc::set_handler(move || handler_token.cancel()) {
        log::debug!("could not install the Ctrl-C handler: {err}");
    }

    let req = Request {
        path1: cli.path1,
        path2: cli.path2,
        keep_build_id: cli.keep_build_id,
        dry_run: cli.dry_run,
        diffoscope_args: cli.diffoscope_args,
    };
    let ctl = Control::new(token, Default::default());
    match run(&req, &ctl, &mut print_notice, Out::Inherit, Out::Inherit) {
        Ok(Outcome::DryRun) => ExitCode::SUCCESS,
        Ok(Outcome::Ran(Termination::Exited(status))) => exit_code(status),
        Ok(Outcome::Ran(Termination::Cancelled)) => ExitCode::from(130),
        Ok(Outcome::Ran(Termination::TimedOut)) => ExitCode::from(124),
        Err(err @ Error::DiffoscopeNotFound) => {
            log::error!("{err}");
            ExitCode::from(127)
        }
        Err(err @ Error::NoSuchPath(_)) => {
            log::error!("{err}");
            ExitCode::from(2)
        }
        Err(Error::Other(err)) => {
            log::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

fn print_notice(notice: Notice) {
    match notice.level {
        // The dry-run plan is the output of that mode.
        Level::Plan => println!("{}", notice.text),
        Level::Note | Level::Info => log::info!("{}", notice.text),
        Level::Warn => log::warn!("{}", notice.text),
        Level::Error => log::error!("{}", notice.text),
    }
}

fn exit_code(status: ExitStatus) -> ExitCode {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitCode::from(code as u8),
        (None, Some(signal)) => ExitCode::from(128 + signal as u8),
        (None, None) => ExitCode::FAILURE,
    }
}
