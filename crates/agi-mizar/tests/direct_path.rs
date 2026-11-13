//! The direct path, end to end: a punch, then the same Mizar handshake.
//!
//! The point of this file is that nothing is faked on the wire. A phone-shaped
//! client signs a ClientHello with an Ed25519 key, punches it at a real UDP
//! socket, verifies the ServerHello that comes back, derives the session keys
//! from the exchange, and then sends a Mizar `hello` frame over the resulting
//! message stream. The answer has to be the same `welcome` a WebSocket gets,
//! because it comes from the same code: the transport changed, the protocol did
//! not.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agi_core::Core;
use agi_mizar::net::crypto;
use agi_mizar::net::engine::{Engine, Reach};
use agi_mizar::net::identity::{verify_signature, Identity, Purpose};
use agi_mizar::net::session::{Event, Role, Session};
use agi_mizar::net::wire::{Packet, PacketBody, PacketType};
use agi_mizar::{DeviceStore, MizarServer};
use serde_json::{json, Value};
use tokio::net::UdpSocket;

/// A server over an empty kernel and a temporary roster.
async fn start() -> (Arc<MizarServer>, Arc<DeviceStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = DeviceStore::open(dir.path().join("devices.json"), "test-host").unwrap();
    let core = Core::new(Vec::new());
    core.start().await;
    let server = MizarServer::new(core, Arc::clone(&store), "/tmp/dsh-home");
    server.bind(0).await.unwrap();
    (server, store, dir)
}

/// A phone-shaped client that has punched through and can talk.
struct Dialer {
    socket: UdpSocket,
    engine: std::net::SocketAddr,
    identity: Identity,
    connection_id: u64,
    session: Session,
}

/// Punch at the engine and settle the session.
async fn punch(engine: std::net::SocketAddr, engine_endpoint_id: &str) -> Dialer {
    let socket = UdpSocket::bind(("127.0.0.1", 0))
        .await
        .expect("a phone socket");
    let identity = Identity::derive(b"the device token", Purpose::Device);
    let (local_secret, local_public) = crypto::ephemeral_keypair();
    let connection_id = 0x1122_3344_5566_7788;

    let mut hello = Packet::hello(
        PacketType::ClientHello,
        connection_id,
        local_public,
        identity.endpoint_id(),
        [0u8; 64],
    );
    let signature = identity.sign(&hello.handshake_message());
    hello = Packet::hello(
        PacketType::ClientHello,
        connection_id,
        local_public,
        identity.endpoint_id(),
        signature,
    );
    socket
        .send_to(&hello.encode(), engine)
        .await
        .expect("the hello goes out");

    let mut buffer = vec![0u8; 2048];
    let (length, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buffer))
        .await
        .expect("a server hello arrives")
        .expect("a datagram");
    let answer = Packet::decode(&buffer[..length]).expect("a decoded answer");
    assert_eq!(answer.kind, PacketType::ServerHello);
    let PacketBody::Hello {
        ephemeral,
        ref endpoint_id,
        ref signature,
    } = answer.body
    else {
        panic!("an answer is a hello");
    };
    // The ticket named a key; this is the proof that the phone reached it.
    assert_eq!(endpoint_id, engine_endpoint_id);
    assert!(verify_signature(
        signature,
        &answer.handshake_message(),
        engine_endpoint_id
    ));

    let shared = crypto::agree(&local_secret, &ephemeral);
    let session = Session::new(
        Role::Dialer,
        connection_id,
        identity.endpoint_id(),
        engine_endpoint_id,
        &shared,
        Instant::now(),
    );
    Dialer {
        socket,
        engine,
        identity,
        connection_id,
        session,
    }
}

impl Dialer {
    async fn send(&mut self, message: &[u8]) {
        for packet in self.session.send(message, Instant::now()) {
            self.socket
                .send_to(&packet.encode(), self.engine)
                .await
                .expect("sent");
        }
    }

    async fn receive(&mut self) -> Vec<u8> {
        let mut buffer = vec![0u8; 65_536];
        // Acks and keepalives can arrive between the data packet and its
        // answer; only a message is what this helper is waiting for.
        loop {
            let (length, _) =
                tokio::time::timeout(Duration::from_secs(2), self.socket.recv_from(&mut buffer))
                    .await
                    .expect("a message arrives")
                    .expect("a datagram");
            let packet = Packet::decode(&buffer[..length]).expect("a packet");
            for event in self.session.on_packet(&packet, Instant::now()) {
                if let Event::Message(message) = event {
                    return message;
                }
            }
        }
    }
}

#[tokio::test]
async fn a_punched_connection_speaks_the_same_protocol_as_a_socket() {
    let (server, store, _dir) = start().await;
    let issued = store.issue("phone", None).expect("a token");

    // The identity the engine derives — on a real launch, from the tunnel
    // secret; here from whatever the test names.
    let identity = Identity::derive(b"a tunnel secret", Purpose::Host);
    let endpoint_id = identity.endpoint_id().to_string();
    let reach = Reach::default();
    let (mut engine, address) = Engine::bind(identity, 0, reach.clone(), Vec::new())
        .await
        .expect("a socket");
    let (punches, _punches) = tokio::sync::mpsc::channel(1);
    server.attach_direct_path(endpoint_id.clone(), reach, punches);

    // The accept loop, in miniature: one connection at a time is enough, and it
    // is the same call the boot sequence makes.
    let serving = Arc::clone(&server);
    tokio::spawn(async move {
        while let Some(stream) = engine.accept().await {
            let server = Arc::clone(&serving);
            tokio::spawn(async move { server.serve_punched(stream).await });
        }
    });

    let mut dialer = punch(address, &endpoint_id).await;
    // A phone's first frame over any transport is the same hello.
    let hello = json!({
        "v": 1,
        "type": "hello",
        "token": issued.token,
        "appVersion": "0.9.9-test",
        "protocolVersion": 1,
    });
    dialer.send(hello.to_string().as_bytes()).await;

    let welcome: Value = serde_json::from_slice(&dialer.receive().await).expect("JSON");
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["hostId"], store.host_id());
    assert_eq!(
        welcome["deviceId"], issued.device.id,
        "the welcome names the device the token belongs to"
    );
    assert_eq!(dialer.identity.endpoint_id().len(), 43);
    assert_eq!(dialer.connection_id, 0x1122_3344_5566_7788);
}

#[tokio::test]
async fn host_info_offers_the_direct_path_only_when_one_is_bound() {
    let (server, store, _dir) = start().await;
    let issued = store.issue("phone", None).expect("a token");

    // Nothing attached: the field is null, and a phone reads that as "no route".
    assert_eq!(server.net_endpoint_id(), None);

    let reach = Reach::default();
    reach.set_port(51_234);
    let reflexive: std::net::SocketAddr = "124.228.221.210:61921".parse().unwrap();
    reach.set_reflexive(reflexive);
    let (punches, _punches) = tokio::sync::mpsc::channel(1);
    server.attach_direct_path("an-endpoint-id", reach, punches);
    assert_eq!(server.net_endpoint_id().as_deref(), Some("an-endpoint-id"));

    // The candidates the signalling channel hands out are the host addresses on
    // the engine's port, with ICE's priority shape.
    let candidates = server.net_candidates_for_tests();
    let kinds: Vec<&str> = candidates
        .iter()
        .filter_map(|candidate| candidate["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"srflx"),
        "the outside address is published: {kinds:?}"
    );
    for candidate in &candidates {
        assert!(candidate["priority"].as_u64().unwrap_or(0) > 0);
        assert!(candidate["port"].as_u64().unwrap_or(0) > 0);
    }
    // Host candidates are on the engine's port; the reflexive one is on whatever
    // port the NAT handed out.
    let reflexive_row = candidates
        .iter()
        .find(|candidate| candidate["kind"] == "srflx")
        .expect("one reflexive candidate");
    assert_eq!(reflexive_row["port"], 61_921);
    assert_eq!(reflexive_row["address"], "124.228.221.210");
    let _ = issued;
}
