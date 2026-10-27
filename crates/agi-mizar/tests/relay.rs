//! The tunnel, against a stand-in relay.
//!
//! A real relay would be another machine; what matters here is the shape of the
//! conversation: the relay says `welcome` with a room, forwards a phone request,
//! and gets an answer replayed from the local listener. Both sockets are real.

use std::sync::Arc;
use std::time::Duration;

use agi_mizar::RelayClient;
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
            axum::routing::post(|body: String| async move {
                axum::Json(json!({ "echoed": body }))
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
        assert!(
            first["body"].as_str().unwrap().contains("tunnel-target"),
            "the answer did not come from the local listener: {first}"
        );

        // A phone's POST, with a body that has to survive the round trip.
        socket
            .send(Message::Text(
                json!({
                    "t": "req", "id": "r2", "method": "POST",
                    "path": "/mizar/v1/pair", "headers": {},
                    "body": "{\"code\":\"K7QP-3M2X\"}"
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let second = read_frame(&mut socket).await;
        assert_eq!(second["id"], "r2");
        assert!(
            second["body"].as_str().unwrap().contains("K7QP-3M2X"),
            "the request body did not reach the listener: {second}"
        );

        // Liveness: a ping is answered with the same id.
        socket
            .send(Message::Text(json!({ "t": "ping", "id": "p1" }).to_string()))
            .await
            .unwrap();
        let pong = read_frame(&mut socket).await;
        assert_eq!(pong["t"], "pong");
        assert_eq!(pong["id"], "p1");

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
