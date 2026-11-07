//! OpenAI-compatible client: `/v1/models` and `/v1/chat/completions`
//! (server-sent events). Works with ollama and llama.cpp's `llama-server`.
//!
//! Reasoning is read from `delta.reasoning_content` (llama.cpp) or
//! `delta.reasoning` (ollama). If a server leaves the thinking inline in the
//! content instead, `parse` strips it afterwards.

use std::io::{BufRead, BufReader};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use url::Url;

use super::{Event, Generation, Sampling};

pub struct OpenAiClient {
    label: &'static str,
    client: Client,
    base: Url,
    api_key: Option<String>,
}

impl OpenAiClient {
    pub fn new(
        label: &'static str,
        host: &str,
        default_port: u16,
        api_key: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            label,
            // No overall timeout: generations can take minutes.
            client: Client::builder()
                .timeout(None)
                .build()
                .context("building HTTP client")?,
            base: parse_host(host, default_port)?,
            api_key: api_key.map(str::to_owned),
        })
    }

    /// Short display name, e.g. "ollama".
    pub fn name(&self) -> &'static str {
        self.label
    }

    /// Human-readable description of where requests go.
    pub fn describe(&self) -> String {
        format!("{} at {}", self.label, self.base)
    }

    /// Names of the models the server offers.
    pub fn list_models(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Models {
            data: Vec<Model>,
        }
        #[derive(Deserialize)]
        struct Model {
            id: String,
        }

        let what = format!("listing {} models", self.label);
        let response = self
            .authorized(self.client.get(self.url("v1/models")?))
            .send()
            .with_context(|| what.clone())?;
        let models: Models = ensure_success(response)
            .with_context(|| what.clone())?
            .json()
            .with_context(|| format!("invalid model list from {}", self.label))?;
        Ok(models.data.into_iter().map(|m| m.id).collect())
    }

    /// Run the prompt, streaming events to `on_event` as they arrive.
    pub fn generate(
        &self,
        model: &str,
        prompt: &str,
        sampling: &Sampling,
        on_event: &mut dyn FnMut(Event<'_>),
    ) -> Result<Generation> {
        let request = ChatRequest {
            // llama-server serves one model and ignores the field; router
            // mode and ollama use it to pick one.
            model: (!model.is_empty()).then_some(model),
            messages: [Message {
                role: "user",
                content: prompt,
            }],
            stream: true,
            temperature: sampling.temperature,
            top_p: sampling.top_p,
            top_k: sampling.top_k,
            max_tokens: sampling.max_tokens,
        };

        let response = self
            .authorized(self.client.post(self.url("v1/chat/completions")?))
            .json(&request)
            .send()
            .with_context(|| format!("sending chat request to {}", self.label))?;
        let response = ensure_success(response)
            .with_context(|| format!("{} rejected the chat request", self.label))?;

        let mut generation = Generation::default();
        for line in BufReader::new(response).lines() {
            let line = line.with_context(|| format!("reading {} stream", self.label))?;
            let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                continue; // blank keep-alive lines, comments
            };
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                break;
            }
            let chunk: Chunk = serde_json::from_str(data)
                .map_err(|e| anyhow!("invalid {} stream chunk ({e}): {data}", self.label))?;
            if let Some(error) = chunk.error {
                bail!("{} error: {error}", self.label);
            }
            for choice in chunk.choices {
                let delta = choice.delta;
                for thinking in [delta.reasoning_content, delta.reasoning].into_iter().flatten() {
                    generation.record(Event::Thinking(&thinking), on_event);
                }
                if let Some(content) = delta.content {
                    generation.record(Event::Content(&content), on_event);
                }
            }
        }
        Ok(generation)
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base
            .join(path)
            .with_context(|| format!("building URL for '{path}'"))
    }

    fn authorized(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.api_key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    messages: [Message<'a>; 1],
    stream: bool,
    temperature: f32,
    top_p: f32,
    top_k: u32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// llama.cpp
    #[serde(default)]
    reasoning_content: Option<String>,
    /// ollama
    #[serde(default)]
    reasoning: Option<String>,
}

/// Accept `host`, `host:port` or a full URL; always returns a URL whose path
/// ends in `/` so `Url::join` appends endpoints instead of replacing them.
fn parse_host(raw: &str, default_port: u16) -> Result<Url> {
    let raw = raw.trim();
    let has_scheme = raw.contains("://");
    let mut url = if has_scheme {
        Url::parse(raw)
    } else {
        Url::parse(&format!("http://{raw}"))
    }
    .with_context(|| format!("invalid host '{raw}'"))?;

    if !has_scheme && url.port().is_none() {
        url.set_port(Some(default_port))
            .map_err(|()| anyhow!("host '{raw}' cannot carry a port"))?;
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

/// Like `error_for_status`, but keeps the server's error body.
fn ensure_success(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().unwrap_or_default();
    bail!("HTTP {status}: {}", body.trim())
}
