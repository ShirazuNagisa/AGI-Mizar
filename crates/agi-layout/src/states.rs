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
//!
//! The snapshot is the **last stretch of text**, not the whole turn glued
//! together: a turn that narrated three paragraphs and then answered is answered
//! by the last one. Re-sending everything turned one answer into four copies on
//! the phone.
//!
//! ### Why every interval is journalled
//!
//! The frames above are a live channel: a phone that was disconnected while the
//! turn ran has nothing to reconstruct from, because the host keeps no history of
//! what it streamed. So each interval opened in the current turn is also kept in
//! a [`JournalEntry`], text included, and `session.stream` hands the whole turn
//! back on request. It is bounded ([`JOURNAL_MAX_BYTES`], oldest first) so a
//! pathological turn cannot grow the kernel without limit.

use agi_protocol::state_frame::StateFrame;
use agi_protocol::SessionState;

use crate::{state_of_turn, Facts, Live, State};

/// How much streamed text one turn may keep for replay.
///
/// Every entry's text counts towards it; past the cap the oldest entries are
/// dropped and [`StateMachine::truncated`] says so, so the phone can tell "this
/// replay starts mid-turn" from "the turn was this short".
pub const JOURNAL_MAX_BYTES: usize = 256 * 1024;

/// One interval of the current turn, as the phone should see it replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// The interval this entry opened.
    pub interval: u32,
    /// The state the interval was in.
    pub state: SessionState,
    /// Text carried by the interval, in the order it was emitted.
    pub text: String,
    /// Whether the interval is the turn's final answer.
    pub final_: bool,
}

/// The largest text one state frame can carry, in bytes.
///
/// A u16 length on the wire (`state_frame.rs`), so this is also what every
/// decoder accepts; a frame over it is dropped as a length mismatch.
const MAX_FRAME_TEXT: usize = u16::MAX as usize;

/// Split text into pieces a state frame can carry, at UTF-8 boundaries.
fn text_pieces(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut take = rest.len().min(MAX_FRAME_TEXT);
        while take > 0 && !rest.is_char_boundary(take) {
            take -= 1;
        }
        // `take` cannot be zero: MAX_FRAME_TEXT is far larger than one
        // character, and byte 0 of a str is always a boundary.
        let (piece, tail) = rest.split_at(take.max(1));
        rest = tail;
        Some(piece)
    })
}

/// One session's state channel.
#[derive(Debug)]
pub struct StateMachine {
    state: SessionState,
    interval: u32,
    /// Bytes of streamed text emitted in the current interval.
    text_seq: u32,
    /// Text emitted in the current interval, for the snapshot on re-entry.
    text: String,
    /// Every interval of the current turn, oldest first, for replay.
    journal: Vec<JournalEntry>,
    /// Bytes of text currently held in `journal`.
    journal_bytes: usize,
    /// Whether the oldest journal entries were dropped to stay within budget.
    truncated: bool,
    /// The turn the journal belongs to, once a turn has started.
    turn: Option<u32>,
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
            journal: Vec::new(),
            journal_bytes: 0,
            truncated: false,
            turn: None,
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

    /// Bytes of text the current interval has emitted.
    pub fn text_seq(&self) -> u32 {
        self.text_seq
    }

    /// The turn the journal belongs to, once one has started.
    pub fn turn(&self) -> Option<u32> {
        self.turn
    }

    /// The turn's intervals so far, oldest first, for replay.
    pub fn journal(&self) -> &[JournalEntry] {
        &self.journal
    }

    /// Whether the oldest intervals were dropped to stay within budget.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The last stretch of text the turn emitted.
    ///
    /// This -- not the whole turn concatenated -- is the answer: the engine sends
    /// one message at a time and only the final one is the answer, so a turn that
    /// narrated before answering must not have its narration folded into it.
    pub fn last_text_run(&self) -> Option<&str> {
        self.last_run_entry().map(|entry| entry.text.as_str())
    }

    /// The journal entry the last stretch of text is in.
    fn last_run_entry(&self) -> Option<&JournalEntry> {
        self.journal
            .iter()
            .rev()
            .find(|entry| entry.state.carries_text() && !entry.text.is_empty())
    }

    /// Move to a state, opening a new interval when it is actually different.
    ///
    /// Re-asserting the same state is not a change: the phone keeps its text and
    /// the heartbeat does the renewing.
    fn enter(&mut self, state: SessionState) -> Option<StateFrame> {
        if self.started && self.state == state {
            return None;
        }
        Some(self.open(state))
    }

    /// Open a new interval, whatever state was on the wire before.
    ///
    /// Always journals: the journal is "the intervals of this turn", and a turn
    /// starts one even when the state code happens to be the same as the previous
    /// turn's last.
    fn open(&mut self, state: SessionState) -> StateFrame {
        self.interval = self.interval.wrapping_add(1);
        self.state = state;
        self.text_seq = 0;
        self.text.clear();
        self.started = true;
        self.journal.push(JournalEntry {
            interval: self.interval,
            state,
            text: String::new(),
            final_: false,
        });
        StateFrame::begin(state, self.interval, &self.text)
    }

    /// A turn began. Clears the last turn's journal and announces the new one.
    ///
    /// `None` for a repeat of the turn already running: engines re-announce a
    /// turn they have already started, and nothing has changed.
    pub fn on_turn_start(&mut self, turn: u32) -> Option<StateFrame> {
        if self.turn == Some(turn) {
            return None;
        }
        self.turn = Some(turn);
        self.journal.clear();
        self.journal_bytes = 0;
        self.truncated = false;
        // `open`, not `enter`: a new turn always gets a new interval, because the
        // phone's text buffer belongs to the interval and it has to be reset even
        // if the previous turn ended while this state was already on the wire.
        Some(self.open(SessionState::Thinking))
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
        // A frame's text length is a u16 on the wire, and the engines deliver a
        // whole assistant message at a time: one answer holding a large file is
        // more than 65535 bytes, which used to encode a truncated frame every
        // decoder rejects. The phone then dropped that append *and every later
        // one for the interval* (its `textSeq` continuity check), so the live
        // answer was missing a stretch. `text_seq` is exactly the field for
        // splitting one message into several appends.
        for piece in text_pieces(chunk) {
            let frame = StateFrame::append(
                SessionState::StreamNarration,
                self.interval,
                self.text_seq,
                piece,
            );
            self.text_seq += piece.len() as u32;
            self.text.push_str(piece);
            if let Some(entry) = self.journal.last_mut() {
                entry.text.push_str(piece);
                self.journal_bytes += piece.len();
            }
            out.push(frame);
        }
        self.trim();
        out
    }

    /// The turn ended. Returns the frames to send, in order.
    pub fn on_turn_end(&mut self, facts: &Facts, verdict: Option<State>) -> Vec<StateFrame> {
        let state = state_of_turn(facts, verdict);
        let mut out = Vec::new();

        // A finished answer is worth naming: the text already went out as
        // narration, so re-announce it as the answer before the terminal frame.
        // `final_` on the journal is what says it has already happened: a turn is
        // settled twice (structure first, model verdict second) and only the
        // first one may re-announce.
        let announced = self.last_run_entry().is_some_and(|entry| entry.final_);
        let answer = self.last_text_run().map(str::to_string);
        if state == SessionState::Completed && !announced {
            if let Some(text) = answer {
                // `enter` opens the interval -- it may not *emit* its frame: the
                // `begin` frame of a text-carrying state is an empty snapshot, and
                // a phone that watches intervals would open an empty answer block
                // for it, then fill that block with the same text it already drew
                // as narration. The snapshot below is the interval's first frame
                // instead, which is what the protocol asks a first frame to be.
                if self.enter(SessionState::StreamFinal).is_some() {
                    // The snapshot *is* this interval's text, so the journal says
                    // so too: a replay draws the answer from here rather than
                    // gluing the turn's narration back together.
                    let added = match self.journal.last_mut() {
                        Some(entry) => {
                            entry.text = text.clone();
                            entry.final_ = true;
                            entry.text.len()
                        }
                        None => 0,
                    };
                    self.journal_bytes += added;
                    self.trim();
                    self.text = text.clone();
                    self.text_seq = text.len() as u32;
                    // The snapshot carries the whole answer and is subject to
                    // the same u16 limit as an append: the first piece is the
                    // snapshot, the rest are appends at increasing offsets.
                    let mut offset = 0u32;
                    for (index, piece) in text_pieces(&text).enumerate() {
                        if index == 0 {
                            out.push(StateFrame::snapshot(
                                SessionState::StreamFinal,
                                self.interval,
                                0,
                                piece,
                            ));
                        } else {
                            out.push(StateFrame::append(
                                SessionState::StreamFinal,
                                self.interval,
                                offset,
                                piece,
                            ));
                        }
                        offset += piece.len() as u32;
                    }
                    out.push(StateFrame::finish(
                        SessionState::StreamFinal,
                        self.interval,
                        offset,
                    ));
                }
            }
        }

        if let Some(frame) = self.enter(state) {
            out.push(frame);
        }
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

    /// Drop the oldest intervals until the journal fits its budget.
    ///
    /// The newest entry always survives, even when it alone is over budget: an
    /// empty replay tells the phone nothing, and the phone still has the last
    /// interval on the wire.
    fn trim(&mut self) {
        while self.journal_bytes > JOURNAL_MAX_BYTES && self.journal.len() > 1 {
            let dropped = self.journal.remove(0);
            self.journal_bytes = self.journal_bytes.saturating_sub(dropped.text.len());
            self.truncated = true;
        }
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
    fn a_long_message_is_split_into_frames_every_decoder_accepts() {
        // The wire's text length is a u16, and the engines deliver a whole
        // assistant message at a time. A 90 KB answer used to encode a
        // truncated frame that every decoder rejected -- and the phone then
        // dropped every later append for the interval, so the live answer was
        // missing a stretch. `text_seq` is the field that makes splitting safe.
        let mut m = StateMachine::new();
        let long = "答".repeat(30_000); // 90_000 bytes, 3 per character
        let frames = m.on_text(&long);
        // The first frame opens the interval and carries no text; the rest are
        // the pieces.
        let pieces: Vec<&StateFrame> = frames
            .iter()
            .filter(|frame| !frame.text.is_empty())
            .collect();
        assert_eq!(pieces.len(), 2, "one frame cannot carry 90 KB");
        let mut offset = 0u32;
        for frame in &pieces {
            let decoded = StateFrame::decode(&frame.encode()).expect("every piece must decode");
            assert!(decoded.text.len() <= u16::MAX as usize);
            assert_eq!(decoded.text_seq, offset);
            offset += decoded.text.len() as u32;
        }
        assert_eq!(offset as usize, long.len());
        assert_eq!(m.text(), long, "the interval still holds the whole answer");
        // A split must land on a character boundary: the pieces are valid &str
        // by construction, and joining them is the original text.
        assert_eq!(
            pieces
                .iter()
                .map(|frame| frame.text.as_str())
                .collect::<String>(),
            long
        );
    }

    #[test]
    fn a_long_final_answer_is_reannounced_in_decodable_pieces() {
        // The end-of-turn snapshot carries the whole answer too, and is subject
        // to the same u16 limit.
        let mut m = StateMachine::new();
        let long = "x".repeat(70_000);
        m.on_text(&long);
        let frames = m.on_turn_end(&facts("completed", &long), Some(State::Done));
        let mut reannounced = String::new();
        for frame in &frames {
            let decoded = StateFrame::decode(&frame.encode()).expect("every piece must decode");
            if decoded.state == SessionState::StreamFinal
                && decoded.flags.has(Flags::APPEND)
                && !decoded.flags.has(Flags::FINAL)
            {
                reannounced.push_str(&decoded.text);
            }
        }
        assert_eq!(reannounced, long);
    }

    #[test]
    fn a_finished_answer_is_reannounced_then_closed() {
        let mut m = StateMachine::new();
        m.on_text("改好了，测试通过。");
        let frames = m.on_turn_end(&facts("completed", "改好了，测试通过。"), Some(State::Done));

        // snapshot + finish + completed. No empty `begin` before the snapshot:
        // a phone that opens a block per interval would draw the answer twice.
        assert_eq!(frames.len(), 3, "{frames:?}");
        assert_eq!(frames[0].state, SessionState::StreamFinal);
        assert_eq!(frames[0].text, "改好了，测试通过。");
        assert!(frames[0].flags.has(Flags::SNAPSHOT));
        assert_eq!(frames[1].state, SessionState::StreamFinal);
        assert!(frames[1].flags.has(Flags::FINAL));
        assert_eq!(frames[2].state, SessionState::Completed);
        assert!(!frames[2].state.needs_heartbeat());
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

    #[test]
    fn the_journal_keeps_every_interval_of_the_turn() {
        let mut m = StateMachine::new();
        let first = m.on_turn_start(7).unwrap();
        assert_eq!(first.state, SessionState::Thinking);
        assert_eq!(m.turn(), Some(7));

        m.on_live(Live::Tool);
        m.on_text("我先看一下…");
        m.on_live(Live::Tool);
        m.on_text("改好了。");

        let journal = m.journal();
        assert_eq!(journal.len(), 5, "{journal:?}");
        assert_eq!(journal[0].state, SessionState::Thinking);
        assert_eq!(journal[1].state, SessionState::Tool);
        assert_eq!(journal[2].state, SessionState::StreamNarration);
        assert_eq!(journal[2].text, "我先看一下…");
        assert_eq!(journal[3].state, SessionState::Tool);
        assert_eq!(journal[4].text, "改好了。");
        // Intervals are the ones the frames carry, and none is an answer yet.
        assert!(journal.iter().all(|entry| !entry.final_));
        assert!(journal
            .windows(2)
            .all(|pair| pair[0].interval < pair[1].interval));
        assert!(!m.truncated());

        // A second chunk of the same message lands in the same entry.
        m.on_text("测试也过了。");
        assert_eq!(m.journal().len(), 5);
        assert_eq!(m.journal()[4].text, "改好了。测试也过了。");
    }

    #[test]
    fn the_journal_starts_over_with_a_new_turn() {
        let mut m = StateMachine::new();
        m.on_turn_start(3).unwrap();
        m.on_text("第一轮的话");
        m.on_turn_end(&facts("completed", "第一轮的话"), Some(State::Done));
        assert!(m.journal().len() > 1, "the first turn left a journal");

        let second = m.on_turn_start(4).unwrap();
        assert_eq!(
            second.interval,
            m.interval(),
            "a new turn opens an interval"
        );
        assert_eq!(m.turn(), Some(4));
        assert_eq!(m.journal().len(), 1, "{:?}", m.journal());
        assert_eq!(m.journal()[0].interval, m.interval());
        assert_eq!(m.journal()[0].state, SessionState::Thinking);
        assert!(m.last_text_run().is_none(), "nothing carried over");

        // Engines re-announce a turn they already started; that is not a new one.
        assert!(m.on_turn_start(4).is_none());
        assert_eq!(m.journal().len(), 1);
    }

    #[test]
    fn the_answer_snapshot_is_the_last_stretch_of_text() {
        let mut m = StateMachine::new();
        m.on_turn_start(1).unwrap();
        m.on_text("我先看一下…");
        m.on_live(Live::Tool);
        m.on_text("改好了。");

        let frames = m.on_turn_end(
            &facts("completed", "我先看一下…改好了。"),
            Some(State::Done),
        );
        let snapshot = frames
            .iter()
            .find(|frame| {
                frame.state == SessionState::StreamFinal
                    && frame.flags.has(Flags::SNAPSHOT)
                    && !frame.text.is_empty()
            })
            .expect("the answer is re-announced");
        assert_eq!(
            snapshot.text, "改好了。",
            "the answer is the last message, not the turn glued together"
        );

        // The journal says the same thing: the final interval holds the answer.
        let final_entry = m
            .journal()
            .iter()
            .find(|entry| entry.final_)
            .expect("the answer is marked as the answer");
        assert_eq!(final_entry.state, SessionState::StreamFinal);
        assert_eq!(final_entry.text, "改好了。");
        assert_eq!(m.last_text_run(), Some("改好了。"));
        assert_eq!(
            m.journal().last().unwrap().state,
            SessionState::Completed,
            "the terminal interval closes the turn after the answer"
        );
    }

    #[test]
    fn a_settled_turn_is_not_announced_twice() {
        let mut m = StateMachine::new();
        m.on_turn_start(1).unwrap();
        m.on_text("改好了。");
        let facts = facts("completed", "改好了。");
        assert_eq!(m.on_turn_end(&facts, Some(State::Done)).len(), 3);

        // The model's verdict lands a moment later and agrees with the structure:
        // the answer must not be re-announced as a second copy.
        assert!(m.on_turn_end(&facts, Some(State::Done)).is_empty());
        assert_eq!(
            m.journal().iter().filter(|entry| entry.final_).count(),
            1,
            "{:?}",
            m.journal()
        );
    }

    #[test]
    fn a_turn_over_budget_keeps_the_newest_text_and_says_so() {
        let mut m = StateMachine::new();
        m.on_turn_start(1).unwrap();
        let chunk = "x".repeat(64 * 1024);
        for _ in 0..6 {
            m.on_live(Live::Tool);
            m.on_text(&chunk);
        }

        assert!(m.truncated(), "6 x 64 KiB is over the 256 KiB budget");
        let kept: usize = m.journal().iter().map(|entry| entry.text.len()).sum();
        assert!(kept <= JOURNAL_MAX_BYTES, "kept {kept} bytes");
        assert_eq!(
            m.journal().last().unwrap().text.len(),
            chunk.len(),
            "the newest interval always survives"
        );
        assert!(
            m.journal().len() < 12,
            "the oldest intervals were dropped: {:?}",
            m.journal().len()
        );
    }
}
