//! `session.workspaceGit`: the composer's Git pill, read-only and scoped.
//!
//! The numbers themselves are pinned by the unit tests beside `git.rs`; these
//! tests put the method behind a real pairing and a real WebSocket, because the
//! things this method can get wrong beyond counting are the answers only a
//! connection can see: which session it may read, what `null` means, and that no
//! path from the host is echoed back.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agi_core::Core;
use agi_engine::{Engine, EngineEvent};
use agi_mizar::{DeviceStore, MizarServer};
use agi_protocol::domain::{Record, Session, SessionKey, Workspace};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// One agent with one session, whose working directory the test chooses.
struct Fake {
    id: EngineId,
    sessions: Vec<Session>,
    events: broadcast::Sender<EngineEvent>,
}

impl Fake {
    fn new(id: EngineId, cwd: Option<&Path>) -> Arc<Self> {
        let (events, _) = broadcast::channel(4);
        let mut session = Session::bare(SessionKey::new(id, "session-1"), 1_700_000_000_000);
        session.cwd = cwd.map(|path| path.to_string_lossy().to_string());
        Arc::new(Self {
            id,
            sessions: vec![session],
            events,
        })
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

    async fn workspaces(&self) -> Result<Vec<Workspace>, agi_engine::EngineError> {
        Ok(Vec::new())
    }

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }
}

/// A server over a DSH and a Claude session, plus a temporary device roster.
async fn start(
    dsh_cwd: Option<&Path>,
    claude_cwd: Option<&Path>,
) -> (Arc<MizarServer>, Arc<DeviceStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = DeviceStore::open(dir.path().join("devices.json"), "test-host").unwrap();
    let dsh: Arc<dyn Engine> = Fake::new(EngineId::Dsh, dsh_cwd);
    let claude: Arc<dyn Engine> = Fake::new(EngineId::Claude, claude_cwd);
    let core = Core::new(vec![dsh, claude]);
    core.start().await;
    let server = MizarServer::new(Arc::clone(&core), Arc::clone(&store), "/tmp/dsh-home");
    server.bind(0).await.unwrap();
    (server, store, dir)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .expect("git is installed");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    std::fs::write(dir.path().join("a.txt"), b"a\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "base"]);
    dir
}

/// A committed file plus an untracked one: two files, two additions.
fn dirty_repository() -> tempfile::TempDir {
    let dir = repository();
    std::fs::write(dir.path().join("a.txt"), b"a\nb\n").unwrap();
    std::fs::write(dir.path().join("u.txt"), b"u\n").unwrap();
    dir
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

#[tokio::test]
async fn the_pill_counts_the_sessions_own_uncommitted_change() {
    let repo = dirty_repository();
    let (server, store, _dir) = start(Some(repo.path()), None).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let answer = call(
        &mut socket,
        "session.workspaceGit",
        json!({ "sessionId": "dsh:session-1" }),
    )
    .await;
    assert_eq!(answer["type"], "ok", "{answer}");
    let result = &answer["result"];
    assert_eq!(result["sessionId"], "dsh:session-1");
    assert_eq!(
        result["git"],
        json!({ "files": 2, "additions": 2, "deletions": 0 })
    );
    // The answer is the three counts and the session id: a path from this
    // machine must never ride along.
    assert_eq!(result.as_object().unwrap().len(), 2);
    assert!(
        !answer.to_string().contains(repo.path().to_str().unwrap()),
        "the answer echoed the host's path: {answer}"
    );
}

#[tokio::test]
async fn a_payload_path_cannot_aim_the_pill_at_another_repository() {
    let clean = repository();
    let dirty = dirty_repository();
    let (server, store, _dir) = start(Some(clean.path()), None).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    // A client that names a repository of its own choosing still gets the
    // session's: the directory is the host's record, not a request parameter.
    let answer = call(
        &mut socket,
        "session.workspaceGit",
        json!({ "sessionId": "dsh:session-1", "path": dirty.path() }),
    )
    .await;
    assert_eq!(answer["type"], "ok", "{answer}");
    assert_eq!(
        answer["result"]["git"],
        json!({ "files": 0, "additions": 0, "deletions": 0 })
    );
}

#[tokio::test]
async fn a_scoped_phone_cannot_ask_about_another_agents_session() {
    let dsh_repo = dirty_repository();
    let claude_repo = repository();
    let (server, store, _dir) = start(Some(dsh_repo.path()), Some(claude_repo.path())).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, Some("claude")).await;

    // Out of scope answers exactly like "no session id": probing must not tell
    // the phone that this computer also runs another agent.
    let refused = call(
        &mut socket,
        "session.workspaceGit",
        json!({ "sessionId": "dsh:session-1" }),
    )
    .await;
    assert_eq!(refused["type"], "error", "{refused}");
    assert_eq!(refused["code"], "MALFORMED_FRAME");

    let own = call(
        &mut socket,
        "session.workspaceGit",
        json!({ "sessionId": "claude:session-1" }),
    )
    .await;
    assert_eq!(own["type"], "ok", "{own}");
    assert_eq!(
        own["result"]["git"],
        json!({ "files": 0, "additions": 0, "deletions": 0 })
    );
}

#[tokio::test]
async fn a_session_outside_a_repository_answers_null() {
    let plain = tempfile::tempdir().unwrap();
    let (server, store, _dir) = start(Some(plain.path()), None).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let answer = call(
        &mut socket,
        "session.workspaceGit",
        json!({ "sessionId": "dsh:session-1" }),
    )
    .await;
    assert_eq!(answer["type"], "ok", "{answer}");
    assert_eq!(answer["result"]["git"], Value::Null);
}

#[tokio::test]
async fn a_session_without_a_working_directory_answers_null() {
    let (server, store, _dir) = start(None, None).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let answer = call(
        &mut socket,
        "session.workspaceGit",
        json!({ "sessionId": "dsh:session-1" }),
    )
    .await;
    assert_eq!(answer["type"], "ok", "{answer}");
    assert_eq!(answer["result"]["git"], Value::Null);
}

#[tokio::test]
async fn the_capability_is_advertised_so_an_older_host_is_tellable() {
    let (server, store, _dir) = start(None, None).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let info = call(&mut socket, "host.info", json!({})).await;
    let capabilities = info["result"]["plugin"]["capabilities"]
        .as_array()
        .expect("host.info has a capability list");
    assert!(
        capabilities.iter().any(|cap| cap == "workspace.git.v1"),
        "{info}"
    );
}

#[tokio::test]
async fn a_second_ask_sees_a_change_made_in_between() {
    // The answer is not cached: the pill must not keep yesterday's numbers
    // because another screen asked a moment ago.
    let repo = repository();
    let (server, store, _dir) = start(Some(repo.path()), None).await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let clean = call(
        &mut socket,
        "session.workspaceGit",
        json!({ "sessionId": "dsh:session-1" }),
    )
    .await;
    assert_eq!(
        clean["result"]["git"],
        json!({ "files": 0, "additions": 0, "deletions": 0 })
    );

    std::fs::write(repo.path().join("a.txt"), b"a\nb\n").unwrap();
    let dirty = call(
        &mut socket,
        "session.workspaceGit",
        json!({ "sessionId": "dsh:session-1" }),
    )
    .await;
    assert_eq!(
        dirty["result"]["git"],
        json!({ "files": 1, "additions": 1, "deletions": 0 })
    );
}
