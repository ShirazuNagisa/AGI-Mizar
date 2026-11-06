//! The phone-facing server: pairing, devices, and the Mizar v1 WebSocket.
//!
//! Three routes, exactly as the protocol says:
//!
//! | route | auth | job |
//! |---|---|---|
//! | `GET /mizar/v1/hello` | none | "is this address dialable, and whose is it" |
//! | `POST /mizar/v1/pair` | none, one shot | a code for a device token |
//! | `GET /mizar/v1/ws` | device token in the first frame | everything else |
//!
//! ### Why `hello` and `pair` are unauthenticated
//!
//! Because a phone that has never paired has no credential yet. The exposure is
//! bounded on purpose: `hello` reveals a host id and a name, `pair` needs a code
//! that exists for five minutes, is single use, and is rate limited. Nothing else
//! answers without a token.
//!
//! ### What this build does not implement
//!
//! `NOT_IMPLEMENTED` is a real answer, not a stub: the protocol says a client
//! seeing it knows the host is older than it. Methods that need machinery this
//! build does not have yet — uploads, file preview, workspace listing, activity,
//! relay switching, push registration — say so instead of pretending.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agi_core::Core;
use agi_engine::MAX_SLICE_BYTES;
use agi_protocol::domain::{AttachmentKind, Balance, InteractionKind, SessionKey, Wallet};
use agi_protocol::engine::EngineId;
use agi_protocol::usage::UsageContext;
use agi_protocol::SessionState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use crate::frames::{
    record_to_envelope, ClientFrame, ErrorCode, Hello, ServerFrame, SessionRow, WelcomeInput,
    HTTP_PREFIX, PROTOCOL_VERSION, WS_PATH,
};
use crate::pairing::{self, PairingCode};
use crate::relay::RelayClient;
use crate::store::DeviceStore;

/// How many pairing attempts a minute are allowed.
const PAIR_ATTEMPTS_PER_MINUTE: usize = 5;

/// The path a phone dials to exchange direct-path candidates.
///
/// Not under `HTTP_PREFIX`: this is MizarNet's channel, not Mizar v1's, and a
/// future version of one must not drag the other along.
const NET_SIGNAL_PATH: &str = "/mizar-net/signal";

/// The live pairing code, if one was issued and has not been used.
struct ActiveCode {
    code: PairingCode,
}

/// The Mizar server.
pub struct MizarServer {
    core: Arc<Core>,
    store: Arc<DeviceStore>,
    /// The DSH home, reported in `host.info` and `welcome`.
    home: String,
    code: Mutex<Option<ActiveCode>>,
    attempts: Mutex<VecDeque<Instant>>,
    port: AtomicU16,
    /// The tunnel, when one is configured.
    relay: Mutex<Option<Arc<RelayClient>>>,
    /// Notices every connected phone receives: today, that the tunnel moved.
    notices: broadcast::Sender<Value>,
    /// Devices whose app is on screen: they see an approval already, so waking
    /// them for it is noise. Presence is host-local, never sent to the relay.
    foreground: Mutex<std::collections::HashSet<String>>,
    /// The client that talks to the relay's push API.
    http: reqwest::Client,
    /// Devices with a socket open right now, and how many each has. A phone
    /// that reconnects before its old socket is reaped must not read as offline
    /// in between, so this counts rather than flags.
    live: Mutex<std::collections::HashMap<String, usize>>,
    /// Files a phone is sending, staged until a prompt names them.
    uploads: std::sync::Arc<crate::uploads::UploadStore>,
    /// The account this computer is signed in to, kept current by the boot
    /// sequence from the account events the window already publishes.
    ///
    /// A copy rather than a call through to the kernel because the check runs on
    /// the socket's path: the answer must be available without taking a lock the
    /// window might hold.
    account: Mutex<Option<String>>,
    /// The direct path, when the boot sequence has bound one.
    ///
    /// `None` on a host with no tunnel: a punch needs a room to exchange
    /// candidates in, so without a relay there is nothing to advertise and the
    /// phone's route plan correctly has no `.p2p` in it.
    direct: Mutex<Option<DirectPath>>,
    /// Sessions each peer created, by `peer:<account>:<device>`.
    ///
    /// The peer token proves the account, not the ownership rule: without this
    /// map a peer could name any session id and read another conversation.
    peer_sessions: Mutex<std::collections::HashMap<String, std::collections::BTreeSet<String>>>,
    /// Where the peer ownership map is kept, once boot points us at it.
    peer_sessions_path: Mutex<Option<std::path::PathBuf>>,
}

/// Where the direct path listens, and the name a phone dials it by.
struct DirectPath {
    /// The base64url Ed25519 public key, derived from the tunnel secret.
    endpoint_id: String,
    /// Where the engine can be reached, kept current by the engine itself.
    reach: crate::net::engine::Reach,
}

/// Keeps the live count honest however the socket ends.
struct LiveSocket {
    server: Arc<MizarServer>,
    device: String,
}

impl LiveSocket {
    fn open(server: Arc<MizarServer>, device: String) -> Self {
        *server
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(device.clone())
            .or_insert(0) += 1;
        // A phone is listening. This is the only signal the kernel has that
        // anything out there will read a verdict, and it is what buys the judge
        // its lease -- see `Core::phone_attached`.
        server.core.phone_attached();
        Self { server, device }
    }
}

impl Drop for LiveSocket {
    fn drop(&mut self) {
        self.server.core.phone_detached();
        let mut live = self
            .server
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match live.get_mut(&self.device) {
            Some(count) if *count > 1 => *count -= 1,
            _ => {
                live.remove(&self.device);
            }
        }
    }
}

impl agi_core::DeviceStatusSource for MizarServer {
    fn device_views(&self) -> Vec<agi_protocol::api::DeviceView> {
        let live = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.store
            .devices()
            .into_iter()
            .filter(|device| device.is_live())
            .map(|device| agi_protocol::api::DeviceView {
                online: live.get(&device.id).copied().unwrap_or(0) > 0,
                last_seen_ms: device.last_seen_at,
                id: device.id,
                name: device.name,
                model: device.model,
            })
            .collect()
    }
}

impl agi_core::RelayStatusSource for MizarServer {
    fn relay_status(&self) -> agi_core::RelayStatus {
        let relay = self
            .relay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match relay {
            Some(relay) => agi_core::RelayStatus {
                connected: relay.is_connected(),
                room: relay.current_room(),
            },
            None => agi_core::RelayStatus::default(),
        }
    }
}

impl MizarServer {
    /// Build the server over a kernel and a device roster.
    pub fn new(core: Arc<Core>, store: Arc<DeviceStore>, home: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            core,
            store,
            home: home.into(),
            code: Mutex::new(None),
            attempts: Mutex::new(VecDeque::new()),
            port: AtomicU16::new(0),
            relay: Mutex::new(None),
            notices: broadcast::channel(16).0,
            foreground: Mutex::new(std::collections::HashSet::new()),
            live: Mutex::new(std::collections::HashMap::new()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            uploads: Arc::new(upload_store()),
            account: Mutex::new(None),
            direct: Mutex::new(None),
            peer_sessions: Mutex::new(std::collections::HashMap::new()),
            peer_sessions_path: Mutex::new(None),
        })
    }

    fn code(&self) -> MutexGuard<'_, Option<ActiveCode>> {
        self.code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn attempts(&self) -> MutexGuard<'_, VecDeque<Instant>> {
        self.attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Attach a tunnel, so `host.info` can tell a phone where to find it from
    /// another network.
    pub fn attach_relay(&self, relay: Arc<RelayClient>) {
        *self
            .relay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(relay);
    }

    /// The relays a phone may ask this host to move between.
    ///
    /// The list is the phone's too (`RelayTier`): a host that accepted an
    /// arbitrary origin would become a way to point a phone's traffic anywhere.
    const SELECTABLE_RELAYS: [&'static str; 3] = [
        "https://dshmizar.shirazu-nagisa.com",
        "https://dshmizar-1.shirazu-nagisa.com",
        // The account-gated relay: same host, different port, no anonymous
        // traffic. Experimental, and only this build knows the address.
        "https://dshmizar-1.shirazu-nagisa.com:8443",
    ];

    /// Move the tunnel to another public relay.
    ///
    /// The secret does not change, and the room derives from it, so every
    /// pairing survives the move: phones keep the same room on a new origin.
    fn switch_relay(&self, url: &str) -> Result<Value, &'static str> {
        let base = url.trim_end_matches('/');
        if !Self::SELECTABLE_RELAYS.contains(&base) {
            return Err("relay.set: that is not one of the public relays");
        }
        let mut guard = self
            .relay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(current) = guard.as_ref().cloned() else {
            return Err("relay.set: this host has no tunnel to move");
        };
        let room = current.current_room();
        let base_url = room.as_ref().map(|room| format!("{}/r/{room}", base));
        if current.base() == base {
            return Ok(json!({ "url": base, "room": room, "baseURL": base_url }));
        }

        current.stop();
        let next = current.rebased(base);
        let running = Arc::clone(&next);
        tokio::spawn(async move { running.run().await });
        *guard = Some(next);
        Ok(json!({ "url": base, "room": room, "baseURL": base_url }))
    }

    /// The tunnel facts a phone reads, when a tunnel is configured.
    fn relay_facts(&self) -> Value {
        let guard = self
            .relay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(relay) = guard.as_ref() else {
            return Value::Null;
        };
        json!({
            "url": relay.base(),
            "room": relay.current_room(),
            "baseURL": relay.phone_base_url(),
        })
    }

    /// The port the listener took.
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    /// Issue a fresh pairing code, replacing any previous one.
    ///
    /// Replacing rather than accumulating is deliberate: two live codes are two
    /// things that can leak, and the operator only ever shows one.
    pub fn issue_code(&self) -> PairingCode {
        let code = pairing::generate(pairing::now_ms());
        *self.code() = Some(ActiveCode { code: code.clone() });
        code
    }

    /// Every address a phone could dial, `host:port`, best first.
    pub fn addresses(&self) -> Vec<String> {
        let port = self.port();
        let mut addresses: Vec<String> = crate::store::local_addresses()
            .into_iter()
            .map(|address| format!("{address}:{port}"))
            .collect();
        // Loopback last, and only as a fallback: a phone cannot dial it, but a
        // simulator on this machine can.
        addresses.push(format!("127.0.0.1:{port}"));
        addresses
    }

    /// The tunnel's base URL for this room, when one is known.
    fn relay_base_url(&self) -> Option<String> {
        let guard = self
            .relay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.as_ref()?.phone_base_url()
    }

    /// The `mizar://pair?...` URI for a code, using the address a phone can reach.
    ///
    /// Two routes when there are two: the LAN address a phone on this network
    /// can dial, and the relay room that works from anywhere else.
    pub fn pairing_uri(&self, code: &PairingCode) -> String {
        let host = self
            .addresses()
            .into_iter()
            .next()
            .unwrap_or_else(|| format!("127.0.0.1:{}", self.port()));
        let relay = self.relay_base_url();
        crate::frames::pairing_uri(
            &host,
            relay.as_deref(),
            &code.display,
            &self.store.host_name(),
        )
    }

    /// The code to show a phone, its URI, and a QR of that URI.
    ///
    /// `refresh` issues a new code; otherwise the one already showing is reused,
    /// because a window that re-renders must not invalidate the code somebody is
    /// in the middle of typing.
    pub fn pairing_view(&self, refresh: bool) -> agi_protocol::api::PairingView {
        let code = if refresh {
            self.issue_code()
        } else {
            // Drop the lock before issuing: `issue_code` takes the same one, and
            // a `unwrap_or_else` reached while the guard is still alive waits on
            // a mutex this very thread holds. That froze the server the first
            // time the window asked again after a code had been used.
            let showing = self.code().as_ref().map(|active| active.code.clone());
            match showing {
                Some(code) => code,
                None => self.issue_code(),
            }
        };
        let uri = self.pairing_uri(&code);
        let svg = qrcode::QrCode::new(uri.as_bytes()).ok().map(|qr| {
            qr.render::<qrcode::render::svg::Color>()
                .min_dimensions(180, 180)
                .dark_color(qrcode::render::svg::Color("#1c1c1e"))
                .light_color(qrcode::render::svg::Color("#ffffff"))
                .build()
        });

        agi_protocol::api::PairingView {
            code: code.display.clone(),
            uri,
            addresses: self.addresses(),
            svg,
            expires_at: code.expires_at,
        }
    }

    /// Record the account this computer is signed in to.
    ///
    /// `None` when signed out. Called by the boot sequence on every account
    /// change, which is also what makes a sign-out take effect on the next
    /// connection rather than the next launch.
    pub fn set_account_user(&self, user_id: Option<String>) {
        *self
            .account
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = user_id;
    }

    /// Why this phone may not talk to this computer, when it may not.
    ///
    /// The one question neither the pairing code nor the device token answers:
    /// a phone signed in to one account and a computer signed in to another are
    /// two different people, however the code travelled. Both sides have to have
    /// said; a phone or a computer that is signed out is left as it was, because
    /// making "signed out" mean "refused" would strand every pairing made before
    /// accounts existed.
    ///
    /// - Parameter phone: the account id from the phone's hello, when it sent one.
    /// - Returns: a sentence for the log and the phone, or `None` when allowed.
    fn account_mismatch(&self, phone: Option<&str>) -> Option<String> {
        let phone = phone.map(str::trim).filter(|id| !id.is_empty())?;
        let desktop = self
            .account
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .filter(|id| !id.is_empty())?;
        if phone == desktop {
            return None;
        }
        Some("this computer is signed in to a different account than this phone".to_string())
    }

    /// Attach the direct path, once its UDP socket is bound.
    ///
    /// Called by the boot sequence, which is the only thing that knows the tunnel
    /// secret the identity is derived from, and which owns the socket.
    ///
    /// - Parameters:
    ///   - endpoint_id: the base64url public key a phone's ticket names.
    ///   - port: the UDP port the engine bound.
    pub fn attach_direct_path(
        &self,
        endpoint_id: impl Into<String>,
        reach: crate::net::engine::Reach,
    ) {
        *self
            .direct
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(DirectPath {
            endpoint_id: endpoint_id.into(),
            reach,
        });
    }

    /// The direct path's name, when one is listening.
    ///
    /// What `host.info` carries and what a phone turns into a `.p2p` route.
    /// `None` — absent on the wire — means this host has no direct path, which a
    /// phone reads as "there is nothing to punch to".
    pub fn net_endpoint_id(&self) -> Option<String> {
        self.direct
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|path| path.endpoint_id.clone())
    }

    /// The candidate list, for the tests that pin its shape.
    ///
    /// Public and `doc(hidden)` because an integration test is another crate:
    /// the alternative is a test that re-implements the formula and therefore
    /// checks nothing.
    #[doc(hidden)]
    pub fn net_candidates_for_tests(&self) -> Vec<Value> {
        self.net_candidates()
    }

    /// Where this computer can be reached directly, best first.
    ///
    /// Host addresses first, then the outside address a STUN server last saw.
    ///
    /// The reflexive candidate is what makes a punch possible from another
    /// network at all, and the engine re-asks every fifteen seconds so the NAT
    /// mapping behind it stays open.
    fn net_candidates(&self) -> Vec<Value> {
        let Some(reach) = self
            .direct
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|path| path.reach.clone())
        else {
            return Vec::new();
        };
        reach
            .candidates()
            .iter()
            .map(crate::net::engine::Candidate::to_json)
            .collect()
    }

    /// Publish a card, for the test that pins what the relay receives.
    ///
    /// `doc(hidden)` rather than private because an integration test is another
    /// crate, and re-implementing the body in the test would check nothing.
    #[doc(hidden)]
    pub async fn publish_card_for_tests(&self, card: &crate::activity::Card) -> bool {
        self.publish_card(card).await
    }

    /// Publish a turn-end notification, for the same reason.
    #[doc(hidden)]
    pub async fn publish_turn_end_for_tests(&self, session: &SessionKey, title: &str, end: bool) {
        self.publish_turn_end(session, title, end).await;
    }

    /// Mark a device's presence for the push-path integration test.
    #[doc(hidden)]
    pub fn set_foreground_for_tests(&self, device_id: &str, foreground: bool) {
        let mut known = self
            .foreground
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if foreground {
            known.insert(device_id.to_string());
        } else {
            known.remove(device_id);
        }
    }

    /// Run one authenticated LAN connection.
    ///
    /// Noise and the account certificate have already proven the peer; this
    /// method mints a short-lived local token with the same `peer:<account>:
    /// <device>:dsh` marker the WAN path uses, sends it over the encrypted
    /// session, and then hands the byte stream to the ordinary Mizar peer loop.
    /// The token is revoked when the session ends, so a captured LAN transcript
    /// cannot be replayed as a bearer token.
    pub async fn serve_lan(
        self: Arc<Self>,
        remote: agi_lan::RemoteDevice,
        session: agi_lan::LanSession,
    ) -> anyhow::Result<()> {
        let prefs = self.core.remote_prefs();
        if !prefs.enabled || !prefs.allow_lan {
            anyhow::bail!("LAN peers are disabled on this computer");
        }
        if prefs
            .blocked
            .iter()
            .any(|blocked| blocked == &remote.device_id)
        {
            anyhow::bail!("this device is blocked on this computer");
        }
        let marker = format!("peer:{}:{}:dsh", remote.account_id, remote.device_id);
        let issued = self
            .store
            .issue(&format!("LAN peer {}", remote.name), Some(marker))
            .map_err(|error| anyhow::anyhow!("issuing the LAN token: {error}"))?;
        let token_frame = json!({
            "v": 1,
            "type": "lan.session",
            "token": issued.token,
        });
        if let Err(error) = session
            .send(
                serde_json::to_vec(&token_frame)
                    .unwrap_or_default()
                    .as_slice(),
            )
            .await
        {
            let _ = self.store.revoke(&issued.device.id);
            return Err(anyhow::anyhow!("sending the LAN token: {error}"));
        }

        let session = Arc::new(session);
        let (inbound_tx, inbound_rx) = mpsc::channel::<String>(64);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(64);
        let peer_name = remote.device_id.clone();
        let peer_task = tokio::spawn(serve_peer(
            Arc::clone(&self),
            peer_name,
            Peer {
                inbound: inbound_rx,
                outbound: outbound_tx,
            },
        ));

        let reader_session = Arc::clone(&session);
        let reader = tokio::spawn(async move {
            loop {
                match reader_session.recv().await {
                    Ok(bytes) => {
                        let Ok(text) = String::from_utf8(bytes) else {
                            continue;
                        };
                        if inbound_tx.send(text).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        let writer_session = Arc::clone(&session);
        let writer = tokio::spawn(async move {
            while let Some(bytes) = outbound_rx.recv().await {
                if writer_session.send(&bytes).await.is_err() {
                    return;
                }
            }
        });

        let _ = peer_task.await;
        reader.abort();
        writer.abort();
        let _ = self.store.revoke(&issued.device.id);
        Ok(())
    }

    /// Run one punched connection: the same handshake and methods as a socket.
    ///
    /// Public because the accept loop lives in the boot sequence, which owns the
    /// engine. Everything below this call is transport-agnostic.
    pub async fn serve_punched(self: &Arc<Self>, stream: crate::net::engine::Stream) {
        serve_punched(Arc::clone(self), stream).await;
    }

    /// Bind the listener and start serving.
    ///
    /// `port` of `0` asks the OS for a free one; the default is the protocol's
    /// 19392, which is where a paired phone will look first.
    pub async fn bind(self: &Arc<Self>, port: u16) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let router = Router::new()
            .route(&format!("{HTTP_PREFIX}/hello"), get(hello))
            .route(&format!("{HTTP_PREFIX}/pair"), post(pair))
            .route(WS_PATH, get(upgrade))
            // The side channel a punch needs. Reached through the relay's room
            // like every other path, which is why it lives beside them.
            .route(NET_SIGNAL_PATH, get(signal))
            .with_state(Arc::clone(self));

        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        let bound = listener.local_addr()?.port();
        self.port.store(bound, Ordering::Relaxed);

        // One listener for the whole host. A phone that is asleep still needs
        // to know the agent is blocked, and that must not depend on a socket
        // being open at the time.
        {
            let server = Arc::clone(self);
            let mut events = server.core.subscribe();
            tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(agi_protocol::api::ApiEvent::InteractionRequested { interaction }) => {
                            server.notify_interaction(&interaction).await;
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return,
                    }
                }
            });
        }

        // The island. Two subscriptions, because a row starts on a session event
        // and ends on a verdict: the tracker is the only thing that knows both,
        // and it is also the only thing that knows the card changed, which is
        // what keeps one turn from becoming a push per record.
        {
            let server = Arc::clone(self);
            let mut events = server.core.subscribe();
            let mut states = server.core.subscribe_states();
            tokio::spawn(async move {
                let mut tracker = crate::activity::Tracker::default();
                // Start with one reconciliation owed: a session may already be
                // running when this process starts, before any SessionUpdated
                // event has reached this loop. The first tick publishes that
                // card; subsequent ticks only run after a failed publish.
                let mut retry = true;
                let mut reconcile = tokio::time::interval(std::time::Duration::from_secs(10));
                reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = reconcile.tick() => {
                            if retry && server.publish_current_card().await {
                                retry = false;
                            }
                        }
                        event = events.recv() => match event {
                            Ok(agi_protocol::api::ApiEvent::SessionUpdated { session }) => {
                                if !session.running {
                                    // Do not rely only on the state machine's terminal
                                    // frame to finish the card. A missed terminal frame
                                    // used to leave the island up until the app was
                                    // opened; the session list's running flag is the
                                    // durable second source.
                                    server.finish_card(&mut tracker, &session.key, true).await;
                                    continue;
                                }
                                if let Some(card) = tracker.running(&session, activity_now()) {
                                    retry = !server.publish_card(&card).await;
                                }
                            }
                            Ok(agi_protocol::api::ApiEvent::SessionRemoved { session }) => {
                                // An archived or vanished session: take it off the
                                // card, but "a turn finished" would be a claim
                                // nobody made.
                                server.finish_card(&mut tracker, &session, false).await;
                            }
                            Ok(agi_protocol::api::ApiEvent::InteractionRequested { interaction }) => {
                                if let Some(card) = tracker.approval_requested(&interaction) {
                                    retry = !server.publish_card(&card).await;
                                }
                            }
                            Ok(agi_protocol::api::ApiEvent::InteractionResolved { id }) => {
                                if let Some(card) = tracker.approval_resolved(&id) {
                                    retry = !server.publish_card(&card).await;
                                }
                            }
                            Ok(_) => {}
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => return,
                        },
                        frame = states.recv() => match frame {
                            Ok((session, bytes)) => {
                                let Ok(frame) = agi_protocol::state_frame::StateFrame::decode(&bytes)
                                else {
                                    continue;
                                };
                                if frame.state.kind() != agi_protocol::session_state::StateKind::Terminal {
                                    continue;
                                }
                                server.finish_card(&mut tracker, &session, true).await;
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => return,
                        },
                    }
                }
            });
        }

        Ok(tokio::spawn(async move {
            // With peer addresses: "the phone says it cannot connect" is
            // otherwise indistinguishable from "nothing arrived", and a request
            // replayed by the tunnel comes from 127.0.0.1 while a phone on this
            // network comes from its own address.
            let service = router.into_make_service_with_connect_info::<SocketAddr>();
            if let Err(error) = axum::serve(listener, service).await {
                tracing::error!("mizar listener stopped: {error}");
            }
        }))
    }

    /// Whether a pairing attempt is allowed right now.
    fn allow_attempt(&self) -> bool {
        let now = Instant::now();
        let mut attempts = self.attempts();
        while attempts
            .front()
            .is_some_and(|at| now.duration_since(*at) > Duration::from_secs(60))
        {
            attempts.pop_front();
        }
        if attempts.len() >= PAIR_ATTEMPTS_PER_MINUTE {
            return false;
        }
        attempts.push_back(now);
        true
    }

    /// The agent this phone connected for, when it said.
    ///
    /// A token minted through the account flow carries the choice, so a phone
    /// that picked "DSH" cannot see the Claude or Codex sessions at all -- the
    /// filter lives here, not in the phone's view code, because "only that
    /// agent's content" is a promise about what leaves this machine.
    fn engine_scope(device: &crate::store::Device) -> Option<agi_protocol::engine::EngineId> {
        let engine = crate::relay::engine_of_marker(device.model.as_deref()?)?;
        agi_protocol::engine::EngineId::parse(engine)
    }

    /// The agent one connection is bound to, if any.
    ///
    /// Two ways to have chosen one, and the order matters. A token minted by the
    /// relay carries the choice because it has to survive the reconnect the relay
    /// brokers; it wins, always, or a phone holding a DSH-scoped token could ask
    /// for Claude at `hello` and read another agent's conversations. A phone that
    /// paired by scanning a code has an unscoped token, so the choice rides on
    /// the socket instead.
    ///
    /// `None` means "all three", which is what every phone did before choosing
    /// was possible; an unscoped connection is not an error, it is the older
    /// contract, and this file keeps honouring it.
    fn connection_scope(
        device: &crate::store::Device,
        hello: Option<&Hello>,
    ) -> Option<agi_protocol::engine::EngineId> {
        let asked = hello
            .and_then(|hello| hello.engine.as_deref())
            .and_then(agi_protocol::engine::EngineId::parse);
        Self::engine_scope(device).or(asked)
    }

    /// The engines this connection may see, in the order the host tries them.
    ///
    /// One for a phone that chose an agent, all three for the older contract.
    /// Every per-engine answer is built from this rather than from the literal
    /// list, which is how `workspace.list` came to draw the other two agents'
    /// folders on a phone that had asked for one.
    fn visible_engines(
        scope: Option<agi_protocol::engine::EngineId>,
    ) -> Vec<agi_protocol::engine::EngineId> {
        match scope {
            Some(engine) => vec![engine],
            None => agi_protocol::engine::EngineId::all().to_vec(),
        }
    }

    /// Whether a session is one this connection may read.
    fn in_scope(scope: Option<agi_protocol::engine::EngineId>, session: &SessionKey) -> bool {
        scope.is_none_or(|engine| session.engine == engine)
    }

    /// One Mizar method call.
    async fn call(
        &self,
        device: &crate::store::Device,
        scope: Option<agi_protocol::engine::EngineId>,
        frame: &ClientFrame,
    ) -> ServerFrame {
        let id = frame.id.clone().unwrap_or_default();

        match frame.kind.as_str() {
            // The cheapest frame the phone can ask for, and the only one that
            // answers without touching the store: a round trip measured with it
            // is the link, not this machine's work. Answered before any method
            // lookup for the same reason -- see the phone's `measureLatency`.
            "ping" => {
                let time = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|since| since.as_millis() as f64)
                    .unwrap_or_default();
                ServerFrame::ok(id, json!({ "time": time }))
            }
            "host.info" => {
                let addresses = self.addresses();
                // Which agents this computer serves, so the phone can ask which
                // one to open instead of having to guess from a merged list. The
                // keys are the wire names `EngineId::parse` accepts.
                let engines: Vec<Value> = agi_protocol::engine::EngineId::all()
                    .into_iter()
                    .filter_map(|engine_id| {
                        let engine = self.core.engine(engine_id)?;
                        let (connected, detail) = engine.status();
                        Some(json!({
                            "id": engine_id.as_str(),
                            "label": engine_id.label(),
                            "connected": connected,
                            "detail": detail,
                        }))
                    })
                    .collect();
                ServerFrame::ok(
                    id,
                    json!({
                        "hostId": self.store.host_id(),
                        "hostName": self.store.host_name(),
                        "home": self.home,
                        "protocolVersion": PROTOCOL_VERSION,
                        "addresses": addresses,
                        "desktop": { "os": os_name(), "client": "AGI Mizar" },
                        "engines": engines,
                        "remote": remote_view(&self.core.remote_prefs()),
                        // What this connection is bound to. Absent on a phone
                        // that did not choose, which is also what an older host
                        // says -- so the phone reads "no answer" and "no choice"
                        // as the same thing and does not invent a filter.
                        "engine": scope.map(|engine| engine.as_str()),
                        // What this host can do beyond the base protocol. The
                        // app gates the balance card on the capability rather
                        // than on a probe: without it, the card says the host
                        // does not support balance and never asks.
                        "plugin": {
                            "id": "dsh-mizar",
                            // The wallet belongs to DSH, so a phone scoped to
                            // another agent is not offered it: a card that can
                            // only say "unsupported" is worse than no card.
                            "capabilities": if self.core.engine(EngineId::Dsh).is_some()
                                && scope.is_none_or(|engine| engine == EngineId::Dsh) {
                                json!(["deepseek.balance.v1"])
                            } else {
                                json!([])
                            },
                        },
                        // `null` when no tunnel is configured, which the phone
                        // reads as "there is no relay route".
                        "relay": self.relay_facts(),
                        // The direct path's key, or `null` when this host has
                        // none. A phone that sees it grows a `.p2p` route; one
                        // that sees `null` keeps the two it had.
                        "netEndpointId": self.net_endpoint_id(),
                    }),
                )
            }

            "session.list" => {
                let snapshot = self.core.snapshot();
                let items: Vec<SessionRow> = snapshot
                    .sessions
                    .iter()
                    .filter(|session| {
                        Self::in_scope(scope, &session.key) && self.peer_owns(device, &session.key)
                    })
                    .map(|session| SessionRow {
                        session_id: session.key.to_string(),
                        updated_at: session.updated_at,
                        running: session.running,
                        blank: session.title.is_none() && session.turns == 0,
                        agent_available: true,
                        cwd: session.cwd.clone(),
                        parent_session_id: session.parent.clone(),
                        origin: session.origin.clone(),
                        engine: session.key.engine.as_str().to_string(),
                        source: session.source.clone(),
                    })
                    .collect();
                ServerFrame::ok(id, json!({ "items": items }))
            }

            "session.messages" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.messages needs a sessionId",
                    );
                };
                let before = frame.payload.get("beforeSeq").and_then(Value::as_i64);
                let limit = frame
                    .payload
                    .get("maxMessages")
                    .and_then(Value::as_u64)
                    .map(|value| value.clamp(1, 400) as usize)
                    .unwrap_or(200);

                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        format!("no engine for {}", key.engine),
                    );
                };
                let context = self
                    .core
                    .snapshot()
                    .sessions
                    .iter()
                    .find(|session| session.key == key)
                    .and_then(|session| session.context)
                    .map(|pressure| {
                        json!({
                            "window": pressure.window,
                            "pressureTokens": pressure.used,
                            "projectedTokens": pressure.used,
                            // An integer: the app decodes this as `Int`, and a
                            // fractional one makes the whole page fail to decode
                            // rather than merely draw oddly.
                            "percent": pressure.percent.round().clamp(0.0, 100.0) as u64,
                        })
                    })
                    .unwrap_or(Value::Null);

                match engine.records(&key, limit, before).await {
                    Ok(records) => {
                        let as_of = records.last().map(|record| record.seq).unwrap_or(0);
                        // `records[]` is a list of *envelopes*, not of bare
                        // events: `{"type":"event","event":{…}}`, the shape the
                        // golden fixture pins. The
                        // phones tolerate a flat event (`event ?? raw`), which is
                        // how this drifted without anyone noticing.
                        let mut records_out: Vec<Value> =
                            records.iter().filter_map(record_to_envelope).collect();
                        // What each turn cost, hung on its own `turn/end` -- the
                        // one place the phone reads it, and the one place it was
                        // always empty. An engine that cannot say answers with an
                        // empty map, so the footer falls back to its own words
                        // rather than to a number nobody counted.
                        if let Ok(totals) = engine.turn_usage(&key).await {
                            attach_turn_usage(&mut records_out, &totals);
                        }
                        ServerFrame::ok(
                            id,
                            json!({
                                "sessionId": key.to_string(),
                                "asOfSeq": as_of,
                                "records": records_out,
                                "hasMore": records.len() >= limit,
                                // The phone merges this into its cached usage frame;
                                // `null` means "this host cannot say", not zero.
                                "context": context,
                            }),
                        )
                    }
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            // The turn's streamed intervals, replayed. The live state channel
            // only carries what is happening now, so a phone that was disconnected
            // mid-turn came back to an empty interval and a hole where the
            // conversation should be. This hands the whole turn back.
            "session.stream" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.stream needs a sessionId",
                    );
                };
                let replay = self.core.stream_of(&key);
                let (turn, state, interval, text_seq, truncated, entries) = match replay {
                    Some(replay) => (
                        replay.turn.map(Value::from).unwrap_or(Value::Null),
                        json!(replay.state.code()),
                        json!(replay.interval),
                        json!(replay.text_seq),
                        json!(replay.truncated),
                        replay.entries,
                    ),
                    // A session that never ran a turn on this host has nothing to
                    // replay. That is an empty answer, not an error: the phone's
                    // older sessions are exactly this case, and a refusal would
                    // make it fall back for no reason.
                    None => (
                        Value::Null,
                        json!(SessionState::Idle.code()),
                        json!(0),
                        json!(0),
                        json!(false),
                        Vec::new(),
                    ),
                };
                let entries: Vec<Value> = entries
                    .into_iter()
                    .map(|entry| {
                        json!({
                            "interval": entry.interval,
                            "state": entry.state.code(),
                            "text": entry.text,
                            "final": entry.final_,
                        })
                    })
                    .collect();
                ServerFrame::ok(
                    id,
                    json!({
                        "sessionId": key.to_string(),
                        "turn": turn,
                        "state": state,
                        "interval": interval,
                        "textSeq": text_seq,
                        "truncated": truncated,
                        "entries": entries,
                    }),
                )
            }

            // The phone's title, usage panel, model row and permission badge all
            // come from this one frame; the old plugin answered it straight from
            // `session/projections`, and this does the same through the engine.
            "session.stats" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.stats needs a sessionId",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        format!("no engine for {}", key.engine),
                    );
                };
                let mut usage = match engine.usage(&key).await {
                    Ok(usage) => usage,
                    Err(error) => {
                        // An empty frame is the right answer — the app keeps what
                        // it has — but silence would hide a projection that
                        // stopped answering, which is how the first version of
                        // this feature went unnoticed for months.
                        tracing::warn!(
                            "mizar: session.stats could not read projections for {key} — {error}"
                        );
                        Default::default()
                    }
                };
                // Whatever the engine could not project, the host already knows:
                // the aggregate it keeps for the window has a title and a context
                // reading for every session it lists.
                if let Some(session) = self
                    .core
                    .snapshot()
                    .sessions
                    .iter()
                    .find(|session| session.key == key)
                {
                    if usage.title.is_none() {
                        usage.title = session.title.clone();
                    }
                    if usage.context.is_none() {
                        // `used` is the pressure reading, not the projected one;
                        // repeating it in both fields would make the app draw the
                        // same number twice.
                        usage.context = session.context.map(|pressure| UsageContext {
                            window: Some(pressure.window),
                            pressure_tokens: Some(pressure.used),
                            projected_tokens: None,
                            percent: Some(pressure.percent.round().clamp(0.0, 100.0) as u64),
                            breakdown: None,
                        });
                    }
                }
                ServerFrame::ok(
                    id,
                    json!({
                        "sessionId": key.to_string(),
                        "asOfSeq": Value::Null,
                        "stats": usage,
                    }),
                )
            }

            // The phone groups its session list by workspace. Membership is the
            // host's, not something the phone can derive: a session started in a
            // bare directory belongs to no workspace, and two spellings of one
            // directory are one workspace only because DSH says so.
            //
            // One engine's list, not three: this is the answer that was merged
            // for every phone, so a phone that had picked DSH was shown Claude's
            // and Codex's folders as empty headings under its own workspaces —
            // "手机上有许多不属于 dsh 的工作区". The scope is the same one
            // `session.list` uses, so the two halves of one screen cannot
            // disagree about which agent is being looked at.
            "workspace.list" => {
                let mut items = Vec::new();
                let mut complete = true;
                for engine_id in Self::visible_engines(scope) {
                    let Some(engine) = self.core.engine(engine_id) else {
                        continue;
                    };
                    match engine.workspaces().await {
                        Ok(spaces) => {
                            for (index, space) in spaces.into_iter().enumerate() {
                                // Prefixed so two engines cannot collide, and
                                // stable across restarts.
                                let local = if space.id.is_empty() {
                                    index.to_string()
                                } else {
                                    space.id.clone()
                                };
                                let title = space
                                    .name
                                    .filter(|name| !name.is_empty())
                                    .or_else(|| {
                                        space
                                            .path
                                            .rsplit('/')
                                            .find(|part| !part.is_empty())
                                            .map(str::to_string)
                                    })
                                    .unwrap_or_else(|| space.path.clone());
                                // The phone compares these against the ids
                                // `session.list` gave it, which carry the
                                // engine prefix. Sending the registry's bare
                                // ids is what makes every session look
                                // unassigned on the other side.
                                let mut row = json!({
                                    "workspaceId": format!("{}:{local}", engine_id.as_str()),
                                    "path": space.path,
                                    "title": title,
                                    // Which agent this folder belongs to. The id
                                    // already says so, but a phone that wants to
                                    // label a row (or an unscoped connection
                                    // that draws all three) should not have to
                                    // parse an id to find out.
                                    "engine": engine_id.as_str(),
                                });
                                if let Some(ids) = membership(engine_id, space.session_ids) {
                                    row["sessionIds"] = json!(ids);
                                }
                                items.push(row);
                            }
                        }
                        // An engine that cannot answer is not an engine with no
                        // workspaces; the phone keeps what it had. But an engine
                        // with no workspace concept at all (Claude, Codex) is
                        // not a failure to read the list either.
                        Err(error) if error.code() != "UNSUPPORTED" => complete = false,
                        Err(_) => {}
                    }
                }
                if Self::peer_key(device).is_some() {
                    let allowed = self.core.remote_prefs().workspaces;
                    items.retain(|row| {
                        row.get("path").and_then(Value::as_str).is_some_and(|path| {
                            allowed.iter().any(|candidate| {
                                candidate.trim_end_matches('/') == path.trim_end_matches('/')
                            })
                        })
                    });
                    // A peer must not learn which sessions its operator has in
                    // these folders; it only needs the paths it may use.
                    for row in &mut items {
                        if let Some(object) = row.as_object_mut() {
                            object.remove("sessionIds");
                        }
                    }
                }
                ServerFrame::ok(id, json!({ "items": items, "complete": complete }))
            }

            // What the model picker draws. The phone's catalog is per host, and
            // an unscoped connection on a host that fronts three engines gets
            // the merged list; the default route comes from the first engine
            // that names one.
            "model.catalog" => {
                let mut catalog = agi_protocol::catalog::ModelCatalog::default();
                // An engine with no catalog concept is not a failure; one that
                // broke is. An empty answer would be cached by the app until the
                // next connection, while an error is retried when the picker
                // opens.
                let mut broken = false;
                for engine_id in Self::visible_engines(scope) {
                    let Some(engine) = self.core.engine(engine_id) else {
                        continue;
                    };
                    match engine.model_catalog().await {
                        Ok(next) => {
                            if catalog.default.is_none() {
                                catalog.default = next.default;
                            }
                            catalog.providers.extend(next.providers);
                            catalog.failures.extend(next.failures);
                        }
                        // An engine with no catalog concept is not a failure to
                        // report; one that broke is worth a line.
                        Err(error) if error.code() != "UNSUPPORTED" => {
                            broken = true;
                            tracing::warn!(
                                "mizar: model.catalog from {} failed — {error}",
                                engine_id.as_str()
                            );
                        }
                        Err(_) => {}
                    }
                }
                if broken && catalog.providers.is_empty() && catalog.default.is_none() {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        "no engine could list its models",
                    );
                }
                ServerFrame::ok(id, serde_json::to_value(catalog).unwrap_or(Value::Null))
            }

            // What the operator's account is worth. The phone sends the client
            // metadata the provider wants and decodes one of four statuses; the
            // credential itself never crosses this wire, in either direction.
            "account.balance" => {
                let client = frame.payload.get("client");
                let version = client
                    .and_then(|entry| entry.get("version"))
                    .and_then(Value::as_str);
                let locale = client
                    .and_then(|entry| entry.get("locale"))
                    .and_then(Value::as_str);
                let offset = client
                    .and_then(|entry| entry.get("timezoneOffsetSeconds"))
                    .and_then(Value::as_i64);
                match (version, locale, offset) {
                    (Some(version), Some(locale), Some(offset))
                        if version.len() <= 100
                            && locale.len() <= 100
                            && offset.unsigned_abs() <= 86_400 => {}
                    _ => {
                        return ServerFrame::error(
                            Some(id),
                            ErrorCode::MalformedFrame,
                            "account.balance: invalid client metadata",
                        );
                    }
                }

                let Some(engine) = self.core.engine(EngineId::Dsh) else {
                    return ServerFrame::ok(id, json!({ "status": "unsupported" }));
                };
                match engine.balance().await {
                    Ok(Balance::Ready {
                        wallets,
                        bonus,
                        top_up_url,
                    }) => {
                        // Only the official provider's own page is offered: a
                        // configurable platform is not necessarily that platform.
                        if top_up_url.as_deref() != Some(OFFICIAL_TOP_UP) {
                            return ServerFrame::ok(id, json!({ "status": "unsupported" }));
                        }
                        let (Some(paid), Some(bonus)) =
                            (wallet_rows(&wallets, true), wallet_rows(&bonus, false))
                        else {
                            return ServerFrame::ok(id, json!({ "status": "unavailable" }));
                        };
                        ServerFrame::ok(
                            id,
                            json!({
                                "status": "ready",
                                "wallets": paid,
                                "bonusWallets": bonus,
                                "topUpURL": OFFICIAL_TOP_UP,
                            }),
                        )
                    }
                    Ok(Balance::SignedOut) => {
                        ServerFrame::ok(id, json!({ "status": "signed-out" }))
                    }
                    Ok(Balance::Unavailable) => {
                        ServerFrame::ok(id, json!({ "status": "unavailable" }))
                    }
                    // No provider at all is a capability answer; a provider that
                    // failed to answer is a transient one, and the app keeps
                    // "unsupported" for the life of a connection.
                    Ok(Balance::Unsupported) => {
                        ServerFrame::ok(id, json!({ "status": "unsupported" }))
                    }
                    Err(error) => {
                        tracing::warn!("mizar: account.balance failed — {error}");
                        ServerFrame::ok(id, json!({ "status": "unavailable" }))
                    }
                }
            }

            // What the permission picker draws. Presets are an engine's own, and
            // the switch goes to the session's engine, so the first engine that
            // answers wins rather than merging two vocabularies.
            "permissions.catalog" => {
                for engine_id in Self::visible_engines(scope) {
                    let Some(engine) = self.core.engine(engine_id) else {
                        continue;
                    };
                    if let Ok(catalog) = engine.permission_catalog().await {
                        return ServerFrame::ok(
                            id,
                            serde_json::to_value(catalog).unwrap_or(Value::Null),
                        );
                    }
                }
                ServerFrame::error(
                    Some(id),
                    ErrorCode::RemoteError,
                    "no engine could list its permission presets",
                )
            }

            // Start a session. The phone names a directory, not a product, so the
            // first engine that can take one answers.
            "session.create" => {
                let cwd = frame.payload.get("cwd").and_then(Value::as_str);
                let peer = Self::peer_key(device).is_some();
                if peer && !cwd.is_some_and(|path| self.core.remote_workspace_allowed(path)) {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::WorkspaceNotAllowed,
                        "a peer may only start work in the operator's remote workspaces",
                    );
                }
                let mut last: Option<ServerFrame> = None;
                for engine_id in Self::visible_engines(scope) {
                    let Some(engine) = self.core.engine(engine_id) else {
                        continue;
                    };
                    match engine.create(cwd).await {
                        Ok(key) => {
                            self.record_peer_session(device, &key);
                            return ServerFrame::ok(id, json!({ "sessionId": key.to_string() }));
                        }
                        Err(error) => {
                            last = Some(ServerFrame::error(
                                Some(id.clone()),
                                ErrorCode::from_engine(error.code()),
                                error.to_string(),
                            ))
                        }
                    }
                }
                last.unwrap_or_else(|| {
                    ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        "no engine can start a session",
                    )
                })
            }

            "session.archive" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.archive needs a sessionId",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        format!("no engine for {}", key.engine),
                    );
                };
                let stop = frame
                    .payload
                    .get("stopActivity")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match engine.archive(&key, stop).await {
                    Ok(()) => ServerFrame::ok(id, json!({ "archived": true })),
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            // The running turn's one-line process rows. The phone polls this a
            // couple of times a second while a turn runs, and clears the feed
            // when the host says no turn is running.
            // What the phone's home page charts: the last weeks of work, one
            // row per day that had any. The kernel already folds the engines
            // together (a date two agents worked on is one row), so this is a
            // pass-through rather than a second accumulation.
            "usage.overview" => {
                let response = self
                    .core
                    .dispatch(agi_protocol::api::Command::UsageOverview)
                    .await;
                match (response.ok, response.value, response.error) {
                    (true, Some(value), _) => ServerFrame::ok(id, value),
                    (_, _, Some(error)) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(&error.code),
                        error.message,
                    ),
                    _ => ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        "usage.overview answered nothing",
                    ),
                }
            }
            "session.activity" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.activity needs a sessionId",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        format!("no engine for {}", key.engine),
                    );
                };
                let limit = frame
                    .payload
                    .get("maxEntries")
                    .and_then(Value::as_u64)
                    .map(|value| value.clamp(1, 400) as usize)
                    .unwrap_or(120);
                let running = self
                    .core
                    .snapshot()
                    .sessions
                    .iter()
                    .find(|session| session.key == key)
                    .map(|session| session.running)
                    .unwrap_or(false);
                match engine.activity(&key, limit).await {
                    Ok(entries) => {
                        // Only the turn that is running: the feed is "what now",
                        // and a finished turn's rows were cleared the moment it
                        // ended.
                        let turn = if running {
                            entries.last().and_then(|entry| entry.turn)
                        } else {
                            None
                        };
                        let visible: Vec<_> = match turn {
                            Some(turn) => entries
                                .iter()
                                .filter(|entry| entry.turn == Some(turn))
                                .collect(),
                            None => Vec::new(),
                        };
                        let rows: Vec<Value> = visible
                            .iter()
                            .map(|entry| {
                                json!({
                                    "id": entry.id,
                                    "seq": entry.seq,
                                    "at": entry.at,
                                    "kind": entry.kind,
                                    "title": entry.title,
                                    "text": entry.text,
                                })
                            })
                            .collect();
                        ServerFrame::ok(
                            id,
                            json!({
                                "sessionId": key.to_string(),
                                "asOfSeq": Value::Null,
                                "turn": turn,
                                "entries": rows,
                                "hasMore": visible.len() >= limit,
                                "oldestSeq": visible.first().map(|entry| entry.seq),
                            }),
                        )
                    }
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            // The phone's preview page: what a path is, then the bytes a slice
            // at a time. The fence lives in the engine, shared by all three
            // adapters, and the code it refuses with is passed through so the
            // app can tell "not yours" from "not there".
            "file.stat" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "file.stat needs a sessionId",
                    );
                };
                let Some(path) = frame.payload.get("path").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "file.stat needs a path",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        format!("no engine for {}", key.engine),
                    );
                };
                match engine.file_stat(&key, path).await {
                    Ok(stat) => ServerFrame::ok(
                        id,
                        json!({
                            "path": stat.path,
                            "name": stat.name,
                            "byteSize": stat.byte_size,
                            "modifiedAt": stat.modified_at,
                            "mediaType": stat.media_type,
                            "previewable": stat.previewable,
                        }),
                    ),
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            "file.read" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "file.read needs a sessionId",
                    );
                };
                let Some(path) = frame.payload.get("path").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "file.read needs a path",
                    );
                };
                let offset = match frame.payload.get("offset") {
                    None | Some(Value::Null) => 0,
                    Some(value) => match value.as_u64() {
                        Some(offset) => offset,
                        None => {
                            return ServerFrame::error(
                                Some(id),
                                ErrorCode::MalformedFrame,
                                "file.read: offset must be a non-negative integer",
                            );
                        }
                    },
                };
                let length = frame
                    .payload
                    .get("length")
                    .and_then(Value::as_u64)
                    .unwrap_or(MAX_SLICE_BYTES)
                    .clamp(1, MAX_SLICE_BYTES);
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        format!("no engine for {}", key.engine),
                    );
                };
                match engine.file_slice(&key, path, offset, length).await {
                    Ok(slice) => ServerFrame::ok(
                        id,
                        json!({
                            "offset": slice.offset,
                            "length": slice.length,
                            "byteSize": slice.byte_size,
                            "data": crate::relay::base64(&slice.data),
                            "eof": slice.eof,
                        }),
                    ),
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            // Branch a conversation at one completed turn boundary. The phone
            // only offers boundaries it read from the transcript; the host still
            // refuses anything that is not a turn end.
            "session.fork" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.fork needs a sessionId",
                    );
                };
                let Some(at) = frame.payload.get("atSeq").and_then(Value::as_i64) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.fork needs an atSeq",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        format!("no engine for {}", key.engine),
                    );
                };
                match engine.fork(&key, at).await {
                    Ok(forked) => ServerFrame::ok(id, json!({ "sessionId": forked.to_string() })),
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            // Move the tunnel between the two public relays. The phone asks
            // because its account's tier decides which relay it belongs on.
            "relay.set" => {
                let Some(url) = frame.payload.get("url").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "relay.set needs a url",
                    );
                };
                match self.switch_relay(url) {
                    Ok(facts) => {
                        // Everybody else on this host is told where it went, so
                        // a phone redials instead of finding out on next failure.
                        let _ = self.notices.send(facts.clone());
                        ServerFrame::ok(id, facts)
                    }
                    Err(message) => {
                        ServerFrame::error(Some(id), ErrorCode::MalformedFrame, message)
                    }
                }
            }

            // Attachments arrive in four calls: where to put them, the bytes a
            // chunk at a time, a record a prompt can name, or a clean abort.
            "upload.begin" => {
                let Some(session) = frame
                    .payload
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "upload.begin needs a sessionId",
                    );
                };
                let Some(kind) = frame
                    .payload
                    .get("kind")
                    .and_then(Value::as_str)
                    .and_then(AttachmentKind::parse)
                else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "upload.begin needs a kind of image or file",
                    );
                };
                let Some(name) = frame.payload.get("name").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "upload.begin needs a name",
                    );
                };
                let Some(byte_size) = frame.payload.get("byteSize").and_then(Value::as_u64) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "upload.begin needs a byteSize",
                    );
                };
                let media_type = frame
                    .payload
                    .get("mediaType")
                    .and_then(Value::as_str)
                    .unwrap_or("application/octet-stream");
                match self.uploads.begin(
                    &device.id,
                    crate::uploads::BeginRequest {
                        session,
                        kind,
                        name: name.to_string(),
                        media_type: media_type.to_string(),
                        byte_size,
                    },
                ) {
                    Ok(begin) => ServerFrame::ok(
                        id,
                        json!({ "uploadId": begin.upload_id, "chunkBytes": begin.chunk_bytes }),
                    ),
                    Err(error) => upload_error(Some(id), error),
                }
            }

            "upload.chunk" => {
                let Some(upload_id) = frame.payload.get("uploadId").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "upload.chunk needs an uploadId",
                    );
                };
                let Some(index) = frame.payload.get("index").and_then(Value::as_u64) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "upload.chunk needs an index",
                    );
                };
                let Some(data) = frame.payload.get("data").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "upload.chunk needs data",
                    );
                };
                let Some(bytes) = crate::relay::unbase64(data) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "upload.chunk: data is not base64",
                    );
                };
                match self.uploads.chunk(&device.id, upload_id, index, &bytes) {
                    Ok(received) => ServerFrame::ok(id, json!({ "received": received })),
                    Err(error) => upload_error(Some(id), error),
                }
            }

            "upload.finish" => {
                let Some(upload_id) = frame.payload.get("uploadId").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "upload.finish needs an uploadId",
                    );
                };
                match self.uploads.finish(&device.id, upload_id) {
                    Ok(record) => {
                        ServerFrame::ok(id, serde_json::to_value(record).unwrap_or(Value::Null))
                    }
                    Err(error) => upload_error(Some(id), error),
                }
            }

            "upload.abort" => {
                let Some(upload_id) = frame.payload.get("uploadId").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "upload.abort needs an uploadId",
                    );
                };
                // Saying "aborted" for an id that was never ours is how a client
                // comes to believe it cleaned something up.
                if self.uploads.abort(&device.id, upload_id) {
                    ServerFrame::ok(id, json!({ "aborted": true }))
                } else {
                    ServerFrame::error(
                        Some(id),
                        ErrorCode::UploadUnknown,
                        "upload.abort: no such upload",
                    )
                }
            }

            // The limits, for a client that would rather ask than assume.
            "upload.capabilities" => ServerFrame::ok(
                id,
                json!({
                    "chunkBytes": crate::uploads::CHUNK_BYTES,
                    "imageBytes": crate::uploads::IMAGE_BYTES,
                    "fileBytes": crate::uploads::FILE_BYTES,
                    "promptBytes": crate::uploads::PROMPT_BYTES,
                    "attachments": crate::uploads::PROMPT_ATTACHMENTS,
                    "images": crate::uploads::PROMPT_ATTACHMENTS,
                    "expiresSeconds": crate::uploads::UPLOAD_TTL_MS / 1000,
                    "imageMediaTypes": crate::uploads::IMAGE_MEDIA_TYPES,
                    "sha256": false,
                }),
            ),

            "session.prompt" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.prompt needs a sessionId",
                    );
                };
                let text = frame
                    .payload
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let attachment_ids: Vec<String> = frame
                    .payload
                    .get("attachmentIds")
                    .and_then(Value::as_array)
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|row| row.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if text.trim().is_empty() && attachment_ids.is_empty() {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.prompt needs text or an attachment",
                    );
                }
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(Some(id), ErrorCode::RemoteError, "no such engine");
                };
                // Resolved before the engine sees them: ownership, session and
                // expiry are checked here, and the record is spent by this call.
                let attachments = if attachment_ids.is_empty() {
                    Vec::new()
                } else {
                    match self.uploads.resolve_for_prompt(
                        &device.id,
                        &key.to_string(),
                        &attachment_ids,
                    ) {
                        Ok(attachments) => attachments,
                        Err(error) => return upload_error(Some(id), error),
                    }
                };
                if !attachments.is_empty() && !engine.supports_attachments() {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::AttachmentUnsupported,
                        "this engine cannot carry an attachment",
                    );
                }
                let request_id = frame
                    .payload
                    .get("requestId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                match engine.prompt_with(&key, text, &attachments).await {
                    Ok(()) => {
                        // Only now is the attachment spent: a refusal above leaves
                        // it in place for the retry the client is allowed to make.
                        if !attachment_ids.is_empty() {
                            self.uploads
                                .consume(&device.id, &key.to_string(), &attachment_ids);
                        }
                        ServerFrame::ok(id, json!({ "accepted": true, "requestId": request_id }))
                    }
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            "session.cancel" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.cancel needs a sessionId",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(Some(id), ErrorCode::RemoteError, "no such engine");
                };
                match engine.cancel(&key).await {
                    Ok(()) => ServerFrame::ok(id, json!({ "accepted": true })),
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            "session.compact" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.compact needs a sessionId",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(Some(id), ErrorCode::RemoteError, "no such engine");
                };
                match engine.compact(&key).await {
                    Ok(()) => ServerFrame::ok(id, json!({ "compacted": true })),
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            "interactions.pending" => {
                let snapshot = self.core.snapshot();
                // Only the approvals and questions of the agent this phone
                // asked for. An approval it cannot act on is not harmless: it
                // is a decision waiting for somebody who is looking at a
                // different agent's work, and this was the second thing that
                // leaked after the session list was filtered.
                let visible = |interaction: &agi_protocol::domain::Interaction| {
                    let Some(key) = interaction.session.as_ref() else {
                        // A peer cannot answer an interaction that names no
                        // session; a phone keeps the old unscoped allowance.
                        return Self::peer_key(device).is_none();
                    };
                    Self::in_scope(scope, key) && self.peer_owns(device, key)
                };
                let approvals: Vec<Value> = snapshot
                    .interactions
                    .iter()
                    .filter(|interaction| visible(interaction))
                    .filter(|interaction| interaction.kind == InteractionKind::Approval)
                    .map(|interaction| {
                        json!({
                            "requestId": interaction.id,
                            "sessionId": interaction.session.as_ref().map(|key| key.to_string()),
                            "toolName": interaction.title,
                            "callId": Value::Null,
                            "reason": interaction.detail,
                            "displayReason": Value::Null,
                            "requestedAt": interaction.requested_at,
                            "risk": approval_risk(interaction).0,
                            "riskReason": approval_risk(interaction).1,
                        })
                    })
                    .collect();
                let questions: Vec<Value> = snapshot
                    .interactions
                    .iter()
                    .filter(|interaction| visible(interaction))
                    .filter(|interaction| interaction.kind == InteractionKind::Question)
                    .map(|interaction| {
                        json!({
                            "callId": interaction.id,
                            "sessionId": interaction.session.as_ref().map(|key| key.to_string()),
                            "requestedAt": interaction.requested_at,
                            "questions": interaction
                                .questions
                                .iter()
                                .map(|question| json!({
                                    "id": question.id,
                                    "question": question.text,
                                    "header": question.header,
                                    "detail": question.detail,
                                    "multiSelect": question.multi_select,
                                    "options": question
                                        .options
                                        .iter()
                                        .map(|label| json!({ "label": label }))
                                        .collect::<Vec<_>>(),
                                }))
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                ServerFrame::ok(
                    id,
                    json!({ "approvals": approvals, "questions": questions }),
                )
            }

            "approval.answer" => {
                let Some(request_id) = frame.payload.get("requestId").and_then(Value::as_str)
                else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "approval.answer needs requestId",
                    );
                };
                let outcome = frame
                    .payload
                    .get("outcome")
                    .and_then(Value::as_str)
                    .unwrap_or("rejected")
                    .to_string();

                if self.answers_another_agent(device, scope, request_id) {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        "no such approval on this connection",
                    );
                }

                // The kernel already maps an interaction id to the engine that
                // asked; resolving it here would be a second copy of that table.
                let command = agi_protocol::api::Command::Answer {
                    id: request_id.to_string(),
                    outcome: Some(outcome),
                    answers: None,
                };
                let response = self.core.dispatch(command).await;
                match (response.ok, response.error) {
                    (true, _) => ServerFrame::ok(id, json!({ "accepted": true })),
                    (false, Some(error)) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(&error.code),
                        error.message,
                    ),
                    _ => ServerFrame::error(Some(id), ErrorCode::Internal, "the answer failed"),
                }
            }

            "question.answer" => {
                let Some(call_id) = frame.payload.get("callId").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "question.answer needs callId",
                    );
                };
                let answers = frame
                    .payload
                    .get("answers")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if self.answers_another_agent(device, scope, call_id) {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::RemoteError,
                        "no such question on this connection",
                    );
                }
                let command = agi_protocol::api::Command::Answer {
                    id: call_id.to_string(),
                    outcome: None,
                    answers: Some(answers),
                };
                let response = self.core.dispatch(command).await;
                match (response.ok, response.error) {
                    (true, _) => ServerFrame::ok(id, json!({ "accepted": true })),
                    (false, Some(error)) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(&error.code),
                        error.message,
                    ),
                    _ => ServerFrame::error(Some(id), ErrorCode::Internal, "the answer failed"),
                }
            }

            "session.model.set" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.model.set needs a sessionId",
                    );
                };
                let (Some(provider), Some(model)) = (
                    frame.payload.get("provider").and_then(Value::as_str),
                    frame.payload.get("model").and_then(Value::as_str),
                ) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.model.set needs provider and model",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(Some(id), ErrorCode::RemoteError, "no such engine");
                };
                // DSH selects by provider *and* model; Codex and Claude Code
                // take the model alone. The phone always sends both, so the
                // join happens here rather than in three adapters.
                let name = if key.engine == agi_protocol::engine::EngineId::Dsh {
                    format!("{provider}/{model}")
                } else {
                    model.to_string()
                };
                match engine.set_model(&key, &name).await {
                    Ok(()) => ServerFrame::ok(
                        id,
                        json!({
                            "sessionId": key.to_string(),
                            "selected": {
                                "provider": provider,
                                "model": model,
                                "reasoningEffort": frame.payload.get("reasoningEffort").cloned(),
                            }
                        }),
                    ),
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            "session.permission.set" => {
                let Some(key) = self.scoped_session_of(device, scope, frame) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.permission.set needs a sessionId",
                    );
                };
                let Some(preset) = frame.payload.get("preset").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "session.permission.set needs preset",
                    );
                };
                let Some(engine) = self.core.engine(key.engine) else {
                    return ServerFrame::error(Some(id), ErrorCode::RemoteError, "no such engine");
                };
                match engine.set_permission(&key, preset).await {
                    Ok(()) => {
                        ServerFrame::ok(id, json!({ "commandResult": { "kind": "success" } }))
                    }
                    Err(error) => ServerFrame::error(
                        Some(id),
                        ErrorCode::from_engine(error.code()),
                        error.to_string(),
                    ),
                }
            }

            // Where this phone's notifications go. The plugin cannot talk to
            // Apple — the signing key lives on the relay — so its job is to tell
            // the relay what happened and who this device is.
            "push.register" => {
                let Some(token) = frame.payload.get("token").and_then(Value::as_str) else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "push.register needs a token",
                    );
                };
                let mut body = json!({
                    "deviceId": device.id,
                    "token": token,
                    "platform": frame.payload.get("platform").and_then(Value::as_str).unwrap_or("ios"),
                });
                for field in [
                    "environment",
                    "locale",
                    "liveActivityToken",
                    "liveActivityStartToken",
                ] {
                    if let Some(value) = frame.payload.get(field) {
                        body[field] = value.clone();
                    }
                }
                match self.relay_post("/push/v1/register", body).await {
                    Ok(answer) => {
                        let registered = answer.get("ok").and_then(Value::as_bool).unwrap_or(false);
                        if registered {
                            // The phone may have registered after a session
                            // started. A start that was sent before this token
                            // existed is gone; publish the current card now so
                            // the relay can use the start token immediately.
                            self.publish_current_card().await;
                        }
                        ServerFrame::ok(id, json!({ "registered": registered }))
                    }
                    Err(error) => {
                        tracing::warn!("mizar: push.register was not carried — {error}");
                        ServerFrame::ok(id, json!({ "registered": false }))
                    }
                }
            }

            "push.unregister" => {
                match self
                    .relay_post("/push/v1/unregister", json!({ "deviceId": device.id }))
                    .await
                {
                    Ok(answer) => ServerFrame::ok(
                        id,
                        json!({ "unregistered": answer.get("ok").and_then(Value::as_bool).unwrap_or(false) }),
                    ),
                    Err(error) => {
                        tracing::warn!("mizar: push.unregister was not carried — {error}");
                        ServerFrame::ok(id, json!({ "unregistered": false }))
                    }
                }
            }

            // Host-local bookkeeping: a phone that is on screen sees the
            // approval already, so it is left out of the wake-up. This never
            // reaches the relay.
            "push.presence" => {
                let Some(foreground) = frame.payload.get("foreground").and_then(Value::as_bool)
                else {
                    return ServerFrame::error(
                        Some(id),
                        ErrorCode::MalformedFrame,
                        "push.presence needs a foreground flag",
                    );
                };
                {
                    let mut known = self
                        .foreground
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if foreground {
                        known.insert(device.id.clone());
                    } else {
                        known.remove(&device.id);
                    }
                }
                ServerFrame::ok(id, json!({ "accepted": true }))
            }

            // Everything else is a method this build has not grown yet. Saying so
            // is what lets the phone tell "older host" from "broken host".
            other => ServerFrame::error(
                Some(id),
                ErrorCode::NotImplemented,
                format!("{other} is not implemented by this host"),
            ),
        }
    }

    /// POST to the relay's API with this tunnel's credential.
    ///
    /// The credential is read out of the lock before the await: holding a
    /// `std::sync::Mutex` across one is how a server stops answering.
    async fn relay_post(&self, path: &str, body: Value) -> Result<Value, String> {
        let (base, token) = {
            let guard = self
                .relay
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(relay) = guard.as_ref() else {
                return Err("this host has no tunnel".to_string());
            };
            (relay.base().to_string(), relay.token().to_string())
        };
        let url = format!("{}{path}", base.trim_end_matches('/'));
        let response = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("the relay answered {}", response.status()));
        }
        response
            .json::<Value>()
            .await
            .map_err(|error| error.to_string())
    }

    /// Wake the phones that are not looking at an approval or a question.
    ///
    /// A device that has said it is on screen is left out: it already has the
    /// card. Everything else on this host gets one notification per interaction,
    /// addressed to the session that is blocked.
    async fn notify_interaction(&self, interaction: &agi_protocol::domain::Interaction) {
        let (kind, title, body) = match interaction.kind {
            InteractionKind::Approval => (
                "approval",
                "Approval needed".to_string(),
                interaction.title.clone(),
            ),
            _ => {
                let question = interaction
                    .questions
                    .first()
                    .map(|question| {
                        question
                            .options
                            .first()
                            .map(|_| question.text.clone())
                            .unwrap_or_else(|| question.text.clone())
                    })
                    .unwrap_or_else(|| interaction.title.clone());
                (
                    "question",
                    "The agent asked a question".to_string(),
                    question,
                )
            }
        };
        let except: Vec<String> = {
            let known = self
                .foreground
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            known.iter().cloned().collect()
        };
        let session = interaction.session.as_ref().map(|key| key.to_string());
        let body = json!({
            "kind": kind,
            "title": title,
            "body": body,
            "sessionId": session,
            "threadId": session,
            "timeSensitive": true,
            "exceptDeviceIds": except,
        });
        if let Err(error) = self.relay_post("/push/v1/notify", body).await {
            // A phone that cannot be woken is a worse app, not a broken host.
            tracing::debug!("mizar: no notification for {kind} — {error}");
        }
    }

    /// Publish the card for whatever is running right now.
    ///
    /// This is the resynchronisation path for a phone that registered its push
    /// address after a session had already started: the original start push
    /// happened before the relay could address this install.
    async fn publish_current_card(&self) -> bool {
        let snapshot = self.core.snapshot();
        let mut rows: Vec<crate::activity::Row> = snapshot
            .sessions
            .iter()
            .filter(|session| session.running)
            .map(|session| crate::activity::Row {
                session_id: session.key.to_string(),
                title: session.title.clone().unwrap_or_default(),
                running_since: session.updated_at,
            })
            .collect();
        rows.sort_by_key(|row| row.running_since);
        rows.truncate(crate::activity::MAX_ROWS);
        if rows.is_empty() {
            return false;
        }
        let approvals = snapshot
            .interactions
            .iter()
            .filter(|interaction| {
                interaction.kind == agi_protocol::domain::InteractionKind::Approval
            })
            .map(|interaction| crate::activity::Approval {
                request_id: interaction.id.clone(),
                session_id: interaction
                    .session
                    .as_ref()
                    .map(|key| key.to_string())
                    .unwrap_or_default(),
                tool: interaction.title.clone(),
                detail: interaction.detail.clone().unwrap_or_default(),
            })
            .collect();
        self.publish_card(&crate::activity::Card { rows, approvals })
            .await
    }

    /// Publish what the island should say.
    ///
    /// Always `phase: "start"`, which the relay turns into an update on a phone
    /// that already has a card. That is what lets this computer stay ignorant of
    /// which phones are showing one -- the relay already knows, because a card
    /// reports its own token.
    async fn publish_card(&self, card: &crate::activity::Card) -> bool {
        // An empty card is not "a card with no rows": it is the end of the
        // activity, and `publish_turn_end` is the call that ends it.
        if card.is_empty() {
            return false;
        }
        // Do not exclude a device just because presence says it is on screen.
        // That report is asynchronous and can be stale after the app is
        // suspended; excluding it is how a session started on the desktop never
        // gets a card on the phone. The relay turns a start into an update when
        // the phone has already reported its own card, so including every device
        // is also what prevents a second card rather than what causes one.
        let body = crate::activity::activity_body(card, &[]);
        match self.relay_post("/push/v1/activity", body).await {
            Ok(answer) => {
                // `sent` is the relay's APNs tally. A start whose transport
                // failed is not a card; keep it eligible for the activity
                // loop's periodic retry rather than waiting for the next turn.
                answer.get("sent").and_then(Value::as_u64).unwrap_or(0) > 0
            }
            Err(error) => {
                // A phone that cannot be reached is a worse app, not a broken host.
                tracing::debug!("mizar: no live activity update — {error}");
                false
            }
        }
    }

    /// Take a session off the card, and say the turn finished.
    ///
    /// The two facts are one call when the card is now empty: `endLiveActivity`
    /// is what brings the island down, and a separate request would be a second
    /// chance to deliver half of a pair that is only true together.
    ///
    /// - Parameters:
    ///   - tracker: the card being edited.
    ///   - session: the session whose turn ended.
    ///   - tell: whether to send the notification at all.
    async fn finish_card(
        &self,
        tracker: &mut crate::activity::Tracker,
        session: &SessionKey,
        tell: bool,
    ) {
        let title = tracker.title_of(session).unwrap_or_default();
        let Some(card) = tracker.finished(session) else {
            return;
        };
        if card.is_empty() {
            if tell {
                self.publish_turn_end(session, &title, true).await;
            }
            return;
        }
        self.publish_card(&card).await;
        if tell {
            self.publish_turn_end(session, &title, false).await;
        }
    }

    /// One turn-end notification, and the end of the card when nothing is left.
    async fn publish_turn_end(&self, session: &SessionKey, title: &str, end: bool) {
        // A foreground phone draws its own card, but the presence report can be
        // stale: iOS suspends the process between the scene change and the
        // `push.presence` call. Excluding such a phone from the **end** push is
        // how a Live Activity stays on the Lock Screen until the app is opened.
        // The end frame must reach every device; only a normal turn-end alert is
        // suppressed for devices that are already looking at the screen.
        let except = if end {
            Vec::new()
        } else {
            self.foreground_devices()
        };
        let body = crate::activity::turn_end_body(title, session, end, &except);
        if let Err(error) = self.relay_post("/push/v1/notify", body).await {
            tracing::debug!("mizar: no turn-end notification — {error}");
        }
    }

    /// The devices whose app is on screen.
    ///
    /// They draw their own card from their own state, so a start sent to one of
    /// them is a second card on the Lock Screen. This is the list that keeps
    /// that from happening.
    fn foreground_devices(&self) -> Vec<String> {
        self.foreground
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// The session a frame names, if it names a valid one.
    fn session_of(&self, frame: &ClientFrame) -> Option<SessionKey> {
        frame
            .payload
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(|text| text.parse().ok())
    }

    /// Whether an interaction belongs to an agent this connection cannot see.
    ///
    /// An id this host does not know answers `false`: it may be one the phone
    /// is answering late, and the kernel's own "no such request" is the right
    /// answer for it. Only a request that exists *and* is another agent's is
    /// refused here.
    fn answers_another_agent(
        &self,
        device: &crate::store::Device,
        scope: Option<agi_protocol::engine::EngineId>,
        id: &str,
    ) -> bool {
        self.core
            .snapshot()
            .interactions
            .iter()
            .find(|interaction| interaction.id == id)
            .is_some_and(|interaction| match interaction.session.as_ref() {
                Some(key) => !Self::in_scope(scope, key) || !self.peer_owns(device, key),
                None => Self::peer_key(device).is_some(),
            })
    }

    /// The session this frame names, when this connection may see it.
    ///
    /// The scope is checked here rather than in each arm because it has to hold
    /// for all of them: a phone that asked for DSH must not reach a Claude
    /// transcript by naming its id, and the id is guessable enough (a date and a
    /// slug) that "the list did not show it" is not a boundary.
    ///
    /// Out of scope answers exactly like a session that does not exist. A
    /// refusal that distinguishes the two would let a phone enumerate the other
    /// agents this machine is running, which is the half of the promise the
    /// filter is for.
    fn scoped_session_of(
        &self,
        device: &crate::store::Device,
        scope: Option<agi_protocol::engine::EngineId>,
        frame: &ClientFrame,
    ) -> Option<SessionKey> {
        let key = self.session_of(frame)?;
        (Self::in_scope(scope, &key) && self.peer_owns(device, &key)).then_some(key)
    }

    /// The stable peer identity a device marker carries, if it is a peer.
    fn peer_key(device: &crate::store::Device) -> Option<String> {
        crate::relay::peer_identity_of_marker(device.model.as_deref()?)
    }

    /// Whether this device may touch one session.
    ///
    /// Phones keep the old engine-scope rule; a peer is additionally confined to
    /// the sessions it created.
    fn peer_owns(&self, device: &crate::store::Device, key: &SessionKey) -> bool {
        let Some(peer) = Self::peer_key(device) else {
            return true;
        };
        self.peer_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&peer)
            .is_some_and(|sessions| sessions.contains(&key.to_string()))
    }

    /// Remember one session a peer created.
    pub fn load_peer_sessions(&self, path: std::path::PathBuf) {
        let sessions = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| {
                serde_json::from_str::<std::collections::HashMap<String, Vec<String>>>(&text).ok()
            })
            .unwrap_or_default()
            .into_iter()
            .map(|(peer, sessions)| (peer, sessions.into_iter().collect()))
            .collect();
        *self
            .peer_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = sessions;
        *self
            .peer_sessions_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
    }

    fn record_peer_session(&self, device: &crate::store::Device, key: &SessionKey) {
        let Some(peer) = Self::peer_key(device) else {
            return;
        };
        self.peer_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(peer)
            .or_default()
            .insert(key.to_string());
        self.save_peer_sessions();
    }

    fn save_peer_sessions(&self) {
        let Some(path) = self
            .peer_sessions_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        let sessions: std::collections::HashMap<String, Vec<String>> = self
            .peer_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(peer, sessions)| (peer.clone(), sessions.iter().cloned().collect()))
            .collect();
        let Ok(bytes) = serde_json::to_vec_pretty(&sessions) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let temp = path.with_extension("json.tmp");
        if std::fs::write(&temp, &bytes).is_err() {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::rename(&temp, &path);
    }
}

/// Membership spelled the way the session list spells ids.
///
/// The registry stores bare `session-…` ids; every id this host sends the phone
/// carries its engine prefix. A workspace list that skipped the prefix made the
/// phone's set intersection empty, so every session was drawn as unassigned.
fn membership(engine: EngineId, ids: Option<Vec<String>>) -> Option<Vec<String>> {
    let ids = ids?;
    let prefix = format!("{}:", engine.as_str());
    Some(
        ids.into_iter()
            .map(|session| {
                if session.starts_with(&prefix) {
                    session
                } else {
                    format!("{prefix}{session}")
                }
            })
            .collect(),
    )
}

/// The error frame for one refused upload call.
fn upload_error(id: Option<String>, error: crate::uploads::UploadError) -> ServerFrame {
    let code = match error.code {
        "UPLOAD_UNKNOWN" => ErrorCode::UploadUnknown,
        "UPLOAD_BAD_CHUNK" => ErrorCode::UploadBadChunk,
        "UPLOAD_TOO_LARGE" | "UPLOAD_LIMITED" => ErrorCode::UploadTooLarge,
        "UPLOAD_INCOMPLETE" => ErrorCode::UploadIncomplete,
        "ATTACHMENT_UNKNOWN" => ErrorCode::AttachmentUnknown,
        "ATTACHMENT_TOO_LARGE" => ErrorCode::AttachmentTooLarge,
        "ATTACHMENT_UNSUPPORTED" => ErrorCode::AttachmentUnsupported,
        "ATTACHMENT_FAILED" | "UPLOAD_FAILED" => ErrorCode::AttachmentFailed,
        _ => ErrorCode::MalformedFrame,
    };
    ServerFrame::error(id, code, error.message)
}

/// Open the staging area beside the device roster.
fn upload_store() -> crate::uploads::UploadStore {
    let root = crate::store::default_store_path()
        .parent()
        .map(|home| home.join("uploads"))
        .unwrap_or_else(|| std::env::temp_dir().join("agi-mizar-uploads"));
    crate::uploads::UploadStore::open(root).unwrap_or_else(|_| {
        crate::uploads::UploadStore::open(std::env::temp_dir().join("agi-mizar-uploads"))
            .expect("the staging area has to exist somewhere")
    })
}

/// The provider's own top-up page, and the only one this host will name.
const OFFICIAL_TOP_UP: &str = "https://platform.deepseek.com/top_up";

/// Whether a string is a monetary amount the app will also accept.
///
/// The grammar is the app's own — `-?digits[.digits]` or `-.digits`, with an
/// optional exponent — because a value this side calls money and the app
/// refuses becomes a failed balance card rather than an unavailable one. In
/// particular `+1` and a padded `" 1 "` are not amounts.
fn is_money(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > 256 {
        return false;
    }
    let mut index = 0;
    if bytes[index] == b'-' {
        index += 1;
    }
    let integer_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let integer_digits = index - integer_start;

    let mut fractional_digits = 0;
    if index < bytes.len() && bytes[index] == b'.' {
        index += 1;
        let fraction_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        fractional_digits = index - fraction_start;
    }
    if integer_digits == 0 && fractional_digits == 0 {
        return false;
    }

    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        index += 1;
        if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
            index += 1;
        }
        let exponent_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == exponent_start {
            return false;
        }
    }
    index == bytes.len()
}

/// The wallets in the shape the app decodes, or `None` when the provider sent
/// something the app must not draw.
///
/// The rules are the plugin's: only the two currencies the build knows, no
/// duplicates, at least one paid wallet, and amounts short enough and numeric
/// enough to be money rather than prose.
fn wallet_rows(wallets: &[Wallet], required: bool) -> Option<Vec<Value>> {
    if required && wallets.is_empty() {
        return None;
    }
    let mut seen: Vec<&str> = Vec::new();
    let mut rows = Vec::new();
    for wallet in wallets {
        if !matches!(wallet.currency.as_str(), "CNY" | "USD") {
            return None;
        }
        if seen.contains(&wallet.currency.as_str()) {
            return None;
        }
        let amount = wallet.amount.as_str();
        if !is_money(amount) {
            return None;
        }
        seen.push(wallet.currency.as_str());
        rows.push(json!({ "currency": wallet.currency, "balance": amount }));
    }
    Some(rows)
}

/// `GET /mizar/v1/hello` — unauthenticated, reveals identity only.
/// What a peer is allowed to learn before it has authenticated.
///
/// A disabled host reports the switch, not the catalogue: the workspace paths
/// are the operator's projects, and `hello` is unauthenticated.
fn remote_view(prefs: &agi_core::RemotePrefs) -> Value {
    if !prefs.enabled {
        return json!({ "enabled": false, "allowLan": false, "allowWan": false, "workspaces": [] });
    }
    json!({
        "enabled": true,
        "allowLan": prefs.allow_lan,
        "allowWan": prefs.allow_wan,
        "workspaces": prefs.workspaces,
    })
}

async fn hello(
    State(server): State<Arc<MizarServer>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    tracing::info!("mizar: hello from {peer}");
    // Which agents this computer serves. Asked before a token exists, because
    // the choice is made before pairing: a phone that has just scanned a code
    // should be able to ask "which agent?" without opening a socket first, and
    // one that paired long ago can ask the same question while the computer is
    // reachable but its token is not being used yet.
    let engines: Vec<Value> = EngineId::all()
        .into_iter()
        .filter_map(|engine_id| {
            let engine = server.core.engine(engine_id)?;
            let (connected, detail) = engine.status();
            Some(json!({
                "id": engine_id.as_str(),
                "label": engine_id.label(),
                "connected": connected,
                "detail": detail,
            }))
        })
        .collect();
    Json(json!({
        "hostId": server.store.host_id(),
        "hostName": server.store.host_name(),
        "home": server.home,
        "protocolVersion": PROTOCOL_VERSION,
        // The app's identity probe decodes these two, and a host that omits
        // them reads as unreachable to its route planner.
        "pairingOpen": server.code().is_some(),
        "paired": !server.store.devices().is_empty(),
        "engines": engines,
        "remote": remote_view(&server.core.remote_prefs()),
    }))
    .into_response()
}

/// `POST /mizar/v1/pair` — a code for a token, once.
async fn pair(
    State(server): State<Arc<MizarServer>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<Value>,
) -> Response {
    tracing::info!("mizar: a phone at {peer} is trying to pair");
    let version = body
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .unwrap_or(PROTOCOL_VERSION as u64);
    if version != PROTOCOL_VERSION as u64 {
        return frame_response(ServerFrame::error(
            None,
            ErrorCode::ProtocolVersionMismatch,
            format!("this host speaks protocol {PROTOCOL_VERSION}"),
        ));
    }

    if !server.allow_attempt() {
        return frame_response(ServerFrame::error(
            None,
            ErrorCode::RateLimited,
            "too many pairing attempts; wait a minute",
        ));
    }

    let Some(supplied) = body.get("code").and_then(Value::as_str) else {
        return frame_response(ServerFrame::error(
            None,
            ErrorCode::PairingCodeInvalid,
            "no code supplied",
        ));
    };
    let Some(canonical) = pairing::normalise(supplied) else {
        return frame_response(ServerFrame::error(
            None,
            ErrorCode::PairingCodeInvalid,
            "that is not a pairing code",
        ));
    };

    let accepted = {
        let mut slot = server.code();
        match slot.as_ref() {
            None => Err(ErrorCode::PairingCodeUnknown),
            Some(active) if !pairing::constant_time_eq(&active.code.canonical, &canonical) => {
                Err(ErrorCode::PairingCodeUnknown)
            }
            Some(active) if pairing::now_ms() > active.code.expires_at => {
                // Expired codes are cleared: keeping them alive would let a
                // retry succeed after the operator has been told to reissue.
                *slot = None;
                Err(ErrorCode::PairingCodeExpired)
            }
            Some(_) => {
                // Single use: consuming it here means two phones racing for the
                // same code cannot both win.
                *slot = None;
                Ok(())
            }
        }
    };

    if let Err(code) = accepted {
        tracing::warn!("mizar: pairing refused for {peer} — {code:?}");
        return frame_response(ServerFrame::error(None, code, code_message(code)));
    }

    let name = body
        .get("deviceName")
        .and_then(Value::as_str)
        .unwrap_or("Phone");
    let model = body
        .get("deviceModel")
        .and_then(Value::as_str)
        .map(str::to_string);

    match server.store.issue(name, model) {
        Ok(issued) => {
            tracing::info!("mizar: paired {} from {peer}", issued.device.name);
            Json(json!({
                "deviceToken": issued.token,
                "deviceId": issued.device.id,
                "hostId": server.store.host_id(),
                "hostName": server.store.host_name(),
                "protocolVersion": PROTOCOL_VERSION,
            }))
            .into_response()
        }
        Err(error) => frame_response(ServerFrame::error(
            None,
            ErrorCode::Internal,
            format!("could not record the device: {error}"),
        )),
    }
}

/// The message that goes with a pairing failure code.
fn code_message(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::PairingCodeUnknown => "that code is not the one this computer is showing",
        ErrorCode::PairingCodeExpired => "that code has expired; ask the computer for a new one",
        _ => "that code cannot be used",
    }
}

/// `GET /mizar/v1/ws` — the phone's socket.
async fn upgrade(
    State(server): State<Arc<MizarServer>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> Response {
    tracing::info!("mizar: a socket is being opened by {peer}");
    ws.on_upgrade(move |socket| serve_socket(server, peer, socket))
}

/// One connection's two halves, whatever is carrying them.
///
/// A phone reaches this server over a WebSocket — on the LAN, or bridged through
/// the relay's room — or over a punched UDP path. The frames are identical, so
/// everything below the transport is written against this pair of channels and
/// never against axum's `WebSocket`.
struct Peer {
    inbound: tokio::sync::mpsc::Receiver<String>,
    outbound: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// `GET /mizar-net/signal` — where a punch begins.
///
/// The phone announces itself with its endpoint key, its candidates and the
/// device token the Mizar handshake uses. This host stays silent until the token
/// verifies: the endpoint sits on the machine's whole network, and a stranger
/// must not be told where this computer is.
async fn signal(
    State(server): State<Arc<MizarServer>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| serve_signal(server, peer, socket))
}

/// The signaling conversation, once the socket is upgraded.
async fn serve_signal(server: Arc<MizarServer>, peer: SocketAddr, mut socket: WebSocket) {
    let Some(endpoint_id) = server.net_endpoint_id() else {
        // A host with no direct path has nothing to say. Closing is the honest
        // answer: the phone falls back to the route it already had.
        let _ = socket.send(Message::Close(None)).await;
        return;
    };

    // The first frame must be the phone's hello, and it must carry a token this
    // host issued.
    let Some(Ok(Message::Text(first))) = socket.recv().await else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(&first) else {
        tracing::debug!("p2p: a signal channel from {peer} sent something that is not JSON");
        return;
    };
    if value.get("type").and_then(Value::as_str) != Some("hello") {
        tracing::debug!("p2p: a signal channel from {peer} did not start with a hello");
        return;
    }
    let Some(token) = value.get("token").and_then(Value::as_str) else {
        tracing::debug!("p2p: a signal channel from {peer} carried no device token");
        return;
    };
    if server.store.authenticate(token).is_none() {
        tracing::warn!(
            "p2p: a signal channel from {peer} presented a token this host does not know"
        );
        return;
    }

    let answer = json!({
        "v": 1,
        "type": "hello",
        "endpointId": endpoint_id,
        "candidates": server.net_candidates(),
    });
    if socket
        .send(Message::Text(answer.to_string()))
        .await
        .is_err()
    {
        return;
    }
    tracing::info!("p2p: signaled {peer}");

    // Keep the channel open. A phone that changed networks re-announces itself
    // on the same socket rather than opening a second one.
    while let Some(Ok(message)) = socket.recv().await {
        let Message::Text(text) = message else {
            if matches!(message, Message::Close(_)) {
                break;
            }
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("hello") => {
                let answer = json!({
                    "v": 1,
                    "type": "hello",
                    "endpointId": server.net_endpoint_id(),
                    "candidates": server.net_candidates(),
                });
                if socket
                    .send(Message::Text(answer.to_string()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Some("bye") => break,
            // Candidates from the phone are not used by an acceptor: the punch
            // is answered at the datagram's source, which is the address that
            // actually works.
            _ => {}
        }
    }
}

/// Run one phone connection over a WebSocket.
///
/// The socket is adapted to a [`Peer`] by two small tasks — one reading text in,
/// one writing frames out — and everything that matters happens in
/// [`serve_peer`]. The two kinds of frame are told apart by their first byte: a
/// state-channel frame starts with `0x53` ('S'), and every JSON frame starts
/// with `{`.
async fn serve_socket(server: Arc<MizarServer>, peer: SocketAddr, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (text_tx, text_rx) = tokio::sync::mpsc::channel::<String>(64);
    let (bytes_tx, mut bytes_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    let reader = tokio::spawn(async move {
        'reading: while let Some(message) = stream.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    if text_tx.send(text).await.is_err() {
                        break 'reading;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break 'reading,
                // Binary from a phone is the on-device kernel's dialect, not this
                // socket's, and a ping is answered by the HTTP layer.
                _ => {}
            }
        }
    });
    let writer = tokio::spawn(async move {
        while let Some(bytes) = bytes_rx.recv().await {
            let message = if bytes.first() == Some(&agi_protocol::state_frame::MAGIC) {
                Message::Binary(bytes)
            } else {
                match String::from_utf8(bytes) {
                    Ok(text) => Message::Text(text),
                    // A frame that is not UTF-8 is not JSON and not a state
                    // frame; nothing downstream could read it.
                    Err(_) => continue,
                }
            };
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    serve_peer(
        server,
        peer.to_string(),
        Peer {
            inbound: text_rx,
            outbound: bytes_tx,
        },
    )
    .await;
    reader.abort();
    let _ = writer.await;
}

/// Run one phone connection over a punched UDP path.
///
/// The phone sends bare Mizar frames on this path — no HTTP upgrade, no WebSocket
/// framing — so the only thing it shares with the socket path is what is below
/// them: the same handshake, the same methods, the same event forwarders.
async fn serve_punched(server: Arc<MizarServer>, stream: crate::net::engine::Stream) {
    let description = format!("p2p {}", stream.describe());
    let (sender, mut receiver) = stream.split();
    let (text_tx, text_rx) = tokio::sync::mpsc::channel::<String>(64);
    let (bytes_tx, mut bytes_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    let reader = tokio::spawn(async move {
        while let Some(message) = receiver.receive().await {
            match String::from_utf8(message) {
                Ok(text) => {
                    if text_tx.send(text).await.is_err() {
                        break;
                    }
                }
                Err(_) => tracing::debug!("mizar: a direct-path message was not UTF-8"),
            }
        }
    });
    let writer = tokio::spawn(async move {
        while let Some(bytes) = bytes_rx.recv().await {
            if sender.send(bytes).await.is_err() {
                break;
            }
        }
    });

    serve_peer(
        server,
        description,
        Peer {
            inbound: text_rx,
            outbound: bytes_tx,
        },
    )
    .await;
    reader.abort();
    let _ = writer.await;
}

/// Run one phone connection.
///
/// The handshake, the state channel, the interaction forwarder and the method
/// loop are all here, and all of them are transport-agnostic. The only thing a
/// caller supplies is a [`Peer`]: text frames in, frame bytes out.
/// The device id behind a peer token, from its `peer:<account>:<device>:<engine>`
/// marker. Phone tokens have no peer identity and are not covered by the
/// remote-device blocklist.
fn peer_device_id(device: &crate::store::Device) -> Option<String> {
    let marker = device.model.as_deref()?;
    let identity = crate::relay::peer_identity_of_marker(marker)?;
    identity.rsplit(':').next().map(str::to_string)
}

async fn serve_peer(server: Arc<MizarServer>, peer: String, connection: Peer) {
    let Peer {
        mut inbound,
        outbound: out,
    } = connection;

    // The handshake is the only frame allowed to be unauthenticated.
    let Some(first) = inbound.recv().await else {
        tracing::warn!("mizar: {peer} connected and sent no hello");
        return;
    };
    let Some(frame) = ClientFrame::parse(&first) else {
        tracing::warn!("mizar: {peer} opened a connection with a frame that is not JSON");
        let _ = send(
            &out,
            ServerFrame::error(
                None,
                ErrorCode::MalformedFrame,
                "the first frame must be a hello",
            ),
        )
        .await;
        return;
    };
    let Some(hello) = frame.hello() else {
        tracing::warn!("mizar: {peer} did not start with hello");
        let _ = send(
            &out,
            ServerFrame::error(None, ErrorCode::Unauthenticated, "send hello first"),
        )
        .await;
        return;
    };
    let Some(device) = server.store.authenticate(&hello.token) else {
        tracing::warn!("mizar: {peer} presented a token this host does not know");
        let _ = send(
            &out,
            ServerFrame::error(
                None,
                ErrorCode::DeviceRevoked,
                "this device is not paired, or was revoked",
            ),
        )
        .await;
        return;
    };

    if peer_device_id(&device).is_some_and(|id| server.core.peer_is_blocked(&id)) {
        tracing::warn!("mizar: {peer} is blocked on this computer");
        let _ = send(
            &out,
            ServerFrame::error(
                None,
                ErrorCode::Unauthenticated,
                "this device is blocked on this computer",
            ),
        )
        .await;
        return;
    }

    tracing::info!(
        "mizar: {} connected from {peer} ({})",
        device.name,
        hello.app_version.as_deref().unwrap_or("unknown version")
    );

    // A different account is a different operator. Refused before the welcome,
    // so nothing about this computer's sessions reaches the phone.
    if let Some(reason) = server.account_mismatch(hello.account_id.as_deref()) {
        tracing::warn!("mizar: {peer} was refused — {reason}");
        let _ = send(
            &out,
            ServerFrame::error(None, ErrorCode::Unauthenticated, reason),
        )
        .await;
        return;
    }

    // Held until this function returns, which is when the connection is done.
    let _live = LiveSocket::open(Arc::clone(&server), device.id.clone());

    let _ = send(
        &out,
        ServerFrame::welcome(WelcomeInput {
            host_id: server.store.host_id(),
            host_name: server.store.host_name(),
            home: server.home.clone(),
            device_id: device.id.clone(),
        }),
    )
    .await;

    // The agent this connection is bound to, from the token or from the hello
    // that opened it. Every per-agent answer below is filtered by it.
    let scope = MizarServer::connection_scope(&device, Some(&hello));

    // The state channel: one byte per session-state change, plus a heartbeat
    // every few seconds, scoped to the agent this phone asked for. This is what
    // turns the phone's status pill and stream bar on at all -- before this, the
    // channel only reached the desktop's own window.
    let states_task = {
        let out = out.clone();
        let state_server = Arc::clone(&server);
        let state_device = device.clone();
        let mut states = server.core.subscribe_states();
        tokio::spawn(async move {
            loop {
                match states.recv().await {
                    Ok((session, bytes)) => {
                        if scope.is_some_and(|engine| session.engine != engine)
                            || !state_server.peer_owns(&state_device, &session)
                        {
                            continue;
                        }
                        let id = session.to_string();
                        let mut payload = Vec::with_capacity(1 + id.len() + bytes.len());
                        payload.push(id.len().min(255) as u8);
                        payload.extend_from_slice(id.as_bytes());
                        payload.extend_from_slice(&bytes);
                        if out.send(payload).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        })
    };

    // Interaction notifications, so an approval reaches the phone without a poll.
    let mut events = server.core.subscribe();
    let forwarder = {
        let out = out.clone();
        let forward_server = Arc::clone(&server);
        let forward_device = device.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if !interaction_in_scope(&forward_server, &forward_device, &event, scope) {
                            continue;
                        }
                        if let Some(frame) = interaction_event(&event) {
                            if send(&out, frame).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        })
    };

    // Host notices: today the only one is the tunnel moving, and a phone that is
    // not told would keep dialling the old relay until something failed.
    let mut notices = server.notices.subscribe();
    let notifier = {
        let out = out.clone();
        tokio::spawn(async move {
            loop {
                match notices.recv().await {
                    Ok(data) => {
                        if send(&out, ServerFrame::event("relay.changed", data))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        })
    };

    let mut prefs_changed = server.core.subscribe_remote_prefs();
    loop {
        let text = tokio::select! {
            text = inbound.recv() => match text {
                Some(text) => text,
                None => break,
            },
            changed = prefs_changed.recv() => {
                // A policy change wakes an idle connection; blocking must not
                // wait for the peer's next frame.
                if changed.is_ok()
                    && peer_device_id(&device)
                        .is_some_and(|id| server.core.peer_is_blocked(&id))
                {
                    tracing::warn!("mizar: {} became blocked; closing the socket", device.name);
                    let _ = send(
                        &out,
                        ServerFrame::error(
                            None,
                            ErrorCode::Unauthenticated,
                            "this device is blocked on this computer",
                        ),
                    )
                    .await;
                    break;
                }
                continue;
            }
        };
        // Per-frame checks still matter: the broadcast can lag or coalesce.
        if peer_device_id(&device).is_some_and(|id| server.core.peer_is_blocked(&id)) {
            tracing::warn!("mizar: {} became blocked; closing the socket", device.name);
            let _ = send(
                &out,
                ServerFrame::error(
                    None,
                    ErrorCode::Unauthenticated,
                    "this device is blocked on this computer",
                ),
            )
            .await;
            break;
        }
        let Some(frame) = ClientFrame::parse(&text) else {
            continue;
        };
        // A second hello on a live connection is a client bug, not a re-handshake.
        if frame.kind == "hello" {
            continue;
        }
        let answer = server.call(&device, scope, &frame).await;
        if send(&out, answer).await.is_err() {
            break;
        }
    }

    states_task.abort();
    forwarder.abort();
    notifier.abort();
    drop(out);
    tracing::info!("mizar: {} disconnected", device.name);
}

/// Write one frame to the connection's writer task.
async fn send(
    out: &tokio::sync::mpsc::Sender<Vec<u8>>,
    frame: ServerFrame,
) -> Result<(), tokio::sync::mpsc::error::SendError<Vec<u8>>> {
    let text = serde_json::to_string(&frame).unwrap_or_else(|_| "{}".to_string());
    out.send(text.into_bytes()).await
}

/// Whether an event is one this connection should hear about.
///
/// Only interactions are filtered here, because they are the only events this
/// socket forwards and the only ones that carry another agent's work. A
/// question from an agent the phone is not driving would draw a prompt it
/// cannot answer usefully, and an approval it answers lands on the wrong
/// agent's turn.
fn interaction_in_scope(
    server: &MizarServer,
    device: &crate::store::Device,
    event: &agi_protocol::api::ApiEvent,
    scope: Option<agi_protocol::engine::EngineId>,
) -> bool {
    use agi_protocol::api::ApiEvent;
    let key = match event {
        ApiEvent::InteractionRequested { interaction } => interaction.session.as_ref(),
        // A resolution carries no session: the phone only ever hears about
        // requests it was told about, and dropping it would leave a sheet open
        // over a request that is gone.
        _ => return true,
    };
    key.is_none_or(|key| MizarServer::in_scope(scope, key) && server.peer_owns(device, key))
}

/// Classify one approval for the parent model.
///
/// This is deliberately conservative and title-based: a tool this host does not
/// recognise is high risk. Build/test/lint commands are low; anything that
/// installs, escalates, reaches the network, or writes outside a workspace is
/// high. A peer may only auto-answer the low tier.
fn approval_risk(interaction: &agi_protocol::domain::Interaction) -> (&'static str, &'static str) {
    let title = interaction.title.to_lowercase();
    if matches!(
        title.as_str(),
        "read" | "list" | "glob" | "grep" | "edit" | "write" | "apply_patch" | "multiedit"
    ) {
        return ("low", "workspace read or edit");
    }
    if matches!(
        title.as_str(),
        "bash" | "shell" | "exec" | "run" | "command"
    ) {
        let detail = interaction.detail.as_deref().unwrap_or("").to_lowercase();
        let destructive = [
            "sudo",
            "rm -rf",
            "mkfs",
            "dd if=",
            "shutdown",
            "reboot",
            "chmod",
            "chown",
            "curl ",
            "wget ",
            "ssh ",
            "scp ",
            "git push",
            "npm install",
            "pnpm add",
            "brew ",
            "pip install",
            "> /",
            "/etc/",
            "~/.ssh",
            "security ",
        ];
        if destructive.iter().any(|needle| detail.contains(needle)) {
            return (
                "high",
                "destructive, privileged, network or install command",
            );
        }
        let routine = [
            "cargo test",
            "cargo build",
            "cargo check",
            "cargo clippy",
            "cargo fmt",
            "npm test",
            "npm run test",
            "pnpm test",
            "pnpm run test",
            "git status",
            "git diff",
            "ls ",
            "rg ",
            "grep ",
            "cat ",
            "sed -n",
        ];
        if routine.iter().any(|needle| detail.contains(needle)) {
            return ("low", "routine build, test or read command");
        }
        return ("high", "unrecognised shell command");
    }
    if title.contains("fetch") || title.contains("browser") || title.contains("web") {
        return ("high", "network egress");
    }
    ("high", "tool is not on the low-risk list")
}

/// Translate one kernel event into the frame the phone understands.
fn interaction_event(event: &agi_protocol::api::ApiEvent) -> Option<ServerFrame> {
    use agi_protocol::api::ApiEvent;
    match event {
        ApiEvent::InteractionRequested { interaction } => match interaction.kind {
            InteractionKind::Approval => Some(ServerFrame::event(
                "approval.request",
                json!({
                    "requestId": interaction.id,
                    "sessionId": interaction.session.as_ref().map(|key| key.to_string()),
                    "toolName": interaction.title,
                    "callId": Value::Null,
                    "reason": interaction.detail,
                    "displayReason": Value::Null,
                    "requestedAt": interaction.requested_at,
                }),
            )),
            // Questions are not modelled yet; the count still moves.
            InteractionKind::Question => Some(ServerFrame::event(
                "interactions.changed",
                json!({ "approvals": 0, "questions": 1 }),
            )),
        },
        ApiEvent::InteractionResolved { id } => Some(ServerFrame::event(
            "approval.resolved",
            json!({ "requestId": id, "outcome": "answered" }),
        )),
        _ => None,
    }
}

/// Epoch milliseconds, for a card row's clock.
///
/// The phone ticks from this instant, so it has to be wall-clock and not a
/// monotonic reading: the two processes are on different clocks and the value
/// crosses a process boundary.
fn activity_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}

/// The OS name the phone displays.
fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Linux"
    }
}

/// Wrap a frame as an HTTP response body.
fn frame_response(frame: ServerFrame) -> Response {
    Json(serde_json::to_value(frame).unwrap_or(Value::Null)).into_response()
}

impl agi_core::PairingSource for MizarServer {
    fn pairing_view(&self, refresh: bool) -> agi_protocol::api::PairingView {
        MizarServer::pairing_view(self, refresh)
    }
}

/// Attach each turn's cost to its own `turn/end` event, inside the envelope.
///
/// The phone reads the conversation footer from `event.data.mizarTokenUsage`, so
/// the number belongs on the event, not on the envelope that carries it. A turn
/// the engine cannot account for is left without the key, and the footer keeps
/// its own words.
fn attach_turn_usage(records: &mut [Value], totals: &std::collections::HashMap<u32, u64>) {
    for envelope in records {
        let Some(event) = envelope.get_mut("event") else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) != Some("turn/end") {
            continue;
        }
        let Some(data) = event.get_mut("data") else {
            continue;
        };
        let Some(turn) = data.get("turn").and_then(Value::as_u64) else {
            continue;
        };
        if let Some(total) = totals.get(&(turn as u32)) {
            data["mizarTokenUsage"] = json!({ "totalTokens": total });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wallet(currency: &str, amount: &str) -> Wallet {
        Wallet {
            currency: currency.to_string(),
            amount: amount.to_string(),
        }
    }

    #[test]
    fn workspace_membership_is_spelled_like_the_session_list() {
        // The registry stores bare ids; the phone compares them with the ids
        // `session.list` returned. Without the prefix nothing ever matches.
        let ids = membership(
            EngineId::Dsh,
            Some(vec!["session-1".to_string(), "dsh:session-2".to_string()]),
        )
        .unwrap();
        assert_eq!(ids, vec!["dsh:session-1", "dsh:session-2"]);
        // "nothing filed here" is an answer; "did not say" is not.
        assert_eq!(
            membership(EngineId::Dsh, Some(Vec::new())).unwrap().len(),
            0
        );
        assert!(membership(EngineId::Dsh, None).is_none());
    }

    #[test]
    fn approval_risk_is_conservative_but_not_all_bash() {
        let mut interaction = agi_protocol::domain::Interaction {
            id: "a".to_string(),
            session: None,
            kind: InteractionKind::Approval,
            title: "bash".to_string(),
            detail: Some("cargo test -p agi-core".to_string()),
            requested_at: 0,
            questions: Vec::new(),
        };
        assert_eq!(approval_risk(&interaction).0, "low");
        interaction.detail = Some("sudo rm -rf /".to_string());
        assert_eq!(approval_risk(&interaction).0, "high");
        interaction.title = "read".to_string();
        interaction.detail = None;
        assert_eq!(approval_risk(&interaction).0, "low");
        interaction.title = "unknown-tool".to_string();
        assert_eq!(approval_risk(&interaction).0, "high");
    }

    #[test]
    fn turn_usage_lands_inside_the_event_envelope() {
        // `records[]` elements are envelopes; the footer reads
        // `event.data.mizarTokenUsage`. Attaching the number to the envelope
        // itself would be exactly the flat shape the golden fixture rejects.
        let mut records = vec![
            json!({
                "type": "event",
                "event": {
                    "type": "turn/end", "seq": 24, "time": 0,
                    "data": { "turn": 1, "reason": { "kind": "success" } }
                }
            }),
            json!({
                "type": "event",
                "event": {
                    "type": "turn/start", "seq": 12, "time": 0,
                    "data": { "turn": 1 }
                }
            }),
        ];
        let totals = std::collections::HashMap::from([(1u32, 1234u64)]);
        attach_turn_usage(&mut records, &totals);
        assert_eq!(
            records[0]["event"]["data"]["mizarTokenUsage"]["totalTokens"],
            1234
        );
        assert!(records[0].get("mizarTokenUsage").is_none());
        assert!(records[0].get("data").is_none());
        // A turn the engine cannot account for keeps no key at all.
        assert!(records[1]["event"]["data"].get("mizarTokenUsage").is_none());
    }

    #[test]
    fn wallets_are_drawn_only_when_they_are_money() {
        let paid = vec![wallet("CNY", "9.86578102")];
        let rows = wallet_rows(&paid, true).unwrap();
        assert_eq!(rows[0]["balance"], "9.86578102");

        // A currency the build does not know is not drawn as a number.
        assert!(wallet_rows(&[wallet("EUR", "1.00")], true).is_none());
        // Two rows for one currency is ambiguous, not additive.
        assert!(wallet_rows(&[wallet("CNY", "1"), wallet("CNY", "2")], true).is_none());
        // Prose is not a balance.
        assert!(wallet_rows(&[wallet("CNY", "about ten")], true).is_none());
        assert!(wallet_rows(&[wallet("CNY", "NaN")], true).is_none());
        assert!(wallet_rows(&[wallet("CNY", "Infinity")], true).is_none());
        assert!(wallet_rows(&[wallet("CNY", "")], true).is_none());
        // The app's own grammar: no leading plus, no padding.
        assert!(wallet_rows(&[wallet("CNY", "+1")], true).is_none());
        assert!(wallet_rows(&[wallet("CNY", " 9.86 ")], true).is_none());
        // What the app does accept.
        for amount in ["1", "-1", ".5", "5.", "1e5", "1E+5", "9.86578102"] {
            assert!(
                wallet_rows(&[wallet("CNY", amount)], true).is_some(),
                "{amount} should be accepted"
            );
        }
        assert!(wallet_rows(&[wallet("CNY", &"9".repeat(257))], true).is_none());
        // A paid wallet is required; bonus wallets are optional.
        assert!(wallet_rows(&[], true).is_none());
        assert_eq!(wallet_rows(&[], false), Some(Vec::new()));
    }

    #[test]
    fn os_names_are_display_names() {
        assert!(["macOS", "Windows", "Linux"].contains(&os_name()));
    }

    #[test]
    fn an_approval_event_carries_the_fields_the_phone_reads() {
        let event = agi_protocol::api::ApiEvent::InteractionRequested {
            interaction: agi_protocol::domain::Interaction {
                id: "a1".to_string(),
                session: Some(SessionKey::new(agi_protocol::engine::EngineId::Dsh, "s1")),
                kind: InteractionKind::Approval,
                title: "bash".to_string(),
                detail: Some("rm -rf build".to_string()),
                requested_at: 1,
                questions: Vec::new(),
            },
        };
        let frame = interaction_event(&event).unwrap();
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["event"], "approval.request");
        assert_eq!(value["data"]["requestId"], "a1");
        assert_eq!(value["data"]["toolName"], "bash");
        assert_eq!(value["data"]["sessionId"], "dsh:s1");
    }
}
