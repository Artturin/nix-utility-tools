//! Glue: read the diff, pick a model, run the LLM, report the verdict.

use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use dialoguer::FuzzySelect;

use crate::cli::Cli;
use crate::llm::{self, Event, OpenAiClient, Sampling};
use crate::logging::{self, GREY, YELLOW};
use crate::parse::{self, Verdict};
use crate::prompt;

pub fn run(cli: &Cli) -> Result<ExitCode> {
    let diff = read_diff(cli.diff_file.as_deref())?;
    let backend = llm::connect(cli.endpoint, cli.host.as_deref(), cli.api_key.as_deref())?;

    if cli.dry_run {
        log::info!(
            "DRY RUN: Would query {} with model '{}'.",
            backend.describe(),
            cli.model.as_deref().unwrap_or("<selected interactively>")
        );
        println!("{}", prompt::preview(&diff));
        return Ok(ExitCode::SUCCESS);
    }

    let model = resolve_model(cli.model.as_deref(), &backend)?;
    let prompt = prompt::build(&diff);

    if cli.quiet {
        log::info!("Analyzing diff via {} (silently in background)...", backend.name());
    } else {
        log::info!("Analyzing diff via {} (streaming live)...", backend.name());
    }

    let mut thinking = ThinkingPrinter::default();
    let result = {
        let mut on_event = |event: Event<'_>| {
            if !cli.quiet {
                thinking.handle(event);
            }
        };
        backend.generate(&model, &prompt, &Sampling::default(), &mut on_event)
    };
    thinking.finish(); // reset colour even when generation failed
    let generation = result?;

    match parse::parse(&generation.content) {
        Verdict::Inconsequential => {
            logging::success!("Result: Inconsequential changes only (Noise).");
            Ok(ExitCode::SUCCESS)
        }
        Verdict::Consequential { explanation } => {
            log::warn!("Result: Consequential differences found!");
            anstream::println!("\n{YELLOW}Explanation:{YELLOW:#}\n{explanation}");
            Ok(ExitCode::from(1))
        }
        Verdict::Unrecognized => {
            if generation.content.trim().is_empty() {
                log::error!("The model returned no answer text (it may have run out of tokens while thinking).");
            }
            log::error!("Unrecognized LLM output format. Raw output dumped below:");
            println!("{}", generation.content);
            Ok(ExitCode::from(2))
        }
    }
}

/// A file argument wins; otherwise read piped stdin.
fn read_diff(path: Option<&Path>) -> Result<String> {
    let diff = match path {
        Some(path) => fs::read_to_string(path)
            .with_context(|| format!("reading diff file {}", path.display()))?,
        None if !io::stdin().is_terminal() => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("reading diff from stdin")?;
            buf
        }
        None => bail!(
            "Missing diff input.\nUsage: nix-log-diff-analyzer [-q|--quiet] [--dry-run] <path-to-diff-file>\n   or: command | nix-log-diff-analyzer [-q|--quiet] [--dry-run]"
        ),
    };
    if diff.trim().is_empty() {
        bail!("Diff input is empty.");
    }
    Ok(diff)
}

/// Use the requested model, or ask the server and let the user pick.
fn resolve_model(requested: Option<&str>, backend: &OpenAiClient) -> Result<String> {
    if let Some(model) = requested {
        return Ok(model.to_owned());
    }

    let mut models = backend.list_models()?;
    match models.len() {
        0 => bail!("{} reports no models; pass --model", backend.name()),
        1 => Ok(models.remove(0)),
        _ => {
            let index = FuzzySelect::new()
                .with_prompt("Select model")
                .items(&models)
                .interact()
                .context("could not prompt for a model; pass --model or set NIX_LOG_DIFF_ANALYZER_MODEL")?;
            Ok(models.swap_remove(index))
        }
    }
}

/// Streams the thinking trace to stderr in grey, on its own clean lines.
#[derive(Default)]
struct ThinkingPrinter {
    active: bool,
    at_line_start: bool,
}

impl ThinkingPrinter {
    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Thinking(text) => {
                if !self.active {
                    anstream::eprint!("{GREY}");
                    self.active = true;
                }
                anstream::eprint!("{text}");
                self.at_line_start = text.ends_with('\n');
            }
            Event::Content(_) => self.finish(),
        }
    }

    fn finish(&mut self) {
        if self.active {
            anstream::eprint!("{GREY:#}");
            if !self.at_line_start {
                anstream::eprintln!();
            }
            self.active = false;
        }
    }
}
