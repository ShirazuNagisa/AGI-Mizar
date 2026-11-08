//! One LAN connection from Noise through to the real Mizar method dispatcher.
//!
//! The transport here is the production one: account certificate, Noise XX,
//! 19393-shaped encrypted frames, then a short-lived peer token and the same
//! `serve_peer` the WebSocket path uses. Only the engine is a fake.

use std::sync::Arc;

use agi_core::{Core, RemotePrefs};
use agi_engine::{Engine, EngineEvent};
use agi_lan::{
    public_key_b64, CertificateBody, DeviceCertificate, DeviceIdentity, LanCredentials,
    LanListener, LanTrust, FORMAT_VERSION,
};
use agi_mizar::{DeviceStore, MizarServer};
use agi_protocol::domain::{Record, Session, SessionKey};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;

struct Fake {
    events: tokio::sync::broadcast::Sender<EngineEvent>,
}

#[async_trait]
impl Engine for Fake {
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
        Ok(Vec::new())
    }
    async fn prompt(
        &self,
        _session: &SessionKey,
        _text: &str,
    ) -> Result<(), agi_engine::EngineError> {
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
        Ok(SessionKey::new(EngineId::Dsh, "lan-created"))
    }
}

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

#[tokio::test]
async fn a_lan_peer_gets_a_scoped_token_and_can_create_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let ca = SigningKey::generate(&mut OsRng);
    let store = DeviceStore::open(dir.path().join("devices.json"), "lan-host").unwrap();
    let (events, _) = tokio::sync::broadcast::channel(8);
    let engine: Arc<dyn Engine> = Arc::new(Fake { events });
    let core = Core::new(vec![engine]);
    core.start().await;
    core.set_remote_prefs(RemotePrefs {
        enabled: true,
        allow_lan: true,
        allow_wan: false,
        workspaces: vec!["/tmp/dsh-project".to_string()],
        blocked: Vec::new(),
    });
    let server = MizarServer::new(Arc::clone(&core), Arc::clone(&store), "/tmp/dsh-home");
    let listener = LanListener::bind(0, credentials(&ca, "acct", "server"))
        .await
        .unwrap();
    let address = listener.dial_addr().unwrap();
    let accept = tokio::spawn(async move {
        let session = listener.accept().await?;
        let remote = session.remote().clone();
        server.serve_lan(remote, session).await
    });

    let client_credentials = credentials(&ca, "acct", "client");
    let client = agi_lan::LanMizarClient::connect(address, &client_credentials)
        .await
        .unwrap();

    let created = client
        .create_session(Some("/tmp/dsh-project"))
        .await
        .unwrap();
    assert_eq!(created, "dsh:lan-created");

    // Outside the allowlist is refused by the same peer scope the WAN path uses.
    let refused = client.create_session(Some("/tmp/not-allowed")).await;
    assert!(
        refused
            .err()
            .is_some_and(|error| error.to_string().contains("WORKSPACE_NOT_ALLOWED")),
        "the LAN peer must honour the workspace allowlist"
    );

    // A device blocked while its socket is open is refused on the next frame.
    core.set_remote_prefs(RemotePrefs {
        enabled: true,
        allow_lan: true,
        allow_wan: false,
        workspaces: vec!["/tmp/dsh-project".to_string()],
        blocked: vec!["client".to_string()],
    });
    let blocked = client.create_session(Some("/tmp/dsh-project")).await;
    assert!(blocked.is_err(), "a blocked live device must be refused");

    drop(client);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), accept).await;
}

#[tokio::test]
async fn a_blocked_device_gets_no_lan_token_even_with_a_valid_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let ca = SigningKey::generate(&mut OsRng);
    let store = DeviceStore::open(dir.path().join("devices.json"), "lan-host").unwrap();
    let (events, _) = tokio::sync::broadcast::channel(8);
    let engine: Arc<dyn Engine> = Arc::new(Fake { events });
    let core = Core::new(vec![engine]);
    core.start().await;
    core.set_remote_prefs(RemotePrefs {
        enabled: true,
        allow_lan: true,
        allow_wan: false,
        workspaces: vec!["/tmp/dsh-project".to_string()],
        blocked: vec!["client".to_string()],
    });
    let server = MizarServer::new(Arc::clone(&core), Arc::clone(&store), "/tmp/dsh-home");
    let listener = LanListener::bind(0, credentials(&ca, "acct", "server"))
        .await
        .unwrap();
    let address = listener.dial_addr().unwrap();
    let accept = tokio::spawn(async move {
        let session = listener.accept().await?;
        let remote = session.remote().clone();
        server.serve_lan(remote, session).await
    });

    let client_credentials = credentials(&ca, "acct", "client");
    let connected = agi_lan::LanMizarClient::connect(address, &client_credentials).await;
    assert!(
        connected.is_err(),
        "a blocked device must not receive lan.session"
    );

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), accept).await;
}
