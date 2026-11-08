mod cli;
mod logging;
mod outcome;
mod run;

use std::process::ExitCode;

use clap::Parser;
use nix_log_diff_analyzer::CancelToken;

use crate::outcome::Outcome;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    logging::init();

    let cancel = CancelToken::new();
    let on_signal = cancel.clone();
    if let Err(err) = ctrlc::set_handler(move || on_signal.cancel()) {
        log::debug!("not handling Ctrl-C gracefully: {err}");
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            log::error!("starting the async runtime: {err}");
            return Outcome::Failed.into();
        }
    };
    match runtime.block_on(run::run(&cli, &cancel)) {
        Ok(outcome) => outcome.into(),
        Err(err) => {
            log::error!("{err:#}");
            Outcome::Failed.into()
        }
    }
}
