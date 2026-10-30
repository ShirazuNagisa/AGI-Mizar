//! One computer, three agents, and a phone that asked for one of them.
//!
//! The reported shape: a phone connected for DSH listed DSH's five workspaces
//! and, under them, two folders that belong to Codex — drawn as empty headings,
//! because the sessions in them had been filtered out and the folders had not.
//! `workspace.list` merged all three adapters while `session.list` honoured the
//! scope, so one screen was built from two different answers.
//!
//! These tests put three fake engines behind a real server, a real pairing and a
//! real WebSocket, and check what each kind of connection can see. Nothing is
//! mocked on the transport: the failures this guards against were in the frames,
//! not in the adapters.

use std::sync::Arc;
use std::time::Duration;

use agi_core::Core;
use agi_engine::{Engine, EngineEvent};
use agi_mizar::{DeviceStore, MizarServer};
use agi_protocol::domain::{Interaction, Session, SessionKey, Workspace};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// One agent that answers from memory.
///
/// It has sessions and one workspace, which is all the phone-facing methods
/// under test read. Everything else is the trait's own refusal, so a test that
/// starts depending on a real adapter's behaviour fails instead of quietly
/// passing.
struct Fake {
    id: EngineId,
    workspace: Workspace,
    sessions: Vec<Session>,
    events: broadcast::Sender<EngineEvent>,
}

impl Fake {
    fn new(id: EngineId) -> Arc<Self> {
        let (events, _) = broadcast::channel(16);
        let id_text = id.as_str();
        let workspace = Workspace {
            id: format!("ws-{id_text}"),
            path: format!("/tmp/{id_text}-project"),
            name: Some(format!("{id_text} project")),
            session_ids: Some(vec![format!("{id_text}:session-1")]),
        };
        let mut session = Session::bare(SessionKey::new(id, "session-1"), 1_700_000_000_000);
        session.cwd = Some(workspace.path.clone());
        session.title = Some(format!("{id_text} conversation"));
        session.turns = 1;
        Arc::new(Self {
            id,
            workspace,
            sessions: vec![session],
            events,
        })
    }

    /// Raise an approval the phone has to answer.
    fn ask(&self, interaction: Interaction) {
        // A full channel is a test bug, not a runtime condition.
        self.events
            .send(EngineEvent::InteractionRequested(interaction))
            .ok();
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

    async fn sessions(&self) -> Result<Vec<Session>, agi_engine::EngineError> {
        Ok(self.sessions.clone())
    }

    async fn records(
        &self,
        _session: &SessionKey,
        _limit: usize,
        _before: Option<i64>,
    ) -> Result<Vec<agi_protocol::domain::Record>, agi_engine::EngineError> {
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

    async fn workspaces(&self) -> Result<Vec<Workspace>, agi_engine::EngineError> {
        Ok(vec![self.workspace.clone()])
    }

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }
}

/// A server over three fake agents and a temporary roster.
async fn start() -> (
    Arc<MizarServer>,
    Arc<DeviceStore>,
    Arc<Core>,
    Vec<Arc<Fake>>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = DeviceStore::open(dir.path().join("devices.json"), "test-host").unwrap();
    let fakes: Vec<Arc<Fake>> = EngineId::all().into_iter().map(Fake::new).collect();
    let engines: Vec<Arc<dyn Engine>> = fakes
        .iter()
        .map(|fake| Arc::clone(fake) as Arc<dyn Engine>)
        .collect();
    let core = Core::new(engines);
    core.start().await;
    let server = MizarServer::new(Arc::clone(&core), Arc::clone(&store), "/tmp/dsh-home");
    server.bind(0).await.unwrap();
    (server, store, core, fakes, dir)
}

/// One HTTP request, reading exactly the declared body.
async fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    loop {
        let read = stream.read(&mut chunk).await.unwrap();
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if header_end.is_none() {
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = Some(position + 4);
            }
        }
        if let Some(end) = header_end {
            let head = String::from_utf8_lossy(&buffer[..end]).to_string();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())?
                })
                .unwrap_or(0);
            if buffer.len() >= end + length {
                let status = head
                    .split_whitespace()
                    .nth(1)
                    .and_then(|code| code.parse().ok())
                    .unwrap_or(0);
                let body = String::from_utf8_lossy(&buffer[end..end + length]).to_string();
                return (status, body);
            }
        }
    }
    (0, String::new())
}

/// The phone measures the link with `ping`, so the host has to answer it --
/// and answer it with a clock, which is all the phone reads.
#[tokio::test]
async fn ping_is_answered_with_the_hosts_clock() {
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let value = call(&mut socket, "ping", json!({})).await;
    assert_eq!(value["type"], "ok");
    let time = value["result"]["time"].as_f64().unwrap();
    // Milliseconds since the epoch, so somewhere past 2020 and not in the
    // future by more than a minute.
    assert!(time > 1_577_836_800_000.0, "time was {time}");
}

#[tokio::test]
async fn host_info_names_the_agents_the_computer_serves() {
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let value = call(&mut socket, "host.info", json!({})).await;
    let engines = value["result"]["engines"].as_array().unwrap();
    let ids: Vec<&str> = engines
        .iter()
        .map(|engine| engine["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["dsh", "claude", "codex"]);
    // A phone that did not choose is told so, rather than being handed a filter
    // it never asked for.
    assert_eq!(value["result"]["engine"], Value::Null);
}

#[tokio::test]
async fn an_unscoped_phone_still_sees_every_agent() {
    // The older contract: a token with no marker is "the owner's phone", and it
    // saw all three lists before there was a choice to make.
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let sessions = call(&mut socket, "session.list", json!({})).await;
    assert_eq!(
        sessions["result"]["items"].as_array().unwrap().len(),
        3,
        "an unscoped connection lists every agent: {sessions}"
    );

    let workspaces = call(&mut socket, "workspace.list", json!({})).await;
    let rows = workspaces["result"]["items"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "one workspace per agent: {workspaces}");
    assert_eq!(workspaces["result"]["complete"], true);
}

#[tokio::test]
async fn a_token_scoped_to_one_agent_sees_only_that_agent() {
    // What the account flow mints: `account:<account>:<phone>:<engine>`.
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store
        .issue("iPhone", Some("account:acct-1:phone-1:dsh".to_string()))
        .unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let sessions = call(&mut socket, "session.list", json!({})).await;
    let items = sessions["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "only DSH's session: {sessions}");
    assert!(items[0]["sessionId"].as_str().unwrap().starts_with("dsh:"));

    // The bug this file exists for: two folders from two other agents, drawn
    // under DSH's as empty headings.
    let workspaces = call(&mut socket, "workspace.list", json!({})).await;
    let rows = workspaces["result"]["items"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only DSH's workspace: {workspaces}");
    assert_eq!(rows[0]["workspaceId"], "dsh:ws-dsh");
    assert_eq!(rows[0]["engine"], "dsh");
    assert_eq!(rows[0]["title"], "dsh project");
    assert_eq!(rows[0]["sessionIds"], json!(["dsh:session-1"]));
    assert_eq!(workspaces["result"]["complete"], true);
}

#[tokio::test]
async fn a_paired_phone_can_ask_for_an_agent_on_the_socket() {
    // A phone that paired by scanning a code has a plain token. It names the
    // agent in `hello` because there is no relay brokering a scoped one.
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, Some("claude")).await;

    let sessions = call(&mut socket, "session.list", json!({})).await;
    let items = sessions["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "only Claude's session: {sessions}");
    assert!(items[0]["sessionId"]
        .as_str()
        .unwrap()
        .starts_with("claude:"));

    let workspaces = call(&mut socket, "workspace.list", json!({})).await;
    let rows = workspaces["result"]["items"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["engine"], "claude");

    // And the connection says which agent it ended up bound to.
    let info = call(&mut socket, "host.info", json!({})).await;
    assert_eq!(info["result"]["engine"], "claude");
}

#[tokio::test]
async fn a_scoped_token_cannot_be_widened_by_the_hello() {
    // The promise is about what leaves the machine: a phone holding a DSH-scoped
    // token must not reach Claude by asking for it at handshake time.
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store
        .issue("iPhone", Some("account:acct-1:phone-1:dsh".to_string()))
        .unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, Some("claude")).await;

    let sessions = call(&mut socket, "session.list", json!({})).await;
    let items = sessions["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "the token's choice wins: {sessions}");
    assert!(items[0]["sessionId"].as_str().unwrap().starts_with("dsh:"));
}

#[tokio::test]
async fn another_agents_session_cannot_be_read_by_naming_it() {
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store
        .issue("iPhone", Some("account:acct-1:phone-1:dsh".to_string()))
        .unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    // A session that exists on this machine, and an id the phone could have
    // read off an older, unscoped connection.
    let refused = call(
        &mut socket,
        "session.messages",
        json!({ "sessionId": "codex:session-1", "maxMessages": 10 }),
    )
    .await;
    assert_eq!(
        refused["type"], "error",
        "another agent's transcript must not be served: {refused}"
    );

    // The same call for its own agent is answered.
    let served = call(
        &mut socket,
        "session.messages",
        json!({ "sessionId": "dsh:session-1", "maxMessages": 10 }),
    )
    .await;
    assert_eq!(served["type"], "ok", "own agent stays readable: {served}");
}

#[tokio::test]
async fn another_agents_approval_is_not_listed_nor_answerable() {
    let (server, store, core, fakes, _dir) = start().await;
    for (index, fake) in fakes.iter().enumerate() {
        fake.ask(Interaction {
            id: format!("approval-{index}"),
            session: Some(SessionKey::new(fake.id, "session-1")),
            kind: agi_protocol::domain::InteractionKind::Approval,
            title: "bash".to_string(),
            detail: Some(format!("from {}", fake.id.as_str())),
            requested_at: 1,
            questions: Vec::new(),
        });
    }
    // The kernel ingests engine events on its own task.
    for _ in 0..50 {
        if core.snapshot().interactions.len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        core.snapshot().interactions.len(),
        3,
        "the fakes never landed"
    );

    let issued = store
        .issue("iPhone", Some("account:acct-1:phone-1:dsh".to_string()))
        .unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let pending = call(&mut socket, "interactions.pending", json!({})).await;
    let approvals = pending["result"]["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1, "only DSH's approval: {pending}");
    assert_eq!(approvals[0]["sessionId"], "dsh:session-1");

    // Answering one that was never listed is refused, so an approval cannot
    // land on the wrong agent's turn.
    let refused = call(
        &mut socket,
        "approval.answer",
        json!({ "requestId": "approval-1", "outcome": "rejected" }),
    )
    .await;
    assert_eq!(
        refused["type"], "error",
        "another agent's approval must not be answerable: {refused}"
    );
}

/// Open the phone socket and complete the handshake, optionally asking for an
/// agent.
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

/// One call, answered.
async fn call(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    method: &str,
    payload: Value,
) -> Value {
    socket
        .send(Message::Text(
            json!({ "v": 1, "type": method, "id": "m1", "payload": payload }).to_string(),
        ))
        .await
        .unwrap();
    next(socket).await
}

/// Read one frame.
async fn next(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .expect("the host sent nothing")
        .expect("the socket closed")
        .unwrap();
    serde_json::from_str(&message.into_text().unwrap()).unwrap()
}

#[tokio::test]
async fn the_hello_probe_still_answers_for_a_phone_that_has_not_paired() {
    // The picker reads this before it can open a socket, so it must keep
    // answering without a token.
    let (server, _store, _core, _fakes, _dir) = start().await;
    let (status, body) = http(server.port(), "GET", "/mizar/v1/hello", "").await;
    assert_eq!(status, 200);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["hostName"], "test-host");
}
