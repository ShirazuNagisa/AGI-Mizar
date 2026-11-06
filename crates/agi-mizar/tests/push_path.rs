//! What the computer tells the relay to push, and when.
//!
//! The island is drawn on a phone that is not running this program, so the only
//! thing this side can be tested on is the request it sends. A fake relay records
//! one request per push and the assertions are on the JSON body: the fields the
//! relay reads (`phase`, `rows`, `approvals`, `endLiveActivity`) are the contract
//! between three repositories, and a rename here is a card that never appears.

use std::sync::{Arc, Mutex};

use agi_core::Core;
use agi_mizar::activity::{self, Tracker};
use agi_mizar::net::identity::{Identity, Purpose};
use agi_mizar::{DeviceStore, MizarServer, RelayClient};
use agi_protocol::domain::{Interaction, InteractionKind, Session, SessionKey};
use agi_protocol::engine::EngineId;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// One request the fake relay received.
#[derive(Clone, Debug)]
struct Received {
    path: String,
    body: Value,
}

/// A relay that records what it is told and answers `{"ok":true}`.
async fn fake_relay() -> (String, Arc<Mutex<Vec<Received>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, length) = loop {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buffer[..position]).to_string();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buffer.len() >= position + 4 + length {
                            break (position + 4, length);
                        }
                    }
                };
                let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                let path = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_string();
                let body = serde_json::from_slice::<Value>(&buffer[head_end..head_end + length])
                    .unwrap_or(Value::Null);
                recorded.lock().unwrap().push(Received { path, body });
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}")
                    .await;
                let _ = socket.flush().await;
            });
        }
    });
    (format!("http://{address}"), seen)
}

async fn start() -> (Arc<MizarServer>, Arc<DeviceStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = DeviceStore::open(dir.path().join("devices.json"), "test-host").unwrap();
    let core = Core::new(Vec::new());
    core.start().await;
    let server = MizarServer::new(core, Arc::clone(&store), "/tmp/dsh-home");
    server.bind(0).await.unwrap();
    (server, store, dir)
}

fn card() -> activity::Card {
    activity::Card {
        rows: vec![activity::Row {
            session_id: "dsh:s-1".to_string(),
            title: "Refactor the parser".to_string(),
            running_since: 1_700_000_000_000,
        }],
        approvals: vec![],
    }
}

#[tokio::test]
async fn a_running_session_becomes_a_card_the_relay_can_start() {
    let (server, _store, _dir) = start().await;
    let (base, seen) = fake_relay().await;
    server.attach_relay(RelayClient::new(base, "a-tunnel-token", 1));

    server.publish_card_for_tests(&card()).await;

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/push/v1/activity");
    let body = &requests[0].body;
    // `start` is an upsert: the relay turns it into an update on a phone that
    // already has a card, so this side never has to know which phones do.
    assert_eq!(body["phase"], "start");
    assert_eq!(body["rows"][0]["sessionId"], "dsh:s-1");
    assert_eq!(body["rows"][0]["runningSince"], 1_700_000_000_000i64);
    assert!(body["approvals"].as_array().unwrap().is_empty());
    // A stale foreground report must not suppress the start: the card is the
    // only thing that can appear when the phone was never opened for this turn.
    assert!(body["exceptDeviceIds"]
        .as_array()
        .is_some_and(Vec::is_empty));
}

#[tokio::test]
async fn an_approval_rides_on_the_card_with_the_id_an_answer_will_name() {
    let (server, _store, _dir) = start().await;
    let (base, seen) = fake_relay().await;
    server.attach_relay(RelayClient::new(base, "a-tunnel-token", 1));

    let mut tracker = Tracker::default();
    let mut session = Session::bare(SessionKey::new(EngineId::Dsh, "s-1"), 1);
    session.running = true;
    session.title = Some("Refactor the parser".to_string());
    tracker.running(&session, 1_700_000_000_000);
    let interaction = Interaction {
        id: "request-1".to_string(),
        session: Some(SessionKey::new(EngineId::Dsh, "s-1")),
        kind: InteractionKind::Approval,
        title: "Bash".to_string(),
        detail: Some("rm -rf build".to_string()),
        requested_at: 1,
        questions: Vec::new(),
    };
    let card = tracker
        .approval_requested(&interaction)
        .expect("the card changed");

    server.publish_card_for_tests(&card).await;

    let requests = seen.lock().unwrap().clone();
    let body = &requests[0].body;
    assert_eq!(body["approvals"][0]["requestId"], "request-1");
    assert_eq!(body["approvals"][0]["tool"], "Bash");
    assert_eq!(body["approvals"][0]["detail"], "rm -rf build");
    // The row stays: the turn is blocked, not over.
    assert_eq!(body["rows"][0]["sessionId"], "dsh:s-1");
}

#[tokio::test]
async fn a_finished_turn_ends_the_card_and_says_which_session() {
    let (server, _store, _dir) = start().await;
    let (base, seen) = fake_relay().await;
    server.attach_relay(RelayClient::new(base, "a-tunnel-token", 1));

    let key = SessionKey::new(EngineId::Dsh, "s-1");
    server
        .publish_turn_end_for_tests(&key, "Refactor the parser", true)
        .await;

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests[0].path, "/push/v1/notify");
    let body = &requests[0].body;
    assert_eq!(body["kind"], "turn_end");
    assert_eq!(body["sessionId"], "dsh:s-1");
    // The last row finished, so the same request brings the island down: one
    // call, because the two facts are only true together.
    assert_eq!(body["endLiveActivity"], true);
    assert_eq!(body["body"], "Refactor the parser is done");
    // An end push must reach even a device the host still believes is on
    // screen: a stale presence report must not leave the card counting.
    assert!(body["exceptDeviceIds"]
        .as_array()
        .is_some_and(Vec::is_empty));
}

#[tokio::test]
async fn a_finished_turn_does_not_exclude_a_foreground_device_from_the_end_push() {
    let (server, _store, _dir) = start().await;
    let (base, seen) = fake_relay().await;
    server.attach_relay(RelayClient::new(base, "a-tunnel-token", 1));
    server.set_foreground_for_tests("phone-1", true);
    let key = SessionKey::new(EngineId::Dsh, "s-1");
    server
        .publish_turn_end_for_tests(&key, "Refactor the parser", true)
        .await;
    let requests = seen.lock().unwrap().clone();
    let body = &requests[0].body;
    assert_eq!(body["endLiveActivity"], true);
    assert!(body["exceptDeviceIds"]
        .as_array()
        .is_some_and(Vec::is_empty));
}

#[tokio::test]
async fn a_card_for_a_punched_connection_advertises_what_the_phone_needs() {
    // `host.info` is how a phone learns there is a direct path at all; the
    // candidate list is what the signalling channel then hands out.
    let (server, _store, _dir) = start().await;
    let reach = agi_mizar::net::engine::Reach::default();
    reach.set_port(51_234);
    server.attach_direct_path("an-endpoint-id", reach);
    assert_eq!(server.net_endpoint_id().as_deref(), Some("an-endpoint-id"));

    // The identity the engine derives is base64url of an Ed25519 key: 43
    // characters, so a phone can put it in a ticket without escaping.
    let identity = Identity::derive(b"a tunnel secret", Purpose::Host);
    assert_eq!(identity.endpoint_id().len(), 43);
    assert_eq!(
        serde_json::to_string(&json!(identity.endpoint_id()))
            .unwrap()
            .len(),
        45
    );
}

#[tokio::test]
async fn the_signal_channel_answers_in_the_shape_the_phone_decodes() {
    // Cross-language contract: `MizarNetSignal` on the phone decodes exactly
    // these keys, and a `v` it does not know is a channel it refuses. The
    // candidate keys are the ones `MizarNetCandidate` reads.
    let (server, store, _dir) = start().await;
    let issued = store.issue("phone", None).expect("a token");
    let reach = agi_mizar::net::engine::Reach::default();
    reach.set_port(51_234);
    server.attach_direct_path("an-endpoint-id", reach);

    let url = format!("ws://127.0.0.1:{}/mizar-net/signal", server.port());
    let (mut socket, _) = connect_async(url).await.expect("a socket");
    socket
        .send(Message::Text(
            json!({
                "v": 1,
                "type": "hello",
                "endpointId": "the-phone",
                "candidates": [],
                "token": issued.token,
            })
            .to_string(),
        ))
        .await
        .expect("the hello goes out");

    let reply = socket.next().await.expect("an answer").expect("a frame");
    let value: Value = serde_json::from_str(reply.to_text().expect("text")).expect("JSON");
    assert_eq!(value["v"], 1);
    assert_eq!(value["type"], "hello");
    assert_eq!(value["endpointId"], "an-endpoint-id");
    let candidates = value["candidates"].as_array().expect("an array");
    // A machine with no interfaces enumerated still answers with an array; when
    // there is one, every row has the four keys the phone reads.
    for candidate in candidates {
        assert_eq!(candidate["kind"], "host");
        assert!(candidate["address"].is_string());
        assert_eq!(candidate["port"], 51_234);
        assert!(candidate["priority"].is_u64());
    }
}

#[tokio::test]
async fn the_signal_channel_refuses_a_phone_this_host_never_paired() {
    let (server, _store, _dir) = start().await;
    let reach = agi_mizar::net::engine::Reach::default();
    reach.set_port(51_234);
    server.attach_direct_path("an-endpoint-id", reach);

    let url = format!("ws://127.0.0.1:{}/mizar-net/signal", server.port());
    let (mut socket, _) = connect_async(url).await.expect("a socket");
    socket
        .send(Message::Text(
            json!({
                "v": 1,
                "type": "hello",
                "endpointId": "a-stranger",
                "candidates": [],
                "token": "not-a-token-this-host-issued",
            })
            .to_string(),
        ))
        .await
        .expect("sent");

    // The endpoint sits on the whole network: a stranger must not be told where
    // this computer is, so the channel ends without an answer.
    let next = socket.next().await;
    match next {
        None => {}
        Some(Ok(Message::Close(_))) => {}
        Some(Ok(other)) => panic!("a stranger was answered with {other:?}"),
        Some(Err(_)) => {}
    }
}

/// Send one hello over a fresh socket and return the first frame the host sends.
async fn handshake(server: &Arc<MizarServer>, token: &str, account: Option<&str>) -> Value {
    let url = format!("ws://127.0.0.1:{}/mizar/v1/ws", server.port());
    let (mut socket, _) = connect_async(url).await.expect("a socket");
    let mut hello = json!({ "v": 1, "type": "hello", "token": token, "protocolVersion": 1 });
    if let Some(account) = account {
        hello["accountId"] = json!(account);
    }
    socket
        .send(Message::Text(hello.to_string()))
        .await
        .expect("the hello goes out");
    let frame = socket.next().await.expect("an answer").expect("a frame");
    let value: Value =
        serde_json::from_str(frame.to_text().expect("text")).expect("the answer is JSON");
    let _ = socket.close(None).await;
    value
}

#[tokio::test]
async fn a_phone_signed_in_to_another_account_is_refused() {
    let (server, store, _dir) = start().await;
    let issued = store.issue("phone", None).expect("a token");
    server.set_account_user(Some("account-a".to_string()));

    // Same phone, same token, different account: the pairing code proves whoever
    // scanned it stood at the screen, not that the two accounts are the same.
    let refused = handshake(&server, &issued.token, Some("account-b")).await;
    assert_eq!(refused["type"], "error");
    assert_eq!(refused["code"], "UNAUTHENTICATED");

    // The same account is the ordinary case.
    let welcome = handshake(&server, &issued.token, Some("account-a")).await;
    assert_eq!(welcome["type"], "welcome");
}

#[tokio::test]
async fn a_phone_that_says_nothing_about_its_account_is_left_as_it_was() {
    // Every phone built before this field existed omits it, and a host that
    // treated "silent" as "different" would have stranded them all.
    let (server, store, _dir) = start().await;
    let issued = store.issue("phone", None).expect("a token");
    server.set_account_user(Some("account-a".to_string()));
    assert_eq!(
        handshake(&server, &issued.token, None).await["type"],
        "welcome"
    );

    // A desktop that is signed out asks nothing of anybody.
    server.set_account_user(None);
    assert_eq!(
        handshake(&server, &issued.token, Some("account-b")).await["type"],
        "welcome"
    );
}
