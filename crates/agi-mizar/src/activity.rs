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
//! The island is one activity with up to two rows, not one activity per session.
//! That is Apple's shape (an app may hold a handful of activities, and the lock
//! screen shows them as cards, not as a list) and the phone's: the app draws the
//! same `rows` array from its own state. So "a session finished while two were
//! running" is an update carrying the remaining row, and "the last one finished"
//! is the end of the card.
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
//! carries the pending approvals beside the rows; the phone draws them, and (in a
//! later build) answers them from there. Everything here is `serde_json`, so a
//! phone that predates the fields ignores them rather than failing to decode.

use std::collections::BTreeMap;

use agi_protocol::domain::{Interaction, InteractionKind, Session, SessionKey};
use serde_json::{json, Value};

/// How many rows one card carries.
///
/// Apple's contract asks for a small card, and the widget that draws it caps the
/// list. Mirrored in the relay (`MAX_ACTIVITY_ROWS`) and the app, so all three
/// agree about which sessions are on screen.
pub const MAX_ROWS: usize = 2;

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
}

/// Everything the island draws.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Card {
    /// Oldest first, already cut to [`MAX_ROWS`].
    pub rows: Vec<Row>,
    /// Oldest first.
    pub approvals: Vec<Approval>,
}

impl Card {
    /// Whether there is nothing to show, which means the activity should end.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.approvals.is_empty()
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
    approvals: BTreeMap<String, Approval>,
}

impl Tracker {
    /// One session is running.
    ///
    /// - Returns: the card when the answer changed, `None` for a session that
    ///   was already on it (a running session is updated often, and every one of
    ///   those updates is a push).
    pub fn running(&mut self, session: &Session, now_ms: i64) -> Option<Card> {
        let session_id = session.key.to_string();
        if self.rows.iter().any(|row| row.session_id == session_id) {
            return None;
        }
        self.rows.push(Row {
            session_id,
            title: session.title.clone().unwrap_or_default(),
            running_since: now_ms,
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
        let approvals = self.approvals.len();
        self.approvals
            .retain(|_, approval| approval.session_id != session_id);
        if self.rows.len() == before && self.approvals.len() == approvals {
            return None;
        }
        Some(self.card())
    }

    /// The agent asked for permission.
    pub fn approval_requested(&mut self, interaction: &Interaction) -> Option<Card> {
        if interaction.kind != InteractionKind::Approval {
            return None;
        }
        let approval = Approval {
            request_id: interaction.id.clone(),
            session_id: interaction
                .session
                .as_ref()
                .map(|key| key.to_string())
                .unwrap_or_default(),
            tool: interaction.title.clone(),
            detail: interaction.detail.clone().unwrap_or_default(),
        };
        let changed = self
            .approvals
            .insert(approval.request_id.clone(), approval)
            .is_none();
        changed.then(|| self.card())
    }

    /// Somebody answered it -- this phone, the desktop window, or a timeout.
    pub fn approval_resolved(&mut self, id: &str) -> Option<Card> {
        self.approvals.remove(id).map(|_| self.card())
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
        Card {
            rows: self.rows.iter().take(MAX_ROWS).cloned().collect(),
            approvals: self.approvals.values().cloned().collect(),
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
pub fn activity_body(card: &Card, except: &[String]) -> Value {
    json!({
        "phase": "start",
        "rows": card
            .rows
            .iter()
            .map(|row| json!({
                "sessionId": row.session_id,
                "title": row.title,
                "runningSince": row.running_since,
            }))
            .collect::<Vec<_>>(),
        "approvals": card
            .approvals
            .iter()
            .map(|approval| json!({
                "requestId": approval.request_id,
                "sessionId": approval.session_id,
                "tool": approval.tool,
                "detail": approval.detail,
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
) -> Value {
    let name = if title.trim().is_empty() {
        "Session".to_string()
    } else {
        title.trim().to_string()
    };
    json!({
        "kind": "turn_end",
        "title": "Turn finished",
        "body": format!("{name} is done"),
        "sessionId": session.to_string(),
        "threadId": session.to_string(),
        "timeSensitive": false,
        "endLiveActivity": end_live_activity,
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
    fn a_running_session_becomes_the_first_row() {
        let mut tracker = Tracker::default();
        let card = tracker.running(&session("a", Some("Fix the tests")), 1_000).unwrap();
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
        assert_eq!(card.rows[0].running_since, 1_000, "the first row's clock is untouched");
    }

    #[test]
    fn finishing_the_last_row_empties_the_card() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", None), 1_000);
        let card = tracker.finished(&key("a")).unwrap();
        assert!(card.is_empty(), "an empty card is what ends the activity");
        assert!(tracker.finished(&key("a")).is_none(), "a second end is not news");
    }

    #[test]
    fn a_third_session_stays_off_the_card_and_moves_up_when_one_finishes() {
        let mut tracker = Tracker::default();
        for (index, id) in ["a", "b", "c"].into_iter().enumerate() {
            tracker.running(&session(id, Some(id)), 1_000 + index as i64);
        }
        let card = tracker.card();
        assert_eq!(card.rows.len(), MAX_ROWS, "the widget draws two rows");
        assert_eq!(card.rows[1].session_id, "dsh:b");
        let card = tracker.finished(&key("a")).unwrap();
        assert_eq!(card.rows[0].session_id, "dsh:b");
        assert_eq!(card.rows[1].session_id, "dsh:c", "the third row moved up");
    }

    #[test]
    fn an_approval_joins_the_rows_and_leaves_when_it_is_answered() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", Some("Build")), 1_000);
        let card = tracker.approval_requested(&approval("r1", "a")).unwrap();
        assert_eq!(card.approvals.len(), 1);
        assert_eq!(card.approvals[0].request_id, "r1");
        assert_eq!(card.approvals[0].tool, "Bash");
        assert_eq!(card.rows.len(), 1, "the row stays: the turn is blocked, not over");

        let card = tracker.approval_resolved("r1").unwrap();
        assert!(card.approvals.is_empty());
        assert_eq!(card.rows.len(), 1);
        assert!(tracker.approval_resolved("r1").is_none(), "answered twice is not news");
    }

    #[test]
    fn a_finished_session_takes_its_approval_with_it() {
        let mut tracker = Tracker::default();
        tracker.running(&session("a", None), 1_000);
        tracker.approval_requested(&approval("r1", "a"));
        let card = tracker.finished(&key("a")).unwrap();
        assert!(card.is_empty(), "an approval for a session that is gone is not actionable");
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
        let body = activity_body(&tracker.card(), &["device-1".to_string()]);
        assert_eq!(body["phase"], "start");
        let rows = body["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["sessionId"], "dsh:a");
        assert_eq!(rows[0]["runningSince"], 1_000);
        assert_eq!(body["exceptDeviceIds"][0], "device-1");
        assert!(body["approvals"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_turn_end_notification_says_which_session_and_whether_to_end_the_card() {
        let body = turn_end_body("Fix the tests", &key("a"), true, &[]);
        assert_eq!(body["kind"], "turn_end");
        assert_eq!(body["sessionId"], "dsh:a");
        assert_eq!(body["endLiveActivity"], true);
        assert_eq!(body["body"], "Fix the tests is done");

        // A session with no title still gets a sentence rather than an empty one.
        let unnamed = turn_end_body("", &key("b"), false, &[]);
        assert_eq!(unnamed["body"], "Session is done");
        assert_eq!(unnamed["endLiveActivity"], false);
    }
}
