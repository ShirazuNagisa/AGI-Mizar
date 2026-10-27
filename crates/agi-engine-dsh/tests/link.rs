//! The DSH adapter against a simulated plugin.
//!
//! `serve` takes channels of text frames rather than a socket, so the whole link
//! can be exercised without a network: the test plays the plugin, and everything
//! between the handshake and a paged history is real code.
//!
//! These are the cases that cost a live host to discover in 0.8.x — the cut that
//! comes from a stream's opening frame, the page that is read below it, the
//! session list that has to be drawn the moment a plugin attaches.

use std::sync::Arc;
use std::time::Duration;

use agi_engine::{Engine, EngineEvent};
use agi_engine_dsh::DshEngine;
use agi_protocol::domain::{RecordKind, SessionKey};
use agi_protocol::engine::EngineId;
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// One test's worth of plumbing.
struct Harness {
    engine: Arc<DshEngine>,
    /// Frames the engine wants to send to the plugin.
    to_plugin: mpsc::Receiver<String>,
    /// Frames the test sends as the plugin.
    from_plugin: mpsc::Sender<String>,
    handle: tokio::task::JoinHandle<()>,
}

fn start() -> Harness {
    let engine = DshEngine::new("tok");
    let (in_tx, in_rx) = mpsc::channel::<String>(32);
    let (out_tx, out_rx) = mpsc::channel::<String>(32);
    let serving = Arc::clone(&engine);
    let handle = tokio::spawn(async move { serving.serve(in_rx, out_tx).await });
    Harness {
        engine,
        to_plugin: out_rx,
        from_plugin: in_tx,
        handle,
    }
}

impl Harness {
    async fn send(&self, frame: Value) {
        self.from_plugin.send(frame.to_string()).await.unwrap();
    }

    async fn hello(&self) {
        self.send(json!({
            "v": 1, "type": "hello", "token": "tok",
            "plugin": { "name": "dsh-mizar", "version": "0.9.0", "linkVersion": 1 },
            "dsh": { "platform": "darwin", "home": "/tmp", "node": "v22" },
            "endpoints": ["session/list", "session/follow", "session/page"],
            "services": ["workspaceRegistry"]
        }))
        .await;
    }

    /// Read one frame the engine sent, failing rather than hanging.
    async fn next(&mut self) -> Value {
        let text = tokio::time::timeout(Duration::from_secs(2), self.to_plugin.recv())
            .await
            .expect("the engine sent nothing")
            .expect("the engine closed its side");
        serde_json::from_str(&text).expect("the engine sent invalid JSON")
    }

    /// Answer the `session/list` call the engine makes on attach.
    async fn answer_session_list(&mut self, items: Value) {
        let frame = self.next().await;
        assert_eq!(
            frame["type"], "invoke",
            "expected the attach-time list call"
        );
        assert_eq!(frame["endpoint"], "session/list");
        let id = frame["id"].as_str().unwrap().to_string();
        self.send(json!({
            "v": 1, "type": "result", "id": id, "ok": true,
            "value": { "items": items }
        }))
        .await;
    }

    /// Answer the next `session/list`, tolerating the projection reads the
    /// background enrichment sends while we wait.
    async fn answer_next_list(&mut self, items: Value) {
        loop {
            let frame = self.next().await;
            if frame["type"] != "invoke" {
                continue;
            }
            let endpoint = frame["endpoint"].as_str().unwrap_or_default();
            let id = frame["id"].as_str().unwrap_or_default().to_string();
            match endpoint {
                "session/list" => {
                    self.send(json!({
                        "v": 1, "type": "result", "id": id, "ok": true,
                        "value": { "items": items }
                    }))
                    .await;
                    return;
                }
                "session/projections" => {
                    self.send(json!({
                        "v": 1, "type": "result", "id": id, "ok": true,
                        "value": { "values": {} }
                    }))
                    .await;
                }
                _ => {}
            }
        }
    }

    /// Read frames until the next call to `endpoint`, ignoring cancels.
    async fn next_invoke(&mut self, endpoint: &str) -> Value {
        loop {
            let frame = self.next().await;
            if frame["type"] == "invoke" && frame["endpoint"] == endpoint {
                return frame;
            }
            // A `cancel` for the stream just opened is expected: the opening
            // frame is read, then the subscription is dropped.
            assert_ne!(
                frame["type"], "result",
                "unexpected answer while waiting for {endpoint}"
            );
        }
    }
}

#[tokio::test]
async fn completes_the_handshake_and_reports_connected() {
    let mut h = start();
    h.hello().await;

    let welcome = h.next().await;
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["linkVersion"], 1);
    assert!(h.engine.connected());

    h.answer_session_list(json!([])).await;
    h.handle.abort();
}

#[tokio::test]
async fn refuses_a_hello_with_the_wrong_token() {
    let h = start();
    h.send(json!({
        "v": 1, "type": "hello", "token": "wrong",
        "plugin": { "name": "dsh-mizar", "version": "0.9.0", "linkVersion": 1 },
        "dsh": { "platform": "darwin", "home": "/tmp", "node": "v22" },
        "endpoints": [], "services": []
    }))
    .await;

    // The connection ends without a welcome and without ever reporting up.
    let _ = tokio::time::timeout(Duration::from_secs(1), h.handle).await;
    assert!(!h.engine.connected());
}

#[tokio::test]
async fn draws_the_session_list_the_moment_a_plugin_attaches() {
    let mut h = start();
    // Subscribed before the handshake: the list is read as part of attaching, so
    // a subscriber that arrives afterwards misses the only interesting event.
    let mut events = h.engine.subscribe();
    h.hello().await;
    assert_eq!(h.next().await["type"], "welcome");

    h.answer_session_list(json!([
        { "sessionId": "s1", "updatedAt": 1_700_000_000_000i64, "running": false, "title": "First" }
    ]))
    .await;

    loop {
        match tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
            Ok(Ok(EngineEvent::SessionUpdated(session))) => {
                assert_eq!(session.key, SessionKey::new(EngineId::Dsh, "s1"));
                assert_eq!(session.title.as_deref(), Some("First"));
                break;
            }
            Ok(Ok(_)) => continue,
            _ => panic!("the attach-time session list was never folded in"),
        }
    }
    h.handle.abort();
}

#[tokio::test]
async fn projects_a_forwarded_event_into_a_record() {
    let mut h = start();
    h.hello().await;
    assert_eq!(h.next().await["type"], "welcome");
    h.answer_session_list(json!([
        { "sessionId": "s1", "updatedAt": 1i64, "running": false }
    ]))
    .await;

    let mut events = h.engine.subscribe();

    h.send(json!({
        "v": 1, "type": "event", "seq": 1, "at": "2026-10-27T00:00:00Z",
        "sessionId": "s1", "name": "session/event",
        "payload": {
            "header": { "origin": "session" },
            "event": {
                "type": "user/message", "seq": 7, "time": 1_700_000_000_000i64,
                "data": { "turn": 1, "content": [{ "type": "text", "text": "hello" }] }
            }
        }
    }))
    .await;

    let mut saw_record = false;
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
            Ok(Ok(EngineEvent::RecordAppended { record, .. })) => {
                assert_eq!(record.kind, RecordKind::UserMessage);
                assert_eq!(record.text.as_deref(), Some("hello"));
                assert_eq!(record.seq, 7);
                saw_record = true;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(saw_record, "no record was projected");

    let key = SessionKey::new(EngineId::Dsh, "s1");
    let records = h.engine.records(&key, 50, None).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].text.as_deref(), Some("hello"));
    h.handle.abort();
}

#[tokio::test]
async fn reads_a_history_page_from_follow_then_page() {
    let mut h = start();
    h.hello().await;
    assert_eq!(h.next().await["type"], "welcome");
    h.answer_session_list(json!([
        { "sessionId": "s2", "updatedAt": 1i64, "running": false }
    ]))
    .await;

    let engine = Arc::clone(&h.engine);
    let key = SessionKey::new(EngineId::Dsh, "s2");
    let reading = tokio::spawn(async move { engine.records(&key, 50, None).await });

    // 1. The log cut comes from the opening frame of a follow stream.
    let follow = h.next_invoke("session/follow").await;
    assert_eq!(follow["mode"], "stream");
    let follow_id = follow["id"].as_str().unwrap().to_string();
    h.send(json!({
        "v": 1, "type": "stream.item", "id": follow_id,
        "value": { "type": "baseline", "cursor": 42 }
    }))
    .await;
    h.send(json!({ "v": 1, "type": "stream.end", "id": follow_id, "reason": "done" }))
        .await;

    // 2. The page is read below that cut.
    let page = h.next_invoke("session/page").await;
    assert_eq!(page["args"]["request"]["throughSeq"], 42);
    let page_id = page["id"].as_str().unwrap().to_string();
    h.send(json!({
        "v": 1, "type": "result", "id": page_id, "ok": true,
        "value": { "records": [
            { "type": "user/message", "seq": 1, "time": 10,
              "data": { "turn": 1, "content": [{ "type": "text", "text": "older" }] } },
            { "type": "assistant/message", "seq": 2, "time": 11,
              "data": { "turn": 1, "step": 1, "message": { "content": [{ "type": "text", "text": "reply" }] } } }
        ] }
    }))
    .await;

    let records = reading.await.unwrap().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].text.as_deref(), Some("older"));
    assert_eq!(records[1].kind, RecordKind::AssistantMessage);
    h.handle.abort();
}

#[tokio::test]
async fn usage_history_pages_every_session_once_and_reuses_the_fold() {
    let mut h = start();
    let mut events = h.engine.subscribe();
    h.hello().await;
    assert_eq!(h.next().await["type"], "welcome");
    h.answer_session_list(json!([
        { "sessionId": "s1", "updatedAt": 2_000i64, "running": false, "model": "deepseek-v4" },
        { "sessionId": "s2", "updatedAt": 1_000i64, "running": false, "model": "deepseek-v4" }
    ]))
    .await;

    // Both sessions are folded before the fold starts: a fold that begins
    // before the attach-time list landed would ask the host for the list again.
    let mut listed = 0;
    while listed < 2 {
        if let Ok(Ok(EngineEvent::SessionUpdated(_))) =
            tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        {
            listed += 1;
        }
    }

    let engine = Arc::clone(&h.engine);
    let folding = tokio::spawn(async move { engine.usage_history().await });
    // The fold lists sessions itself, so that a fold started before the attach
    // list landed still sees every session.
    h.answer_next_list(json!([
        { "sessionId": "s1", "updatedAt": 2_000i64, "running": false, "model": "deepseek-v4" },
        { "sessionId": "s2", "updatedAt": 1_000i64, "running": false, "model": "deepseek-v4" }
    ]))
    .await;

    // Two sessions, so two follows and two pages, newest session first.
    let pages = [
        json!([
            { "type": "user/message", "seq": 1, "time": 1_700_000_000_000i64,
              "data": { "turn": 1, "content": [{ "type": "text", "text": "hello" }] } },
            { "type": "tool/call", "seq": 2, "time": 1_700_000_001_000i64,
              "data": { "turn": 1, "step": 1, "callId": "c1", "name": "bash" } },
            { "type": "assistant/message", "seq": 3, "time": 1_700_000_002_000i64,
              "data": { "turn": 1, "step": 1,
                        "message": { "content": [{ "type": "text", "text": "hi" }],
                                     "source": { "kind": "model", "provider": "deepseek-account",
                                                 "model": "deepseek-flash" } },
                        "usage": { "inputTokens": 100, "outputTokens": 10,
                                   "cacheReadTokens": 50, "cacheWriteTokens": 0 } } }
        ]),
        json!([
            { "type": "user/message", "seq": 1, "time": 1_700_000_003_000i64,
              "data": { "turn": 1, "source": { "kind": "runtime-context" },
                        "content": [{ "type": "text", "text": "injected" }] } },
            { "type": "user/message", "seq": 2, "time": 1_700_000_004_000i64,
              "data": { "turn": 1, "content": [{ "type": "text", "text": "second" }] } }
        ]),
    ];
    for records in pages {
        let follow = h.next_invoke("session/follow").await;
        let follow_id = follow["id"].as_str().unwrap().to_string();
        h.send(json!({
            "v": 1, "type": "stream.item", "id": follow_id,
            "value": { "type": "baseline", "cursor": 42 }
        }))
        .await;
        h.send(json!({ "v": 1, "type": "stream.end", "id": follow_id, "reason": "done" }))
            .await;

        let page = h.next_invoke("session/page").await;
        let page_id = page["id"].as_str().unwrap().to_string();
        // The shape the host's `session/page` descriptor answers with: each
        // record wrapped in an event, which is what a live page looks like.
        let wrapped: Vec<Value> = records
            .as_array()
            .unwrap()
            .iter()
            .map(|event| json!({ "type": "event", "event": event }))
            .collect();
        h.send(json!({
            "v": 1, "type": "result", "id": page_id, "ok": true,
            "value": { "records": wrapped, "hasMore": false }
        }))
        .await;
    }

    let days = folding.await.unwrap().unwrap();
    assert_eq!(days.len(), 1);
    // The tool call is not a message, the injected context is not a person's.
    assert_eq!(days[0].messages, 3);
    assert_eq!(days[0].hours.iter().sum::<u64>(), 3);
    // The record's own `data.usage` lands on the day: billed input is
    // uncached + written + read, the sum the host's projection reports.
    assert_eq!(days[0].input_tokens, 150);
    assert_eq!(days[0].output_tokens, 10);
    // The record's own model wins over the one the session list named; the
    // prompts, which name none, keep the session's.
    assert_eq!(days[0].models.len(), 2);
    let answered = days[0]
        .models
        .iter()
        .find(|entry| entry.model == "deepseek-flash")
        .expect("the record's own model");
    assert_eq!(answered.input_tokens, 150);
    assert_eq!(answered.messages, 1);
    let prompted = days[0]
        .models
        .iter()
        .find(|entry| entry.model == "deepseek-v4")
        .expect("the session's model");
    assert_eq!(prompted.messages, 2);
    assert_eq!(prompted.input_tokens, 0);

    // A second ask inside the cache window pages nothing again.
    let again = h.engine.usage_history().await.unwrap();
    assert_eq!(
        serde_json::to_value(&again).unwrap(),
        serde_json::to_value(&days).unwrap()
    );
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while let Ok(Some(text)) = tokio::time::timeout_at(deadline, h.to_plugin.recv()).await {
        let frame: Value = serde_json::from_str(&text).expect("invalid JSON");
        let endpoint = frame["endpoint"].as_str().unwrap_or_default();
        assert!(
            endpoint != "session/follow" && endpoint != "session/page",
            "the fold was not reused: {frame}"
        );
    }
    h.handle.abort();
}

#[tokio::test]
async fn usage_history_pages_backwards_through_a_long_session() {
    let mut h = start();
    let mut events = h.engine.subscribe();
    h.hello().await;
    assert_eq!(h.next().await["type"], "welcome");
    h.answer_session_list(json!([
        { "sessionId": "long", "updatedAt": 1i64, "running": false, "model": "deepseek-v4" }
    ]))
    .await;
    // The attach-time list has to be folded in before the fold starts, or the
    // engine would ask the host for the list again mid-test.
    while let Ok(received) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
        if matches!(received, Ok(EngineEvent::SessionUpdated(_))) {
            break;
        }
    }

    let engine = Arc::clone(&h.engine);
    let folding = tokio::spawn(async move { engine.usage_history().await });
    // The fold lists sessions itself; answer that call, then the pages.
    h.answer_next_list(json!([
        { "sessionId": "long", "updatedAt": 1i64, "running": false, "model": "deepseek-v4" }
    ]))
    .await;

    // One full page, so the engine must ask for the one below it, then a short
    // page, which is the start of the history.
    let full: Vec<Value> = (1..=500i64)
        .map(|index| {
            json!({
                "type": "event",
                "event": {
                    "type": "assistant/message", "seq": index, "time": 1_700_000_000_000i64,
                    "data": { "turn": 1, "step": index,
                              "message": { "content": [{ "type": "text", "text": "step" }],
                                           "source": { "kind": "model", "model": "deepseek-flash" } },
                              "usage": { "inputTokens": 2, "outputTokens": 1 } }
                }
            })
        })
        .collect();
    let mut pages = vec![full];
    pages.push(vec![json!({
        "type": "event",
        "event": {
            "type": "assistant/message", "seq": 3, "time": 1_600_000_000_000i64,
            "data": { "turn": 0, "step": 0,
                      "message": { "content": [{ "type": "text", "text": "older" }] },
                      "usage": { "inputTokens": 7, "outputTokens": 3 } }
        }
    })]);

    let mut cursors = Vec::new();
    for records in pages {
        let follow = h.next_invoke("session/follow").await;
        let follow_id = follow["id"].as_str().unwrap().to_string();
        h.send(json!({
            "v": 1, "type": "stream.item", "id": follow_id,
            "value": { "type": "baseline", "cursor": 5_000 }
        }))
        .await;
        h.send(json!({ "v": 1, "type": "stream.end", "id": follow_id, "reason": "done" }))
            .await;

        let page = h.next_invoke("session/page").await;
        cursors.push(page["args"]["request"]["beforeSeq"].clone());
        let page_id = page["id"].as_str().unwrap().to_string();
        h.send(json!({
            "v": 1, "type": "result", "id": page_id, "ok": true,
            "value": { "records": records, "hasMore": true }
        }))
        .await;
    }

    let days = folding.await.unwrap().unwrap();
    // The first page asked from the cut, the second from the oldest seq it got.
    assert!(cursors[0].is_null(), "the first page is read from the cut");
    assert_eq!(cursors[1], 1);

    let input: u64 = days.iter().map(|day| day.input_tokens).sum();
    let output: u64 = days.iter().map(|day| day.output_tokens).sum();
    let messages: u64 = days.iter().map(|day| day.messages).sum();
    assert_eq!(input, 500 * 2 + 7);
    assert_eq!(output, 500 + 3);
    assert_eq!(messages, 501);
    assert_eq!(days.len(), 2, "the older page belongs to an earlier day");
    h.handle.abort();
}

#[tokio::test]
async fn a_vanished_plugin_withdraws_what_it_was_waiting_on() {
    let mut h = start();
    h.hello().await;
    assert_eq!(h.next().await["type"], "welcome");
    h.answer_session_list(json!([])).await;

    let mut events = h.engine.subscribe();
    h.send(json!({
        "v": 1, "type": "approval", "id": "a1",
        "request": { "toolName": "bash", "agent": { "id": "s1" }, "reason": "needs a shell" }
    }))
    .await;

    // Wait for the request to be announced, then drop the plugin's side.
    loop {
        match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
            Ok(Ok(EngineEvent::InteractionRequested(interaction))) => {
                assert_eq!(interaction.id, "a1");
                assert_eq!(interaction.title, "bash");
                break;
            }
            Ok(Ok(_)) => continue,
            _ => panic!("the approval was never announced"),
        }
    }

    drop(h.from_plugin);
    let _ = tokio::time::timeout(Duration::from_secs(1), h.handle).await;

    let mut resolved = false;
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
            Ok(Ok(EngineEvent::InteractionResolved { id })) => {
                assert_eq!(id, "a1");
                resolved = true;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(resolved, "a dropped plugin left an approval outstanding");
    assert!(!h.engine.connected());
}
