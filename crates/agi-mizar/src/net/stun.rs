//! Just enough STUN to learn where this computer appears from outside.
//!
//! A phone that is not on the same network cannot punch to `192.168.1.5` — that
//! address is not routable from where it is. The only address it *can* reach is
//! the one the NAT in front of this machine hands out when a datagram leaves, and
//! the only way to learn that address is to ask somebody on the outside what they
//! saw. That is all this file does.
//!
//! ### Why the same socket
//!
//! The request has to leave from **the engine's own UDP socket**. Asking over a
//! second socket learns the mapping of a different port, which is an address
//! nobody can reach this engine on — and the mapping only exists while something
//! keeps sending from it, which is why [`PROBE_INTERVAL`] is far shorter than any
//! NAT's expiry and why the probe keeps running for the life of the process.
//!
//! ### The subset
//!
//! RFC 5389's binding request and success response, with `XOR-MAPPED-ADDRESS`
//! (0x0020) and, as a fallback, `MAPPED-ADDRESS` (0x0001). No authentication, no
//! other attributes, no IPv6 responses: the relay that runs this STUN is this
//! project's own, and a reflexive candidate is an optimisation — a server that
//! answers something else costs the phone the relay route it already has.
//!
//! ```text
//! request : 0001 0000 2112A442 ‖ 96-bit transaction id
//! response: 0101 length 2112A442 ‖ transaction id ‖ attributes…
//! ```

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;

/// The magic cookie that makes a STUN message unmistakable.
pub const MAGIC_COOKIE: u32 = 0x2112_A442;

/// A binding request.
const TYPE_REQUEST: u16 = 0x0001;
/// A binding success response.
const TYPE_SUCCESS: u16 = 0x0101;
/// The attribute that says what address the server saw, XORed with the cookie.
const ATTR_XOR_MAPPED: u16 = 0x0020;
/// The older, unobfuscated attribute, used when a server answers only that.
const ATTR_MAPPED: u16 = 0x0001;

/// How long to wait for one answer.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(700);

/// How often to ask again.
///
/// Every NAT this project has met expires an idle UDP mapping in thirty seconds
/// or more, so fifteen is comfortably inside every one of them: the mapping this
/// machine publishes stays open as long as this application runs.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(15);

/// Why a datagram is not a STUN answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StunError {
    /// Shorter than the twenty-byte header.
    #[error("shorter than a STUN header")]
    Truncated,
    /// Not a binding success.
    #[error("not a binding success response")]
    NotAResponse,
    /// The transaction id is not the one this probe sent.
    #[error("the transaction id does not match")]
    WrongTransaction,
    /// No address attribute this build understands.
    #[error("no mapped address in the answer")]
    NoAddress,
    /// The declared length and the datagram disagree.
    #[error("the declared length is wrong")]
    BadLength,
}

/// One probe: the bytes to send and what will identify the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    transaction: [u8; 12],
}

impl Probe {
    /// A fresh probe, with a random transaction id.
    pub fn new() -> Self {
        use rand::RngCore;
        let mut transaction = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut transaction);
        Self { transaction }
    }

    /// The datagram to send.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20);
        out.extend_from_slice(&TYPE_REQUEST.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&self.transaction);
        out
    }

    /// Read one answer.
    ///
    /// - Parameter bytes: exactly one datagram.
    /// - Returns: the address the server saw.
    pub fn decode(&self, bytes: &[u8]) -> Result<SocketAddr, StunError> {
        if bytes.len() < 20 {
            return Err(StunError::Truncated);
        }
        let kind = u16::from_be_bytes([bytes[0], bytes[1]]);
        if kind != TYPE_SUCCESS {
            return Err(StunError::NotAResponse);
        }
        let declared = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        if bytes.len() != 20 + declared {
            return Err(StunError::BadLength);
        }
        if bytes[4..8] != MAGIC_COOKIE.to_be_bytes() {
            return Err(StunError::NotAResponse);
        }
        if bytes[8..20] != self.transaction {
            return Err(StunError::WrongTransaction);
        }

        let mut mapped = None;
        let mut at = 20;
        while at + 4 <= bytes.len() {
            let attribute = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
            let length = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
            let value_start = at + 4;
            let value_end = value_start + length;
            if value_end > bytes.len() {
                return Err(StunError::BadLength);
            }
            let value = &bytes[value_start..value_end];
            match attribute {
                ATTR_XOR_MAPPED if mapped.is_none() => mapped = xor_mapped(value),
                ATTR_MAPPED if mapped.is_none() => mapped = plain_mapped(value),
                _ => {}
            }
            // Attributes are padded to four bytes.
            at = value_end + (4 - length % 4) % 4;
        }
        mapped.ok_or(StunError::NoAddress)
    }
}

impl Default for Probe {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode `XOR-MAPPED-ADDRESS` (or the same layout inside an IPv6 one we skip).
fn xor_mapped(value: &[u8]) -> Option<SocketAddr> {
    if value.len() < 8 {
        return None;
    }
    let family = value[1];
    let port = u16::from_be_bytes([value[2], value[3]]) ^ (MAGIC_COOKIE >> 16) as u16;
    if family != 0x01 {
        // IPv6 would be sixteen bytes XORed with the cookie and the transaction
        // id. A host-only candidate already covers the IPv6 LAN case, and a
        // half-understood IPv6 candidate would be worse than none.
        return None;
    }
    let raw = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
    let address = raw ^ MAGIC_COOKIE;
    Some(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::from(address)),
        port,
    ))
}

/// Decode the unobfuscated `MAPPED-ADDRESS`.
fn plain_mapped(value: &[u8]) -> Option<SocketAddr> {
    if value.len() < 8 || value[1] != 0x01 {
        return None;
    }
    let port = u16::from_be_bytes([value[2], value[3]]);
    Some(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(value[4], value[5], value[6], value[7])),
        port,
    ))
}

/// Ask one server where this socket appears.
///
/// - Parameters:
///   - socket: the socket the answer must come back on.
///   - server: `host:port`, already resolved or a literal.
/// - Returns: the address the server saw, or an error when it did not answer in
///   time. Failure is ordinary: a STUN server that is down costs the direct path
///   its outside candidate, and the relay is still there.
pub async fn probe(socket: &UdpSocket, server: &str) -> io::Result<SocketAddr> {
    let resolved = tokio::net::lookup_host(server).await?.next();
    let Some(destination) = resolved else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the STUN server does not resolve",
        ));
    };
    let request = Probe::new();
    socket.send_to(&request.encode(), destination).await?;

    let mut buffer = [0u8; 512];
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the STUN server did not answer",
            ));
        }
        let received = tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await;
        let Ok(Ok((length, from))) = received else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the STUN server did not answer",
            ));
        };
        // A datagram that is not an answer to *this* probe is not ours to
        // interpret: the engine's socket also receives the phone's hellos.
        if from != destination {
            continue;
        }
        if let Ok(address) = request.decode(&buffer[..length]) {
            return Ok(address);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A success response for one probe, with an XOR-mapped address.
    fn answer(probe: &Probe, seen: SocketAddr) -> Vec<u8> {
        let SocketAddr::V4(v4) = seen else {
            panic!("the fixture is IPv4");
        };
        let port = v4.port() ^ (MAGIC_COOKIE >> 16) as u16;
        let address = u32::from(*v4.ip()) ^ MAGIC_COOKIE;
        let mut attribute = Vec::new();
        attribute.extend_from_slice(&0u8.to_be_bytes());
        attribute.push(0x01);
        attribute.extend_from_slice(&port.to_be_bytes());
        attribute.extend_from_slice(&address.to_be_bytes());

        // The message length counts every attribute, header included: four
        // bytes of type and length plus the value.
        let mut body = Vec::new();
        body.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
        body.extend_from_slice(&(attribute.len() as u16).to_be_bytes());
        body.extend_from_slice(&attribute);

        let mut out = Vec::new();
        out.extend_from_slice(&TYPE_SUCCESS.to_be_bytes());
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&probe.transaction);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn a_request_is_the_twenty_byte_header_and_nothing_else() {
        let request = Probe::new().encode();
        assert_eq!(request.len(), 20);
        assert_eq!(&request[..2], &TYPE_REQUEST.to_be_bytes());
        assert_eq!(&request[2..4], &[0, 0]);
        assert_eq!(&request[4..8], &MAGIC_COOKIE.to_be_bytes());
    }

    #[test]
    fn the_answer_says_which_address_the_server_saw() {
        let probe = Probe::new();
        let seen: SocketAddr = "124.228.221.210:61921".parse().unwrap();
        assert_eq!(probe.decode(&answer(&probe, seen)), Ok(seen));
    }

    #[test]
    fn an_answer_to_somebody_elses_probe_is_refused() {
        // The engine's socket receives the phone's hellos too; a datagram that is
        // a valid STUN response but not an answer to this probe is not ours.
        let ours = Probe::new();
        let theirs = Probe::new();
        let seen: SocketAddr = "10.0.0.1:1234".parse().unwrap();
        assert_eq!(theirs.decode(&answer(&theirs, seen)), Ok(seen));
        assert_eq!(ours.decode(&answer(&theirs, seen)), Err(StunError::WrongTransaction));
    }

    #[test]
    fn a_truncated_or_mislabelled_datagram_is_refused() {
        let probe = Probe::new();
        assert_eq!(probe.decode(&[0; 4]), Err(StunError::Truncated));
        let mut wrong = answer(&probe, "10.0.0.1:1".parse().unwrap());
        wrong[0] = 0x00;
        wrong[1] = 0x03;
        assert_eq!(probe.decode(&wrong), Err(StunError::NotAResponse));
    }

    #[tokio::test]
    async fn a_probe_against_a_local_server_learns_its_own_address() {
        // A tiny STUN server: answers every binding request with the sender's
        // address, which is exactly what the real one does.
        let server = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let server_address = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buffer = [0u8; 64];
            while let Ok((length, from)) = server.recv_from(&mut buffer).await {
                let probe = Probe::new();
                let mut transaction = [0u8; 12];
                transaction.copy_from_slice(&buffer[8..20]);
                let _ = length;
                // Reuse the sender's transaction id so the client accepts it.
                let reply = {
                    let mut out = Vec::new();
                    let SocketAddr::V4(v4) = from else { continue };
                    let port = v4.port() ^ (MAGIC_COOKIE >> 16) as u16;
                    let address = u32::from(*v4.ip()) ^ MAGIC_COOKIE;
                    let mut attribute = Vec::new();
                    attribute.push(0);
                    attribute.push(0x01);
                    attribute.extend_from_slice(&port.to_be_bytes());
                    attribute.extend_from_slice(&address.to_be_bytes());
                    let mut body = Vec::new();
                    body.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
                    body.extend_from_slice(&(attribute.len() as u16).to_be_bytes());
                    body.extend_from_slice(&attribute);
                    out.extend_from_slice(&TYPE_SUCCESS.to_be_bytes());
                    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
                    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
                    out.extend_from_slice(&transaction);
                    out.extend_from_slice(&body);
                    out
                };
                let _ = probe;
                let _ = server.send_to(&reply, from).await;
            }
        });

        let client = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let mine = client.local_addr().unwrap();
        let learned = probe(&client, &server_address.to_string())
            .await
            .expect("the local server answers");
        assert_eq!(
            learned, mine,
            "the mapping the server saw is the socket that asked"
        );
    }
}
