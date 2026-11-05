//! The local IPC channel to an already-running Codex desktop window.
//!
//! The desktop app owns a `codex app-server` child of its own. Its thread state
//! is coordinated through a private Unix socket (`~/.codex/ipc/ipc.sock`):
//! 4-byte little-endian length followed by a UTF-8 JSON frame.
//!
//! This module is deliberately short-lived: one socket per prompt/cancel, one
//! owner discovery, then close. A long-lived event subscriber is a later layer;
//! a request that starts a turn should not depend on a background connection
//! surviving a GUI restart.
//!
//! The methods are private and versioned. The versions below are the ones
//! observed against Codex Desktop 26.917.71314:
//! - owner discovery: 1
//! - start turn: 2, or 3 when `hostId` is present
//! - interrupt: 4, or 5 when `hostId` is present
//!
//! A future app can reject these; the caller turns that into an error instead
//! of silently falling back to a second app-server writer.

#[cfg(unix)]
mod unix {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
    const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
    const MAX_FRAME: usize = 16 * 1024 * 1024;

    static REQUEST: AtomicU64 = AtomicU64::new(1);

    fn socket_path() -> PathBuf {
        let home = std::env::var("CODEX_HOME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| agi_platform::home().join(".codex"));
        home.join("ipc").join("ipc.sock")
    }

    fn next_id() -> String {
        let n = REQUEST.fetch_add(1, Ordering::Relaxed);
        format!("agi-gui-{n}-{}", std::process::id())
    }

    async fn write_frame(stream: &mut UnixStream, value: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        if body.len() > MAX_FRAME {
            return Err("codex ipc frame is too large".to_string());
        }
        stream
            .write_all(&(body.len() as u32).to_le_bytes())
            .await
            .map_err(|error| format!("codex ipc write failed: {error}"))?;
        stream
            .write_all(&body)
            .await
            .map_err(|error| format!("codex ipc write failed: {error}"))?;
        stream
            .flush()
            .await
            .map_err(|error| format!("codex ipc flush failed: {error}"))
    }

    async fn read_frame(stream: &mut UnixStream) -> Result<Value, String> {
        let mut header = [0u8; 4];
        stream
            .read_exact(&mut header)
            .await
            .map_err(|error| format!("codex ipc header failed: {error}"))?;
        let length = u32::from_le_bytes(header) as usize;
        if length == 0 || length > MAX_FRAME {
            return Err(format!("codex ipc invalid frame length {length}"));
        }
        let mut body = vec![0u8; length];
        stream
            .read_exact(&mut body)
            .await
            .map_err(|error| format!("codex ipc body failed: {error}"))?;
        serde_json::from_slice(&body).map_err(|error| format!("codex ipc json: {error}"))
    }

    async fn request(
        stream: &mut UnixStream,
        method: &str,
        params: Value,
        source: Option<&str>,
        target: Option<&str>,
        version: Option<u32>,
        host: Option<&str>,
    ) -> Result<Value, String> {
        let id = next_id();
        let mut envelope = json!({
            "type": "request",
            "requestId": id,
            "method": method,
            "params": params,
            "timeoutMs": 20_000,
        });
        if let Some(source) = source {
            envelope["sourceClientId"] = json!(source);
        }
        if let Some(target) = target {
            envelope["targetClientId"] = json!(target);
        }
        if let Some(version) = version {
            envelope["version"] = json!(version);
        }
        if let Some(host) = host {
            envelope["hostId"] = json!(host);
        }
        write_frame(stream, &envelope).await?;
        let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!("codex ipc {method} timed out"));
            }
            let frame = tokio::time::timeout(remaining, read_frame(stream))
                .await
                .map_err(|_| format!("codex ipc {method} timed out"))??;
            if frame.get("type").and_then(Value::as_str) != Some("response")
                || frame.get("requestId").and_then(Value::as_str) != Some(id.as_str())
            {
                // Broadcasts are not this request's answer. The short-lived
                // path does not need them; drop them rather than mixing a
                // stream snapshot into a control response.
                continue;
            }
            match frame.get("resultType").and_then(Value::as_str) {
                Some("success") => return Ok(frame),
                Some("error") => {
                    let message = frame
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("codex desktop rejected the request");
                    return Err(message.to_string());
                }
                _ => return Err("codex ipc response has no result type".to_string()),
            }
        }
    }

    async fn connect() -> Result<UnixStream, String> {
        let path = socket_path();
        tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&path))
            .await
            .map_err(|_| format!("timed out opening {}", path.display()))?
            .map_err(|error| {
                format!(
                    "could not open {} ({error}); is the Codex desktop window running?",
                    path.display()
                )
            })
    }

    async fn initialize(stream: &mut UnixStream) -> Result<String, String> {
        let result = request(
            stream,
            "initialize",
            json!({ "clientType": "agi-mizar" }),
            None,
            None,
            None,
            None,
        )
        .await?;
        result
            .get("result")
            .and_then(|value| value.get("clientId"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "codex ipc initialize did not return a client id".to_string())
    }

    async fn owner(
        stream: &mut UnixStream,
        client: &str,
        conversation: &str,
    ) -> Result<String, String> {
        let result = request(
            stream,
            "thread-owner-discovery",
            json!({ "conversationId": conversation, "hostId": "local" }),
            Some(client),
            None,
            Some(1),
            Some("local"),
        )
        .await?;
        result
            .get("handledByClientId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                "the Codex desktop window is not showing this thread; open it and retry".to_string()
            })
    }

    async fn follow(
        stream: &mut UnixStream,
        client: &str,
        owner: &str,
        conversation: &str,
    ) -> Result<(), String> {
        let frame = json!({
            "type": "broadcast",
            "method": "thread-stream-following-changed",
            "sourceClientId": client,
            "targetClientIds": [owner],
            "version": 1,
            "params": {
                "conversationId": conversation,
                "hostId": "local",
                "following": true,
            },
        });
        write_frame(stream, &frame).await
    }

    pub(super) fn turn_start(conversation: &str, text: &str, attachments: &[Value]) -> Value {
        let mut input = vec![json!({ "type": "text", "text": text, "text_elements": [] })];
        input.extend(attachments.iter().cloned());
        json!({
            "conversationId": conversation,
            "turnStart": {
                "request": {
                    "threadId": conversation,
                    "turnTrigger": "user",
                    "input": input,
                    "clientUserMessageId": next_id(),
                    "additionalContext": null,
                    "toolOutput": null,
                    "serviceTier": null,
                },
                "context": {
                    "attachments": [],
                    "commentAttachments": [],
                    "useAppServerPermissionDefault": true,
                    "usePermissionSelection": false,
                    "inheritThreadSettings": true,
                    "writingBlockContextPrepared": false,
                    "mcpAppModelContextAttachments": [],
                    "responseItems": [],
                    "localTurnMetadata": {},
                },
            },
        })
    }

    /// Add a message to the turn the desktop window is already running.
    pub async fn steer(conversation: &str, text: &str, cwd: Option<&str>) -> Result<(), String> {
        let mut stream = connect().await?;
        let client = initialize(&mut stream).await?;
        let owner = owner(&mut stream, &client, conversation).await?;
        let cwd = cwd.unwrap_or("/");
        let restore_id = next_id();
        // The desktop's steerTurn reads `restoreMessage.cwd` and
        // `restoreMessage.context.workspaceRoots`; without them it throws
        // "Cannot read properties of undefined (reading 'cwd')".
        let restore = json!({
            "id": restore_id,
            "cwd": cwd,
            "context": { "workspaceRoots": [cwd] },
        });
        request(
            &mut stream,
            "thread-follower-steer-turn",
            json!({
                "conversationId": conversation,
                "input": [{ "type": "text", "text": text, "text_elements": [] }],
                "restoreMessage": restore,
                "clientUserMessageId": next_id(),
            }),
            Some(&client),
            Some(&owner),
            Some(2),
            Some("local"),
        )
        .await?;
        Ok(())
    }

    /// Read one complete conversation snapshot for a thread the GUI has open.
    ///
    /// The owner only emits a snapshot to clients that announce themselves as
    /// followers, so this repeats the discovery + follow handshake and then
    /// waits for `thread-stream-state-changed`. It is intentionally short-lived:
    /// a watcher polls this while a GUI turn is in flight instead of keeping a
    /// private socket open across app restarts.
    pub async fn snapshot(conversation: &str) -> Result<Value, String> {
        let mut stream = connect().await?;
        let client = initialize(&mut stream).await?;
        let owner = owner(&mut stream, &client, conversation).await?;
        follow(&mut stream, &client, &owner, conversation).await?;
        let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for the Codex desktop snapshot".to_string());
            }
            let frame = tokio::time::timeout(remaining, read_frame(&mut stream))
                .await
                .map_err(|_| "timed out waiting for the Codex desktop snapshot".to_string())??;
            if frame.get("method").and_then(Value::as_str) != Some("thread-stream-state-changed") {
                continue;
            }
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            if params.get("conversationId").and_then(Value::as_str) != Some(conversation) {
                continue;
            }
            let change = params.get("change").cloned().unwrap_or(Value::Null);
            if change.get("type").and_then(Value::as_str) == Some("snapshot") {
                if let Some(state) = change.get("conversationState") {
                    return Ok(state.clone());
                }
            }
        }
    }

    /// Answer one pending request on the Codex desktop window.
    pub async fn answer(
        conversation: &str,
        request_id: &Value,
        method: &str,
        decision: &str,
    ) -> Result<(), String> {
        let (ipc_method, params) = match method {
            "item/commandExecution/requestApproval" => (
                "thread-follower-command-approval-decision",
                json!({ "decision": decision }),
            ),
            "item/fileChange/requestApproval" => (
                "thread-follower-file-approval-decision",
                json!({ "decision": decision }),
            ),
            "item/permissions/requestApproval" => (
                "thread-follower-permissions-request-approval-response",
                json!({
                    "response": {
                        "permissions": {},
                        "scope": "turn",
                    }
                }),
            ),
            "item/tool/requestUserInput" => (
                "thread-follower-submit-user-input",
                json!({ "response": { "answers": {} } }),
            ),
            _ => return Err(format!("unsupported Codex desktop request {method}")),
        };
        let mut stream = connect().await?;
        let client = initialize(&mut stream).await?;
        let owner = owner(&mut stream, &client, conversation).await?;
        let mut body = params;
        body["conversationId"] = json!(conversation);
        body["requestId"] = request_id.clone();
        request(
            &mut stream,
            ipc_method,
            body,
            Some(&client),
            Some(&owner),
            Some(2),
            Some("local"),
        )
        .await?;
        Ok(())
    }

    /// Answer a `requestUserInput` question from the desktop window.
    pub async fn answer_user_input(
        conversation: &str,
        request_id: &Value,
        response: Value,
    ) -> Result<(), String> {
        let mut stream = connect().await?;
        let client = initialize(&mut stream).await?;
        let owner = owner(&mut stream, &client, conversation).await?;
        request(
            &mut stream,
            "thread-follower-submit-user-input",
            json!({
                "conversationId": conversation,
                "requestId": request_id,
                "response": response,
            }),
            Some(&client),
            Some(&owner),
            Some(2),
            Some("local"),
        )
        .await?;
        Ok(())
    }

    /// Start a turn in the thread the Codex desktop window is showing.
    pub async fn start_turn(
        conversation: &str,
        text: &str,
        attachments: &[Value],
    ) -> Result<String, String> {
        let mut stream = connect().await?;
        let client = initialize(&mut stream).await?;
        let owner = owner(&mut stream, &client, conversation).await?;
        follow(&mut stream, &client, &owner, conversation).await?;
        let result = request(
            &mut stream,
            "thread-follower-start-turn",
            turn_start(conversation, text, attachments),
            Some(&client),
            Some(&owner),
            Some(3),
            Some("local"),
        )
        .await?;
        result
            .get("result")
            .and_then(|value| value.get("result"))
            .and_then(|value| value.get("turn"))
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "Codex desktop accepted the turn but returned no turn id".to_string())
    }

    async fn update_settings(conversation: &str, settings: Value) -> Result<(), String> {
        let mut stream = connect().await?;
        let client = initialize(&mut stream).await?;
        let owner = owner(&mut stream, &client, conversation).await?;
        request(
            &mut stream,
            "thread-follower-update-thread-settings",
            json!({
                "conversationId": conversation,
                "threadSettings": settings,
                "condition": null,
            }),
            Some(&client),
            Some(&owner),
            Some(3),
            Some("local"),
        )
        .await?;
        Ok(())
    }

    /// Choose the model for the next GUI turn.
    pub async fn set_model(conversation: &str, model: &str) -> Result<(), String> {
        update_settings(conversation, json!({ "model": model })).await
    }

    /// Choose the permission profile for the next GUI turn.
    ///
    /// The desktop accepts the profile id on its own and derives the matching
    /// sandbox; sending a hand-built sandbox as well would make the two fields
    /// disagree after an app update.
    pub async fn set_permission(conversation: &str, preset: &str) -> Result<(), String> {
        update_settings(
            conversation,
            json!({ "activePermissionProfile": { "id": preset, "extends": null } }),
        )
        .await
    }

    /// Ask the Codex desktop window to stop the turn it is running.
    pub async fn interrupt(conversation: &str, turn: &str) -> Result<(), String> {
        let mut stream = connect().await?;
        let client = initialize(&mut stream).await?;
        let owner = owner(&mut stream, &client, conversation).await?;
        request(
            &mut stream,
            "thread-follower-interrupt-turn",
            json!({
                "conversationId": conversation,
                "mode": "user-stop",
                "expectedTurnId": turn,
            }),
            Some(&client),
            Some(&owner),
            Some(5),
            Some("local"),
        )
        .await?;
        Ok(())
    }
}

#[cfg(unix)]
pub use unix::{
    answer, answer_user_input, interrupt, set_model, set_permission, snapshot, start_turn, steer,
};

#[cfg(not(unix))]
pub async fn start_turn(_conversation: &str, _text: &str) -> Result<String, String> {
    Err("Codex desktop control is only available on macOS and Linux".to_string())
}

#[cfg(not(unix))]
pub async fn interrupt(_conversation: &str, _turn: &str) -> Result<(), String> {
    Err("Codex desktop control is only available on macOS and Linux".to_string())
}

#[cfg(not(unix))]
pub async fn set_model(_conversation: &str, _model: &str) -> Result<(), String> {
    Err("Codex desktop control is only available on macOS and Linux".to_string())
}

#[cfg(not(unix))]
pub async fn set_permission(_conversation: &str, _preset: &str) -> Result<(), String> {
    Err("Codex desktop control is only available on macOS and Linux".to_string())
}

#[cfg(not(unix))]
pub async fn steer(_conversation: &str, _text: &str, _cwd: Option<&str>) -> Result<(), String> {
    Err("Codex desktop control is only available on macOS and Linux".to_string())
}

#[cfg(not(unix))]
pub async fn snapshot(_conversation: &str) -> Result<Value, String> {
    Err("Codex desktop control is only available on macOS and Linux".to_string())
}

#[cfg(not(unix))]
pub async fn answer(
    _conversation: &str,
    _request_id: &Value,
    _method: &str,
    _decision: &str,
) -> Result<(), String> {
    Err("Codex desktop control is only available on macOS and Linux".to_string())
}

#[cfg(not(unix))]
pub async fn answer_user_input(
    _conversation: &str,
    _request_id: &Value,
    _response: Value,
) -> Result<(), String> {
    Err("Codex desktop control is only available on macOS and Linux".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_turn_start_matches_the_shape_the_desktop_accepts() {
        let value = unix::turn_start("thread-1", "hello", &[]);
        assert_eq!(value["conversationId"], "thread-1");
        assert_eq!(value["turnStart"]["request"]["threadId"], "thread-1");
        assert_eq!(value["turnStart"]["request"]["input"][0]["text"], "hello");
        assert_eq!(value["turnStart"]["context"]["inheritThreadSettings"], true);
    }
}

#[cfg(all(test, unix))]
mod live_tests {
    use super::*;

    /// Exercise the real desktop IPC against a thread the GUI has open.
    ///
    /// Ignored by default: it opens a socket and sends one tiny turn. Run with
    /// `AGI_CODEX_GUI_TEST_THREAD=<thread-id> cargo test -p agi-engine-codex \
    ///  live_desktop_start_and_interrupt -- --ignored`.
    #[tokio::test]
    #[ignore = "uses the live Codex desktop window and sends one test turn"]
    async fn live_desktop_start_and_interrupt() {
        let conversation = std::env::var("AGI_CODEX_GUI_TEST_THREAD")
            .expect("AGI_CODEX_GUI_TEST_THREAD is required");
        let turn = start_turn(&conversation, "Reply with exactly: OK", &[])
            .await
            .expect("the desktop window should start the turn");
        assert!(!turn.is_empty());
        interrupt(&conversation, &turn)
            .await
            .expect("the desktop window should interrupt the turn it just started");
        set_model(&conversation, "deepseek-flash")
            .await
            .expect("the desktop window should accept a model choice");
        set_permission(&conversation, ":read-only")
            .await
            .expect("the desktop window should accept a permission profile");
    }
}
