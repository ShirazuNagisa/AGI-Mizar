//! The endpoint key: who this computer is on the direct path.
//!
//! There is no new credential here. The key pair is **derived** from the tunnel
//! secret the pairing already trusts, so:
//!
//! - nothing extra is stored, and a reinstall keeps the same endpoint id;
//! - the phone learns the id once (`host.info.netEndpointId`) and can dial it
//!   for ever after — the ticket names a key, not an address, because neither
//!   side knows its own public address until the punch is under way;
//! - an attacker who can reach the UDP port still cannot pretend to be this
//!   computer without the tunnel secret.
//!
//! The construction is `SHA-256(secret ‖ purpose ‖ 0x01)`; the purpose string is
//! what keeps a host key from ever being usable as a device key, even if the two
//! sides were handed the same secret.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

/// Length of an Ed25519 signature, in bytes.
pub const SIGNATURE_LENGTH: usize = 64;

/// Length of a public key, in bytes.
pub const PUBLIC_KEY_LENGTH: usize = 32;

/// Characters in a Crockford base32 short id.
const SHORT_ID_LENGTH: usize = 8;

/// Crockford base32: digits plus letters, without `I`, `L`, `O` and `U`.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// What a derived key is for.
///
/// Part of the wire contract: the two strings are mixed into the seed, so a
/// desktop and a phone that used the same secret would still get different keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// This computer.
    Host,
    /// A phone.
    Device,
}

impl Purpose {
    /// The string mixed into the seed.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "mizar-net/host/v1",
            Self::Device => "mizar-net/device/v1",
        }
    }
}

/// One endpoint's key pair, and the two ways it is named.
pub struct Identity {
    signing: SigningKey,
    endpoint_id: String,
    short_id: String,
}

impl Identity {
    /// Derive this endpoint's key from a secret.
    ///
    /// - Parameters:
    ///   - secret: high-entropy bytes the pairing already established — the
    ///     tunnel secret on the desktop, the device token on the phone.
    ///   - purpose: what the key is for.
    /// - Returns: the identity.
    pub fn derive(secret: &[u8], purpose: Purpose) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(secret);
        hasher.update(purpose.as_str().as_bytes());
        hasher.update([0x01]);
        let seed: [u8; 32] = hasher.finalize().into();

        let signing = SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();
        Self {
            signing,
            endpoint_id: URL_SAFE_NO_PAD.encode(public),
            short_id: crockford(&public, SHORT_ID_LENGTH),
        }
    }

    /// The base64url public key. What a phone's ticket names.
    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    /// Eight base32 characters, for a log line.
    pub fn short_id(&self) -> &str {
        &self.short_id
    }

    /// The raw 32-byte public key.
    pub fn public_key(&self) -> [u8; PUBLIC_KEY_LENGTH] {
        self.signing.verifying_key().to_bytes()
    }

    /// Sign the bytes a hello covers.
    ///
    /// Ed25519 signatures are deterministic in this implementation and random
    /// under CryptoKit. Nothing may depend on either property: the far end
    /// verifies, and that is all.
    ///
    /// - Parameter message: the bytes to sign.
    /// - Returns: the 64-byte signature.
    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LENGTH] {
        self.signing.sign(message).to_bytes()
    }
}

/// Whether a signature is this endpoint's, over this message.
///
/// - Parameters:
///   - signature: the 64 bytes from a hello.
///   - message: the bytes the hello covers.
///   - endpoint_id: the base64url public key the ticket named.
/// - Returns: whether it verifies. Anything malformed is `false`, never an error:
///   an unverifiable hello is a stranger, not a bug.
pub fn verify_signature(signature: &[u8], message: &[u8], endpoint_id: &str) -> bool {
    let Ok(raw) = URL_SAFE_NO_PAD.decode(endpoint_id) else {
        return false;
    };
    let Ok(public) = <[u8; PUBLIC_KEY_LENGTH]>::try_from(raw.as_slice()) else {
        return false;
    };
    let Ok(signature) = <[u8; SIGNATURE_LENGTH]>::try_from(signature) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&public) else {
        return false;
    };
    key.verify(message, &Signature::from_bytes(&signature)).is_ok()
}

/// Encode bytes as Crockford base32, taking the first `characters` of it.
///
/// - Parameters:
///   - bytes: the input.
///   - characters: how many base32 characters to keep.
/// - Returns: the short id.
fn crockford(bytes: &[u8], characters: usize) -> String {
    let mut out = String::with_capacity(characters);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 && out.len() < characters {
            bits -= 5;
            out.push(CROCKFORD[((buffer >> bits) & 0b1_1111) as usize] as char);
        }
        if out.len() >= characters {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_id_is_eight_crockford_characters() {
        // The alphabet has no I, L, O or U: a short id is read off a screen.
        let id = crockford(&[0xFF; 8], SHORT_ID_LENGTH);
        assert_eq!(id.len(), SHORT_ID_LENGTH);
        assert!(!id.contains('I') && !id.contains('L') && !id.contains('O') && !id.contains('U'));
    }

    #[test]
    fn a_derived_identity_is_stable_and_purpose_bound() {
        let secret = b"the tunnel secret";
        let first = Identity::derive(secret, Purpose::Host);
        let second = Identity::derive(secret, Purpose::Host);
        assert_eq!(first.endpoint_id(), second.endpoint_id());
        assert_eq!(first.public_key(), second.public_key());

        // The same secret under the other purpose is a different endpoint, which
        // is what stops a host key from being usable as a phone key.
        let device = Identity::derive(secret, Purpose::Device);
        assert_ne!(first.endpoint_id(), device.endpoint_id());
    }

    #[test]
    fn a_signature_verifies_against_its_own_key_and_no_other() {
        let one = Identity::derive(b"one", Purpose::Host);
        let two = Identity::derive(b"two", Purpose::Host);
        let message = b"hello";
        let signature = one.sign(message);

        assert!(verify_signature(&signature, message, one.endpoint_id()));
        assert!(!verify_signature(&signature, message, two.endpoint_id()));
        assert!(!verify_signature(&signature, b"other", one.endpoint_id()));
    }
}
