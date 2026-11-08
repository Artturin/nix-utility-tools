//! LLM calling. ollama, llama.cpp and hosted services (e.g. Gemini) all serve
//! the OpenAI-compatible API, so a single client talks to any of them; the
//! selected endpoint only decides the default address and which non-standard
//! options are sent.
//!
//! - `openai.rs`: HTTP transport (models, chat request).
//! - `stream.rs`: decoding of the streamed response and budget enforcement.

mod openai;
mod stream;

use std::future::Future;
use std::time::Duration;

use anyhow::{Result, bail};
use nix_utility_common::CancelToken;

pub use openai::OpenAiClient;

/// Which LLM server to talk to. All of them speak the OpenAI-compatible API
/// (`/chat/completions`); the choice sets the default address and whether
/// non-standard sampling options are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmEndpoint {
    Ollama,
    LlamaCpp,
    Openai,
}

/// Network patience. A model may be silent for a long while while it loads or
/// thinks, so the idle limit is generous; cancelling never waits for it.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    /// Longest silence tolerated while waiting for the response headers or
    /// the next streamed chunk. `None` waits forever.
    pub idle: Option<Duration>,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            idle: Some(Duration::from_secs(600)),
        }
    }
}

/// How an awaited step ended.
pub(crate) enum Step<T> {
    Done(T),
    Cancelled,
    TimedOut,
}

/// Await `fut`, giving up early on cancellation or after `idle` of waiting.
/// Dropping `fut` closes any connection it owns.
pub(crate) async fn step<T>(
    cancel: &CancelToken,
    idle: Option<Duration>,
    fut: impl Future<Output = T>,
) -> Step<T> {
    let limited = async {
        match idle {
            Some(idle) => tokio::time::timeout(idle, fut).await.ok(),
            None => Some(fut.await),
        }
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Step::Cancelled,
        result = limited => match result {
            Some(value) => Step::Done(value),
            None => Step::TimedOut,
        },
    }
}

/// Sampling settings sent with every request.
#[derive(Debug, Clone, Copy)]
pub struct Sampling {
    pub temperature: f32,
    pub top_p: f32,
    /// Not part of the OpenAI spec. llama.cpp honours it; ollama's OpenAI
    /// endpoint doesn't document it and likely ignores it; strict servers
    /// such as Gemini's reject it, so it's `None` for `--endpoint openai`.
    pub top_k: Option<u32>,
    /// Ask the server to report token usage in the stream (`stream_options`).
    /// Only sent to servers known to accept it; hosted APIs may reject it.
    pub include_usage: bool,
}

impl Sampling {
    pub fn for_endpoint(endpoint: LlmEndpoint) -> Self {
        Self {
            temperature: 0.0,
            top_p: 0.5,
            top_k: match endpoint {
                LlmEndpoint::Ollama | LlmEndpoint::LlamaCpp => Some(10),
                LlmEndpoint::Openai => None,
            },
            include_usage: matches!(endpoint, LlmEndpoint::Ollama | LlmEndpoint::LlamaCpp),
        }
    }
}

/// Separate token budgets for thinking and for the answer.
///
/// The OpenAI API only has one `max_tokens` covering both, so these are
/// enforced client-side by counting streamed chunks (about one per token)
/// and dropping the connection, which makes the server stop generating.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    pub thinking: Option<u32>,
    pub output: Option<u32>,
}

impl Limits {
    /// Which budget, if any, the given counts have gone past.
    pub fn check(&self, thinking_tokens: u32, output_tokens: u32) -> Option<Exceeded> {
        match (self.thinking, self.output) {
            (Some(max), _) if thinking_tokens > max => Some(Exceeded::Thinking(max)),
            (_, Some(max)) if output_tokens > max => Some(Exceeded::Output(max)),
            _ => None,
        }
    }

    /// Server-side backstop: the combined budget, when both are set.
    fn server_cap(&self) -> Option<u32> {
        self.thinking
            .zip(self.output)
            .map(|(thinking, output)| thinking.saturating_add(output))
    }
}

/// Which budget stopped a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exceeded {
    Thinking(u32),
    Output(u32),
}

/// A piece of streamed model output.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    Thinking(&'a str),
    Content(&'a str),
}

/// Everything the model produced, with thinking kept apart from the answer.
#[derive(Debug, Default)]
pub struct Generation {
    pub thinking: String,
    pub content: String,
    /// Why the server stopped, e.g. "stop" or "length" (server-side limit hit).
    pub finish_reason: Option<String>,
    /// Streamed chunks received, roughly one per token.
    pub thinking_tokens: u32,
    pub output_tokens: u32,
    /// Prompt tokens the server says it read, when it reports usage. Servers
    /// that cut an oversized prompt to their context report the cut size.
    pub prompt_tokens: Option<u32>,
    /// Set when we stopped the stream because a client-side budget ran out.
    pub exceeded: Option<Exceeded>,
    /// Set when the caller cancelled; what arrived before that is kept.
    pub cancelled: bool,
}

impl Generation {
    /// Accumulate an event and forward it to `sink`. Empty chunks are ignored.
    fn record(&mut self, event: Event<'_>, sink: &mut (dyn FnMut(Event<'_>) + Send)) {
        match event {
            Event::Thinking(text) if !text.is_empty() => {
                self.thinking.push_str(text);
                self.thinking_tokens += 1;
            }
            Event::Content(text) if !text.is_empty() => {
                self.content.push_str(text);
                self.output_tokens += 1;
            }
            _ => return,
        }
        sink(event);
    }
}

/// Build a client for the chosen endpoint kind, applying its default address.
pub fn connect(
    endpoint: LlmEndpoint,
    host: Option<&str>,
    api_key: Option<&str>,
    timeouts: Timeouts,
) -> Result<OpenAiClient> {
    let (label, port, env_host) = match endpoint {
        LlmEndpoint::Ollama => ("ollama", Some(11434), std::env::var("OLLAMA_HOST").ok()),
        LlmEndpoint::LlamaCpp => ("llama.cpp", Some(8080), None),
        LlmEndpoint::Openai => ("OpenAI-compatible server", None, None),
    };
    let raw = match (host.map(str::to_owned).or(env_host), port) {
        (Some(raw), _) => raw,
        (None, Some(port)) => format!("127.0.0.1:{port}"),
        (None, None) => bail!(
            "--endpoint openai has no default address; pass --host or set NIX_LOG_DIFF_ANALYZER_LLM_HOST"
        ),
    };
    OpenAiClient::new(label, &raw, port, api_key, timeouts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_limits_never_trigger() {
        assert_eq!(Limits::default().check(u32::MAX, u32::MAX), None);
    }

    #[test]
    fn limits_trigger_only_when_strictly_exceeded() {
        let limits = Limits {
            thinking: Some(3),
            output: Some(2),
        };
        assert_eq!(limits.check(3, 2), None);
        assert_eq!(limits.check(4, 0), Some(Exceeded::Thinking(3)));
        assert_eq!(limits.check(0, 3), Some(Exceeded::Output(2)));
    }

    #[test]
    fn each_limit_works_alone() {
        let thinking_only = Limits {
            thinking: Some(1),
            output: None,
        };
        assert_eq!(thinking_only.check(1, 1000), None);
        assert_eq!(thinking_only.check(2, 0), Some(Exceeded::Thinking(1)));

        let output_only = Limits {
            thinking: None,
            output: Some(1),
        };
        assert_eq!(output_only.check(1000, 1), None);
        assert_eq!(output_only.check(0, 2), Some(Exceeded::Output(1)));
    }

    #[test]
    fn server_cap_needs_both_limits() {
        let both = Limits {
            thinking: Some(100),
            output: Some(50),
        };
        assert_eq!(both.server_cap(), Some(150));
        let one = Limits {
            thinking: Some(100),
            output: None,
        };
        assert_eq!(one.server_cap(), None);
        assert_eq!(Limits::default().server_cap(), None);
    }

    #[test]
    fn record_counts_chunks_and_skips_empty_ones() {
        let mut generation = Generation::default();
        let mut seen = 0;
        let mut sink = |_: Event<'_>| seen += 1;
        generation.record(Event::Thinking("a"), &mut sink);
        generation.record(Event::Thinking(""), &mut sink);
        generation.record(Event::Content("b"), &mut sink);
        generation.record(Event::Content(""), &mut sink);
        assert_eq!(
            (generation.thinking_tokens, generation.output_tokens),
            (1, 1)
        );
        assert_eq!(
            (generation.thinking.as_str(), generation.content.as_str()),
            ("a", "b")
        );
        assert_eq!(seen, 2);
    }
}
