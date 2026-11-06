//! Same-account LAN identity, certificates and discovery.
//!
//! This crate starts the LAN path with the part that must be right before a
//! socket is opened: a stable device keypair, an account-CA-signed certificate,
//! and a beacon that carries an account tag rather than an account id. It does
//! **not** open a listener or an insecure tunnel; Noise and the 19393 server
//! belong to the next layer, after the certificate is verified.
//!
//! The certificate is a signed JSON body, not X.509. The account service is the
//! only issuer, the body carries both public keys (Ed25519 for signing, X25519
//! for Noise), and the signature is over canonical JSON. A verifier needs only
//! the account CA public key it already trusts.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub mod discovery;
pub mod mdns;
pub mod mizar;
pub use discovery::{LanDiscovery, DISCOVERY_PORT};
pub use mdns::{MdnsAdvertiser, MdnsBrowser, SERVICE_TYPE};
pub use mizar::{LanMessages, LanMizarClient, LanTurnResult};
pub mod noise;
pub use noise::{
    connect, LanCredentials, LanHello, LanListener, LanSession, LanTrust, RemoteDevice,
};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x25519_dalek::{PublicKey as X25519Public, StaticSecret};

/// The transport port a LAN peer dials after discovery.
pub const LAN_PORT: u16 = 19_393;

/// Bumped when the stored key file or certificate body changes shape.
pub const FORMAT_VERSION: u32 = 1;

/// A certificate older than this is useless even if the clock is wrong.
pub const MAX_CERTIFICATE_LIFETIME_MS: i64 = 90 * 24 * 60 * 60 * 1000;

/// How much clock skew a certificate's `issuedAt` may be in the future.
const CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, Error)]
pub enum LanError {
    #[error("the key file is not readable: {0}")]
    Io(#[from] std::io::Error),
    #[error("the key file is not valid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the key material is malformed")]
    Key,
    #[error("the certificate signature is not valid")]
    Signature,
    #[error("the certificate is for another account or device")]
    WrongIdentity,
    #[error("the certificate has expired")]
    Expired,
    #[error("the certificate was issued in the future")]
    NotYetValid,
    #[error("the certificate lives longer than the CA allows")]
    TooLong,
    #[error("the beacon is not a version this build understands")]
    BadBeacon,
    #[error("the Noise handshake failed: {0}")]
    Noise(String),
    #[error("the peer refused the request: {0}")]
    Remote(String),
}

/// The private half of this machine's LAN identity.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredKeys {
    v: u32,
    ed25519_secret: String,
    x25519_secret: String,
}

/// One machine's LAN identity, with both keys derived.
#[derive(Clone)]
pub struct DeviceIdentity {
    ed25519: SigningKey,
    x25519: StaticSecret,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceIdentity")
            .field("ed25519", &self.ed25519.verifying_key().to_bytes())
            .finish_non_exhaustive()
    }
}

impl DeviceIdentity {
    /// Load the identity, or create it 0600 on first use.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, LanError> {
        let path = path.as_ref();
        if let Ok(text) = std::fs::read_to_string(path) {
            return Self::from_json(&text);
        }
        let identity = Self::generate();
        identity.save(path)?;
        Ok(identity)
    }

    /// A fresh identity, not yet written.
    pub fn generate() -> Self {
        Self {
            ed25519: SigningKey::generate(&mut OsRng),
            x25519: StaticSecret::random_from_rng(OsRng),
        }
    }

    fn from_json(text: &str) -> Result<Self, LanError> {
        let stored: StoredKeys = serde_json::from_str(text)?;
        if stored.v != FORMAT_VERSION {
            return Err(LanError::Key);
        }
        let ed: [u8; 32] = B64
            .decode(stored.ed25519_secret)
            .map_err(|_| LanError::Key)?
            .try_into()
            .map_err(|_| LanError::Key)?;
        let x: [u8; 32] = B64
            .decode(stored.x25519_secret)
            .map_err(|_| LanError::Key)?
            .try_into()
            .map_err(|_| LanError::Key)?;
        Ok(Self {
            ed25519: SigningKey::from_bytes(&ed),
            x25519: StaticSecret::from(x),
        })
    }

    /// Write both secrets as a private file. A directory is created 0700.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), LanError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let stored = StoredKeys {
            v: FORMAT_VERSION,
            ed25519_secret: B64.encode(self.ed25519.to_bytes()),
            x25519_secret: B64.encode(self.x25519.to_bytes()),
        };
        let bytes = serde_json::to_vec_pretty(&stored)?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&temp, path)?;
        Ok(())
    }

    /// The X25519 private key, for the Noise layer only.
    pub(crate) fn noise_private_key(&self) -> [u8; 32] {
        self.x25519.to_bytes()
    }

    /// The public half, as the certificate carries it.
    pub fn public(&self) -> PublicIdentity {
        PublicIdentity {
            ed25519: self.ed25519.verifying_key().to_bytes(),
            x25519: X25519Public::from(&self.x25519).to_bytes(),
        }
    }

    /// Sign an arbitrary message, for tests and future handshake transcripts.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.ed25519.sign(message).to_bytes()
    }

    /// The public identity in the JSON shape the relay registers.
    pub fn public_json(&self) -> serde_json::Value {
        let public = self.public();
        serde_json::json!({
            "ed25519": B64.encode(public.ed25519),
            "x25519": B64.encode(public.x25519),
        })
    }
}

/// The two public keys a peer needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicIdentity {
    pub ed25519: [u8; 32],
    pub x25519: [u8; 32],
}

/// The signed part of a device certificate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CertificateBody {
    pub v: u32,
    pub account_id: String,
    pub device_id: String,
    /// Base64 Ed25519 public key.
    pub ed25519_public: String,
    /// Base64 X25519 public key.
    pub x25519_public: String,
    pub issued_at: i64,
    pub expires_at: i64,
}

impl CertificateBody {
    /// The bytes the account CA signs. Field order is fixed by this struct.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn ed25519_key(&self) -> Result<[u8; 32], LanError> {
        B64.decode(&self.ed25519_public)
            .map_err(|_| LanError::Key)?
            .try_into()
            .map_err(|_| LanError::Key)
    }

    pub fn x25519_key(&self) -> Result<[u8; 32], LanError> {
        B64.decode(&self.x25519_public)
            .map_err(|_| LanError::Key)?
            .try_into()
            .map_err(|_| LanError::Key)
    }
}

/// A device certificate: canonical body plus the account CA's signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCertificate {
    pub body: CertificateBody,
    /// Base64 Ed25519 signature over `body.canonical_bytes()`.
    pub signature: String,
}

impl DeviceCertificate {
    /// Sign a body with an account CA key. The relay owns the production CA;
    /// this constructor exists so the verifier and tests share one format.
    pub fn sign(body: CertificateBody, ca: &SigningKey) -> Self {
        let signature = ca.sign(&body.canonical_bytes());
        Self {
            body,
            signature: B64.encode(signature.to_bytes()),
        }
    }

    /// Check the CA signature, identity and lifetime.
    pub fn verify(
        &self,
        ca_public: &[u8; 32],
        account_id: &str,
        device_id: &str,
        now_ms: i64,
    ) -> Result<(), LanError> {
        if self.body.v != FORMAT_VERSION {
            return Err(LanError::Key);
        }
        if self.body.account_id != account_id || self.body.device_id != device_id {
            return Err(LanError::WrongIdentity);
        }
        if self.body.expires_at <= now_ms {
            return Err(LanError::Expired);
        }
        if self.body.issued_at > now_ms + CLOCK_SKEW_MS {
            return Err(LanError::NotYetValid);
        }
        if self.body.expires_at - self.body.issued_at > MAX_CERTIFICATE_LIFETIME_MS {
            return Err(LanError::TooLong);
        }
        let signature: [u8; 64] = B64
            .decode(&self.signature)
            .map_err(|_| LanError::Signature)?
            .try_into()
            .map_err(|_| LanError::Signature)?;
        let verifying = VerifyingKey::from_bytes(ca_public).map_err(|_| LanError::Key)?;
        verifying
            .verify(
                &self.body.canonical_bytes(),
                &Signature::from_bytes(&signature),
            )
            .map_err(|_| LanError::Signature)
    }

    /// A short fingerprint for the beacon, so the handshake can name a certificate
    /// without putting it in a broadcast packet.
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.signature.as_bytes());
        digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// Base64 one public key, the wire shape the relay and certificates use.
pub fn public_key_b64(key: [u8; 32]) -> String {
    B64.encode(key)
}

/// Decode one raw 32-byte base64 public key.
pub fn decode_public_key(text: &str) -> Result<[u8; 32], LanError> {
    B64.decode(text)
        .map_err(|_| LanError::Key)?
        .try_into()
        .map_err(|_| LanError::Key)
}

/// The account tag a LAN beacon carries.
///
/// A hash rather than the account id: the beacon is readable by every machine on
/// the LAN, and the tag only needs to distinguish accounts, not name one.
pub fn account_tag(account_id: &str) -> String {
    let digest = Sha256::digest(account_id.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One discovery packet. It deliberately contains no secret and no session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LanBeacon {
    pub v: u32,
    pub account_tag: String,
    pub device_id: String,
    pub name: String,
    pub port: u16,
    pub certificate_fingerprint: String,
}

impl LanBeacon {
    pub fn new(
        account_id: &str,
        device_id: impl Into<String>,
        name: impl Into<String>,
        port: u16,
        certificate: &DeviceCertificate,
    ) -> Self {
        Self {
            v: FORMAT_VERSION,
            account_tag: account_tag(account_id),
            device_id: device_id.into(),
            name: name.into(),
            port,
            certificate_fingerprint: certificate.fingerprint(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LanError> {
        let beacon: Self = serde_json::from_slice(bytes)?;
        if beacon.v != FORMAT_VERSION || beacon.port == 0 {
            return Err(LanError::BadBeacon);
        }
        Ok(beacon)
    }

    /// Whether this beacon belongs to the account the caller is signed in to.
    pub fn matches_account(&self, account_id: &str) -> bool {
        self.account_tag == account_tag(account_id)
    }
}

/// Where the identity lives beside the other per-install state.
pub fn default_identity_path() -> PathBuf {
    agi_platform::data_dir().join("lan-identity.json")
}

/// Now in epoch milliseconds, the unit certificates use.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn body(now: i64) -> CertificateBody {
        let identity = DeviceIdentity::generate();
        let public = identity.public();
        CertificateBody {
            v: FORMAT_VERSION,
            account_id: "acct-1".to_string(),
            device_id: "device-1".to_string(),
            ed25519_public: B64.encode(public.ed25519),
            x25519_public: B64.encode(public.x25519),
            issued_at: now,
            expires_at: now + 86_400_000,
        }
    }

    #[test]
    fn keys_survive_a_reload_and_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lan-identity.json");
        let first = DeviceIdentity::load_or_create(&path).unwrap();
        let second = DeviceIdentity::load_or_create(&path).unwrap();
        assert_eq!(first.public(), second.public());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_certificate_verifies_and_tampering_is_refused() {
        let now = now_ms();
        let authority = ca();
        let mut certificate = DeviceCertificate::sign(body(now), &authority);
        let ca_public = authority.verifying_key().to_bytes();
        certificate
            .verify(&ca_public, "acct-1", "device-1", now)
            .unwrap();
        certificate.body.device_id = "device-2".to_string();
        assert!(matches!(
            certificate.verify(&ca_public, "acct-1", "device-2", now),
            Err(LanError::Signature)
        ));
    }

    #[test]
    fn expiry_wrong_account_and_a_different_ca_are_refused() {
        let now = now_ms();
        let authority = ca();
        let certificate = DeviceCertificate::sign(body(now), &authority);
        let ca_public = authority.verifying_key().to_bytes();
        assert!(matches!(
            certificate.verify(&ca_public, "acct-2", "device-1", now),
            Err(LanError::WrongIdentity)
        ));
        assert!(matches!(
            certificate.verify(&ca_public, "acct-1", "device-1", now + 2 * 86_400_000),
            Err(LanError::Expired)
        ));
        let other = ca();
        assert!(matches!(
            certificate.verify(&other.verifying_key().to_bytes(), "acct-1", "device-1", now),
            Err(LanError::Signature)
        ));
    }

    #[test]
    fn a_beacon_names_the_account_but_does_not_leak_it() {
        let now = now_ms();
        let authority = ca();
        let certificate = DeviceCertificate::sign(body(now), &authority);
        let beacon = LanBeacon::new("acct-1", "device-1", "Mac", 19393, &certificate);
        let wire = beacon.encode();
        assert!(!String::from_utf8_lossy(&wire).contains("acct-1"));
        let decoded = LanBeacon::decode(&wire).unwrap();
        assert!(decoded.matches_account("acct-1"));
        assert!(!decoded.matches_account("acct-2"));
        assert_eq!(decoded.certificate_fingerprint, certificate.fingerprint());
    }
}
