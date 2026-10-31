//! The handshake's result, and the per-packet seal.
//!
//! Nothing in this file is negotiable: the salt's info string, the two HKDF
//! labels and the AEAD are the wire contract, and changing any one of them stops
//! every existing session from opening. `protocol/fixtures/p2p/wire.json` pins
//! them against the phone's implementation, which is why they are written as
//! constants with the reason next to them rather than inline.
//!
//! ```text
//! salt    = SHA-256("mizar-net/session/v1" ‖ sorted(id1, id2) 各接 0x00)
//! k_dir   = HKDF-SHA256(ikm = X25519 shared, salt, info =
//!           "mizar-net/dialer-to-acceptor/v1" | "mizar-net/acceptor-to-dialer/v1")
//! nonce   = 4 × 0x00 ‖ packet number (8 bytes, big-endian)
//! AEAD    = ChaCha20-Poly1305, AAD = header ‖ packet number ‖ message length
//! ```
//!
//! The packet number *is* the nonce: nothing on the wire repeats it, and there is
//! no nonce an attacker could choose. It is also the sequence number the session
//! layer acknowledges, so one number serves both purposes.

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

use super::wire::{self, Packet, PacketBody, PacketType, TAG_LEN};

/// Mixed into the salt. Part of the wire contract.
pub const SESSION_SALT_INFO: &str = "mizar-net/session/v1";
/// HKDF label for the dialler's key. Part of the wire contract.
pub const DIALER_TO_ACCEPTOR_INFO: &str = "mizar-net/dialer-to-acceptor/v1";
/// HKDF label for the acceptor's key. Part of the wire contract.
pub const ACCEPTOR_TO_DIALER_INFO: &str = "mizar-net/acceptor-to-dialer/v1";

/// The two session keys, from neither side's point of view.
///
/// Which one seals is decided by the role, not stored: a dialler seals with
/// `dialer_to_acceptor` and opens with the other, and the acceptor does the
/// reverse. Keeping both here rather than a `seal`/`open` pair is what lets a
/// test check that the keys are what the fixture says they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionKeys {
    /// The key the dialler seals with.
    pub dialer_to_acceptor: [u8; 32],
    /// The key the acceptor seals with.
    pub acceptor_to_dialer: [u8; 32],
}

impl SessionKeys {
    /// The key this side seals with.
    ///
    /// - Parameter local_is_dialer: which side is asking.
    /// - Returns: the key.
    pub fn sealing(&self, local_is_dialer: bool) -> &[u8; 32] {
        if local_is_dialer {
            &self.dialer_to_acceptor
        } else {
            &self.acceptor_to_dialer
        }
    }

    /// The key this side opens with.
    ///
    /// - Parameter local_is_dialer: which side is asking.
    /// - Returns: the key.
    pub fn opening(&self, local_is_dialer: bool) -> &[u8; 32] {
        if local_is_dialer {
            &self.acceptor_to_dialer
        } else {
            &self.dialer_to_acceptor
        }
    }
}

/// Why a packet could not be sealed or opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    /// The AEAD refused: a wrong key, a changed header, or a corrupt packet.
    #[error("the packet did not open")]
    Open,
    /// The packet is not a data packet, so there is nothing to open.
    #[error("not a data packet")]
    NotData,
    /// The message length inside the tag disagrees with what came out.
    #[error("the message length inside the tag disagrees with the plaintext")]
    LengthMismatch,
}

/// The salt that binds a session to its two peers.
///
/// Sorted, so both ends compute the same salt without negotiating who is first;
/// the NUL after each id keeps `"ab"+"c"` from hashing like `"a"+"bc"`.
///
/// - Parameters:
///   - local_id: this endpoint's base64url id.
///   - remote_id: the peer's.
/// - Returns: the 32-byte salt.
pub fn session_salt(local_id: &str, remote_id: &str) -> [u8; 32] {
    let mut ids = [local_id, remote_id];
    ids.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(SESSION_SALT_INFO.as_bytes());
    for id in ids {
        hasher.update(id.as_bytes());
        hasher.update([0x00]);
    }
    hasher.finalize().into()
}

/// The two directional keys.
///
/// - Parameters:
///   - shared: the X25519 shared secret.
///   - salt: [`session_salt`] for the two endpoints.
/// - Returns: the keys.
pub fn session_keys(shared: &[u8; 32], salt: &[u8; 32]) -> SessionKeys {
    SessionKeys {
        dialer_to_acceptor: expand(shared, salt, DIALER_TO_ACCEPTOR_INFO),
        acceptor_to_dialer: expand(shared, salt, ACCEPTOR_TO_DIALER_INFO),
    }
}

fn expand(shared: &[u8; 32], salt: &[u8; 32], info: &str) -> [u8; 32] {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), shared);
    let mut out = [0u8; 32];
    // 32 bytes is well inside HKDF's limit for SHA-256, so this cannot fail.
    let _ = hkdf.expand(info.as_bytes(), &mut out);
    out
}

/// The ephemeral-ephemeral agreement.
///
/// - Parameters:
///   - local_secret: this side's ephemeral private key.
///   - remote_public: the peer's ephemeral public key from its hello.
/// - Returns: the shared secret.
pub fn agree(local_secret: &[u8; 32], remote_public: &[u8; 32]) -> [u8; 32] {
    let secret = StaticSecret::from(*local_secret);
    let remote = PublicKey::from(*remote_public);
    secret.diffie_hellman(&remote).to_bytes()
}

/// A fresh ephemeral key pair, and its raw public half.
///
/// - Returns: the private key and the 32 bytes that travel in the hello.
pub fn ephemeral_keypair() -> ([u8; 32], [u8; 32]) {
    use rand::RngCore;
    let mut secret = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut secret);
    let secret = StaticSecret::from(secret);
    let public = PublicKey::from(&secret);
    (secret.to_bytes(), public.to_bytes())
}

/// A fresh connection id.
///
/// Over the whole 64-bit space, and never zero: a zero id reads like an
/// uninitialised field in a log.
pub fn connection_id() -> u64 {
    use rand::RngCore;
    loop {
        let mut bytes = [0u8; 8];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let id = u64::from_be_bytes(bytes);
        if id != 0 {
            return id;
        }
    }
}

/// The AEAD nonce for one packet number.
///
/// - Parameter packet_number: the number carried in the header.
/// - Returns: twelve bytes: four of zero, then the number big-endian.
pub fn nonce(packet_number: u32) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[4..].copy_from_slice(&u64::from(packet_number).to_be_bytes());
    out
}

/// Seal one message into a data packet.
///
/// - Parameters:
///   - key: this side's sealing key.
///   - connection_id: the connection.
///   - number: the packet number, which is also the nonce.
///   - acks: what has been received so far.
///   - plaintext: the message.
/// - Returns: the packet, ready to send.
pub fn seal_packet(
    key: &[u8; 32],
    connection_id: u64,
    number: u32,
    acks: Vec<u32>,
    plaintext: &[u8],
) -> Packet {
    seal_chunk(
        key,
        connection_id,
        number,
        acks,
        plaintext.len() as u32,
        plaintext,
    )
}

/// Seal one *chunk* of a message.
///
/// A message larger than a datagram is split, and every chunk carries the
/// whole message's length inside the tag so the receiver knows when it has the
/// lot. The chunk's own length is not on the wire at all: it is the sealed
/// length minus the tag, which is exactly what the receiver reads.
///
/// - Parameters:
///   - key: this side's sealing key.
///   - connection_id: the connection.
///   - number: the packet number, which is also the nonce.
///   - acks: what has been received so far.
///   - message_length: the length of the whole message.
///   - chunk: this packet's share of it.
/// - Returns: the packet.
pub fn seal_chunk(
    key: &[u8; 32],
    connection_id: u64,
    number: u32,
    acks: Vec<u32>,
    message_length: u32,
    chunk: &[u8],
) -> Packet {
    let sealed_len = chunk.len() + TAG_LEN;
    let payload_len = 4 + 1 + 4 * acks.len() + 4 + sealed_len;
    let mut aad = wire::header(PacketType::Data.as_u8(), connection_id, payload_len);
    aad.extend_from_slice(&number.to_be_bytes());
    aad.extend_from_slice(&message_length.to_be_bytes());

    let cipher = ChaCha20Poly1305::new(key.into());
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce(number)),
            Payload {
                msg: chunk,
                aad: &aad,
            },
        )
        // The only failure this can have is a chunk too large for a datagram,
        // which the session layer refuses before it gets here.
        .expect("a datagram-sized chunk seals");

    Packet::data(connection_id, number, acks, message_length, sealed)
}

/// Open one data packet.
///
/// - Parameters:
///   - packet: the packet, already decoded.
///   - key: this side's opening key.
/// - Returns: the plaintext.
pub fn open_packet(packet: &Packet, key: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    let PacketBody::Data {
        number,
        message_length,
        sealed,
        ..
    } = &packet.body
    else {
        return Err(CryptoError::NotData);
    };
    let cipher = ChaCha20Poly1305::new(key.into());
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&nonce(*number)),
            Payload {
                msg: sealed,
                aad: &packet.authenticated_data(),
            },
        )
        .map_err(|_| CryptoError::Open)?;
    // A message larger than a datagram arrives in chunks, so the plaintext here
    // is a *chunk* and is normally shorter than the message. What must hold is
    // the invariant a chunk cannot violate: no chunk is longer than the message
    // it belongs to. A sender that breaks that is framing its own messages
    // wrongly, which is worth refusing rather than reassembling into nonsense.
    if plaintext.len() > *message_length as usize {
        return Err(CryptoError::LengthMismatch);
    }
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_salt_does_not_depend_on_which_side_computes_it() {
        let one = session_salt("A", "B");
        let two = session_salt("B", "A");
        assert_eq!(one, two);
    }

    #[test]
    fn two_different_pairs_do_not_share_a_salt() {
        assert_ne!(session_salt("A", "B"), session_salt("A", "C"));
        // The NUL separator is what keeps these apart.
        assert_ne!(session_salt("ab", "c"), session_salt("a", "bc"));
    }

    #[test]
    fn the_directional_keys_are_different_and_role_ordered() {
        let salt = session_salt("A", "B");
        let keys = session_keys(&[7; 32], &salt);
        assert_ne!(keys.dialer_to_acceptor, keys.acceptor_to_dialer);
        assert_eq!(keys.sealing(true), &keys.dialer_to_acceptor);
        assert_eq!(keys.sealing(false), &keys.acceptor_to_dialer);
        assert_eq!(keys.opening(true), &keys.acceptor_to_dialer);
        assert_eq!(keys.opening(false), &keys.dialer_to_acceptor);
    }

    #[test]
    fn a_packet_sealed_by_one_side_opens_on_the_other_and_only_there() {
        let salt = session_salt("A", "B");
        let keys = session_keys(&[7; 32], &salt);
        let packet = seal_packet(keys.sealing(true), 42, 7, vec![6, 5], b"hello mizar");

        assert_eq!(
            open_packet(&packet, keys.opening(false)).expect("the peer opens it"),
            b"hello mizar"
        );
        // The AEAD is symmetric: the sealing key opens its own packet. What must
        // not open it is the *other direction's* key, which is what
        // `keys.opening(true)` is for the dialler.
        assert_eq!(
            open_packet(&packet, keys.opening(true)),
            Err(CryptoError::Open),
            "a dialler does not open its own packets with the acceptor's key"
        );
    }

    #[test]
    fn changing_the_header_or_the_number_breaks_the_tag() {
        let salt = session_salt("A", "B");
        let keys = session_keys(&[7; 32], &salt);
        let packet = seal_packet(keys.sealing(true), 42, 7, Vec::new(), b"hi");
        let key = *keys.opening(false);
        assert!(open_packet(&packet, &key).is_ok());

        let mut moved = packet.clone();
        moved.connection_id = 43;
        assert_eq!(open_packet(&moved, &key), Err(CryptoError::Open));

        let mut renumbered = packet;
        if let PacketBody::Data { number, .. } = &mut renumbered.body {
            *number = 8;
        }
        assert_eq!(open_packet(&renumbered, &key), Err(CryptoError::Open));
    }

    #[test]
    fn only_data_packets_can_be_opened() {
        let keys = session_keys(&[7; 32], &session_salt("A", "B"));
        assert_eq!(
            open_packet(&Packet::keepalive(1), keys.sealing(true)),
            Err(CryptoError::NotData)
        );
    }
}
