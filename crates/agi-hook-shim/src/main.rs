//! The binary Claude Code's hooks invoke.
//!
//! Claude Code runs a hook as a command, writes the event payload to its stdin,
//! and reads stdout for an answer. This shim is the smallest thing that can sit
//! between that and the kernel:
//!
//! ```text
//! stdin (hook payload) → read ~/.agi-mizar/hook.json → POST → stdout (answer)
//! ```
//!
//! ### Why not put the port and token in settings.json
//!
//! Because settings.json is a file people read and edit, and because the port is
//! chosen at launch. A file the kernel rewrites each time keeps the settings
//! stable and the secret out of the editor.
//!
//! ### Why a separate binary
//!
//! Hooks run on every tool call, so this has to start in a few milliseconds.
//! Launching the GUI application instead would cost a few hundred and, on macOS,
//! risk a Dock icon flashing on every command the model runs.
//!
//! ### Why it never fails loudly
//!
//! A hook that exits non-zero, or prints something Claude Code cannot parse, can
//! block the operator's own work. Every failure here is silent and exits 0, which
//! leaves Claude Code doing exactly what it would have done without us.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

/// How long to wait for the kernel. Longer than the kernel's own approval
/// timeout, so the kernel's answer is the one that arrives.
const READ_TIMEOUT: Duration = Duration::from_secs(70);

fn main() {
    // The arguments are `claude <event>`; a bare `<event>` is accepted too, so a
    // hand-written settings entry still works.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("claude") {
        args.remove(0);
    }
    let Some(event) = args.into_iter().next() else {
        return;
    };

    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        return;
    }

    let Some((port, token)) = read_hook_file() else {
        return;
    };
    let Some(answer) = post(port, &token, &event, &payload) else {
        return;
    };

    let answer = answer.trim();
    if answer.is_empty() || answer == "null" {
        return;
    }
    println!("{answer}");
}

/// `~/.agi-mizar/hook.json`, written by the kernel at startup.
fn read_hook_file() -> Option<(u16, String)> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    let path = home.join(".agi-mizar").join("hook.json");
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let port = u16::try_from(value.get("port")?.as_u64()?).ok()?;
    let token = value.get("token")?.as_str()?.to_string();
    if token.is_empty() {
        return None;
    }
    Some((port, token))
}

/// POST one hook event and return the response body.
///
/// Hand-rolled HTTP: one request, one response, `Connection: close`. A client
/// crate would add a runtime and a dependency tree to a binary whose whole job is
/// to be fast to start.
fn post(port: u16, token: &str, event: &str, body: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok()?;

    let request = format!(
        "POST /hooks/claude/{event} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    let text = String::from_utf8_lossy(&response);
    let (headers, body) = text.split_once("\r\n\r\n")?;
    // A refused hook (401, 404) has no answer to give Claude Code.
    if !headers.starts_with("HTTP/1.1 200") && !headers.starts_with("HTTP/1.0 200") {
        return None;
    }
    Some(body.to_string())
}
