//! Outbound Mizar v1 client for same-account remote agents.
//!
//! This is the transport half of the delegation feature: it knows the Mizar v1
//! envelope, the relay's account-gate headers, and how to wait for one turn to
//! settle. It knows nothing about the kernel's store, UI or policies.
//!
//! ### Why a separate crate
//!
//! The kernel already depends on `agi-mizar` (the server). Putting the client
//! there would make the server crate depend on its own consumer; putting it in
//! `agi-core` would make the kernel own the wire. A small crate with one seam
//! keeps the protocol checks testable without an HTTP server or a GUI.
//!
//! ### What it deliberately does not do
//!
//! It does not mint tokens, read `account.json`, retry forever, or decide who
//! may be delegated to. Those belong to the account service and the delegation
//! manager. This layer takes credentials that were already authorised.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

/// How long one method call may take before it is reported as a timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the hello/welcome handshake may take.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// How often `run_turn` reads the next page while a turn is running.
const TURN_POLL: Duration = Duration::from_millis(1_000);

/// How long a remote approval may wait before this client rejects it.
///
/// The parent model may be busy or asleep; a target agent must not hang for the
/// full thirty-minute turn budget because nobody looked. Five minutes matches
/// the product decision and the target's own fail-closed posture.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// What went wrong talking to one peer.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("the peer is not connected")]
    Closed,
    #[error("the peer refused the upgrade: {0}")]
    Upgrade(String),
    #[error("the peer answered with {code}: {message}")]
    Protocol { code: String, message: String },
    #[error("the peer did not answer in time")]
    Timeout,
    #[error("the peer's frame was not readable: {0}")]
    Json(String),
    #[error("the peer link failed: {0}")]
    Io(String),
    #[error("{0}")]
    Other(String),
}

/// Everything needed to open one authenticated peer socket.
///
/// `account_session` is the account bearer the relay gate checks; `peer_token`
/// is the target-minted device token the Mizar host authenticates in `hello`.
/// They are separate credentials on purpose: the relay must not be able to mint
/// a peer token, and the target must not need the account bearer.
#[derive(Clone)]
pub struct Credentials {
    /// Account service origin, `https://…`.
    pub base: String,
    /// The target's relay room.
    pub room: String,
    /// Account session token (relay gate).
    pub account_session: String,
    /// This desktop's account device id (relay client id).
    pub device_id: String,
    /// This account's user id (hello.accountId).
    pub account_id: String,
    /// The target-minted peer token (hello.token).
    pub peer_token: String,
    /// What this client reports as its app version.
    pub app_version: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("base", &self.base)
            .field("room", &self.room)
            .field("device_id", &self.device_id)
            .field("account_id", &self.account_id)
            .field("app_version", &self.app_version)
            .finish_non_exhaustive()
    }
}

/// In-flight method calls, keyed by the request id echoed on the answer.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, PeerError>>>>>;

/// What the host answered to `hello`.
#[derive(Debug, Clone, Deserialize)]
pub struct Welcome {
    #[serde(rename = "hostId", default)]
    pub host_id: String,
    #[serde(rename = "hostName", default)]
    pub host_name: String,
    #[serde(rename = "protocolVersion", default = "one")]
    pub protocol_version: u32,
    #[serde(rename = "deviceId", default)]
    pub device_id: String,
}

fn one() -> u32 {
    1
}

/// One server-pushed event, for approvals and notices later.
#[derive(Debug, Clone)]
pub enum PeerEvent {
    Event { name: String, data: Value },
    Closed,
}

/// One page of `session.messages`.
#[derive(Debug, Clone)]
pub struct MessagesPage {
    pub as_of_seq: i64,
    pub records: Vec<Value>,
}

/// What one settled turn produced.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub turn: u32,
    pub reason: String,
    pub final_text: Option<String>,
}

/// One live Mizar peer connection.
pub struct PeerClient {
    tx: mpsc::Sender<Message>,
    pending: Pending,
    events: broadcast::Sender<PeerEvent>,
    welcome: Welcome,
    next_id: AtomicU64,
}

impl std::fmt::Debug for PeerClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerClient")
            .field("host_name", &self.welcome.host_name)
            .finish_non_exhaustive()
    }
}

impl PeerClient {
    /// Dial the relay's room and complete the Mizar hello.
    pub async fn connect(credentials: Credentials) -> Result<Self, PeerError> {
        let url = ws_url(&credentials.base, &credentials.room)?;
        let mut request = url
            .into_client_request()
            .map_err(|error| PeerError::Upgrade(error.to_string()))?;
        let account_hash = format!(
            "{:x}",
            Sha256::digest(credentials.account_session.as_bytes())
        );
        let headers = request.headers_mut();
        for (name, value) in [
            ("x-mizar-session", credentials.account_session.as_str()),
            ("x-mizar-client-id", credentials.device_id.as_str()),
            ("x-mizar-account", account_hash.as_str()),
        ] {
            headers.insert(
                name,
                HeaderValue::from_str(value)
                    .map_err(|error| PeerError::Other(format!("bad {name}: {error}")))?,
            );
        }
        headers.insert(
            "user-agent",
            HeaderValue::from_str(&format!("agi-peer/{}", env!("CARGO_PKG_VERSION")))
                .map_err(|error| PeerError::Other(format!("bad user-agent: {error}")))?,
        );

        let (socket, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|error| PeerError::Upgrade(error.to_string()))?;
        let (mut sink, mut stream) = socket.split();
        let (tx, mut rx) = mpsc::channel::<Message>(32);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(256);
        let (welcome_tx, welcome_rx) = oneshot::channel::<Result<Welcome, PeerError>>();

        let writer = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let reader_pending = Arc::clone(&pending);
        let reader_events = events.clone();
        let reader = tokio::spawn(async move {
            let mut welcome_tx = Some(welcome_tx);
            while let Some(message) = stream.next().await {
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        finish(
                            &reader_pending,
                            &reader_events,
                            error.to_string(),
                            &mut welcome_tx,
                        );
                        break;
                    }
                };
                match message {
                    Message::Text(text) => {
                        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        match frame.get("type").and_then(Value::as_str) {
                            Some("welcome") => {
                                if let Some(sender) = welcome_tx.take() {
                                    let answer = serde_json::from_value::<Welcome>(frame.clone())
                                        .map_err(|error| PeerError::Json(error.to_string()));
                                    let _ = sender.send(answer);
                                }
                            }
                            Some("ok") => {
                                resolve(
                                    &reader_pending,
                                    &frame,
                                    Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
                                );
                            }
                            Some("error") => {
                                let code = frame
                                    .get("code")
                                    .and_then(Value::as_str)
                                    .unwrap_or("REMOTE_ERROR")
                                    .to_string();
                                let message = frame
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("the peer refused the call")
                                    .to_string();
                                let error = PeerError::Protocol { code, message };
                                if frame.get("id").and_then(Value::as_str).is_none() {
                                    if let Some(sender) = welcome_tx.take() {
                                        let _ = sender.send(Err(PeerError::Protocol {
                                            code: "UNAUTHENTICATED".to_string(),
                                            message: "the peer refused the hello".to_string(),
                                        }));
                                    }
                                    let _ = reader_events.send(PeerEvent::Closed);
                                } else {
                                    resolve(&reader_pending, &frame, Err(error));
                                }
                            }
                            Some("event") => {
                                let name = frame
                                    .get("event")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown")
                                    .to_string();
                                let data = frame.get("data").cloned().unwrap_or(Value::Null);
                                let _ = reader_events.send(PeerEvent::Event { name, data });
                            }
                            _ => {}
                        }
                    }
                    // The state channel is binary and is intentionally ignored
                    // here; P1 reads summaries, not per-session state frames.
                    Message::Binary(_) => {}
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            finish(
                &reader_pending,
                &reader_events,
                "the peer socket closed".to_string(),
                &mut welcome_tx,
            );
        });

        let hello = json!({
            "v": 1,
            "type": "hello",
            "id": "h1",
            "token": credentials.peer_token,
            "protocolVersion": 1,
            "appVersion": credentials.app_version,
            "deviceModel": "agi-peer",
            "accountId": credentials.account_id,
            "engine": "dsh",
        });
        tx.send(Message::Text(hello.to_string()))
            .await
            .map_err(|_| PeerError::Closed)?;

        let welcome = match tokio::time::timeout(HANDSHAKE_TIMEOUT, welcome_rx).await {
            Ok(Ok(Ok(welcome))) => welcome,
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(_)) => return Err(PeerError::Closed),
            Err(_) => return Err(PeerError::Timeout),
        };

        let client = Self {
            tx,
            pending,
            events,
            welcome,
            next_id: AtomicU64::new(1),
        };
        // Keep the tasks alive until the client is dropped. Detached tasks exit
        // when the channels close; aborting them here would race the welcome.
        drop((writer, reader));
        Ok(client)
    }

    /// The host's identity, as `hello` answered.
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Subscribe to server-pushed events.
    pub fn events(&self) -> broadcast::Receiver<PeerEvent> {
        self.events.subscribe()
    }

    /// Call one Mizar method.
    pub async fn request(&self, kind: &str, payload: Value) -> Result<Value, PeerError> {
        let id = format!("r{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), sender);
        let frame = json!({ "v": 1, "type": kind, "id": id, "payload": payload });
        if self
            .tx
            .send(Message::Text(frame.to_string()))
            .await
            .is_err()
        {
            return Err(PeerError::Closed);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(PeerError::Closed),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                Err(PeerError::Timeout)
            }
        }
    }

    /// Start a DSH session on the peer.
    pub async fn create_session(&self, cwd: Option<&str>) -> Result<String, PeerError> {
        let payload = match cwd {
            Some(cwd) if !cwd.trim().is_empty() => json!({ "cwd": cwd }),
            _ => json!({}),
        };
        let answer = self.request("session.create", payload).await?;
        answer
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| PeerError::Other("session.create returned no sessionId".to_string()))
    }

    /// Send one prompt and return the engine's acceptance.
    /// The workspaces the target permits this peer to see.
    pub async fn workspaces(&self) -> Result<Value, PeerError> {
        self.request("workspace.list", json!({})).await
    }

    /// The permission presets the target engine exposes.
    pub async fn permissions(&self) -> Result<Value, PeerError> {
        self.request("permissions.catalog", json!({})).await
    }

    /// Set the permission preset on one remote session.
    pub async fn set_permission(&self, session: &str, preset: &str) -> Result<Value, PeerError> {
        self.request(
            "session.permission.set",
            json!({ "sessionId": session, "preset": preset }),
        )
        .await
    }

    pub async fn prompt(&self, session: &str, text: &str) -> Result<Value, PeerError> {
        self.request(
            "session.prompt",
            json!({
                "sessionId": session,
                "text": text,
                "requestId": format!("mizar-peer-{}", self.next_id.load(Ordering::Relaxed)),
            }),
        )
        .await
    }

    /// One page of the conversation summary.
    pub async fn messages(
        &self,
        session: &str,
        after_seq: Option<i64>,
    ) -> Result<MessagesPage, PeerError> {
        let mut payload = json!({ "sessionId": session });
        if let Some(after) = after_seq {
            payload["afterSeq"] = json!(after);
        }
        let answer = self.request("session.messages", payload).await?;
        Ok(MessagesPage {
            as_of_seq: answer.get("asOfSeq").and_then(Value::as_i64).unwrap_or(0),
            records: answer
                .get("records")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        })
    }

    /// Stop the running turn on the peer.
    pub async fn cancel(&self, session: &str) -> Result<Value, PeerError> {
        self.request("session.cancel", json!({ "sessionId": session }))
            .await
    }

    /// Archive the remote session (best effort cleanup).
    pub async fn archive(&self, session: &str) -> Result<Value, PeerError> {
        self.request("session.archive", json!({ "sessionId": session }))
            .await
    }

    /// The approvals and questions waiting on the peer right now.
    pub async fn pending_interactions(&self) -> Result<Value, PeerError> {
        self.request("interactions.pending", json!({})).await
    }

    /// Answer one approval on the peer.
    pub async fn answer_approval(
        &self,
        request_id: &str,
        outcome: &str,
    ) -> Result<Value, PeerError> {
        self.request(
            "approval.answer",
            json!({ "requestId": request_id, "outcome": outcome }),
        )
        .await
    }

    /// Run one turn and wait for its `turn/end`.
    ///
    /// `cancel` is a watch channel rather than a token type so the kernel can
    /// reuse the signal it already owns. The remote session is left running on
    /// timeout; that is the caller's decision, not this client's.
    pub async fn run_turn(
        &self,
        session: &str,
        text: &str,
        timeout: Duration,
        cancel: watch::Receiver<bool>,
    ) -> Result<TurnResult, PeerError> {
        self.run_turn_with(session, text, timeout, cancel, |_| {})
            .await
    }

    /// The same turn, reporting each new pending approval once.
    ///
    /// The callback is deliberately synchronous and small: it writes the
    /// approval into the manager and returns. Answering it is a separate call
    /// from the host model, so the turn keeps waiting instead of blocking this
    /// task on a human.
    pub async fn run_turn_with<F>(
        &self,
        session: &str,
        text: &str,
        timeout: Duration,
        mut cancel: watch::Receiver<bool>,
        on_approval: F,
    ) -> Result<TurnResult, PeerError>
    where
        F: Fn(&Value),
    {
        let baseline = self.messages(session, None).await?;
        let baseline_turn = max_turn(&baseline.records);
        self.prompt(session, text).await?;
        let mut as_of = baseline.as_of_seq;
        let mut latest: Vec<Value> = Vec::new();
        let mut seen_approvals: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut approval_since: std::collections::HashMap<String, tokio::time::Instant> =
            std::collections::HashMap::new();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if *cancel.borrow() {
                let _ = self.cancel(session).await;
                return Err(PeerError::Other("cancelled".to_string()));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(PeerError::Timeout);
            }
            tokio::select! {
                _ = tokio::time::sleep(TURN_POLL) => {}
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        let _ = self.cancel(session).await;
                        return Err(PeerError::Other("cancelled".to_string()));
                    }
                }
            }
            let page = self.messages(session, Some(as_of)).await?;
            as_of = page.as_of_seq;
            latest.extend(page.records);
            if let Ok(pending) = self.pending_interactions().await {
                let mut still_waiting: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                if let Some(approvals) = pending.get("approvals").and_then(Value::as_array) {
                    for approval in approvals {
                        let same_session = approval
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .is_some_and(|id| id == session);
                        let request = approval.get("requestId").and_then(Value::as_str);
                        if same_session {
                            if let Some(request) = request {
                                still_waiting.insert(request.to_string());
                                approval_since
                                    .entry(request.to_string())
                                    .or_insert_with(tokio::time::Instant::now);
                                if seen_approvals.insert(request.to_string()) {
                                    on_approval(approval);
                                }
                            }
                        }
                    }
                }
                // An approval the model already answered disappears from the
                // pending list; stop counting it.
                approval_since.retain(|request, _| still_waiting.contains(request));
                let expired: Vec<String> = approval_since
                    .iter()
                    .filter(|(_, since)| since.elapsed() >= APPROVAL_TIMEOUT)
                    .map(|(request, _)| request.clone())
                    .collect();
                for request in expired {
                    // Fail closed: an unanswered approval is rejected, not left
                    // to block the target until the turn timeout.
                    let _ = self.answer_approval(&request, "rejected").await;
                    approval_since.remove(&request);
                }
            }
            if let Some((turn, reason)) = settled_turn(&latest, baseline_turn) {
                return Ok(TurnResult {
                    turn,
                    reason,
                    final_text: final_text_for_turn(&latest, turn),
                });
            }
        }
    }
}

fn ws_url(base: &str, room: &str) -> Result<String, PeerError> {
    let base = base.trim_end_matches('/');
    let scheme = if base.starts_with("https://") {
        "wss://"
    } else if base.starts_with("http://") {
        "ws://"
    } else {
        return Err(PeerError::Other(format!(
            "unsupported relay origin: {base}"
        )));
    };
    let host = base
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    if room.trim().is_empty() {
        return Err(PeerError::Other("the relay room is empty".to_string()));
    }
    Ok(format!("{scheme}{host}/r/{room}/mizar/v1/ws"))
}

fn resolve(pending: &Pending, frame: &Value, answer: Result<Value, PeerError>) {
    let Some(id) = frame.get("id").and_then(Value::as_str) else {
        return;
    };
    if let Some(sender) = pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(id)
    {
        let _ = sender.send(answer);
    }
}

fn finish(
    pending: &Pending,
    events: &broadcast::Sender<PeerEvent>,
    reason: String,
    welcome_tx: &mut Option<oneshot::Sender<Result<Welcome, PeerError>>>,
) {
    if let Some(sender) = welcome_tx.take() {
        let _ = sender.send(Err(PeerError::Io(reason.clone())));
    }
    let mut map = pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for (_, sender) in map.drain() {
        let _ = sender.send(Err(PeerError::Io(reason.clone())));
    }
    let _ = events.send(PeerEvent::Closed);
}

fn event_of(record: &Value) -> &Value {
    record.get("event").unwrap_or(record)
}

fn max_turn(records: &[Value]) -> u32 {
    records
        .iter()
        .filter_map(|record| event_of(record).get("data"))
        .filter_map(|data| data.get("turn"))
        .filter_map(Value::as_u64)
        .max()
        .unwrap_or(0) as u32
}

fn settled_turn(records: &[Value], after: u32) -> Option<(u32, String)> {
    let mut settled = None;
    for record in records {
        let event = event_of(record);
        if event.get("type").and_then(Value::as_str) != Some("turn/end") {
            continue;
        }
        let Some(data) = event.get("data") else {
            continue;
        };
        let turn = data.get("turn").and_then(Value::as_u64).unwrap_or(0) as u32;
        if turn <= after {
            continue;
        }
        let reason = data
            .get("reason")
            .and_then(|reason| reason.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("success")
            .to_string();
        settled = Some((turn, reason));
    }
    settled
}

fn final_text_for_turn(records: &[Value], turn: u32) -> Option<String> {
    let mut found = None;
    for record in records {
        let event = event_of(record);
        if event.get("type").and_then(Value::as_str) != Some("assistant/message") {
            continue;
        }
        let Some(data) = event.get("data") else {
            continue;
        };
        let event_turn = data.get("turn").and_then(Value::as_u64).unwrap_or(0) as u32;
        if event_turn != turn {
            continue;
        }
        let content = data
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array);
        let Some(content) = content else { continue };
        let parts: Vec<&str> = content
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect();
        if !parts.is_empty() {
            found = Some(parts.join("\n"));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(event: Value) -> Value {
        json!({ "type": "event", "event": event })
    }

    #[test]
    fn the_websocket_url_keeps_the_account_port() {
        assert_eq!(
            ws_url("https://dshmizar-1.example:8443/", "ROOM").unwrap(),
            "wss://dshmizar-1.example:8443/r/ROOM/mizar/v1/ws"
        );
        assert_eq!(
            ws_url("http://127.0.0.1:8787", "r").unwrap(),
            "ws://127.0.0.1:8787/r/r/mizar/v1/ws"
        );
        assert!(ws_url("ftp://example", "r").is_err());
    }

    #[test]
    fn a_turn_is_settled_by_turn_end_not_by_assistant_text() {
        let records = vec![
            envelope(json!({"type":"turn/start","data":{"turn":1}})),
            envelope(
                json!({"type":"assistant/message","data":{"turn":1,"message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}}),
            ),
            envelope(json!({"type":"turn/end","data":{"turn":1,"reason":{"kind":"completed"}}})),
        ];
        assert_eq!(
            settled_turn(&records, 0),
            Some((1, "completed".to_string()))
        );
        assert_eq!(final_text_for_turn(&records, 1).as_deref(), Some("hello"));
        assert_eq!(settled_turn(&records, 1), None);
    }

    #[test]
    fn a_bare_event_is_still_read_when_a_host_forgets_the_envelope() {
        let records =
            vec![json!({"type":"turn/end","data":{"turn":2,"reason":{"kind":"success"}}})];
        assert_eq!(settled_turn(&records, 1), Some((2, "success".to_string())));
    }

    #[test]
    fn multiple_text_parts_join_with_newlines() {
        let records = vec![envelope(json!({
            "type":"assistant/message",
            "data":{"turn":1,"message":{"role":"assistant","content":[
                {"type":"text","text":"line one"},
                {"type":"text","text":"line two"}
            ]}}
        }))];
        assert_eq!(
            final_text_for_turn(&records, 1).as_deref(),
            Some("line one\nline two")
        );
    }

    #[test]
    fn credentials_debug_does_not_print_tokens() {
        let credentials = Credentials {
            base: "https://example".into(),
            room: "ROOM".into(),
            account_session: "account-secret".into(),
            device_id: "device".into(),
            account_id: "account".into(),
            peer_token: "peer-secret".into(),
            app_version: "test".into(),
        };
        let text = format!("{credentials:?}");
        assert!(!text.contains("account-secret"));
        assert!(!text.contains("peer-secret"));
    }
}
