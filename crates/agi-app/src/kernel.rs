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
