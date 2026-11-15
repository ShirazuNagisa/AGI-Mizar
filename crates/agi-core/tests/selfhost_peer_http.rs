//! The self-hosted pair/directory calls against a fake relay over real HTTP.
//!
//! The stored origin is https-only, so the kernel command cannot be pointed at a
//! loopback test server; [`agi_core::peer`] takes the origin as an argument for
//! exactly that reason. This file is the transport half: the request body the
//! target will see, and how each of its refusals is classified.

use std::sync::{Arc, Mutex};

use agi_core::peer;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Seen {
    /// Every pairing body the fake host received, as sent.
    bodies: Arc<Mutex<Vec<Value>>>,
    /// The last `x-mizar-server-pin` header the directory saw.
    pin: Arc<Mutex<Option<String>>>,
}

/// The directory: the PIN header decides which refusal to model.
async fn desktops(State(seen): State<Seen>, headers: HeaderMap) -> Response {
    let pin = headers
        .get("x-mizar-server-pin")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    *seen.pin.lock().unwrap() = Some(pin.clone());
    match pin.as_str() {
        "" => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "PIN_REQUIRED" })),
        )
            .into_response(),
        "bad" => (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "SECRET_INVALID" })),
        )
            .into_response(),
        "limited" => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "TOO_MANY_ATTEMPTS" })),
        )
            .into_response(),
        _ => Json(json!({
            "relay": { "version": 1, "protocol": 1, "publicURL": "https://relay.example.com" },
            "desktops": [{
                "room": "ROOM00000001",
                "deviceId": "",
                "name": "Studio Mac",
                "platform": "macos",
                "onlineSince": 1_700_000_000_000i64
            }]
        }))
        .into_response(),
    }
}

/// The target's `/pair`: the code decides success, a wrong-code frame, or 404.
async fn pair(
    State(seen): State<Seen>,
    Path(room): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    seen.bodies.lock().unwrap().push(body.clone());
    // The real host normalises dashes away before comparing.
    let normalized = body
        .get("code")
        .and_then(Value::as_str)
        .map(|code| code.replace('-', ""));
    match normalized.as_deref() {
        Some("K7QP3M2X") => Json(json!({
            "deviceToken": "mz_issued",
            "deviceId": "issued-device",
            "hostId": "host-1",
            "hostName": "Studio Host",
            "protocolVersion": 1
        }))
        .into_response(),
        Some("AUTH0000") => StatusCode::UNAUTHORIZED.into_response(),
        Some("FORBID00") => StatusCode::FORBIDDEN.into_response(),
        Some("GONE0000") => StatusCode::NOT_FOUND.into_response(),
        Some(_) => Json(json!({
            "v": 1,
            "type": "error",
            "code": "PAIRING_CODE_UNKNOWN",
            "message": "that code is not the one this computer is showing"
        }))
        .into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no code", "room": room })),
        )
            .into_response(),
    }
}

async fn fake_relay() -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/selfhost/v1/desktops", get(desktops))
        .route("/r/:room/mizar/v1/pair", post(pair))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), seen)
}

#[tokio::test]
async fn the_directory_reads_the_relay_answer_and_classifies_its_refusals() {
    let (base, seen) = fake_relay().await;

    let listed = peer::desktops(&base, "secret").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].room, "ROOM00000001");
    assert_eq!(listed[0].name, "Studio Mac");
    assert_eq!(listed[0].platform, "macos");
    assert_eq!(listed[0].online_since, 1_700_000_000_000);
    assert_eq!(
        seen.pin.lock().unwrap().as_deref(),
        Some("secret"),
        "the stored PIN travels in x-mizar-server-pin"
    );

    // The contract's three answers, each to the code the window branches on.
    let missing = peer::desktops(&base, "").await.unwrap_err();
    assert_eq!(missing.code, "PIN_REQUIRED");
    let wrong = peer::desktops(&base, "bad").await.unwrap_err();
    assert_eq!(wrong.code, "PIN_INVALID");
    let limited = peer::desktops(&base, "limited").await.unwrap_err();
    assert_eq!(limited.code, "RATE_LIMITED");

    // No stored origin is its own answer, not a network failure.
    let none = peer::desktops("  ", "secret").await.unwrap_err();
    assert_eq!(none.code, "NO_SERVER");
}

#[tokio::test]
async fn pairing_asks_for_a_peer_token_and_stores_what_the_host_issued() {
    let (base, seen) = fake_relay().await;

    // The code is typed lower case with a space, as a person reads it.
    let record = peer::pair(&base, "ROOM00000001", "k7qp 3m2x", "B Mac", "b-device")
        .await
        .unwrap();
    assert_eq!(record.id, "issued-device");
    assert_eq!(record.name, "Studio Host");
    assert_eq!(record.origin, base);
    assert_eq!(record.room, "ROOM00000001");
    assert_eq!(record.device_id, "b-device");
    assert_eq!(record.token, "mz_issued");
    assert!(record.paired_at > 0);

    let bodies = seen.bodies.lock().unwrap();
    let body = bodies.last().expect("the target saw the request");
    assert_eq!(body["peer"], true, "the host must build the marker itself");
    assert_eq!(body["deviceId"], "b-device");
    assert_eq!(body["deviceName"], "B Mac");
    assert_eq!(
        body["code"], "K7QP3M2X",
        "spaces stripped, letters upper-cased"
    );
    assert_eq!(body["protocolVersion"], 1);
    assert!(
        body.get("deviceModel").is_none(),
        "a caller must not be able to claim a marker: {body}"
    );
}

#[tokio::test]
async fn a_refused_code_keeps_the_hosts_own_sentence() {
    let (base, _seen) = fake_relay().await;

    let refused = peer::pair(&base, "ROOM00000001", "ZZZZ-ZZZZ", "B Mac", "b-device")
        .await
        .unwrap_err();
    assert_eq!(refused.code, "PAIRING_CODE_UNKNOWN");
    assert_eq!(
        refused.message,
        "that code is not the one this computer is showing"
    );

    // A room the relay does not know is a machine that is gone, not a bad code.
    let gone = peer::pair(&base, "ROOM00000001", "GONE0000", "B Mac", "b-device")
        .await
        .unwrap_err();
    assert_eq!(gone.code, "HOST_OFFLINE");

    // The relay's own gates, classified rather than shown as a status code.
    let unauthorized = peer::pair(&base, "ROOM00000001", "AUTH0000", "B Mac", "b-device")
        .await
        .unwrap_err();
    assert_eq!(unauthorized.code, "UNAUTHORIZED");
    let forbidden = peer::pair(&base, "ROOM00000001", "FORBID00", "B Mac", "b-device")
        .await
        .unwrap_err();
    assert_eq!(forbidden.code, "FORBIDDEN");
}

#[tokio::test]
async fn a_room_this_server_could_not_have_named_is_refused_before_the_network() {
    let (base, seen) = fake_relay().await;
    let refused = peer::pair(&base, "../elsewhere", "K7QP-3M2X", "B Mac", "b-device")
        .await
        .unwrap_err();
    assert_eq!(refused.code, "MALFORMED_ROOM");
    assert!(seen.bodies.lock().unwrap().is_empty());

    let empty = peer::pair(&base, "ROOM00000001", "   ", "B Mac", "b-device")
        .await
        .unwrap_err();
    assert_eq!(empty.code, "CODE_REQUIRED");
    assert!(seen.bodies.lock().unwrap().is_empty());
}
