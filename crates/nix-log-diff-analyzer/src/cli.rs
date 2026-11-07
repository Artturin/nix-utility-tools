//! Command-line interface (all clap derives live here).

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Which LLM server to talk to. All of them speak the OpenAI-compatible API
/// (`/chat/completions`); the choice sets the default address and whether
/// non-standard sampling options are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LlmEndpoint {
    /// ollama (default address 127.0.0.1:11434)
    Ollama,
    /// llama.cpp `llama-server` (default address 127.0.0.1:8080)
    #[value(name = "llama-cpp", alias = "llamacpp", alias = "llama.cpp")]
    LlamaCpp,
    /// Any other OpenAI-compatible API, e.g. Gemini or OpenAI. Sends only
    /// standard options (strict servers reject others) and needs --host.
    Openai,
}

/// Ask a local LLM whether a diff of two sorted Nix build logs is
/// inconsequential noise or a consequential change.
///
/// Exit status: 0 = inconsequential, 1 = consequential or error,
/// 2 = no usable answer (unrecognised output, or the thinking budget ran out).
#[derive(Debug, Parser)]
#[command(version, about, long_about)]
pub struct Cli {
    /// Diff file to analyze (reads stdin when omitted)
    pub diff_file: Option<PathBuf>,

    /// Stream the model's thinking trace to stderr
    #[arg(short = 't', long)]
    pub show_thinking: bool,

    /// Show what would be sent, without contacting the LLM server
    #[arg(long)]
    pub dry_run: bool,

    /// Kind of LLM server to use
    #[arg(
        long,
        env = "NIX_LOG_DIFF_ANALYZER_LLM_ENDPOINT",
        value_enum,
        default_value_t = LlmEndpoint::Ollama
    )]
    pub endpoint: LlmEndpoint,

    /// Server address: `host[:port]`, or an API base URL
    ///
    /// A bare host gets `/v1` appended; a URL with a path is used as given,
    /// e.g. https://generativelanguage.googleapis.com/v1beta/openai/
    /// [default: $OLLAMA_HOST or 127.0.0.1:11434 for ollama, 127.0.0.1:8080 for llama-cpp]
    #[arg(long, env = "NIX_LOG_DIFF_ANALYZER_LLM_HOST")]
    pub host: Option<String>,

    /// Model name (prompts interactively when omitted and several are available)
    #[arg(short, long, env = "NIX_LOG_DIFF_ANALYZER_MODEL")]
    pub model: Option<String>,

    /// Stop if the model thinks for more than this many tokens [default: no limit]
    ///
    /// Tokens are counted as streamed chunks, which is roughly one per token.
    #[arg(long, env = "NIX_LOG_DIFF_ANALYZER_MAX_THINKING_TOKENS", value_name = "TOKENS")]
    pub max_thinking_tokens: Option<u32>,

    /// Cut the answer off after this many tokens, thinking not counted [default: no limit]
    #[arg(long, env = "NIX_LOG_DIFF_ANALYZER_MAX_OUTPUT_TOKENS", value_name = "TOKENS")]
    pub max_output_tokens: Option<u32>,

    /// Bearer token for servers that require one (llama-server --api-key)
    #[arg(long, env = "NIX_LOG_DIFF_ANALYZER_LLM_API_KEY", hide_env_values = true)]
    pub api_key: Option<String>,
}
