//! Running one analysis in the background and turning what the model says
//! into updates for the window. Free of any widgets.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nix_log_diff_analyzer::llm::{self, Event, Exceeded, Limits, Sampling, Timeouts};
use nix_log_diff_analyzer::{CancelToken, analyze};

use crate::analysis::{AnalysisResult, AnalyzerSettings, Provenance, Trigger, now};
use crate::keystore::KeyStore;

/// Streamed text reaches the window at most this often.
const FLUSH_EVERY: Duration = Duration::from_millis(100);

pub struct Job {
    pub diff: String,
    pub trigger: Trigger,
}

#[derive(Debug)]
pub enum Outcome {
    Done(Box<AnalysisResult>),
    Cancelled,
    Failed(String),
}

#[derive(Debug)]
pub enum Update {
    /// New streamed text since the last one.
    Delta {
        thinking: String,
        answer: String,
    },
    Finished(Outcome),
}

/// What the key store files the API key under.
pub fn key_account(settings: &AnalyzerSettings) -> String {
    format!("{} {}", settings.endpoint.id(), settings.host.trim())
}

fn nonzero(limit: u32) -> Option<u32> {
    (limit > 0).then_some(limit)
}

/// Hands streamed text on in batches rather than token by token.
struct Batcher<'a> {
    thinking: String,
    answer: String,
    last: Instant,
    emit: &'a mut (dyn FnMut(Update) + Send),
}

impl Batcher<'_> {
    fn add(&mut self, event: Event<'_>) {
        match event {
            Event::Thinking(text) => self.thinking.push_str(text),
            Event::Content(text) => self.answer.push_str(text),
        }
        if self.last.elapsed() >= FLUSH_EVERY {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.last = Instant::now();
        if self.thinking.is_empty() && self.answer.is_empty() {
            return;
        }
        (self.emit)(Update::Delta {
            thinking: std::mem::take(&mut self.thinking),
            answer: std::mem::take(&mut self.answer),
        });
    }
}

/// Ask the model about `job` and stream its progress to `emit`. Does not send
/// `Update::Finished`; the caller does with the returned outcome.
pub async fn run_job(
    settings: &AnalyzerSettings,
    api_key: Option<&str>,
    job: &Job,
    cancel: &CancelToken,
    emit: &mut (dyn FnMut(Update) + Send),
) -> Outcome {
    let timeouts = Timeouts {
        idle: (settings.idle_timeout_secs > 0)
            .then(|| Duration::from_secs(settings.idle_timeout_secs)),
        ..Timeouts::default()
    };
    let host = Some(settings.host.trim()).filter(|host| !host.is_empty());
    let client = match llm::connect(settings.endpoint.llm(), host, api_key, timeouts) {
        Ok(client) => client,
        Err(err) => return Outcome::Failed(format!("{err:#}")),
    };
    let limits = Limits {
        thinking: nonzero(settings.max_thinking_tokens),
        output: nonzero(settings.max_output_tokens),
    };
    let sampling = Sampling::for_endpoint(settings.endpoint.llm());

    let mut batcher = Batcher {
        thinking: String::new(),
        answer: String::new(),
        last: Instant::now(),
        emit,
    };
    let result = analyze(
        &client,
        &settings.model,
        &job.diff,
        &sampling,
        &limits,
        cancel,
        &mut |event| batcher.add(event),
    )
    .await;
    batcher.flush();

    let analysis = match result {
        Ok(analysis) => analysis,
        Err(err) => return Outcome::Failed(format!("{err:#}")),
    };
    let generation = &analysis.generation;
    if generation.cancelled {
        return Outcome::Cancelled;
    }
    if let Some(Exceeded::Thinking(limit)) = generation.exceeded {
        return Outcome::Failed(format!(
            "The model thought for more than {limit} tokens and was stopped before it answered. \
             Raise the thinking limit in the preferences."
        ));
    }

    let provenance = Provenance {
        endpoint: settings.endpoint,
        model: settings.model.clone(),
        host: Some(client.origin()),
        trigger: job.trigger,
        timestamp: now(),
    };
    let Some(mut result) = AnalysisResult::from_model(
        analysis.verdict,
        analysis.prompt_check,
        generation.prompt_tokens,
        provenance,
    ) else {
        return Outcome::Failed(if generation.content.trim().is_empty() {
            "The model returned no answer.".to_owned()
        } else {
            "The model's answer did not start with True or False.".to_owned()
        });
    };
    let cut = match generation.exceeded {
        Some(Exceeded::Output(limit)) => Some(format!("The answer went past {limit} tokens")),
        _ if generation.finish_reason.as_deref() == Some("length") => {
            Some("The server stopped the answer at a token limit".to_owned())
        }
        _ => None,
    };
    if let Some(cut) = cut {
        let note = format!("{cut} and was cut off; the explanation may be incomplete.");
        result.warning = Some(match result.warning.take() {
            Some(warning) => format!("{warning} {note}"),
            None => note,
        });
    }
    Outcome::Done(Box::new(result))
}

/// Run `job` on its own thread, looking up the API key first, and report
/// through `emit`, which ends with `Update::Finished`.
pub fn spawn(
    settings: AnalyzerSettings,
    keys: Arc<dyn KeyStore>,
    job: Job,
    cancel: CancelToken,
    mut emit: impl FnMut(Update) + Send + 'static,
) {
    std::thread::spawn(move || {
        let outcome = (|| {
            let key = keys
                .get(&key_account(&settings))
                .map_err(|err| Outcome::Failed(format!("cannot read the API key: {err}")))?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| Outcome::Failed(err.to_string()))?;
            Ok::<_, Outcome>(runtime.block_on(run_job(
                &settings,
                key.as_deref(),
                &job,
                &cancel,
                &mut |update| emit(update),
            )))
        })()
        .unwrap_or_else(|failed| failed);
        emit(Update::Finished(outcome));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{Endpoint, Verdict};
    use serde_json::json;

    fn sse(chunks: &[serde_json::Value]) -> String {
        let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        body
    }

    fn delta(delta: serde_json::Value) -> serde_json::Value {
        json!({ "choices": [{ "delta": delta }] })
    }

    fn job() -> Job {
        Job {
            diff: "-a\n+b\n".into(),
            trigger: Trigger::Auto,
        }
    }

    fn settings(server: &mockito::ServerGuard) -> AnalyzerSettings {
        AnalyzerSettings {
            endpoint: Endpoint::Openai,
            host: server.url(),
            model: "m".into(),
            ..AnalyzerSettings::default()
        }
    }

    async fn run(
        settings: &AnalyzerSettings,
        key: Option<&str>,
        cancel: &CancelToken,
    ) -> (Outcome, Vec<Update>) {
        let mut updates = Vec::new();
        let outcome = run_job(settings, key, &job(), cancel, &mut |u| updates.push(u)).await;
        (outcome, updates)
    }

    #[tokio::test]
    async fn a_verdict_comes_with_its_provenance_and_the_streamed_text() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_header("authorization", "Bearer sekrit")
            .with_status(200)
            .with_body(sse(&[
                delta(json!({ "reasoning_content": "hm" })),
                delta(json!({ "content": "True\nLinks to a new lib." })),
            ]))
            .create_async()
            .await;

        let (outcome, updates) = run(&settings(&server), Some("sekrit"), &CancelToken::new()).await;
        mock.assert_async().await;
        let Outcome::Done(result) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(result.verdict, Verdict::Consequential);
        assert_eq!(result.explanation, "Links to a new lib.");
        assert_eq!(result.provenance.trigger, Trigger::Auto);
        assert_eq!(result.provenance.model, "m");
        assert_eq!(
            result.provenance.host.as_deref(),
            Some(server.url().as_str())
        );
        let (thinking, answer) =
            updates
                .iter()
                .fold((String::new(), String::new()), |mut acc, u| {
                    if let Update::Delta { thinking, answer } = u {
                        acc.0 += thinking;
                        acc.1 += answer;
                    }
                    acc
                });
        assert_eq!(
            (thinking.as_str(), answer.as_str()),
            ("hm", "True\nLinks to a new lib.")
        );
    }

    #[tokio::test]
    async fn a_server_that_read_only_part_of_the_diff_makes_the_verdict_unreliable() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(sse(&[
                delta(json!({ "content": "False" })),
                json!({ "choices": [], "usage": { "prompt_tokens": 3 } }),
            ]))
            .create_async()
            .await;
        let mut settings = settings(&server);
        settings.endpoint = Endpoint::LlamaCpp;
        let big = Job {
            diff: "x".repeat(8000),
            ..job()
        };
        let outcome = run_job(&settings, None, &big, &CancelToken::new(), &mut |_| {}).await;
        let Outcome::Done(result) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(result.unreliable);
        assert_eq!(result.prompt_tokens, Some(3));
        assert!(result.warning.is_some());
    }

    #[tokio::test]
    async fn failures_are_reported_not_hidden() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(sse(&[delta(json!({ "content": "Maybe" }))]))
            .create_async()
            .await;
        let (outcome, _) = run(&settings(&server), None, &CancelToken::new()).await;
        let Outcome::Failed(message) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(message.contains("True or False"), "{message}");

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(401)
            .with_body("bad key")
            .create_async()
            .await;
        let (outcome, _) = run(&settings(&server), None, &CancelToken::new()).await;
        let Outcome::Failed(message) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(
            message.contains("401") || message.contains("bad key"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn thinking_for_too_long_is_a_failure_that_names_the_setting() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(sse(&[
                delta(json!({ "reasoning_content": "a" })),
                delta(json!({ "reasoning_content": "b" })),
                delta(json!({ "reasoning_content": "c" })),
                delta(json!({ "content": "False" })),
            ]))
            .create_async()
            .await;
        let mut settings = settings(&server);
        settings.max_thinking_tokens = 1;
        let (outcome, _) = run(&settings, None, &CancelToken::new()).await;
        let Outcome::Failed(message) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(message.contains("thinking limit"), "{message}");
    }

    #[tokio::test]
    async fn a_cancelled_token_stops_before_any_answer() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(sse(&[delta(json!({ "content": "False" }))]))
            .create_async()
            .await;
        let token = CancelToken::new();
        token.cancel();
        let (outcome, _) = run(&settings(&server), None, &token).await;
        assert!(matches!(outcome, Outcome::Cancelled), "{outcome:?}");
    }

    #[test]
    fn the_key_is_filed_under_the_endpoint_and_host() {
        let settings = AnalyzerSettings {
            endpoint: Endpoint::Openai,
            host: " https://api.example.org ".into(),
            ..AnalyzerSettings::default()
        };
        assert_eq!(key_account(&settings), "openai https://api.example.org");
    }
}
