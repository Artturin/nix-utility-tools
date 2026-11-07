//! Command-line interface (all clap derives live here).

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Which LLM server to talk to. Both speak the OpenAI-compatible API
/// (`/v1/chat/completions`); the choice only sets the default address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LlmEndpoint {
    /// ollama (default address 127.0.0.1:11434)
    Ollama,
    /// llama.cpp `llama-server` (default address 127.0.0.1:8080)
    #[value(name = "llama-cpp", alias = "llamacpp", alias = "llama.cpp")]
    LlamaCpp,
}

/// Ask a local LLM whether a diff of two sorted Nix build logs is
/// inconsequential noise or a consequential change.
///
/// Exit status: 0 = inconsequential, 1 = consequential or error,
/// 2 = unrecognised model output.
#[derive(Debug, Parser)]
#[command(version, about, long_about)]
pub struct Cli {
    /// Diff file to analyze (reads stdin when omitted)
    pub diff_file: Option<PathBuf>,

    /// Don't stream the model's thinking trace to stderr
    #[arg(short, long)]
    pub quiet: bool,

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

    /// Server address, e.g. http://127.0.0.1:11434
    /// [default: $OLLAMA_HOST or 127.0.0.1:11434 for ollama, 127.0.0.1:8080 for llama-cpp]
    #[arg(long, env = "NIX_LOG_DIFF_ANALYZER_LLM_HOST")]
    pub host: Option<String>,

    /// Model name (prompts interactively when omitted and several are available)
    #[arg(short, long, env = "NIX_LOG_DIFF_ANALYZER_MODEL")]
    pub model: Option<String>,

    /// Bearer token for servers that require one (llama-server --api-key)
    #[arg(long, env = "NIX_LOG_DIFF_ANALYZER_LLM_API_KEY", hide_env_values = true)]
    pub api_key: Option<String>,
}
