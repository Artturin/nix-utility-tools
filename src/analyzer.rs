//! Glue: read the diff, pick a model, run the LLM, report the verdict.

use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use dialoguer::FuzzySelect;

use crate::cli::Cli;
use crate::llm::{self, Event, Exceeded, Generation, Limits, OpenAiClient, Sampling};
use crate::logging::{self, GREY, YELLOW};
use crate::outcome::Outcome;
use crate::parse::{self, Verdict};
use crate::prompt;

pub fn run(cli: &Cli) -> Result<Outcome> {
    let diff = read_diff(cli.diff_file.as_deref())?;
    let client = llm::connect(cli.endpoint, cli.host.as_deref(), cli.api_key.as_deref())?;

    if cli.dry_run {
        log::info!(
            "DRY RUN: Would query {} with model '{}'.",
            client.describe(),
            cli.model.as_deref().unwrap_or("<selected interactively>")
        );
        println!("{}", prompt::preview(&diff));
        return Ok(Outcome::DryRun);
    }

    let model = resolve_model(cli.model.as_deref(), &client)?;
    let limits = Limits {
        thinking: cli.max_thinking_tokens,
        output: cli.max_output_tokens,
    };

    log::info!(
        "Analyzing diff via {} ({})...",
        client.name(),
        if cli.quiet { "silently in background" } else { "streaming live" }
    );
    let generation = generate(&client, &model, &prompt::build(&diff), &limits, cli.quiet)?;
    Ok(report(&generation))
}

/// Run the model, streaming its thinking trace to stderr unless `quiet`.
fn generate(
    client: &OpenAiClient,
    model: &str,
    prompt: &str,
    limits: &Limits,
    quiet: bool,
) -> Result<Generation> {
    let mut thinking = ThinkingPrinter::default();
    let result = {
        let mut on_event = |event: Event<'_>| {
            if !quiet {
                thinking.handle(event);
            }
        };
        client.generate(model, prompt, &Sampling::default(), limits, &mut on_event)
    };
    thinking.finish(); // reset colour even when generation failed
    result
}

/// Turn a finished generation into output for the user and an outcome.
fn report(generation: &Generation) -> Outcome {
    match generation.exceeded {
        Some(Exceeded::Thinking(limit)) => {
            log::error!(
                "The model thought for more than {limit} tokens, so generation was stopped before it answered. Raise --max-thinking-tokens."
            );
            return Outcome::NoAnswer;
        }
        Some(Exceeded::Output(limit)) => log::warn!(
            "The answer went past {limit} tokens and was cut off; the explanation is incomplete."
        ),
        None if generation.finish_reason.as_deref() == Some("length") => log::warn!(
            "Generation stopped at a token limit set by the server or model; the output is incomplete."
        ),
        None => {}
    }

    match parse::parse(&generation.content) {
        Verdict::Inconsequential => {
            logging::success!("Result: Inconsequential changes only (Noise).");
            Outcome::Inconsequential
        }
        Verdict::Consequential { explanation } => {
            log::warn!("Result: Consequential differences found!");
            anstream::println!("\n{YELLOW}Explanation:{YELLOW:#}\n{explanation}");
            Outcome::Consequential
        }
        Verdict::Unrecognized => {
            if generation.content.trim().is_empty() {
                log::error!("The model returned no answer text.");
            }
            log::error!("Unrecognized LLM output format. Raw output dumped below:");
            println!("{}", generation.content);
            Outcome::NoAnswer
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
fn resolve_model(requested: Option<&str>, client: &OpenAiClient) -> Result<String> {
    if let Some(model) = requested {
        return Ok(model.to_owned());
    }

    let mut models = client.list_models()?;
    match models.len() {
        0 => bail!("{} reports no models; pass --model", client.name()),
        1 => Ok(models.remove(0)),
        // The picker draws on stderr; without a terminal it would just wait.
        count if !io::stderr().is_terminal() => bail!(
            "{} offers {count} models and there is no terminal to choose from; pass --model or set NIX_LOG_DIFF_ANALYZER_MODEL",
            client.name()
        ),
        _ => {
            let index = FuzzySelect::new()
                .with_prompt("Select model")
                .default(0) // otherwise nothing is highlighted and Enter does nothing
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
