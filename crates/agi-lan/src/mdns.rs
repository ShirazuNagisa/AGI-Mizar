//! mDNS advertisement and browsing for `_agi-mizar._tcp`.
//!
//! The TXT record is the same discovery payload as the UDP fallback, split into
//! short properties so it stays within DNS-SD's 255-byte value limit. The
//! service is not a trust decision: a resolved peer still has to pass Noise and
//! the account certificate before a Mizar frame is accepted.
//!
//! This module is deliberately small and blocking. `mdns-sd` owns its own
//! threads; the relay-side boot task wraps the receiver in `spawn_blocking`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::mpsc::{self, Receiver};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::{LanBeacon, LanError};

/// The DNS-SD service type this build advertises.
pub const SERVICE_TYPE: &str = "_agi-mizar._tcp.local.";

/// A running mDNS advertiser. Dropping it unregisters the daemon.
pub struct MdnsAdvertiser {
    daemon: ServiceDaemon,
}

impl MdnsAdvertiser {
    /// Advertise one beacon. `host` should end in `.local.`.
    pub fn start(beacon: &LanBeacon, host: &str) -> Result<Self, LanError> {
        let daemon = ServiceDaemon::new().map_err(|error| LanError::Noise(error.to_string()))?;
        let info = service_info(beacon, host)?;
        daemon
            .register(info)
            .map_err(|error| LanError::Noise(error.to_string()))?;
        Ok(Self { daemon })
    }
}

impl Drop for MdnsAdvertiser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// Browse for peers of one account. The caller owns the receiver.
pub struct MdnsBrowser {
    daemon: ServiceDaemon,
    receiver: Receiver<(LanBeacon, IpAddr)>,
}

impl MdnsBrowser {
    pub fn start(account_tag: &str) -> Result<Self, LanError> {
        let daemon = ServiceDaemon::new().map_err(|error| LanError::Noise(error.to_string()))?;
        let events = daemon
            .browse(SERVICE_TYPE)
            .map_err(|error| LanError::Noise(error.to_string()))?;
        let (sender, receiver) = mpsc::channel();
        let tag = account_tag.to_string();
        std::thread::Builder::new()
            .name("mizar-mdns".to_string())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    if let ServiceEvent::ServiceResolved(info) = event {
                        if let Some(found) = beacon_from_info(&info, &tag) {
                            if sender.send(found).is_err() {
                                return;
                            }
                        }
                    }
                }
            })
            .ok();
        Ok(Self { daemon, receiver })
    }

    pub fn receiver(&self) -> &Receiver<(LanBeacon, IpAddr)> {
        &self.receiver
    }
}

impl Drop for MdnsBrowser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

fn service_info(beacon: &LanBeacon, host: &str) -> Result<ServiceInfo, LanError> {
    let properties = HashMap::from([
        ("tag".to_string(), beacon.account_tag.clone()),
        ("device".to_string(), beacon.device_id.clone()),
        ("name".to_string(), beacon.name.clone()),
        ("port".to_string(), beacon.port.to_string()),
        ("cert".to_string(), beacon.certificate_fingerprint.clone()),
    ]);
    ServiceInfo::new(
        SERVICE_TYPE,
        &beacon.device_id,
        host,
        IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        beacon.port,
        properties,
    )
    .map_err(|error| LanError::Noise(error.to_string()))
}

/// Build a beacon from a resolved service's TXT record.
fn beacon_from_info(info: &ServiceInfo, account_tag: &str) -> Option<(LanBeacon, IpAddr)> {
    let tag = info.get_property_val_str("tag")?;
    if tag != account_tag {
        return None;
    }
    let device_id = info.get_property_val_str("device")?.to_string();
    let name = info
        .get_property_val_str("name")
        .unwrap_or(&device_id)
        .to_string();
    let port = info
        .get_property_val_str("port")
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or_else(|| info.get_port());
    let certificate_fingerprint = info.get_property_val_str("cert")?.to_string();
    let ip = info.get_addresses().iter().next().copied()?;
    Some((
        LanBeacon {
            v: crate::FORMAT_VERSION,
            account_tag: tag.to_string(),
            device_id,
            name,
            port,
            certificate_fingerprint,
        },
        ip,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn a_matching_txt_record_becomes_a_beacon() {
        let properties = HashMap::from([
            ("tag".to_string(), "tag1".to_string()),
            ("device".to_string(), "dev1".to_string()),
            ("name".to_string(), "Mac".to_string()),
            ("port".to_string(), "19393".to_string()),
            ("cert".to_string(), "abc".to_string()),
        ]);
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "dev1",
            "host.local.",
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            19393,
            properties,
        )
        .unwrap();
        let (beacon, ip) = beacon_from_info(&info, "tag1").unwrap();
        assert_eq!(beacon.device_id, "dev1");
        assert_eq!(beacon.port, 19393);
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(beacon_from_info(&info, "tag2").is_none());
    }
}
