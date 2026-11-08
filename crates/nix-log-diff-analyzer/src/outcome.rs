//! What a run amounted to, and the process exit status for each case.

use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `--dry-run` finished. Exit 0.
    DryRun,
    /// The diff is only environmental noise. Exit 0.
    Inconsequential,
    /// The diff contains real differences. Exit 1.
    Consequential,
    /// Something went wrong (bad input, unreachable server, ...). Exit 1.
    Failed,
    /// No usable answer: unrecognised output, or the thinking budget ran
    /// out before the model answered. Exit 2.
    NoAnswer,
    /// Interrupted (Ctrl-C). Exit 130.
    Cancelled,
}

impl From<Outcome> for ExitCode {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::DryRun | Outcome::Inconsequential => ExitCode::SUCCESS,
            Outcome::Consequential | Outcome::Failed => ExitCode::FAILURE,
            Outcome::NoAnswer => ExitCode::from(2),
            Outcome::Cancelled => ExitCode::from(130),
        }
    }
}
