//! One punched connection: a reliable, ordered message stream over datagrams.
//!
//! UDP gives neither order nor delivery, and a chat with an agent needs both. So
//! this is the small amount of machinery that turns a punched path into
//! something a reader can use:
//!
//! - **Every message is chunked** to fit a datagram, and every chunk carries the
//!   whole message's length *inside* the tag, so a receiver knows when it has the
//!   lot without a second header.
//! - **Packet numbers are sequence numbers and nonces at once.** The receiver
//!   delivers in order, buffering up to [`REORDER_CAPACITY`] out-of-order chunks;
//!   anything already delivered is dropped rather than delivered twice.
//! - **Acknowledgements are cumulative in effect, explicit on the wire**: the last
//!   [`MAX_ACKS_PER_PACKET`] received numbers ride along on every outgoing data
//!   packet, and a standalone ack goes out on the next tick if one is owed.
//! - **Lost chunks are resent** on a retransmit timeout that starts at
//!   [`RTO_FLOOR`], doubles per attempt and caps at [`RTO_CEILING`]; after
//!   [`MAX_ATTEMPTS`] the session calls itself stalled instead of pretending.
//! - **Silence is a state, not a guess**: [`IDLE_TIMEOUT`] of hearing nothing
//!   marks the path degraded, and a keepalive every [`KEEPALIVE_INTERVAL`] gives
//!   a NAT something to renew.
//!
//! ### Why this is a state machine and not a task
//!
//! Every method takes the current instant. A test can therefore advance a clock
//! by four hundred milliseconds and assert that a packet comes back, with no
//! timers, no runtime and no sockets -- and the async driver that does own the
//! socket stays a thin loop over [`Session::tick`] and [`Session::on_packet`].
//! The numbers below are the phone's: they are part of how the two
//! implementations behave towards each other, so they are copied, not chosen.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use super::crypto::{self, SessionKeys};
use super::wire::{Packet, PacketBody, PacketType};

/// How long a session may hear nothing before it calls itself degraded.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(66);

/// How many attempts a chunk gets before the session calls itself stalled.
pub const MAX_ATTEMPTS: u8 = 8;

/// How many unacked chunks a session carries before it calls itself stalled.
pub const UNACKED_CAPACITY: usize = 512;

/// How many out-of-order chunks the receiver buffers.
pub const REORDER_CAPACITY: usize = 64;

/// How many received packet numbers the ack window remembers.
pub const ACK_WINDOW: usize = 64;

/// How many acknowledgements one packet carries.
pub const MAX_ACKS_PER_PACKET: usize = 8;

/// The largest plaintext chunk one datagram carries.
pub const CHUNK_BYTES: usize = 1024;

/// How often an idle path is kept alive.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(22);

/// How often the driver should call [`Session::tick`].
pub const TICK: Duration = Duration::from_millis(25);

/// The first retransmit timeout.
pub const RTO_FLOOR: Duration = Duration::from_millis(300);

/// The longest retransmit timeout.
pub const RTO_CEILING: Duration = Duration::from_millis(2000);

/// Which end of the connection this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The side that dialled and sent the ClientHello.
    Dialer,
    /// The side that was dialled and answered.
    Acceptor,
}

/// Something that happened to the session while it was reading a packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// One whole message, in order.
    Message(Vec<u8>),
    /// The peer closed the connection, with what it said.
    Closed(String),
}

impl Event {
    /// The message, when this is one.
    pub fn message(self) -> Option<Vec<u8>> {
        match self {
            Self::Message(data) => Some(data),
            Self::Closed(_) => None,
        }
    }
}

/// A chunk waiting for its acknowledgement.
struct Unacked {
    packet: Packet,
    sent_at: Instant,
    attempts: u8,
}

/// One punched connection.
pub struct Session {
    role: Role,
    connection_id: u64,
    keys: SessionKeys,

    /// The number the next outgoing packet gets.
    next_number: u32,
    /// Chunks sent and not yet acknowledged.
    unacked: BTreeMap<u32, Unacked>,

    /// The next packet number this side expects to deliver.
    expected: u32,
    /// Chunks that arrived before their turn.
    reorder: BTreeMap<u32, (u32, Vec<u8>)>,
    /// The numbers recently received, oldest first.
    recent: VecDeque<u32>,
    seen: HashSet<u32>,
    /// Whether an acknowledgement is owed.
    ack_dirty: bool,

    /// Bytes of the message being reassembled.
    parts: Vec<u8>,
    /// How long that message is, once a chunk told us.
    part_length: Option<u32>,

    /// When the last datagram arrived.
    last_received: Instant,
    /// When the last keepalive went out.
    last_keepalive: Instant,
    /// Whether this session is over.
    closed: bool,
    /// Whether this session has stopped making progress.
    stalled: bool,
    /// Whether it has heard nothing for longer than [`IDLE_TIMEOUT`].
    idle: bool,
    /// Smoothed round-trip time, in seconds. Zero until the first ack.
    smoothed_rtt: f64,
}

impl Session {
    /// Build a session from the handshake's two endpoints and the shared secret.
    ///
    /// - Parameters:
    ///   - role: which end this is.
    ///   - connection_id: the id both hello packets agreed on.
    ///   - local_endpoint_id: this endpoint's base64url id.
    ///   - remote_endpoint_id: the peer's.
    ///   - shared_secret: the X25519 agreement.
    ///   - now: the instant the session starts.
    /// - Returns: the session.
    pub fn new(
        role: Role,
        connection_id: u64,
        local_endpoint_id: &str,
        remote_endpoint_id: &str,
        shared_secret: &[u8; 32],
        now: Instant,
    ) -> Self {
        let salt = crypto::session_salt(local_endpoint_id, remote_endpoint_id);
        Self {
            role,
            connection_id,
            keys: crypto::session_keys(shared_secret, &salt),
            next_number: 0,
            unacked: BTreeMap::new(),
            expected: 0,
            reorder: BTreeMap::new(),
            recent: VecDeque::new(),
            seen: HashSet::new(),
            ack_dirty: false,
            parts: Vec::new(),
            part_length: None,
            last_received: now,
            last_keepalive: now,
            closed: false,
            stalled: false,
            idle: false,
            smoothed_rtt: 0.0,
        }
    }

    /// Whether this session is over.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Whether it has stopped making progress.
    pub fn is_stalled(&self) -> bool {
        self.stalled
    }

    /// Whether it has heard nothing for too long.
    pub fn is_idle(&self) -> bool {
        self.idle
    }

    /// The smoothed round trip, in milliseconds, or `nil` before the first ack.
    pub fn rtt_milliseconds(&self) -> Option<u64> {
        (self.smoothed_rtt > 0.0).then(|| (self.smoothed_rtt * 1000.0).round() as u64)
    }

    /// How many chunks are waiting for an acknowledgement.
    pub fn unacked(&self) -> usize {
        self.unacked.len()
    }

    /// Queue one message, returning the packets that carry it.
    ///
    /// - Parameters:
    ///   - message: the bytes; chunked to fit datagrams.
    ///   - now: the instant.
    /// - Returns: the packets to send, in order.
    pub fn send(&mut self, message: &[u8], now: Instant) -> Vec<Packet> {
        if self.closed {
            return Vec::new();
        }
        if message.is_empty() {
            return vec![self.emit_chunk(0, &[], now)];
        }
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < message.len() {
            let end = (offset + CHUNK_BYTES).min(message.len());
            out.push(self.emit_chunk(message.len() as u32, &message[offset..end], now));
            offset = end;
        }
        out
    }

    /// Read one datagram.
    ///
    /// - Parameters:
    ///   - packet: the decoded packet.
    ///   - now: the instant.
    /// - Returns: whatever the session learned — a whole message, or that the
    ///   peer is gone.
    pub fn on_packet(&mut self, packet: &Packet, now: Instant) -> Vec<Event> {
        if self.closed {
            return Vec::new();
        }
        self.last_received = now;
        self.idle = false;
        match packet.kind {
            PacketType::Data => self.handle_data(packet, now),
            PacketType::Ack => {
                if let PacketBody::Ack { acks } = &packet.body {
                    self.process_acks(acks, now);
                }
                Vec::new()
            }
            PacketType::Keepalive => Vec::new(),
            PacketType::Close => {
                let reason = match &packet.body {
                    PacketBody::Close { reason } => reason.clone(),
                    _ => String::new(),
                };
                self.closed = true;
                vec![Event::Closed(reason)]
            }
            // Handshakes belong to the engine; a session that sees one is
            // hearing a conversation it is not part of.
            PacketType::ClientHello | PacketType::ServerHello => Vec::new(),
        }
    }

    /// Run the timers: owed acks, retransmissions and keepalives.
    ///
    /// - Parameter now: the instant.
    /// - Returns: the packets to send.
    pub fn tick(&mut self, now: Instant) -> Vec<Packet> {
        if self.closed {
            return Vec::new();
        }
        if now.duration_since(self.last_received) > IDLE_TIMEOUT {
            self.idle = true;
        }
        if self.unacked.len() > UNACKED_CAPACITY {
            self.stalled = true;
        }

        let mut out = Vec::new();

        if self.ack_dirty {
            // Read the numbers *before* clearing the flag: `piggybacked_acks`
            // answers nothing once nothing is owed, and an ack packet carrying an
            // empty list is a packet that acknowledges nothing.
            let acks = self.recent_acks();
            // Cleared here rather than when the packet is written: a standalone
            // ack that is lost is the sender's retransmit to notice, and the next
            // data packet carries the numbers again.
            self.ack_dirty = false;
            out.push(Packet::ack(self.connection_id, acks));
        }

        let expired: Vec<u32> = self
            .unacked
            .iter()
            .filter(|(_, pending)| now.duration_since(pending.sent_at) > rto(pending.attempts))
            .map(|(number, _)| *number)
            .collect();
        for number in expired {
            let Some(pending) = self.unacked.get_mut(&number) else {
                continue;
            };
            if pending.attempts >= MAX_ATTEMPTS {
                self.stalled = true;
                continue;
            }
            out.push(pending.packet.clone());
            pending.sent_at = now;
            pending.attempts += 1;
        }

        if now.duration_since(self.last_keepalive) >= KEEPALIVE_INTERVAL {
            self.last_keepalive = now;
            out.push(Packet::keepalive(self.connection_id));
        }

        out
    }

    /// Close the session, with a packet to send on the way out.
    pub fn close(&mut self, reason: impl Into<String>) -> Packet {
        self.closed = true;
        Packet::close(self.connection_id, reason)
    }

    /// The key this side seals with.
    fn sealing(&self) -> &[u8; 32] {
        self.keys.sealing(self.role == Role::Dialer)
    }

    /// The key this side opens with.
    fn opening(&self) -> &[u8; 32] {
        self.keys.opening(self.role == Role::Dialer)
    }

    fn emit_chunk(&mut self, message_length: u32, chunk: &[u8], now: Instant) -> Packet {
        let number = self.next_number;
        self.next_number = self.next_number.wrapping_add(1);
        let acks = self.piggybacked_acks();
        let packet = crypto::seal_chunk(
            self.sealing(),
            self.connection_id,
            number,
            acks,
            message_length,
            chunk,
        );
        self.unacked.insert(
            number,
            Unacked {
                packet: packet.clone(),
                sent_at: now,
                attempts: 0,
            },
        );
        packet
    }

    fn handle_data(&mut self, packet: &Packet, now: Instant) -> Vec<Event> {
        let PacketBody::Data { number, acks, .. } = &packet.body else {
            return Vec::new();
        };
        let number = *number;
        let acks = acks.clone();
        self.process_acks(&acks, now);

        let Ok(chunk) = crypto::open_packet(packet, self.opening()) else {
            // A packet that will not open is not this session's: a wrong key is
            // a stranger, and a wrong tag is corruption. Neither is fatal.
            return Vec::new();
        };
        let message_length = match &packet.body {
            PacketBody::Data { message_length, .. } => *message_length,
            _ => return Vec::new(),
        };

        self.record_received(number);
        let mut events = Vec::new();
        if number == self.expected {
            self.accept_chunk(message_length, &chunk, &mut events);
            self.expected = self.expected.wrapping_add(1);
            while let Some((length, buffered)) = self.reorder.remove(&self.expected) {
                self.accept_chunk(length, &buffered, &mut events);
                self.expected = self.expected.wrapping_add(1);
            }
        } else if number > self.expected && self.reorder.len() < REORDER_CAPACITY {
            self.reorder.insert(number, (message_length, chunk));
        }
        // Below the expected number: delivered already, and the sender has not
        // seen our ack yet. It is recorded above, so the ack goes out again.
        events
    }

    /// Add one chunk to the message being reassembled, delivering it when whole.
    fn accept_chunk(&mut self, message_length: u32, chunk: &[u8], events: &mut Vec<Event>) {
        if self.part_length.is_none() {
            self.part_length = Some(message_length);
        }
        self.parts.extend_from_slice(chunk);
        let Some(total) = self.part_length else {
            return;
        };
        if self.parts.len() < total as usize {
            return;
        }
        let whole: Vec<u8> = self.parts.drain(..total as usize).collect();
        events.push(Event::Message(whole));
        // Anything left is the beginning of the next message, whose length this
        // packet did not tell us; the next chunk does.
        self.part_length = None;
    }

    fn record_received(&mut self, number: u32) {
        if self.seen.insert(number) {
            self.recent.push_back(number);
            if self.recent.len() > ACK_WINDOW {
                if let Some(dropped) = self.recent.pop_front() {
                    self.seen.remove(&dropped);
                }
            }
        }
        self.ack_dirty = true;
    }

    /// The acknowledgements to ride along on an outgoing packet.
    ///
    /// Deliberately does not clear the dirty flag: if the packet carrying them is
    /// lost, the standalone ack is still owed, and a sender that never hears them
    /// retransmits until the session stalls.
    fn piggybacked_acks(&self) -> Vec<u32> {
        if !self.ack_dirty {
            return Vec::new();
        }
        self.recent_acks()
    }

    /// The last few received numbers, whether or not an ack is owed.
    fn recent_acks(&self) -> Vec<u32> {
        let skip = self.recent.len().saturating_sub(MAX_ACKS_PER_PACKET);
        self.recent.iter().skip(skip).copied().collect()
    }

    fn process_acks(&mut self, acks: &[u32], now: Instant) {
        for number in acks {
            let Some(pending) = self.unacked.remove(number) else {
                continue;
            };
            let sample = now.duration_since(pending.sent_at).as_secs_f64();
            self.smoothed_rtt = if self.smoothed_rtt == 0.0 {
                sample
            } else {
                0.875 * self.smoothed_rtt + 0.125 * sample
            };
        }
    }
}

/// The retransmit timeout for the nth attempt: 300 ms, doubling, capped at 2 s.
///
/// The floor is above a Wi-Fi round trip, so a single loss never causes a
/// retransmit; the ceiling keeps a bad path from waiting longer than a person's
/// patience.
fn rto(attempts: u8) -> Duration {
    if attempts == 0 {
        return RTO_FLOOR;
    }
    let factor = 1u32 << u32::from(attempts.min(4));
    (RTO_FLOOR * factor).min(RTO_CEILING)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIALER: &str = "dialer-endpoint-id";
    const ACCEPTOR: &str = "acceptor-endpoint-id";

    fn pair(now: Instant) -> (Session, Session) {
        let shared = [7u8; 32];
        (
            Session::new(Role::Dialer, 1, DIALER, ACCEPTOR, &shared, now),
            Session::new(Role::Acceptor, 1, ACCEPTOR, DIALER, &shared, now),
        )
    }

    fn messages(events: Vec<Event>) -> Vec<Vec<u8>> {
        events.into_iter().filter_map(Event::message).collect()
    }

    #[test]
    fn a_message_arrives_whole() {
        let now = Instant::now();
        let (mut dialer, mut acceptor) = pair(now);
        let packets = dialer.send(b"hello mizar", now);
        assert_eq!(packets.len(), 1);

        let events = acceptor.on_packet(&packets[0], now);
        assert_eq!(messages(events), vec![b"hello mizar".to_vec()]);
    }

    #[test]
    fn a_message_larger_than_a_datagram_is_chunked_and_reassembled() {
        let now = Instant::now();
        let (mut dialer, mut acceptor) = pair(now);
        let message = vec![0x5A; CHUNK_BYTES * 2 + 100];
        let packets = dialer.send(&message, now);
        assert_eq!(packets.len(), 3);

        let mut delivered = Vec::new();
        for packet in &packets {
            delivered.extend(messages(acceptor.on_packet(packet, now)));
        }
        assert_eq!(delivered, vec![message], "one message, not three chunks");
    }

    #[test]
    fn chunks_that_arrive_out_of_order_are_delivered_in_order() {
        let now = Instant::now();
        let (mut dialer, mut acceptor) = pair(now);
        let message = vec![1u8; CHUNK_BYTES * 2 + 1];
        let packets = dialer.send(&message, now);

        // The middle packet first: it waits, and the message is only delivered
        // once the one before it arrives.
        assert!(acceptor.on_packet(&packets[1], now).is_empty());
        assert!(acceptor.on_packet(&packets[2], now).is_empty());
        let delivered = messages(acceptor.on_packet(&packets[0], now));
        assert_eq!(delivered, vec![message]);
    }

    #[test]
    fn a_lost_chunk_is_retransmitted_after_the_floor() {
        let now = Instant::now();
        let (mut dialer, mut acceptor) = pair(now);
        let packets = dialer.send(b"lost then found", now);

        // Nothing came back, so the chunk is still owed.
        assert!(dialer.tick(now + Duration::from_millis(100)).is_empty());
        let retry = dialer.tick(now + Duration::from_millis(400));
        assert_eq!(retry.len(), 1, "the chunk comes back after the rto");
        assert_eq!(retry[0].encode(), packets[0].encode());

        assert_eq!(
            messages(acceptor.on_packet(&retry[0], now)),
            vec![b"lost then found".to_vec()]
        );
    }

    #[test]
    fn a_session_stalls_rather_than_retrying_for_ever() {
        let now = Instant::now();
        let (mut dialer, _acceptor) = pair(now);
        dialer.send(b"nobody is listening", now);

        let mut at = now;
        for _ in 0..MAX_ATTEMPTS + 2 {
            at += Duration::from_secs(3);
            dialer.tick(at);
        }
        assert!(dialer.is_stalled(), "eight attempts, then the session says so");
    }

    #[test]
    fn an_acknowledgement_retires_the_chunk() {
        let now = Instant::now();
        let (mut dialer, mut acceptor) = pair(now);
        let packets = dialer.send(b"ack me", now);
        assert_eq!(dialer.unacked(), 1);

        acceptor.on_packet(&packets[0], now);
        // The receiver owes an ack; the next tick sends it.
        let acks = acceptor.tick(now + Duration::from_millis(25));
        assert_eq!(acks.len(), 1, "one standalone ack");
        dialer.on_packet(&acks[0], now + Duration::from_millis(30));

        assert_eq!(dialer.unacked(), 0);
        assert!(dialer.tick(now + Duration::from_secs(1)).is_empty(), "nothing left to resend");
        assert_eq!(dialer.rtt_milliseconds(), Some(30));
    }

    #[test]
    fn a_keepalive_goes_out_on_the_interval_and_not_before() {
        let now = Instant::now();
        let (mut dialer, _) = pair(now);
        assert!(dialer.tick(now + Duration::from_secs(21)).is_empty());
        let keepalives = dialer.tick(now + KEEPALIVE_INTERVAL);
        assert_eq!(keepalives.len(), 1);
        assert_eq!(keepalives[0].packet_type(), PacketType::Keepalive);
    }

    #[test]
    fn a_session_that_hears_nothing_calls_itself_idle() {
        let now = Instant::now();
        let (mut dialer, _) = pair(now);
        dialer.tick(now + IDLE_TIMEOUT + Duration::from_secs(1));
        assert!(dialer.is_idle());
    }

    #[test]
    fn the_peer_closing_ends_the_session_with_its_words() {
        let now = Instant::now();
        let (mut dialer, mut acceptor) = pair(now);
        let close = acceptor.close("the operator closed it");
        let events = dialer.on_packet(&close, now);
        assert_eq!(
            events,
            vec![Event::Closed("the operator closed it".to_string())]
        );
        assert!(dialer.is_closed());
        assert!(dialer.send(b"too late", now).is_empty());
    }

    #[test]
    fn a_packet_sealed_for_another_session_is_ignored() {
        let now = Instant::now();
        let (mut dialer, _) = pair(now);
        let (mut other, mut acceptor) = (
            Session::new(Role::Dialer, 2, DIALER, ACCEPTOR, &[9u8; 32], now),
            Session::new(Role::Acceptor, 1, ACCEPTOR, DIALER, &[7u8; 32], now),
        );
        let packets = other.send(b"not for you", now);
        assert!(acceptor.on_packet(&packets[0], now).is_empty());
        // The real dialer's packets still work.
        let packets = dialer.send(b"for you", now);
        assert_eq!(
            messages(acceptor.on_packet(&packets[0], now)),
            vec![b"for you".to_vec()]
        );
    }

    #[test]
    fn an_empty_message_is_a_message() {
        let now = Instant::now();
        let (mut dialer, mut acceptor) = pair(now);
        let packets = dialer.send(b"", now);
        assert_eq!(packets.len(), 1);
        assert_eq!(
            messages(acceptor.on_packet(&packets[0], now)),
            vec![Vec::<u8>::new()]
        );
    }

    #[test]
    fn the_retransmit_timeout_doubles_and_caps() {
        assert_eq!(rto(0), RTO_FLOOR);
        assert_eq!(rto(1), Duration::from_millis(600));
        assert_eq!(rto(2), Duration::from_millis(1200));
        assert_eq!(rto(3), RTO_CEILING);
        assert_eq!(rto(8), RTO_CEILING);
    }
}
