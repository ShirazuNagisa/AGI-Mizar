//! End-to-end-ish wire test: a fake Mizar v1 host, the real client.
//!
//! This is deliberately not an HTTP test. The client's contract is the frame
//! envelope, so a WebSocket server that speaks the same five frames proves the
//! handshake, request correlation, turn settlement and final-text extraction
//! without the relay, account service or kernel.

use std::time::Duration;

use agi_peer::{Credentials, PeerClient};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

async fn fake_host() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a listener");
    let port = listener.local_addr().expect("an address").port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a connection");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("a websocket");
        let mut prompted = false;
        while let Some(message) = socket.next().await {
            let Ok(message) = message else { break };
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let id = frame.get("id").and_then(Value::as_str).unwrap_or("x");
            let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
            let answer = match kind {
                "hello" => json!({
                    "v": 1, "type": "welcome", "hostId": "fake-host",
                    "hostName": "Fake Mac", "protocolVersion": 1, "deviceId": "fake-device"
                }),
                "session.create" => json!({
                    "v": 1, "type": "ok", "id": id,
                    "result": { "sessionId": "dsh:test-session" }
                }),
                "session.prompt" => {
                    prompted = true;
                    json!({
                        "v": 1, "type": "ok", "id": id,
                        "result": { "accepted": true, "requestId": "req-1" }
                    })
                }
                "session.messages" => {
                    let after = frame
                        .get("payload")
                        .and_then(|payload| payload.get("afterSeq"))
                        .and_then(Value::as_i64);
                    let records = if prompted && after == Some(0) {
                        json!([
                            { "type": "event", "event": { "type": "turn/start", "data": { "turn": 1 } } },
                            { "type": "event", "event": {
                                "type": "assistant/message",
                                "data": { "turn": 1, "message": { "role": "assistant", "content": [
                                    { "type": "text", "text": "P1-OK" }
                                ] } }
                            } },
                            { "type": "event", "event": {
                                "type": "turn/end",
                                "data": { "turn": 1, "reason": { "kind": "completed" } }
                            } }
                        ])
                    } else {
                        json!([])
                    };
                    json!({
                        "v": 1, "type": "ok", "id": id,
                        "result": { "sessionId": "dsh:test-session", "asOfSeq": if prompted { 3 } else { 0 }, "records": records }
                    })
                }
                "session.cancel" | "session.archive" => json!({
                    "v": 1, "type": "ok", "id": id, "result": { "accepted": true }
                }),
                other => json!({
                    "v": 1, "type": "error", "id": id, "code": "NOT_IMPLEMENTED",
                    "message": format!("fake host does not implement {other}")
                }),
            };
            if socket
                .send(Message::Text(answer.to_string()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    port
}

#[tokio::test]
async fn the_client_runs_one_turn_and_finds_its_final_text() {
    let port = fake_host().await;
    let client = PeerClient::connect(Credentials {
        base: format!("http://127.0.0.1:{port}"),
        room: "ROOM".to_string(),
        account_session: "account-session".to_string(),
        device_id: "device".to_string(),
        account_id: "account".to_string(),
        peer_token: "peer-token".to_string(),
        app_version: "test".to_string(),
    })
    .await
    .expect("the fake host accepts the hello");

    assert_eq!(client.welcome().host_name, "Fake Mac");
    let session = client.create_session(None).await.expect("a session");
    assert_eq!(session, "dsh:test-session");

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let result = client
        .run_turn(&session, "do the thing", Duration::from_secs(5), cancel_rx)
        .await
        .expect("the turn settles");
    assert_eq!(result.turn, 1);
    assert_eq!(result.reason, "completed");
    assert_eq!(result.final_text.as_deref(), Some("P1-OK"));
}
