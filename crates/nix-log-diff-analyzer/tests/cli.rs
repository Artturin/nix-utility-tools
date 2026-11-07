//! End-to-end tests: run the real binary against a mock OpenAI-compatible
//! server. stdout/stderr are pipes here, so these also check that no colour
//! codes leak into non-terminal output.

use std::io::Write;
use std::process::{Command, Stdio};

use mockito::{Matcher, Mock, Server, ServerGuard};
use serde_json::{Value, json};

const DIFF: &str = "-foo\n+bar\n";

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_with_env(args: &[&str], env: &[(&str, &str)], stdin: &str) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nix-log-diff-analyzer"))
        .args(args)
        .env_clear()
        .envs(std::env::var_os("SSL_CERT_FILE").map(|cert| ("SSL_CERT_FILE", cert))) // the HTTP client needs CA certificates even for plain http
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the binary");
    // The binary may exit without reading stdin; that's fine.
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    let output = child.wait_with_output().expect("waiting for the binary");
    Run {
        code: output.status.code().expect("exit code"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn run(args: &[&str], stdin: &str) -> Run {
    run_with_env(args, &[], stdin)
}

/// Run against `server` with a model chosen, so no model lookup happens.
fn analyze(server: &ServerGuard, extra: &[&str]) -> Run {
    let host = server.url();
    let mut args = vec!["--host", host.as_str(), "--model", "test-model"];
    args.extend_from_slice(extra);
    run(&args, DIFF)
}

fn delta(d: Value) -> Value {
    json!({ "choices": [{ "delta": d }] })
}

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn chat(server: &mut ServerGuard, chunks: &[Value]) -> Mock {
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse(chunks))
        .create()
}

fn models(server: &mut ServerGuard, ids: &[&str]) -> Mock {
    let data: Vec<Value> = ids.iter().map(|id| json!({ "id": id })).collect();
    server
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(json!({ "object": "list", "data": data }).to_string())
        .create()
}

// ---- verdicts and exit codes ----

#[test]
fn inconsequential_answer_exits_0() {
    let mut server = Server::new();
    chat(&mut server, &[delta(json!({ "content": "False" }))]);

    let result = analyze(&server, &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result.stderr.contains("[INCONSEQUENTIAL]"), "{}", result.stderr);
}

#[test]
fn consequential_answer_exits_1_and_prints_the_explanation_on_stdout() {
    let mut server = Server::new();
    chat(
        &mut server,
        &[
            delta(json!({ "content": "True\n" })),
            delta(json!({ "content": "A new library is linked.\n" })),
        ],
    );

    let result = analyze(&server, &[]);

    assert_eq!(result.code, 1);
    assert!(result.stdout.contains("A new library is linked."), "{}", result.stdout);
    assert!(result.stderr.contains("[WARNING]"), "{}", result.stderr);
}

#[test]
fn unrecognised_answer_exits_2_and_dumps_the_raw_output() {
    let mut server = Server::new();
    chat(&mut server, &[delta(json!({ "content": "Maybe?" }))]);

    let result = analyze(&server, &[]);

    assert_eq!(result.code, 2);
    assert!(result.stdout.contains("Maybe?"), "{}", result.stdout);
    assert!(result.stderr.contains("Unrecognized"), "{}", result.stderr);
}

#[test]
fn inline_think_block_is_ignored_when_reading_the_verdict() {
    let mut server = Server::new();
    chat(
        &mut server,
        &[delta(json!({ "content": "<think>True? hmm</think>\nFalse" }))],
    );

    assert_eq!(analyze(&server, &[]).code, 0);
}

// ---- thinking trace ----

#[test]
fn thinking_is_streamed_to_stderr_for_both_server_flavours() {
    for field in ["reasoning_content", "reasoning"] {
        let mut server = Server::new();
        chat(
            &mut server,
            &[
                delta(json!({ field: "checking targets\n" })),
                delta(json!({ "content": "False" })),
            ],
        );

        let result = analyze(&server, &["--show-thinking"]);

        assert_eq!(result.code, 0, "{field}: {}", result.stderr);
        assert!(result.stderr.contains("checking targets"), "{field}: {}", result.stderr);
        assert!(!result.stdout.contains("checking targets"), "{field}");
    }
}

#[test]
fn thinking_is_hidden_by_default() {
    let mut server = Server::new();
    chat(
        &mut server,
        &[
            delta(json!({ "reasoning_content": "secret thoughts" })),
            delta(json!({ "content": "False" })),
        ],
    );

    let result = analyze(&server, &[]);

    assert_eq!(result.code, 0);
    assert!(!result.stderr.contains("secret thoughts"), "{}", result.stderr);
}

#[test]
fn no_colour_codes_when_output_is_not_a_terminal() {
    let mut server = Server::new();
    chat(
        &mut server,
        &[
            delta(json!({ "reasoning_content": "thinking\n" })),
            delta(json!({ "content": "True\nwhy" })),
        ],
    );

    let result = analyze(&server, &["--show-thinking"]);

    assert!(!result.stderr.contains('\u{1b}'), "{:?}", result.stderr);
    assert!(!result.stdout.contains('\u{1b}'), "{:?}", result.stdout);
}

// ---- token budgets ----

#[test]
fn exceeding_the_thinking_budget_exits_2_before_any_answer() {
    let mut server = Server::new();
    let thoughts: Vec<Value> = (0..3).map(|_| delta(json!({ "reasoning_content": "t" }))).collect();
    chat(&mut server, &thoughts);

    let result = analyze(&server, &["--max-thinking-tokens", "2"]);

    assert_eq!(result.code, 2);
    assert!(result.stderr.contains("--max-thinking-tokens"), "{}", result.stderr);
}

#[test]
fn exceeding_the_output_budget_warns_but_still_reads_the_verdict() {
    let mut server = Server::new();
    chat(
        &mut server,
        &[
            delta(json!({ "content": "True\n" })),
            delta(json!({ "content": "first\n" })),
            delta(json!({ "content": "second\n" })),
        ],
    );

    let result = analyze(&server, &["--max-output-tokens", "2"]);

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("cut off"), "{}", result.stderr);
    assert!(result.stdout.contains("first"), "{}", result.stdout);
}

#[test]
fn server_side_truncation_is_reported() {
    let mut server = Server::new();
    chat(
        &mut server,
        &[
            delta(json!({ "reasoning_content": "still thinking" })),
            json!({ "choices": [{ "delta": {}, "finish_reason": "length" }] }),
        ],
    );

    let result = analyze(&server, &[]);

    assert_eq!(result.code, 2);
    assert!(result.stderr.contains("token limit"), "{}", result.stderr);
}

// ---- request contents ----

#[test]
fn sampling_options_and_combined_cap_are_sent() {
    let mut server = Server::new();
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(json!({
            "model": "test-model",
            "stream": true,
            "temperature": 0.0,
            "top_p": 0.5,
            "top_k": 10,
            "max_tokens": 150,
        })))
        .with_status(200)
        .with_body(sse(&[delta(json!({ "content": "False" }))]))
        .create();

    let result = analyze(
        &server,
        &["--max-thinking-tokens", "100", "--max-output-tokens", "50"],
    );

    assert_eq!(result.code, 0, "{}", result.stderr);
    mock.assert();
}

#[test]
fn the_prompt_contains_the_diff() {
    let mut server = Server::new();
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::Regex(r"<diff_content>\\n-foo\\n\+bar\\n\\n</diff_content>".into()))
        .with_status(200)
        .with_body(sse(&[delta(json!({ "content": "False" }))]))
        .create();

    let result = analyze(&server, &[]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    mock.assert();
}

#[test]
fn api_key_is_sent_as_a_bearer_token() {
    let mut server = Server::new();
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_header("authorization", "Bearer sekret")
        .with_status(200)
        .with_body(sse(&[delta(json!({ "content": "False" }))]))
        .create();

    let result = analyze(&server, &["--api-key", "sekret"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    mock.assert();
}

// ---- host handling ----

#[test]
fn host_with_a_v1_suffix_works() {
    let mut server = Server::new();
    let mock = chat(&mut server, &[delta(json!({ "content": "False" }))]);
    let host = format!("{}/v1", server.url());

    let result = run(&["--host", &host, "--model", "m"], DIFF);

    assert_eq!(result.code, 0, "{}", result.stderr);
    mock.assert();
}

#[test]
fn endpoint_selects_the_default_address() {
    let ollama = run(&["--dry-run", "--model", "m"], DIFF);
    assert!(ollama.stderr.contains("ollama at http://127.0.0.1:11434/v1/"), "{}", ollama.stderr);

    let llama = run(&["--dry-run", "--model", "m", "--endpoint", "llama-cpp"], DIFF);
    assert!(llama.stderr.contains("llama.cpp at http://127.0.0.1:8080/v1/"), "{}", llama.stderr);

    let from_env = run_with_env(
        &["--dry-run", "--model", "m"],
        &[("NIX_LOG_DIFF_ANALYZER_LLM_ENDPOINT", "llama-cpp")],
        DIFF,
    );
    assert!(from_env.stderr.contains("llama.cpp at"), "{}", from_env.stderr);
}

#[test]
fn ollama_host_variable_is_honoured_for_ollama_only() {
    let env = [("OLLAMA_HOST", "somehost")];

    let ollama = run_with_env(&["--dry-run", "--model", "m"], &env, DIFF);
    assert!(ollama.stderr.contains("http://somehost:11434/v1/"), "{}", ollama.stderr);

    let llama = run_with_env(&["--dry-run", "--model", "m", "--endpoint", "llama-cpp"], &env, DIFF);
    assert!(llama.stderr.contains("http://127.0.0.1:8080/v1/"), "{}", llama.stderr);
}

// ---- generic OpenAI-compatible servers (Gemini, OpenAI, ...) ----

/// Matches a chat request whose JSON body has no `top_k` field.
fn without_top_k(request: &mockito::Request) -> bool {
    let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
    body.get("top_k").is_none()
}

#[test]
fn the_openai_endpoint_does_not_send_top_k() {
    let mut server = Server::new();
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_request(without_top_k)
        .match_body(Matcher::PartialJson(json!({ "temperature": 0.0, "top_p": 0.5 })))
        .with_status(200)
        .with_body(sse(&[delta(json!({ "content": "False" }))]))
        .create();

    let result = analyze(&server, &["--endpoint", "openai"]);

    assert_eq!(result.code, 0, "{}", result.stderr);
    mock.assert();
}

#[test]
fn a_host_with_a_path_is_used_as_the_complete_api_base() {
    // Gemini's layout: no /v1 segment before /chat/completions.
    let mut server = Server::new();
    let mock = server
        .mock("POST", "/v1beta/openai/chat/completions")
        .match_request(without_top_k)
        .with_status(200)
        .with_body(sse(&[delta(json!({ "content": "False" }))]))
        .create();
    let host = format!("{}/v1beta/openai/", server.url());

    let result = run(&["--endpoint", "openai", "--host", &host, "--model", "m"], DIFF);

    assert_eq!(result.code, 0, "{}", result.stderr);
    mock.assert();
}

#[test]
fn models_are_listed_from_a_path_prefixed_base_and_models_prefix_is_dropped() {
    let mut server = Server::new();
    server
        .mock("GET", "/v1beta/openai/models")
        .with_status(200)
        .with_body(json!({ "data": [{ "id": "models/gemini-flash" }] }).to_string())
        .create();
    let mock = server
        .mock("POST", "/v1beta/openai/chat/completions")
        .match_body(Matcher::PartialJson(json!({ "model": "gemini-flash" })))
        .with_status(200)
        .with_body(sse(&[delta(json!({ "content": "False" }))]))
        .create();
    let host = format!("{}/v1beta/openai/", server.url());

    let result = run(&["--endpoint", "openai", "--host", &host], DIFF);

    assert_eq!(result.code, 0, "{}", result.stderr);
    mock.assert();
}

#[test]
fn the_openai_endpoint_requires_a_host() {
    let result = run(&["--endpoint", "openai", "--model", "m"], DIFF);

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("--host"), "{}", result.stderr);
}

#[test]
fn a_server_rejecting_top_k_suggests_the_openai_endpoint() {
    let mut server = Server::new();
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(400)
        .with_body(r#"{"error":{"message":"Unknown name \"top_k\": Cannot find field."}}"#)
        .create();

    let result = analyze(&server, &[]);

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("--endpoint openai"), "{}", result.stderr);
}

// ---- model selection ----

#[test]
fn a_single_available_model_is_used_automatically() {
    let mut server = Server::new();
    models(&mut server, &["only-model"]);
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(json!({ "model": "only-model" })))
        .with_status(200)
        .with_body(sse(&[delta(json!({ "content": "False" }))]))
        .create();
    let host = server.url();

    let result = run(&["--host", &host], DIFF);

    assert_eq!(result.code, 0, "{}", result.stderr);
    mock.assert();
}

#[test]
fn several_models_without_a_terminal_asks_for_the_model_flag() {
    let mut server = Server::new();
    models(&mut server, &["a", "b"]);
    let host = server.url();

    let result = run(&["--host", &host], DIFF);

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("--model"), "{}", result.stderr);
}

#[test]
fn no_models_is_an_error() {
    let mut server = Server::new();
    models(&mut server, &[]);
    let host = server.url();

    let result = run(&["--host", &host], DIFF);

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("no models"), "{}", result.stderr);
}

// ---- input handling ----

#[test]
fn dry_run_needs_no_server_and_shows_a_diff_summary() {
    let result = run(&["--dry-run", "--host", "http://127.0.0.1:9"], DIFF);

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result.stderr.contains("DRY RUN"), "{}", result.stderr);
    assert!(result.stdout.contains("diff omitted: 2 lines"), "{}", result.stdout);
    assert!(result.stdout.contains("# OUTPUT FORMAT"), "{}", result.stdout);
}

#[test]
fn a_file_argument_wins_over_stdin() {
    let path = std::env::temp_dir().join(format!("nlda-test-{}.diff", std::process::id()));
    std::fs::write(&path, "-from file\n+from file\n").unwrap();

    let result = run(&["--dry-run", "--model", "m", path.to_str().unwrap()], DIFF);
    std::fs::remove_file(&path).unwrap();

    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result.stdout.contains("diff omitted: 2 lines, 22 bytes"), "{}", result.stdout);
}

#[test]
fn empty_input_is_an_error() {
    let result = run(&["--dry-run", "--model", "m"], "");

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("Diff input is empty"), "{}", result.stderr);
}

#[test]
fn a_missing_file_is_an_error() {
    let result = run(&["--model", "m", "/nonexistent/nope.diff"], "");

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("/nonexistent/nope.diff"), "{}", result.stderr);
}

// ---- server failures ----

#[test]
fn http_errors_show_the_servers_message() {
    let mut server = Server::new();
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(404)
        .with_body(r#"{"error":{"message":"model 'nope' not found"}}"#)
        .create();

    let result = analyze(&server, &[]);

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("model 'nope' not found"), "{}", result.stderr);
}

#[test]
fn an_unreachable_server_is_an_error() {
    let result = run(&["--host", "http://127.0.0.1:9", "--model", "m"], DIFF);

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("[ERROR]"), "{}", result.stderr);
}

#[test]
fn error_chunks_in_the_stream_are_reported() {
    let mut server = Server::new();
    chat(&mut server, &[json!({ "error": { "message": "out of memory" } })]);

    let result = analyze(&server, &[]);

    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("out of memory"), "{}", result.stderr);
}
