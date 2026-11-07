mod analyzer;
mod cli;
mod llm;
mod logging;
mod parse;
mod prompt;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    logging::init();

    match analyzer::run(&cli) {
        Ok(code) => code,
        Err(err) => {
            log::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}
