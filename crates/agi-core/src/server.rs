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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agi_engine_claude::ClaudeEngine;
use agi_engine_dsh::DshEngine;
use agi_protocol::api::{Command, StateSnapshot};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::header;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
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
    /// Presented by the hook shim on every hook invocation.
    pub hook_token: String,
}

#[derive(Clone)]
struct AppState {
    core: Arc<Core>,
    dsh: Arc<DshEngine>,
    /// Present when a `claude` executable was found. Absent means the hook route
    /// answers 404, which is the honest answer on a machine without Claude Code.
    claude: Option<Arc<ClaudeEngine>>,
    api_token: Arc<String>,
    hook_token: Arc<String>,
    /// How many UI sockets are open. Claude's `PreToolUse` only blocks while
    /// somebody is watching.
    watchers: Arc<AtomicUsize>,
}

/// Bind the server on a loopback ephemeral port and start serving.
///
/// Returns the port so the caller can publish the endpoint file before anything
/// tries to dial.
pub async fn bind(
    core: Arc<Core>,
    dsh: Arc<DshEngine>,
    claude: Option<Arc<ClaudeEngine>>,
    config: Config,
) -> anyhow::Result<(u16, JoinHandle<()>)> {
    let state = AppState {
        core,
        dsh,
        claude,
        api_token: Arc::new(config.api_token),
        hook_token: Arc::new(config.hook_token),
        watchers: Arc::new(AtomicUsize::new(0)),
    };

    let router = Router::new()
        // The window loads this. A placeholder until the React app exists, but a
        // real one: it reads the live state through the same API a front end
        // will, so the seam is exercised from the first build.
        .route("/", get(page))
        .route("/assets/:file", get(asset))
        .route("/healthz", get(healthz))
        .route("/api/v1/state", get(state_handler))
        .route("/api/v1/command", post(command_handler))
        .route("/api/v1/events", get(events_handler))
        .route("/dsh", get(dsh_handler))
        .route("/hooks/claude/:event", post(hook_handler))
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

/// The React app, embedded at build time.
///
/// A build without `ui/dist` still works: the fallback page below draws the same
/// data through the same API, so `cargo test` and a fresh clone are not blocked
/// on a front-end build.
#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist"]
struct Ui;

/// The window's entry point.
async fn page() -> Response {
    match Ui::get("index.html") {
        Some(file) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            file.data.into_owned(),
        )
            .into_response(),
        None => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            PLACEHOLDER_PAGE.as_bytes().to_vec(),
        )
            .into_response(),
    }
}

/// One built asset.
async fn asset(AxumPath(file): AxumPath<String>) -> Response {
    // A path with a separator is not a name we produced; refusing keeps a
    // request from walking out of the embedded tree.
    if file.contains('/') || file.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Ui::get(&format!("assets/{file}")) {
        Some(embedded) => (
            [(header::CONTENT_TYPE, content_type(&file))],
            embedded.data.into_owned(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A content type from a file extension.
fn content_type(file: &str) -> &'static str {
    if file.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if file.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if file.ends_with(".svg") {
        "image/svg+xml"
    } else if file.ends_with(".woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}

/// Deliberately plain: no build step, no framework, no design. Its whole job is
/// to prove that a window can reach the kernel and draw what it says.
const PLACEHOLDER_PAGE: &str = r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<title>AGI Mizar</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 13px/1.5 -apple-system, "Segoe UI", system-ui, sans-serif; margin: 0; padding: 24px; }
  h1 { font-size: 15px; font-weight: 600; margin: 0 0 4px; }
  .sub { opacity: .6; margin-bottom: 24px; }
  table { border-collapse: collapse; width: 100%; }
  td { padding: 6px 8px; border-bottom: 1px solid color-mix(in srgb, currentColor 12%, transparent); }
  td.engine { width: 90px; opacity: .6; }
  .dot { display: inline-block; width: 7px; height: 7px; border-radius: 50%; margin-right: 6px; }
  .on { background: #2f9e44; } .off { background: #adb5bd; }
  .empty { opacity: .5; padding: 16px 0; }
</style>
<h1>AGI Mizar</h1>
<div class="sub" id="sub">loading…</div>
<table id="engines"></table>
<div id="sessions"></div>
<script>
  const token = new URLSearchParams(location.search).get('token') || '';
  const headers = { authorization: 'Bearer ' + token };
  const escape = (text) => String(text ?? '').replace(/[<>&]/g, (c) => ({'<':'&lt;','>':'&gt;','&':'&amp;'}[c]));

  async function draw() {
    const response = await fetch('/api/v1/state', { headers });
    if (!response.ok) {
      document.getElementById('sub').textContent = 'kernel refused the window: ' + response.status;
      return;
    }
    const state = await response.json();
    document.getElementById('sub').textContent =
      state.engines.map((engine) => engine.engine + (engine.connected ? '' : ' (off)')).join(' · ') +
      ' — ' + state.sessions.length + ' sessions';

    document.getElementById('engines').innerHTML = state.engines.map((engine) => `
      <tr>
        <td class="engine"><span class="dot ${engine.connected ? 'on' : 'off'}"></span>${escape(engine.engine)}</td>
        <td>${escape(engine.detail || '')}</td>
        <td style="text-align:right;opacity:.6">${engine.sessions}</td>
      </tr>`).join('');

    const rows = state.sessions.slice(0, 40).map((session) => `
      <tr>
        <td class="engine">${escape(session.key.split(':')[0])}</td>
        <td>${escape(session.title || session.cwd || session.key)}</td>
      </tr>`).join('');
    document.getElementById('sessions').innerHTML =
      rows === '' ? '<div class="empty">No sessions yet.</div>' : '<table>' + rows + '</table>';
  }

  draw();
  setInterval(draw, 2000);
</script>
"#;

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
    /// Ask for the binary state channel on this socket (`?state=1`).
    ///
    /// Off by default, and the default is what a 0.9.8 client gets: it neither
    /// asks nor would understand a binary frame, so it must never receive one.
    #[serde(default)]
    state: Option<String>,
}

async fn events_handler(
    State(app): State<AppState>,
    Query(query): Query<TokenQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !constant_time_eq(&query.token, &app.api_token) {
        return unauthorized();
    }
    let attendance = Attendance::join(&app.claude, &app.watchers);
    let wants_states = query.state.as_deref() == Some("1") && state_channel_enabled();
    upgrade.on_upgrade(move |socket| pump_events(app.core, socket, attendance, wants_states))
}

/// Forward every API event to one client.
async fn pump_events(
    core: Arc<Core>,
    socket: WebSocket,
    _attendance: Attendance,
    wants_states: bool,
) {
    let (mut sink, mut stream) = socket.split();
    let mut events = core.subscribe();
    let mut states = core.subscribe_states();
    let mut wants_states = wants_states;

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
            // The state channel, for clients that asked for it. Binary frames
            // carry their own type byte, so a client can tell them from the
            // JSON events without any envelope.
            frame = states.recv(), if wants_states => {
                match frame {
                    Ok((session, bytes)) => {
                        // The socket carries many sessions; the frame does not
                        // name one. A one-byte length prefix and the session id
                        // make the binding explicit without touching the frame
                        // format the phone already decodes.
                        let mut payload = Vec::with_capacity(bytes.len() + 1 + session.id.len());
                        payload.push(session.id.len().min(255) as u8);
                        payload.extend_from_slice(session.id.as_bytes());
                        payload.extend_from_slice(&bytes);
                        if sink.send(Message::Binary(payload.into())).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
            // Read (and ignore) client frames so a close is noticed promptly.
            frame = stream.next() => {
                match frame {
                    // The one client frame this socket acts on: asking for the
                    // state channel. Everything else is still ignored -- the
                    // socket is a push channel, not a command endpoint.
                    Some(Ok(Message::Text(text))) => {
                        if !wants_states
                            && state_channel_enabled()
                            && is_state_subscribe(&text)
                        {
                            wants_states = true;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    _ => {}
                }
            }
        }
    }
}

/// Counts UI sockets, and tells the Claude engine whether anyone is watching.
///
/// A `PreToolUse` hook blocks until the operator decides. Blocking when no window
/// is open would stall the operator's own terminal for a minute per tool call,
/// which is worse than the hook not existing — so the count is what makes the
/// feature safe to install.
struct Attendance {
    engine: Option<Arc<ClaudeEngine>>,
    count: Arc<AtomicUsize>,
}

impl Attendance {
    fn join(engine: &Option<Arc<ClaudeEngine>>, count: &Arc<AtomicUsize>) -> Self {
        if let Some(engine) = engine {
            count.fetch_add(1, Ordering::Relaxed);
            engine.set_attended(true);
        }
        Self {
            engine: engine.clone(),
            count: Arc::clone(count),
        }
    }
}

impl Drop for Attendance {
    fn drop(&mut self) {
        let Some(engine) = &self.engine else { return };
        let remaining = self.count.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
        if remaining == 0 {
            engine.set_attended(false);
        }
    }
}

/// One hook invocation from Claude Code, relayed to the engine.
///
/// The answer is whatever the engine produced: a permission decision for
/// `PreToolUse`, or nothing at all — which tells the shim to print nothing and
/// leaves Claude Code's own behaviour untouched.
async fn hook_handler(
    State(app): State<AppState>,
    AxumPath(event): AxumPath<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !authorized(&headers, &app.hook_token) {
        return unauthorized();
    }
    let Some(claude) = &app.claude else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "the hook payload is not JSON").into_response();
    };
    match claude.handle_hook(&event, &payload).await {
        Some(answer) => Json(answer).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn dsh_handler(State(app): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    // A page of a long session is megabytes of JSON in one frame. The default
    // sixteen-megabyte ceiling is close enough that one big page kills the link
    // mid-read, so leave room for the largest page this side asks for.
    upgrade
        .max_message_size(64 * 1024 * 1024)
        .max_frame_size(64 * 1024 * 1024)
        .on_upgrade(move |socket| pump_dsh(app.dsh, socket))
}

/// Hand one accepted socket to the DSH engine.
///
/// The socket has exactly one owner. Splitting it between a reader and a writer
/// looks tidier and loses the transport's control frames: the plugin's watchdog
/// pings this side and closes the link after two missed replies, which showed up
/// as a reconnect every minute. Handling both directions in one loop lets every
/// ping be answered, and gives the reconnect kick a single place to hang up.
async fn pump_dsh(dsh: Arc<DshEngine>, mut socket: WebSocket) {
    let (incoming_tx, incoming_rx) = mpsc::channel::<String>(CHANNEL_DEPTH);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<String>(CHANNEL_DEPTH);

    let mut kick = dsh.kick_receiver();
    let engine = Arc::clone(&dsh);
    let serve = dsh.serve(incoming_rx, outgoing_tx);
    tokio::pin!(serve);

    let hang_up = loop {
        tokio::select! {
            // The engine finished on its own (the plugin sent a close, or the
            // protocol said so); its own detach has already run.
            _ = &mut serve => break false,
            // Somebody asked this link to be dropped: hanging up is how the
            // plugin is told to dial again.
            _ = kick.recv() => break true,
            frame = socket.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    if incoming_tx.send(text).await.is_err() {
                        break true;
                    }
                }
                // Answering the plugin's watchdog is what keeps this link up.
                Some(Ok(Message::Ping(payload))) => {
                    let _ = socket.send(Message::Pong(payload)).await;
                    let _ = socket.flush().await;
                }
                Some(Ok(Message::Close(frame))) => {
                    tracing::debug!("dsh link: plugin asked to close ({frame:?})");
                    break true;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    tracing::warn!("dsh link: socket failed — {error}");
                    break true;
                }
                None => {
                    tracing::debug!("dsh link: socket ended");
                    break true;
                }
            },
            outgoing = outgoing_rx.recv() => match outgoing {
                Some(text) => {
                    if socket.send(Message::Text(text)).await.is_err() {
                        break true;
                    }
                }
                None => break true,
            },
        }
    };

    // Dropping the serve future is how the channels close. If it never reached
    // its own detach (every path but the first), do that here, or the engine
    // keeps claiming a connection whose socket is gone.
    drop(incoming_tx);
    if hang_up {
        DshEngine::forget_link(&engine);
    }
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

/// Whether this build offers the state channel at all.
///
/// Off unless `MIZAR_STATE_CHANNEL=1`. The experimental channel runs on one
/// relay host only; a build without the variable behaves exactly as before, so a
/// client cannot discover a channel that is not meant to be there.
fn state_channel_enabled() -> bool {
    std::env::var("MIZAR_STATE_CHANNEL").map(|v| v == "1").unwrap_or(false)
}

/// Whether a client frame asks for the state channel.
///
/// Deliberately narrow: an exact `{"method":"state.subscribe"}` and nothing
/// else. A typo is silence, not a half-subscription.
fn is_state_subscribe(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    value.get("method").and_then(Value::as_str) == Some("state.subscribe")
}
