//! This computer's own DSH, as the local half of Agent interconnect.
//!
//! A delegation is the same object whichever side of the wire it runs on: the
//! manager in [`crate::remote`] owns the record, the cancel signal and the audit
//! line, and only the six calls below differ. This module is that difference for
//! the local route — it drives the DSH engine this process already hosts instead
//! of dialling a peer.
//!
//! The engine is resolved through a [`Weak`] handle on every call rather than
//! captured: an agent installed after launch lands in `Core::late`, and a strong
//! handle here would also be a cycle (the kernel owns the delegation manager,
//! which owns this adapter, which would own the kernel).

use std::sync::{Arc, Weak};
use std::time::Duration;

use agi_engine::{Engine, EngineEvent};
use agi_protocol::domain::{RecordKind, SessionKey};
use agi_protocol::engine::EngineId;
use tokio::sync::{broadcast, watch};

use crate::remote::{LocalAgent, LocalTurn};
use crate::Core;

/// The DSH this computer hosts, adapted to one delegation.
pub struct LocalDsh {
    core: Weak<Core>,
}

impl LocalDsh {
    pub fn new(core: Weak<Core>) -> Arc<Self> {
        Arc::new(Self { core })
    }

    fn engine(&self) -> Result<Arc<dyn Engine>, String> {
        self.core
            .upgrade()
            .and_then(|core| core.engine(EngineId::Dsh))
            .ok_or_else(|| "this build has no DSH engine attached".to_string())
    }
}

#[async_trait::async_trait]
impl LocalAgent for LocalDsh {
    fn status(&self) -> (bool, Option<String>) {
        match self.engine() {
            Ok(engine) => engine.status(),
            Err(message) => (false, Some(message)),
        }
    }

    async fn create_session(&self, cwd: Option<&str>) -> Result<String, String> {
        self.engine()?
            .create(cwd)
            .await
            .map(|key| key.to_string())
            .map_err(|error| error.to_string())
    }

    async fn set_model(
        &self,
        session: &str,
        provider: &str,
        model: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<(), String> {
        let key = parse_key(session)?;
        // DSH selects by provider *and* model, the same join the phone server
        // makes for `session.model.set`.
        self.engine()?
            .set_model_with_effort(&key, &format!("{provider}/{model}"), reasoning_effort)
            .await
            .map_err(|error| error.to_string())
    }

    async fn set_permission(&self, session: &str, preset: &str) -> Result<(), String> {
        let key = parse_key(session)?;
        self.engine()?
            .set_permission(&key, preset)
            .await
            .map_err(|error| error.to_string())
    }

    async fn run_turn(
        &self,
        session: &str,
        prompt: &str,
        timeout: Duration,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<LocalTurn, String> {
        let engine = self.engine()?;
        let key = parse_key(session)?;
        // Subscribe before prompting: a turn that ends in a millisecond must not
        // race the subscription, and nothing before the prompt can be its end.
        let mut events = engine.subscribe();
        // The newest sequence number before the prompt: any `turn/end` above it
        // belongs to this turn. Used by the slow poll below, which is the net
        // under a lagging event stream — DSH can outrun this subscriber on a
        // chatty turn, and a lost end event would otherwise burn the timeout.
        let baseline_seq = engine
            .records(&key, 5, None)
            .await
            .map(|records| records.iter().map(|record| record.seq).max().unwrap_or(0))
            .unwrap_or(0);
        engine
            .prompt(&key, prompt)
            .await
            .map_err(|error| error.to_string())?;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut poll = tokio::time::interval(Duration::from_secs(5));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        poll.tick().await;
        let mut saw_running = false;
        // The answer is accumulated from the same events the phone reads rather
        // than re-read afterwards. A follow-up turn runs on a session that was
        // archived when the first turn settled, and DSH drops an archived
        // session's cached records — a settle-time read would return "done"
        // with no text at all.
        let mut answer: Vec<String> = Vec::new();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(format!(
                        "the local turn did not settle within {} minutes",
                        timeout.as_secs() / 60
                    ));
                }
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        let _ = engine.cancel(&key).await;
                        return Err("cancelled".to_string());
                    }
                }
                _ = poll.tick() => {
                    // Read the log only when the stream has not already
                    // settled the turn; this is the fallback, not the signal.
                    if let Ok(records) = engine.records(&key, 50, None).await {
                        let ended = records.iter().any(|record| {
                            record.kind == RecordKind::TurnEnd && record.seq > baseline_seq
                        });
                        if ended {
                            return settled(&engine, &key, None, &answer).await;
                        }
                    }
                }
                event = events.recv() => match event {
                    // The structured end: DSH's plugin forwards `turn/end` for
                    // every turn, which is the same signal the phone reads.
                    Ok(EngineEvent::RecordAppended { session, record }) if session == key => {
                        match record.kind {
                            RecordKind::AssistantMessage => {
                                if let Some(text) = record.text.as_deref() {
                                    if !text.trim().is_empty() {
                                        answer.push(text.to_string());
                                    }
                                }
                            }
                            RecordKind::TurnEnd => {
                                return Ok(LocalTurn {
                                    reason: crate::reason_of(record.text.as_deref()).to_string(),
                                    final_text: joined(&answer),
                                });
                            }
                            _ => {}
                        }
                    }
                    // The fallback for a turn whose end has no record (a link
                    // that only reports state): wait until it stops running
                    // after having watched it run, so a session idling before
                    // the prompt cannot settle the delegation immediately.
                    Ok(EngineEvent::SessionUpdated(updated)) if updated.key == key => {
                        if updated.running {
                            saw_running = true;
                        } else if saw_running {
                            return settled(&engine, &key, None, &answer).await;
                        }
                    }
                    Ok(_) => {}
                    // A slow consumer is not a failed turn; the next event or
                    // the deadline decides. Its cost is an empty answer, which
                    // the settle-time read below covers when records survive.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err("the DSH link closed during the turn".to_string());
                    }
                },
            }
        }
    }

    async fn archive(&self, session: &str) -> Result<(), String> {
        let key = parse_key(session)?;
        self.engine()?
            .archive(&key, true)
            .await
            .map_err(|error| error.to_string())
    }
}

fn parse_key(session: &str) -> Result<SessionKey, String> {
    session
        .parse::<SessionKey>()
        .map_err(|_| format!("{session} is not a session key"))
}

/// The turn's answer from what the event stream already delivered, and — when
/// that came up empty because the consumer lagged — from the session's records.
///
/// `turn` is the one the end record named; `None` means the state channel
/// reported the stop without a record, so the newest finished turn is read.
async fn settled(
    engine: &Arc<dyn Engine>,
    key: &SessionKey,
    turn: Option<u32>,
    answer: &[String],
) -> Result<LocalTurn, String> {
    if let Some(text) = joined(answer) {
        return Ok(LocalTurn {
            reason: "completed".to_string(),
            final_text: Some(text),
        });
    }
    let records = engine
        .records(key, 200, None)
        .await
        .map_err(|error| error.to_string())?;
    let (_turn, slice) = match turn {
        Some(turn) => {
            let start = records
                .iter()
                .rposition(|record| {
                    record.kind == RecordKind::TurnStart && record.turn == Some(turn)
                })
                .unwrap_or(0);
            let slice: Vec<_> = records[start..]
                .iter()
                .filter(|record| record.turn == Some(turn))
                .cloned()
                .collect();
            (turn, slice)
        }
        None => crate::last_finished_turn(&records)
            .ok_or_else(|| "the local turn ended without a readable turn".to_string())?,
    };
    let reason = slice
        .iter()
        .rev()
        .find(|record| record.kind == RecordKind::TurnEnd)
        .and_then(|record| record.text.as_deref());
    let text = slice
        .iter()
        .filter(|record| record.kind == RecordKind::AssistantMessage)
        .filter_map(|record| record.text.as_deref())
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(LocalTurn {
        reason: crate::reason_of(reason).to_string(),
        final_text: (!text.is_empty()).then_some(text),
    })
}

/// The assistant messages of one turn, as one answer.
fn joined(answer: &[String]) -> Option<String> {
    if answer.is_empty() {
        return None;
    }
    Some(answer.join("\n\n"))
}
