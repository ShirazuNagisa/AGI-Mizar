//! Asking the gateway to forward the punch port.
//!
//! The punch and STUN work for every NAT that maps a port the same way whatever
//! the destination. A symmetric NAT does not: the mapping this machine learns
//! from STUN is not the one a phone reaches, and no number of punch rounds
//! changes that. The one thing that does is not asking the NAT at all -- asking
//! the gateway for a forwarding, which a router that grants it makes valid from
//! anywhere.
//!
//! Two protocols, because home routers speak one or the other:
//!
//! - **NAT-PMP** (RFC 6886): a twelve-byte binary request to the gateway's UDP
//!   port 5351. It can return the external address and the mapped port.
//! - **UPnP-IGD**: SSDP discovery, an HTTP device description, then SOAP. Most
//!   consumer routers; the external address is a separate call.
//!
//! Neither is required for the direct path to work. A campus network with
//! multicast blocked answers neither, and that is a normal outcome: the punch
//! proceeds without a mapped candidate, exactly as before.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::timeout;

/// How long the lease asks a router to keep the forwarding.
const LEASE_SECONDS: u32 = 3600;

/// How long one NAT-PMP exchange waits for an answer.
const NATPMP_TIMEOUT: Duration = Duration::from_millis(700);

/// How many times a NAT-PMP request is tried.
const NATPMP_ATTEMPTS: usize = 3;

/// How long SSDP discovery listens for a gateway.
const SSDP_WINDOW: Duration = Duration::from_secs(2);

/// What a gateway granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The outside address, when the protocol reported one. UPnP sometimes does
    /// not; the engine then pairs the port with the address STUN learned.
    pub external_ip: Option<IpAddr>,
    /// The port the gateway forwards to this machine.
    pub external_port: u16,
    /// How long it promised to keep it.
    pub lifetime: Duration,
    /// `nat-pmp` or `upnp`, for the log line.
    pub provider: &'static str,
}

/// Ask the gateway for a forwarding to `internal_port`.
///
/// Returns `None` when the network has no gateway, no mapping protocol, or a
/// gateway that refuses. Every failure is normal and costs only the mapped
/// candidate.
pub async fn map_port(internal_port: u16) -> Option<Grant> {
    if std::env::var_os("AGI_DISABLE_PORT_MAPPING").is_some_and(|value| !value.is_empty()) {
        return None;
    }
    let gateway = default_gateway().await?;
    if let Some(grant) = natpmp_map(gateway, internal_port).await {
        tracing::info!(
            "p2p: NAT-PMP mapped {internal_port} -> {}:{} ({} s)",
            grant
                .external_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "?".to_string()),
            grant.external_port,
            grant.lifetime.as_secs()
        );
        return Some(grant);
    }
    if let Some(grant) = upnp_map(gateway, internal_port).await {
        tracing::info!(
            "p2p: UPnP mapped {internal_port} -> {}:{} ({} s)",
            grant
                .external_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "?".to_string()),
            grant.external_port,
            grant.lifetime.as_secs()
        );
        return Some(grant);
    }
    tracing::debug!("p2p: no gateway granted a port mapping for {internal_port}");
    None
}

// MARK: - Gateway

/// The default gateway's IPv4 address, if the platform can name one.
pub async fn default_gateway() -> Option<IpAddr> {
    let output = tokio::task::spawn_blocking(|| {
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("route")
                .args(["-n", "get", "default"])
                .output()
                .ok()
        }
        #[cfg(target_os = "linux")]
        {
            std::process::Command::new("ip")
                .args(["route", "show", "default"])
                .output()
                .ok()
        }
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("route")
                .args(["print", "-4"])
                .output()
                .ok()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            None
        }
    })
    .await
    .ok()
    .flatten()?;
    if !output.status.success() {
        return linux_gateway();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    #[cfg(target_os = "macos")]
    {
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("gateway:") {
                if let Ok(ip) = rest.trim().parse() {
                    return Some(ip);
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(rest) = text.split_whitespace().nth(2) {
            if let Ok(ip) = rest.parse() {
                return Some(ip);
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 3 && fields[0] == "0.0.0.0" && fields[1] == "0.0.0.0" {
                if let Ok(ip) = fields[2].parse() {
                    return Some(ip);
                }
            }
        }
    }
    linux_gateway()
}

/// `/proc/net/route` as a fallback (and the only source on a bare Linux).
fn linux_gateway() -> Option<IpAddr> {
    let text = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 || fields[1] != "00000000" {
            continue;
        }
        let raw = u32::from_str_radix(fields[2], 16).ok()?;
        // The kernel writes the address in host (little-endian) order.
        let address = Ipv4Addr::from(raw.to_le_bytes());
        if !address.is_unspecified() {
            return Some(IpAddr::V4(address));
        }
    }
    None
}

// MARK: - NAT-PMP

/// The port NAT-PMP lives on.
const NATPMP_PORT: u16 = 5351;

/// Ask a gateway for an external address and a UDP mapping.
async fn natpmp_map(gateway: IpAddr, internal_port: u16) -> Option<Grant> {
    let gateway = SocketAddr::new(gateway, NATPMP_PORT);
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    socket.connect(gateway).await.ok()?;

    let external_ip = natpmp_external_address(&socket).await;
    let (external_port, lifetime) =
        natpmp_request_mapping(&socket, internal_port, LEASE_SECONDS).await?;
    Some(Grant {
        external_ip,
        external_port,
        lifetime: Duration::from_secs(u64::from(lifetime)),
        provider: "nat-pmp",
    })
}

/// One request/response exchange, retried a few times.
async fn natpmp_exchange(socket: &UdpSocket, request: &[u8]) -> Option<Vec<u8>> {
    for _ in 0..NATPMP_ATTEMPTS {
        socket.send(request).await.ok()?;
        let mut buffer = [0u8; 64];
        if let Ok(Ok(length)) = timeout(NATPMP_TIMEOUT, socket.recv(&mut buffer)).await {
            return Some(buffer[..length].to_vec());
        }
    }
    None
}

/// Opcode 0: "what address do you see me from?"
async fn natpmp_external_address(socket: &UdpSocket) -> Option<IpAddr> {
    let answer = natpmp_exchange(socket, &[0, 0]).await?;
    // version, opcode, result (2), epoch (4), address (4)
    if answer.len() < 12 || answer[0] != 0 || answer[1] != 128 {
        return None;
    }
    if u16::from_be_bytes([answer[2], answer[3]]) != 0 {
        return None;
    }
    Some(IpAddr::V4(Ipv4Addr::new(
        answer[8], answer[9], answer[10], answer[11],
    )))
}

/// Opcode 1: map a UDP port.
async fn natpmp_request_mapping(
    socket: &UdpSocket,
    internal_port: u16,
    lifetime: u32,
) -> Option<(u16, u32)> {
    let mut request = [0u8; 12];
    request[1] = 1; // UDP
    request[4..6].copy_from_slice(&internal_port.to_be_bytes());
    request[6..8].copy_from_slice(&internal_port.to_be_bytes());
    request[8..12].copy_from_slice(&lifetime.to_be_bytes());
    let answer = natpmp_exchange(socket, &request).await?;
    if answer.len() < 16 || answer[0] != 0 || answer[1] != 129 {
        return None;
    }
    if u16::from_be_bytes([answer[2], answer[3]]) != 0 {
        return None;
    }
    let external_port = u16::from_be_bytes([answer[10], answer[11]]);
    let lifetime = u32::from_be_bytes([answer[12], answer[13], answer[14], answer[15]]);
    Some((external_port, lifetime))
}

// MARK: - UPnP-IGD

/// Discover an IGD and ask it for a mapping.
async fn upnp_map(gateway: IpAddr, internal_port: u16) -> Option<Grant> {
    let location = ssdp_location().await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .ok()?;
    let description = client.get(&location).send().await.ok()?.text().await.ok()?;
    let (service, control) = upnp_control(&description, &location)?;
    let local_ip = local_ip_for(gateway)?;

    let external_ip = upnp_external_address(&client, &service, &control).await;
    let granted = upnp_add_mapping(&client, &service, &control, local_ip, internal_port).await?;
    Some(Grant {
        external_ip,
        external_port: granted,
        lifetime: Duration::from_secs(u64::from(LEASE_SECONDS)),
        provider: "upnp",
    })
}

/// One SSDP M-SEARCH, and the first `LOCATION` it answers with.
async fn ssdp_location() -> Option<String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    socket.set_multicast_ttl_v4(2).ok()?;
    let search = concat!(
        "M-SEARCH * HTTP/1.1\r\n",
        "HOST: 239.255.255.250:1900\r\n",
        "MAN: \"ssdp:discover\"\r\n",
        "MX: 2\r\n",
        "ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n",
        "\r\n"
    );
    socket
        .send_to(search.as_bytes(), "239.255.255.250:1900")
        .await
        .ok()?;
    let deadline = tokio::time::Instant::now() + SSDP_WINDOW;
    let mut buffer = [0u8; 4096];
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Ok((length, _))) = timeout(remaining, socket.recv_from(&mut buffer)).await else {
            break;
        };
        let text = String::from_utf8_lossy(&buffer[..length]);
        if let Some(location) = ssdp_location_in(&text) {
            return Some(location);
        }
    }
    None
}

/// The `LOCATION` header of one SSDP reply, if it names an HTTP URL.
fn ssdp_location_in(text: &str) -> Option<String> {
    for line in text.lines() {
        if line.to_ascii_lowercase().starts_with("location:") {
            let rest = line[9..].trim();
            if rest.starts_with("http://") || rest.starts_with("https://") {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// The WANIPConnection (or WANPPPConnection) service and its absolute control URL.
fn upnp_control(description: &str, location: &str) -> Option<(String, String)> {
    for service in [
        "urn:schemas-upnp-org:service:WANIPConnection:1",
        "urn:schemas-upnp-org:service:WANPPPConnection:1",
    ] {
        let Some(service_at) = description.find(service) else {
            continue;
        };
        let Some(control_at) = description[service_at..].find("<controlURL>") else {
            continue;
        };
        let start = service_at + control_at + "<controlURL>".len();
        let Some(end) = description[start..].find("</controlURL>") else {
            continue;
        };
        let control = description[start..start + end].trim();
        if control.is_empty() {
            continue;
        }
        let control = reqwest::Url::parse(location)
            .ok()?
            .join(control)
            .ok()?
            .to_string();
        return Some((service.to_string(), control));
    }
    None
}

/// The outside address as the IGD reports it.
async fn upnp_external_address(
    client: &reqwest::Client,
    service: &str,
    control: &str,
) -> Option<IpAddr> {
    let body = format!(
        concat!(
            "<?xml version=\"1.0\"?>\n",
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" ",
            "s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">",
            "<s:Body><u:GetExternalIPAddress xmlns:u=\"{}\"/></s:Body></s:Envelope>"
        ),
        service
    );
    let response = client
        .post(control)
        .header("SOAPAction", format!("\"{service}#GetExternalIPAddress\""))
        .header("Content-Type", "text/xml; charset=utf-8")
        .body(body)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let text = response.text().await.ok()?;
    let start = text.find("<NewExternalIPAddress>")? + "<NewExternalIPAddress>".len();
    let end = text[start..].find("</NewExternalIPAddress>")?;
    let address: IpAddr = text[start..start + end].trim().parse().ok()?;
    (!address.is_unspecified()).then_some(address)
}

/// Ask the IGD to forward the port. Returns the external port it granted.
async fn upnp_add_mapping(
    client: &reqwest::Client,
    service: &str,
    control: &str,
    local_ip: IpAddr,
    internal_port: u16,
) -> Option<u16> {
    let body = format!(
        concat!(
            "<?xml version=\"1.0\"?>\n",
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" ",
            "s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">",
            "<s:Body><u:AddPortMapping xmlns:u=\"{service}\">",
            "<NewRemoteHost></NewRemoteHost>",
            "<NewExternalPort>{internal_port}</NewExternalPort>",
            "<NewProtocol>UDP</NewProtocol>",
            "<NewInternalPort>{internal_port}</NewInternalPort>",
            "<NewInternalClient>{local_ip}</NewInternalClient>",
            "<NewEnabled>1</NewEnabled>",
            "<NewPortMappingDescription>AGI Mizar P2P</NewPortMappingDescription>",
            "<NewLeaseDuration>{LEASE_SECONDS}</NewLeaseDuration>",
            "</u:AddPortMapping></s:Body></s:Envelope>"
        ),
        service = service,
        internal_port = internal_port,
        local_ip = local_ip,
        LEASE_SECONDS = LEASE_SECONDS
    );
    let response = client
        .post(control)
        .header("SOAPAction", format!("\"{service}#AddPortMapping\""))
        .header("Content-Type", "text/xml; charset=utf-8")
        .body(body)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    Some(internal_port)
}

/// The local address the OS would use to reach `gateway`.
fn local_ip_for(gateway: IpAddr) -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((gateway, NATPMP_PORT)).ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_unspecified() && !address.is_loopback()).then_some(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gateway that answers the external-address and mapping opcodes.
    async fn fake_natpmp() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.expect("a gateway");
        let address = socket.local_addr().expect("an address");
        let task = tokio::spawn(async move {
            let mut buffer = [0u8; 64];
            loop {
                let Ok((length, from)) = socket.recv_from(&mut buffer).await else {
                    break;
                };
                if length < 2 {
                    continue;
                }
                let reply = match buffer[1] {
                    0 => {
                        // version, opcode 128, result 0, epoch, 203.0.113.7
                        vec![0, 128, 0, 0, 0, 0, 0, 1, 203, 0, 113, 7]
                    }
                    1 => {
                        let internal = u16::from_be_bytes([buffer[4], buffer[5]]);
                        let mut reply = vec![0, 129, 0, 0, 0, 0, 0, 1];
                        reply.extend_from_slice(&internal.to_be_bytes());
                        reply.extend_from_slice(&40100u16.to_be_bytes());
                        reply.extend_from_slice(&3600u32.to_be_bytes());
                        reply
                    }
                    _ => continue,
                };
                let _ = socket.send_to(&reply, from).await;
            }
        });
        (address, task)
    }

    #[tokio::test]
    async fn natpmp_learns_the_address_and_the_mapped_port() {
        let (gateway, task) = fake_natpmp().await;
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.expect("a socket");
        socket.connect(gateway).await.expect("connected");
        let external = natpmp_external_address(&socket).await;
        assert_eq!(external, Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))));
        let (port, lifetime) = natpmp_request_mapping(&socket, 54321, 3600)
            .await
            .expect("a mapping");
        assert_eq!(port, 40100);
        assert_eq!(lifetime, 3600);
        task.abort();
    }

    #[test]
    fn an_ssdp_reply_yields_its_location_without_its_header() {
        let reply = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nLOCATION: http://192.168.0.1:5000/rootDesc.xml\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n";
        assert_eq!(
            ssdp_location_in(reply).as_deref(),
            Some("http://192.168.0.1:5000/rootDesc.xml")
        );
        assert_eq!(ssdp_location_in("HTTP/1.1 200 OK\r\n\r\n"), None);
    }

    #[test]
    fn the_wan_control_url_is_absolute() {
        let description = r#"
            <root><device><serviceList><service>
              <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
              <controlURL>/ctl/IPConn</controlURL>
            </service></serviceList></device></root>
        "#;
        let (service, control) =
            upnp_control(description, "http://192.168.0.1:5000/rootDesc.xml").expect("a service");
        assert_eq!(service, "urn:schemas-upnp-org:service:WANIPConnection:1");
        assert_eq!(control, "http://192.168.0.1:5000/ctl/IPConn");
    }

    #[test]
    fn a_relative_control_url_keeps_the_description_origin() {
        let description = r#"
            <serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>
            <controlURL>ctl/ppp</controlURL>
        "#;
        let (_, control) =
            upnp_control(description, "http://10.0.0.1/desc.xml").expect("a service");
        assert_eq!(control, "http://10.0.0.1/ctl/ppp");
    }
}
