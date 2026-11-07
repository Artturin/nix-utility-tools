mod analyzer;
mod cli;
mod llm;
mod logging;
mod outcome;
mod parse;
mod prompt;

use std::process::ExitCode;

use clap::Parser;

use crate::outcome::Outcome;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    logging::init();

    match analyzer::run(&cli) {
        Ok(outcome) => outcome.into(),
        Err(err) => {
            log::error!("{err:#}");
            Outcome::Failed.into()
        }
    }
}
