//! The tunnel, against a stand-in relay.
//!
//! A real relay would be another machine; what matters here is the shape of the
//! conversation: the relay says `welcome` with a room, forwards a phone request,
//! and gets an answer replayed from the local listener. Both sockets are real.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agi_mizar::RelayClient;
use axum::extract::WebSocketUpgrade;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

/// Start the local listener a tunnel replays onto.
async fn local_listener() -> u16 {
    let app = Router::new()
        .route(
            "/mizar/v1/hello",
            get(|| async { axum::Json(json!({ "hostName": "tunnel-target" })) }),
        )
        .route(
            "/mizar/v1/pair",
            axum::routing::post(
                |body: String| async move { axum::Json(json!({ "echoed": body })) },
            ),
        )
        // A stream that says back what it is told, the way the phone's socket
        // answers frames.
        .route(
            "/mizar/v1/ws",
            get(|upgrade: WebSocketUpgrade| async {
                upgrade.on_upgrade(|mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        if socket.send(message).await.is_err() {
                            break;
                        }
                    }
                })
            }),
        );
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

/// Start a stand-in relay that accepts one tunnel.
async fn fake_relay() -> (u16, oneshot::Receiver<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done_tx, done_rx) = oneshot::channel();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();

        // The relay chooses the room; the computer never invents one.
        socket
            .send(Message::Text(
                json!({ "t": "welcome", "room": "room-1", "protocolVersion": 1 }).to_string(),
            ))
            .await
            .unwrap();

        // A phone's GET.
        socket
            .send(Message::Text(
                json!({
                    "t": "req", "id": "r1", "method": "GET",
                    "path": "/mizar/v1/hello", "headers": {}, "body": null
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let first = read_frame(&mut socket).await;
        assert_eq!(first["t"], "res", "expected an answer, got {first}");
        assert_eq!(first["id"], "r1");
        assert_eq!(first["status"], 200);
        let answered = body_of(&first);
        assert!(
            answered.contains("tunnel-target"),
            "the answer did not come from the local listener: {answered}"
        );

        // A phone's POST, with a body that has to survive the round trip. The
        // tunnel carries bodies as base64, which is the part a stand-in relay
        // could previously get away with ignoring.
        socket
            .send(Message::Text(
                json!({
                    "t": "req", "id": "r2", "method": "POST",
                    "path": "/mizar/v1/pair", "headers": {},
                    "body": b64("{\"code\":\"K7QP-3M2X\"}")
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let second = read_frame(&mut socket).await;
        assert_eq!(second["id"], "r2");
        let echoed = body_of(&second);
        assert!(
            echoed.contains("K7QP-3M2X"),
            "the request body did not reach the listener: {echoed}"
        );

        // Liveness: a ping is answered with the same id.
        socket
            .send(Message::Text(
                json!({ "t": "ping", "id": "p1" }).to_string(),
            ))
            .await
            .unwrap();
        let pong = read_frame(&mut socket).await;
        assert_eq!(pong["t"], "pong");
        assert_eq!(pong["id"], "p1");

        // The phone's WebSocket: the relay holds it until the computer confirms
        // the stream is up, and `ws-msg` travels as base64 in both directions.
        socket
            .send(Message::Text(
                json!({
                    "t": "ws-open", "id": "w1",
                    "path": "/mizar/v1/ws", "headers": {}
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let opened = read_frame(&mut socket).await;
        assert_eq!(opened["t"], "ws-open-ok", "expected the stream to open");
        assert_eq!(opened["id"], "w1");

        socket
            .send(Message::Text(
                json!({
                    "t": "ws-msg", "id": "w1",
                    "data": b64("{\"v\":1,\"type\":\"hello\"}"), "binary": false
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let echoed = read_frame(&mut socket).await;
        assert_eq!(echoed["t"], "ws-msg");
        assert_eq!(echoed["id"], "w1");
        assert_eq!(decoded(&echoed, "data"), "{\"v\":1,\"type\":\"hello\"}");

        // A stream the listener refuses is reported as an error, at once. A
        // `ws-close` here would reach the relay before `ws-open-ok`, and the
        // phone would wait out its own timeout with a slot held.
        socket
            .send(Message::Text(
                json!({
                    "t": "ws-open", "id": "w2",
                    "path": "/no-such-stream", "headers": {}
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let refused = read_frame(&mut socket).await;
        assert_eq!(refused["t"], "error", "expected the refusal, got {refused}");
        assert_eq!(refused["id"], "w2");

        let _ = done_tx.send(());
    });

    (port, done_rx)
}

/// Read one JSON frame.
async fn read_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the tunnel sent nothing")
        .expect("the tunnel closed")
        .unwrap();
    serde_json::from_str(&message.into_text().unwrap()).unwrap()
}

/// The `res` frame's body, decoded the way the relay decodes it.
fn body_of(frame: &Value) -> String {
    decoded(frame, "body")
}

/// One base64 field of a frame, decoded.
fn decoded(frame: &Value, key: &str) -> String {
    use base64::Engine;
    let text = frame[key].as_str().unwrap_or_default();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .unwrap_or_else(|error| panic!("{key} was not base64: {error}"));
    String::from_utf8(bytes).unwrap()
}

/// Encode a request body the way the relay does.
fn b64(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
}

#[tokio::test]
async fn replays_requests_and_answers_pings() {
    let local_port = local_listener().await;
    let (relay_port, done) = fake_relay().await;

    let client = RelayClient::new(
        format!("http://127.0.0.1:{relay_port}"),
        "tunnel-secret",
        local_port,
    );
    tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.run().await }
    });

    tokio::time::timeout(Duration::from_secs(10), done)
        .await
        .expect("the relay conversation never finished")
        .expect("the relay task panicked");
}

/// A relay that negotiated the binary framing, and the desktop that must honour
/// it in both directions.
///
/// The JSON + base64 spelling costs a third more bytes and two extra passes over
/// every history page; the framing is therefore worth having. What must hold is
/// that it is *negotiated*: a relay that never says `wsMsgBinary` (every relay
/// before 0.3.7, and the test above) keeps receiving JSON, and a desktop that
/// never advertises the header is never sent binary.
// The handshake callback tungstenite hands us must answer with its own
// `Response`, so the Err variant is a header map and not a string we can shrink;
// boxing it here would only make the test read worse.
#[allow(clippy::result_large_err)]
#[tokio::test]
async fn binary_ws_msg_frames_when_the_relay_advertises_them() {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    let local_port = local_listener().await;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done_tx, done_rx) = oneshot::channel();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let advertised = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&advertised);
        let callback = move |request: &Request, response: Response| {
            seen.store(
                request
                    .headers()
                    .get("x-mizar-tunnel-frames")
                    .is_some_and(|value| value == "binary"),
                Ordering::Relaxed,
            );
            Ok(response)
        };
        let mut socket = tokio_tungstenite::accept_hdr_async(stream, callback)
            .await
            .unwrap();
        assert!(
            advertised.load(Ordering::Relaxed),
            "the desktop did not advertise the binary framing on its upgrade"
        );

        socket
            .send(Message::Text(
                json!({
                    "t": "welcome", "room": "room-1", "protocolVersion": 1,
                    "wsMsgBinary": true
                })
                .to_string(),
            ))
            .await
            .unwrap();

        socket
            .send(Message::Text(
                json!({
                    "t": "ws-open", "id": "w1",
                    "path": "/mizar/v1/ws", "headers": {}
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let opened = read_frame(&mut socket).await;
        assert_eq!(opened["t"], "ws-open-ok");

        // Relay -> desktop is framed.
        socket
            .send(Message::Binary(agi_mizar::relay::encode_ws_msg(
                "w1",
                false,
                b"{\"v\":1,\"type\":\"hello\"}",
            )))
            .await
            .unwrap();

        // Desktop -> relay must be framed too: the local listener echoes the
        // text, and the answer is a binary `ws-msg`, not JSON + base64.
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("the tunnel sent nothing")
            .expect("the tunnel closed")
            .unwrap();
        match message {
            Message::Binary(bytes) => {
                let (id, binary, payload) =
                    agi_mizar::relay::decode_ws_msg(&bytes).expect("a framed ws-msg");
                assert_eq!(id, "w1");
                assert!(!binary, "a text frame must keep its kind");
                assert_eq!(
                    std::str::from_utf8(payload).unwrap(),
                    "{\"v\":1,\"type\":\"hello\"}"
                );
            }
            other => panic!("expected a binary ws-msg, got {other:?}"),
        }

        let _ = done_tx.send(());
    });

    let client = RelayClient::new(format!("ws://127.0.0.1:{port}"), "tunnel-secret", local_port);
    tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.run().await }
    });
    tokio::time::timeout(Duration::from_secs(10), done_rx)
        .await
        .expect("the relay conversation never finished")
        .expect("the relay task panicked");
}
