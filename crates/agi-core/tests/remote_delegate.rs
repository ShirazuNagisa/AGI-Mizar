//! P1 acceptance: the real kernel commands over a fake account relay.
//!
//! This is deliberately one integration test binary. `Core::new` reads
//! `AGI_ACCOUNT_URL` and `MIZAR_DATA_DIR` from the process environment, so a
//! second test in the same binary would race the first; one test keeps the
//! fake relay and the temp data dir unambiguous.
//!
//! What it proves: `RemotePeers` reads the account's desktops, `RemoteDelegate`
//! mints a peer token through `/connect`, the outbound client completes a DSH
//! turn against a fake Mizar host, and `RemoteResult` returns the final text.

use std::sync::Arc;
use std::time::Duration;

use agi_core::Core;
use agi_protocol::api::{Command, CommandResponse};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Seen {
    connected: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn desktops() -> Json<Value> {
    Json(json!({
        "desktops": [{
            "deviceId": "peer-1",
            "name": "Peer Mac",
            "platform": "macos",
            "online": true,
            "room": "ROOM"
        }]
    }))
}

async fn me() -> Json<Value> {
    Json(json!({
        "user": {
            "id": "account-1", "username": "tester", "email": null,
            "plan": "paid", "paidUntil": 1_800_000_000_000i64,
            "devices": [{
                "id": "this-device", "name": "This Mac", "model": "Mac",
                "platform": "macos", "loginAt": 1, "lastSeenAt": 2, "current": true
            }]
        }
    }))
}

async fn connect(State(seen): State<Seen>, Json(body): Json<Value>) -> Json<Value> {
    seen.connected.lock().unwrap().push(format!(
        "connect {} {}",
        body["deviceId"].as_str().unwrap_or("?"),
        body["kind"].as_str().unwrap_or("")
    ));
    Json(json!({ "room": "ROOM", "token": "peer-token" }))
}

async fn ws(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(fake_peer)
}

async fn fake_peer(mut socket: WebSocket) {
    let mut prompted = false;
    while let Some(Ok(message)) = socket.recv().await {
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
                "v": 1, "type": "welcome", "hostId": "peer-host",
                "hostName": "Peer Mac", "protocolVersion": 1, "deviceId": "peer-device"
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
                    "result": {
                        "sessionId": "dsh:test-session",
                        "asOfSeq": if prompted { 3 } else { 0 },
                        "records": records
                    }
                })
            }
            "session.cancel" | "session.archive" => json!({
                "v": 1, "type": "ok", "id": id, "result": { "accepted": true }
            }),
            other => json!({
                "v": 1, "type": "error", "id": id, "code": "NOT_IMPLEMENTED",
                "message": format!("fake peer does not implement {other}")
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
}

async fn fake_account() -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/account/v1/desktops", get(desktops))
        .route("/account/v1/me", get(me))
        .route("/account/v1/connect", post(connect))
        .route("/r/:room/mizar/v1/ws", get(ws))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), seen)
}

fn ok(response: CommandResponse) -> Value {
    assert!(response.ok, "command failed: {:?}", response.error);
    response.value.expect("a value")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_delegation_runs_through_the_kernel_commands() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MIZAR_DATA_DIR", dir.path());
    let (base, seen) = fake_account().await;
    std::env::set_var("AGI_ACCOUNT_URL", &base);

    let account_file = dir.path().join("account.json");
    std::fs::write(
        &account_file,
        serde_json::to_vec(&json!({
            "token": "account-session",
            "device": {
                "id": "this-device",
                "name": "This Mac",
                "model": "Mac",
                "platform": "macos",
                "appVersion": "test"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let core = Core::new(Vec::new());
    core.load_account(account_file);
    core.load_remote(dir.path().join("delegations.json"));

    let peers = ok(core.dispatch(Command::RemotePeers).await);
    assert_eq!(peers["thisDeviceId"], "this-device");
    assert_eq!(peers["peers"][0]["deviceId"], "peer-1");
    assert_eq!(peers["peers"][0]["online"], true);

    let started = ok(core
        .dispatch(Command::RemoteDelegate {
            peer: "peer-1".to_string(),
            cwd: None,
            prompt: "say P1-OK".to_string(),
        })
        .await);
    let id = started["delegation"]["id"]
        .as_str()
        .expect("a delegation id")
        .to_string();
    assert_eq!(
        *seen.connected.lock().unwrap(),
        vec!["connect peer-1 peer"],
        "the delegation must ask the relay for a peer token"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the delegation did not settle"
        );
        let result = ok(core
            .dispatch(Command::RemoteResult { id: id.clone() })
            .await);
        match result["delegation"]["state"].as_str().unwrap_or("") {
            "done" => {
                assert_eq!(result["finalText"], "P1-OK");
                assert_eq!(result["delegation"]["sessionId"], "dsh:test-session");
                break;
            }
            "failed" | "cancelled" | "interrupted" => {
                panic!("delegation ended as {result}");
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }

    let listed = ok(core.dispatch(Command::RemoteDelegations).await);
    assert_eq!(listed["delegations"][0]["id"], id);

    // The LAN identity is generated beside the other state and exposes only
    // public keys; a certificate from the fake relay is rejected because its
    // signature is not a real CA signature.
    let lan = ok(core.dispatch(Command::LanIdentity).await);
    assert!(lan["ed25519Public"]
        .as_str()
        .is_some_and(|key| !key.is_empty()));
    assert!(lan["x25519Public"]
        .as_str()
        .is_some_and(|key| !key.is_empty()));
    assert!(lan["certificate"].is_null());
    let certificate = core.dispatch(Command::LanCertificate).await;
    assert!(!certificate.ok);
    assert_eq!(certificate.error.unwrap().code, "LAN_CERTIFICATE_FAILED");

    // A valid certificate for exactly these keys survives a cached reload; a
    // certificate that does not name these keys is refused.
    use agi_lan::{CertificateBody, DeviceCertificate};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    let ca = SigningKey::generate(&mut OsRng);
    let now = agi_lan::now_ms();
    let body = CertificateBody {
        v: agi_lan::FORMAT_VERSION,
        account_id: "account-1".to_string(),
        device_id: "this-device".to_string(),
        ed25519_public: lan["ed25519Public"].as_str().unwrap().to_string(),
        x25519_public: lan["x25519Public"].as_str().unwrap().to_string(),
        issued_at: now,
        expires_at: now + 86_400_000,
    };
    let signed = DeviceCertificate::sign(body, &ca);
    let cached = dir.path().join("lan-certificate.json");
    std::fs::write(
        &cached,
        serde_json::to_vec(&serde_json::json!({
            "body": signed.body,
            "signature": signed.signature,
            "caPublicKey": agi_lan::public_key_b64(ca.verifying_key().to_bytes()),
        }))
        .unwrap(),
    )
    .unwrap();
    core.load_lan_certificate(cached);
    let loaded = ok(core.dispatch(Command::LanIdentity).await);
    assert!(loaded["certificate"].is_object(), "{loaded}");
    assert_eq!(loaded["certificate"]["deviceId"], "this-device");
}

#[tokio::test]
async fn a_high_risk_approval_is_refused_before_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("delegations.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!([{
            "id": "d1", "peer": "p", "peerName": "B", "state": "running",
            "promptPreview": "x", "startedAt": 1,
            "approval": { "requestId": "a1", "risk": "high", "reason": "sudo", "requestedAt": 2 }
        }]))
        .unwrap(),
    )
    .unwrap();
    let core = Core::new(Vec::new());
    core.load_remote(path);
    let answer = core
        .dispatch(Command::RemoteAnswer {
            id: "d1".to_string(),
            approval: "a1".to_string(),
            outcome: "allowed-once".to_string(),
        })
        .await;
    assert!(!answer.ok);
    assert_eq!(answer.error.unwrap().code, "HUMAN_REQUIRED");
}
