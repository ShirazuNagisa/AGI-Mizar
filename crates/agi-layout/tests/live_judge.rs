//! Live smoke test: the real judge against a real `llama-server` and model.
//!
//! These are `#[ignore]`d because they need a sidecar on loopback. Run them with
//! a warm server and `MIZAR_LIVE_JUDGE_URL` pointing at it:
//!
//! ```powershell
//! # from the `AGI Mizar` repo root, using the pinned b12000 CPU build
//! $srv = "C:\llama.cpp\b12000-cpu\llama-server.exe"
//! $gguf = "C:\models\mizarllm-1.0.0-q4km.gguf"
//! Start-Process $srv -ArgumentList "-m", $gguf, "--port", "8791", "--ctx-size", "4096", "-t", "4", "--host", "127.0.0.1"
//! $env:MIZAR_LIVE_JUDGE_URL = "http://127.0.0.1:8791"
//! cargo test -p agi-layout --test live_judge -- --ignored --nocapture
//! ```
//!
//! The point is the *shape* of the request, not one particular verdict: the
//! model was trained on `system` (`teacher-prompt.md`) + `user`
//! (`render_prompt`) chat samples, so a chat-completions call must parse to a
//! state. `old_raw_completion_does_not_parse` keeps the contrast that motivated
//! this file: the pre-fix kernel posted the user message to `/completion` with no
//! system prompt and the model just continued the prompt (measured 2026-11-02 on
//! b12000 CPU/CUDA: `parse_state` was `None`, so every turn fell back to rules).

use std::time::Instant;

use agi_layout::{parse_state, render_prompt, Facts, Judge, LlamaServerJudge, State};

/// The fixture is a question the agent answered in full, so its training label
/// is `done`.
const EXPECTED: State = State::Done;

fn endpoint() -> String {
    std::env::var("MIZAR_LIVE_JUDGE_URL")
        .expect("set MIZAR_LIVE_JUDGE_URL to a warm llama-server, e.g. http://127.0.0.1:8791")
}

fn fixture() -> Facts {
    serde_json::from_str(include_str!("fixture_facts.json")).expect("fixture_facts.json parses")
}

#[tokio::test]
#[ignore = "needs a live llama-server; see the module docs"]
async fn live_chat_completions_judge_returns_a_state() {
    let judge = LlamaServerJudge {
        endpoint: endpoint(),
        api_key: None,
    };
    let started = Instant::now();
    let verdict = judge.judge(&fixture()).await;
    let elapsed = started.elapsed();

    println!("live chat judge -> {verdict:?} in {elapsed:?}");
    assert!(
        verdict.is_some(),
        "the judge must parse a state from choices[0].message.content; got {verdict:?} \
         (a None here means the wire format or the system prompt drifted)"
    );
    assert_eq!(
        verdict,
        Some(EXPECTED),
        "the fixture is a finished turn; a different state means the model or prompt changed"
    );
}

/// Diagnostic: the pre-fix request shape. Not something the kernel sends any
/// more — it is kept so the next person can re-measure why chat completions
/// replaced it instead of trusting the story.
#[tokio::test]
#[ignore = "needs a live llama-server; see the module docs"]
async fn old_raw_completion_does_not_parse() {
    let facts = fixture();
    let body = serde_json::json!({
        "prompt": render_prompt(&facts),
        "n_predict": 24,
        "temperature": 0.0,
        "stop": ["\n\n"],
        "cache_prompt": true,
    });
    let started = Instant::now();
    let response = reqwest::Client::new()
        .post(format!("{}/completion", endpoint().trim_end_matches('/')))
        .json(&body)
        .send()
        .await
        .expect("llama-server is reachable");
    let status = response.status();
    let json: serde_json::Value = response.json().await.expect("a JSON body");
    let content = json
        .get("content")
        .and_then(|content| content.as_str())
        .unwrap_or_default();

    println!(
        "old raw /completion ({status}, {:?}) content={:?} -> parse_state={:?}",
        started.elapsed(),
        content,
        parse_state(content)
    );
    assert!(
        parse_state(content).is_none(),
        "the raw /completion path unexpectedly produced a parseable state; \
         re-check whether the training/inference format mismatch still exists"
    );
}
