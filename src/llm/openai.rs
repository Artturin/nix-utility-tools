//! OpenAI-compatible HTTP transport: `/v1/models` and `/v1/chat/completions`.
//! Works with ollama and llama.cpp's `llama-server`. Decoding of the streamed
//! reply lives in `stream.rs`.

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use url::Url;

use super::{Event, Generation, Limits, Sampling, stream};

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
        limits: &Limits,
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
            max_tokens: limits.server_cap(),
        };

        let response = self
            .authorized(self.client.post(self.url("v1/chat/completions")?))
            .json(&request)
            .send()
            .with_context(|| format!("sending chat request to {}", self.label))?;
        let response = ensure_success(response)
            .with_context(|| format!("{} rejected the chat request", self.label))?;

        stream::read_generation(response, self.label, limits, on_event)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

/// Accept `host`, `host:port` or a full URL. A trailing `/v1` is dropped
/// (endpoints add it), and the path always ends in `/` so `Url::join`
/// appends endpoints instead of replacing the last segment.
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

    let mut path = url.path().to_owned();
    if !path.ends_with('/') {
        path.push('/');
    }
    if let Some(prefix) = path.strip_suffix("/v1/") {
        path = format!("{prefix}/");
    }
    url.set_path(&path);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn host(raw: &str) -> String {
        parse_host(raw, 11434).unwrap().to_string()
    }

    #[test]
    fn bare_host_gets_scheme_and_default_port() {
        assert_eq!(host("localhost"), "http://localhost:11434/");
    }

    #[test]
    fn explicit_port_is_kept() {
        assert_eq!(host("127.0.0.1:8080"), "http://127.0.0.1:8080/");
        assert_eq!(host("http://example.org:9000"), "http://example.org:9000/");
    }

    #[test]
    fn full_url_keeps_its_scheme() {
        assert_eq!(host("https://example.org"), "https://example.org/");
    }

    #[test]
    fn trailing_v1_is_dropped() {
        assert_eq!(host("http://h:1/v1"), "http://h:1/");
        assert_eq!(host("http://h:1/v1/"), "http://h:1/");
        assert_eq!(host("https://x.org/api/v1"), "https://x.org/api/");
    }

    #[test]
    fn path_prefixes_are_kept() {
        assert_eq!(host("http://h/ollama"), "http://h/ollama/");
        assert_eq!(host("http://h/v10"), "http://h/v10/");
    }

    #[test]
    fn endpoints_join_correctly_with_or_without_v1() {
        for raw in ["http://h:1", "http://h:1/v1", "http://h:1/v1/"] {
            let base = parse_host(raw, 1).unwrap();
            assert_eq!(base.join("v1/models").unwrap().as_str(), "http://h:1/v1/models");
        }
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_host("http://", 1).is_err());
        assert!(parse_host("", 1).is_err());
    }
}
