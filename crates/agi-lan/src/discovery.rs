//! UDP broadcast discovery, the fallback when mDNS is blocked.
//!
//! One socket carries both directions: a desktop sends `LanBeacon` to the LAN
//! broadcast address on the discovery port, and every desktop on the same
//! account keeps the beacons whose account tag matches. The beacon is not a
//! trust decision; the Noise handshake and the account certificate still decide
//! whether a connection is allowed. Discovery only says where to try.
//!
//! mDNS is the first-choice path; this module stays because AP isolation and
//! some managed switches allow broadcast but not multicast.

use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::UdpSocket;

use crate::{account_tag, LanBeacon, LanError};

/// The default discovery port. The transport port is 19393; this is a separate
/// socket so a beacon cannot be mistaken for a Mizar frame.
pub const DISCOVERY_PORT: u16 = 19_394;

/// The largest datagram this module will read or write.
pub const MAX_BEACON_BYTES: usize = 2_048;

/// One UDP socket for broadcasting and receiving beacons.
pub struct LanDiscovery {
    socket: UdpSocket,
}

impl LanDiscovery {
    /// Bind the discovery socket and enable broadcast.
    pub async fn bind(port: u16) -> Result<Self, LanError> {
        let socket = UdpSocket::bind(("0.0.0.0", port)).await?;
        socket.set_broadcast(true)?;
        Ok(Self { socket })
    }

    /// Bind an ephemeral port, for tests and for a host that already has a
    /// discovery socket.
    pub async fn bind_ephemeral() -> Result<Self, LanError> {
        Self::bind(0).await
    }

    pub fn local_addr(&self) -> Result<SocketAddr, LanError> {
        Ok(self.socket.local_addr()?)
    }

    /// Broadcast one beacon to the default discovery port.
    pub async fn announce(&self, beacon: &LanBeacon) -> Result<(), LanError> {
        self.announce_to(
            beacon,
            SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT)),
        )
        .await
    }

    /// Broadcast one beacon to a specific address. Tests use loopback here.
    pub async fn announce_to(
        &self,
        beacon: &LanBeacon,
        target: SocketAddr,
    ) -> Result<(), LanError> {
        let bytes = beacon.encode();
        if bytes.len() > MAX_BEACON_BYTES {
            return Err(LanError::BadBeacon);
        }
        self.socket.send_to(&bytes, target).await?;
        Ok(())
    }

    /// Wait for one beacon, returning only those from the caller's account.
    ///
    /// A beacon from another account is dropped rather than surfaced, so the
    /// caller cannot accidentally offer a peer it is not allowed to use.
    pub async fn recv_for_account(
        &self,
        account_id: &str,
    ) -> Result<(LanBeacon, SocketAddr), LanError> {
        let tag = account_tag(account_id);
        let mut buffer = [0u8; MAX_BEACON_BYTES];
        loop {
            let (len, from) = self.socket.recv_from(&mut buffer).await?;
            let Ok(beacon) = LanBeacon::decode(&buffer[..len]) else {
                continue;
            };
            if beacon.account_tag == tag {
                return Ok((beacon, from));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CertificateBody, DeviceCertificate, FORMAT_VERSION};
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;

    fn beacon(account: &str) -> LanBeacon {
        let ca = SigningKey::generate(&mut OsRng);
        let body = CertificateBody {
            v: FORMAT_VERSION,
            account_id: account.to_string(),
            device_id: "d1".to_string(),
            ed25519_public: base64::engine::general_purpose::STANDARD.encode([1u8; 32]),
            x25519_public: base64::engine::general_purpose::STANDARD.encode([2u8; 32]),
            issued_at: 1,
            expires_at: 2,
        };
        let certificate = DeviceCertificate::sign(body, &ca);
        LanBeacon::new(account, "d1", "Mac", 19393, &certificate)
    }

    #[tokio::test]
    async fn a_matching_account_is_delivered_and_another_is_dropped() {
        let sender = LanDiscovery::bind_ephemeral().await.unwrap();
        let receiver = LanDiscovery::bind_ephemeral().await.unwrap();
        let mut target = receiver.local_addr().unwrap();
        target.set_ip("127.0.0.1".parse().unwrap());
        sender.announce_to(&beacon("acct-1"), target).await.unwrap();
        sender.announce_to(&beacon("acct-2"), target).await.unwrap();
        let (found, _) = receiver.recv_for_account("acct-1").await.unwrap();
        assert_eq!(found.device_id, "d1");
        assert!(found.matches_account("acct-1"));
    }
}
