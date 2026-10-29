//! The Codex adapter against a simulated app-server.
//!
//! The transport spawns a real child and speaks the real framing, so these tests
//! exercise everything except Codex itself: the handshake, the thread list, a
//! history read, a prompt, and — the one that is easy to get wrong — answering an
//! approval, where the reply has a different shape per method and Codex is
//! waiting on it.
//!
//! The stand-in is `tests/fake-app-server.mjs`; Node is the only runtime this
//! project can assume is present besides Rust, because the DSH helper plugin is
//! one too.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agi_engine::{Answer, Engine, EngineEvent};
use agi_engine_codex::{AppServerConfig, CodexEngine};
use agi_protocol::domain::{ActivityKind, RecordKind, SessionKey};
use agi_protocol::engine::EngineId;

/// One engine wired to the stand-in, plus the file it records decisions in.
struct Harness {
    engine: Arc<CodexEngine>,
    record: PathBuf,
    _dir: tempfile::TempDir,
}

fn start() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record.jsonl");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-app-server.mjs");

    let engine = CodexEngine::new(AppServerConfig {
        binary: "node".to_string(),
        // The transport appends `app-server` after these; the stand-in ignores it.
        args: vec![
            script.to_string_lossy().to_string(),
            record.to_string_lossy().to_string(),
        ],
        cwd: None,
    });

    let supervisor = Arc::clone(&engine);
    tokio::spawn(async move { supervisor.run().await });

    Harness {
        engine,
        record,
        _dir: dir,
    }
}

/// Poll until the engine reports connected, or fail.
async fn wait_connected(engine: &Arc<CodexEngine>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if engine.status().0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the app-server never became ready: {:?}", engine.status());
}

/// Wait for the stand-in to record something containing `needle`.
///
/// `answer` returns when the frame is written, not when the child has read it;
/// reading the file once would be a race that passes on a fast machine and fails
/// in CI.
fn wait_for_record(path: &PathBuf, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = String::new();
    while Instant::now() < deadline {
        last = std::fs::read_to_string(path).unwrap_or_default();
        if last.contains(needle) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("never saw {needle:?} in {path:?}; last contents: {last}");
}

fn key() -> SessionKey {
    SessionKey::new(EngineId::Codex, "th_1")
}

#[tokio::test]
async fn completes_the_handshake_and_lists_threads() {
    let h = start();
    wait_connected(&h.engine).await;

    let sessions = h.engine.sessions().await.unwrap();
    assert_eq!(sessions.len(), 2);

    let first = sessions.iter().find(|s| s.key.id == "th_1").unwrap();
    assert_eq!(first.key.to_string(), "codex:th_1");
    assert_eq!(first.title.as_deref(), Some("Fix the parser"));
    assert_eq!(first.cwd.as_deref(), Some("/tmp/work"));
    assert_eq!(first.model.as_deref(), Some("gpt-5"));
    // Seconds in, milliseconds out.
    assert_eq!(first.updated_at, 1_700_000_000_000);
    assert!(!first.running);

    let running = sessions.iter().find(|s| s.key.id == "th_2").unwrap();
    assert!(running.running, "status.type active must read as running");
    assert_eq!(running.title.as_deref(), Some("what is this codebase"));
}

#[tokio::test]
async fn reads_a_transcript_from_turns() {
    let h = start();
    wait_connected(&h.engine).await;

    let records = h.engine.records(&key(), 50, None).await.unwrap();
    let kinds: Vec<RecordKind> = records.iter().map(|record| record.kind).collect();
    assert_eq!(
        kinds,
        vec![
            RecordKind::TurnStart,
            RecordKind::UserMessage,
            RecordKind::ToolCall,
            RecordKind::ToolResult,
            RecordKind::AssistantMessage,
            RecordKind::TurnEnd,
        ]
    );
    assert_eq!(records[1].text.as_deref(), Some("list the files"));
    assert_eq!(records[2].tool.as_ref().unwrap().name, "shell");
    assert_eq!(records[4].text.as_deref(), Some("one file"));

    // A page limited to two records returns the tail.
    let tail = h.engine.records(&key(), 2, None).await.unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[1].kind, RecordKind::TurnEnd);
}

#[tokio::test]
async fn answers_the_approval_codex_is_waiting_on() {
    let h = start();
    wait_connected(&h.engine).await;

    let mut events = h.engine.subscribe();
    h.engine.prompt(&key(), "run the build").await.unwrap();

    // Wait for the approval to be announced.
    let mut approval = None;
    for _ in 0..20 {
        match tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
            Ok(Ok(EngineEvent::InteractionRequested(interaction))) => {
                approval = Some(interaction);
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    let approval = approval.expect("Codex asked for approval and nothing was announced");
    assert_eq!(approval.title, "shell");
    assert_eq!(
        approval.session.as_ref().map(|s| s.id.as_str()),
        Some("th_1")
    );
    assert_eq!(
        approval.detail.as_deref(),
        Some("wants to run rm -rf build")
    );

    h.engine
        .answer(
            &approval.id,
            Answer {
                outcome: Some("allowed-once".to_string()),
                answers: None,
            },
        )
        .await
        .unwrap();

    // The stand-in records what it received; the decision must be Codex's
    // vocabulary, not ours.
    wait_for_record(&h.record, r#""answered":{"decision":"accept"}"#);
}

#[tokio::test]
async fn refuses_an_approval_it_does_not_know() {
    let h = start();
    wait_connected(&h.engine).await;

    let error = h
        .engine
        .answer(
            "cx-999",
            Answer {
                outcome: Some("allowed-once".to_string()),
                answers: None,
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no pending approval"));
}

#[tokio::test]
async fn switches_model_and_permission_through_settings() {
    let h = start();
    wait_connected(&h.engine).await;

    h.engine.set_model(&key(), "gpt-5-mini").await.unwrap();
    h.engine.set_permission(&key(), "workspace").await.unwrap();

    wait_for_record(&h.record, r#""model":"gpt-5-mini""#);
    // Codex names a profile with a leading colon.
    wait_for_record(&h.record, r#""permissions":":workspace""#);
}

#[tokio::test]
async fn reads_the_catalog_the_app_server_volunteers() {
    let h = start();
    wait_connected(&h.engine).await;

    let models = h.engine.models().await.unwrap();
    assert_eq!(models[0].id, "deepseek-flash");
    assert_eq!(models[0].reasoning, vec!["max".to_string()]);

    let presets = h.engine.permissions().await.unwrap();
    assert_eq!(presets[0].id, ":read-only");
    assert_eq!(presets[0].label, "read only");
}

#[tokio::test]
async fn starts_renames_archives_and_locates_a_thread() {
    let h = start();
    wait_connected(&h.engine).await;

    let created = h.engine.create(Some("/tmp/new")).await.unwrap();
    assert_eq!(created.raw(), "th_new");
    wait_for_record(&h.record, r#""started":{"cwd":"/tmp/new"}"#);

    h.engine.rename(&created, "A name").await.unwrap();
    let renamed = wait_for_record(&h.record, r#""renamed""#);
    assert!(renamed.contains(r#""threadId":"th_new""#));
    assert!(renamed.contains(r#""name":"A name""#));

    h.engine.archive(&created, false).await.unwrap();
    wait_for_record(&h.record, r#""archived":{"threadId":"th_new"}"#);

    assert_eq!(
        h.engine.session_cwd(&created).await.unwrap(),
        "/Users/example/project"
    );
}

#[tokio::test]
async fn describes_the_latest_turn_from_its_records() {
    let h = start();
    wait_connected(&h.engine).await;

    let entries = h.engine.activity(&key(), 20).await.unwrap();
    assert!(!entries.is_empty());
    assert!(entries
        .iter()
        .any(|entry| entry.kind == ActivityKind::Command));
    assert!(entries.iter().all(|entry| entry.turn.is_some()));
}

#[tokio::test]
async fn reports_the_turns_and_steps_of_a_thread() {
    let h = start();
    wait_connected(&h.engine).await;

    let stats = h.engine.stats(&key()).await.unwrap();
    assert_eq!(stats.turns, 1);
    assert!(stats.steps >= 2, "the turn has a command and a reply");
}
