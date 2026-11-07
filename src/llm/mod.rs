//! LLM calling. Both ollama and llama.cpp serve the OpenAI-compatible API,
//! so a single client in `openai.rs` talks to either; the selected endpoint
//! only decides the default address.

mod openai;

use anyhow::Result;

pub use openai::OpenAiClient;

use crate::cli::LlmEndpoint;

/// Sampling settings sent with every request.
///
/// Note: `top_k` is not part of the OpenAI spec. llama.cpp honours it;
/// ollama's OpenAI endpoint does not document it and likely ignores it.
#[derive(Debug, Clone, Copy)]
pub struct Sampling {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub max_tokens: u32,
}

impl Default for Sampling {
    fn default() -> Self {
        Self {
            temperature: 0.0,
            top_p: 0.5,
            top_k: 10,
            max_tokens: 1024,
        }
    }
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
}

impl Generation {
    /// Accumulate an event and forward it to `sink`. Empty chunks are ignored.
    fn record(&mut self, event: Event<'_>, sink: &mut dyn FnMut(Event<'_>)) {
        match event {
            Event::Thinking(text) if !text.is_empty() => self.thinking.push_str(text),
            Event::Content(text) if !text.is_empty() => self.content.push_str(text),
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
) -> Result<OpenAiClient> {
    let (label, port, env_host) = match endpoint {
        LlmEndpoint::Ollama => ("ollama", 11434, std::env::var("OLLAMA_HOST").ok()),
        LlmEndpoint::LlamaCpp => ("llama.cpp", 8080, None),
    };
    let raw = host
        .map(str::to_owned)
        .or(env_host)
        .unwrap_or_else(|| format!("127.0.0.1:{port}"));
    OpenAiClient::new(label, &raw, port, api_key)
}
