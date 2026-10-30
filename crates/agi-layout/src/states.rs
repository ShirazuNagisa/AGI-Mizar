//! The state machine behind the state channel.
//!
//! Pure: it takes what happened, hands back the frames to send. No clock, no
//! tokio, no I/O -- so the whole thing is testable and the kernel only has to
//! wire a ticker to [`StateMachine::heartbeat`].
//!
//! ### Intervals
//!
//! Every state change opens a new interval. That is the only signal the phone
//! needs to tell "this is the same state, renewed" from "this is a new state",
//! and it is what makes a lost or reordered frame harmless.
//!
//! ### Why the final answer is re-sent as a snapshot
//!
//! The engine delivers assistant text a whole message at a time, so the kernel
//! cannot know a message is the *last* one while it is being written. It learns
//! at `turn/end`. Rather than show the answer as narration forever, the final
//! message is re-emitted as `stream_final` with a snapshot, then the terminal
//! state follows in the same batch. The phone sees: text arrived, it is the
//! answer, the turn is over.

use agi_protocol::state_frame::StateFrame;
use agi_protocol::SessionState;

use crate::{state_of_turn, Facts, Live, State};

/// One session's state channel.
#[derive(Debug)]
pub struct StateMachine {
    state: SessionState,
    interval: u32,
    /// Bytes of streamed text emitted in the current interval.
    text_seq: u32,
    /// Text emitted in the current interval, for the snapshot on re-entry.
    text: String,
    /// Text seen since the last interval change, for the final-answer re-send.
    pending_text: String,
    started: bool,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            state: SessionState::Idle,
            interval: 0,
            text_seq: 0,
            text: String::new(),
            pending_text: String::new(),
            started: false,
        }
    }

    /// The state currently on the wire.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// The interval currently open.
    pub fn interval(&self) -> u32 {
        self.interval
    }

    /// Move to a state, opening a new interval when it is actually different.
    ///
    /// Re-asserting the same state is not a change: the phone keeps its text and
    /// the heartbeat does the renewing.
    fn enter(&mut self, state: SessionState) -> Option<StateFrame> {
        if self.started && self.state == state {
            // Same interval: the first frame of a new turn still needs to
            // announce the text buffer reset, but an unchanged state does not.
            return None;
        }
        self.interval = self.interval.wrapping_add(1);
        self.state = state;
        self.text_seq = 0;
        self.text.clear();
        self.started = true;
        let frame = StateFrame::begin(state, self.interval, &self.text);
        Some(frame)
    }

    /// A live signal from the record stream.
    pub fn on_live(&mut self, live: Live) -> Option<StateFrame> {
        self.enter(live.state())
    }

    /// A streamed chunk of assistant text.
    ///
    /// Emitted as `stream_narration`: while the message is being written the
    /// kernel cannot know whether it is the final one. The answer is re-sent as
    /// `stream_final` when the turn ends, if it turns out to be the last word.
    pub fn on_text(&mut self, chunk: &str) -> Vec<StateFrame> {
        if chunk.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.state != SessionState::StreamNarration {
            if let Some(frame) = self.enter(SessionState::StreamNarration) {
                out.push(frame);
            }
        }
        let frame = StateFrame::append(
            SessionState::StreamNarration,
            self.interval,
            self.text_seq,
            chunk,
        );
        self.text_seq += chunk.len() as u32;
        self.text.push_str(chunk);
        self.pending_text.push_str(chunk);
        out.push(frame);
        out
    }

    /// The turn ended. Returns the frames to send, in order.
    pub fn on_turn_end(&mut self, facts: &Facts, verdict: Option<State>) -> Vec<StateFrame> {
        let state = state_of_turn(facts, verdict);
        let mut out = Vec::new();

        // A finished answer is worth naming: the text already went out as
        // narration, so re-announce it as the answer before the terminal frame.
        let is_answer = state == SessionState::Completed && !self.pending_text.is_empty();
        if is_answer {
            if let Some(frame) = self.enter(SessionState::StreamFinal) {
                out.push(frame);
            }
            out.push(StateFrame::snapshot(
                SessionState::StreamFinal,
                self.interval,
                0,
                &self.pending_text,
            ));
            out.push(StateFrame::finish(
                SessionState::StreamFinal,
                self.interval,
                self.pending_text.len() as u32,
            ));
        }

        if let Some(frame) = self.enter(state) {
            out.push(frame);
        }
        self.pending_text.clear();
        out
    }

    /// The five-second renewal. `None` when the current state needs none.
    pub fn heartbeat(&mut self) -> Option<StateFrame> {
        if !self.started || !self.state.needs_heartbeat() {
            return None;
        }
        Some(StateFrame::heartbeat(self.state, self.interval))
    }

    /// The text this machine has emitted in the current interval.
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agi_protocol::state_frame::Flags;

    fn facts(reason: &str, text: &str) -> Facts {
        Facts {
            reason: reason.to_string(),
            texts: if text.is_empty() {
                Vec::new()
            } else {
                vec![text.to_string()]
            },
            user: "u".into(),
            ..Facts::default()
        }
    }

    #[test]
    fn a_state_change_opens_a_new_interval() {
        let mut m = StateMachine::new();
        let a = m.on_live(Live::Thinking).unwrap();
        assert_eq!(a.interval, 1);
        assert_eq!(a.state, SessionState::Thinking);

        // Re-asserting the same state is not a change.
        assert!(m.on_live(Live::Thinking).is_none());

        let b = m.on_live(Live::Tool).unwrap();
        assert_eq!(b.interval, 2);
        assert_eq!(b.state, SessionState::Tool);
    }

    #[test]
    fn text_opens_a_stream_and_appends() {
        let mut m = StateMachine::new();
        let frames = m.on_text("先说结论");
        assert_eq!(frames.len(), 2, "begin + append");
        assert_eq!(frames[0].state, SessionState::StreamNarration);
        assert!(frames[0].flags.has(Flags::SNAPSHOT));
        assert_eq!(frames[1].text, "先说结论");
        assert_eq!(frames[1].text_seq, 0);

        let more = m.on_text("：改好了。");
        assert_eq!(more.len(), 1, "no new interval for the same stream");
        assert_eq!(more[0].text_seq, "先说结论".len() as u32);
        assert_eq!(m.text(), "先说结论：改好了。");
    }

    #[test]
    fn a_finished_answer_is_reannounced_then_closed() {
        let mut m = StateMachine::new();
        m.on_text("改好了，测试通过。");
        let frames = m.on_turn_end(&facts("completed", "改好了，测试通过。"), Some(State::Done));

        // stream_final begin + snapshot + finish + completed
        assert_eq!(frames.len(), 4, "{frames:?}");
        assert_eq!(frames[0].state, SessionState::StreamFinal);
        assert_eq!(frames[1].text, "改好了，测试通过。");
        assert!(frames[1].flags.has(Flags::SNAPSHOT));
        assert!(frames[2].flags.has(Flags::FINAL));
        assert_eq!(frames[3].state, SessionState::Completed);
        assert!(!frames[3].state.needs_heartbeat());
    }

    #[test]
    fn a_failure_skips_the_answer_ceremony() {
        let mut m = StateMachine::new();
        m.on_text("我先看看…");
        let frames = m.on_turn_end(&facts("error", ""), None);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].state, SessionState::Failed);
    }

    #[test]
    fn only_held_states_heartbeat() {
        let mut m = StateMachine::new();
        assert!(m.heartbeat().is_none(), "nothing has happened yet");

        m.on_live(Live::Tool);
        let beat = m.heartbeat().unwrap();
        assert!(beat.is_heartbeat());
        assert_eq!(beat.interval, 1);

        m.on_turn_end(&facts("completed", "好了"), Some(State::Done));
        assert!(m.heartbeat().is_none(), "a terminal state does not renew");
    }

    #[test]
    fn a_new_turn_clears_the_text_buffer() {
        let mut m = StateMachine::new();
        m.on_text("第一轮的话");
        m.on_turn_end(&facts("completed", "第一轮的话"), Some(State::Done));
        m.on_live(Live::Thinking).unwrap();
        m.on_text("第二轮");
        assert_eq!(m.text(), "第二轮", "text must not leak across turns");
    }
}
