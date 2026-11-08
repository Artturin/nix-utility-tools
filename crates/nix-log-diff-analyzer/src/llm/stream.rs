//! Decoding of the OpenAI streaming response (server-sent events) into a
//! [`Generation`], enforcing the client-side token budgets as it goes.
//!
//! Reasoning arrives in `delta.reasoning_content` (llama.cpp) or
//! `delta.reasoning` (ollama). If a server leaves the thinking inline in the
//! content instead, `parse` strips it afterwards.

use std::pin::pin;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use nix_utility_common::CancelToken;
use serde::Deserialize;

use super::{Event, Generation, Limits, Step, step};

/// Read the whole stream, calling `on_event` as chunks arrive.
///
/// Stops early, dropping the stream (and so the connection), as soon as a
/// budget in `limits` is exceeded or `cancel` fires. Waiting more than `idle`
/// for the next chunk is an error.
pub(super) async fn read_generation<S, B, E>(
    bytes: S,
    label: &str,
    limits: &Limits,
    idle: Option<Duration>,
    cancel: &CancelToken,
    on_event: &mut (dyn FnMut(Event<'_>) + Send),
) -> Result<Generation>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut generation = Generation::default();
    let mut events = pin!(bytes.eventsource());

    loop {
        let event = match step(cancel, idle, events.next()).await {
            Step::Done(Some(event)) => event,
            Step::Done(None) => break,
            Step::Cancelled => {
                generation.cancelled = true;
                break;
            }
            Step::TimedOut => bail!(
                "{label} sent nothing for {}s; giving up",
                idle.map_or(0, |idle| idle.as_secs())
            ),
        };
        let event = event.with_context(|| format!("reading {label} stream"))?;
        let data = event.data.trim();
        if data == "[DONE]" {
            break;
        }
        if data.is_empty() {
            continue;
        }

        let chunk: Chunk = serde_json::from_str(data)
            .map_err(|e| anyhow!("invalid {label} stream chunk ({e}): {data}"))?;
        if let Some(error) = chunk.error {
            bail!("{label} error: {error}");
        }

        if let Some(tokens) = chunk.usage.and_then(|usage| usage.prompt_tokens) {
            generation.prompt_tokens = Some(tokens);
        }

        for choice in chunk.choices {
            if choice.finish_reason.is_some() {
                generation.finish_reason = choice.finish_reason;
            }
            let delta = choice.delta;
            for thinking in [delta.reasoning_content, delta.reasoning]
                .into_iter()
                .flatten()
            {
                generation.record(Event::Thinking(&thinking), on_event);
            }
            if let Some(content) = delta.content {
                generation.record(Event::Content(&content), on_event);
            }
        }

        generation.exceeded = limits.check(generation.thinking_tokens, generation.output_tokens);
        if generation.exceeded.is_some() {
            break;
        }
    }
    Ok(generation)
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    error: Option<serde_json::Value>,
    /// Sent in the last chunk when usage was requested.
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Exceeded;

    /// Hands out one byte per chunk, like a very slow connection, so events
    /// get split across reads at every possible point.
    fn trickle(body: &str) -> impl Stream<Item = Result<Vec<u8>, std::io::Error>> + use<> {
        futures::stream::iter(body.bytes().map(|b| Ok(vec![b])).collect::<Vec<_>>())
    }

    fn event(delta: &str) -> String {
        format!("data: {{\"choices\":[{{\"delta\":{delta}}}]}}\n\n")
    }

    async fn run(body: &str, limits: Limits) -> (Result<Generation>, Vec<String>) {
        let mut seen = Vec::new();
        let result = read_generation(
            trickle(body),
            "test",
            &limits,
            None,
            &CancelToken::new(),
            &mut |e| seen.push(format!("{e:?}")),
        )
        .await;
        (result, seen)
    }

    #[tokio::test]
    async fn splits_thinking_from_content_and_records_finish_reason() {
        let body = [
            event(r#"{"reasoning_content":"think "}"#),
            event(r#"{"reasoning":"more"}"#),
            event(r#"{"content":"True"}"#),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".into(),
            "data: [DONE]\n\n".into(),
        ]
        .concat();

        let (result, seen) = run(&body, Limits::default()).await;
        let generation = result.unwrap();

        assert_eq!(generation.thinking, "think more");
        assert_eq!(generation.content, "True");
        assert_eq!(generation.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            (generation.thinking_tokens, generation.output_tokens),
            (2, 1)
        );
        assert_eq!(
            seen,
            [
                r#"Thinking("think ")"#,
                r#"Thinking("more")"#,
                r#"Content("True")"#
            ]
        );
    }

    #[tokio::test]
    async fn records_the_prompt_tokens_the_server_reports() {
        let body = [
            event(r#"{"content":"True"}"#),
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4096,\"completion_tokens\":1}}\n\n"
                .into(),
            "data: [DONE]\n\n".into(),
        ]
        .concat();
        let (result, _) = run(&body, Limits::default()).await;
        assert_eq!(result.unwrap().prompt_tokens, Some(4096));

        let (result, _) = run(&event(r#"{"content":"x"}"#), Limits::default()).await;
        assert_eq!(result.unwrap().prompt_tokens, None);
    }

    #[tokio::test]
    async fn handles_crlf_line_endings_and_a_missing_done_marker() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"False\"}}]}\r\n\r\n";
        let (result, _) = run(body, Limits::default()).await;
        assert_eq!(result.unwrap().content, "False");
    }

    #[tokio::test]
    async fn ignores_comments_keep_alives_and_empty_events() {
        let body = format!(
            ": keep-alive\n\ndata: \n\n{}data: [DONE]\n\n",
            event(r#"{"content":"x"}"#)
        );
        let (result, _) = run(&body, Limits::default()).await;
        assert_eq!(result.unwrap().content, "x");
    }

    #[tokio::test]
    async fn stops_as_soon_as_the_thinking_budget_is_exceeded() {
        let body: String = (0..5)
            .map(|_| event(r#"{"reasoning_content":"t"}"#))
            .collect();
        let limits = Limits {
            thinking: Some(2),
            output: None,
        };
        let (result, seen) = run(&body, limits).await;
        let generation = result.unwrap();

        assert_eq!(generation.exceeded, Some(Exceeded::Thinking(2)));
        assert_eq!(generation.thinking_tokens, 3);
        assert_eq!(seen.len(), 3);
        assert!(generation.content.is_empty());
    }

    #[tokio::test]
    async fn output_budget_keeps_what_arrived() {
        let body: String = ["a", "b", "c", "d"]
            .iter()
            .map(|c| event(&format!(r#"{{"content":"{c}"}}"#)))
            .collect();
        let limits = Limits {
            thinking: None,
            output: Some(1),
        };
        let generation = run(&body, limits).await.0.unwrap();

        assert_eq!(generation.exceeded, Some(Exceeded::Output(1)));
        assert_eq!(generation.content, "ab");
    }

    #[tokio::test]
    async fn error_chunk_becomes_an_error() {
        let (result, _) = run(
            "data: {\"error\":{\"message\":\"boom\"}}\n\n",
            Limits::default(),
        )
        .await;
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("test error"), "{message}");
        assert!(message.contains("boom"), "{message}");
    }

    #[tokio::test]
    async fn invalid_json_becomes_an_error() {
        let (result, _) = run("data: not json\n\n", Limits::default()).await;
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("invalid test stream chunk"), "{message}");
    }

    #[tokio::test]
    async fn cancelling_while_the_server_is_silent_returns_what_arrived() {
        let cancel = CancelToken::new();
        let first = event(r#"{"content":"x"}"#).into_bytes();
        let stream = futures::stream::iter([Ok::<_, std::io::Error>(first)])
            .chain(futures::stream::pending());
        let canceller = cancel.clone();
        let mut on_event = move |_: Event<'_>| canceller.cancel();
        let generation = read_generation(
            stream,
            "test",
            &Limits::default(),
            None,
            &cancel,
            &mut on_event,
        )
        .await
        .unwrap();
        assert!(generation.cancelled);
        assert_eq!(generation.content, "x");
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_server_times_out() {
        let stream = futures::stream::pending::<Result<Vec<u8>, std::io::Error>>();
        let result = read_generation(
            stream,
            "test",
            &Limits::default(),
            Some(Duration::from_secs(5)),
            &CancelToken::new(),
            &mut |_| {},
        )
        .await;
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("sent nothing for 5s"), "{message}");
    }
}
