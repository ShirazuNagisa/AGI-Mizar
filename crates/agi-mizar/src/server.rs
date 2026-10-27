//! The phone-facing server: pairing, devices, and the Mizar v1 WebSocket.
//!
//! Three routes, exactly as the protocol says:
//!
//! | route | auth | job |
//! |---|---|---|
//! | `GET /mizar/v1/hello` | none | "is this address dialable, and whose is it" |
//! | `POST /mizar/v1/pair` | none, one shot | a code for a device token |
//! | `GET /mizar/v1/ws` | device token in the first frame | everything else |
//!
//! ### Why `hello` and `pair` are unauthenticated
//!
//! Because a phone that has never paired has no credential yet. The exposure is
//! bounded on purpose: `hello` reveals a host id and a name, `pair` needs a code
//! that exists for five minutes, is single use, and is rate limited. Nothing else
//! answers without a token.
//!
//! ### What this build does not implement
//!
//! `NOT_IMPLEMENTED` is a real answer, not a stub: the protocol says a client
//! seeing it knows the host is older than it. Methods that need machinery this
//! build does not have yet — uploads, file preview, workspace listing, activity,
//! relay switching, push registration — say so instead of pretending.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agi_core::Core;
use agi_protocol::domain::{InteractionKind, SessionKey};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::frames::{
    record_to_event, ClientFrame, ErrorCode, ServerFrame, SessionRow, WelcomeInput, HTTP_PREFIX,
    PROTOCOL_VERSION, WS_PATH,
};
use crate::pairing::{self, PairingCode};
use crate::relay::RelayClient;
use crate::store::DeviceStore;

/// How many pairing attempts a minute are allowed.
const PAIR_ATTEMPTS_PER_MINUTE: usize = 5;

/// The live pairing code, if one was issued and has not been used.
struct ActiveCode {
    code: PairingCode,
}

/// The Mizar server.
pub struct MizarServer {
    core: Arc<Core>,
    store: Arc<DeviceStore>,
    /// The DSH home, reported in `host.info` and `welcome`.
    home: String,
    code: Mutex<Option<ActiveCode>>,
    attempts: Mutex<VecDeque<Instant>>,
    port: AtomicU16,
    /// The tunnel, when one is configured.
    relay: Mutex<Option<Arc<RelayClient>>>,
}

impl MizarServer {
    /// Build the server over a kernel and a device roster.
    pub fn new(core: Arc<Core>, store: Arc<DeviceStore>, home: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            core,
            store,
            home: home.into(),
            code: Mutex::new(None),
            attempts: Mutex::new(VecDeque::new()),
            port: AtomicU16::new(0),
            relay: Mutex::new(None),
        })
    }

    fn code(&self) -> MutexGuard<'_, Option<ActiveCode>> {
        self.code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn attempts(&self) -> MutexGuard<'_, VecDeque<Instant>> {
        self.attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Attach a tunnel, so `host.info` can tell a phone where to find it from
    /// another network.
    pub fn attach_relay(&self, relay: Arc<RelayClient>) {
        *self
            .relay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(relay);
    }

    /// The tunnel facts a phone reads, when a tunnel is configured.
    fn relay_facts(&self) -> Value {
        let guard = self
            .relay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(relay) = guard.as_ref() else {
            return Value::Null;
        };
        json!({
            "url": relay.base(),
            "room": relay.current_room(),
            "baseURL": relay.phone_base_url(),
        })
    }

    /// The port the listener took.
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    /// Issue a fresh pairing code, replacing any previous one.
    ///
    /// Replacing rather than accumulating is deliberate: two live codes are two
    /// things that can leak, and the operator only ever shows one.
    pub fn issue_code(&self) -> PairingCode {
        let code = pairing::generate(pairing::now_ms());
        *self.code() = Some(ActiveCode { code: code.clone() });
        code
    }

    /// The `mizar://pair?...` URI for a code, using the address a phone can reach.
    pub fn pairing_uri(&self, code: &PairingCode) -> String {
        let port = self.port();
        let host = crate::store::local_address()
            .map(|address| format!("{address}:{port}"))
            .unwrap_or_else(|| format!("127.0.0.1:{port}"));
        crate::frames::pairing_uri(&host, &code.display, &self.store.host_name())
    }

    /// Bind the listener and start serving.
    ///
    /// `port` of `0` asks the OS for a free one; the default is the protocol's
    /// 19392, which is where a paired phone will look first.
    pub async fn bind(self: &Arc<Self>, port: u16) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let router = Router::new()
            .route(&format!("{HTTP_PREFIX}/hello"), get(hello))
            .route(&format!("{HTTP_PREFIX}/pair"), post(pair))
            .route(WS_PATH, get(upgrade))
            .with_state(Arc::clone(self));

        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        let bound = listener.local_addr()?.port();
        self.port.store(bound, Ordering::Relaxed);

        Ok(tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::error!("mizar listener stopped: {error}");
            }
        }))
    }

    /// Whether a pairing attempt is allowed right now.
    fn allow_attempt(&self) -> bool {
        let now = Instant::now();
        let mut attempts = self.attempts();
        while attempts
            .front()
            .is_some_and(|at| now.duration_since(*at) > Duration::from_secs(60))
        {
            attempts.pop_front();
        }
        if attempts.len() >= PAIR_ATTEMPTS_PER_MINUTE {
            return false;
        }
        attempts.push_back(now);
        true
    }

    /// One Mizar method call.
    async fn call(&self, frame: &ClientFrame) -> ServerFrame {
        let id = frame.id.clone().unwrap_or_default();

        match frame.kind.as_str() {
            "host.info" => {
                let port = self.port();
                let addresses: Vec<String> = crate::store::local_address()
                    .map(|address| vec![format!("{address}:{port}")])
                    .unwrap_or_default()
                    .into_iter()
                    .chain(std::iter::once(format!("127.0.0.1:{port}")))
                    .collect();
                ServerFrame::ok(
                    id,
                    json!({
                        "hostId": self.store.host_id(),
                        "hostName": self.store.host_name(),
                        "home": self.home,
                        "protocolVersion": PROTOCOL_VERSION,
                        "addresses": addresses,
                        "desktop": { "os": os_name(), "client": "AGI Mizar" },
                        // `null` when no tunnel is configured, which the phone
                        // reads as "there is no relay route".
                        "relay": self.relay_facts(),
                    }),
                )
            }

            "session.list" => {
                let snapshot = self.core.snapshot();
                let items: Vec<SessionRow> = snapshot
                    .sessions
                    .iter()
                    .map(|session| SessionRow {
                        session_id: session.key.to_string(),
                        updated_at: session.updated_at,
                        running: session.running,
                        blank: session.title.is_none() && session.turns == 0,
                        agent_available: true,
                        cwd: session.cwd.clone(),
                        parent_session_id: session.parent.clone(),
                        origin: session.origin.clone(),
                        engine: session.key.engine.as_str().to_string(),
                    })
                    .collect();
                ServerFrame::ok(id, json!({ "items": items }))
            }

            "session.messages" => {
                let Some(key) = self.session_of(frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.messages needs a sessionId",
                    );
                };
                let before = frame.payload.get("beforeSeq").and_then(Value::as_i64);
                let limit = frame
                    .payload
                    .get("maxMessages")
                    .and_then(Value::as_u64)
                    .map(|value| value.clamp(1, 400) as usize)
                    .unwrap_or(200);

                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        format!("no engine for {}", key.engine),
                    );
                };
                let context = self
                    .core
                    .snapshot()
                    .sessions
                    .iter()
                    .find(|session| session.key == key)
                    .and_then(|session| session.context)
                    .map(|pressure| {
                        json!({
                            "window": pressure.window,
                            "pressureTokens": pressure.used,
                            "projectedTokens": pressure.used,
                            "percent": pressure.percent,
                        })
                    })
                    .unwrap_or(Value::Null);

                match engine.records(&key, limit, before).await {
                    Ok(records) => {
                        let as_of = records.last().map(|record| record.seq).unwrap_or(0);
                        let events: Vec<Value> =
                            records.iter().filter_map(record_to_event).collect();
                        ServerFrame::ok(
                            id,
                            json!({
                                "sessionId": key.to_string(),
                                "asOfSeq": as_of,
                                "records": events,
                                "hasMore": records.len() >= limit,
                                // The phone merges this into its cached usage frame;
                                // `null` means "this host cannot say", not zero.
                                "context": context,
                            }),
                        )
                    }
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            "session.prompt" => {
                let Some(key) = self.session_of(frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.prompt needs a sessionId",
                    );
                };
                let text = frame
                    .payload
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if text.trim().is_empty() {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.prompt needs text",
                    );
                }
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(Some(id), ErrorCode::RemoteError, "no such engine");
                };
                let request_id = frame
                    .payload
                    .get("requestId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                match engine.prompt(&key, text).await {
                    Ok(()) => {
                        ServerFrame::ok(id, json!({ "accepted": true, "requestId": request_id }))
                    }
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            "session.cancel" => {
                let Some(key) = self.session_of(frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.cancel needs a sessionId",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(Some(id), ErrorCode::RemoteError, "no such engine");
                };
                match engine.cancel(&key).await {
                    Ok(()) => ServerFrame::ok(id, json!({ "accepted": true })),
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            "interactions.pending" => {
                let snapshot = self.core.snapshot();
                let approvals: Vec<Value> = snapshot
                    .interactions
                    .iter()
                    .filter(|interaction| interaction.kind == InteractionKind::Approval)
                    .map(|interaction| {
                        json!({
                            "requestId": interaction.id,
                            "sessionId": interaction.session.as_ref().map(|key| key.to_string()),
                            "toolName": interaction.title,
                            "callId": Value::Null,
                            "reason": interaction.detail,
                            "displayReason": Value::Null,
                            "requestedAt": interaction.requested_at,
                        })
                    })
                    .collect();
                // Questions are not modelled on this host yet. An empty list is
                // the honest answer: it says "nothing is waiting", which is true,
                // rather than inventing a question shape the phone cannot answer.
                ServerFrame::ok(id, json!({ "approvals": approvals, "questions": [] }))
            }

            "approval.answer" => {
                let Some(request_id) = frame.payload.get("requestId").and_then(Value::as_str)
                else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "approval.answer needs requestId",
                    );
                };
                let outcome = frame
                    .payload
                    .get("outcome")
                    .and_then(Value::as_str)
                    .unwrap_or("rejected")
                    .to_string();

                // The kernel already maps an interaction id to the engine that
                // asked; resolving it here would be a second copy of that table.
                let command = agi_protocol::api::Command::Answer {
                    id: request_id.to_string(),
                    outcome: Some(outcome),
                    answers: None,
                };
                let response = self.core.dispatch(command).await;
                match (response.ok, response.error) {
                    (true, _) => ServerFrame::ok(id, json!({ "accepted": true })),
                    (false, Some(error)) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(&error.code),
                        error.message,
                    ),
                    _ => ServerFrame::error(Some(id), ErrorCode::Internal, "the answer failed"),
                }
            }

            "session.model.set" => {
                let Some(key) = self.session_of(frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.model.set needs a sessionId",
                    );
                };
                let (Some(provider), Some(model)) = (
                    frame.payload.get("provider").and_then(Value::as_str),
                    frame.payload.get("model").and_then(Value::as_str),
                ) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.model.set needs provider and model",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(Some(id), ErrorCode::RemoteError, "no such engine");
                };
                // DSH selects by provider *and* model; Codex and Claude Code
                // take the model alone. The phone always sends both, so the
                // join happens here rather than in three adapters.
                let name = if key.engine == agi_protocol::engine::EngineId::Dsh {
                    format!("{provider}/{model}")
                } else {
                    model.to_string()
                };
                match engine.set_model(&key, &name).await {
                    Ok(()) => ServerFrame::ok(
                        id,
                        json!({
                            "sessionId": key.to_string(),
                            "selected": {
                                "provider": provider,
                                "model": model,
                                "reasoningEffort": frame.payload.get("reasoningEffort").cloned(),
                            }
                        }),
                    ),
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            "session.permission.set" => {
                let Some(key) = self.session_of(frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.permission.set needs a sessionId",
                    );
                };
                let Some(preset) = frame.payload.get("preset").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.permission.set needs preset",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(Some(id), ErrorCode::RemoteError, "no such engine");
                };
                match engine.set_permission(&key, preset).await {
                    Ok(()) => {
                        ServerFrame::ok(id, json!({ "commandResult": { "kind": "success" } }))
                    }
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            // Everything else is a method this build has not grown yet. Saying so
            // is what lets the phone tell "older host" from "broken host".
            other => ServerFrame::error(
                Some(id),
                ErrorCode::NotImplemented,
                format!("{other} is not implemented by this host"),
            ),
        }
    }

    /// The session a frame names, if it names a valid one.
    fn session_of(&self, frame: &ClientFrame) -> Option<SessionKey> {
        frame
            .payload
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(|text| text.parse().ok())
    }
}

/// `GET /mizar/v1/hello` — unauthenticated, reveals identity only.
async fn hello(State(server): State<Arc<MizarServer>>) -> Response {
    Json(json!({
        "hostId": server.store.host_id(),
        "hostName": server.store.host_name(),
        "home": server.home,
        "protocolVersion": PROTOCOL_VERSION,
    }))
    .into_response()
}

/// `POST /mizar/v1/pair` — a code for a token, once.
async fn pair(State(server): State<Arc<MizarServer>>, Json(body): Json<Value>) -> Response {
    let version = body
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .unwrap_or(PROTOCOL_VERSION as u64);
    if version != PROTOCOL_VERSION as u64 {
        return frame_response(ServerFrame::error(
            None,
            ErrorCode::ProtocolVersionMismatch,
            format!("this host speaks protocol {PROTOCOL_VERSION}"),
        ));
    }

    if !server.allow_attempt() {
        return frame_response(ServerFrame::error(
            None,
            ErrorCode::RateLimited,
            "too many pairing attempts; wait a minute",
        ));
    }

    let Some(supplied) = body.get("code").and_then(Value::as_str) else {
        return frame_response(ServerFrame::error(
            None,
            ErrorCode::PairingCodeInvalid,
            "no code supplied",
        ));
    };
    let Some(canonical) = pairing::normalise(supplied) else {
        return frame_response(ServerFrame::error(
            None,
            ErrorCode::PairingCodeInvalid,
            "that is not a pairing code",
        ));
    };

    let accepted = {
        let mut slot = server.code();
        match slot.as_ref() {
            None => Err(ErrorCode::PairingCodeUnknown),
            Some(active) if !pairing::constant_time_eq(&active.code.canonical, &canonical) => {
                Err(ErrorCode::PairingCodeUnknown)
            }
            Some(active) if pairing::now_ms() > active.code.expires_at => {
                // Expired codes are cleared: keeping them alive would let a
                // retry succeed after the operator has been told to reissue.
                *slot = None;
                Err(ErrorCode::PairingCodeExpired)
            }
            Some(_) => {
                // Single use: consuming it here means two phones racing for the
                // same code cannot both win.
                *slot = None;
                Ok(())
            }
        }
    };

    if let Err(code) = accepted {
        return frame_response(ServerFrame::error(None, code, code_message(code)));
    }

    let name = body
        .get("deviceName")
        .and_then(Value::as_str)
        .unwrap_or("Phone");
    let model = body
        .get("deviceModel")
        .and_then(Value::as_str)
        .map(str::to_string);

    match server.store.issue(name, model) {
        Ok(issued) => Json(json!({
            "deviceToken": issued.token,
            "deviceId": issued.device.id,
            "hostId": server.store.host_id(),
            "hostName": server.store.host_name(),
            "protocolVersion": PROTOCOL_VERSION,
        }))
        .into_response(),
        Err(error) => frame_response(ServerFrame::error(
            None,
            ErrorCode::Internal,
            format!("could not record the device: {error}"),
        )),
    }
}

/// The message that goes with a pairing failure code.
fn code_message(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::PairingCodeUnknown => "that code is not the one this computer is showing",
        ErrorCode::PairingCodeExpired => "that code has expired; ask the computer for a new one",
        _ => "that code cannot be used",
    }
}

/// `GET /mizar/v1/ws` — the phone's socket.
async fn upgrade(State(server): State<Arc<MizarServer>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| serve_socket(server, socket))
}

/// Run one phone connection.
///
/// One writer task owns the sink and drains a channel; the read loop and the
/// event forwarder both produce into it. Two tasks writing to one sink is the
/// kind of bug that only shows up as a truncated frame under load.
async fn serve_socket(server: Arc<MizarServer>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out, mut out_rx) = tokio::sync::mpsc::channel::<String>(64);

    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });

    // The handshake is the only frame allowed to be unauthenticated.
    let first = match stream.next().await {
        Some(Ok(Message::Text(text))) => text,
        _ => return,
    };
    let Some(frame) = ClientFrame::parse(&first) else {
        let _ = send(
            &out,
            ServerFrame::error(
                None,
                ErrorCode::MalformedFrame,
                "the first frame must be a hello",
            ),
        )
        .await;
        drop(out);
        let _ = writer.await;
        return;
    };
    let Some(hello) = frame.hello() else {
        let _ = send(
            &out,
            ServerFrame::error(None, ErrorCode::Unauthenticated, "send hello first"),
        )
        .await;
        drop(out);
        let _ = writer.await;
        return;
    };
    let Some(device) = server.store.authenticate(&hello.token) else {
        let _ = send(
            &out,
            ServerFrame::error(
                None,
                ErrorCode::DeviceRevoked,
                "this device is not paired, or was revoked",
            ),
        )
        .await;
        drop(out);
        let _ = writer.await;
        return;
    };

    tracing::info!(
        "mizar: {} connected ({})",
        device.name,
        hello.app_version.as_deref().unwrap_or("unknown version")
    );

    let _ = send(
        &out,
        ServerFrame::welcome(WelcomeInput {
            host_id: server.store.host_id(),
            host_name: server.store.host_name(),
            home: server.home.clone(),
            device_id: device.id.clone(),
        }),
    )
    .await;

    // Interaction notifications, so an approval reaches the phone without a poll.
    let mut events = server.core.subscribe();
    let forwarder = {
        let out = out.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if let Some(frame) = interaction_event(&event) {
                            if send(&out, frame).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        })
    };

    while let Some(message) = stream.next().await {
        let Ok(Message::Text(text)) = message else {
            break;
        };
        let Some(frame) = ClientFrame::parse(&text) else {
            continue;
        };
        // A second hello on a live socket is a client bug, not a re-handshake.
        if frame.kind == "hello" {
            continue;
        }
        let answer = server.call(&frame).await;
        if send(&out, answer).await.is_err() {
            break;
        }
    }

    forwarder.abort();
    drop(out);
    let _ = writer.await;
    tracing::info!("mizar: {} disconnected", device.name);
}

/// Write one frame to the connection's writer task.
async fn send(
    out: &tokio::sync::mpsc::Sender<String>,
    frame: ServerFrame,
) -> Result<(), tokio::sync::mpsc::error::SendError<String>> {
    let text = serde_json::to_string(&frame).unwrap_or_else(|_| "{}".to_string());
    out.send(text).await
}

/// Translate one kernel event into the frame the phone understands.
fn interaction_event(event: &agi_protocol::api::ApiEvent) -> Option<ServerFrame> {
    use agi_protocol::api::ApiEvent;
    match event {
        ApiEvent::InteractionRequested { interaction } => match interaction.kind {
            InteractionKind::Approval => Some(ServerFrame::event(
                "approval.request",
                json!({
                    "requestId": interaction.id,
                    "sessionId": interaction.session.as_ref().map(|key| key.to_string()),
                    "toolName": interaction.title,
                    "callId": Value::Null,
                    "reason": interaction.detail,
                    "displayReason": Value::Null,
                    "requestedAt": interaction.requested_at,
                }),
            )),
            // Questions are not modelled yet; the count still moves.
            InteractionKind::Question => Some(ServerFrame::event(
                "interactions.changed",
                json!({ "approvals": 0, "questions": 1 }),
            )),
        },
        ApiEvent::InteractionResolved { id } => Some(ServerFrame::event(
            "approval.resolved",
            json!({ "requestId": id, "outcome": "answered" }),
        )),
        _ => None,
    }
}

/// The OS name the phone displays.
fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Linux"
    }
}

/// Wrap a frame as an HTTP response body.
fn frame_response(frame: ServerFrame) -> Response {
    Json(serde_json::to_value(frame).unwrap_or(Value::Null)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_names_are_display_names() {
        assert!(["macOS", "Windows", "Linux"].contains(&os_name()));
    }

    #[test]
    fn an_approval_event_carries_the_fields_the_phone_reads() {
        let event = agi_protocol::api::ApiEvent::InteractionRequested {
            interaction: agi_protocol::domain::Interaction {
                id: "a1".to_string(),
                session: Some(SessionKey::new(agi_protocol::engine::EngineId::Dsh, "s1")),
                kind: InteractionKind::Approval,
                title: "bash".to_string(),
                detail: Some("rm -rf build".to_string()),
                requested_at: 1,
            },
        };
        let frame = interaction_event(&event).unwrap();
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["event"], "approval.request");
        assert_eq!(value["data"]["requestId"], "a1");
        assert_eq!(value["data"]["toolName"], "bash");
        assert_eq!(value["data"]["sessionId"], "dsh:s1");
    }
}
