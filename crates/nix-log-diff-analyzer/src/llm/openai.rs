//! OpenAI-compatible HTTP transport: `{base}/models` and
//! `{base}/chat/completions`. Works with ollama, llama.cpp's `llama-server`
//! and hosted APIs such as Gemini's. Decoding of the streamed reply lives in
//! `stream.rs`.

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
    /// `host` is a bare `host[:port]` or a URL; see [`parse_host`].
    /// `default_port` is applied to a scheme-less host without a port.
    pub fn new(
        label: &'static str,
        host: &str,
        default_port: Option<u16>,
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
            .authorized(self.client.get(self.url("models")?))
            .send()
            .with_context(|| what.clone())?;
        let models: Models = ensure_success(response)
            .with_context(|| what.clone())?
            .json()
            .with_context(|| format!("invalid model list from {}", self.label))?;
        // Some APIs (Gemini) prefix ids with "models/", which they also
        // accept being left off; show the plain name.
        Ok(models
            .data
            .into_iter()
            .map(|m| m.id.strip_prefix("models/").map_or(m.id.clone(), str::to_owned))
            .collect())
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
            // mode, ollama and hosted APIs use it to pick one.
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
            .authorized(self.client.post(self.url("chat/completions")?))
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
    /// Not in the OpenAI spec: strict servers reject it.
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

/// Turn the user's address into the API base URL that endpoints hang off,
/// following the OpenAI SDK convention for `base_url`:
///
/// - `host` / `host:port` / `http://host:port` (no path): the usual OpenAI
///   layout, so `/v1` is appended.
/// - A URL with a path (`.../v1`, `.../v1beta/openai/`, ...): used as given.
///
/// The result always ends in `/` so `Url::join` appends endpoints instead of
/// replacing the last path segment.
fn parse_host(raw: &str, default_port: Option<u16>) -> Result<Url> {
    let raw = raw.trim();
    let has_scheme = raw.contains("://");
    let mut url = if has_scheme {
        Url::parse(raw)
    } else {
        Url::parse(&format!("http://{raw}"))
    }
    .with_context(|| format!("invalid host '{raw}'"))?;

    if let (false, None, Some(port)) = (has_scheme, url.port(), default_port) {
        url.set_port(Some(port))
            .map_err(|()| anyhow!("host '{raw}' cannot carry a port"))?;
    }

    let path = match url.path().trim_end_matches('/') {
        "" => "/v1/".to_owned(),
        path => format!("{path}/"),
    };
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
    let hint = if status.is_client_error() && body.contains("top_k") {
        "\nhint: this server rejects the non-standard `top_k` option; use --endpoint openai"
    } else {
        ""
    };
    bail!("HTTP {status}: {}{hint}", body.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(raw: &str) -> String {
        parse_host(raw, Some(11434)).unwrap().to_string()
    }

    #[test]
    fn bare_host_gets_scheme_default_port_and_v1() {
        assert_eq!(host("localhost"), "http://localhost:11434/v1/");
    }

    #[test]
    fn explicit_port_is_kept() {
        assert_eq!(host("127.0.0.1:8080"), "http://127.0.0.1:8080/v1/");
        assert_eq!(host("http://example.org:9000"), "http://example.org:9000/v1/");
    }

    #[test]
    fn url_without_a_port_gets_no_default_port() {
        assert_eq!(host("https://example.org"), "https://example.org/v1/");
        assert_eq!(
            parse_host("example.org", None).unwrap().to_string(),
            "http://example.org/v1/"
        );
    }

    #[test]
    fn a_path_is_taken_as_the_api_base() {
        assert_eq!(host("http://h:1/v1"), "http://h:1/v1/");
        assert_eq!(host("http://h:1/v1/"), "http://h:1/v1/");
        assert_eq!(host("https://x.org/api/v1"), "https://x.org/api/v1/");
        assert_eq!(
            host("https://generativelanguage.googleapis.com/v1beta/openai/"),
            "https://generativelanguage.googleapis.com/v1beta/openai/"
        );
    }

    #[test]
    fn endpoints_join_onto_the_base() {
        let gemini = parse_host("https://g.example/v1beta/openai/", None).unwrap();
        assert_eq!(
            gemini.join("chat/completions").unwrap().as_str(),
            "https://g.example/v1beta/openai/chat/completions"
        );
        for raw in ["http://h:1", "http://h:1/v1", "http://h:1/v1/"] {
            let base = parse_host(raw, None).unwrap();
            assert_eq!(base.join("models").unwrap().as_str(), "http://h:1/v1/models");
        }
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_host("http://", Some(1)).is_err());
        assert!(parse_host("", Some(1)).is_err());
    }
}
