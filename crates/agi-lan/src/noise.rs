//! Noise XX over TCP, with the account certificate as the authorisation.
//!
//! Noise XX authenticates both static X25519 keys without either side knowing
//! the other's key in advance. That is exactly the LAN case: any desktop of the
//! same account may appear, and the account CA decides which ones are allowed.
//!
//! The handshake carries a `LanHello` payload on message 3 and message 4. The
//! certificate inside is signed by the account CA; the verifier additionally
//! checks that the certificate's X25519 public key is the Noise static key that
//! just completed the handshake. A stolen certificate without the private key
//! therefore fails at the handshake, and a valid key from another account fails
//! at the certificate check.
//!
//! Transport messages are length-prefixed (u16 big-endian) Noise transport
//! messages. The Mizar frame layer sits on top of this and is intentionally not
//! part of this module.

use std::net::SocketAddr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use snow::{Builder, TransportState};
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as AsyncMutex;

use crate::{account_tag, now_ms, DeviceCertificate, DeviceIdentity, LanError, FORMAT_VERSION};

/// The Noise pattern. ChaChaPoly and BLAKE2s are the portable 25519 suite.
const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// The largest handshake or transport message this layer accepts.
const MAX_MESSAGE: usize = 65_535;

/// What a peer is allowed to be.
#[derive(Debug, Clone)]
pub struct LanTrust {
    /// Raw Ed25519 public key of the account CA.
    pub ca_public: [u8; 32],
    /// The account both ends must belong to.
    pub account_id: String,
}

/// This machine's identity plus the certificate the account CA signed for it.
#[derive(Clone)]
pub struct LanCredentials {
    pub identity: Arc<DeviceIdentity>,
    pub certificate: DeviceCertificate,
    pub trust: LanTrust,
}

impl std::fmt::Debug for LanCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LanCredentials")
            .field("certificate", &self.certificate.fingerprint())
            .field("trust", &self.trust)
            .finish_non_exhaustive()
    }
}

/// What the Noise payload says about the machine on the other end.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanHello {
    pub v: u32,
    pub account_id: String,
    pub device_id: String,
    pub name: String,
    pub certificate: DeviceCertificate,
}

/// A successful peer, after certificate verification.
#[derive(Debug, Clone)]
pub struct RemoteDevice {
    pub account_id: String,
    pub device_id: String,
    pub name: String,
    pub certificate: DeviceCertificate,
}

/// An encrypted LAN session.
pub struct LanSession {
    read: AsyncMutex<OwnedReadHalf>,
    write: AsyncMutex<OwnedWriteHalf>,
    transport: StdMutex<TransportState>,
    remote: RemoteDevice,
}

impl std::fmt::Debug for LanSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LanSession")
            .field("remote", &self.remote)
            .finish_non_exhaustive()
    }
}

impl LanSession {
    pub fn remote(&self) -> &RemoteDevice {
        &self.remote
    }

    /// Send one Noise transport message.
    pub async fn send(&self, payload: &[u8]) -> Result<(), LanError> {
        if payload.len() > MAX_MESSAGE {
            return Err(LanError::Noise("message is too large".to_string()));
        }
        let (ciphertext, len) = {
            let mut transport = self
                .transport
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut ciphertext = vec![0u8; MAX_MESSAGE];
            let len = transport
                .write_message(payload, &mut ciphertext)
                .map_err(|error| LanError::Noise(error.to_string()))?;
            (ciphertext, len)
        };
        let mut stream = self.write.lock().await;
        stream.write_all(&(len as u16).to_be_bytes()).await?;
        stream.write_all(&ciphertext[..len]).await?;
        stream.flush().await?;
        Ok(())
    }

    /// Read one Noise transport message.
    pub async fn recv(&self) -> Result<Vec<u8>, LanError> {
        let ciphertext = {
            let mut stream = self.read.lock().await;
            read_frame(&mut *stream).await?
        };
        let mut transport = self
            .transport
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut plaintext = vec![0u8; MAX_MESSAGE];
        let len = transport
            .read_message(&ciphertext, &mut plaintext)
            .map_err(|error| LanError::Noise(error.to_string()))?;
        plaintext.truncate(len);
        Ok(plaintext)
    }
}

/// A listening LAN endpoint. It opens the port only when constructed.
pub struct LanListener {
    listener: TcpListener,
    credentials: LanCredentials,
}

impl LanListener {
    /// Bind the LAN port and keep the credentials for each accept.
    pub async fn bind(port: u16, credentials: LanCredentials) -> Result<Self, LanError> {
        let listener = TcpListener::bind(("0.0.0.0", port)).await?;
        Ok(Self {
            listener,
            credentials,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, LanError> {
        Ok(self.listener.local_addr()?)
    }

    /// The address a peer on this machine can dial right now.
    ///
    /// [`Self::local_addr`] reports the wildcard address (`0.0.0.0`) for the
    /// normal bind. Unix engines treat connecting to it as loopback, but
    /// Windows refuses the dial with `WSAEADDRNOTAVAIL` (10049), so tests and
    /// same-host tooling must map it to loopback explicitly.
    pub fn dial_addr(&self) -> Result<SocketAddr, LanError> {
        let bound = self.local_addr()?;
        Ok(if bound.ip().is_unspecified() {
            SocketAddr::from(([127, 0, 0, 1], bound.port()))
        } else {
            bound
        })
    }

    /// Accept one peer and finish the handshake before returning.
    pub async fn accept(&self) -> Result<LanSession, LanError> {
        let (mut stream, _) = self.listener.accept().await?;
        let (transport, remote) = handshake_responder(&mut stream, &self.credentials).await?;
        let (read, write) = stream.into_split();
        Ok(LanSession {
            read: AsyncMutex::new(read),
            write: AsyncMutex::new(write),
            transport: StdMutex::new(transport),
            remote,
        })
    }
}

/// Dial a LAN peer and finish the handshake before returning.
pub async fn connect(
    address: SocketAddr,
    credentials: &LanCredentials,
) -> Result<LanSession, LanError> {
    let mut stream = TcpStream::connect(address).await?;
    let (transport, remote) = handshake_initiator(&mut stream, credentials).await?;
    let (read, write) = stream.into_split();
    Ok(LanSession {
        read: AsyncMutex::new(read),
        write: AsyncMutex::new(write),
        transport: StdMutex::new(transport),
        remote,
    })
}

async fn handshake_initiator(
    stream: &mut TcpStream,
    credentials: &LanCredentials,
) -> Result<(TransportState, RemoteDevice), LanError> {
    let key = credentials.identity.noise_private_key();
    let tag = account_tag(&credentials.trust.account_id);
    let params = NOISE_PARAMS
        .parse()
        .map_err(|error| LanError::Noise(format!("bad Noise parameters: {error}")))?;
    let builder = Builder::new(params)
        .local_private_key(&key)
        .prologue(tag.as_bytes());
    let mut session = builder
        .build_initiator()
        .map_err(|error| LanError::Noise(error.to_string()))?;
    let mut buffer = vec![0u8; MAX_MESSAGE];
    let len = session
        .write_message(&[], &mut buffer)
        .map_err(|error| LanError::Noise(error.to_string()))?;
    write_frame(stream, &buffer[..len]).await?;

    let second = read_frame(stream).await?;
    let len = session
        .read_message(&second, &mut buffer)
        .map_err(|error| LanError::Noise(error.to_string()))?;
    let remote_hello: LanHello = serde_json::from_slice(&buffer[..len])?;

    let hello = hello_for(credentials);
    let payload = serde_json::to_vec(&hello)?;
    let len = session
        .write_message(&payload, &mut buffer)
        .map_err(|error| LanError::Noise(error.to_string()))?;
    write_frame(stream, &buffer[..len]).await?;
    let remote = verify_hello(
        &remote_hello,
        &credentials.trust,
        session.get_remote_static(),
    )?;
    let transport = session
        .into_transport_mode()
        .map_err(|error| LanError::Noise(error.to_string()))?;
    Ok((transport, remote))
}

async fn handshake_responder(
    stream: &mut TcpStream,
    credentials: &LanCredentials,
) -> Result<(TransportState, RemoteDevice), LanError> {
    let key = credentials.identity.noise_private_key();
    let tag = account_tag(&credentials.trust.account_id);
    let params = NOISE_PARAMS
        .parse()
        .map_err(|error| LanError::Noise(format!("bad Noise parameters: {error}")))?;
    let builder = Builder::new(params)
        .local_private_key(&key)
        .prologue(tag.as_bytes());
    let mut session = builder
        .build_responder()
        .map_err(|error| LanError::Noise(error.to_string()))?;
    let mut buffer = vec![0u8; MAX_MESSAGE];

    let first = read_frame(stream).await?;
    session
        .read_message(&first, &mut buffer)
        .map_err(|error| LanError::Noise(error.to_string()))?;

    let hello = hello_for(credentials);
    let payload = serde_json::to_vec(&hello)?;
    let len = session
        .write_message(&payload, &mut buffer)
        .map_err(|error| LanError::Noise(error.to_string()))?;
    write_frame(stream, &buffer[..len]).await?;

    let third = read_frame(stream).await?;
    let len = session
        .read_message(&third, &mut buffer)
        .map_err(|error| LanError::Noise(error.to_string()))?;
    let remote_hello: LanHello = serde_json::from_slice(&buffer[..len])?;
    let remote = verify_hello(
        &remote_hello,
        &credentials.trust,
        session.get_remote_static(),
    )?;
    let transport = session
        .into_transport_mode()
        .map_err(|error| LanError::Noise(error.to_string()))?;
    Ok((transport, remote))
}

fn hello_for(credentials: &LanCredentials) -> LanHello {
    LanHello {
        v: FORMAT_VERSION,
        account_id: credentials.certificate.body.account_id.clone(),
        device_id: credentials.certificate.body.device_id.clone(),
        name: credentials.certificate.body.device_id.clone(),
        certificate: credentials.certificate.clone(),
    }
}

fn verify_hello(
    hello: &LanHello,
    trust: &LanTrust,
    remote_static: Option<&[u8]>,
) -> Result<RemoteDevice, LanError> {
    if hello.v != FORMAT_VERSION || hello.account_id != trust.account_id {
        return Err(LanError::WrongIdentity);
    }
    hello.certificate.verify(
        &trust.ca_public,
        &hello.account_id,
        &hello.device_id,
        now_ms(),
    )?;
    let remote_static = remote_static
        .ok_or_else(|| LanError::Noise("the peer presented no static key".to_string()))?;
    let certificate_x = hello.certificate.body.x25519_key()?;
    if remote_static != certificate_x {
        return Err(LanError::WrongIdentity);
    }
    Ok(RemoteDevice {
        account_id: hello.account_id.clone(),
        device_id: hello.device_id.clone(),
        name: hello.name.clone(),
        certificate: hello.certificate.clone(),
    })
}

async fn write_frame(
    stream: &mut (impl AsyncWriteExt + Unpin),
    bytes: &[u8],
) -> Result<(), LanError> {
    if bytes.len() > MAX_MESSAGE {
        return Err(LanError::Noise(
            "handshake message is too large".to_string(),
        ));
    }
    stream
        .write_all(&(bytes.len() as u16).to_be_bytes())
        .await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame(stream: &mut (impl AsyncReadExt + Unpin)) -> Result<Vec<u8>, LanError> {
    let mut length = [0u8; 2];
    stream.read_exact(&mut length).await?;
    let length = u16::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_MESSAGE {
        return Err(LanError::Noise(
            "the peer sent an invalid frame".to_string(),
        ));
    }
    let mut bytes = vec![0u8; length];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CertificateBody, DeviceCertificate};
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;

    fn credentials(ca: &SigningKey, account: &str, device: &str) -> LanCredentials {
        let identity = Arc::new(DeviceIdentity::generate());
        let public = identity.public();
        let body = CertificateBody {
            v: FORMAT_VERSION,
            account_id: account.to_string(),
            device_id: device.to_string(),
            ed25519_public: base64::engine::general_purpose::STANDARD.encode(public.ed25519),
            x25519_public: base64::engine::general_purpose::STANDARD.encode(public.x25519),
            issued_at: now_ms(),
            expires_at: now_ms() + 86_400_000,
        };
        LanCredentials {
            identity,
            certificate: DeviceCertificate::sign(body, ca),
            trust: LanTrust {
                ca_public: ca.verifying_key().to_bytes(),
                account_id: account.to_string(),
            },
        }
    }

    #[tokio::test]
    async fn dial_addr_is_loopback_when_the_listener_binds_the_wildcard() {
        let ca = SigningKey::generate(&mut OsRng);
        let listener = LanListener::bind(0, credentials(&ca, "acct", "server"))
            .await
            .unwrap();
        let dial = listener.dial_addr().unwrap();
        assert!(
            dial.ip().is_loopback(),
            "dial_addr must map 0.0.0.0 to loopback, got {dial}"
        );
        assert_eq!(dial.port(), listener.local_addr().unwrap().port());
    }

    #[tokio::test]
    async fn two_desktops_handshake_and_exchange_an_encrypted_message() {
        let ca = SigningKey::generate(&mut OsRng);
        let server_credentials = credentials(&ca, "acct", "server");
        let client_credentials = credentials(&ca, "acct", "client");
        let listener = LanListener::bind(0, server_credentials).await.unwrap();
        let address = listener.dial_addr().unwrap();
        let accept = tokio::spawn(async move { listener.accept().await });

        let client = connect(address, &client_credentials).await.unwrap();
        let server = accept.await.unwrap().unwrap();
        assert_eq!(client.remote().device_id, "server");
        assert_eq!(server.remote().device_id, "client");
        client.send(b"hello over noise").await.unwrap();
        assert_eq!(server.recv().await.unwrap(), b"hello over noise");
    }

    #[tokio::test]
    async fn another_account_is_refused_at_the_payload() {
        let ca = SigningKey::generate(&mut OsRng);
        let server_credentials = credentials(&ca, "acct", "server");
        let wrong = credentials(&ca, "other", "client");
        let listener = LanListener::bind(0, server_credentials).await.unwrap();
        let address = listener.dial_addr().unwrap();
        let accept = tokio::spawn(async move { listener.accept().await });
        assert!(connect(address, &wrong).await.is_err());
        assert!(accept.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn a_certificate_for_another_x25519_key_is_refused() {
        let ca = SigningKey::generate(&mut OsRng);
        let server_credentials = credentials(&ca, "acct", "server");
        let mut client = credentials(&ca, "acct", "client");
        // Keep the certificate but replace the Noise key: the CA never signed
        // this combination, and the static-key check must notice.
        client.identity = Arc::new(DeviceIdentity::generate());
        let listener = LanListener::bind(0, server_credentials).await.unwrap();
        let address = listener.dial_addr().unwrap();
        let accept = tokio::spawn(async move { listener.accept().await });
        let _client = connect(address, &client).await;
        assert!(accept.await.unwrap().is_err());
    }
}
