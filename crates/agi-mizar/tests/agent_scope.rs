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
use agi_protocol::domain::{
    ContextPressure, Interaction, Record, RecordKind, Session, SessionKey, Workspace,
};
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
    /// The transcript every session has: three assistant messages, seq 1..3.
    ///
    /// Small on purpose. It is enough to prove a delta read returns only what is
    /// newer than the position the phone named, which is the difference between
    /// an incremental sync costing one record and costing a whole page.
    records: Vec<Record>,
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
        // The two fields `session.list` now carries so the phone does not have
        // to ask `session.stats` once per row to draw the model and the context
        // ring. Values are recognisable rather than realistic.
        session.model = Some(format!("{id_text}-model"));
        session.context = Some(ContextPressure {
            window: 128_000,
            used: 32_000,
            percent: 25.0,
        });
        let records = (1..=3)
            .map(|seq| Record {
                seq,
                time: seq * 1_000,
                turn: Some(1),
                step: Some(1),
                kind: RecordKind::AssistantMessage,
                text: Some(format!("answer {seq}")),
                tool: None,
            })
            .collect();
        Arc::new(Self {
            id,
            workspace,
            sessions: vec![session],
            records,
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

    /// Tell the kernel a turn happened, the way an adapter does.
    fn record(&self, seq: i64, turn: u32, kind: RecordKind, text: Option<&str>) {
        self.events
            .send(EngineEvent::RecordAppended {
                session: SessionKey::new(self.id, "session-1"),
                record: Record {
                    seq,
                    time: seq * 1_000,
                    turn: Some(turn),
                    step: None,
                    kind,
                    text: text.map(str::to_string),
                    tool: None,
                },
            })
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

    async fn create(&self, _cwd: Option<&str>) -> Result<SessionKey, agi_engine::EngineError> {
        Ok(SessionKey::new(self.id, "peer-created"))
    }

    async fn records(
        &self,
        _session: &SessionKey,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<agi_protocol::domain::Record>, agi_engine::EngineError> {
        let mut kept: Vec<Record> = self
            .records
            .iter()
            .filter(|record| before.is_none_or(|cut| record.seq < cut))
            .cloned()
            .collect();
        let start = kept.len().saturating_sub(limit);
        Ok(kept.split_off(start))
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
async fn a_session_row_carries_the_title_model_and_context_ring() {
    // The phone's history sync used to fetch every row's title, model and
    // context with one `session.stats` request each -- 342 sessions meant 342
    // round trips over the relay for three fields the host already had. They
    // ride on the list now, so this pins all three (and that a field the host
    // cannot say stays `null` rather than becoming a made-up zero).
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let sessions = call(&mut socket, "session.list", json!({})).await;
    let items = sessions["result"]["items"].as_array().unwrap();
    let dsh = items
        .iter()
        .find(|row| row["sessionId"] == "dsh:session-1")
        .expect("DSH's session is listed");
    assert_eq!(dsh["title"], "dsh conversation");
    assert_eq!(dsh["model"], "dsh-model");
    assert_eq!(dsh["context"]["window"], 128_000);
    // `pressureTokens`, not the domain type's `used`: this is the same spelling
    // `session.messages` and `session.stats` already put on the wire, and the
    // phone's `ContextPressure` decodes it.
    assert_eq!(dsh["context"]["pressureTokens"], 32_000);
    assert_eq!(dsh["context"]["percent"], 25);
}

#[tokio::test]
async fn session_sync_answers_a_batch_and_names_what_it_left_out() {
    // The phone's history sync asks one frame for a window of sessions. What
    // has to hold: the row's `updatedAt` plus a numeric `afterSeq` is enough to
    // answer `unchanged` without a read, a batch that is full names the rest in
    // `remaining` instead of dropping them, and a page comes back in the same
    // shape `session.messages` uses (so the phone's merge code is one code path,
    // not two).
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let sync = call(
        &mut socket,
        "session.sync",
        json!({
            "maxItems": 1,
            "items": [
                {"sessionId": "dsh:session-1", "updatedAt": 1_700_000_000_000_i64, "afterSeq": 10},
                {"sessionId": "claude:session-1", "updatedAt": 0, "afterSeq": 0},
                {"sessionId": "codex:session-1", "updatedAt": 0, "afterSeq": 0},
            ]
        }),
    )
    .await;
    let items = sync["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "the batch was capped at one item: {sync}");
    assert_eq!(items[0]["sessionId"], "dsh:session-1");
    assert_eq!(items[0]["unchanged"], true);
    let remaining: Vec<&str> = sync["result"]["remaining"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(remaining, vec!["claude:session-1", "codex:session-1"]);

    // A second round asks for the two that were left; each comes back as a page.
    let sync = call(
        &mut socket,
        "session.sync",
        json!({
            "items": [
                {"sessionId": "claude:session-1", "updatedAt": 0, "afterSeq": 0},
                {"sessionId": "codex:session-1", "updatedAt": 0, "afterSeq": 0},
            ]
        }),
    )
    .await;
    let items = sync["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "both pages ride in one frame: {sync}");
    for item in items {
        assert!(item["messages"].is_object(), "no page: {item}");
        assert_eq!(
            item["messages"]["records"].as_array().map(Vec::len),
            Some(3),
            "the fake transcript has three records: {item}"
        );
        // `session.sync` deliberately carries no stats: the title and model ride
        // on `session.list`, and a stats call per session is the plugin work this
        // method exists to remove.
        assert!(item.get("stats").is_none(), "a batch must not pay for stats: {item}");
    }
    assert_eq!(sync["result"]["remaining"], json!([]));
}

#[tokio::test]
async fn session_sync_reports_one_scoped_session_without_losing_the_rest() {
    // A batch is also a place where scope can be widened by accident: asking for
    // another agent's session must fail *that item* and leave the others alone.
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store
        .issue("iPhone", Some("account:acct-1:phone-1:dsh".to_string()))
        .unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let sync = call(
        &mut socket,
        "session.sync",
        json!({
            "items": [
                {"sessionId": "dsh:session-1", "updatedAt": 0, "afterSeq": 0},
                {"sessionId": "claude:session-1", "updatedAt": 0, "afterSeq": 0},
            ]
        }),
    )
    .await;
    let items = sync["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items[0]["messages"].is_object(), "DSH's page: {sync}");
    assert_eq!(items[1]["sessionId"], "claude:session-1");
    assert_eq!(items[1]["error"]["code"], "MALFORMED_FRAME");
    assert!(items[1].get("messages").is_none());
}

#[tokio::test]
async fn a_delta_read_returns_only_what_is_new() {
    // `afterSeq` is the phone saying what it already holds. Ignoring it made
    // every incremental sync of a changed session pay for the newest page to
    // receive the one record that was new -- and, worse, made `hasMore` claim
    // older history on every such read.
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    // The fake session has records 1, 2 and 3.
    let full = call(
        &mut socket,
        "session.messages",
        json!({ "sessionId": "dsh:session-1", "maxMessages": 200 }),
    )
    .await;
    assert_eq!(full["result"]["records"].as_array().unwrap().len(), 3);
    assert_eq!(full["result"]["asOfSeq"], 3);

    let delta = call(
        &mut socket,
        "session.messages",
        json!({ "sessionId": "dsh:session-1", "maxMessages": 200, "afterSeq": 2 }),
    )
    .await;
    let records = delta["result"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 1, "only seq 3 is new: {delta}");
    assert_eq!(records[0]["event"]["seq"], 3);
    // The log cut still moves: a delta with something new must not make the
    // phone's stored `throughSeq` go backwards.
    assert_eq!(delta["result"]["asOfSeq"], 3);
    // A delta does not claim older history; the phone's own backfill tracks it.
    assert_eq!(delta["result"]["hasMore"], false);

    // Nothing new is an empty page, not an error, and the cut survives it.
    let empty = call(
        &mut socket,
        "session.messages",
        json!({ "sessionId": "dsh:session-1", "maxMessages": 200, "afterSeq": 3 }),
    )
    .await;
    assert_eq!(empty["result"]["records"].as_array().unwrap().len(), 0);
    assert_eq!(empty["result"]["asOfSeq"], 3);

    // Explicit `null` is "I hold nothing" (a reinstall), not "no claim".
    let all = call(
        &mut socket,
        "session.messages",
        json!({ "sessionId": "dsh:session-1", "maxMessages": 200, "afterSeq": null }),
    )
    .await;
    assert_eq!(all["result"]["records"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn a_phone_that_registers_live_activity_is_marked_reachable() {
    // A backgrounded phone has no socket, but a running session still reaches
    // it through its lock screen. The roster records the capability when the
    // phone registers a live-activity token, and the window pairs it with
    // "something is running" before it draws the lamp.
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store
        .issue("iPhone", Some("iPhone17,1".to_string()))
        .unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let answer = call(
        &mut socket,
        "push.register",
        json!({
            "token": "push-token",
            "platform": "ios",
            "liveActivityToken": "live-token",
        }),
    )
    .await;
    assert_eq!(answer["type"], "ok", "registration was not accepted: {answer}");
    let views = agi_core::DeviceStatusSource::device_views(&*server);
    assert!(
        views.iter().any(|view| view.live_activity),
        "the registration must reach the roster: {views:?}"
    );

    let answer = call(&mut socket, "push.unregister", json!({})).await;
    assert_eq!(answer["type"], "ok", "{answer}");
    let views = agi_core::DeviceStatusSource::device_views(&*server);
    assert!(
        views.iter().all(|view| !view.live_activity),
        "unregistering must put the lamp out: {views:?}"
    );
}

#[tokio::test]
async fn two_phones_with_the_same_name_show_their_models() {
    // One iPhone and one iPad both named "iPhone" used to be two identical
    // rows. The hardware is what tells them apart, and it is only worth
    // showing when the name alone cannot.
    let (server, store, _core, _fakes, _dir) = start().await;
    let first = store.issue("iPhone", Some("account:acct:phone-1:dsh".to_string())).unwrap();
    store.describe(&first.device.id, "iPhone17,1").unwrap();
    let second = store.issue("iPhone", Some("account:acct:phone-2:dsh".to_string())).unwrap();
    store.describe(&second.device.id, "iPhone18,1").unwrap();

    let views = agi_core::DeviceStatusSource::device_views(&*server);
    let names: Vec<&str> = views.iter().map(|view| view.name.as_str()).collect();
    assert_eq!(names, vec!["iPhone (iPhone17,1)", "iPhone (iPhone18,1)"], "{views:?}");
}

#[tokio::test]
async fn a_generic_name_is_corrected_by_the_hardware() {
    // Before the app reported the idiom, an iPad paired as "iPhone". The name
    // is what the person reads, so the model has to be able to overrule the
    // generic name -- but not a name the person chose.
    let (server, store, _core, _fakes, _dir) = start().await;
    let ipad = store.issue("iPhone", Some("account:acct:phone-1:dsh".to_string())).unwrap();
    store.describe(&ipad.device.id, "iPad13,18").unwrap();
    let named = store.issue("Studio iPad", Some("account:acct:phone-2:dsh".to_string())).unwrap();
    store.describe(&named.device.id, "iPhone17,1").unwrap();

    let views = agi_core::DeviceStatusSource::device_views(&*server);
    let names: Vec<&str> = views.iter().map(|view| view.name.as_str()).collect();
    assert_eq!(names, vec!["iPad", "Studio iPad"], "{views:?}");
}

#[tokio::test]
async fn the_same_phone_paired_twice_merges_before_it_reconnects() {
    // The iPhone was paired locally months ago and through its account later.
    // The account record only learns the hardware when the phone next says
    // hello; until then the local record's model is the only thing that knows
    // this is one phone, and the list must not show it twice.
    let (server, store, _core, _fakes, _dir) = start().await;
    store.issue("iPhone", Some("iPhone17,1".to_string())).unwrap();
    let account = store
        .issue("iPhone", Some("account:acct:phone-1:dsh".to_string()))
        .unwrap();

    let views = agi_core::DeviceStatusSource::device_views(&*server);
    assert_eq!(views.len(), 1, "{views:?}");
    assert_eq!(views[0].name, "iPhone");
    assert_eq!(views[0].model.as_deref(), Some("iPhone17,1"));
    assert_eq!(views[0].id, account.device.id, "the account record stays canonical");
}

#[tokio::test]
async fn the_device_list_shows_phones_not_this_computer() {
    // The tray listed every live record in the roster: the computer's own
    // account device (named after the Mac), the same iPhone twice (paired
    // locally once, then through its account), and an iPad stuck as "Phone"
    // because the relay that minted its token had no name for it. The list
    // should be one row per phone, named for what the phone says it is.
    let (server, store, _core, _fakes, _dir) = start().await;
    let host_name = store.host_name();
    // Signing in provisions a device for this computer itself.
    store
        .issue(&host_name, Some("account:acct:self:dsh".to_string()))
        .unwrap();
    // The same iPhone, first paired locally, then through its account.
    store.issue("iPhone", Some("iPhone17,1".to_string())).unwrap();
    let linked = store
        .issue("iPhone", Some("account:acct:phone-1:dsh".to_string()))
        .unwrap();
    store.describe(&linked.device.id, "iPhone17,1").unwrap();
    // The iPad's token came from a relay that did not know its name.
    let ipad = store
        .issue("Phone", Some("account:acct:phone-2:dsh".to_string()))
        .unwrap();
    store.describe(&ipad.device.id, "iPad13,18").unwrap();

    let views = agi_core::DeviceStatusSource::device_views(&*server);
    let names: Vec<&str> = views.iter().map(|view| view.name.as_str()).collect();
    assert_eq!(names, vec!["iPhone", "iPad"], "{views:?}");
    assert!(
        views.iter().all(|view| view.name != host_name),
        "the computer itself is not a phone: {views:?}"
    );
    // The merged row kept the account-backed id: a later rotation revokes the
    // local pairing's token, and the account record is the one that stays live.
    let iphone = views.iter().find(|view| view.name == "iPhone").unwrap();
    assert_eq!(iphone.id, linked.device.id);
    assert_eq!(iphone.model.as_deref(), Some("iPhone17,1"));
}

#[tokio::test]
async fn a_renamed_computer_is_still_not_a_phone() {
    // The roster's `hostName` is from first launch; the account provisioned this
    // computer under the name it had when it signed in. After a rename the two
    // spellings differ, and the computer itself came back as a phone row. The
    // machine's current name is filtered too, not just the stored one.
    let (server, store, _core, _fakes, _dir) = start().await;
    store
        .issue(
            &agi_platform::host_name(),
            Some("account:acct:self:dsh".to_string()),
        )
        .unwrap();
    store.issue("iPhone", Some("iPhone17,1".to_string())).unwrap();

    let views = agi_core::DeviceStatusSource::device_views(&*server);
    let names: Vec<&str> = views.iter().map(|view| view.name.as_str()).collect();
    assert_eq!(names, vec!["iPhone"], "{views:?}");
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
    assert_eq!(
        workspaces["type"], "ok",
        "workspace.list failed: {workspaces}"
    );
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
    assert_eq!(
        workspaces["type"], "ok",
        "workspace.list failed: {workspaces}"
    );
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
    assert_eq!(
        workspaces["type"], "ok",
        "workspace.list failed: {workspaces}"
    );
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

/// A phone that was away while a turn ran asks for what it missed.
///
/// The state channel only carries what is happening now, so this is the only way
/// back to the intervals that came and went while the socket was down.
#[tokio::test]
async fn a_turn_that_streamed_is_replayed_to_a_phone_that_missed_it() {
    let (server, store, core, fakes, _dir) = start().await;
    let dsh = &fakes[0];
    dsh.record(1, 3, RecordKind::TurnStart, None);
    dsh.record(2, 3, RecordKind::ToolCall, None);
    dsh.record(3, 3, RecordKind::AssistantMessage, Some("我先看一下…"));
    dsh.record(4, 3, RecordKind::ToolCall, None);
    dsh.record(5, 3, RecordKind::AssistantMessage, Some("改好了。"));

    // The kernel ingests engine events on its own task.
    let key = SessionKey::new(EngineId::Dsh, "session-1");
    for _ in 0..50 {
        if core
            .stream_of(&key)
            .is_some_and(|replay| replay.entries.len() == 5)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;
    let answered = call(
        &mut socket,
        "session.stream",
        json!({ "sessionId": "dsh:session-1" }),
    )
    .await;

    assert_eq!(answered["type"], "ok", "{answered}");
    let result = &answered["result"];
    assert_eq!(result["sessionId"], "dsh:session-1");
    assert_eq!(result["turn"], 3);
    // The state on the wire now, not the state the turn ended in.
    assert_eq!(result["state"], 5, "narration is code 5");
    assert_eq!(result["truncated"], false);

    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 5, "{result}");
    assert_eq!(entries[0]["state"], 1, "the turn opened thinking");
    assert_eq!(entries[1]["state"], 2, "then a tool ran");
    assert_eq!(entries[2]["text"], "我先看一下…");
    assert_eq!(entries[4]["text"], "改好了。");
    // Intervals are handed back oldest first, which is the order to draw them in.
    let intervals: Vec<u64> = entries
        .iter()
        .map(|entry| entry["interval"].as_u64().unwrap())
        .collect();
    assert!(
        intervals.windows(2).all(|pair| pair[0] < pair[1]),
        "{intervals:?}"
    );
    assert!(
        entries.iter().all(|entry| entry["final"] == false),
        "the turn is still running"
    );
}

/// A session that never ran a turn has nothing to replay, and saying so is not
/// an error: every session that predates this build is that case.
#[tokio::test]
async fn a_session_with_no_journal_replays_empty() {
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let answered = call(
        &mut socket,
        "session.stream",
        json!({ "sessionId": "dsh:session-1" }),
    )
    .await;
    assert_eq!(answered["type"], "ok", "{answered}");
    assert_eq!(answered["result"]["entries"].as_array().unwrap().len(), 0);
    assert_eq!(answered["result"]["state"], 0, "idle");
    assert_eq!(answered["result"]["truncated"], false);
    assert_eq!(answered["result"]["turn"], Value::Null);

    // No session id at all is a broken frame, the same as every other
    // per-session method.
    let broken = call(&mut socket, "session.stream", json!({})).await;
    assert_eq!(broken["type"], "error", "{broken}");
    assert_eq!(broken["code"], "MALFORMED_FRAME");
}

#[tokio::test]
async fn another_agents_stream_cannot_be_replayed_by_naming_it() {
    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store
        .issue("iPhone", Some("account:acct-1:phone-1:dsh".to_string()))
        .unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    let refused = call(
        &mut socket,
        "session.stream",
        json!({ "sessionId": "codex:session-1" }),
    )
    .await;
    assert_eq!(
        refused["type"], "error",
        "another agent's turn must not be replayed: {refused}"
    );

    // The same call for its own agent is answered.
    let served = call(
        &mut socket,
        "session.stream",
        json!({ "sessionId": "dsh:session-1" }),
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
    assert_eq!(pending["type"], "ok", "pending failed: {pending}");
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
        // An event (an approval, a state notice) is not this call's answer.
    }
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

/// A peer is confined to the sessions it created and the operator's workspaces.
#[tokio::test]
async fn a_peer_only_sees_its_own_sessions_and_allowed_workspaces() {
    use agi_core::RemotePrefs;

    let (server, store, core, fakes, dir) = start().await;
    server.load_peer_sessions(dir.path().join("peer-sessions.json"));
    core.set_remote_prefs(RemotePrefs {
        enabled: true,
        allow_lan: true,
        allow_wan: true,
        workspaces: vec!["/tmp/dsh-project".to_string()],
        blocked: Vec::new(),
    });
    let issued = store
        .issue("Peer", Some("peer:acct-1:peer-1:dsh".to_string()))
        .unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token, None).await;

    // The host has DSH's session-1, but the peer did not create it.
    let sessions = call(&mut socket, "session.list", json!({})).await;
    assert_eq!(
        sessions["result"]["items"].as_array().unwrap().len(),
        0,
        "a new peer owns nothing: {sessions}"
    );
    let refused = call(
        &mut socket,
        "session.messages",
        json!({ "sessionId": "dsh:session-1" }),
    )
    .await;
    assert_eq!(refused["type"], "error", "another session must be refused");

    // A path outside the allowlist is refused before the engine is touched.
    let outside = call(
        &mut socket,
        "session.create",
        json!({ "cwd": "/tmp/not-allowed" }),
    )
    .await;
    assert_eq!(outside["type"], "error");
    assert_eq!(outside["code"], "WORKSPACE_NOT_ALLOWED");

    // The allowlisted path creates a session the peer now owns.
    let created = call(
        &mut socket,
        "session.create",
        json!({ "cwd": "/tmp/dsh-project" }),
    )
    .await;
    assert_eq!(
        created["type"], "ok",
        "the allowed workspace creates: {created}"
    );
    let owned = created["result"]["sessionId"].as_str().unwrap().to_string();
    let served = call(
        &mut socket,
        "session.messages",
        json!({ "sessionId": owned }),
    )
    .await;
    assert_eq!(
        served["type"], "ok",
        "the peer's own session stays readable"
    );

    // Another agent's approval is not this peer's to see; one on its own
    // session is.
    fakes[0].ask(Interaction {
        id: "approval-other".to_string(),
        session: Some(SessionKey::new(EngineId::Dsh, "session-1")),
        kind: agi_protocol::domain::InteractionKind::Approval,
        title: "bash".to_string(),
        detail: Some("another session".to_string()),
        requested_at: 1,
        questions: Vec::new(),
    });
    let owned_key: SessionKey = owned.parse().unwrap();
    fakes[0].ask(Interaction {
        id: "approval-owned".to_string(),
        session: Some(owned_key),
        kind: agi_protocol::domain::InteractionKind::Approval,
        title: "bash".to_string(),
        detail: Some("my session".to_string()),
        requested_at: 2,
        questions: Vec::new(),
    });
    for _ in 0..50 {
        if core.snapshot().interactions.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pending = call(&mut socket, "interactions.pending", json!({})).await;
    assert_eq!(pending["type"], "ok", "pending failed: {pending}");
    let approvals = pending["result"]["approvals"].as_array().unwrap();
    assert_eq!(
        approvals.len(),
        1,
        "only the peer's own approval: {pending}"
    );
    assert_eq!(approvals[0]["requestId"], "approval-owned");

    // The ownership map is on disk for a restart.
    let saved = std::fs::read_to_string(dir.path().join("peer-sessions.json")).unwrap();
    assert!(saved.contains(&owned), "peer ownership survives: {saved}");

    let workspaces = call(&mut socket, "workspace.list", json!({})).await;
    assert_eq!(
        workspaces["type"], "ok",
        "workspace.list failed: {workspaces}"
    );
    let rows = workspaces["result"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("workspace.list shape: {workspaces}"));
    assert_eq!(
        rows.len(),
        1,
        "only the allowed DSH workspace: {workspaces}"
    );
    assert_eq!(rows[0]["path"], "/tmp/dsh-project");
    assert!(
        rows[0].get("sessionIds").is_none(),
        "peer must not see other sessions"
    );
}

/// The whole path, through the real relay.
///
/// This is the only test in the crate that starts another process: the relay is
/// a Node program, and the phone reaches the computer through it exactly the way
/// the app does -- upgrade, binary/JSON `ws-msg` framing, the local listener,
/// and back. A stand-in relay would prove the desktop's side of the protocol and
/// nothing about the relay's, which is where the transfer's real cost lives.
///
/// Ignored by default because it needs `cd dsh-relay && npm run build` and a
/// `node` on PATH; run it with:
///
///     cargo test -p agi-mizar --test agent_scope -- --ignored a_phone_reaches
#[tokio::test]
#[ignore = "needs the relay built (cd dsh-relay && npm run build) and node on PATH"]
async fn a_phone_reaches_the_host_through_the_real_relay() {
    use std::process::Stdio;
    use tokio::process::Command;

    let (server, store, _core, _fakes, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();

    // `MIZAR_E2E_RELAY_URL` runs the same test through a deployed relay (the
    // production one, for a release check); without it, a throwaway relay is
    // started from `dsh-relay/lib`.
    let deployed = std::env::var("MIZAR_E2E_RELAY_URL").ok();
    let mut relay = None;
    let base = match deployed {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => {
            // A free port for the relay: bind, read, release. The race is
            // theoretical on a test machine and a fixed port would collide with
            // a developer's own relay.
            let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let relay_port = probe.local_addr().unwrap().port();
            drop(probe);

            let relay_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../dsh-relay");
            assert!(
                std::path::Path::new(relay_dir).join("lib/index.js").exists(),
                "build the relay first: cd dsh-relay && npm run build"
            );
            relay = Some(
                Command::new("node")
                    .arg("lib/index.js")
                    .current_dir(relay_dir)
                    .env("MIZAR_RELAY_HOST", "127.0.0.1")
                    .env("MIZAR_RELAY_PORT", relay_port.to_string())
                    .env("MIZAR_RELAY_PROBE_PORT", "0")
                    .env("MIZAR_RELAY_STUN_PORT", "0")
                    .env("MIZAR_RELAY_TRUST_PROXY", "0")
                    .env("MIZAR_RELAY_REQUEST_TIMEOUT", "10000")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .expect("node could not start the relay"),
            );

            // Wait for the listener. A successful connect is the whole
            // readiness signal: the relay binds before it serves anything.
            let mut ready = false;
            for _ in 0..100 {
                if tokio::net::TcpStream::connect(("127.0.0.1", relay_port))
                    .await
                    .is_ok()
                {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(ready, "the relay never started listening");
            format!("http://127.0.0.1:{relay_port}")
        }
    };

    // The desktop dials the relay; the relay tells it the room.
    let client = agi_mizar::RelayClient::new(
        base,
        "e2e-tunnel-token-0123456789",
        server.port(),
    );
    tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.run().await }
    });
    let mut phone_base = None;
    for _ in 0..100 {
        if let Some(base) = client.phone_base_url() {
            phone_base = Some(base);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let phone_base = phone_base.expect("the tunnel never joined a room");
    let url = format!(
        "{}/mizar/v1/ws",
        phone_base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    );

    let (mut phone, _) = connect_to(&url).await;
    phone
        .send(Message::Text(
            json!({ "v": 1, "type": "hello", "id": "h1", "token": issued.token }).to_string(),
        ))
        .await
        .unwrap();
    let welcome = read_json(&mut phone).await;
    assert_eq!(welcome["type"], "welcome", "handshake failed: {welcome}");

    // One batched read, through the relay, answered by the real server.
    phone
        .send(Message::Text(
            json!({
                "v": 1, "type": "session.sync", "id": "s1",
                "payload": {
                    "items": [
                        { "sessionId": "dsh:session-1", "updatedAt": 0, "afterSeq": 0 },
                        { "sessionId": "claude:session-1", "updatedAt": 0, "afterSeq": 0 }
                    ]
                }
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let answer = read_json(&mut phone).await;
    assert_eq!(answer["type"], "ok", "session.sync failed through the relay: {answer}");
    assert_eq!(
        answer["result"]["items"].as_array().map(Vec::len),
        Some(2),
        "both pages should ride in one frame: {answer}"
    );
    for item in answer["result"]["items"].as_array().unwrap() {
        assert!(item["messages"].is_object(), "no page for {item}");
    }

    // A burst of batched reads. Before 0.3.7 the relay priced every bridged
    // frame as a request -- 300 a minute with a burst of 50 -- and dropped
    // everything past the burst without telling the phone, so forty syncs (eighty
    // frames) would have hung. This is the regression that must not come back.
    for index in 0..40 {
        phone
            .send(Message::Text(
                json!({
                    "v": 1, "type": "session.sync", "id": format!("burst-{index}"),
                    "payload": {
                        "items": [
                            { "sessionId": "dsh:session-1", "updatedAt": 0, "afterSeq": 0 }
                        ]
                    }
                })
                .to_string(),
            ))
            .await
            .unwrap();
    }
    for index in 0..40 {
        let answer = read_json(&mut phone).await;
        assert_eq!(
            answer["type"], "ok",
            "burst frame {index} was dropped: {answer}"
        );
    }

    if let Some(mut relay) = relay {
        let _ = relay.kill().await;
    }
}

/// Connect a phone WebSocket to a URL.
async fn connect_to(
    url: &str,
) -> (
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    tokio_tungstenite::tungstenite::handshake::client::Response,
) {
    connect_async(url).await.expect("the phone could not reach the relay")
}

/// Read one JSON frame from a phone socket, with a deadline.
async fn read_json(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("the relay answered nothing")
        .expect("the phone socket closed")
        .unwrap();
    serde_json::from_str(&message.into_text().unwrap()).unwrap()
}
