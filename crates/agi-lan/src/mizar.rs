//! A tiny Mizar v1 client over a Noise LAN session.
//!
//! This is deliberately separate from `agi-peer`'s WebSocket client: the LAN
//! transport is a Noise stream, while the WAN transport is a WebSocket. Both
//! speak the same Mizar JSON frames, and this module implements only the subset
//! the delegation manager needs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::watch;

use crate::{LanCredentials, LanError, LanSession};

/// What a turn ended with.
#[derive(Debug, Clone)]
pub struct LanTurnResult {
    pub turn: u32,
    pub reason: String,
    pub final_text: Option<String>,
}

/// A page of conversation records.
#[derive(Debug, Clone)]
pub struct LanMessages {
    pub as_of_seq: i64,
    pub records: Vec<Value>,
}

/// A Mizar connection over LAN.
pub struct LanMizarClient {
    session: LanSession,
    next_id: AtomicU64,
    welcome: Value,
}

impl std::fmt::Debug for LanMizarClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LanMizarClient")
            .field("remote", self.session.remote())
            .finish_non_exhaustive()
    }
}

impl LanMizarClient {
    /// Handshake, receive the temporary peer token, then send the ordinary
    /// Mizar hello and read the welcome.
    pub async fn connect(
        address: std::net::SocketAddr,
        credentials: &LanCredentials,
    ) -> Result<Self, LanError> {
        let session = crate::connect(address, credentials).await?;
        let token_frame = recv_json(&session).await?;
        if token_frame.get("type").and_then(Value::as_str) != Some("lan.session") {
            return Err(LanError::Remote("expected lan.session".to_string()));
        }
        let token = token_frame
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| LanError::Remote("lan.session carried no token".to_string()))?;
        session
            .send(
                json!({
                    "v": 1, "type": "hello", "id": "hello", "token": token,
                    "engine": "dsh", "appVersion": "agi-lan"
                })
                .to_string()
                .as_bytes(),
            )
            .await?;
        let welcome = recv_json(&session).await?;
        if welcome.get("type").and_then(Value::as_str) != Some("welcome") {
            return Err(LanError::Remote(format!("expected welcome, got {welcome}")));
        }
        Ok(Self {
            session,
            next_id: AtomicU64::new(1),
            welcome,
        })
    }

    pub fn welcome(&self) -> &Value {
        &self.welcome
    }

    pub fn remote(&self) -> &crate::RemoteDevice {
        self.session.remote()
    }

    /// Send one request and wait for its matching id.
    pub async fn request(&self, kind: &str, payload: Value) -> Result<Value, LanError> {
        let id = format!("l{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        self.session
            .send(
                json!({ "v": 1, "type": kind, "id": id, "payload": payload })
                    .to_string()
                    .as_bytes(),
            )
            .await?;
        loop {
            let frame = recv_json(&self.session).await?;
            if frame.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                continue;
            }
            return match frame.get("type").and_then(Value::as_str) {
                Some("ok") => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
                Some("error") => Err(LanError::Remote(format!(
                    "{}: {}",
                    frame.get("code").and_then(Value::as_str).unwrap_or("ERROR"),
                    frame.get("message").and_then(Value::as_str).unwrap_or("")
                ))),
                other => Err(LanError::Remote(format!("unexpected frame {other:?}"))),
            };
        }
    }

    pub async fn create_session(&self, cwd: Option<&str>) -> Result<String, LanError> {
        let payload = match cwd {
            Some(cwd) if !cwd.trim().is_empty() => json!({ "cwd": cwd }),
            _ => json!({}),
        };
        let answer = self.request("session.create", payload).await?;
        answer
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| LanError::Remote("session.create returned no sessionId".to_string()))
    }

    /// The workspaces the target permits this peer to see.
    pub async fn workspaces(&self) -> Result<Value, LanError> {
        self.request("workspace.list", json!({})).await
    }

    /// The permission presets the target engine exposes.
    pub async fn permissions(&self) -> Result<Value, LanError> {
        self.request("permissions.catalog", json!({})).await
    }

    /// Set the permission preset on one remote session.
    pub async fn set_permission(&self, session: &str, preset: &str) -> Result<Value, LanError> {
        self.request(
            "session.permission.set",
            json!({ "sessionId": session, "preset": preset }),
        )
        .await
    }

    pub async fn prompt(&self, session: &str, text: &str) -> Result<Value, LanError> {
        self.request(
            "session.prompt",
            json!({ "sessionId": session, "text": text }),
        )
        .await
    }

    pub async fn messages(
        &self,
        session: &str,
        after: Option<i64>,
    ) -> Result<LanMessages, LanError> {
        let mut payload = json!({ "sessionId": session });
        if let Some(after) = after {
            payload["afterSeq"] = json!(after);
        }
        let answer = self.request("session.messages", payload).await?;
        Ok(LanMessages {
            as_of_seq: answer.get("asOfSeq").and_then(Value::as_i64).unwrap_or(0),
            records: answer
                .get("records")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        })
    }

    pub async fn pending(&self) -> Result<Value, LanError> {
        self.request("interactions.pending", json!({})).await
    }

    pub async fn answer_approval(
        &self,
        request_id: &str,
        outcome: &str,
    ) -> Result<Value, LanError> {
        self.request(
            "approval.answer",
            json!({ "requestId": request_id, "outcome": outcome }),
        )
        .await
    }

    pub async fn cancel(&self, session: &str) -> Result<Value, LanError> {
        self.request("session.cancel", json!({ "sessionId": session }))
            .await
    }

    /// Run one turn to `turn/end`, optionally observing new approvals.
    pub async fn run_turn<F>(
        &self,
        session: &str,
        text: &str,
        timeout: Duration,
        mut cancel: watch::Receiver<bool>,
        on_approval: F,
    ) -> Result<LanTurnResult, LanError>
    where
        F: Fn(&Value),
    {
        let baseline = self.messages(session, None).await?;
        let baseline_turn = max_turn(&baseline.records);
        self.prompt(session, text).await?;
        let mut as_of = baseline.as_of_seq;
        let mut latest = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if *cancel.borrow() {
                let _ = self.cancel(session).await;
                return Err(LanError::Remote("cancelled".to_string()));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(LanError::Noise("turn timed out".to_string()));
            }
            tokio::select! {
                _ = tokio::time::sleep(crate::mizar::near::POLL) => {}
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        let _ = self.cancel(session).await;
                        return Err(LanError::Remote("cancelled".to_string()));
                    }
                }
            }
            let page = self.messages(session, Some(as_of)).await?;
            as_of = page.as_of_seq;
            latest.extend(page.records);
            if let Ok(pending) = self.pending().await {
                if let Some(approvals) = pending.get("approvals").and_then(Value::as_array) {
                    for approval in approvals {
                        let same = approval
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .is_some_and(|id| id == session);
                        if same {
                            if let Some(id) = approval.get("requestId").and_then(Value::as_str) {
                                if seen.insert(id.to_string()) {
                                    on_approval(approval);
                                }
                            }
                        }
                    }
                }
            }
            if let Some((turn, reason)) = settled(&latest, baseline_turn) {
                return Ok(LanTurnResult {
                    turn,
                    reason,
                    final_text: final_text(&latest, turn),
                });
            }
        }
    }
}

async fn recv_json(session: &LanSession) -> Result<Value, LanError> {
    let bytes = session.recv().await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| LanError::Remote("the peer sent non-JSON".to_string()))
}

fn max_turn(records: &[Value]) -> u32 {
    records
        .iter()
        .filter_map(|record| record.get("event"))
        .filter_map(|event| event.get("data"))
        .filter_map(|data| data.get("turn"))
        .filter_map(Value::as_u64)
        .max()
        .unwrap_or(0) as u32
}

fn settled(records: &[Value], baseline: u32) -> Option<(u32, String)> {
    records.iter().rev().find_map(|record| {
        let event = record.get("event")?;
        if event.get("type").and_then(Value::as_str) != Some("turn/end") {
            return None;
        }
        let data = event.get("data")?;
        let turn = data.get("turn").and_then(Value::as_u64)? as u32;
        if turn <= baseline {
            return None;
        }
        let reason = data
            .get("reason")
            .and_then(|reason| reason.get("kind").or(Some(reason)))
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_string();
        Some((turn, reason))
    })
}

fn final_text(records: &[Value], turn: u32) -> Option<String> {
    let mut lines = Vec::new();
    for record in records {
        let Some(event) = record.get("event") else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) != Some("assistant/message") {
            continue;
        }
        let Some(data) = event.get("data") else {
            continue;
        };
        if data.get("turn").and_then(Value::as_u64) != Some(turn as u64) {
            continue;
        }
        let Some(parts) = data
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let text: String = parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            lines.push(text);
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n\n"))
}

// Small module so the poll interval is named once and testable.
pub(crate) mod near {
    pub const POLL: std::time::Duration = std::time::Duration::from_millis(500);
}
