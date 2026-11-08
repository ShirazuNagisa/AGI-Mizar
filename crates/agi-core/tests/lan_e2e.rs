//! The manager's LAN route against a real Noise listener and a scripted Mizar host.
//!
//! The target is not `agi-mizar` (that would make the core tests depend on the
//! server crate); it speaks the same frames by hand so the test covers the
//! manager, the LAN client, the certificate check and the turn polling together.

use std::sync::Arc;
use std::time::Duration;

use agi_core::remote::{PeerRef, RemoteManager, RemoteState};
use agi_lan::{
    public_key_b64, CertificateBody, DeviceCertificate, DeviceIdentity, LanCredentials,
    LanListener, LanTrust, FORMAT_VERSION,
};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use serde_json::{json, Value};

fn credentials(ca: &SigningKey, account: &str, device: &str) -> LanCredentials {
    let identity = Arc::new(DeviceIdentity::generate());
    let public = identity.public();
    let now = agi_lan::now_ms();
    let body = CertificateBody {
        v: FORMAT_VERSION,
        account_id: account.to_string(),
        device_id: device.to_string(),
        ed25519_public: public_key_b64(public.ed25519),
        x25519_public: public_key_b64(public.x25519),
        issued_at: now,
        expires_at: now + 86_400_000,
    };
    LanCredentials {
        identity,
        certificate: DeviceCertificate::sign(body, ca),
        trust: LanTrust {
            ca_public: ca.verifying_key().to_bytes(),
            account_id: account.to_string(),
        },
    }
}

async fn recv(session: &agi_lan::LanSession) -> Option<Value> {
    let bytes = session.recv().await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn target(listener: LanListener) {
    let session = listener.accept().await.unwrap();
    session
        .send(
            json!({ "v": 1, "type": "lan.session", "token": "lan-token" })
                .to_string()
                .as_bytes(),
        )
        .await
        .unwrap();
    let hello = recv(&session).await.expect("hello");
    assert_eq!(hello["type"], "hello");
    session
        .send(
            json!({ "v": 1, "type": "welcome", "id": "hello" })
                .to_string()
                .as_bytes(),
        )
        .await
        .unwrap();

    loop {
        let Some(frame) = recv(&session).await else {
            return;
        };
        let id = frame["id"].as_str().unwrap_or_default().to_string();
        let answer = match frame["type"].as_str().unwrap_or_default() {
            "session.create" => {
                json!({ "v": 1, "type": "ok", "id": id, "result": { "sessionId": "dsh:lan-1" } })
            }
            "session.prompt" => json!({
                "v": 1, "type": "ok", "id": id,
                "result": { "accepted": true, "requestId": "r1" }
            }),
            "interactions.pending" => json!({
                "v": 1, "type": "ok", "id": id,
                "result": { "approvals": [], "questions": [] }
            }),
            "session.messages" => {
                let after = frame["payload"]["afterSeq"].as_i64();
                let records = if after == Some(0) {
                    json!([
                        { "type": "event", "event": { "type": "turn/start", "data": { "turn": 1 } } },
                        { "type": "event", "event": {
                            "type": "assistant/message",
                            "data": { "turn": 1, "message": { "role": "assistant", "content": [
                                { "type": "text", "text": "LAN-E2E" }
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
                    "result": { "sessionId": "dsh:lan-1", "asOfSeq": if after == Some(0) { 3 } else { 0 }, "records": records }
                })
            }
            "session.cancel" | "session.archive" => {
                json!({ "v": 1, "type": "ok", "id": id, "result": { "accepted": true } })
            }
            other => json!({
                "v": 1, "type": "error", "id": id, "code": "NOT_IMPLEMENTED",
                "message": format!("fake target does not implement {other}")
            }),
        };
        if session.send(answer.to_string().as_bytes()).await.is_err() {
            return;
        }
    }
}

#[tokio::test]
async fn the_manager_runs_a_lan_turn_and_returns_its_final_text() {
    let ca = SigningKey::generate(&mut OsRng);
    let listener = LanListener::bind(0, credentials(&ca, "acct", "target"))
        .await
        .unwrap();
    let address = listener.dial_addr().unwrap();
    let server = tokio::spawn(target(listener));

    let manager = RemoteManager::new();
    let record = manager
        .start_lan(
            PeerRef {
                device_id: "target".to_string(),
                name: "Target".to_string(),
            },
            address,
            credentials(&ca, "acct", "client"),
            Some("/tmp/dsh-project".to_string()),
            "say LAN-E2E".to_string(),
        )
        .unwrap();
    assert_eq!(record.route, "lan");
    assert_eq!(record.state, RemoteState::Running);

    let id = record.id;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "LAN delegation did not settle"
        );
        let current = manager.get(&id).expect("record");
        match current.state {
            RemoteState::Done => {
                assert_eq!(current.final_text.as_deref(), Some("LAN-E2E"));
                assert_eq!(current.route, "lan");
                break;
            }
            RemoteState::Failed | RemoteState::Cancelled | RemoteState::Interrupted => {
                panic!(
                    "LAN delegation ended as {:?}: {:?}",
                    current.state, current.error
                );
            }
            RemoteState::Running => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    server.abort();
}
