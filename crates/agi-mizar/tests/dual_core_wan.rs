//! A's kernel delegates to B's kernel through a fake relay, over WAN-shaped
//! `/connect` + WebSocket frames. Only the relay and the engine are fake.

use std::sync::{Arc, Mutex};

use agi_core::{Core, RemotePrefs};
use agi_engine::{Engine, EngineEvent};
use agi_lan::{public_key_b64, CertificateBody, DeviceCertificate, LanBeacon, FORMAT_VERSION};
use agi_mizar::{DeviceStore, MizarServer};
use agi_protocol::api::Command;
use agi_protocol::domain::{Record, RecordKind, Session, SessionKey};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use rand::rngs::OsRng;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as TungMessage;

struct FakeTarget {
    records: Mutex<Vec<Record>>,
    turn: Mutex<i64>,
    events: tokio::sync::broadcast::Sender<EngineEvent>,
}

#[async_trait]
impl Engine for FakeTarget {
    fn id(&self) -> EngineId {
        EngineId::Dsh
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            attachments: false,
            file_preview: false,
            fork: false,
            archive: false,
            activity: false,
            balance: false,
            models: false,
            permissions: false,
        }
    }
    fn status(&self) -> (bool, Option<String>) {
        (true, None)
    }
    async fn sessions(&self) -> Result<Vec<Session>, agi_engine::EngineError> {
        Ok(Vec::new())
    }
    async fn records(
        &self,
        _session: &SessionKey,
        _limit: usize,
        _before: Option<i64>,
    ) -> Result<Vec<Record>, agi_engine::EngineError> {
        Ok(self.records.lock().unwrap().clone())
    }
    async fn prompt(
        &self,
        _session: &SessionKey,
        _text: &str,
    ) -> Result<(), agi_engine::EngineError> {
        let mut records = self.records.lock().unwrap();
        let mut turn = self.turn.lock().unwrap();
        *turn += 1;
        let base = *turn * 3 - 2;
        for (offset, kind, text) in [
            (0, RecordKind::TurnStart, None),
            (1, RecordKind::AssistantMessage, Some("WAN-DUAL")),
            (2, RecordKind::TurnEnd, None),
        ] {
            records.push(record(base + offset, *turn as u32, kind, text));
        }
        Ok(())
    }
    async fn cancel(&self, _session: &SessionKey) -> Result<(), agi_engine::EngineError> {
        Ok(())
    }
    async fn answer(
        &self,
        _id: &str,
        _answer: agi_engine::Answer,
    ) -> Result<(), agi_engine::EngineError> {
        Ok(())
    }
    async fn set_model(
        &self,
        _session: &SessionKey,
        _model: &str,
    ) -> Result<(), agi_engine::EngineError> {
        Ok(())
    }
    async fn set_permission(
        &self,
        _session: &SessionKey,
        _preset: &str,
    ) -> Result<(), agi_engine::EngineError> {
        Ok(())
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }
    async fn create(&self, _cwd: Option<&str>) -> Result<SessionKey, agi_engine::EngineError> {
        Ok(SessionKey::new(EngineId::Dsh, "wan"))
    }
}

fn record(seq: i64, turn: u32, kind: RecordKind, text: Option<&str>) -> Record {
    Record {
        seq,
        time: seq,
        turn: Some(turn),
        step: None,
        kind,
        text: text.map(str::to_string),
        tool: None,
    }
}

#[derive(Clone)]
struct Relay {
    token: String,
    target_ws: String,
}

async fn me() -> Json<Value> {
    Json(json!({
        "user": {
            "id": "acct", "username": "tester", "email": null,
            "plan": "paid", "paidUntil": 9_000_000_000_000i64,
            "devices": [{
                "id": "a-device", "name": "A", "model": "Mac",
                "platform": "macos", "loginAt": 1, "lastSeenAt": 2, "current": true
            }]
        }
    }))
}

async fn desktops() -> Json<Value> {
    Json(json!({
        "desktops": [{
            "deviceId": "b-device", "name": "B", "model": "Mac",
            "platform": "macos", "online": true, "room": "room"
        }]
    }))
}

async fn connect(State(relay): State<Relay>) -> Json<Value> {
    Json(json!({ "room": "room", "token": relay.token }))
}

async fn ws(State(relay): State<Relay>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| proxy(socket, relay.target_ws))
}

async fn proxy(client: WebSocket, target_url: String) {
    let Ok((target, _)) = tokio_tungstenite::connect_async(target_url).await else {
        return;
    };
    let (mut client_tx, mut client_rx) = client.split();
    let (mut target_tx, mut target_rx) = target.split();

    let client_to_target = async {
        while let Some(Ok(message)) = client_rx.next().await {
            let outgoing = match message {
                AxumMessage::Text(text) => TungMessage::Text(text.to_string()),
                AxumMessage::Binary(bytes) => TungMessage::Binary(bytes.to_vec()),
                AxumMessage::Ping(bytes) => TungMessage::Ping(bytes.to_vec()),
                AxumMessage::Pong(bytes) => TungMessage::Pong(bytes.to_vec()),
                AxumMessage::Close(_) => break,
            };
            if target_tx.send(outgoing).await.is_err() {
                break;
            }
        }
    };
    let target_to_client = async {
        while let Some(Ok(message)) = target_rx.next().await {
            let outgoing = match message {
                TungMessage::Text(text) => AxumMessage::Text(text),
                TungMessage::Binary(bytes) => AxumMessage::Binary(bytes),
                TungMessage::Ping(bytes) => AxumMessage::Ping(bytes),
                TungMessage::Pong(bytes) => AxumMessage::Pong(bytes),
                TungMessage::Close(_) => break,
                _ => continue,
            };
            if client_tx.send(outgoing).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = client_to_target => {}
        _ = target_to_client => {}
    }
}

async fn fake_relay(token: String, target_ws: String) -> (String, tokio::task::JoinHandle<()>) {
    let state = Relay { token, target_ws };
    let app = Router::new()
        .route("/account/v1/me", get(me))
        .route("/account/v1/desktops", get(desktops))
        .route("/account/v1/connect", axum::routing::post(connect))
        .route("/r/:room/mizar/v1/ws", get(ws))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_delegate_uses_the_wan_route_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let target_store = DeviceStore::open(dir.path().join("b-devices.json"), "b-host").unwrap();
    let (events, _) = tokio::sync::broadcast::channel(8);
    let target_engine: Arc<dyn Engine> = Arc::new(FakeTarget {
        records: Mutex::new(Vec::new()),
        turn: Mutex::new(0),
        events,
    });
    let target_core = Core::new(vec![target_engine]);
    target_core.start().await;
    target_core.set_remote_prefs(RemotePrefs {
        enabled: true,
        allow_lan: false,
        allow_wan: true,
        allow_all_workspaces: false,
        workspaces: vec!["/tmp/dsh-project".to_string()],
        blocked: Vec::new(),
    });
    let target = MizarServer::new(
        Arc::clone(&target_core),
        Arc::clone(&target_store),
        "/tmp/dsh-home",
    );
    target.bind(0).await.unwrap();
    let target_port = target.port();
    let target_ws = format!("ws://127.0.0.1:{target_port}/mizar/v1/ws");
    let token = target_store
        .issue("A", Some("peer:acct:a-device:dsh".to_string()))
        .unwrap()
        .token;
    let (base, relay_task) = fake_relay(token, target_ws).await;

    std::env::set_var("AGI_ACCOUNT_URL", &base);
    let account_file = dir.path().join("account.json");
    std::fs::write(
        &account_file,
        serde_json::to_vec(&json!({
            "token": "account-token",
            "device": { "id": "a-device", "name": "A", "model": "Mac", "platform": "macos", "appVersion": "test" }
        }))
        .unwrap(),
    )
    .unwrap();
    let client = Core::new(Vec::new());
    client.load_account(account_file);
    client.start().await;
    client.refresh_account().await.expect("fake account");
    client.set_remote_prefs(RemotePrefs {
        enabled: true,
        allow_lan: true,
        allow_wan: true,
        allow_all_workspaces: false,
        workspaces: vec!["/tmp/dsh-project".to_string()],
        blocked: Vec::new(),
    });
    // A fresh beacon with a valid certificate but a dead listener: the
    // preflight handshake fails and the delegation must still fall through.
    let identity = client.dispatch(Command::LanIdentity).await;
    assert!(identity.ok, "LanIdentity failed: {identity:?}");
    let identity = identity.value.unwrap();
    let ca = SigningKey::generate(&mut OsRng);
    let now = agi_lan::now_ms();
    let body = CertificateBody {
        v: FORMAT_VERSION,
        account_id: "acct".to_string(),
        device_id: "a-device".to_string(),
        ed25519_public: identity["ed25519Public"].as_str().unwrap().to_string(),
        x25519_public: identity["x25519Public"].as_str().unwrap().to_string(),
        issued_at: now,
        expires_at: now + 86_400_000,
    };
    let certificate = DeviceCertificate::sign(body, &ca);
    let cache = dir.path().join("lan-certificate.json");
    std::fs::write(
        &cache,
        serde_json::to_vec(&json!({
            "body": certificate.body,
            "signature": certificate.signature,
            "caPublicKey": public_key_b64(ca.verifying_key().to_bytes()),
        }))
        .unwrap(),
    )
    .unwrap();
    client.load_lan_certificate(cache);
    let beacon = LanBeacon::new("acct", "b-device", "B", 9, &certificate);
    client.record_lan_beacon_ip(&beacon, "127.0.0.1".parse().unwrap());

    let started = client
        .dispatch(Command::RemoteDelegate {
            peer: "b-device".to_string(),
            permission: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            cwd: Some("/tmp/dsh-project".to_string()),
            prompt: "return WAN-DUAL".to_string(),
        })
        .await;
    assert!(started.ok, "RemoteDelegate failed: {started:?}");
    let id = started.value.as_ref().unwrap()["delegation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "WAN delegation did not settle"
        );
        let result = client
            .dispatch(Command::RemoteResult { id: id.clone() })
            .await;
        assert!(result.ok, "{result:?}");
        let value = result.value.unwrap();
        match value["delegation"]["state"].as_str().unwrap_or("") {
            "done" => {
                assert_eq!(value["finalText"], "WAN-DUAL");
                assert_eq!(value["delegation"]["route"], "wan");
                break;
            }
            "failed" | "cancelled" | "interrupted" => panic!("delegation ended: {value}"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    let follow = client
        .dispatch(Command::RemoteSend {
            id: id.clone(),
            message: "again".to_string(),
        })
        .await;
    assert!(follow.ok, "RemoteSend failed: {follow:?}");
    let follow = follow.value.unwrap();
    let next_id = follow["delegation"]["id"].as_str().unwrap().to_string();
    assert_eq!(follow["continuedFrom"].as_str().unwrap(), id);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "follow-up did not settle"
        );
        let result = client
            .dispatch(Command::RemoteResult {
                id: next_id.clone(),
            })
            .await;
        let value = result.value.unwrap();
        if value["delegation"]["state"] == "done" {
            assert_eq!(value["finalText"], "WAN-DUAL");
            assert_eq!(
                value["delegation"]["sessionId"],
                follow["delegation"]["sessionId"]
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    relay_task.abort();
}
