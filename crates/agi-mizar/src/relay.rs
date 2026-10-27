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
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

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
    /// Open phone WebSockets, by relay stream id.
    streams: Mutex<HashMap<String, mpsc::Sender<StreamCommand>>>,
}

/// A command for one proxied WebSocket.
enum StreamCommand {
    Text(String),
    Close,
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
            streams: Mutex::new(HashMap::new()),
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
        loop {
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

        let (socket, _) = tokio_tungstenite::connect_async(request).await?;
        tracing::info!("relay: tunnel established with {}", self.base);

        let (mut sink, mut stream) = socket.split();
        let (out, mut out_rx) = mpsc::channel::<Value>(64);

        // One writer task: every frame from every replayed request goes through
        // it, so two handlers can never interleave on the socket.
        let writer = tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                let text = frame.to_string();
                if sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
        });

        while let Some(message) = stream.next().await {
            let Ok(message) = message else { break };
            let Message::Text(text) = message else {
                continue;
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
                    tracing::info!("relay: room {room}");
                    *self.room() = Some(room);
                    self.connected.store(true, Ordering::Relaxed);
                }
                Some("ping") => {
                    let id = frame.get("id").cloned().unwrap_or(Value::Null);
                    let _ = out.send(json!({ "t": "pong", "id": id })).await;
                }
                Some("req") => {
                    let client = self.clone_handle();
                    let out = out.clone();
                    tokio::spawn(async move { replay_request(client, frame, out).await });
                }
                Some("ws-open") => self.open_stream(frame, out.clone()),
                Some("ws-msg") => self.forward_stream_message(&frame),
                Some("ws-close") => self.close_stream(&frame),
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
        self.streams().clear();
        drop(out);
        let _ = writer.await;
        Ok(())
    }

    /// A cheap handle for a task that only needs the local port.
    fn clone_handle(&self) -> LocalTarget {
        LocalTarget {
            port: self.local_port,
        }
    }

    /// Start proxying one phone WebSocket.
    fn open_stream(&self, frame: Value, out: mpsc::Sender<Value>) {
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

        tokio::spawn(async move {
            let url = format!("ws://127.0.0.1:{port}{path}");
            let socket = match tokio_tungstenite::connect_async(url).await {
                Ok((socket, _)) => socket,
                Err(error) => {
                    // Not `ws-close`: the relay has not heard `ws-open-ok` yet,
                    // so a close lands on a stream with no phone attached, and
                    // the relay drops its own deadline instead of failing the
                    // upgrade — the phone then waits out its own timeout with a
                    // slot held. `error` is the frame that ends it now.
                    let _ = out
                        .send(json!({
                            "t": "error", "id": id,
                            "message": format!("the local listener refused the stream: {error}")
                        }))
                        .await;
                    return;
                }
            };

            let (mut sink, mut stream) = socket.split();

            // The relay holds the phone until the computer says the stream is
            // up, and gives up on a timer if it never hears: `ws-open-ok` is not
            // optional bookkeeping.
            let _ = out.send(json!({ "t": "ws-open-ok", "id": id })).await;

            // Local → relay.
            let relay_out = out.clone();
            let local_id = id.clone();
            let reading = tokio::spawn(async move {
                while let Some(message) = stream.next().await {
                    let Ok(message) = message else { break };
                    match message {
                        Message::Text(text) => {
                            // Base64 even for text: the tunnel is JSON, and the
                            // relay decodes every `ws-msg` the same way.
                            let _ = relay_out
                                .send(json!({
                                    "t": "ws-msg", "id": local_id,
                                    "data": base64(text.as_bytes()), "binary": false
                                }))
                                .await;
                        }
                        Message::Binary(bytes) => {
                            let _ = relay_out
                                .send(json!({
                                    "t": "ws-msg", "id": local_id,
                                    "data": base64(&bytes), "binary": true
                                }))
                                .await;
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
                let _ = relay_out
                    .send(json!({ "t": "ws-close", "id": local_id }))
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
        });
    }

    fn forward_stream_message(&self, frame: &Value) {
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
        let Ok(text) = String::from_utf8(bytes) else {
            return;
        };
        if let Some(sender) = self.streams().get(id) {
            let _ = sender.try_send(StreamCommand::Text(text));
        }
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
async fn replay_request(target: LocalTarget, frame: Value, out: mpsc::Sender<Value>) {
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

    let _ = out.send(answer).await;
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
    let scheme = if trimmed.starts_with("https://") {
        "wss://"
    } else if trimmed.starts_with("http://") {
        "ws://"
    } else if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        ""
    } else {
        anyhow::bail!("{base} is not a relay URL");
    };
    let rest = trimmed
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("wss://")
        .trim_start_matches("ws://");
    Ok(format!("{scheme}{rest}{TUNNEL_PATH}"))
}

/// Standard base64, for binary frames that have to ride inside JSON.
pub(crate) fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let value = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(value >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(value >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(value >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[value as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Decode standard base64.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let index = ALPHABET.iter().position(|candidate| *candidate == byte)? as u32;
        buffer = (buffer << 6) | index;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
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
}
