//! A's kernel delegates to B's kernel over LAN.
//!
//! A is a real `Core` using the LAN route added for P5. B is a real `Core` and a
//! real `MizarServer`; only the agent engine is scripted. The account service is
//! a fake loopback server, because the test must not need a relay.

use std::sync::{Arc, Mutex};

use agi_core::{Core, RemotePrefs};
use agi_engine::{Engine, EngineEvent};
use agi_lan::{
    public_key_b64, CertificateBody, DeviceCertificate, DeviceIdentity, LanBeacon, LanListener,
    LanTrust, FORMAT_VERSION,
};
use agi_mizar::{DeviceStore, MizarServer};
use agi_protocol::api::Command;
use agi_protocol::domain::{Record, RecordKind, Session, SessionKey};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use axum::routing::{get, post};
use axum::{Json, Router};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use serde_json::{json, Value};

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
    async fn prompt(&self, key: &SessionKey, _text: &str) -> Result<(), agi_engine::EngineError> {
        let mut records = self.records.lock().unwrap();
        let mut turn = self.turn.lock().unwrap();
        *turn += 1;
        let base = *turn * 3 - 2;
        for (offset, kind, text) in [
            (0, RecordKind::TurnStart, None),
            (1, RecordKind::AssistantMessage, Some("DUAL-CORE-LAN")),
            (2, RecordKind::TurnEnd, None),
        ] {
            records.push(Record {
                seq: base + offset,
                time: base + offset,
                turn: Some(*turn as u32),
                step: None,
                kind,
                text: text.map(str::to_string),
                tool: None,
            });
        }
        let _ = key;
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
        Ok(SessionKey::new(EngineId::Dsh, "dual"))
    }
}

fn credentials(ca: &SigningKey, account: &str, device: &str) -> agi_lan::LanCredentials {
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
    agi_lan::LanCredentials {
        identity,
        certificate: DeviceCertificate::sign(body, ca),
        trust: LanTrust {
            ca_public: ca.verifying_key().to_bytes(),
            account_id: account.to_string(),
        },
    }
}

async fn fake_account() -> (String, tokio::task::JoinHandle<()>) {
    async fn me() -> Json<Value> {
        Json(json!({
            "user": {
                "id": "acct", "username": "tester", "email": null,
                "plan": "free", "paidUntil": null,
                "devices": [{
                    "id": "a-device", "name": "A", "model": "Mac",
                    "platform": "macos", "loginAt": 1, "lastSeenAt": 2, "current": true
                }]
            }
        }))
    }
    async fn empty() -> Json<Value> {
        Json(json!({ "desktops": [] }))
    }
    let app = Router::new()
        .route("/account/v1/me", get(me))
        .route("/account/v1/desktops", get(empty))
        .route("/account/v1/connect", post(|| async { Json(json!({})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_delegate_uses_the_lan_route_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MIZAR_DATA_DIR", dir.path());
    let (account, account_task) = fake_account().await;
    std::env::set_var("AGI_ACCOUNT_URL", &account);

    let ca = SigningKey::generate(&mut OsRng);
    let server_credentials = credentials(&ca, "acct", "b-device");
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
        allow_lan: true,
        allow_wan: false,
        workspaces: vec!["/tmp/dsh-project".to_string()],
        blocked: Vec::new(),
    });
    let target = MizarServer::new(
        Arc::clone(&target_core),
        Arc::clone(&target_store),
        "/tmp/dsh-home",
    );
    let listener = LanListener::bind(0, server_credentials.clone())
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let accept = tokio::spawn(async move {
        loop {
            let Ok(session) = listener.accept().await else {
                return;
            };
            let remote = session.remote().clone();
            let target = Arc::clone(&target);
            tokio::spawn(async move {
                let _ = target.serve_lan(remote, session).await;
            });
        }
    });

    // A's kernel: fake account session, fresh identity, cached certificate.
    let client = Core::new(Vec::new());
    client.load_account(dir.path().join("account.json"));
    std::fs::write(
        dir.path().join("account.json"),
        serde_json::to_vec(&json!({
            "token": "account-token",
            "device": { "id": "a-device", "name": "A", "model": "Mac", "platform": "macos", "appVersion": "test" }
        }))
        .unwrap(),
    )
    .unwrap();
    // load_account happened before the file existed; reload it now.
    let client = {
        drop(client);
        let client = Core::new(Vec::new());
        client.load_account(dir.path().join("account.json"));
        client
    };
    client.start().await;
    client
        .refresh_account()
        .await
        .expect("A's fake account refresh did not land");
    assert!(
        client.account_user().is_some(),
        "A's fake account user is missing"
    );
    client.set_remote_prefs(RemotePrefs {
        enabled: true,
        allow_lan: true,
        allow_wan: false,
        workspaces: vec!["/tmp/dsh-project".to_string()],
        blocked: Vec::new(),
    });

    let lan = client.dispatch(Command::LanIdentity).await;
    assert!(lan.ok, "LanIdentity failed: {lan:?}");
    let lan = lan.value.unwrap();
    let body = CertificateBody {
        v: FORMAT_VERSION,
        account_id: "acct".to_string(),
        device_id: "a-device".to_string(),
        ed25519_public: lan["ed25519Public"].as_str().unwrap().to_string(),
        x25519_public: lan["x25519Public"].as_str().unwrap().to_string(),
        issued_at: agi_lan::now_ms(),
        expires_at: agi_lan::now_ms() + 86_400_000,
    };
    let signed = DeviceCertificate::sign(body, &ca);
    let cache = dir.path().join("lan-certificate.json");
    std::fs::write(
        &cache,
        serde_json::to_vec(&json!({
            "body": signed.body,
            "signature": signed.signature,
            "caPublicKey": public_key_b64(ca.verifying_key().to_bytes()),
        }))
        .unwrap(),
    )
    .unwrap();
    client.load_lan_certificate(cache);
    let beacon = LanBeacon::new(
        "acct",
        "b-device",
        "B",
        port,
        &server_credentials.certificate,
    );
    client.record_lan_beacon_ip(&beacon, "127.0.0.1".parse().unwrap());

    let started = client
        .dispatch(Command::RemoteDelegate {
            peer: "b-device".to_string(),
            cwd: Some("/tmp/dsh-project".to_string()),
            prompt: "return DUAL-CORE-LAN".to_string(),
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
            "LAN delegation did not settle"
        );
        let result = client
            .dispatch(Command::RemoteResult { id: id.clone() })
            .await;
        assert!(result.ok, "{result:?}");
        let value = result.value.unwrap();
        match value["delegation"]["state"].as_str().unwrap_or("") {
            "done" => {
                assert_eq!(value["finalText"], "DUAL-CORE-LAN");
                assert_eq!(value["delegation"]["route"], "lan");
                break;
            }
            "failed" | "cancelled" | "interrupted" => panic!("delegation ended: {value}"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    // A follow-up reuses the peer session instead of creating another one.
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
            assert_eq!(value["finalText"], "DUAL-CORE-LAN");
            assert_eq!(
                value["delegation"]["sessionId"],
                follow["delegation"]["sessionId"]
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    accept.abort();
    account_task.abort();
}
