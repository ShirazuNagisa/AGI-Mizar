//! The kernel's address, remembered for the shell.
//!
//! The tray can change the theme, but the theme lives in the kernel and the
//! kernel's API needs a bearer token that only the window URL carries. Rather
//! than teach the tray to read the file the plugin uses, the launch that started
//! the kernel keeps the pair here: one process, one address, one token.

use std::sync::OnceLock;

static ENDPOINT: OnceLock<(u16, String)> = OnceLock::new();

/// Remember the kernel this process started.
pub fn remember(port: u16, token: String) {
    let _ = ENDPOINT.set((port, token));
}

/// Change the window's theme from outside the window.
///
/// Fire and forget: a menu item that blocks on a network call is a menu item
/// that hangs without saying why.
pub fn set_theme(theme: &str) {
    let Some((port, token)) = ENDPOINT.get().cloned() else {
        return;
    };
    let body = serde_json::json!({ "op": "set_theme", "theme": theme });
    std::thread::spawn(move || {
        let _ = ureq_post(port, &token, body);
    });
}

/// What the menu bar needs from the kernel, in one round trip.
pub struct Summary {
    pub version: String,
    /// Each agent, whether it can be prompted now, and whether it is installed.
    pub agents: Vec<(String, bool, bool)>,
    /// Each paired phone: name, whether it is connected, when it was last seen.
    pub phones: Vec<(String, bool, i64)>,
}

/// Read the kernel's state, or nothing when it cannot be reached.
pub fn summary() -> Option<Summary> {
    let (port, token) = ENDPOINT.get().cloned()?;
    let body = std::thread::spawn(move || ureq_get(port, &token))
        .join()
        .ok()
        .flatten()?;
    let state: serde_json::Value = serde_json::from_str(&body).ok()?;

    let agents = state
        .get("engines")
        .and_then(|engines| engines.as_array())
        .map(|engines| {
            engines
                .iter()
                .filter_map(|engine| {
                    let id = engine.get("engine")?.as_str()?.to_string();
                    let connected = engine.get("connected")?.as_bool()?;
                    let available = engine
                        .get("available")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(connected);
                    Some((id, connected, available))
                })
                .collect()
        })
        .unwrap_or_default();

    let phones = state
        .get("devices")
        .and_then(|devices| devices.as_array())
        .map(|devices| {
            devices
                .iter()
                .filter_map(|device| {
                    let name = device.get("name")?.as_str()?.to_string();
                    let online = device
                        .get("online")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let last_seen = device
                        .get("last_seen_ms")
                        .and_then(|value| value.as_i64())
                        .unwrap_or(0);
                    Some((name, online, last_seen))
                })
                .collect()
        })
        .unwrap_or_default();

    Some(Summary {
        version: state.get("version")?.as_str()?.to_string(),
        agents,
        phones,
    })
}

/// One small GET, for the same reason as the POST below.
fn ureq_get(port: u16, token: &str) -> Option<String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .ok()?;
    let request = format!(
        "GET /api/v1/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).ok()?;
    // Status line, headers, blank line, body.
    let body = answer.split_once("\r\n\r\n").map(|(_, body)| body)?;
    Some(body.to_string())
}

/// One small POST, without pulling a runtime into a menu callback.
fn ureq_post(port: u16, token: &str, body: serde_json::Value) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let payload = body.to_string();
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).map_err(|error| format!("connect: {error}"))?;
    let request = format!(
        "POST /api/v1/command HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write: {error}"))?;
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    Ok(())
}
