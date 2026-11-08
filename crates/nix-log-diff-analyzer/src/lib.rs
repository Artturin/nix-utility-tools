//! Ask an LLM whether a diff of two sorted Nix build logs is noise or
//! consequential. No printing and no prompting here: callers feed in the diff,
//! stream [`llm::Event`]s, and get a [`Analysis`] back. Everything is async
//! and honours a [`CancelToken`] promptly, even while the model is silent.

pub mod llm;
pub mod parse;
pub mod prompt;

use anyhow::Result;
pub use nix_utility_common::CancelToken;

use llm::{Event, Generation, Limits, OpenAiClient, Sampling};
use parse::Verdict;

/// A finished (or cancelled) analysis.
#[derive(Debug)]
pub struct Analysis {
    pub generation: Generation,
    /// Meaningless when `generation.cancelled`.
    pub verdict: Verdict,
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
    let generation = client
        .generate(
            model,
            &prompt::build(diff),
            sampling,
            limits,
            cancel,
            on_event,
        )
        .await?;
    let verdict = parse::parse(&generation.content);
    Ok(Analysis {
        generation,
        verdict,
    })
}
