//! Checking a self-hosted relay before this install trusts it.
//!
//! The guide and the settings page ask the same question — "is this URL a DSH
//! relay I can use?" — and the answer is the contract's [`health`] check plus,
//! when the deployment requires one, a desktop-list request with the server PIN.
//! Keeping the check in one place means the two entry points cannot disagree
//! about what "verified" means.
//!
//! The origin is normalised here too: a person types `relay.example.com`, the
//! rest of the kernel needs `https://relay.example.com` with no path. A server
//! address that is not https is refused rather than downgraded — the PIN and the
//! tunnel both travel over it.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

/// How long one `/healthz` or desktop-list request may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The only product string a relay's `/healthz` may name.
const RELAY_PRODUCT: &str = "dsh-mizar-relay";

/// The oldest tunnel protocol this build can speak.
pub const MIN_TUNNEL_PROTOCOL: u32 = 1;

/// What a connectivity check found.
///
/// `ok` is the product answer; a failed check still carries whatever the relay
/// said before it failed (`relayVersion`, `protocol`, `pinRequired`), because
/// "the PIN is wrong" is more useful next to the server's version than alone.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProbeResult {
    pub ok: bool,
    /// The relay build's own version, when `/healthz` named one.
    pub relay_version: Option<String>,
    /// The tunnel protocol the relay speaks.
    pub protocol: Option<u32>,
    /// Whether the relay's desktop list wants a PIN.
    pub pin_required: bool,
    /// Whether the PIN this check carried was accepted; `None` when untested.
    pub pin_ok: Option<bool>,
    /// A stable code when `ok` is false: `invalid_origin`, `network`,
    /// `not_relay`, `protocol_too_old`, `pin_required`, `pin_invalid` or
    /// `rate_limited`.
    pub error: Option<String>,
    /// One English sentence for a log or a tooltip. The window writes its own
    /// copy from `error`, so the two surfaces can word it for their audience.
    pub message: Option<String>,
}

impl ServerProbeResult {
    /// A failure with no relay facts yet.
    fn failure(code: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            relay_version: None,
            protocol: None,
            pin_required: false,
            pin_ok: None,
            error: Some(code.to_string()),
            message: Some(message.into()),
        }
    }

    /// Attach what `/healthz` said to a later failure.
    fn with_relay(
        mut self,
        version: Option<String>,
        protocol: Option<u32>,
        pin_required: bool,
    ) -> Self {
        self.relay_version = version;
        self.protocol = protocol;
        self.pin_required = pin_required;
        self
    }
}

/// Turn what a person typed into the one spelling the kernel stores.
///
/// Accepts a bare host (`relay.example.com`), which gets `https://`; refuses
/// anything else that is not an https origin with no path, query or credentials.
/// A trailing slash is dropped, so `https://relay.example.com/` and
/// `https://relay.example.com` are one server.
pub fn normalize_origin(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("the server address is empty".to_string());
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("the server address contains whitespace".to_string());
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = reqwest::Url::parse(&with_scheme)
        .map_err(|_| "the server address is not a URL".to_string())?;
    if url.scheme() != "https" {
        return Err("a self-hosted relay must use https".to_string());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("the server address has no host".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the server address must not carry credentials".to_string());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("the server address must not carry a query or fragment".to_string());
    }
    if !matches!(url.path(), "" | "/") {
        return Err("the server address must not carry a path".to_string());
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// Check one origin, and the PIN when the relay asks for one.
///
/// The order is the contract's: `/healthz` first (is this a relay, and can this
/// build talk to it), then the desktop list with the PIN. A relay that requires
/// no PIN never sees the desktop-list request, so a check cannot fail on a PIN
/// the deployment does not use.
pub async fn probe(origin: &str, pin: &str) -> ServerProbeResult {
    let origin = match normalize_origin(origin) {
        Ok(origin) => origin,
        Err(message) => return ServerProbeResult::failure("invalid_origin", message),
    };
    let client = match reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        // No redirects: a relay answers `/healthz` itself, and following a
        // redirect would let the server move this check (and the PIN it carries)
        // to a scheme or host the address never named.
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return ServerProbeResult::failure("network", format!("the HTTP client: {error}"))
        }
    };

    let response = match client.get(format!("{origin}/healthz")).send().await {
        Ok(response) => response,
        Err(error) => {
            return ServerProbeResult::failure("network", format!("{origin}/healthz: {error}"))
        }
    };
    if !response.status().is_success() {
        return ServerProbeResult::failure(
            "not_relay",
            format!("/healthz answered {}", response.status()),
        );
    }
    let health: Value = match response.json().await {
        Ok(value) => value,
        Err(error) => {
            return ServerProbeResult::failure(
                "not_relay",
                format!("/healthz did not answer JSON: {error}"),
            )
        }
    };
    let product = health
        .get("product")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if product != RELAY_PRODUCT {
        return ServerProbeResult::failure(
            "not_relay",
            format!("/healthz named product {product:?}, not {RELAY_PRODUCT:?}"),
        );
    }

    let relay_version = health
        .get("relayVersion")
        .and_then(Value::as_str)
        .map(str::to_string);
    let protocol = health
        .get("version")
        .and_then(Value::as_u64)
        .map(|version| version as u32);
    let pin_required = health
        .get("pinRequired")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let protocol = match protocol {
        Some(protocol) if protocol >= MIN_TUNNEL_PROTOCOL => protocol,
        Some(protocol) => {
            return ServerProbeResult::failure(
                "protocol_too_old",
                format!("the relay speaks tunnel protocol {protocol}; this build needs {MIN_TUNNEL_PROTOCOL}"),
            )
            .with_relay(relay_version, Some(protocol), pin_required)
        }
        None => {
            return ServerProbeResult::failure(
                "protocol_too_old",
                "/healthz did not name a tunnel protocol",
            )
            .with_relay(relay_version, None, pin_required)
        }
    };

    if !pin_required {
        return ServerProbeResult {
            ok: true,
            relay_version,
            protocol: Some(protocol),
            pin_required,
            pin_ok: None,
            error: None,
            message: None,
        };
    }
    if pin.trim().is_empty() {
        return ServerProbeResult::failure(
            "pin_required",
            "this relay requires a server PIN for its desktop list",
        )
        .with_relay(relay_version, Some(protocol), pin_required);
    }

    let response = match client
        .get(format!("{origin}/selfhost/v1/desktops"))
        .header("x-mizar-server-pin", pin.trim())
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return ServerProbeResult::failure(
                "network",
                format!("{origin}/selfhost/v1/desktops: {error}"),
            )
            .with_relay(relay_version, Some(protocol), pin_required)
        }
    };
    let status = response.status();
    if status.is_success() {
        return ServerProbeResult {
            ok: true,
            relay_version,
            protocol: Some(protocol),
            pin_required,
            pin_ok: Some(true),
            error: None,
            message: None,
        };
    }
    let code = match status.as_u16() {
        // The contract says 401 means "no credential" and 403 "wrong one". This
        // check always sends one, so 401 is a relay that treats a bad PIN as
        // missing; either way the person has to type a different PIN.
        401 | 403 => "pin_invalid",
        429 => "rate_limited",
        _ => "network",
    };
    ServerProbeResult::failure(
        code,
        format!("{origin}/selfhost/v1/desktops answered {status}"),
    )
    .with_relay(relay_version, Some(protocol), pin_required)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_becomes_one_canonical_https_origin() {
        assert_eq!(
            normalize_origin("relay.example.com").unwrap(),
            "https://relay.example.com"
        );
        assert_eq!(
            normalize_origin("  https://relay.example.com/  ").unwrap(),
            "https://relay.example.com"
        );
        assert_eq!(
            normalize_origin("relay.example.com:8443").unwrap(),
            "https://relay.example.com:8443"
        );
        // A default port is not part of the identity.
        assert_eq!(
            normalize_origin("https://relay.example.com:443").unwrap(),
            "https://relay.example.com"
        );
    }

    #[test]
    fn an_origin_that_is_not_an_https_root_is_refused() {
        assert!(normalize_origin("").is_err());
        assert!(normalize_origin("http://relay.example.com").is_err());
        assert!(normalize_origin("https://relay.example.com/relay").is_err());
        assert!(normalize_origin("https://relay.example.com/?x=1").is_err());
        assert!(normalize_origin("https://user:pass@relay.example.com").is_err());
        assert!(normalize_origin("not a url").is_err());
    }

    #[test]
    fn a_failed_check_says_which_kind_of_failure_it_was() {
        let failure = ServerProbeResult::failure("network", "no route");
        assert!(!failure.ok);
        assert_eq!(failure.error.as_deref(), Some("network"));
        assert!(!failure.pin_required);
        assert!(failure.pin_ok.is_none());
    }
}
