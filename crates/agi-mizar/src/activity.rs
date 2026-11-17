//! What the island says about this computer, and when it has to say it.
//!
//! A phone in a pocket has no socket and no code running, so the only thing that
//! can make its Live Activity appear, change, and come down is a push from the
//! relay — and the relay only knows what this computer tells it. This module is
//! that telling: it watches the kernel's events, keeps the small amount of state
//! a card is drawn from, and answers with a [`Card`] whenever the answer changed.
//!
//! ### One card, several rows
//!
//! The host reports one complete snapshot, bounded by [`MAX_ROWS`], not one
//! activity per session. The relay merges snapshots from multiple computers and
//! applies the smaller limit used by the Lock Screen. So "a session finished
//! while others were running" is an update carrying the remaining rows, and the
//! relay ends the shared activity only after every host reports an empty snapshot.
//!
//! ### Why a turn's end is a state frame and not `running == false`
//!
//! The engines report `running` as a summary they refresh whenever they like; the
//! state machine reports a *verdict* once, when the turn is over. The card is
//! meant to end when the work is over, so the verdict is the signal — with
//! `running == false` kept only as the fallback for an engine that never says.
//!
//! ### Approvals live on the card
//!
//! A blocked turn is the one moment the operator must act, and making them open
//! the app to see *what* is blocked defeats the island. The card therefore
//! carries pending approvals beside the rows; the phone draws them and routes
//! responses through the existing interaction flow. Everything here is
//! `serde_json`, so a phone that predates the fields ignores them rather than
//! failing to decode.

use std::collections::BTreeMap;

use agi_protocol::domain::{Interaction, InteractionKind, Session, SessionKey};
use serde_json::{json, Value};

/// How many rows one card carries.
///
/// Complete per-host snapshot limit. The relay applies the smaller UI limit
/// after merging snapshots from every host.
pub const MAX_ROWS: usize = 32;
/// Keep pending requests bounded independently from the session rows.
pub const MAX_APPROVALS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Running,
    WaitingApproval,
    WaitingAnswer,
}

impl RowStatus {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingApproval => "waitingApproval",
            Self::WaitingAnswer => "waitingAnswer",
        }
    }
}

/// One running session, as a card row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The session, spelled the way every id the phone sees is spelled.
    pub session_id: String,
    /// What the host calls it, or empty while it has no title yet.
    pub title: String,
    /// Epoch milliseconds the turn was first seen running here.
    ///
    /// The app ticks its clock from this, so it has to be a real instant and not
    /// a duration. A desktop that restarted mid-turn starts the row at the
    /// restart: the true turn start is on the other side of a process it no
    /// longer has.
    pub running_since: i64,
    /// Epoch milliseconds when this row began waiting, if it is waiting.
    pub waiting_since: Option<i64>,
    pub status: RowStatus,
}

/// One approval waiting for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub request_id: String,
    pub session_id: String,
    /// The tool asking, e.g. `Bash`.
    pub tool: String,
    /// The sentence the agent wrote about it.
    pub detail: String,
    pub requested_at: i64,
}

/// Everything the island draws.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Card {
    /// Oldest first, already cut to [`MAX_ROWS`].
    pub rows: Vec<Row>,
    /// Oldest first.
    pub approvals: Vec<Approval>,
    /// Pending request identity and kind, including questions that have no
    /// separate payload on the Live Activity wire.
    pub pending: Vec<PendingInteraction>,
}

impl Card {
    /// Whether there is nothing to show, which means the activity should end.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.approvals.is_empty() && self.pending.is_empty()
    }
}

/// Build the complete per-host snapshot from all engines' sessions. Child
/// conversations use the domain's established `subagent` and `fork` origin
/// markers and are omitted. A main session with an unresolved interaction is
/// included even after its engine reports `running = false`.
pub fn snapshot_card(sessions: &[Session], interactions: &[Interaction]) -> Card {
    let main_sessions: std::collections::HashSet<String> = sessions
        .iter()
        .filter(|session| !matches!(session.origin.as_deref(), Some("subagent")))
        .map(|session| session.key.to_string())
        .collect();
    let mut pending: Vec<PendingInteraction> = interactions
        .iter()
        .filter_map(|interaction| {
            let session_id = interaction.session.as_ref()?.to_string();
            if !main_sessions.contains(&session_id) {
                return None;
            }
            let (status, approval) = match interaction.kind {
                InteractionKind::Approval => (
                    RowStatus::WaitingApproval,
                    Some(Approval {
                        request_id: interaction.id.clone(),
                        session_id: session_id.clone(),
                        tool: interaction.title.clone(),
                        detail: interaction.detail.clone().unwrap_or_default(),
                        requested_at: interaction.requested_at,
                    }),
                ),
                InteractionKind::Question => (RowStatus::WaitingAnswer, None),
            };
            Some(PendingInteraction {
                request_id: interaction.id.clone(),
                session_id,
                status,
                requested_at: interaction.requested_at,
                approval,
            })
        })
        .collect();
    pending.sort_by(|a, b| {
        a.requested_at
            .cmp(&b.requested_at)
            .then(a.request_id.cmp(&b.request_id))
    });
    pending.truncate(MAX_APPROVALS);
    let waiting: BTreeMap<&str, RowStatus> =
        pending
            .iter()
            .fold(BTreeMap::new(), |mut statuses, interaction| {
                statuses
                    .entry(interaction.session_id.as_str())
                    .and_modify(|status| {
                        if interaction.status == RowStatus::WaitingApproval {
                            *status = interaction.status;
                        }
                    })
                    .or_insert(interaction.status);
                statuses
            });
    let mut rows: Vec<Row> = sessions
        .iter()
        .filter(|session| {
            !matches!(session.origin.as_deref(), Some("subagent"))
                && (session.running || waiting.contains_key(session.key.to_string().as_str()))
        })
        .map(|session| Row {
            session_id: session.key.to_string(),
            title: session
                .title
                .clone()
                .unwrap_or_else(|| "Session".to_string()),
            running_since: session.updated_at,
            waiting_since: waiting
                .get(session.key.to_string().as_str())
                .and_then(|status| {
                    pending
                        .iter()
                        .filter(|interaction| {
                            interaction.session_id == session.key.to_string()
                                && interaction.status == *status
                        })
                        .map(|interaction| interaction.requested_at)
                        .min()
                }),
            status: waiting
                .get(session.key.to_string().as_str())
                .copied()
                .unwrap_or(RowStatus::Running),
        })
        .collect();
    rows.sort_by_key(|row| row.running_since);
    rows.truncate(MAX_ROWS);
    let approvals: Vec<Approval> = pending
        .iter()
        .filter_map(|interaction| interaction.approval.clone())
        .take(MAX_APPROVALS)
        .collect();
    Card {
        rows,
        approvals,
        pending,
    }
}

/// The kernel's events, folded into a card.
///
/// Deliberately not a service: it owns no clock, no socket and no lock, so the
/// rules ("a second session does not restart the first row's clock", "an approval
/// that has been answered leaves the card") are unit tests rather than something
/// discovered on a phone.
#[derive(Debug, Default)]
pub struct Tracker {
    /// Every running session, oldest first. Beyond [`MAX_ROWS`] they are kept —
    /// a row that was third becomes second when one finishes — but only the head
    /// is published.
    rows: Vec<Row>,
    pending: BTreeMap<String, PendingInteraction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInteraction {
    pub request_id: String,
    pub session_id: String,
    pub status: RowStatus,
    pub requested_at: i64,
    pub approval: Option<Approval>,
}

impl Tracker {
    /// One session is running.
    ///
    /// - Returns: the card when the answer changed, `None` for a session that
    ///   was already on it (a running session is updated often, and every one of
    ///   those updates is a push).
    pub fn running(&mut self, session: &Session, now_ms: i64) -> Option<Card> {
        let session_id = session.key.to_string();
        let title = session
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "Session".to_string());
        let is_waiting = self.status_for_session(&session_id).is_some();
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.session_id == session_id)
        {
            // A turn often starts before DSH has named the session. The row was
            // created with an empty title and then never touched again, which
            // is why the island showed only a timer for that row. Publish the
            // title when it arrives; an unchanged title still produces no card.
            let mut changed = false;
            if row.title != title {
                row.title = title;
                changed = true;
            }
            if row.status != RowStatus::Running && !is_waiting {
                row.status = RowStatus::Running;
                changed = true;
            }
            return changed.then(|| self.card());
        }
        self.rows.push(Row {
            session_id,
            title,
            running_since: now_ms,
            waiting_since: None,
            status: RowStatus::Running,
        });
        // Oldest first, and a tie broken by insertion so two turns that started
        // in the same millisecond keep a stable order.
        self.rows.sort_by_key(|row| row.running_since);
        Some(self.card())
    }

    /// A turn ended, or the session went away.
    ///
    /// - Returns: the card when this session was on it.
    pub fn finished(&mut self, key: &SessionKey) -> Option<Card> {
        let session_id = key.to_string();
        let before = self.rows.len();
        self.rows.retain(|row| row.session_id != session_id);
        let pending = self.pending.len();
        self.pending
            .retain(|_, interaction| interaction.session_id != session_id);
        if self.rows.len() == before && self.pending.len() == pending {
            return None;
        }
        Some(self.card())
    }

    /// The agent asked for permission.
    pub fn approval_requested(&mut self, interaction: &Interaction) -> Option<Card> {
        if interaction.kind != InteractionKind::Approval {
            return None;
        }
        self.interaction_requested(interaction)
    }

    /// An approval or question is waiting for the operator.
    pub fn interaction_requested(&mut self, interaction: &Interaction) -> Option<Card> {
        let session_id = interaction.session.as_ref()?.to_string();
        let status = match interaction.kind {
            InteractionKind::Approval => RowStatus::WaitingApproval,
            InteractionKind::Question => RowStatus::WaitingAnswer,
        };
        let approval = (interaction.kind == InteractionKind::Approval).then(|| Approval {
            request_id: interaction.id.clone(),
            session_id: session_id.clone(),
            tool: interaction.title.clone(),
            detail: interaction.detail.clone().unwrap_or_default(),
            requested_at: interaction.requested_at,
        });
        let pending = PendingInteraction {
            request_id: interaction.id.clone(),
            session_id,
            status,
            requested_at: interaction.requested_at,
            approval,
        };
        let changed = self.pending.get(&interaction.id) != Some(&pending);
        if !changed {
            return None;
        }
        self.pending.insert(interaction.id.clone(), pending);
        self.refresh_row_statuses();
        Some(self.card())
    }

    /// Somebody answered it -- this phone, the desktop window, or a timeout.
    pub fn approval_resolved(&mut self, id: &str) -> Option<Card> {
        self.pending.remove(id).map(|_| {
            self.refresh_row_statuses();
            self.card()
        })
    }

    /// Replace this tracker's view of the world with an authoritative card.
    ///
    /// The event loop starts empty, and a session or an approval may already be
    /// waiting when it does: the first reconciliation reads the snapshot and
    /// publishes it, but publishing does not teach the tracker anything. Later,
    /// an `InteractionResolved` for an approval the tracker never saw then has
    /// no card to send, and the phone keeps showing an approval the computer has
    /// already answered. Reconciliation is what closes that gap.
    ///
    /// The card's rows are already the published head of the list, which is the
    /// only part the tracker needs: a third row that is not on screen cannot
    /// become visible without a new card from a session event anyway.
    pub fn reconcile(&mut self, card: &Card) {
        // The snapshot's `session.updated_at` moves on every update, while the
        // clock the card draws must stay on the instant this host first saw the
        // turn running. Keep a row's existing `running_since`; only a row this
        // tracker has never seen takes the snapshot's instant.
        let mut rows = card.rows.clone();
        for row in &mut rows {
            if let Some(existing) = self
                .rows
                .iter()
                .find(|known| known.session_id == row.session_id)
            {
                row.running_since = existing.running_since;
            }
        }
        self.rows = rows;
        // Reconciliation updates row state without changing a known turn's
        // start instant. Pending interactions are rebuilt by the server's
        // authoritative snapshot below when requested/resolved events arrive.
        self.pending = card
            .pending
            .iter()
            .cloned()
            .map(|pending| (pending.request_id.clone(), pending))
            .collect();
        for row in &mut self.rows {
            row.status = card
                .rows
                .iter()
                .find(|source| source.session_id == row.session_id)
                .map(|source| source.status)
                .unwrap_or(RowStatus::Running);
        }
    }

    /// Reconcile a snapshot whose turn clocks have already been stabilized by
    /// the server, including a new turn observed without an intermediate idle.
    pub fn reconcile_authoritative(&mut self, card: &Card) {
        self.reconcile(card);
        for row in &mut self.rows {
            if let Some(source) = card
                .rows
                .iter()
                .find(|source| source.session_id == row.session_id)
            {
                row.running_since = source.running_since;
            }
        }
    }

    /// What the row for this session was called, before it leaves the card.
    ///
    /// Read first, because `finished` removes it: a turn-end notification that
    /// cannot name its session is a push the operator cannot act on.
    pub fn title_of(&self, key: &SessionKey) -> Option<String> {
        let session_id = key.to_string();
        self.rows
            .iter()
            .find(|row| row.session_id == session_id)
            .map(|row| row.title.clone())
    }

    /// The card as the wire wants it.
    pub fn card(&self) -> Card {
        let mut rows: Vec<Row> = self.rows.iter().take(MAX_ROWS).cloned().collect();
        for row in &mut rows {
            if let Some(status) = self.status_for_session(&row.session_id) {
                row.status = status;
                row.waiting_since = self
                    .pending
                    .values()
                    .filter(|pending| {
                        pending.session_id == row.session_id && pending.status == status
                    })
                    .map(|pending| pending.requested_at)
                    .min();
            } else {
                row.waiting_since = None;
            }
        }
        let mut approvals: Vec<Approval> = self
            .pending
            .values()
            .filter_map(|pending| pending.approval.clone())
            .collect();
        approvals.sort_by(|a, b| {
            a.requested_at
                .cmp(&b.requested_at)
                .then(a.request_id.cmp(&b.request_id))
        });
        approvals.truncate(MAX_APPROVALS);
        Card {
            rows,
            approvals,
            pending: self.pending.values().cloned().collect(),
        }
    }

    fn status_for_session(&self, session_id: &str) -> Option<RowStatus> {
        self.pending
            .values()
            .filter(|pending| pending.session_id == session_id)
            .map(|pending| pending.status)
            .min_by_key(|status| match status {
                RowStatus::WaitingApproval => 0,
                RowStatus::WaitingAnswer => 1,
                RowStatus::Running => 2,
            })
    }

    fn refresh_row_statuses(&mut self) {
        let waiting: BTreeMap<String, RowStatus> =
            self.pending
                .values()
                .fold(BTreeMap::new(), |mut map, pending| {
                    map.entry(pending.session_id.clone())
                        .and_modify(|status| {
                            if pending.status == RowStatus::WaitingApproval {
                                *status = pending.status;
                            }
                        })
                        .or_insert(pending.status);
                    map
                });
        for row in &mut self.rows {
            row.status = waiting
                .get(&row.session_id)
                .copied()
                .unwrap_or(RowStatus::Running);
        }
    }

    /// Every session this tracker believes is running, for diagnostics.
    pub fn running_rows(&self) -> usize {
        self.rows.len()
    }
}

/// The body of one `POST /push/v1/activity`.
///
/// `phase` is `start` for "make sure a card says this": the relay turns it into
/// an update on a phone that already has one, so this computer never has to know
/// which phones are showing a card. `approvals` is additive — an older relay
/// ignores it, an older app ignores it, and a newer one draws it.
///
/// - Parameters:
///   - card: what should be on screen.
///   - except: devices whose app is on screen and has its own card.
/// - Returns: the JSON body.
pub fn activity_body(
    card: &Card,
    except: &[String],
    host_id: &str,
    host_name: &str,
    sent_at: i64,
) -> Value {
    json!({
        "phase": "start",
        "hostId": host_id,
        "hostName": host_name,
        "snapshotVersion": 2,
        "sentAt": sent_at,
        "rows": card
            .rows
            .iter()
            .map(|row| {
                let mut value = json!({
                    "sessionId": row.session_id,
                    "title": row.title,
                    "runningSince": row.running_since,
                    "status": row.status.wire_name(),
                });
                if let Some(waiting_since) = row.waiting_since {
                    value["waitingSince"] = json!(waiting_since);
                }
                value
            })
            .collect::<Vec<_>>(),
        "approvals": card
            .approvals
            .iter()
            .map(|approval| json!({
                "requestId": approval.request_id,
                "sessionId": approval.session_id,
                "tool": approval.tool,
                "detail": approval.detail,
                "requestedAt": approval.requested_at,
            }))
            .collect::<Vec<_>>(),
        "exceptDeviceIds": except,
    })
}

/// The body of the notification a finished turn sends.
///
/// One call does two things, deliberately: the alert is the "a turn ended" the
/// operator asked for, and `endLiveActivity` is what takes the card down when
/// nothing is left on it. Two requests would be two chances to deliver half of a
/// pair of facts that are only true together.
///
/// - Parameters:
///   - title: what the session is called, or empty.
///   - session: the session's id, so a tap opens the right one.
///   - end_live_activity: whether the card has no rows left.
///   - except: devices that are looking at the app.
/// - Returns: the JSON body.
pub fn turn_end_body(
    title: &str,
    session: &SessionKey,
    end_live_activity: bool,
    except: &[String],
    zh: bool,
    host_id: &str,
    host_name: &str,
) -> Value {
    let name = if title.trim().is_empty() {
        if zh { "会话" } else { "Session" }.to_string()
    } else {
        title.trim().to_string()
    };
    let (headline, body) = if zh {
        ("会话已完成".to_string(), format!("{name} 已完成"))
    } else {
        ("Turn finished".to_string(), format!("{name} is done"))
    };
    json!({
        "kind": "turn_end",
        "title": headline,
        "body": body,
        "sessionId": session.to_string(),
        "threadId": session.to_string(),
        "timeSensitive": false,
        "endLiveActivity": end_live_activity,
        "hostId": host_id,
        "hostName": host_name,
        "exceptDeviceIds": except,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agi_protocol::domain::{Interaction, InteractionKind};
    use agi_protocol::engine::EngineId;

    fn session(id: &str, title: Option<&str>) -> Session {
        let mut session = Session::bare(SessionKey::new(EngineId::Dsh, id), 1);
        session.running = true;
        session.title = title.map(str::to_string);
        session
    }

    fn key(id: &str) -> SessionKey {
        SessionKey::new(EngineId::Dsh, id)
    }

    fn approval(id: &str, session: &str) -> Interaction {
        Interaction {
            id: id.to_string(),
            kind: InteractionKind::Approval,
            session: Some(key(session)),
            title: "Bash".to_string(),
            detail: Some("rm -rf build".to_string()),
            requested_at: 1,
            questions: Vec::new(),
        }
    }

    #[test]
    fn a_title_that_arrives_after_the_row_updates_the_card() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", None), 1_000);
        // DSH names a session a moment after its turn starts. The row was
        // created before that and used to keep an empty title for the whole
        // turn -- the island showed a ticking clock with nothing in front of it.
        let card = tracker
            .running(&session("a", Some("Name the island row")), 2_000)
            .expect("the title change is a card update");
        assert_eq!(card.rows[0].title, "Name the island row");
        assert_eq!(card.rows[0].running_since, 1_000);
        // A repeat of the same title is not a push.
        assert!(tracker
            .running(&session("a", Some("Name the island row")), 3_000)
            .is_none());
    }

    #[test]
    fn a_running_session_becomes_the_first_row() {
        let mut tracker = Tracker::default();
        let card = tracker
            .running(&session("a", Some("Fix the tests")), 1_000)
            .unwrap();
        assert_eq!(card.rows.len(), 1);
        assert_eq!(card.rows[0].session_id, "dsh:a");
        assert_eq!(card.rows[0].title, "Fix the tests");
        assert_eq!(card.rows[0].running_since, 1_000);
    }

    #[test]
    fn a_session_that_is_already_running_does_not_push_again() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", None), 1_000);
        // Every session list refresh reports the same running session; a push per
        // refresh is a metered relay carrying nothing new.
        assert!(tracker.running(&session("a", None), 2_000).is_none());
    }

    #[test]
    fn two_rows_and_a_finish_leaves_the_older_one_on_screen() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", Some("first")), 1_000);
        tracker.running(&session("b", Some("second")), 2_000);
        let card = tracker.finished(&key("b")).unwrap();
        assert_eq!(card.rows.len(), 1);
        assert_eq!(card.rows[0].session_id, "dsh:a");
        assert_eq!(
            card.rows[0].running_since, 1_000,
            "the first row's clock is untouched"
        );
    }

    #[test]
    fn finishing_the_last_row_empties_the_card() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", None), 1_000);
        let card = tracker.finished(&key("a")).unwrap();
        assert!(card.is_empty(), "an empty card is what ends the activity");
        assert!(
            tracker.finished(&key("a")).is_none(),
            "a second end is not news"
        );
    }

    #[test]
    fn a_third_session_stays_off_the_card_and_moves_up_when_one_finishes() {
        let mut tracker = Tracker::default();
        for (index, id) in ["a", "b", "c"].into_iter().enumerate() {
            tracker.running(&session(id, Some(id)), 1_000 + index as i64);
        }
        let card = tracker.card();
        assert_eq!(
            card.rows.len(),
            3,
            "the producer keeps the complete local snapshot"
        );
        assert_eq!(card.rows[1].session_id, "dsh:b");
        let card = tracker.finished(&key("a")).unwrap();
        assert_eq!(card.rows[0].session_id, "dsh:b");
        assert_eq!(
            card.rows[1].session_id, "dsh:c",
            "the third row remains in the snapshot"
        );
    }

    #[test]
    fn an_approval_joins_the_rows_and_leaves_when_it_is_answered() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", Some("Build")), 1_000);
        let card = tracker.approval_requested(&approval("r1", "a")).unwrap();
        assert_eq!(card.approvals.len(), 1);
        assert_eq!(card.approvals[0].request_id, "r1");
        assert_eq!(card.approvals[0].tool, "Bash");
        assert_eq!(
            card.rows.len(),
            1,
            "the row stays: the turn is blocked, not over"
        );

        let card = tracker.approval_resolved("r1").unwrap();
        assert!(card.approvals.is_empty());
        assert_eq!(card.rows.len(), 1);
        assert!(
            tracker.approval_resolved("r1").is_none(),
            "answered twice is not news"
        );
    }

    #[test]
    fn reconciliation_teaches_the_tracker_an_approval_it_started_too_late_for() {
        let mut tracker = Tracker::default();
        let card = Card {
            rows: vec![Row {
                session_id: "dsh:a".to_string(),
                title: "Build".to_string(),
                running_since: 1_000,
                waiting_since: Some(1),
                status: RowStatus::WaitingApproval,
            }],
            approvals: vec![Approval {
                request_id: "r1".to_string(),
                session_id: "dsh:a".to_string(),
                tool: "Bash".to_string(),
                detail: "rm -rf build".to_string(),
                requested_at: 1,
            }],
            pending: vec![PendingInteraction {
                request_id: "r1".to_string(),
                session_id: "dsh:a".to_string(),
                status: RowStatus::WaitingApproval,
                requested_at: 1,
                approval: Some(Approval {
                    request_id: "r1".to_string(),
                    session_id: "dsh:a".to_string(),
                    tool: "Bash".to_string(),
                    detail: "rm -rf build".to_string(),
                    requested_at: 1,
                }),
            }],
        };

        tracker.reconcile(&card);
        assert_eq!(tracker.card(), card, "reconciliation adopts the snapshot");

        let after = tracker.approval_resolved("r1").unwrap();
        assert!(after.approvals.is_empty());
        assert_eq!(after.rows[0].status, RowStatus::Running);
        assert_eq!(after.rows[0].running_since, card.rows[0].running_since);
        assert_eq!(
            tracker.card(),
            Card {
                rows: after.rows.clone(),
                approvals: Vec::new(),
                pending: Vec::new(),
                ..card
            },
        );
    }

    #[test]
    fn reconciliation_keeps_a_known_rows_clock_but_takes_a_new_rows_snapshot_time() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", Some("Build")), 1_000);
        let card = Card {
            rows: vec![
                Row {
                    session_id: "dsh:a".to_string(),
                    title: "Build".to_string(),
                    running_since: 5_000,
                    waiting_since: None,
                    status: RowStatus::Running,
                },
                Row {
                    session_id: "dsh:b".to_string(),
                    title: "Tests".to_string(),
                    running_since: 5_000,
                    waiting_since: None,
                    status: RowStatus::Running,
                },
            ],
            approvals: Vec::new(),
            pending: Vec::new(),
        };

        tracker.reconcile(&card);
        assert_eq!(
            tracker.card().rows[0].running_since,
            1_000,
            "the row this tracker already knew keeps the instant it first saw",
        );
        assert_eq!(
            tracker.card().rows[1].running_since,
            5_000,
            "a row the tracker has never seen takes the snapshot's instant",
        );
    }

    #[test]
    fn a_finished_session_takes_its_approval_with_it() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", None), 1_000);
        tracker.approval_requested(&approval("r1", "a"));
        let card = tracker.finished(&key("a")).unwrap();
        assert!(
            card.is_empty(),
            "an approval for a session that is gone is not actionable"
        );
    }

    #[test]
    fn a_question_is_not_an_approval() {
        let mut tracker = Tracker::default();
        let mut question = approval("q1", "a");
        question.kind = InteractionKind::Question;
        assert!(tracker.approval_requested(&question).is_none());
    }

    #[test]
    fn the_body_names_every_row_and_no_others() {
        let mut tracker = Tracker::default();
        for (index, id) in ["a", "b", "c"].into_iter().enumerate() {
            tracker.running(&session(id, Some(id)), 1_000 + index as i64);
        }
        let body = activity_body(
            &tracker.card(),
            &["device-1".to_string()],
            "host-id",
            "Mac mini",
            99_000,
        );
        assert_eq!(body["phase"], "start");
        let rows = body["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["sessionId"], "dsh:a");
        assert_eq!(rows[0]["runningSince"], 1_000);
        assert_eq!(rows[0]["status"], "running");
        assert!(rows[0].get("waitingSince").is_none());
        assert_eq!(body["hostId"], "host-id");
        assert_eq!(body["hostName"], "Mac mini");
        assert_eq!(body["snapshotVersion"], 2);
        assert_eq!(body["sentAt"], 99_000);
        assert_eq!(body["exceptDeviceIds"][0], "device-1");
        assert!(body["approvals"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_waiting_main_session_is_included_and_child_sessions_are_excluded() {
        let mut waiting = session("question", Some("Question"));
        waiting.key = SessionKey::new(EngineId::Claude, "question");
        waiting.running = false;
        let mut child = session("child", Some("Child"));
        child.origin = Some("subagent".to_string());
        let mut fork = session("fork", Some("Fork"));
        fork.origin = Some("fork".to_string());
        let mut question = approval("q1", "question");
        question.session = Some(waiting.key.clone());
        question.kind = InteractionKind::Question;

        let card = snapshot_card(&[waiting, child, fork], &[question]);
        assert_eq!(card.rows.len(), 2);
        assert!(card.rows.iter().any(|row| row.session_id == "dsh:fork"));
        assert_eq!(card.rows[0].session_id, "claude:question");
        assert_eq!(card.rows[0].status, RowStatus::WaitingAnswer);
        assert!(card.approvals.is_empty());
    }

    #[test]
    fn pending_does_not_restart_the_turn_clock() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", Some("Build")), 1_000);
        let mut question = approval("q1", "a");
        question.kind = InteractionKind::Question;
        let waiting = tracker.interaction_requested(&question).unwrap();
        assert_eq!(waiting.rows[0].status, RowStatus::WaitingAnswer);
        assert_eq!(waiting.rows[0].running_since, 1_000);
        assert_eq!(waiting.rows[0].waiting_since, Some(question.requested_at));
        let body = activity_body(&waiting, &[], "id", "host", 10);
        assert_eq!(body["rows"][0]["waitingSince"], question.requested_at);
        let resolved = tracker.approval_resolved("q1").unwrap();
        assert_eq!(resolved.rows[0].status, RowStatus::Running);
        assert_eq!(resolved.rows[0].running_since, 1_000);
        assert_eq!(resolved.rows[0].waiting_since, None);
    }

    #[test]
    fn snapshot_uses_the_oldest_request_for_the_visible_wait_status() {
        let mut running = session("a", Some("Build"));
        running.updated_at = 90_000;
        let mut oldest = approval("r-old", "a");
        oldest.requested_at = 10_000;
        let mut newer = approval("r-new", "a");
        newer.requested_at = 20_000;
        let mut question = approval("q", "a");
        question.kind = InteractionKind::Question;
        question.requested_at = 1_000;

        let card = snapshot_card(&[running], &[newer, question, oldest]);
        assert_eq!(card.rows[0].status, RowStatus::WaitingApproval);
        assert_eq!(card.rows[0].running_since, 90_000);
        assert_eq!(card.rows[0].waiting_since, Some(10_000));
    }

    #[test]
    fn a_new_turn_after_finish_gets_a_new_clock_start() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", Some("Build")), 1_000);
        tracker.finished(&key("a")).unwrap();
        let next = tracker
            .running(&session("a", Some("Build")), 5_000)
            .unwrap();
        assert_eq!(next.rows[0].running_since, 5_000);
    }

    #[test]
    fn the_body_keeps_more_than_the_four_rows_the_relay_displays() {
        let mut tracker = Tracker::default();
        for index in 0..5 {
            tracker.running(
                &session(&format!("s{index}"), Some("Session")),
                1_000 + index,
            );
        }
        let body = activity_body(&tracker.card(), &[], "id", "host", 10);
        assert_eq!(body["rows"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn a_turn_end_notification_says_which_session_and_whether_to_end_the_card() {
        let body = turn_end_body(
            "Fix the tests",
            &key("a"),
            true,
            &[],
            false,
            "host-id",
            "Host",
        );
        assert_eq!(body["kind"], "turn_end");
        assert_eq!(body["sessionId"], "dsh:a");
        assert_eq!(body["endLiveActivity"], true);
        assert_eq!(body["body"], "Fix the tests is done");

        // A session with no title still gets a sentence rather than an empty one.
        let unnamed = turn_end_body("", &key("b"), false, &[], false, "host-id", "Host");
        assert_eq!(unnamed["body"], "Session is done");
        assert_eq!(unnamed["endLiveActivity"], false);

        // A phone that registered a Chinese locale gets Chinese text, and the
        // session name stays the operator's own words.
        let zh = turn_end_body(
            "Fix the tests",
            &key("c"),
            true,
            &[],
            true,
            "host-id",
            "Host",
        );
        assert_eq!(zh["title"], "会话已完成");
        assert_eq!(zh["body"], "Fix the tests 已完成");
        let zh_unnamed = turn_end_body("", &key("d"), false, &[], true, "host-id", "Host");
        assert_eq!(zh_unnamed["body"], "会话 已完成");
    }
}
