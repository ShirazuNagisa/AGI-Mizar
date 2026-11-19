//! `session.enhancePrompt`: a one-shot model rewrite, scoped like every other
//! session method.
//!
//! The rewrite itself belongs to the engine's model service; what a connection
//! can get wrong is everything around it: whose session may be rewritten, what
//! a blank draft or a missing field answers, how the token budget is bounded,
//! and that the host does not invent a `text` when the engine failed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agi_core::Core;
use agi_engine::{Engine, EngineError, EngineEvent};
use agi_mizar::{DeviceStore, MizarServer};
use agi_protocol::domain::{Record, Session, SessionKey, Workspace};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// What this fake's model service answers.
#[derive(Clone)]
enum Answer {
    Text(String),
    Fail,
}

/// One call the host made into the fake's `enhance`.
#[derive(Debug, Clone, PartialEq)]
struct Call {
    provider: String,
    model: String,
    system: String,
    draft: String,
    max_tokens: u32,
}

struct Fake {
    id: EngineId,
    sessions: Vec<Session>,
    events: broadcast::Sender<EngineEvent>,
    answer: Answer,
    calls: Mutex<Vec<Call>>,
}

impl Fake {
    fn new(id: EngineId, answer: Answer) -> Arc<Self> {
        let (events, _) = broadcast::channel(4);
        Arc::new(Self {
            id,
            sessions: vec![Session::bare(
                SessionKey::new(id, "session-1"),
                1_700_000_000_000,
            )],
            events,
            answer,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait]
impl Engine for Fake {
    fn id(&self) -> EngineId {
        self.id
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

    async fn sessions(&self) -> Result<Vec<Session>, EngineError> {
        Ok(self.sessions.clone())
    }

    async fn records(
        &self,
        _session: &SessionKey,
        _limit: usize,
        _before: Option<i64>,
    ) -> Result<Vec<Record>, EngineError> {
        Ok(Vec::new())
    }

    async fn prompt(&self, _session: &SessionKey, _text: &str) -> Result<(), EngineError> {
        Ok(())
    }

    async fn cancel(&self, _session: &SessionKey) -> Result<(), EngineError> {
        Ok(())
    }

    async fn answer(&self, _id: &str, _answer: agi_engine::Answer) -> Result<(), EngineError> {
        Ok(())
    }

    async fn set_model(&self, _session: &SessionKey, _model: &str) -> Result<(), EngineError> {
        Ok(())
    }

    async fn set_permission(
        &self,
        _session: &SessionKey,
        _preset: &str,
    ) -> Result<(), EngineError> {
        Ok(())
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>, EngineError> {
        Ok(Vec::new())
    }

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    async fn enhance(
        &self,
        _session: &SessionKey,
        provider: &str,
        model: &str,
        system: &str,
        draft: &str,
        max_tokens: u32,
    ) -> Result<String, EngineError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Call {
                provider: provider.to_string(),
                model: model.to_string(),
                system: system.to_string(),
                draft: draft.to_string(),
                max_tokens,
            });
        match &self.answer {
            Answer::Text(text) => Ok(text.clone()),
            // The shape every adapter uses when its product is not there.
            Answer::Fail => Err(EngineError::NotConnected { engine: self.id }),
        }
    }
}

/// A server over a DSH and a Claude fake, plus a temporary device roster.
async fn start(
    answer: Answer,
) -> (
    Arc<MizarServer>,
    Arc<DeviceStore>,
    tempfile::TempDir,
    Arc<Fake>,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = DeviceStore::open(dir.path().join("devices.json"), "test-host").unwrap();
    let dsh = Fake::new(EngineId::Dsh, answer);
    let claude = Fake::new(EngineId::Claude, Answer::Text("claude".to_string()));
    let dsh_engine: Arc<dyn Engine> = Arc::clone(&dsh) as Arc<dyn Engine>;
    let claude_engine: Arc<dyn Engine> = Arc::clone(&claude) as Arc<dyn Engine>;
    let core = Core::new(vec![dsh_engine, claude_engine]);
    core.start().await;
    let server = MizarServer::new(Arc::clone(&core), Arc::clone(&store), "/tmp/dsh-home");
    server.bind(0).await.unwrap();
    (server, store, dir, dsh)
}

async fn connect(
    port: u16,
    token: &str,
    engine: Option<&str>,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    Value,
) {
    let (mut socket, _) = connect_async(format!("ws://127.0.0.1:{port}/mizar/v1/ws"))
        .await
        .unwrap();
    let mut hello = json!({
        "v": 1, "type": "hello", "id": "h1",
        "payload": { "token": token, "appVersion": "0.1.0" }
    });
    if let Some(engine) = engine {
        hello["engine"] = json!(engine);
    }
    socket.send(Message::Text(hello.to_string())).await.unwrap();
    let welcome = next(&mut socket).await;
    (socket, welcome)
}

async fn call(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    method: &str,
    payload: Value,
) -> Value {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = format!("m{}", NEXT.fetch_add(1, Ordering::Relaxed));
    socket
        .send(Message::Text(
            json!({ "v": 1, "type": method, "id": id, "payload": payload }).to_string(),
        ))
        .await
        .unwrap();
    loop {
        let value = next(socket).await;
        if value.get("id").and_then(Value::as_str) == Some(&id) {
            return value;
        }
    }
}

async fn next(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the host sent nothing")
        .expect("the socket closed")
        .unwrap();
    serde_json::from_str(&message.into_text().unwrap()).unwrap()
}

/// A valid payload, which each test then bends.
fn good_payload() -> Value {
    json!({
        "sessionId": "dsh:session-1",
        "provider": "deepseek-account",
        "model": "deepseek-v4",
        "system": "Rewrite the draft as one sentence.",
        "draft": "hey can u fix this",
        "maxTokens": 512,
    })
}

#[tokio::test]
async fn the_rewrite_comes_back_as_text() {
    let (server, store, _dir, dsh) = start(Answer::Text("Could you fix this?".to_string())).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    // The words arrive with the padding a phone keyboard leaves behind.
    let mut payload = good_payload();
    payload["system"] = json!("  Rewrite the draft as one sentence.  ");
    payload["draft"] = json!("  hey can u fix this  ");
    let answer = call(&mut socket, "session.enhancePrompt", payload).await;
    assert_eq!(answer["type"], "ok", "{answer}");
    // Text out, and nothing else: no session id, no engine, no echo of the draft.
    assert_eq!(answer["result"], json!({ "text": "Could you fix this?" }));
    assert_eq!(answer["result"].as_object().unwrap().len(), 1);

    assert_eq!(
        dsh.calls(),
        vec![Call {
            provider: "deepseek-account".to_string(),
            model: "deepseek-v4".to_string(),
            system: "Rewrite the draft as one sentence.".to_string(),
            draft: "hey can u fix this".to_string(),
            max_tokens: 512,
        }]
    );
}

#[tokio::test]
async fn blank_or_missing_fields_are_refused_before_the_model_is_called() {
    let (server, store, _dir, dsh) = start(Answer::Text("unused".to_string())).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let mut cases = Vec::new();
    for field in ["provider", "model", "system", "draft"] {
        let mut missing = good_payload();
        missing.as_object_mut().unwrap().remove(field);
        cases.push(missing);
        let mut blank = good_payload();
        blank[field] = json!("   ");
        cases.push(blank);
    }

    for payload in cases {
        let answer = call(&mut socket, "session.enhancePrompt", payload.clone()).await;
        assert_eq!(answer["type"], "error", "{payload} → {answer}");
        assert_eq!(answer["code"], "MALFORMED_FRAME", "{payload} → {answer}");
    }
    // A rewrite that was never asked for costs nothing.
    assert!(dsh.calls().is_empty());
}

#[tokio::test]
async fn the_token_budget_is_bounded_by_the_host() {
    let (server, store, _dir, dsh) = start(Answer::Text("ok".to_string())).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    for (asked, expected) in [(5u64, 64u32), (1000, 1000), (100_000, 2048)] {
        let mut payload = good_payload();
        payload["maxTokens"] = json!(asked);
        let answer = call(&mut socket, "session.enhancePrompt", payload).await;
        assert_eq!(answer["type"], "ok", "{answer}");
        let calls = dsh.calls();
        assert_eq!(
            calls.last().expect("the model was called").max_tokens,
            expected,
            "maxTokens {asked} was not clamped"
        );
    }

    // Absent is the app's default, not zero and not the cap.
    let mut payload = good_payload();
    payload.as_object_mut().unwrap().remove("maxTokens");
    let answer = call(&mut socket, "session.enhancePrompt", payload).await;
    assert_eq!(answer["type"], "ok", "{answer}");
    assert_eq!(
        dsh.calls().last().expect("the model was called").max_tokens,
        1024
    );
}

#[tokio::test]
async fn a_scoped_phone_cannot_enhance_another_agents_session() {
    let (server, store, _dir, dsh) = start(Answer::Text("unused".to_string())).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, Some("claude")).await;

    // Out of scope answers exactly like "no session id": probing must not tell
    // the phone that this computer also runs another agent.
    let answer = call(&mut socket, "session.enhancePrompt", good_payload()).await;
    assert_eq!(answer["type"], "error", "{answer}");
    assert_eq!(answer["code"], "MALFORMED_FRAME");
    assert!(dsh.calls().is_empty());
}

#[tokio::test]
async fn an_engine_failure_is_a_protocol_error_not_empty_text() {
    let (server, store, _dir, _dsh) = start(Answer::Fail).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let answer = call(&mut socket, "session.enhancePrompt", good_payload()).await;
    assert_eq!(answer["type"], "error", "{answer}");
    // The engine's NOT_CONNECTED maps onto the protocol's remote failure, which
    // is what the app shows as "电脑端模型不可用"; it must not look like success.
    assert_eq!(answer["code"], "REMOTE_ERROR");
    assert!(
        answer.get("result").is_none_or(Value::is_null),
        "a failure carried a result: {answer}"
    );
}

#[tokio::test]
async fn the_capability_is_advertised() {
    let (server, store, _dir, _dsh) = start(Answer::Text("unused".to_string())).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let info = call(&mut socket, "host.info", json!({})).await;
    let capabilities = info["result"]["plugin"]["capabilities"]
        .as_array()
        .expect("host.info has a capability list");
    assert!(
        capabilities
            .iter()
            .any(|cap| cap == "session.promptEnhance.v1"),
        "{info}"
    );
}
