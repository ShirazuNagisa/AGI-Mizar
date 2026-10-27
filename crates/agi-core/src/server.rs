//! The loopback HTTP server.
//!
//! One socket, three jobs:
//!
//! - `/dsh` — the DSH helper plugin dials in here. Unauthenticated at the HTTP
//!   layer because the token is inside the protocol's `hello` frame, which is
//!   where the plugin already puts it.
//! - `/api/v1/*` — the UI. Bearer token, loopback only.
//! - `/healthz` — a liveness probe that reveals nothing.
//!
//! ### Why the UI is not on Tauri's IPC
//!
//! A `#[tauri::command]` couples the front end to the shell: it cannot be opened
//! in a browser, cannot be tested without the shell, and cannot be reached by a
//! CLI. This server is the seam. Tauri's only job is to start the kernel, put a
//! window in front of it, and hand the page two values it cannot guess.
//!
//! ### Why loopback only
//!
//! Binding a wildcard address is rejected by construction: the listener is
//! created on `127.0.0.1:0`. A desktop app's control API has no business being
//! reachable from the network it happens to be attached to, and the DSH plugin
//! already runs on this machine.

use std::sync::Arc;

use agi_engine_dsh::DshEngine;
use agi_protocol::api::{Command, StateSnapshot};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::Core;

/// How many frames may queue in each direction on a WebSocket.
const CHANNEL_DEPTH: usize = 64;

/// Tokens this launch accepts.
pub struct Config {
    /// Presented by the UI as a bearer token.
    pub api_token: String,
    /// Presented by the plugin inside its `hello` frame.
    pub plugin_token: String,
}

#[derive(Clone)]
struct AppState {
    core: Arc<Core>,
    dsh: Arc<DshEngine>,
    api_token: Arc<String>,
}

/// Bind the server on a loopback ephemeral port and start serving.
///
/// Returns the port so the caller can publish the endpoint file before anything
/// tries to dial.
pub async fn bind(
    core: Arc<Core>,
    dsh: Arc<DshEngine>,
    config: Config,
) -> anyhow::Result<(u16, JoinHandle<()>)> {
    let state = AppState {
        core,
        dsh,
        api_token: Arc::new(config.api_token),
    };

    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/state", get(state_handler))
        .route("/api/v1/command", post(command_handler))
        .route("/api/v1/events", get(events_handler))
        .route("/dsh", get(dsh_handler))
        .with_state(state);

    // Loopback, ephemeral. `0` asks the OS for a free port, which is what makes
    // two windows (or a dev run beside a packaged build) coexist.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();

    let handle = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::error!("api server stopped: {error}");
        }
    });

    Ok((port, handle))
}

async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

async fn state_handler(State(app): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &app.api_token) {
        return unauthorized();
    }
    let snapshot: StateSnapshot = app.core.snapshot();
    Json(snapshot).into_response()
}

async fn command_handler(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(command): Json<Command>,
) -> Response {
    if !authorized(&headers, &app.api_token) {
        return unauthorized();
    }
    Json(app.core.dispatch(command).await).into_response()
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

async fn events_handler(
    State(app): State<AppState>,
    Query(query): Query<TokenQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !constant_time_eq(&query.token, &app.api_token) {
        return unauthorized();
    }
    upgrade.on_upgrade(move |socket| pump_events(app.core, socket))
}

/// Forward every API event to one client.
async fn pump_events(core: Arc<Core>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let mut events = core.subscribe();

    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        let Ok(text) = serde_json::to_string(&event) else { continue };
                        if sink.send(Message::Text(text)).await.is_err() {
                            return;
                        }
                    }
                    // A client that fell behind is told nothing: the state it
                    // reads next is authoritative, and dropping the socket would
                    // be a worse answer than a missing delta.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
            // Read (and ignore) client frames so a close is noticed promptly.
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    _ => {}
                }
            }
        }
    }
}

async fn dsh_handler(State(app): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| pump_dsh(app.dsh, socket))
}

/// Hand one accepted socket to the DSH engine.
///
/// The engine speaks in text frames, not sockets, so the socket is split into two
/// channels here. That is the whole seam: everything above it is protocol.
async fn pump_dsh(dsh: Arc<DshEngine>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (incoming_tx, incoming_rx) = mpsc::channel::<String>(CHANNEL_DEPTH);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<String>(CHANNEL_DEPTH);

    let reader = tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Text(text)) => {
                    if incoming_tx.send(text).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                // Ping/Pong are answered by the transport; binary is not part of
                // this protocol.
                _ => {}
            }
        }
    });

    let writer = tokio::spawn(async move {
        while let Some(text) = outgoing_rx.recv().await {
            if sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });

    dsh.serve(incoming_rx, outgoing_tx).await;

    reader.abort();
    writer.abort();
}

/// Whether the request carries this launch's API token.
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented, token))
}

/// Compare without an early exit.
///
/// The value being protected is a token on a loopback socket, so this is not the
/// most exposed secret in the system — but a comparison that returns early is the
/// kind of thing that gets copied into a context where it matters.
fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "ok": false, "error": { "code": "UNAUTHORIZED", "message": "missing or wrong token" } })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_by_value_only() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
    }
}
