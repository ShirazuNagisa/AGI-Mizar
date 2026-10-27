//! Pairing and the phone socket, end to end over real HTTP and a real WebSocket.
//!
//! The point of this file is that nothing is mocked on the transport: a code is
//! typed, a token comes back, the token opens a socket, and the socket answers a
//! method. Everything in between is the code that will run on a phone's behalf.

use std::sync::Arc;
use std::time::Duration;

use agi_core::Core;
use agi_mizar::{DeviceStore, MizarServer};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// A server over an empty kernel and a temporary roster.
async fn start() -> (Arc<MizarServer>, Arc<DeviceStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = DeviceStore::open(dir.path().join("devices.json"), "test-host").unwrap();
    let core = Core::new(Vec::new());
    core.start().await;
    let server = MizarServer::new(core, Arc::clone(&store), "/tmp/dsh-home");
    server.bind(0).await.unwrap();
    (server, store, dir)
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
    // Read headers, then the declared length: a keep-alive connection would
    // otherwise leave us waiting for a close that never comes.
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

/// Open the phone socket and complete the handshake.
async fn connect(
    port: u16,
    token: &str,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    Value,
) {
    let (mut socket, _) = connect_async(format!("ws://127.0.0.1:{port}/mizar/v1/ws"))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            json!({
                "v": 1, "type": "hello", "id": "h1",
                "payload": { "token": token, "appVersion": "0.1.0" }
            })
            .to_string(),
        ))
        .await
        .unwrap();

    let welcome = next(&mut socket).await;
    (socket, welcome)
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
async fn hello_answers_without_a_token() {
    let (server, _, _dir) = start().await;
    let (status, body) = http(server.port(), "GET", "/mizar/v1/hello", "").await;
    assert_eq!(status, 200);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["protocolVersion"], 1);
    assert_eq!(value["hostName"], "test-host");
    assert!(value["hostId"].as_str().unwrap().len() > 8);
}

#[tokio::test]
async fn a_wrong_code_is_refused() {
    let (server, _, _dir) = start().await;
    server.issue_code();

    let (status, body) = http(
        server.port(),
        "POST",
        "/mizar/v1/pair",
        &json!({
            "code": "ZZZZ-ZZZZ", "deviceName": "iPhone", "protocolVersion": 1
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, 200);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["type"], "error");
    assert_eq!(value["code"], "PAIRING_CODE_UNKNOWN");
}

#[tokio::test]
async fn a_code_pairs_once_and_opens_a_socket() {
    let (server, _, _dir) = start().await;
    let code = server.issue_code();

    let request_body = json!({
        "code": code.display,
        "deviceName": "iPhone 17",
        "deviceModel": "iPhone17,1",
        "appVersion": "0.1.0",
        "protocolVersion": 1,
    })
    .to_string();
    let (status, body) = http(server.port(), "POST", "/mizar/v1/pair", &request_body).await;
    assert_eq!(status, 200);
    let value: Value = serde_json::from_str(&body).unwrap();
    let token = value["deviceToken"].as_str().unwrap().to_string();
    assert!(token.starts_with("mz_"));
    assert_eq!(value["hostName"], "test-host");
    assert_eq!(value["protocolVersion"], 1);

    // The same code cannot be used twice.
    let (_, again) = http(server.port(), "POST", "/mizar/v1/pair", &request_body).await;
    let again: Value = serde_json::from_str(&again).unwrap();
    assert_eq!(
        again["code"], "PAIRING_CODE_UNKNOWN",
        "second attempt answered {again}"
    );

    let (mut socket, welcome) = connect(server.port(), &token).await;
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["hostName"], "test-host");
    assert_eq!(welcome["protocolVersion"], 1);
    // The golden frame carries no id, and neither does ours.
    assert!(welcome.get("id").is_none());

    socket
        .send(Message::Text(
            json!({ "v": 1, "type": "session.list", "id": "m1", "payload": {} }).to_string(),
        ))
        .await
        .unwrap();
    let answer = next(&mut socket).await;
    assert_eq!(answer["type"], "ok");
    assert_eq!(answer["id"], "m1");
    assert_eq!(answer["result"]["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn an_unknown_method_says_so_instead_of_failing() {
    let (server, store, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token).await;

    socket
        .send(Message::Text(
            json!({ "v": 1, "type": "session.unknown", "id": "m9", "payload": {} }).to_string(),
        ))
        .await
        .unwrap();
    let answer = next(&mut socket).await;
    assert_eq!(answer["type"], "error");
    assert_eq!(answer["code"], "NOT_IMPLEMENTED");
    assert_eq!(answer["id"], "m9");
}

#[tokio::test]
async fn a_revoked_device_cannot_open_a_socket() {
    let (server, store, _dir) = start().await;
    let issued = store.issue("iPhone", None).unwrap();
    store.revoke(&issued.device.id).unwrap();

    let (_, answer) = connect(server.port(), &issued.token).await;
    assert_eq!(answer["type"], "error");
    assert_eq!(answer["code"], "DEVICE_REVOKED");
}

#[tokio::test]
async fn the_tunnel_moves_only_to_a_relay_both_sides_know() {
    let (server, store, _dir) = start().await;
    server.attach_relay(agi_mizar::RelayClient::new(
        "https://dshmizar.shirazu-nagisa.com",
        "tunnel-secret",
        1,
    ));
    let issued = store.issue("iPhone", None).unwrap();
    let (mut socket, _) = connect(server.port(), &issued.token).await;

    // An origin neither this host nor the app trusts is refused, not dialled.
    socket
        .send(Message::Text(
            json!({
                "v": 1, "type": "relay.set", "id": "r1",
                "payload": { "url": "https://evil.example.com" }
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let refused = next(&mut socket).await;
    assert_eq!(refused["type"], "error");
    assert_eq!(refused["code"], "MALFORMED_FRAME");

    // Asking for the relay it is already on is answered, not churned.
    socket
        .send(Message::Text(
            json!({
                "v": 1, "type": "relay.set", "id": "r2",
                "payload": { "url": "https://dshmizar.shirazu-nagisa.com" }
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let answer = next(&mut socket).await;
    assert_eq!(answer["type"], "ok");
    assert_eq!(
        answer["result"]["url"],
        "https://dshmizar.shirazu-nagisa.com"
    );
    assert_eq!(answer["result"]["room"], Value::Null);
}

#[tokio::test]
async fn a_socket_without_hello_is_told_to_handshake() {
    let (server, _, _dir) = start().await;
    let (mut socket, _) = connect_async(format!("ws://127.0.0.1:{}/mizar/v1/ws", server.port()))
        .await
        .unwrap();

    socket
        .send(Message::Text(
            json!({ "v": 1, "type": "session.list", "id": "m1", "payload": {} }).to_string(),
        ))
        .await
        .unwrap();
    let answer = next(&mut socket).await;
    assert_eq!(answer["type"], "error");
    assert_eq!(answer["code"], "UNAUTHENTICATED");
}

/// The window asks for the code every time it draws the pairing card, and the
/// slot is empty until the first code is issued — and empty again after one is
/// used. Taking the code lock twice on one thread froze the whole server; this
/// test holds the door shut.
#[tokio::test]
async fn asking_for_the_code_again_does_not_deadlock() {
    let (server, _, _dir) = start().await;

    let mut views = Vec::new();
    for _ in 0..3 {
        let server = Arc::clone(&server);
        let view = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || server.pairing_view(false)),
        )
        .await
        .expect("pairing_view deadlocked on the code lock")
        .expect("the blocking task panicked");
        views.push(view);
    }

    assert!(!views[0].code.is_empty());
    assert_eq!(
        views[1].code, views[0].code,
        "a window that redraws must not invalidate the code on screen"
    );
    assert!(views[0].uri.starts_with("mizar://pair?"));
}
