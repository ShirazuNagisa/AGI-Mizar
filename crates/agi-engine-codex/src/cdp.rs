//! A narrow Chrome DevTools Protocol bridge for Codex Desktop.
//!
//! The desktop app's private IPC can start/stop turns, but it deliberately
//! refuses `thread/archive` while the GUI owns a thread (`active writer`). The
//! GUI itself has the archive action, so the fallback is to drive the visible
//! menu through CDP.
//!
//! This is opt-in and local-only: the caller sets `AGI_CODEX_CDP` to the address
//! the desktop was launched with, for example
//! `AGI_CODEX_CDP=http://127.0.0.1:9333`. If it is absent, nothing changes.
//! It is an experimental escape hatch, not a replacement for the IPC control
//! path.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn base_url() -> Option<String> {
    std::env::var("AGI_CODEX_CDP")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

async fn page_socket(base: &str) -> Result<String, String> {
    let pages: Value = reqwest::get(format!("{base}/json/list"))
        .await
        .map_err(|error| format!("Codex CDP list failed: {error}"))?
        .json()
        .await
        .map_err(|error| format!("Codex CDP list was not JSON: {error}"))?;
    pages
        .as_array()
        .and_then(|pages| {
            pages.iter().find_map(|page| {
                (page.get("type").and_then(Value::as_str) == Some("page"))
                    .then(|| page.get("webSocketDebuggerUrl").and_then(Value::as_str))
                    .flatten()
            })
        })
        .map(str::to_string)
        .ok_or_else(|| "Codex CDP has no page target".to_string())
}

struct Session {
    socket: Socket,
    next_id: u64,
}

impl Session {
    async fn connect(url: &str) -> Result<Self, String> {
        let (socket, _) = connect_async(url)
            .await
            .map_err(|error| format!("Codex CDP connect failed: {error}"))?;
        Ok(Self { socket, next_id: 1 })
    }

    async fn command(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.socket
            .send(Message::Text(
                json!({ "id": id, "method": method, "params": params }).to_string(),
            ))
            .await
            .map_err(|error| format!("Codex CDP send failed: {error}"))?;
        loop {
            let message = tokio::time::timeout(Duration::from_secs(8), self.socket.next())
                .await
                .map_err(|_| format!("Codex CDP {method} timed out"))?
                .ok_or_else(|| "Codex CDP closed".to_string())?
                .map_err(|error| format!("Codex CDP read failed: {error}"))?;
            let text = match message {
                Message::Text(text) => text,
                _ => continue,
            };
            let value: Value =
                serde_json::from_str(&text).map_err(|error| format!("Codex CDP JSON: {error}"))?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(format!("Codex CDP {method} failed: {error}"));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    async fn eval(&mut self, expression: &str) -> Result<Value, String> {
        let result = self
            .command(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if result.get("exceptionDetails").is_some() {
            return Err(format!("Codex CDP JS failed: {result}"));
        }
        Ok(result
            .get("result")
            .and_then(|value| value.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn click_at(&mut self, x: f64, y: f64) -> Result<(), String> {
        for kind in ["mousePressed", "mouseReleased"] {
            self.command(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            )
            .await?;
        }
        Ok(())
    }

    async fn click_selector(&mut self, selector: &str) -> Result<(), String> {
        let point = self
            .eval(&format!(
                r#"(() => {{
                    const el = document.querySelector({selector:?});
                    if (!el) return null;
                    const r = el.getBoundingClientRect();
                    return {{ x: r.x + r.width / 2, y: r.y + r.height / 2 }};
                }})()"#,
            ))
            .await?;
        let x = point.get("x").and_then(Value::as_f64);
        let y = point.get("y").and_then(Value::as_f64);
        match (x, y) {
            (Some(x), Some(y)) => self.click_at(x, y).await,
            _ => Err(format!("Codex CDP selector not found: {selector}")),
        }
    }
}

/// Archive a thread through the visible Codex Desktop menu.
///
/// This is only used after the app-server reports an active writer. It selects
/// the thread row, opens the chat-actions menu, and clicks `归档`.
pub async fn archive(thread_id: &str) -> Result<(), String> {
    let Some(base) = base_url() else {
        return Err("AGI_CODEX_CDP is not set".to_string());
    };
    let url = page_socket(&base).await?;
    let mut session = Session::connect(&url).await?;
    let selected = session
        .eval(&format!(
            r#"(() => {{
                const el = document.querySelector('[data-app-action-sidebar-thread-id="local:{thread_id}"]');
                if (!el) return 'missing';
                el.click();
                return 'ok';
            }})()"#,
        ))
        .await?;
    if selected.as_str() != Some("ok") {
        return Err("Codex Desktop could not select the thread".to_string());
    }
    tokio::time::sleep(Duration::from_millis(350)).await;

    let actions = r#"button[aria-label="聊天操作"]"#;
    session.click_selector(actions).await?;
    tokio::time::sleep(Duration::from_millis(350)).await;

    let clicked = session
        .eval(
            r#"(() => {
                const item = [...document.querySelectorAll('[role="menuitem"]')]
                    .find((el) => (el.innerText || '').trim().startsWith('归档'));
                if (!item) return 'missing';
                item.click();
                return 'ok';
            })()"#,
        )
        .await?;
    if clicked.as_str() != Some("ok") {
        return Err("Codex Desktop menu has no archive item".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "needs AGI_CODEX_CDP and a thread open in the copied Codex window"]
    async fn live_archive_through_cdp() {
        let thread =
            std::env::var("AGI_CODEX_CDP_THREAD").expect("AGI_CODEX_CDP_THREAD is required");
        archive(&thread)
            .await
            .expect("the archive action should be clickable");
    }
}
