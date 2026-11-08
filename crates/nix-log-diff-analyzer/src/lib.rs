//! Ask an LLM whether a diff of two sorted Nix build logs is noise or
//! consequential. No printing and no prompting here: callers feed in the diff,
//! stream [`llm::Event`]s, and get a [`Analysis`] back. Everything is async
//! and honours a [`CancelToken`] promptly, even while the model is silent.

pub mod llm;
pub mod parse;
pub mod prompt;
pub mod usage;

use anyhow::Result;
pub use nix_utility_common::CancelToken;

use llm::{Event, Generation, Limits, OpenAiClient, Sampling};
use parse::Verdict;
use usage::PromptCheck;

/// A finished (or cancelled) analysis.
#[derive(Debug)]
pub struct Analysis {
    pub generation: Generation,
    /// Meaningless when `generation.cancelled`.
    pub verdict: Verdict,
    /// Whether the server says it read the whole prompt.
    pub prompt_check: PromptCheck,
}

/// Send `diff` to the model and read its verdict, streaming events as they
/// arrive.
pub async fn analyze(
    client: &OpenAiClient,
    model: &str,
    diff: &str,
    sampling: &Sampling,
    limits: &Limits,
    cancel: &CancelToken,
    on_event: &mut (dyn FnMut(Event<'_>) + Send),
) -> Result<Analysis> {
    let prompt = prompt::build(diff);
    let generation = client
        .generate(model, &prompt, sampling, limits, cancel, on_event)
        .await?;
    let verdict = parse::parse(&generation.content);
    let prompt_check = usage::check_prompt(prompt.len(), generation.prompt_tokens);
    Ok(Analysis {
        generation,
        verdict,
        prompt_check,
    })
}
