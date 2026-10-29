//! The Claude Code adapter against a temporary transcript tree.
//!
//! No child process is needed for the interesting half: a transcript is a file,
//! and a `PreToolUse` approval is a hook call that blocks until it is answered.
//! Both are exercised here exactly as the kernel drives them.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agi_engine::{Answer, Engine, EngineEvent};
use agi_engine_claude::ClaudeEngine;
use agi_protocol::domain::{RecordKind, SessionKey};
use agi_protocol::engine::EngineId;
use serde_json::{json, Value};

/// One transcript line.
fn line(value: Value) -> String {
    format!("{value}\n")
}

/// Write a transcript for `session` under `root`.
///
/// `open` omits the model's final message, which is what a turn still in progress
/// looks like on disk.
fn write_transcript(root: &Path, project: &str, session: &str, open: bool) -> std::path::PathBuf {
    let dir = root.join(project);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session}.jsonl"));

    let mut text = String::new();
    text.push_str(&line(json!({
        "type": "user",
        "timestamp": "2026-10-27T05:00:00Z",
        "sessionId": session,
        "cwd": "/tmp/work",
        "message": { "role": "user", "content": [{ "type": "text", "text": "list the files" }] }
    })));
    text.push_str(&line(json!({
        "type": "assistant",
        "timestamp": "2026-10-27T05:00:02Z",
        "message": {
            "role": "assistant",
            "model": "claude-opus-4",
            "content": [{ "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls" } }]
        }
    })));
    text.push_str(&line(json!({
        "type": "user",
        "timestamp": "2026-10-27T05:00:03Z",
        "message": {
            "role": "user",
            "content": [{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt" }]
        }
    })));
    if !open {
        text.push_str(&line(json!({
            "type": "assistant",
            "timestamp": "2026-10-27T05:00:05Z",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "one file" }] }
        })));
    }

    std::fs::write(&path, text).unwrap();
    path
}

fn engine_over(root: &Path) -> Arc<ClaudeEngine> {
    ClaudeEngine::new(root, "claude")
}

#[tokio::test]
async fn lists_sessions_from_transcripts() {
    let dir = tempfile::tempdir().unwrap();
    write_transcript(
        dir.path(),
        "-tmp-work",
        "aaaa1111-2222-3333-4444-555566667777",
        false,
    );

    let engine = engine_over(dir.path());
    let sessions = engine.sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);

    let session = &sessions[0];
    assert_eq!(
        session.key.to_string(),
        format!("claude:{}", "aaaa1111-2222-3333-4444-555566667777")
    );
    assert_eq!(session.title.as_deref(), Some("list the files"));
    assert_eq!(session.cwd.as_deref(), Some("/tmp/work"));
    assert_eq!(session.model.as_deref(), Some("claude-opus-4"));
    assert!(!session.running);
}

#[tokio::test]
async fn reads_a_transcript_into_records() {
    let dir = tempfile::tempdir().unwrap();
    write_transcript(
        dir.path(),
        "-tmp-work",
        "bbbb1111-2222-3333-4444-555566667777",
        false,
    );

    let engine = engine_over(dir.path());
    let key = SessionKey::new(EngineId::Claude, "bbbb1111-2222-3333-4444-555566667777");
    let records = engine.records(&key, 100, None).await.unwrap();

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
    assert_eq!(records[2].tool.as_ref().unwrap().name, "Bash");
    assert_eq!(
        records[3].tool.as_ref().unwrap().output.as_deref(),
        Some("a.txt")
    );
    assert_eq!(records[4].text.as_deref(), Some("one file"));
}

#[tokio::test]
async fn a_turn_stays_open_while_a_hook_says_it_is_running() {
    let dir = tempfile::tempdir().unwrap();
    let session = "cccc1111-2222-3333-4444-555566667777";
    write_transcript(dir.path(), "-tmp-work", session, true);

    let engine = engine_over(dir.path());
    let key = SessionKey::new(EngineId::Claude, session);

    // Before any hook, the file alone cannot say whether the turn is over: a
    // session that was killed mid-turn looks identical to one that is working.
    let closed = engine.records(&key, 100, None).await.unwrap();
    assert!(
        closed
            .iter()
            .any(|record| record.kind == RecordKind::TurnEnd),
        "with no hook signal the turn is treated as finished"
    );

    // `UserPromptSubmit` is the signal that a turn started.
    let answer = engine
        .handle_hook("UserPromptSubmit", &json!({ "session_id": session }))
        .await;
    assert!(answer.is_none());

    let open = engine.records(&key, 100, None).await.unwrap();
    assert!(
        open.iter().all(|record| record.kind != RecordKind::TurnEnd),
        "a running turn must not be closed"
    );

    // And `Stop` closes it again.
    engine
        .handle_hook("Stop", &json!({ "session_id": session }))
        .await;
    let finished = engine.records(&key, 100, None).await.unwrap();
    assert!(finished
        .iter()
        .any(|record| record.kind == RecordKind::TurnEnd));
}

#[tokio::test]
async fn an_unattended_hook_answers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_over(dir.path());
    // Nobody is watching: blocking would stall the operator's own terminal.
    let answer = engine
        .handle_hook("PreToolUse", &pre_tool_use("Bash"))
        .await;
    assert!(answer.is_none());
}

#[tokio::test]
async fn an_attended_hook_waits_for_the_operator() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_over(dir.path());
    engine.set_attended(true);

    let mut events = engine.subscribe();
    let hook = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .handle_hook("PreToolUse", &pre_tool_use("Bash"))
                .await
        })
    };

    // The card must appear before anything is answered.
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
    let approval = approval.expect("the approval was never announced");
    assert_eq!(approval.title, "Bash");
    assert_eq!(approval.detail.as_deref(), Some("ls -la"));

    engine
        .answer(
            &approval.id,
            Answer {
                outcome: Some("allowed-once".to_string()),
                answers: None,
            },
        )
        .await
        .unwrap();

    let answer = hook.await.unwrap().expect("the hook produced no decision");
    assert_eq!(answer["hookSpecificOutput"]["permissionDecision"], "allow");
    assert_eq!(answer["hookSpecificOutput"]["hookEventName"], "PreToolUse");
}

#[tokio::test]
async fn a_rejected_hook_denies_the_tool() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_over(dir.path());
    engine.set_attended(true);

    let mut events = engine.subscribe();
    let hook = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .handle_hook("PreToolUse", &pre_tool_use("Bash"))
                .await
        })
    };

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
    let approval = approval.expect("the approval was never announced");
    engine
        .answer(
            &approval.id,
            Answer {
                outcome: Some("rejected".to_string()),
                answers: None,
            },
        )
        .await
        .unwrap();

    let answer = hook.await.unwrap().unwrap();
    assert_eq!(answer["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[tokio::test]
async fn a_hook_outside_the_default_permission_mode_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_over(dir.path());
    engine.set_attended(true);

    // In `bypassPermissions` there is no question to relay: answering one would
    // be us granting what the operator deliberately left ungated.
    let mut payload = pre_tool_use("Bash");
    payload["permission_mode"] = json!("bypassPermissions");
    assert!(engine.handle_hook("PreToolUse", &payload).await.is_none());
}

#[tokio::test]
async fn a_hook_that_is_not_an_approval_only_refreshes() {
    let dir = tempfile::tempdir().unwrap();
    write_transcript(
        dir.path(),
        "-tmp-work",
        "dddd1111-2222-3333-4444-555566667777",
        false,
    );
    let engine = engine_over(dir.path());
    let _ = engine.sessions().await.unwrap();

    let mut events = engine.subscribe();
    let answer = engine
        .handle_hook(
            "Stop",
            &json!({ "session_id": "dddd1111-2222-3333-4444-555566667777" }),
        )
        .await;
    assert!(answer.is_none());

    // The row is marked stale so the UI redraws it.
    let mut touched = false;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
            Ok(Ok(EngineEvent::SessionUpdated(_))) => {
                touched = true;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(touched, "a Stop hook did not refresh the session row");
}

/// A `PreToolUse` payload, shaped the way Claude Code sends it.
fn pre_tool_use(tool: &str) -> Value {
    json!({
        "session_id": "eeee1111-2222-3333-4444-555566667777",
        "hook_event_name": "PreToolUse",
        "tool_name": tool,
        "tool_input": { "command": "ls -la" },
        "tool_use_id": "toolu_9",
        "permission_mode": "default"
    })
}

#[tokio::test]
async fn offers_the_cli_permission_modes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_over(dir.path());
    let modes = engine.permissions().await.unwrap();
    assert_eq!(modes.len(), 6);
    assert!(modes.iter().any(|mode| mode.id == "acceptEdits"));
}

#[tokio::test]
async fn a_created_session_knows_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_over(dir.path());

    let key = engine.create(Some("/tmp/work")).await.unwrap();
    assert_eq!(key.engine, EngineId::Claude);
    assert_eq!(engine.session_cwd(&key).await.unwrap(), "/tmp/work");
}

#[tokio::test]
async fn activity_describes_the_latest_turns_tools() {
    let dir = tempfile::tempdir().unwrap();
    write_transcript(
        dir.path(),
        "-tmp-work",
        "cccc1111-2222-3333-4444-555566667777",
        false,
    );
    let engine = engine_over(dir.path());
    let key = SessionKey::new(EngineId::Claude, "cccc1111-2222-3333-4444-555566667777");

    let entries = engine.activity(&key, 20).await.unwrap();
    assert!(!entries.is_empty());
    assert!(entries
        .iter()
        .any(|entry| entry.kind == agi_protocol::domain::ActivityKind::Command));
}
