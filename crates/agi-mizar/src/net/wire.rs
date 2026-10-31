//! The packet format, byte for byte.
//!
//! One UDP datagram is one packet, and the length field must equal the datagram
//! exactly: a length that disagrees is either corruption or something that is not
//! this protocol, and a tolerant parser would hand that ambiguity to the session
//! layer, where it would look like a truncated message.
//!
//! ```text
//! magic "MZP1" (4) | type (1) | connection id (8) | payload length (2) | payload
//! ```
//!
//! Every integer is big-endian. The connection id is a full 64-bit value — not a
//! `u32`, and not a JSON number — so the port from TypeScript does not repeat the
//! mistake of routing a packet to a connection that does not exist because a
//! double rounded it.

/// The four bytes every packet starts with.
pub const MAGIC: [u8; 4] = *b"MZP1";

/// The fixed part before any payload.
pub const HEADER_LEN: usize = 15;

/// The AEAD tag's length, also the smallest a sealed message can be.
pub const TAG_LEN: usize = 16;

/// What a packet is.
///
/// The hello types share one body: a `ClientHello` and a `ServerHello` differ
/// only in this byte, which is exactly why the byte is inside the signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    /// The dialler's hello — also the first punch probe.
    ClientHello = 1,
    /// The acceptor's answer.
    ServerHello = 2,
    /// One sealed message, with what has been received so far.
    Data = 3,
    /// Liveness, and a reason to send from this socket.
    Keepalive = 4,
    /// The connection is over, with a sentence for the log.
    Close = 5,
    /// Acknowledgements on their own.
    Ack = 6,
}

impl PacketType {
    /// The byte this type travels as.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// The type a byte names, when it names one this build knows.
    pub fn from_u8(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::ClientHello,
            2 => Self::ServerHello,
            3 => Self::Data,
            4 => Self::Keepalive,
            5 => Self::Close,
            6 => Self::Ack,
            _ => return None,
        })
    }

    /// Whether this type is a hello.
    pub fn is_hello(self) -> bool {
        matches!(self, Self::ClientHello | Self::ServerHello)
    }
}

/// What a packet carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketBody {
    /// A hello: an ephemeral key, the sender's name, and a signature over both.
    Hello {
        /// The sender's fresh X25519 public key.
        ephemeral: [u8; 32],
        /// The sender's base64url Ed25519 public key.
        endpoint_id: String,
        /// 64 bytes over [`Packet::handshake_message`].
        signature: [u8; 64],
    },
    /// One sealed message.
    Data {
        /// Monotonic per sender; also the AEAD nonce.
        number: u32,
        /// The highest packet numbers this sender has received.
        acks: Vec<u32>,
        /// The plaintext's length, inside the tag so a fragment cannot be reframed.
        message_length: u32,
        /// Ciphertext with the 16-byte tag appended.
        sealed: Vec<u8>,
    },
    /// Liveness.
    Keepalive,
    /// The end, and why.
    Close { reason: String },
    /// Acknowledgements alone.
    Ack { acks: Vec<u32> },
}

/// One packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// The connection this belongs to. Random over the whole 64-bit space.
    pub connection_id: u64,
    /// What the header's type byte says. The body alone cannot tell a
    /// `ClientHello` from a `ServerHello`, so the type is kept rather than
    /// inferred.
    pub kind: PacketType,
    /// What it carries.
    pub body: PacketBody,
}

/// Why a datagram is not a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// Shorter than the header.
    #[error("shorter than the header")]
    Truncated,
    /// Not our magic.
    #[error("not a MZP1 packet")]
    NotAPacket,
    /// The declared length and the datagram length disagree.
    #[error("the length field disagrees with the datagram")]
    LengthMismatch,
    /// A type byte this build does not know.
    #[error("unknown packet type {0}")]
    UnknownType(u8),
    /// The payload does not have the shape its type requires.
    #[error("malformed {0} payload")]
    Malformed(&'static str),
}

impl Packet {
    /// A hello packet.
    ///
    /// - Parameters:
    ///   - kind: which side is greeting.
    ///   - connection_id: the connection.
    ///   - ephemeral: this side's fresh X25519 public key.
    ///   - endpoint_id: this side's name.
    ///   - signature: over [`Packet::handshake_message`].
    /// - Returns: the packet.
    pub fn hello(
        kind: PacketType,
        connection_id: u64,
        ephemeral: [u8; 32],
        endpoint_id: impl Into<String>,
        signature: [u8; 64],
    ) -> Self {
        debug_assert!(kind.is_hello());
        Self {
            connection_id,
            kind,
            body: PacketBody::Hello {
                ephemeral,
                endpoint_id: endpoint_id.into(),
                signature,
            },
        }
    }

    /// A data packet.
    pub fn data(
        connection_id: u64,
        number: u32,
        acks: Vec<u32>,
        message_length: u32,
        sealed: Vec<u8>,
    ) -> Self {
        Self {
            connection_id,
            kind: PacketType::Data,
            body: PacketBody::Data {
                number,
                acks,
                message_length,
                sealed,
            },
        }
    }

    /// An acknowledgement packet.
    pub fn ack(connection_id: u64, acks: Vec<u32>) -> Self {
        Self {
            connection_id,
            kind: PacketType::Ack,
            body: PacketBody::Ack { acks },
        }
    }

    /// A keepalive packet.
    pub fn keepalive(connection_id: u64) -> Self {
        Self {
            connection_id,
            kind: PacketType::Keepalive,
            body: PacketBody::Keepalive,
        }
    }

    /// A close packet.
    pub fn close(connection_id: u64, reason: impl Into<String>) -> Self {
        Self {
            connection_id,
            kind: PacketType::Close,
            body: PacketBody::Close {
                reason: reason.into(),
            },
        }
    }

    /// The wire type.
    pub fn packet_type(&self) -> PacketType {
        self.kind
    }

    /// The bytes a hello's signature covers.
    ///
    /// `type (1) ‖ connection id (8) ‖ ephemeral (32) ‖ endpoint id (utf8)`.
    /// The type byte is a domain separator: the same key cannot turn a
    /// ClientHello into a ServerHello, and a connection id in the signature
    /// keeps a hello from being replayed into another connection.
    pub fn handshake_message(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(self.kind.as_u8());
        out.extend_from_slice(&self.connection_id.to_be_bytes());
        if let PacketBody::Hello {
            ephemeral,
            endpoint_id,
            ..
        } = &self.body
        {
            out.extend_from_slice(ephemeral);
            out.extend_from_slice(endpoint_id.as_bytes());
        }
        out
    }

    /// The bytes the AEAD authenticates.
    ///
    /// The header, the packet number and the message length — everything that
    /// must not be changed in flight, and nothing that is.
    pub fn authenticated_data(&self) -> Vec<u8> {
        let PacketBody::Data {
            number,
            message_length,
            sealed,
            ..
        } = &self.body
        else {
            return Vec::new();
        };
        let payload_len = 4 + 1 + 4 * acks_len(&self.body) + 4 + sealed.len();
        let mut out = header(self.kind.as_u8(), self.connection_id, payload_len);
        out.extend_from_slice(&number.to_be_bytes());
        out.extend_from_slice(&message_length.to_be_bytes());
        out
    }

    /// Encode to the bytes that go in one datagram.
    pub fn encode(&self) -> Vec<u8> {
        let payload = self.encode_payload();
        let mut out = header(self.kind.as_u8(), self.connection_id, payload.len());
        out.extend_from_slice(&payload);
        out
    }

    /// Decode one datagram.
    ///
    /// - Parameter bytes: exactly one datagram.
    /// - Returns: the packet.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < HEADER_LEN {
            return Err(WireError::Truncated);
        }
        if bytes[..4] != MAGIC {
            return Err(WireError::NotAPacket);
        }
        let kind = PacketType::from_u8(bytes[4]).ok_or(WireError::UnknownType(bytes[4]))?;
        let connection_id = u64::from_be_bytes(bytes[5..13].try_into().expect("8 bytes"));
        let declared = u16::from_be_bytes(bytes[13..15].try_into().expect("2 bytes")) as usize;
        let payload = &bytes[HEADER_LEN..];
        if declared != payload.len() {
            return Err(WireError::LengthMismatch);
        }
        let body = decode_payload(kind, payload)?;
        Ok(Self {
            connection_id,
            kind,
            body,
        })
    }

    fn encode_payload(&self) -> Vec<u8> {
        match &self.body {
            PacketBody::Hello {
                ephemeral,
                endpoint_id,
                signature,
            } => {
                let mut out = Vec::with_capacity(32 + 1 + endpoint_id.len() + 64);
                out.extend_from_slice(ephemeral);
                out.push(endpoint_id.len().min(255) as u8);
                out.extend_from_slice(endpoint_id.as_bytes());
                out.extend_from_slice(signature);
                out
            }
            PacketBody::Data {
                number,
                acks,
                message_length,
                sealed,
            } => {
                let mut out = Vec::with_capacity(4 + 1 + 4 * acks.len() + 4 + sealed.len());
                out.extend_from_slice(&number.to_be_bytes());
                out.push(acks.len().min(255) as u8);
                for ack in acks {
                    out.extend_from_slice(&ack.to_be_bytes());
                }
                out.extend_from_slice(&message_length.to_be_bytes());
                out.extend_from_slice(sealed);
                out
            }
            PacketBody::Keepalive => Vec::new(),
            PacketBody::Close { reason } => {
                let text = reason.as_bytes();
                let keep = text.len().min(255);
                let mut out = Vec::with_capacity(1 + keep);
                out.push(keep as u8);
                out.extend_from_slice(&text[..keep]);
                out
            }
            PacketBody::Ack { acks } => {
                let mut out = Vec::with_capacity(1 + 4 * acks.len());
                out.push(acks.len().min(255) as u8);
                for ack in acks {
                    out.extend_from_slice(&ack.to_be_bytes());
                }
                out
            }
        }
    }
}

fn acks_len(body: &PacketBody) -> usize {
    match body {
        PacketBody::Data { acks, .. } => acks.len(),
        _ => 0,
    }
}

/// The fixed header, with a payload length the caller computed.
///
/// Shared with `crypto`, which has to build the same bytes *before* sealing in
/// order to authenticate them: the length inside the tag is the sealed length,
/// so it cannot be filled in afterwards.
pub(crate) fn header(kind: u8, connection_id: u64, payload_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(&MAGIC);
    out.push(kind);
    out.extend_from_slice(&connection_id.to_be_bytes());
    out.extend_from_slice(&(payload_len.min(u16::MAX as usize) as u16).to_be_bytes());
    out
}

fn decode_payload(kind: PacketType, payload: &[u8]) -> Result<PacketBody, WireError> {
    match kind {
        PacketType::ClientHello | PacketType::ServerHello => {
            if payload.len() < 32 + 1 {
                return Err(WireError::Malformed("hello"));
            }
            let ephemeral: [u8; 32] = payload[..32].try_into().expect("32 bytes");
            let id_len = payload[32] as usize;
            let start = 33;
            let end = start + id_len;
            if payload.len() < end + 64 {
                return Err(WireError::Malformed("hello"));
            }
            let endpoint_id = String::from_utf8(payload[start..end].to_vec())
                .map_err(|_| WireError::Malformed("hello"))?;
            let signature: [u8; 64] = payload[end..end + 64]
                .try_into()
                .map_err(|_| WireError::Malformed("hello"))?;
            Ok(PacketBody::Hello {
                ephemeral,
                endpoint_id,
                signature,
            })
        }
        PacketType::Data => {
            if payload.len() < 9 {
                return Err(WireError::Malformed("data"));
            }
            let number = u32::from_be_bytes(payload[..4].try_into().expect("4 bytes"));
            let ack_count = payload[4] as usize;
            let acks_start = 5;
            let acks_end = acks_start + 4 * ack_count;
            if payload.len() < acks_end + 4 {
                return Err(WireError::Malformed("data"));
            }
            let mut acks = Vec::with_capacity(ack_count);
            for index in 0..ack_count {
                let at = acks_start + 4 * index;
                acks.push(u32::from_be_bytes(
                    payload[at..at + 4].try_into().expect("4 bytes"),
                ));
            }
            let message_length =
                u32::from_be_bytes(payload[acks_end..acks_end + 4].try_into().expect("4 bytes"));
            let sealed = payload[acks_end + 4..].to_vec();
            // A sealed message is a tag and nothing else at the smallest, and the
            // length it claims is what the tag is expected to open into.
            if sealed.len() < TAG_LEN || message_length as usize > sealed.len() {
                return Err(WireError::Malformed("data"));
            }
            Ok(PacketBody::Data {
                number,
                acks,
                message_length,
                sealed,
            })
        }
        PacketType::Keepalive => {
            if payload.is_empty() {
                Ok(PacketBody::Keepalive)
            } else {
                Err(WireError::Malformed("keepalive"))
            }
        }
        PacketType::Close => {
            let Some(length) = payload.first() else {
                return Err(WireError::Malformed("close"));
            };
            let length = *length as usize;
            if payload.len() < 1 + length {
                return Err(WireError::Malformed("close"));
            }
            let reason = String::from_utf8(payload[1..1 + length].to_vec())
                .map_err(|_| WireError::Malformed("close"))?;
            Ok(PacketBody::Close { reason })
        }
        PacketType::Ack => {
            let Some(count) = payload.first() else {
                return Err(WireError::Malformed("ack"));
            };
            let count = *count as usize;
            if payload.len() < 1 + 4 * count {
                return Err(WireError::Malformed("ack"));
            }
            let mut acks = Vec::with_capacity(count);
            for index in 0..count {
                let at = 1 + 4 * index;
                acks.push(u32::from_be_bytes(
                    payload[at..at + 4].try_into().expect("4 bytes"),
                ));
            }
            Ok(PacketBody::Ack { acks })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(kind: PacketType) -> Packet {
        Packet::hello(
            kind,
            0x0102_0304_0506_0708,
            [7; 32],
            "an-endpoint-id",
            [9; 64],
        )
    }

    #[test]
    fn a_hello_round_trips() {
        let packet = hello(PacketType::ClientHello);
        let bytes = packet.encode();
        assert_eq!(bytes.len(), HEADER_LEN + 32 + 1 + 14 + 64);
        let decoded = Packet::decode(&bytes).expect("decodes");
        assert_eq!(decoded, packet);
        // The signature is over the type, the connection id, the key and the id --
        // and not over the signature itself.
        assert_eq!(
            packet.handshake_message().len(),
            1 + 8 + 32 + "an-endpoint-id".len()
        );
    }

    #[test]
    fn the_two_hellos_are_distinguishable_and_sign_different_bytes() {
        // The type byte is a domain separator, which is what stops a hello being
        // reflected back at whoever sent it.
        let client = hello(PacketType::ClientHello);
        let server = hello(PacketType::ServerHello);
        assert_ne!(client.encode(), server.encode());
        assert_ne!(client.handshake_message(), server.handshake_message());
        assert_eq!(
            Packet::decode(&server.encode()).expect("decodes").kind,
            PacketType::ServerHello
        );
    }

    #[test]
    fn a_datagram_that_is_not_one_of_ours_is_refused() {
        assert_eq!(Packet::decode(&[0; 20]), Err(WireError::NotAPacket));
        assert_eq!(Packet::decode(&[0; 4]), Err(WireError::Truncated));
        let mut bytes = hello(PacketType::ClientHello).encode();
        bytes[4] = 99;
        assert_eq!(Packet::decode(&bytes), Err(WireError::UnknownType(99)));
    }

    #[test]
    fn the_length_field_is_the_datagram_length() {
        let mut bytes = Packet::keepalive(1).encode();
        assert_eq!(bytes.len(), HEADER_LEN);
        bytes.push(0xAA);
        assert_eq!(Packet::decode(&bytes), Err(WireError::LengthMismatch));
    }

    #[test]
    fn a_close_carries_its_reason_and_nothing_after_it() {
        let packet = Packet::close(5, "the operator closed it");
        let decoded = Packet::decode(&packet.encode()).expect("decodes");
        assert_eq!(decoded, packet);
    }

    #[test]
    fn a_data_packet_round_trips_with_its_acks() {
        let packet = Packet::data(9, 41, vec![40, 39, 38], 5, vec![0xAB; 5 + TAG_LEN]);
        let decoded = Packet::decode(&packet.encode()).expect("decodes");
        assert_eq!(decoded, packet);
        assert_eq!(
            decoded.authenticated_data().len(),
            HEADER_LEN + 4 + 4,
            "the aad is the header, the number and the length"
        );
    }
}
