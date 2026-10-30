//! The state channel: one small binary frame carrying a session state.
//!
//! A JSON object for "the agent is thinking" is 40+ bytes; the phone redraws it
//! every five seconds; over a metered relay that is real money. This frame is
//! 13 bytes plus whatever text the state carries.
//!
//! ```text
//! 0   u8   kind    0x53 ('S')
//! 1   u8   state   SessionState code
//! 2   u8   flags   see Flags
//! 3   u32  interval  monotonic; a new interval means "this state starts now"
//! 7   u32  text seq  position of the first byte of text in this interval
//! 11  u16  length   payload bytes
//! 13  ...  payload  UTF-8 body text, when the frame carries any
//! ```
//!
//! ### Why an interval id and not a timestamp
//!
//! The phone needs one cheap question answered: *is this frame newer than what I
//! am showing, or a duplicate heartbeat?* An interval id answers it with an
//! integer compare and no clock assumptions. Timestamps would additionally need
//! the host's clock to be sane, which it is not guaranteed to be.
//!
//! ### Why heartbeats
//!
//! An interval state is a promise: "this is still true". If the host dies, the
//! phone must stop believing it. `heartbeat()` every five seconds is that
//! promise being renewed; [`STALE_AFTER_MS`] is when the phone stops believing
//! it. Terminal states do not heartbeat -- they are the end of the story.

use crate::session_state::{SessionState, StateKind};

/// The first byte of every state frame. Chosen to never collide with the JSON
/// frames MizarKit v1 already carries, which always start with `{`.
pub const MAGIC: u8 = b'S';

/// Fixed header length.
pub const HEADER_LEN: usize = 13;

/// How often the host renews an interval state.
pub const HEARTBEAT_MS: u64 = 5_000;

/// When a phone should consider an interval dead. Slightly over two missed
/// heartbeats, so a single dropped frame does not flicker the UI.
pub const STALE_AFTER_MS: u64 = 12_000;

/// What a frame is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags(pub u8);

impl Flags {
    /// A renewal of the current interval. Carries no text.
    pub const HEARTBEAT: u8 = 0b0000_0001;
    /// Text is appended at `text_seq`.
    pub const APPEND: u8 = 0b0000_0010;
    /// Text replaces the buffer (used on the first frame of an interval and
    /// after a reconnect, so a phone can join a stream mid-flight).
    pub const SNAPSHOT: u8 = 0b0000_0100;
    /// This is the last chunk of the stream.
    pub const FINAL: u8 = 0b0000_1000;

    pub fn new(bits: u8) -> Self {
        Self(bits)
    }
    pub fn bits(self) -> u8 {
        self.0
    }
    pub fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
}

/// Why a frame was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Shorter than the header.
    Truncated,
    /// Does not start with [`MAGIC`].
    NotAStateFrame,
    /// The declared payload length and the actual length disagree.
    LengthMismatch,
    /// A heartbeat carrying text, or a text frame with no text flag.
    ContradictoryFlags,
    /// The state code is not one this build knows.
    UnknownState(u8),
    /// Body text on a state that cannot carry any.
    TextNotAllowed(u8),
    /// Text is not valid UTF-8.
    NotUtf8,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Truncated => write!(f, "frame shorter than its header"),
            FrameError::NotAStateFrame => write!(f, "not a state frame"),
            FrameError::LengthMismatch => write!(f, "declared payload length is wrong"),
            FrameError::ContradictoryFlags => write!(f, "flags contradict the payload"),
            FrameError::UnknownState(c) => write!(f, "unknown state code {c}"),
            FrameError::TextNotAllowed(c) => write!(f, "state {c} carries no text"),
            FrameError::NotUtf8 => write!(f, "text payload is not UTF-8"),
        }
    }
}

impl std::error::Error for FrameError {}

/// One frame of the state channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateFrame {
    pub state: SessionState,
    /// Monotonic per session. A new value starts a new interval.
    pub interval: u32,
    /// Byte offset in the current interval's text. Meaningless without text.
    pub text_seq: u32,
    pub flags: Flags,
    /// Body text, empty for a heartbeat.
    pub text: String,
}

impl StateFrame {
    /// Start a new interval.
    ///
    /// The first frame of an interval is a snapshot even when the buffer is
    /// empty, so a phone that missed the start still lands in a known state.
    pub fn begin(state: SessionState, interval: u32, text: &str) -> Self {
        let carries = state.carries_text();
        Self {
            state,
            interval,
            text_seq: 0,
            flags: Flags::new(if carries {
                Flags::SNAPSHOT | Flags::APPEND
            } else {
                0
            }),
            text: if carries { text.to_string() } else { String::new() },
        }
    }

    /// Renew an interval. No text, by construction.
    pub fn heartbeat(state: SessionState, interval: u32) -> Self {
        Self {
            state,
            interval,
            text_seq: 0,
            flags: Flags::new(Flags::HEARTBEAT),
            text: String::new(),
        }
    }

    /// Append streamed text at `text_seq`.
    pub fn append(state: SessionState, interval: u32, text_seq: u32, chunk: &str) -> Self {
        Self {
            state,
            interval,
            text_seq,
            flags: Flags::new(Flags::APPEND),
            text: chunk.to_string(),
        }
    }

    /// Replace the buffer with everything so far. Sent first, and after a
    /// reconnect.
    pub fn snapshot(state: SessionState, interval: u32, text_seq: u32, text: &str) -> Self {
        Self {
            state,
            interval,
            text_seq,
            flags: Flags::new(Flags::SNAPSHOT | Flags::APPEND),
            text: text.to_string(),
        }
    }

    /// Mark the stream as finished. Empty chunk is legal: "that is all".
    pub fn finish(state: SessionState, interval: u32, text_seq: u32) -> Self {
        Self {
            state,
            interval,
            text_seq,
            flags: Flags::new(Flags::FINAL),
            text: String::new(),
        }
    }

    /// Whether this frame renews rather than announces.
    pub fn is_heartbeat(&self) -> bool {
        self.flags.has(Flags::HEARTBEAT)
    }

    /// Whether this frame expects a heartbeat to follow.
    pub fn is_terminal(&self) -> bool {
        self.state.kind() == StateKind::Terminal
    }

    /// Encode to bytes.
    pub fn encode(&self) -> Vec<u8> {
        let payload = self.text.as_bytes();
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.push(MAGIC);
        out.push(self.state.code());
        out.push(self.flags.bits());
        out.extend_from_slice(&self.interval.to_be_bytes());
        out.extend_from_slice(&self.text_seq.to_be_bytes());
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Decode bytes, rejecting anything this build cannot honour.
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.len() < HEADER_LEN {
            return Err(FrameError::Truncated);
        }
        if bytes[0] != MAGIC {
            return Err(FrameError::NotAStateFrame);
        }
        let state = SessionState::from_code(bytes[1]).ok_or(FrameError::UnknownState(bytes[1]))?;
        let flags = Flags::new(bytes[2]);
        let interval = u32::from_be_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]);
        let text_seq = u32::from_be_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]);
        let len = u16::from_be_bytes([bytes[11], bytes[12]]) as usize;
        if bytes.len() != HEADER_LEN + len {
            return Err(FrameError::LengthMismatch);
        }

        let has_text = flags.has(Flags::APPEND);
        // A heartbeat is a promise that nothing changed. Checked first so the
        // error names the real fault instead of a downstream symptom.
        if flags.has(Flags::HEARTBEAT) && (has_text || len != 0) {
            return Err(FrameError::ContradictoryFlags);
        }
        if !has_text && len != 0 {
            return Err(FrameError::ContradictoryFlags);
        }
        if has_text && !state.carries_text() {
            return Err(FrameError::TextNotAllowed(state.code()));
        }

        let text = std::str::from_utf8(&bytes[HEADER_LEN..])
            .map_err(|_| FrameError::NotUtf8)?
            .to_string();

        Ok(Self {
            state,
            interval,
            text_seq,
            flags,
            text,
        })
    }
}

/// The phone's view of one session's state channel.
///
/// Kept here rather than in the app so both ends apply the same rules, and so
/// the "is this frame newer?" question has exactly one answer.
#[derive(Debug, Clone, Default)]
pub struct StateChannel {
    interval: Option<u32>,
    state: Option<SessionState>,
    text: String,
    /// When the last frame of the current interval arrived (epoch ms).
    last_at: i64,
    finished: bool,
}

/// What the display should do after receiving a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// A new interval: replace everything.
    Began,
    /// Same state, just a liveness renewal. Redraw nothing.
    Renewed,
    /// Text changed.
    Updated,
    /// The stream ended.
    Finished,
    /// An older frame arrived after a newer one. Dropped.
    Stale,
}

impl StateChannel {
    /// Feed a frame. `now_ms` is the receiver's clock, used only for staleness.
    pub fn apply(&mut self, frame: &StateFrame, now_ms: i64) -> Applied {
        if let Some(current) = self.interval {
            if frame.interval < current {
                return Applied::Stale;
            }
            if frame.interval > current {
                return self.begin(frame, now_ms);
            }
            // Same interval.
            self.last_at = now_ms;
            if frame.is_heartbeat() {
                return Applied::Renewed;
            }
            if frame.flags.has(Flags::SNAPSHOT) {
                self.text = frame.text.clone();
            } else if frame.flags.has(Flags::APPEND) {
                self.text.push_str(&frame.text);
            }
            if frame.flags.has(Flags::FINAL) {
                self.finished = true;
                return Applied::Finished;
            }
            return if frame.flags.has(Flags::APPEND) || frame.flags.has(Flags::SNAPSHOT) {
                Applied::Updated
            } else {
                Applied::Renewed
            };
        }
        self.begin(frame, now_ms)
    }

    fn begin(&mut self, frame: &StateFrame, now_ms: i64) -> Applied {
        self.interval = Some(frame.interval);
        self.state = Some(frame.state);
        self.text = if frame.flags.has(Flags::SNAPSHOT) || frame.flags.has(Flags::APPEND) {
            frame.text.clone()
        } else {
            String::new()
        };
        self.finished = frame.flags.has(Flags::FINAL);
        self.last_at = now_ms;
        Applied::Began
    }

    /// The state being shown.
    pub fn state(&self) -> Option<SessionState> {
        self.state
    }

    /// The body text accumulated in this interval.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The interval currently displayed.
    pub fn interval(&self) -> Option<u32> {
        self.interval
    }

    /// Whether the host has gone quiet on an interval that promised heartbeats.
    ///
    /// Terminal states never go stale: "failed" stays true after the host
    /// disconnects, because the failure already happened.
    pub fn is_stale(&self, now_ms: i64) -> bool {
        let Some(state) = self.state else {
            return false;
        };
        if !state.needs_heartbeat() {
            return false;
        }
        now_ms.saturating_sub(self.last_at) > STALE_AFTER_MS as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heartbeat_is_thirteen_bytes() {
        let bytes = StateFrame::heartbeat(SessionState::Thinking, 7).encode();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(bytes[0], MAGIC);
        assert_eq!(bytes[1], SessionState::Thinking.code());
    }

    #[test]
    fn every_frame_round_trips() {
        let frames = [
            StateFrame::begin(SessionState::Idle, 1, ""),
            StateFrame::heartbeat(SessionState::AwaitingUser, 2),
            StateFrame::begin(SessionState::StreamFinal, 3, "先说结论："),
            StateFrame::append(SessionState::StreamFinal, 3, 15, "改好了。"),
            StateFrame::snapshot(SessionState::StreamNarration, 4, 0, "从头再说一遍"),
            StateFrame::finish(SessionState::StreamFinal, 3, 20),
            StateFrame::begin(SessionState::Failed, 5, ""),
        ];
        for frame in frames {
            let bytes = frame.encode();
            assert_eq!(StateFrame::decode(&bytes).unwrap(), frame);
        }
    }

    #[test]
    fn a_heartbeat_cannot_smuggle_text() {
        let mut bytes = StateFrame::heartbeat(SessionState::Thinking, 1).encode();
        bytes[2] |= Flags::APPEND;
        assert!(matches!(
            StateFrame::decode(&bytes),
            Err(FrameError::ContradictoryFlags)
        ));
    }

    #[test]
    fn text_on_a_stateless_state_is_refused() {
        let frame = StateFrame::append(SessionState::Thinking, 1, 0, "偷偷带字");
        assert_eq!(
            StateFrame::decode(&frame.encode()),
            Err(FrameError::TextNotAllowed(SessionState::Thinking.code()))
        );
    }

    #[test]
    fn a_short_frame_is_not_silently_accepted() {
        assert_eq!(StateFrame::decode(&[MAGIC; 4]), Err(FrameError::Truncated));
        let mut bytes = StateFrame::heartbeat(SessionState::Idle, 1).encode();
        bytes.push(0);
        assert_eq!(StateFrame::decode(&bytes), Err(FrameError::LengthMismatch));
    }

    #[test]
    fn an_unknown_code_is_refused_rather_than_guessed() {
        let mut bytes = StateFrame::heartbeat(SessionState::Idle, 1).encode();
        bytes[1] = 200;
        assert_eq!(StateFrame::decode(&bytes), Err(FrameError::UnknownState(200)));
    }

    #[test]
    fn the_receiver_follows_intervals_and_appends() {
        let mut ch = StateChannel::default();
        assert_eq!(
            ch.apply(&StateFrame::begin(SessionState::StreamFinal, 1, "第一段"), 0),
            Applied::Began
        );
        assert_eq!(
            ch.apply(&StateFrame::append(SessionState::StreamFinal, 1, 9, "第二段"), 100),
            Applied::Updated
        );
        assert_eq!(ch.text(), "第一段第二段");
        assert_eq!(ch.state(), Some(SessionState::StreamFinal));

        // A late frame from an older interval changes nothing.
        assert_eq!(
            ch.apply(&StateFrame::begin(SessionState::Idle, 0, ""), 200),
            Applied::Stale
        );
        assert_eq!(ch.state(), Some(SessionState::StreamFinal));
    }

    #[test]
    fn a_new_interval_replaces_state_and_text() {
        let mut ch = StateChannel::default();
        ch.apply(&StateFrame::begin(SessionState::StreamFinal, 1, "旧"), 0);
        assert_eq!(
            ch.apply(&StateFrame::begin(SessionState::Thinking, 2, ""), 10),
            Applied::Began
        );
        assert_eq!(ch.state(), Some(SessionState::Thinking));
        assert_eq!(ch.text(), "");
    }

    #[test]
    fn silence_ages_out_a_held_state_but_not_a_terminal_one() {
        let mut ch = StateChannel::default();
        ch.apply(&StateFrame::begin(SessionState::Tool, 1, ""), 0);
        assert!(!ch.is_stale(STALE_AFTER_MS as i64));
        assert!(ch.is_stale(STALE_AFTER_MS as i64 + 1));

        let mut done = StateChannel::default();
        done.apply(&StateFrame::begin(SessionState::Completed, 2, ""), 0);
        assert!(!done.is_stale(600_000));
    }

    #[test]
    fn a_snapshot_repairs_a_missed_chunk() {
        let mut ch = StateChannel::default();
        ch.apply(&StateFrame::begin(SessionState::StreamFinal, 1, "开头"), 0);
        ch.apply(&StateFrame::snapshot(SessionState::StreamFinal, 1, 0, "开头，补全了"), 5);
        assert_eq!(ch.text(), "开头，补全了");
    }
}
