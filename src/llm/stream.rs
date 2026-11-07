//! Decoding of the OpenAI streaming response (server-sent events) into a
//! [`Generation`], enforcing the client-side token budgets as it goes.
//!
//! Reasoning arrives in `delta.reasoning_content` (llama.cpp) or
//! `delta.reasoning` (ollama). If a server leaves the thinking inline in the
//! content instead, `parse` strips it afterwards.

use std::io::{self, ErrorKind, Read};

use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::Eventsource;
use futures::{Stream, executor::block_on_stream, stream};
use serde::Deserialize;

use super::{Event, Generation, Limits};

/// Read the whole stream from `reader`, calling `on_event` as chunks arrive.
///
/// Stops early, dropping the reader (and so the connection), as soon as a
/// budget in `limits` is exceeded.
pub(super) fn read_generation<R: Read>(
    reader: R,
    label: &str,
    limits: &Limits,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<Generation> {
    let mut generation = Generation::default();

    for event in block_on_stream(byte_chunks(reader).eventsource()) {
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

        for choice in chunk.choices {
            if choice.finish_reason.is_some() {
                generation.finish_reason = choice.finish_reason;
            }
            let delta = choice.delta;
            for thinking in [delta.reasoning_content, delta.reasoning].into_iter().flatten() {
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

/// Adapt a blocking reader into the byte-chunk stream the SSE parser wants.
fn byte_chunks<R: Read>(mut reader: R) -> impl Stream<Item = io::Result<Vec<u8>>> + Unpin {
    stream::iter(std::iter::from_fn(move || {
        let mut buf = vec![0; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return None,
                Ok(n) => {
                    buf.truncate(n);
                    return Some(Ok(buf));
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Some(Err(e)),
            }
        }
    }))
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

    /// Hands out one byte per read, like a very slow connection, so events
    /// get split across reads at every possible point.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some((&first, rest)) = self.0.split_first() else {
                return Ok(0);
            };
            buf[0] = first;
            self.0 = rest;
            Ok(1)
        }
    }

    fn event(delta: &str) -> String {
        format!("data: {{\"choices\":[{{\"delta\":{delta}}}]}}\n\n")
    }

    fn run(body: &str, limits: Limits) -> (Result<Generation>, Vec<String>) {
        let mut seen = Vec::new();
        let result = read_generation(Trickle(body.as_bytes()), "test", &limits, &mut |e| {
            seen.push(format!("{e:?}"))
        });
        (result, seen)
    }

    #[test]
    fn splits_thinking_from_content_and_records_finish_reason() {
        let body = [
            event(r#"{"reasoning_content":"think "}"#),
            event(r#"{"reasoning":"more"}"#),
            event(r#"{"content":"True"}"#),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".into(),
            "data: [DONE]\n\n".into(),
        ]
        .concat();

        let (result, seen) = run(&body, Limits::default());
        let generation = result.unwrap();

        assert_eq!(generation.thinking, "think more");
        assert_eq!(generation.content, "True");
        assert_eq!(generation.finish_reason.as_deref(), Some("stop"));
        assert_eq!((generation.thinking_tokens, generation.output_tokens), (2, 1));
        assert_eq!(
            seen,
            [r#"Thinking("think ")"#, r#"Thinking("more")"#, r#"Content("True")"#]
        );
    }

    #[test]
    fn handles_crlf_line_endings_and_a_missing_done_marker() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"False\"}}]}\r\n\r\n";
        let (result, _) = run(body, Limits::default());
        assert_eq!(result.unwrap().content, "False");
    }

    #[test]
    fn ignores_comments_keep_alives_and_empty_events() {
        let body = format!(": keep-alive\n\ndata: \n\n{}data: [DONE]\n\n", event(r#"{"content":"x"}"#));
        let (result, _) = run(&body, Limits::default());
        assert_eq!(result.unwrap().content, "x");
    }

    #[test]
    fn stops_as_soon_as_the_thinking_budget_is_exceeded() {
        let body: String = (0..5).map(|_| event(r#"{"reasoning_content":"t"}"#)).collect();
        let limits = Limits {
            thinking: Some(2),
            output: None,
        };
        let (result, seen) = run(&body, limits);
        let generation = result.unwrap();

        assert_eq!(generation.exceeded, Some(Exceeded::Thinking(2)));
        assert_eq!(generation.thinking_tokens, 3);
        assert_eq!(seen.len(), 3);
        assert!(generation.content.is_empty());
    }

    #[test]
    fn output_budget_keeps_what_arrived() {
        let body: String = ["a", "b", "c", "d"]
            .iter()
            .map(|c| event(&format!(r#"{{"content":"{c}"}}"#)))
            .collect();
        let limits = Limits {
            thinking: None,
            output: Some(1),
        };
        let generation = run(&body, limits).0.unwrap();

        assert_eq!(generation.exceeded, Some(Exceeded::Output(1)));
        assert_eq!(generation.content, "ab");
    }

    #[test]
    fn error_chunk_becomes_an_error() {
        let (result, _) = run("data: {\"error\":{\"message\":\"boom\"}}\n\n", Limits::default());
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("test error"), "{message}");
        assert!(message.contains("boom"), "{message}");
    }

    #[test]
    fn invalid_json_becomes_an_error() {
        let (result, _) = run("data: not json\n\n", Limits::default());
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("invalid test stream chunk"), "{message}");
    }
}
