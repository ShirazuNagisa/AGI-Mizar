//! The acceptor: a UDP socket that turns a punch into a stream.
//!
//! This computer is the side being dialled, and it is also the side that has to
//! send first when the phone is behind a restricted NAT. What it does, in order:
//!
//! 1. **Binds one socket** whose address is published to a phone as a candidate.
//! 2. **Verifies every ClientHello** against the endpoint id it carries. That
//!    signature is the whole admission check: it proves the sender holds the
//!    private half of the name it claims, which is the trust the pairing already
//!    established. There is no allow-list — the session key is bound to *these
//!    two* endpoint ids, and Mizar's own device-token check runs on top of it.
//! 3. **Answers at the source address** with a signed ServerHello. Replying to
//!    the datagram's source rather than to a configured address is what punches:
//!    the reply leaves from the port the phone just opened a mapping for.
//! 4. **Runs a [`Session`]** per accepted connection and hands it to the server
//!    as a [`Stream`].
//! 5. **Punches back** when signaling names a phone: a ClientHello from this
//!    socket opens the local NAT for the phone's own hello, which a
//!    port-restricted NAT would otherwise drop.
//!
//! ### Why a repeated hello is answered again
//!
//! The dialler sends the same hello every 120 ms until it hears back, and any one
//! of those datagrams can be the one the path drops. A second hello for a
//! connection that exists is therefore not an error: it is the dialler still
//! waiting, and the only useful answer is the same ServerHello again — with the
//! *same* ephemeral key, because the dialler derives its session keys from what
//! that hello carries.
//!
//! ### The duplicate-session rule
//!
//! A simultaneous open leaves two sessions, one from each side's hello. Both
//! sides wait the same grace and keep the session with the smaller connection
//! id, so the two ends finish on the same one without another handshake.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::crypto;
use super::identity::{verify_signature, Identity};
use super::session::{Event, Role, Session, TICK};
use super::stun;
use super::wire::{Packet, PacketBody, PacketType};

/// How many messages one stream buffers before the socket loop waits.
const STREAM_DEPTH: usize = 64;

/// How many commands the streams may have queued.
const COMMAND_DEPTH: usize = 64;

/// How many connections one engine holds at once.
///
/// An acceptor cannot know in advance which keys are phones of this operator's:
/// the Mizar handshake's device token is what admits a caller, and that runs
/// after this. So the bound is on how many sessions exist at all rather than on
/// who opened them — a stranger can cost one slot until the token check fails,
/// and cannot cost the machine its memory.
pub const MAX_SESSIONS: usize = 64;

/// How often a punch round repeats while the acceptor is opening its own NAT.
const PUNCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);

/// How long an acceptor punches back before giving the path up.
///
/// The same three seconds the phone spends: a punch either lands in the first
/// few rounds or the two networks are not going to agree.
const PUNCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// How long two sessions to the same phone are allowed to coexist before one is
/// chosen.
///
/// A simultaneous open leaves one session for the phone's hello and one for
/// ours. The phone applies the same grace and the same rule (the smaller
/// connection id wins), so both sides finish with the same session.
const YIELD_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// A phone the server has asked the engine to punch back to.
#[derive(Debug, Clone)]
pub struct PunchTarget {
    /// The phone's base64url endpoint id, as its signaling hello named it.
    pub endpoint_id: String,
    /// Where the phone says it can be reached, best first.
    pub candidates: Vec<SocketAddr>,
}

/// Where a direct path can be reached, kept current by the socket that owns it.
///
/// One value shared by the engine (which learns the port at bind time and the
/// outside address from STUN) and the server (which publishes the list to a
/// phone). It is a small mutex rather than a channel because a reader wants the
/// *current* list, not a history of it: a candidate that has been replaced is
/// not something to catch up on.
#[derive(Clone, Default)]
pub struct Reach {
    inner: Arc<std::sync::Mutex<ReachState>>,
}

#[derive(Default)]
struct ReachState {
    port: u16,
    reflexive: Option<SocketAddr>,
}

/// One address a phone may reach this computer on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// `host` or `srflx`, the names ICE uses.
    pub kind: &'static str,
    /// The address, as text.
    pub address: String,
    /// The UDP port.
    pub port: u16,
    /// Higher is tried first.
    pub priority: u32,
}

impl Candidate {
    /// The JSON a signalling channel sends.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind,
            "address": self.address,
            "port": self.port,
            "priority": self.priority,
        })
    }
}

impl Reach {
    /// Record the port the engine bound.
    pub fn set_port(&self, port: u16) {
        self.state().port = port;
    }

    /// The port the engine bound.
    pub fn port(&self) -> u16 {
        self.state().port
    }

    /// Record what a STUN server saw.
    pub fn set_reflexive(&self, address: SocketAddr) {
        self.state().reflexive = Some(address);
    }

    /// What a STUN server last saw, when it has answered.
    pub fn reflexive(&self) -> Option<SocketAddr> {
        self.state().reflexive
    }

    /// Every address this computer may be reached on, best first.
    ///
    /// Host addresses first — they are what a phone on the same network reaches
    /// with no help from anybody — then the outside address a NAT handed out.
    pub fn candidates(&self) -> Vec<Candidate> {
        let state = self.state();
        let port = state.port;
        let mut out: Vec<Candidate> = crate::store::local_addresses()
            .into_iter()
            .enumerate()
            .map(|(index, address)| Candidate {
                kind: "host",
                address: address.to_string(),
                port,
                priority: priority(126, index),
            })
            .collect();
        if let Some(reflexive) = state.reflexive {
            out.push(Candidate {
                kind: "srflx",
                address: reflexive.ip().to_string(),
                port: reflexive.port(),
                priority: priority(100, 0),
            });
        }
        out
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ReachState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// ICE's priority, compressed the way the phone's own type documents it.
fn priority(type_preference: u32, index: usize) -> u32 {
    let local = 65_535u32.saturating_sub(index as u32);
    (type_preference << 24) | (local << 8) | 255
}

/// One accepted connection, as the server uses it.
///
/// A stream carries whole messages in both directions. The socket loop owns the
/// session; this handle only has the two channels, which is what lets the server
/// treat a punched connection exactly like any other transport.
pub struct Stream {
    /// Where the peer's datagrams come from.
    pub remote: SocketAddr,
    /// The peer's base64url endpoint id.
    pub remote_endpoint_id: String,
    /// The connection id, for a log line that can be matched to the phone's.
    pub connection_id: u64,
    outgoing: mpsc::Sender<Command>,
    incoming: mpsc::Receiver<Vec<u8>>,
}

/// What the server can ask of one connection.
enum Command {
    Send(u64, Vec<u8>),
    Close(u64),
}

impl Stream {
    /// Send one message.
    ///
    /// - Parameter message: the bytes; the session chunks them.
    /// - Returns: an error when the connection is gone.
    pub async fn send(&mut self, message: Vec<u8>) -> io::Result<()> {
        self.outgoing
            .send(Command::Send(self.connection_id, message))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the direct connection is gone"))
    }

    /// Wait for the next message.
    ///
    /// - Returns: the message, or `nil` when the peer closed or the engine
    ///   dropped the session.
    pub async fn receive(&mut self) -> Option<Vec<u8>> {
        self.incoming.recv().await
    }

    /// Close the connection, telling the peer.
    pub async fn close(&mut self) {
        let _ = self.outgoing.send(Command::Close(self.connection_id)).await;
    }

    /// A description for a log line: `12.0.0.1:51234 (Q2085V4H…)`.
    pub fn describe(&self) -> String {
        format!(
            "{} ({}…)",
            self.remote,
            &self.remote_endpoint_id[..self.remote_endpoint_id.len().min(8)]
        )
    }

    /// Split the two directions so each can be owned by its own task.
    ///
    /// The socket loop owns the session and reads one channel while writing
    /// another, so a connection handler with a reader task and a writer task
    /// needs the halves separately: one `Stream` cannot be borrowed mutably by
    /// both, and a mutex around it would make a read block a write.
    pub fn split(self) -> (StreamSender, StreamReceiver) {
        (
            StreamSender {
                connection_id: self.connection_id,
                outgoing: self.outgoing,
            },
            StreamReceiver {
                incoming: self.incoming,
            },
        )
    }
}

/// The sending half of a punched connection.
pub struct StreamSender {
    connection_id: u64,
    outgoing: mpsc::Sender<Command>,
}

impl StreamSender {
    /// Send one message.
    pub async fn send(&self, message: Vec<u8>) -> io::Result<()> {
        self.outgoing
            .send(Command::Send(self.connection_id, message))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the direct connection is gone"))
    }

    /// Close the connection, telling the peer.
    pub async fn close(&self) {
        let _ = self.outgoing.send(Command::Close(self.connection_id)).await;
    }
}

/// The receiving half of a punched connection.
pub struct StreamReceiver {
    incoming: mpsc::Receiver<Vec<u8>>,
}

impl StreamReceiver {
    /// Wait for the next message; `nil` when the connection is gone.
    pub async fn receive(&mut self) -> Option<Vec<u8>> {
        self.incoming.recv().await
    }
}

/// A bound engine, and the stream of connections it accepts.
pub struct Engine {
    identity: Arc<Identity>,
    address: SocketAddr,
    incoming: mpsc::Receiver<Stream>,
    /// Where the server asks for an acceptor-side punch.
    punches: mpsc::Sender<PunchTarget>,
}

/// One live connection inside the socket loop.
struct Live {
    session: Session,
    remote: SocketAddr,
    /// The phone's endpoint id, kept for the duplicate-session rule.
    endpoint_id: String,
    incoming: mpsc::Sender<Vec<u8>>,
    /// Held here until the grace decides which session the server gets.
    receiver: Option<mpsc::Receiver<Vec<u8>>>,
    /// The ServerHello this connection was answered with, encoded.
    ///
    /// Kept so a repeated hello is answered with the *same* ephemeral key: the
    /// dialler derives its session keys from that hello, and a fresh key would
    /// give it a session the acceptor does not have. A session we dialled
    /// (the acceptor-side punch) has no ServerHello of its own.
    server_hello: Option<Vec<u8>>,
    /// Whether the server has received this session as a [`Stream`].
    yielded: bool,
}

/// One acceptor-side punch in flight.
struct Dial {
    /// The phone the server asked us to reach.
    endpoint_id: String,
    /// The X25519 secret whose public half the hello carries.
    local_secret: [u8; 32],
    /// The encoded, signed ClientHello, resent until answered.
    hello: Vec<u8>,
    /// Where to send it.
    candidates: Vec<SocketAddr>,
    /// When to stop.
    deadline: Instant,
    /// When the next round goes out.
    next_send: Instant,
}

impl Engine {
    /// Bind a UDP socket and start accepting.
    ///
    /// The port is chosen by the operating system unless one is named: nothing
    /// dials this address until the candidate list has been handed to a phone, so
    /// a fixed port would be a collision waiting to happen with whatever else is
    /// on the machine.
    ///
    /// - Parameters:
    ///   - identity: this endpoint's key.
    ///   - port: the local port to bind, or `0` for one the OS chooses.
    /// - Returns: the engine, and the address it bound.
    pub async fn bind(
        identity: Identity,
        port: u16,
        reach: Reach,
        stun_servers: Vec<String>,
    ) -> io::Result<(Self, SocketAddr)> {
        let socket = UdpSocket::bind(("0.0.0.0", port)).await?;
        // `0.0.0.0` is what the socket listens on, not where anything can be
        // dialled: a caller (and a test) needs an address that routes. The
        // candidate list a phone receives is built from the interfaces, not from
        // this value.
        let bound = socket.local_addr()?;
        let address = if bound.ip().is_unspecified() {
            SocketAddr::from(([127, 0, 0, 1], bound.port()))
        } else {
            bound
        };
        let identity = Arc::new(identity);
        let (incoming_tx, incoming_rx) = mpsc::channel(STREAM_DEPTH);
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_DEPTH);
        let (punch_tx, punch_rx) = mpsc::channel(16);
        reach.set_port(address.port());
        let engine_identity = Arc::clone(&identity);
        let engine_reach = reach.clone();
        tokio::spawn(async move {
            if let Err(error) = run(
                socket,
                engine_identity,
                engine_reach,
                stun_servers,
                commands_rx,
                commands_tx,
                incoming_tx,
                punch_rx,
            )
            .await
            {
                tracing::warn!("p2p: the acceptor stopped — {error}");
            }
        });
        Ok((
            Self {
                identity,
                address,
                incoming: incoming_rx,
                punches: punch_tx,
            },
            address,
        ))
    }

    /// Where the server asks for an acceptor-side punch.
    pub fn punch_sender(&self) -> mpsc::Sender<PunchTarget> {
        self.punches.clone()
    }

    /// This endpoint's id, which a phone's ticket names.
    pub fn endpoint_id(&self) -> &str {
        self.identity.endpoint_id()
    }

    /// Where the socket is bound.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Wait for the next accepted connection.
    pub async fn accept(&mut self) -> Option<Stream> {
        self.incoming.recv().await
    }
}

async fn run(
    socket: UdpSocket,
    identity: Arc<Identity>,
    reach: Reach,
    stun_servers: Vec<String>,
    mut commands: mpsc::Receiver<Command>,
    command_sender: mpsc::Sender<Command>,
    incoming: mpsc::Sender<Stream>,
    mut punches: mpsc::Receiver<PunchTarget>,
) -> io::Result<()> {
    let mut live: HashMap<u64, Live> = HashMap::new();
    let mut dialing: HashMap<u64, Dial> = HashMap::new();
    // Endpoint -> when the grace for choosing among its sessions ends.
    let mut pending: HashMap<String, Instant> = HashMap::new();
    // Endpoint -> the connection id the server was finally given.
    let mut active: HashMap<String, u64> = HashMap::new();
    let mut buffer = vec![0u8; 65_536];
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately, so the outside address is learned at
    // launch rather than fifteen seconds into the first call.
    let mut stun_ticker = tokio::time::interval(stun::PROBE_INTERVAL);
    stun_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The server that answered last. A deployment can list more than one (the
    // unified relay's own host does not answer; the main host does), and a
    // machine should not pay a timeout for the dead one on every refresh.
    let mut preferred_stun: Option<String> = None;

    loop {
        tokio::select! {
            datagram = socket.recv_from(&mut buffer) => {
                let Ok((length, from)) = datagram else { continue };
                let Ok(packet) = Packet::decode(&buffer[..length]) else { continue };
                match packet.kind {
                    PacketType::ClientHello => {
                        accept(
                            &socket,
                            &identity,
                            &mut live,
                            &mut pending,
                            &active,
                            &packet,
                            from,
                        )
                        .await;
                    }
                    // An answer to a hello this engine sent. That is the
                    // acceptor-side punch working, not a conversation for
                    // somebody else.
                    PacketType::ServerHello => {
                        answer_server_hello(
                            &socket,
                            &identity,
                            &command_sender,
                            &mut live,
                            &mut dialing,
                            &mut pending,
                            &active,
                            &packet,
                            from,
                        )
                        .await;
                    }
                    _ => {
                        let Some(entry) = live.get_mut(&packet.connection_id) else { continue };
                        for event in entry.session.on_packet(&packet, Instant::now()) {
                            match event {
                                Event::Message(message) => {
                                    // A server that stops reading is a server with
                                    // a full queue; dropping is what the session's
                                    // reliability is for, not blocking the socket.
                                    let _ = entry.incoming.try_send(message);
                                }
                                Event::Closed(reason) => {
                                    tracing::debug!("p2p: the peer closed the connection — {reason}");
                                }
                            }
                        }
                    }
                }
            }
            target = punches.recv() => {
                let Some(target) = target else { return Ok(()) };
                start_punch(&socket, &identity, &mut dialing, &live, &active, target).await;
            }
            command = commands.recv() => {
                match command {
                    Some(Command::Send(connection_id, message)) => {
                        let Some(entry) = live.get_mut(&connection_id) else { continue };
                        for packet in entry.session.send(&message, Instant::now()) {
                            let _ = socket.send_to(&packet.encode(), entry.remote).await;
                        }
                    }
                    Some(Command::Close(connection_id)) => {
                        let Some(mut entry) = live.remove(&connection_id) else { continue };
                        if active.get(&entry.endpoint_id) == Some(&connection_id) {
                            active.remove(&entry.endpoint_id);
                        }
                        pending.remove(&entry.endpoint_id);
                        let packet = entry.session.close("closed by the server");
                        let _ = socket.send_to(&packet.encode(), entry.remote).await;
                    }
                    None => return Ok(()),
                }
            }
            _ = stun_ticker.tick() => {
                // Asking from this socket is the point: the mapping this opens
                // is the one a phone can punch into, and re-asking every fifteen
                // seconds is what keeps it open. More than one server is normal
                // (the unified relay's host answers no STUN), so the one that
                // answered last is asked first.
                let mut order: Vec<String> = Vec::with_capacity(stun_servers.len());
                if let Some(preferred) = preferred_stun
                    .as_ref()
                    .filter(|preferred| stun_servers.contains(preferred))
                {
                    order.push(preferred.clone());
                }
                order.extend(
                    stun_servers
                        .iter()
                        .filter(|server| Some(*server) != preferred_stun.as_ref())
                        .cloned(),
                );
                let mut answered = false;
                for server in order {
                    match stun::probe(&socket, &server).await {
                        Ok(seen) => {
                            if preferred_stun.as_ref() != Some(&server) {
                                tracing::info!("p2p: STUN learned {seen} from {server}");
                                preferred_stun = Some(server);
                            } else {
                                tracing::debug!("p2p: STUN refreshed {seen} from {server}");
                            }
                            reach.set_reflexive(seen);
                            answered = true;
                            break;
                        }
                        Err(error) => {
                            tracing::debug!("p2p: STUN {server} did not answer — {error}");
                        }
                    }
                }
                if !answered {
                    tracing::debug!("p2p: no STUN server answered");
                }
            }
            _ = ticker.tick() => {
                let now = Instant::now();
                // The acceptor's own punch: the outbound datagrams are what
                // open a restricted NAT for the phone's answer.
                let mut expired = Vec::new();
                for (connection_id, dial) in dialing.iter_mut() {
                    if now >= dial.deadline {
                        expired.push(*connection_id);
                        continue;
                    }
                    if now >= dial.next_send {
                        for address in &dial.candidates {
                            let _ = socket.send_to(&dial.hello, address).await;
                        }
                        dial.next_send = now + PUNCH_INTERVAL;
                    }
                }
                for connection_id in expired {
                    if let Some(dial) = dialing.remove(&connection_id) {
                        tracing::info!(
                            "p2p: punching back to {}… timed out",
                            &dial.endpoint_id[..dial.endpoint_id.len().min(8)]
                        );
                    }
                }

                resolve_pending(
                    &socket,
                    &incoming,
                    &command_sender,
                    &mut live,
                    &mut pending,
                    &mut active,
                    now,
                )
                .await;

                let mut stalled = Vec::new();
                for (connection_id, entry) in live.iter_mut() {
                    for packet in entry.session.tick(now) {
                        let _ = socket.send_to(&packet.encode(), entry.remote).await;
                    }
                    if entry.session.is_stalled() {
                        stalled.push(*connection_id);
                    }
                }
                for connection_id in stalled {
                    if let Some(entry) = live.remove(&connection_id) {
                        tracing::debug!("p2p: a direct connection stalled and was dropped");
                        if active.get(&entry.endpoint_id) == Some(&connection_id) {
                            active.remove(&entry.endpoint_id);
                        }
                        pending.remove(&entry.endpoint_id);
                    }
                }
            }
        }
    }
}

/// Admit one ClientHello, answer the dialler, and queue the session for the
/// duplicate-session grace.
///
/// The server is deliberately not handed a stream here. A simultaneous open
/// (the phone dialled us while we were dialling it) leaves two sessions, and
/// which one is usable is not known until both have had a chance to arrive.
/// ``resolve_pending`` makes that choice once, on the ticker.
#[allow(clippy::too_many_arguments)]
async fn accept(
    socket: &UdpSocket,
    identity: &Identity,
    live: &mut HashMap<u64, Live>,
    pending: &mut HashMap<String, Instant>,
    active: &HashMap<String, u64>,
    packet: &Packet,
    from: SocketAddr,
) {
    let PacketBody::Hello {
        ephemeral: remote_ephemeral,
        endpoint_id: remote_endpoint_id,
        signature,
    } = &packet.body
    else {
        return;
    };
    if !verify_signature(signature, &packet.handshake_message(), remote_endpoint_id) {
        tracing::debug!("p2p: a hello from {from} did not verify and was ignored");
        return;
    }

    if let Some(entry) = live.get(&packet.connection_id) {
        // The dialler is still punching: answer with what it is waiting for.
        if let Some(server_hello) = entry.server_hello.as_ref() {
            let _ = socket.send_to(server_hello, from).await;
        }
        return;
    }
    if active.contains_key(remote_endpoint_id) {
        // A path to this phone is already up. This hello is the losing half of
        // an old race; close it rather than handing the server a second socket.
        let close = Packet::close(packet.connection_id, "another direct path is already up");
        let _ = socket.send_to(&close.encode(), from).await;
        return;
    }
    if live.len() >= MAX_SESSIONS {
        tracing::debug!("p2p: refused a connection — {MAX_SESSIONS} already open");
        let close = Packet::close(packet.connection_id, "too many direct connections");
        let _ = socket.send_to(&close.encode(), from).await;
        return;
    }

    let (local_secret, local_public) = crypto::ephemeral_keypair();
    let mut hello = Packet::hello(
        PacketType::ServerHello,
        packet.connection_id,
        local_public,
        identity.endpoint_id(),
        [0u8; 64],
    );
    let signed = identity.sign(&hello.handshake_message());
    hello = Packet::hello(
        PacketType::ServerHello,
        packet.connection_id,
        local_public,
        identity.endpoint_id(),
        signed,
    );
    let server_hello = hello.encode();

    let shared = crypto::agree(&local_secret, remote_ephemeral);
    let session = Session::new(
        Role::Acceptor,
        packet.connection_id,
        identity.endpoint_id(),
        remote_endpoint_id,
        &shared,
        Instant::now(),
    );

    let (incoming_tx, incoming_rx) = mpsc::channel(64);
    let _ = socket.send_to(&server_hello, from).await;
    tracing::debug!(
        "p2p: accepted {} from {from}",
        &remote_endpoint_id[..remote_endpoint_id.len().min(8)]
    );

    live.insert(
        packet.connection_id,
        Live {
            session,
            remote: from,
            endpoint_id: remote_endpoint_id.clone(),
            incoming: incoming_tx,
            receiver: Some(incoming_rx),
            server_hello: Some(server_hello),
            yielded: false,
        },
    );
    pending
        .entry(remote_endpoint_id.clone())
        .or_insert_with(|| Instant::now() + YIELD_GRACE);
}

/// The phone answered a ClientHello this engine sent.
///
/// This is the half the acceptor-only design was missing: a phone behind a
/// restricted NAT can only be reached after this side has sent to the address
/// the phone's signaling hello named. The phone's answer proves the mapping is
/// open, and the session it starts is offered to the server like any other.
#[allow(clippy::too_many_arguments)]
async fn answer_server_hello(
    socket: &UdpSocket,
    identity: &Identity,
    _commands: &mpsc::Sender<Command>,
    live: &mut HashMap<u64, Live>,
    dialing: &mut HashMap<u64, Dial>,
    pending: &mut HashMap<String, Instant>,
    active: &HashMap<String, u64>,
    packet: &Packet,
    from: SocketAddr,
) {
    let Some(dial) = dialing.get(&packet.connection_id) else {
        return;
    };
    let expected_endpoint = dial.endpoint_id.clone();
    let local_secret = dial.local_secret;
    let PacketBody::Hello {
        ephemeral: remote_ephemeral,
        endpoint_id: remote_endpoint_id,
        signature,
    } = &packet.body
    else {
        return;
    };
    if *remote_endpoint_id != expected_endpoint {
        tracing::debug!("p2p: a ServerHello from {from} named a different phone");
        return;
    }
    if !verify_signature(signature, &packet.handshake_message(), remote_endpoint_id) {
        tracing::debug!("p2p: a ServerHello from {from} did not verify");
        return;
    }
    if active.contains_key(remote_endpoint_id) {
        let close = Packet::close(packet.connection_id, "another direct path is already up");
        let _ = socket.send_to(&close.encode(), from).await;
        dialing.remove(&packet.connection_id);
        return;
    }

    let shared = crypto::agree(&local_secret, remote_ephemeral);
    let session = Session::new(
        Role::Dialer,
        packet.connection_id,
        identity.endpoint_id(),
        remote_endpoint_id,
        &shared,
        Instant::now(),
    );
    let (incoming_tx, incoming_rx) = mpsc::channel(64);
    dialing.remove(&packet.connection_id);
    live.insert(
        packet.connection_id,
        Live {
            session,
            remote: from,
            endpoint_id: remote_endpoint_id.clone(),
            incoming: incoming_tx,
            receiver: Some(incoming_rx),
            server_hello: None,
            yielded: false,
        },
    );
    pending
        .entry(remote_endpoint_id.clone())
        .or_insert_with(|| Instant::now() + YIELD_GRACE);
    tracing::info!(
        "p2p: the phone answered our punch from {from} ({})",
        &remote_endpoint_id[..remote_endpoint_id.len().min(8)]
    );
}

/// Start punching back to a phone the server has just signalled.
///
/// One punch per phone: a repeated signal hello refreshes nothing while the
/// socket already exists, and a second punch to the same peer would race the
/// first one's session.
async fn start_punch(
    socket: &UdpSocket,
    identity: &Identity,
    dialing: &mut HashMap<u64, Dial>,
    live: &HashMap<u64, Live>,
    active: &HashMap<String, u64>,
    target: PunchTarget,
) {
    let endpoint = target.endpoint_id.trim();
    if endpoint.is_empty()
        || active.contains_key(endpoint)
        || live.values().any(|entry| entry.endpoint_id == endpoint)
        || dialing.values().any(|dial| dial.endpoint_id == endpoint)
    {
        return;
    }
    let mut candidates = Vec::new();
    for candidate in target.candidates {
        // Loopback is allowed here because the signaling handler is the only
        // caller in production and it filters it; a unit test punches to a
        // loopback phone, and refusing that would test nothing. Link-local is
        // not: it is the phone's own interface (AWDL or a stale 169.254), and
        // a punch to it would look like success on the bench and fail on a
        // real network.
        let link_local = match candidate.ip() {
            std::net::IpAddr::V4(v4) => v4.is_link_local(),
            std::net::IpAddr::V6(v6) => v6.is_unicast_link_local(),
        };
        if candidate.ip().is_unspecified() || link_local {
            continue;
        }
        if candidates.contains(&candidate) {
            continue;
        }
        candidates.push(candidate);
        if candidates.len() >= 8 {
            break;
        }
    }
    if candidates.is_empty() {
        tracing::debug!("p2p: the phone's signaling hello named no usable candidate");
        return;
    }

    let connection_id = loop {
        let id = crypto::connection_id();
        if id != 0 && !live.contains_key(&id) && !dialing.contains_key(&id) {
            break id;
        }
    };
    let (local_secret, local_public) = crypto::ephemeral_keypair();
    let mut hello = Packet::hello(
        PacketType::ClientHello,
        connection_id,
        local_public,
        identity.endpoint_id(),
        [0u8; 64],
    );
    let signed = identity.sign(&hello.handshake_message());
    hello = Packet::hello(
        PacketType::ClientHello,
        connection_id,
        local_public,
        identity.endpoint_id(),
        signed,
    );
    let now = Instant::now();
    let dial = Dial {
        endpoint_id: endpoint.to_string(),
        local_secret,
        hello: hello.encode(),
        candidates,
        deadline: now + PUNCH_BUDGET,
        next_send: now,
    };
    for address in &dial.candidates {
        let _ = socket.send_to(&dial.hello, address).await;
    }
    tracing::info!(
        "p2p: punching back to {}… at {} address(es): {:?}",
        &endpoint[..endpoint.len().min(8)],
        dial.candidates.len(),
        dial.candidates
    );
    dialing.insert(connection_id, dial);
}

/// Choose one session per phone after the grace and hand it to the server.
#[allow(clippy::too_many_arguments)]
async fn resolve_pending(
    socket: &UdpSocket,
    incoming: &mpsc::Sender<Stream>,
    commands: &mpsc::Sender<Command>,
    live: &mut HashMap<u64, Live>,
    pending: &mut HashMap<String, Instant>,
    active: &mut HashMap<String, u64>,
    now: Instant,
) {
    let ready: Vec<String> = pending
        .iter()
        .filter(|(_, deadline)| **deadline <= now)
        .map(|(endpoint, _)| endpoint.clone())
        .collect();
    for endpoint in ready {
        let ids: Vec<u64> = live
            .iter()
            .filter(|(_, entry)| entry.endpoint_id == endpoint && !entry.yielded)
            .map(|(id, _)| *id)
            .collect();
        if ids.is_empty() {
            pending.remove(&endpoint);
            continue;
        }
        // Both sides apply this rule to the same two connection ids, so the
        // loser is the same session on both ends.
        let keeper = *ids.iter().min().expect("non-empty");
        for id in ids.iter().filter(|id| **id != keeper) {
            if let Some(mut entry) = live.remove(id) {
                let packet = entry.session.close("a second direct path won");
                let _ = socket.send_to(&packet.encode(), entry.remote).await;
                tracing::debug!("p2p: dropped connection {id} in favour of {keeper}");
            }
        }
        if let Some(entry) = live.get_mut(&keeper) {
            if !entry.yielded {
                entry.yielded = true;
                active.insert(endpoint.clone(), keeper);
                if let Some(receiver) = entry.receiver.take() {
                    let stream = Stream {
                        remote: entry.remote,
                        remote_endpoint_id: endpoint.clone(),
                        connection_id: keeper,
                        outgoing: commands.clone(),
                        incoming: receiver,
                    };
                    if incoming.send(stream).await.is_err() {
                        live.remove(&keeper);
                        active.remove(&endpoint);
                        pending.remove(&endpoint);
                        continue;
                    }
                }
            }
        }
        pending.remove(&endpoint);
        tracing::debug!(
            "p2p: yielded connection {keeper} for {}…",
            &endpoint[..endpoint.len().min(8)]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::identity::Purpose;
    use super::super::session::Role;
    use super::super::wire::PacketType;
    use super::*;
    use std::time::Duration;

    /// A phone, simulated with the same primitives the kernel uses.
    ///
    /// Dialling is written out here rather than hidden behind a helper: the test
    /// is the contract, and a punch that only works through a convenience
    /// function would be a punch that proves nothing about the wire.
    struct Dialer {
        socket: UdpSocket,
        identity: Identity,
        connection_id: u64,
        local_secret: [u8; 32],
        session: Option<Session>,
    }

    async fn hello(engine: SocketAddr) -> Dialer {
        let socket = UdpSocket::bind(("127.0.0.1", 0))
            .await
            .expect("a phone socket");
        let identity = Identity::derive(b"the device token", Purpose::Device);
        let (local_secret, local_public) = crypto::ephemeral_keypair();
        let connection_id = 0x0102_0304_0506_0708;

        let mut packet = Packet::hello(
            PacketType::ClientHello,
            connection_id,
            local_public,
            identity.endpoint_id(),
            [0u8; 64],
        );
        let signature = identity.sign(&packet.handshake_message());
        packet = Packet::hello(
            PacketType::ClientHello,
            connection_id,
            local_public,
            identity.endpoint_id(),
            signature,
        );
        socket
            .send_to(&packet.encode(), engine)
            .await
            .expect("the hello goes out");
        Dialer {
            socket,
            identity,
            connection_id,
            local_secret,
            session: None,
        }
    }

    impl Dialer {
        /// Read the answer and build the session the phone would build.
        async fn settle(&mut self, engine_endpoint_id: &str) {
            let mut buffer = vec![0u8; 2048];
            let (length, _from) =
                tokio::time::timeout(Duration::from_secs(2), self.socket.recv_from(&mut buffer))
                    .await
                    .expect("a server hello arrives")
                    .expect("a datagram");
            let packet = Packet::decode(&buffer[..length]).expect("a decoded hello");
            assert_eq!(packet.kind, PacketType::ServerHello);
            let PacketBody::Hello {
                ephemeral,
                ref endpoint_id,
                ref signature,
            } = packet.body
            else {
                panic!("a server hello is a hello");
            };
            assert_eq!(endpoint_id, engine_endpoint_id);
            assert!(
                verify_signature(signature, &packet.handshake_message(), endpoint_id),
                "the acceptor's signature is over the bytes the dialler checks"
            );
            let shared = crypto::agree(&self.local_secret, &ephemeral);
            self.session = Some(Session::new(
                Role::Dialer,
                self.connection_id,
                self.identity.endpoint_id(),
                engine_endpoint_id,
                &shared,
                Instant::now(),
            ));
        }
    }

    #[tokio::test]
    async fn a_hello_is_answered_and_a_message_crosses_both_ways() {
        let identity = Identity::derive(b"the host secret", Purpose::Host);
        let endpoint_id = identity.endpoint_id().to_string();
        let (mut engine, address) = Engine::bind(identity, 0, Reach::default(), Vec::new())
            .await
            .expect("a socket");

        let mut dialer = hello(address).await;
        dialer.settle(&endpoint_id).await;
        let mut stream = tokio::time::timeout(Duration::from_secs(2), engine.accept())
            .await
            .expect("a stream is accepted")
            .expect("one stream");
        assert_eq!(stream.remote_endpoint_id, dialer.identity.endpoint_id());

        // Phone to computer. The dialer's session needs the engine's address to
        // send to, so re-point the datagram's destination here.
        let session = dialer.session.as_mut().expect("settled");
        let packet = session
            .send(b"hello from the phone", Instant::now())
            .remove(0);
        dialer
            .socket
            .send_to(&packet.encode(), address)
            .await
            .expect("sent");

        let received = tokio::time::timeout(Duration::from_secs(2), stream.receive())
            .await
            .expect("the message arrives")
            .expect("one message");
        assert_eq!(received, b"hello from the phone");

        // Computer to phone.
        stream
            .send(b"hello from the computer".to_vec())
            .await
            .expect("sent");
        // Computer to phone. The engine may have queued more than the data
        // datagram (an ack for the phone's message, a keepalive), and packet
        // order across a real socket is not the send order; keep feeding what
        // arrives into the session until the message shows up.
        let mut buffer = vec![0u8; 2048];
        let mut messages: Vec<Vec<u8>> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while messages.is_empty() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "no computer-to-phone message arrived");
            let (length, _) = tokio::time::timeout(remaining, dialer.socket.recv_from(&mut buffer))
                .await
                .expect("a datagram comes back")
                .expect("a datagram");
            let packet = Packet::decode(&buffer[..length]).expect("a packet");
            let events = dialer
                .session
                .as_mut()
                .expect("settled")
                .on_packet(&packet, Instant::now());
            messages.extend(events.into_iter().filter_map(Event::message));
        }
        assert_eq!(messages, vec![b"hello from the computer".to_vec()]);
    }

    #[tokio::test]
    async fn the_engine_learns_where_it_looks_from_outside() {
        // A STUN server that answers with the sender's own address, which is
        // exactly what a real one does for a machine with no NAT in front of it.
        let stun_server = UdpSocket::bind(("127.0.0.1", 0)).await.expect("a socket");
        let stun_address = stun_server.local_addr().expect("an address");
        tokio::spawn(async move {
            let mut buffer = [0u8; 64];
            while let Ok((_length, from)) = stun_server.recv_from(&mut buffer).await {
                let mut transaction = [0u8; 12];
                transaction.copy_from_slice(&buffer[8..20]);
                let SocketAddr::V4(v4) = from else { continue };
                let port = v4.port() ^ (super::super::stun::MAGIC_COOKIE >> 16) as u16;
                let address = u32::from(*v4.ip()) ^ super::super::stun::MAGIC_COOKIE;
                let mut attribute = vec![0u8, 0x01];
                attribute.extend_from_slice(&port.to_be_bytes());
                attribute.extend_from_slice(&address.to_be_bytes());
                let mut body = Vec::new();
                body.extend_from_slice(&0x0020u16.to_be_bytes());
                body.extend_from_slice(&(attribute.len() as u16).to_be_bytes());
                body.extend_from_slice(&attribute);
                let mut reply = Vec::new();
                reply.extend_from_slice(&0x0101u16.to_be_bytes());
                reply.extend_from_slice(&(body.len() as u16).to_be_bytes());
                reply.extend_from_slice(&super::super::stun::MAGIC_COOKIE.to_be_bytes());
                reply.extend_from_slice(&transaction);
                reply.extend_from_slice(&body);
                let _ = stun_server.send_to(&reply, from).await;
            }
        });

        let identity = Identity::derive(b"the host secret", Purpose::Host);
        let reach = Reach::default();
        let (_engine, address) =
            Engine::bind(identity, 0, reach.clone(), vec![stun_address.to_string()])
                .await
                .expect("a socket");

        // The first probe fires immediately; a few ticks are enough to see it.
        for _ in 0..40 {
            if reach.reflexive().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let reflexive = reach.reflexive().expect("the outside address is learned");
        assert_eq!(reflexive.port(), address.port(), "the same socket asked");
        let kinds: Vec<&str> = reach
            .candidates()
            .iter()
            .map(|candidate| candidate.kind)
            .collect();
        assert!(
            kinds.contains(&"srflx"),
            "the signalling list has it: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn a_hello_that_does_not_verify_is_ignored() {
        let identity = Identity::derive(b"the host secret", Purpose::Host);
        let (mut engine, address) = Engine::bind(identity, 0, Reach::default(), Vec::new())
            .await
            .expect("a socket");

        let stranger = Identity::derive(b"some other phone", Purpose::Device);
        let (_secret, public) = crypto::ephemeral_keypair();
        let mut packet = Packet::hello(
            PacketType::ClientHello,
            7,
            public,
            stranger.endpoint_id(),
            [0u8; 64],
        );
        // A signature from a different key: the name is not this sender's.
        let signature =
            Identity::derive(b"not that phone", Purpose::Device).sign(&packet.handshake_message());
        packet = Packet::hello(
            PacketType::ClientHello,
            7,
            public,
            stranger.endpoint_id(),
            signature,
        );

        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.expect("a socket");
        socket
            .send_to(&packet.encode(), address)
            .await
            .expect("sent");

        assert!(
            tokio::time::timeout(Duration::from_millis(300), engine.accept())
                .await
                .is_err(),
            "a hello nobody signed for is not a connection"
        );
    }

    #[tokio::test]
    async fn a_repeated_hello_is_answered_with_the_same_key() {
        let identity = Identity::derive(b"the host secret", Purpose::Host);
        let endpoint_id = identity.endpoint_id().to_string();
        let (mut engine, address) = Engine::bind(identity, 0, Reach::default(), Vec::new())
            .await
            .expect("a socket");

        let mut dialer = hello(address).await;
        dialer.settle(&endpoint_id).await;
        let _stream = tokio::time::timeout(Duration::from_secs(2), engine.accept())
            .await
            .expect("a stream is accepted");

        // The dialler did not hear the first answer and punches again. The second
        // answer must carry the key the session was built with, or the dialler
        // would derive keys the acceptor does not have.
        let (local_secret, local_public) = crypto::ephemeral_keypair();
        let _ = local_secret;
        let mut repeat = Packet::hello(
            PacketType::ClientHello,
            dialer.connection_id,
            local_public,
            dialer.identity.endpoint_id(),
            [0u8; 64],
        );
        let signature = dialer.identity.sign(&repeat.handshake_message());
        repeat = Packet::hello(
            PacketType::ClientHello,
            dialer.connection_id,
            local_public,
            dialer.identity.endpoint_id(),
            signature,
        );
        dialer
            .socket
            .send_to(&repeat.encode(), address)
            .await
            .expect("sent");

        let mut buffer = vec![0u8; 2048];
        let (length, _) =
            tokio::time::timeout(Duration::from_secs(2), dialer.socket.recv_from(&mut buffer))
                .await
                .expect("the answer comes again")
                .expect("a datagram");
        let packet = Packet::decode(&buffer[..length]).expect("a packet");
        let PacketBody::Hello { ref ephemeral, .. } = packet.body else {
            panic!("an answer is a hello");
        };
        // The same ephemeral key as the first answer: the session the acceptor
        // holds was built from it.
        let first = dialer.session.is_some();
        assert!(first);
        let shared = crypto::agree(&dialer.local_secret, ephemeral);
        let mut session = Session::new(
            Role::Dialer,
            dialer.connection_id,
            dialer.identity.endpoint_id(),
            &endpoint_id,
            &shared,
            Instant::now(),
        );
        // Proving the key is the session's: a message sealed with it opens.
        let packet = session.send(b"still here", Instant::now()).remove(0);
        dialer
            .socket
            .send_to(&packet.encode(), address)
            .await
            .expect("sent");
    }

    /// The acceptor's own punch: a phone behind a restricted NAT is reached by
    /// this machine sending first, not by the phone's Hello arriving.
    #[tokio::test]
    async fn an_acceptor_punch_reaches_a_phone_behind_a_restricted_nat() {
        let identity = Identity::derive(b"the host secret", Purpose::Host);
        let (mut engine, _address) = Engine::bind(identity, 0, Reach::default(), Vec::new())
            .await
            .expect("an engine");
        let host_endpoint_id = engine.endpoint_id().to_string();

        let phone_socket = UdpSocket::bind(("127.0.0.1", 0))
            .await
            .expect("a phone socket");
        let phone_address = phone_socket.local_addr().expect("an address");
        let phone_identity = Identity::derive(b"the device token", Purpose::Device);
        let (phone_secret, phone_public) = crypto::ephemeral_keypair();

        engine
            .punch_sender()
            .send(PunchTarget {
                endpoint_id: phone_identity.endpoint_id().to_string(),
                candidates: vec![phone_address],
            })
            .await
            .expect("the punch is queued");

        // The engine's ClientHello is the packet that opens this machine's NAT.
        let mut buffer = vec![0u8; 2048];
        let (length, from) =
            tokio::time::timeout(Duration::from_secs(2), phone_socket.recv_from(&mut buffer))
                .await
                .expect("the acceptor punches back")
                .expect("a datagram");
        let packet = Packet::decode(&buffer[..length]).expect("a packet");
        let connection_id = packet.connection_id;
        let PacketBody::Hello {
            ephemeral: host_public,
            endpoint_id: host_id,
            ..
        } = packet.body
        else {
            panic!("the punch is a hello");
        };
        assert_eq!(packet.kind, PacketType::ClientHello);
        assert_eq!(host_id, host_endpoint_id);

        let mut hello = Packet::hello(
            PacketType::ServerHello,
            connection_id,
            phone_public,
            phone_identity.endpoint_id(),
            [0u8; 64],
        );
        let signature = phone_identity.sign(&hello.handshake_message());
        hello = Packet::hello(
            PacketType::ServerHello,
            connection_id,
            phone_public,
            phone_identity.endpoint_id(),
            signature,
        );
        phone_socket
            .send_to(&hello.encode(), from)
            .await
            .expect("the answer goes out");

        let mut stream = tokio::time::timeout(Duration::from_secs(2), engine.accept())
            .await
            .expect("the punched session reaches the server")
            .expect("a stream");

        let shared = crypto::agree(&phone_secret, &host_public);
        let mut session = Session::new(
            Role::Acceptor,
            connection_id,
            phone_identity.endpoint_id(),
            &host_id,
            &shared,
            Instant::now(),
        );
        for packet in session.send(b"back through the punch", Instant::now()) {
            phone_socket
                .send_to(&packet.encode(), from)
                .await
                .expect("sent");
        }
        let message = tokio::time::timeout(Duration::from_secs(2), stream.receive())
            .await
            .expect("the engine reads it")
            .expect("a message");
        assert_eq!(message, b"back through the punch");
    }
}
