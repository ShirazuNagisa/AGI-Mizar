//! The reverse tunnel: how a phone reaches this computer from another network.
//!
//! A home router does not accept inbound connections, so the computer dials *out*
//! to a relay and the relay forwards phone requests back down that socket. From
//! the phone's side nothing has changed — a request arrives and an answer goes
//! back — which is why the listener never learns that a request came through a
//! tunnel.
//!
//! ### The shape of it
//!
//! ```text
//! phone ──https──▶ relay ──ws──▶ this computer ──http──▶ 127.0.0.1:19392
//!                    ▲                                        │
//!                    └────────────── res ─────────────────────┘
//! ```
//!
//! The tunnel carries four kinds of work: an HTTP request, a WebSocket open, a
//! message on an open WebSocket, and a close. Plus a ping the relay sends so a
//! half-open socket is noticed rather than believed.
//!
//! ### Why replay through a real socket
//!
//! The alternative — calling the axum handlers directly — would mean the tunnel
//! and the LAN path answer through different code, and every future bug would
//! have to be fixed twice. A loopback HTTP request is a few microseconds and
//! keeps one path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

use crate::store::DeviceStore;

/// Where the account headers come from: the session token and this device's id.
type AccountSource = Arc<dyn Fn() -> Option<(String, String)> + Send + Sync>;

/// Whether this computer currently accepts same-account peers.
pub type PeerPolicy = Arc<dyn Fn() -> bool + Send + Sync>;

/// The path a computer dials to establish its tunnel.
pub const TUNNEL_PATH: &str = "/tunnel";

/// The tunnel protocol version.
pub const TUNNEL_PROTOCOL_VERSION: u32 = 1;

/// How long a replayed request may take before the relay is told it failed.
///
/// The relay gives up at 30 s; answering first means the phone gets a real error
/// instead of a timeout with no explanation.
const REPLAY_TIMEOUT: Duration = Duration::from_secs(25);

/// How long to wait before redialling.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);

/// The relay this build points at unless told otherwise.
pub const PUBLIC_RELAY_URL: &str = "https://dshmizar.shirazu-nagisa.com";

/// One live tunnel.
pub struct RelayClient {
    /// The relay's base URL, `https://…`.
    base: String,
    /// The tunnel secret. The relay derives the room from it.
    token: String,
    /// The local port to replay onto.
    local_port: u16,
    room: Mutex<Option<String>>,
    connected: AtomicBool,
    /// Set when this client has been replaced by one pointed elsewhere.
    stopped: AtomicBool,
    /// Open phone WebSockets, by relay stream id.
    streams: Arc<Mutex<HashMap<String, mpsc::Sender<StreamCommand>>>>,
    /// The account session this desktop presents, when signed in.
    account: Mutex<Option<AccountSource>>,
    /// The roster tokens are minted from, when the relay brokers a phone.
    store: Mutex<Option<Arc<DeviceStore>>>,
    /// The operator's remote-peer switch, read when the relay asks for a peer.
    peer_policy: Mutex<Option<PeerPolicy>>,
    /// Tells the read loop to hang up so the next dial carries fresher headers.
    kick: broadcast::Sender<()>,
}

/// A command for one proxied WebSocket.
enum StreamCommand {
    Text(String),
    Close,
}

/// One frame for the tunnel's writer.
///
/// Control frames are JSON and stay JSON; a `ws-msg` may be binary when the
/// relay advertised that it reads the framing (see [`encode_ws_msg`]). The
/// writer is the only task that touches the sink, so the two kinds cannot
/// interleave mid-frame.
enum Outgoing {
    Json(Value),
    Bytes(Vec<u8>),
    /// A WebSocket ping, answered by the relay's protocol implementation.
    Ping,
}

impl From<Value> for Outgoing {
    fn from(value: Value) -> Self {
        Outgoing::Json(value)
    }
}

/// The first byte of a binary tunnel frame: a `ws-msg`.
const FRAME_WS_MSG: u8 = 0x01;

/// Encode one `ws-msg` as the binary framing.
///
/// `{kind, id length, id, flags, payload}`. The JSON + base64 spelling costs a
/// third more bytes and two full passes over every conversation page, on the one
/// hop that already knows the payload is a WebSocket message.
pub fn encode_ws_msg(id: &str, binary: bool, payload: &[u8]) -> Vec<u8> {
    let id = id.as_bytes();
    let length = id.len().min(255);
    let mut out = Vec::with_capacity(payload.len() + length + 3);
    out.push(FRAME_WS_MSG);
    out.push(length as u8);
    out.extend_from_slice(&id[..length]);
    out.push(if binary { 1 } else { 0 });
    out.extend_from_slice(payload);
    out
}

/// Decode one binary `ws-msg`.
///
/// `None` rather than a panic: this runs inside the tunnel reader, where one
/// malformed frame must not end the tunnel.
pub fn decode_ws_msg(data: &[u8]) -> Option<(String, bool, &[u8])> {
    if data.len() < 3 || data[0] != FRAME_WS_MSG {
        return None;
    }
    let length = data[1] as usize;
    let start = 2 + length;
    if length == 0 || data.len() < start + 1 {
        return None;
    }
    let id = std::str::from_utf8(&data[2..start]).ok()?.to_string();
    let binary = data[start] == 1;
    Some((id, binary, &data[start + 1..]))
}

impl RelayClient {
    /// Build a client for one relay and one local port.
    pub fn new(base: impl Into<String>, token: impl Into<String>, local_port: u16) -> Arc<Self> {
        Arc::new(Self {
            base: base.into(),
            token: token.into(),
            local_port,
            room: Mutex::new(None),
            connected: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            streams: Arc::new(Mutex::new(HashMap::new())),
            account: Mutex::new(None),
            store: Mutex::new(None),
            peer_policy: Mutex::new(None),
            kick: broadcast::channel(4).0,
        })
    }

    fn room(&self) -> MutexGuard<'_, Option<String>> {
        self.room
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn streams(&self) -> MutexGuard<'_, HashMap<String, mpsc::Sender<StreamCommand>>> {
        self.streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The relay's base URL, for `host.info`.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The secret this tunnel authenticates with, kept for a move to another
    /// relay: the room derives from it, so a move keeps every pairing intact.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Ask this client to stop redialling. The socket itself is closed by the
    /// relay when the replacement dials the same room.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// A client for the same tunnel on a different relay.
    pub fn rebased(&self, base: impl Into<String>) -> Arc<Self> {
        Self::new(base, self.token.clone(), self.local_port)
    }

    /// Where the account session comes from, when this desktop is signed in.
    pub fn set_account_source(&self, source: AccountSource) {
        *self
            .account
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source);
    }

    /// Give the tunnel the roster it mints phone tokens from.
    pub fn attach_store(&self, store: Arc<DeviceStore>) {
        *self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(store);
    }

    /// Give the tunnel the operator's remote-peer switch.
    pub fn set_peer_policy(&self, policy: PeerPolicy) {
        *self
            .peer_policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(policy);
    }

    /// Hang up so the next dial presents the current account headers.
    pub fn recycle(&self) {
        let _ = self.kick.send(());
    }

    /// The room the relay assigned, once it has.
    pub fn current_room(&self) -> Option<String> {
        self.room().clone()
    }

    /// Whether the tunnel is up.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// The base URL a phone should be given, when a room is known.
    ///
    /// `https://relay.example.com/r/<room>` — the relay routes everything under
    /// that prefix to this computer.
    pub fn phone_base_url(&self) -> Option<String> {
        let room = self.current_room()?;
        // A relay that welcomed us without a room would otherwise produce
        // `https://relay/r/`, a URL that 404s on every request a phone makes.
        if room.is_empty() {
            return None;
        }
        Some(format!("{}/r/{room}", self.base.trim_end_matches('/')))
    }

    /// Dial, serve, redial. Runs until the process ends.
    pub async fn run(self: Arc<Self>) {
        // rustls 0.23 refuses to guess which crypto provider to use when the
        // dependency graph leaves more than one (or none) reachable, and it does
        // so by panicking inside whichever thread dialled first. The tunnel is
        // the only TLS client in this process, so the choice is made here, next
        // to the code that depends on it, before the first handshake.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut backoff = RECONNECT_MIN;
        while !self.stopped.load(Ordering::Relaxed) {
            match self.serve_once().await {
                Ok(()) => {
                    backoff = RECONNECT_MIN;
                    tracing::info!("relay: tunnel closed");
                }
                Err(error) => {
                    tracing::warn!("relay: {error}");
                }
            }
            self.connected.store(false, Ordering::Relaxed);
            *self.room() = None;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_MAX);
        }
    }

    /// One connection, from dial to close.
    async fn serve_once(&self) -> anyhow::Result<()> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let url = tunnel_url(&self.base)?;
        let mut request = url.as_str().into_client_request()?;
        // The relay authenticates the tunnel with the same secret it derives the
        // room from, which is what keeps a second enrolment mechanism out.
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.token)
                .parse()
                .map_err(|_| anyhow::anyhow!("the tunnel token is not a header value"))?,
        );

        if let Some((session, device)) = self
            .account
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|source| source())
        {
            request.headers_mut().insert(
                "x-mizar-session",
                session
                    .parse()
                    .map_err(|_| anyhow::anyhow!("the account session is not a header value"))?,
            );
            request.headers_mut().insert(
                "x-mizar-client-id",
                device
                    .parse()
                    .map_err(|_| anyhow::anyhow!("the device id is not a header value"))?,
            );
        }

        // Advertised per dial: the relay answers `wsMsgBinary` in `welcome` when
        // it will carry framed `ws-msg`, and only then does this side send one.
        // A relay older than the header keeps receiving JSON + base64.
        request.headers_mut().insert(
            "x-mizar-tunnel-frames",
            "binary"
                .parse()
                .map_err(|_| anyhow::anyhow!("the frame capability is not a header value"))?,
        );

        let (socket, _) = tokio_tungstenite::connect_async(request).await?;
        tracing::info!("relay: tunnel established with {}", self.base);

        let (mut sink, mut stream) = socket.split();
        let (out, mut out_rx) = mpsc::channel::<Outgoing>(64);
        // Shared with every stream task: set once, when `welcome` says the relay
        // reads framed `ws-msg`. Until then (and on a relay that never says so)
        // the JSON + base64 spelling is what goes out.
        let binary_frames = Arc::new(AtomicBool::new(false));

        // One writer task: every frame from every replayed request goes through
        // it, so two handlers can never interleave on the socket.
        let writer = tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                let message = match frame {
                    Outgoing::Json(value) => Message::Text(value.to_string()),
                    Outgoing::Bytes(bytes) => Message::Binary(bytes),
                    Outgoing::Ping => Message::Ping(Vec::new()),
                };
                // A relay that stops reading must not hold every phone request
                // behind a full TCP window forever. The reader below notices a
                // closed socket; a timed-out writer has to close it too.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    sink.send(message),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    _ => break,
                }
            }
            // Read loop wakes on EOF and redials instead of leaving a half-open
            // tunnel in the room.
            let _ = sink.close().await;
        });

        // A Wi-Fi change leaves the old TCP tunnel half-open: writes are
        // accepted by the kernel and nothing ever arrives. The relay's own
        // 90 s sweeper eventually drops the room, but that is far too late for
        // a phone that is already trying to reconnect. Ping every five seconds
        // and treat fifteen seconds without any frame (including the pong) as
        // a dead tunnel, then redial immediately.
        let heartbeat_out = out.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if heartbeat_out.send(Outgoing::Ping).await.is_err() {
                    break;
                }
            }
        });

        let mut kick = self.kick.subscribe();
        'socket: loop {
            let message = tokio::select! {
                _ = kick.recv() => break 'socket,
                message = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    stream.next(),
                ) => match message {
                    Ok(message) => message,
                    Err(_) => {
                        tracing::warn!("relay: no frame or pong for 15s; redialling the tunnel");
                        break 'socket;
                    }
                },
            };
            let Some(message) = message else { break };
            let Ok(message) = message else { break };
            let text = match message {
                Message::Text(text) => text,
                Message::Binary(bytes) => {
                    // Only a `ws-msg` is ever framed; the relay sends none until
                    // `welcome` said so, so an old build never sees one.
                    if let Some((id, binary, payload)) = decode_ws_msg(&bytes) {
                        self.forward_stream_frame(&id, binary, payload, &out).await;
                    }
                    continue;
                }
                _ => continue,
            };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            match frame.get("t").and_then(Value::as_str) {
                Some("welcome") => {
                    let room = frame
                        .get("room")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let version = frame
                        .get("protocolVersion")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if version != TUNNEL_PROTOCOL_VERSION as u64 {
                        tracing::warn!(
                            "relay: speaks tunnel v{version}, this build speaks v{TUNNEL_PROTOCOL_VERSION}"
                        );
                        break;
                    }
                    if frame.get("wsMsgBinary").and_then(Value::as_bool) == Some(true) {
                        binary_frames.store(true, Ordering::Relaxed);
                        tracing::info!("relay: framed ws-msg enabled");
                    }
                    tracing::info!("relay: room {room}");
                    *self.room() = Some(room);
                    self.connected.store(true, Ordering::Relaxed);
                }
                Some("ping") => {
                    let id = frame.get("id").cloned().unwrap_or(Value::Null);
                    let _ = out.send(json!({ "t": "pong", "id": id }).into()).await;
                }
                Some("req") => {
                    let client = self.clone_handle();
                    let out = out.clone();
                    tokio::spawn(async move { replay_request(client, frame, out).await });
                }
                Some("ws-open") => self.open_stream(frame, out.clone(), Arc::clone(&binary_frames)),
                Some("ws-msg") => self.forward_stream_message(&frame, &out).await,
                Some("ws-close") => self.close_stream(&frame),
                Some("provision") => self.provision_phone(frame, out.clone()),
                Some("error") => {
                    let message = frame
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("the relay refused the tunnel");
                    tracing::warn!("relay: {message}");
                    break;
                }
                _ => {}
            }
        }

        // Every stream belongs to this socket; they all end with it.
        //
        // Do not await the writer. It may be stuck in `sink.send` on a
        // half-open socket whose peer already sent FIN -- which is exactly what
        // a relay timeout leaves behind. Awaiting it made `serve_once` never
        // return, so the redial loop above never ran and the room stayed dark
        // for hours. Drop the output channel, then abort the writer and reap it.
        self.streams().clear();
        heartbeat.abort();
        drop(out);
        writer.abort();
        let _ = writer.await;
        Ok(())
    }

    /// A cheap handle for a task that only needs the local port.
    fn clone_handle(&self) -> LocalTarget {
        LocalTarget {
            port: self.local_port,
        }
    }

    /// Mint a token for one phone, or say why it could not be done.
    fn provision_phone(&self, frame: Value, out: mpsc::Sender<Outgoing>) {
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let account = frame
            .get("accountId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let phone = frame
            .get("phoneDeviceId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = frame
            .get("phoneName")
            .and_then(Value::as_str)
            .unwrap_or("Phone")
            .to_string();
        // Which agent this phone asked for. Empty means "everything", which is
        // what an older client gets: it never sends the field.
        let engine = frame
            .get("engine")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // A desktop peer is a separate capability from a phone: the operator
        // turns it on for this machine, and the relay only asks after the paid
        // gate has passed.
        let peer = frame.get("peer").and_then(Value::as_bool).unwrap_or(false);
        let peer_allowed = self
            .peer_policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|policy| policy())
            .unwrap_or(false);
        if peer && !peer_allowed {
            let out = out.clone();
            tokio::spawn(async move {
                let _ = out
                    .send(json!({
                        "t": "error", "id": id,
                        "message": "this computer does not accept remote peers"
                    }).into())
                    .await;
            });
            return;
        }
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        tokio::spawn(async move {
            let token = match store {
                Some(store) if !account.is_empty() && !phone.is_empty() => {
                    if peer {
                        provision_for_peer(&store, &account, &phone, &name, &engine).ok()
                    } else {
                        provision_for_phone(&store, &account, &phone, &name, &engine).ok()
                    }
                }
                _ => None,
            };
            let frame = match token {
                Some(token) => json!({ "t": "provision-result", "id": id, "token": token }),
                None => json!({
                    "t": "error", "id": id,
                    "message": "this computer cannot mint a token right now"
                }),
            };
            let _ = out.send(frame.into()).await;
        });
    }

    /// Start proxying one phone WebSocket.
    fn open_stream(&self, frame: Value, out: mpsc::Sender<Outgoing>, binary_frames: Arc<AtomicBool>) {
        let Some(id) = frame.get("id").and_then(Value::as_str).map(str::to_string) else {
            return;
        };
        let path = frame
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .to_string();
        let port = self.local_port;

        let (commands, mut command_rx) = mpsc::channel::<StreamCommand>(64);
        self.streams().insert(id.clone(), commands);
        let streams = Arc::clone(&self.streams);

        tokio::spawn(async move {
            let url = format!("ws://127.0.0.1:{port}{path}");
            let socket = match tokio_tungstenite::connect_async(url).await {
                Ok((socket, _)) => socket,
                Err(error) => {
                    // The map entry was inserted before this task started; with
                    // no local socket behind it nothing would ever remove it
                    // until the whole tunnel reconnected.
                    streams
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&id);
                    // Not `ws-close`: the relay has not heard `ws-open-ok` yet,
                    // so a close lands on a stream with no phone attached, and
                    // the relay drops its own deadline instead of failing the
                    // upgrade — the phone then waits out its own timeout with a
                    // slot held. `error` is the frame that ends it now.
                    let _ = out
                        .send(json!({
                            "t": "error", "id": id,
                            "message": format!("the local listener refused the stream: {error}")
                        }).into())
                        .await;
                    return;
                }
            };

            let (mut sink, mut stream) = socket.split();

            // The relay holds the phone until the computer says the stream is
            // up, and gives up on a timer if it never hears: `ws-open-ok` is not
            // optional bookkeeping.
            let _ = out.send(json!({ "t": "ws-open-ok", "id": id }).into()).await;

            // Local → relay.
            let relay_out = out.clone();
            let local_id = id.clone();
            let reading = tokio::spawn(async move {
                while let Some(message) = stream.next().await {
                    let Ok(message) = message else { break };
                    match message {
                        Message::Text(text) => {
                            if binary_frames.load(Ordering::Relaxed) {
                                // Framed: the payload goes as it is, with no
                                // base64 and no outer JSON.
                                let _ = relay_out
                                    .send(Outgoing::Bytes(encode_ws_msg(
                                        &local_id,
                                        false,
                                        text.as_bytes(),
                                    )))
                                    .await;
                            } else {
                                // Base64 even for text: the tunnel is JSON, and
                                // the relay decodes every `ws-msg` the same way.
                                let _ = relay_out
                                    .send(json!({
                                        "t": "ws-msg", "id": local_id,
                                        "data": base64(text.as_bytes()), "binary": false
                                    }).into())
                                    .await;
                            }
                        }
                        Message::Binary(bytes) => {
                            if binary_frames.load(Ordering::Relaxed) {
                                let _ = relay_out
                                    .send(Outgoing::Bytes(encode_ws_msg(
                                        &local_id,
                                        true,
                                        &bytes,
                                    )))
                                    .await;
                            } else {
                                let _ = relay_out
                                    .send(json!({
                                        "t": "ws-msg", "id": local_id,
                                        "data": base64(&bytes), "binary": true
                                    }).into())
                                    .await;
                            }
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
                let _ = relay_out
                    .send(json!({ "t": "ws-close", "id": local_id }).into())
                    .await;
            });

            // Relay → local.
            while let Some(command) = command_rx.recv().await {
                match command {
                    StreamCommand::Text(text) => {
                        if sink.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    StreamCommand::Close => {
                        let _ = sink.close().await;
                        break;
                    }
                }
            }

            reading.abort();
            // Either side ending means this stream is over: the relay will
            // learn it from the `ws-close` the reading task sent, and the map
            // must not keep a sender nobody reads.
            streams
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
        });
    }

    /// Route one already-decoded `ws-msg` to its local stream.
    ///
    /// The binary framing hands over bytes, not a JSON object; the local
    /// listener is a Mizar socket that speaks JSON text, so a binary payload is
    /// dropped rather than invented into text -- the same rule the JSON path
    /// applies.
    async fn forward_stream_frame(
        &self,
        id: &str,
        binary: bool,
        payload: &[u8],
        out: &mpsc::Sender<Outgoing>,
    ) {
        if binary {
            return;
        }
        let Ok(text) = std::str::from_utf8(payload) else {
            return;
        };
        let sender = self.streams().get(id).cloned();
        let delivered = sender.is_some_and(|sender| {
            sender
                .try_send(StreamCommand::Text(text.to_string()))
                .is_ok()
        });
        if delivered {
            return;
        }
        // Unknown, or its 64-slot command queue is full. A dropped frame is a
        // phone waiting out its full timeout (or an upload discovering a gap
        // chunks later), so end the stream instead of pretending it arrived.
        self.streams().remove(id);
        let _ = out
            .send(json!({ "t": "ws-close", "id": id }).into())
            .await;
    }

    async fn forward_stream_message(&self, frame: &Value, out: &mpsc::Sender<Outgoing>) {
        let Some(id) = frame.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(data) = frame.get("data").and_then(Value::as_str) else {
            return;
        };
        // Every `ws-msg` is base64, text included: the tunnel is JSON and the
        // relay does not special-case a text frame. The local listener is a
        // Mizar socket, which speaks JSON text, so a binary frame from a phone is
        // dropped rather than invented into text.
        if frame.get("binary").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let Some(bytes) = unbase64(data) else {
            return;
        };
        self.forward_stream_frame(id, false, &bytes, out).await;
    }

    fn close_stream(&self, frame: &Value) {
        let Some(id) = frame.get("id").and_then(Value::as_str) else {
            return;
        };
        if let Some(sender) = self.streams().remove(id) {
            let _ = sender.try_send(StreamCommand::Close);
        }
    }
}

/// Where a replayed request goes.
struct LocalTarget {
    port: u16,
}

/// Replay one relayed HTTP request onto the local listener and answer it.
async fn replay_request(target: LocalTarget, frame: Value, out: mpsc::Sender<Outgoing>) {
    let id = frame
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let method = frame
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_string();
    let path = frame
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("/")
        .to_string();
    // The tunnel is JSON, so a body rides as base64 in both directions. Sending
    // the text itself was invisible on a fake relay and fatal on the real one:
    // the relay base64-decodes whatever a computer answers, so a JSON document
    // arrived at the phone as 82 bytes of noise.
    let body = frame
        .get("body")
        .and_then(Value::as_str)
        .map(|text| unbase64(text).unwrap_or_default())
        .unwrap_or_default();

    let answer = match tokio::time::timeout(
        REPLAY_TIMEOUT,
        local_request(target.port, &method, &path, &body),
    )
    .await
    {
        Ok(Ok(response)) => json!({
            "t": "res", "id": id,
            "status": response.status,
            "headers": response.headers,
            "body": base64(&response.body),
        }),
        Ok(Err(error)) => json!({
            "t": "res", "id": id, "status": 502,
            "headers": { "content-type": "application/json" },
            "body": base64(
                json!({ "error": format!("the local listener did not answer: {error}") })
                    .to_string()
                    .as_bytes()
            ),
        }),
        Err(_) => json!({
            "t": "res", "id": id, "status": 504,
            "headers": { "content-type": "application/json" },
            "body": base64(b"{\"error\":\"the local listener timed out\"}"),
        }),
    };

    let _ = out.send(answer.into()).await;
}

/// One HTTP response from the local listener.
struct LocalResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// Perform one loopback HTTP request.
///
/// `Connection: close` and read to end: the listener is ours, it honours the
/// header, and this avoids a content-length parser that would have to handle
/// chunked encoding to be correct. The bytes are kept as bytes — a body is not
/// necessarily UTF-8, and it is the relay's job to hand the phone what the
/// listener wrote.
async fn local_request(
    port: u16,
    method: &str,
    path: &str,
    body: &[u8],
) -> std::io::Result<LocalResponse> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept-Encoding: identity\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;

    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).await?;
    let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the listener answered without headers",
        ));
    };
    let head = String::from_utf8_lossy(&buffer[..end]).to_string();
    let mut body = buffer[end + 4..].to_vec();

    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(502);

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    // Undo chunked framing when the listener used it: the relay wants the
    // payload, not the transport. An incomplete body is an error rather than an
    // empty 200 — a truncated answer that looks successful is the worst answer.
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        let Some(decoded) = dechunk(&body) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the listener's chunked body was incomplete",
            ));
        };
        body = decoded;
        headers.remove("transfer-encoding");
    }

    // Hop-by-hop headers describe our loopback hop, not the phone's: forwarding
    // `connection: close` would make the relay hang up after every request.
    const HOP_BY_HOP: [&str; 5] = ["connection", "keep-alive", "te", "trailer", "upgrade"];
    headers.retain(|name, _| !HOP_BY_HOP.contains(&name.as_str()));

    Ok(LocalResponse {
        status,
        headers,
        body,
    })
}

/// Strip HTTP chunked framing from a body.
fn dechunk(body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let line_end = rest.windows(2).position(|window| window == b"\r\n")?;
        let size = std::str::from_utf8(&rest[..line_end])
            .ok()?
            .split(';')
            .next()?
            .trim();
        let size = usize::from_str_radix(size, 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(rest.get(..size)?);
        rest = rest.get(size.checked_add(2)?..)?;
    }
}

/// Turn `https://relay` into `wss://relay/tunnel`.
pub fn tunnel_url(base: &str) -> anyhow::Result<String> {
    let trimmed = base.trim_end_matches('/');
    // A `ws`/`wss` base keeps its own scheme. The first version stripped every
    // scheme and then re-added one only for http/https, so a caller that passed
    // an already-WebSocket relay URL got a scheme-less `host:port/tunnel` -- a
    // URL that cannot be dialled. Every current caller passes https, which is
    // why it went unnoticed; the tests below pin the ws spelling too.
    let (scheme, rest) = if let Some(rest) = trimmed.strip_prefix("https://") {
        ("wss://", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        ("ws://", rest)
    } else if let Some(rest) = trimmed.strip_prefix("wss://") {
        ("wss://", rest)
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        ("ws://", rest)
    } else {
        anyhow::bail!("{base} is not a relay URL");
    };
    Ok(format!("{scheme}{rest}{TUNNEL_PATH}"))
}

/// Standard base64, for binary frames that have to ride inside JSON.
pub(crate) fn base64(bytes: &[u8]) -> String {
    agi_protocol::b64::encode(bytes)
}

/// Decode standard base64.
pub(crate) fn unbase64(text: &str) -> Option<Vec<u8>> {
    agi_protocol::b64::decode(text)
}

/// The marker a provisioned token carries, so logout can find it again.
///
/// The engine rides at the end: a phone that chose "DSH" gets a token that can
/// see nothing else, and the choice survives a reconnect because it is part of
/// the identity rather than part of the session.
fn account_marker(account: &str, phone: &str, engine: &str) -> String {
    if engine.is_empty() {
        format!("account:{account}:{phone}")
    } else {
        format!("account:{account}:{phone}:{engine}")
    }
}

/// The marker a peer token carries: account, calling desktop, engine.
fn peer_marker(account: &str, peer: &str, engine: &str) -> String {
    if engine.is_empty() {
        format!("peer:{account}:{peer}")
    } else {
        format!("peer:{account}:{peer}:{engine}")
    }
}

/// The stable peer identity embedded in a peer token marker.
///
/// `peer:<account>:<device>` — deliberately without the engine, so switching
/// the target's agent does not create a second session-ownership bucket.
pub fn peer_identity_of_marker(marker: &str) -> Option<String> {
    let mut parts = marker.split(':');
    if parts.next() != Some("peer") {
        return None;
    }
    let account = parts.next().filter(|part| !part.is_empty())?;
    let peer = parts.next().filter(|part| !part.is_empty())?;
    Some(format!("peer:{account}:{peer}"))
}

/// The engine a provisioned token was scoped to, if any.
pub fn engine_of_marker(marker: &str) -> Option<&str> {
    let mut parts = marker.split(':');
    let kind = parts.next()?;
    if kind != "account" && kind != "peer" {
        return None;
    }
    parts.next()?; // account id
    parts.next()?; // device id
    let engine = parts.next()?;
    (!engine.is_empty()).then_some(engine)
}

/// Mint one peer's token, replacing the one it had before.
///
/// Peer tokens and phone tokens use different markers: a phone switching agents
/// must not revoke a peer, and a peer reconnecting must not touch the phones.
fn provision_for_peer(
    store: &DeviceStore,
    account: &str,
    peer: &str,
    name: &str,
    engine: &str,
) -> std::io::Result<String> {
    let marker = peer_marker(account, peer, engine);
    let stale = format!("peer:{account}:{peer}");
    for device in store.devices() {
        let is_this_peer = device
            .model
            .as_deref()
            .is_some_and(|model| model == stale || model.starts_with(&format!("{stale}:")));
        if device.is_live() && is_this_peer {
            let _ = store.revoke(&device.id);
        }
    }
    store.issue(name, Some(marker)).map(|issued| issued.token)
}

/// Mint one phone's token, replacing the one it had before.
///
/// Reusing the roster's revoke-then-issue keeps the number of live tokens at one
/// per phone however many times the phone reconnects.
fn provision_for_phone(
    store: &DeviceStore,
    account: &str,
    phone: &str,
    name: &str,
    engine: &str,
) -> std::io::Result<String> {
    let marker = account_marker(account, phone, engine);
    // Revoke every token this phone had, whatever agent it was scoped to: the
    // phone holds one token at a time, and switching from DSH to Codex must not
    // leave the previous scope alive.
    let stale = format!("account:{account}:{phone}");
    for device in store.devices() {
        let is_this_phone = device
            .model
            .as_deref()
            .is_some_and(|model| model == stale || model.starts_with(&format!("{stale}:")));
        if device.is_live() && is_this_phone {
            let _ = store.revoke(&device.id);
        }
    }
    store.issue(name, Some(marker)).map(|issued| issued.token)
}

/// Revoke every token this account was provisioned, without touching the tunnel.
pub fn revoke_account_tokens(store: &DeviceStore, account: &str) {
    for prefix in [format!("account:{account}:"), format!("peer:{account}:")] {
        for device in store.devices() {
            if device.is_live()
                && device
                    .model
                    .as_deref()
                    .is_some_and(|model| model.starts_with(&prefix))
            {
                let _ = store.revoke(&device.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_urls_translate_the_scheme() {
        assert_eq!(
            tunnel_url("https://relay.example.com").unwrap(),
            "wss://relay.example.com/tunnel"
        );
        assert_eq!(
            tunnel_url("https://relay.example.com/").unwrap(),
            "wss://relay.example.com/tunnel"
        );
        assert_eq!(
            tunnel_url("http://127.0.0.1:8080").unwrap(),
            "ws://127.0.0.1:8080/tunnel"
        );
        // A base that is already a WebSocket URL keeps its scheme: stripping it
        // produces a URL with no scheme at all, which cannot be dialled.
        assert_eq!(
            tunnel_url("ws://127.0.0.1:8080").unwrap(),
            "ws://127.0.0.1:8080/tunnel"
        );
        assert_eq!(
            tunnel_url("wss://relay.example.com").unwrap(),
            "wss://relay.example.com/tunnel"
        );
        assert!(tunnel_url("ftp://nope").is_err());
    }

    #[test]
    fn the_phone_url_carries_the_room() {
        let client = RelayClient::new("https://relay.example.com", "t", 1);
        assert!(client.phone_base_url().is_none());
        *client.room() = Some("abc".to_string());
        assert_eq!(
            client.phone_base_url().as_deref(),
            Some("https://relay.example.com/r/abc")
        );
    }

    #[test]
    fn base64_round_trips() {
        for text in ["", "a", "ab", "abc", "hello world", "\u{1F600}"] {
            let encoded = base64(text.as_bytes());
            assert_eq!(unbase64(&encoded).unwrap(), text.as_bytes());
        }
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn a_relay_that_names_no_room_gets_no_phone_url() {
        let client = RelayClient::new("https://relay.example.com", "t", 1);
        *client.room() = Some(String::new());
        assert!(client.phone_base_url().is_none());
    }

    #[test]
    fn dechunks_what_a_listener_sends() {
        assert_eq!(
            dechunk(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(),
            b"Wikipedia"
        );
        // Chunk extensions and uppercase hex are both legal.
        assert_eq!(
            dechunk(b"4;name=value\r\nWiki\r\n0\r\n\r\n").unwrap(),
            b"Wiki"
        );
        assert_eq!(
            dechunk(b"A\r\n0123456789\r\n0\r\n\r\n").unwrap(),
            b"0123456789"
        );
        assert_eq!(dechunk(b"0\r\n\r\n").unwrap(), b"");
        // A truncated chunk is an error, not an empty answer.
        assert!(dechunk(b"4\r\nWiki").is_none());
        assert!(dechunk(b"5\r\nWiki\r\n0\r\n\r\n").is_none());
        assert!(dechunk(b"zz\r\n").is_none());
    }

    #[test]
    fn a_provisioned_token_replaces_the_phone_that_had_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(dir.path().join("devices.json"), "host").unwrap();
        // A pairing-issued device is not this code's business.
        let paired = store.issue("iPhone (paired)", None).unwrap();

        let first = provision_for_phone(&store, "u1", "phone-a", "iPhone", "").unwrap();
        assert!(store.lookup(&first).is_some());
        let second = provision_for_phone(&store, "u1", "phone-a", "iPhone", "").unwrap();
        assert_ne!(first, second);
        assert!(store.lookup(&first).is_none(), "the old token is revoked");
        assert!(store.lookup(&second).is_some());
        assert!(store.lookup(&paired.token).is_some());

        // Another phone of the same account keeps its own token.
        let other = provision_for_phone(&store, "u1", "phone-b", "iPad", "").unwrap();
        assert!(store.lookup(&other).is_some());
        assert!(store.lookup(&second).is_some());

        // Signing out revokes both, and nothing else.
        revoke_account_tokens(&store, "u1");
        assert!(store.lookup(&second).is_none());
        assert!(store.lookup(&other).is_none());
        assert!(store.lookup(&paired.token).is_some());
    }

    #[test]
    fn peer_markers_keep_their_identity_and_engine() {
        assert_eq!(
            peer_identity_of_marker("peer:acct-1:peer-1:dsh").as_deref(),
            Some("peer:acct-1:peer-1")
        );
        assert_eq!(engine_of_marker("peer:acct-1:peer-1:dsh"), Some("dsh"));
        assert_eq!(peer_identity_of_marker("account:acct-1:phone-1:dsh"), None);
        assert_eq!(engine_of_marker("account:acct-1:phone-1:dsh"), Some("dsh"));
    }

    /// A peer re-provision touches peers of the same caller, not phones.
    #[test]
    fn a_provisioned_peer_replaces_only_that_peer() {
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(dir.path().join("devices.json"), "host").unwrap();
        let phone = provision_for_phone(&store, "u1", "caller", "Phone", "dsh").unwrap();
        let first = provision_for_peer(&store, "u1", "caller", "Peer", "dsh").unwrap();
        let second = provision_for_peer(&store, "u1", "caller", "Peer", "dsh").unwrap();
        assert_ne!(first, second);
        assert!(
            store.lookup(&first).is_none(),
            "the old peer token is revoked"
        );
        assert!(store.lookup(&second).is_some());
        assert!(store.lookup(&phone).is_some(), "phones are not peer tokens");

        // Another caller's peer keeps its token.
        let other = provision_for_peer(&store, "u1", "caller-2", "Peer 2", "dsh").unwrap();
        assert!(store.lookup(&other).is_some());
        revoke_account_tokens(&store, "u1");
        assert!(store.lookup(&second).is_none());
        assert!(store.lookup(&other).is_none());
        assert!(store.lookup(&phone).is_none());
    }

    #[test]
    fn another_accounts_tokens_are_not_revoked_by_a_sign_out() {
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(dir.path().join("devices.json"), "host").unwrap();
        let mine = provision_for_phone(&store, "u1", "p", "iPhone", "").unwrap();
        let theirs = provision_for_phone(&store, "u2", "q", "iPhone", "").unwrap();
        revoke_account_tokens(&store, "u1");
        assert!(store.lookup(&mine).is_none());
        assert!(store.lookup(&theirs).is_some());
    }
}
