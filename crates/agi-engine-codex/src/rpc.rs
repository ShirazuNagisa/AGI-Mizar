//! The transport to `codex app-server`.
//!
//! Codex speaks newline-delimited JSON over stdio: `{id, method, params}` for a
//! request, `{id, result}` or `{id, error}` for its answer, `{method, params}` for
//! a notification, and — the part that makes it a real driver interface rather
//! than a command line — **server-initiated requests** that carry an `id` and
//! expect an answer. Approvals arrive that way.
//!
//! ### Why this file knows nothing about threads
//!
//! Correlation, framing and restart are one concern; what a `turn/start` means is
//! another. Keeping the meaning out of the transport is what makes a Codex
//! protocol change a one-file edit instead of a rewrite.
//!
//! ### Why the child is restarted rather than reconnected
//!
//! There is no reconnect: the app-server *is* the child process. If it exits, the
//! next start spawns a new one, and every cached thread view is invalid — a new
//! server has a different set of loaded threads. The engine clears its cache on
//! [`RpcEvent::Exited`] for exactly that reason.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};

/// How long one request may take before it is failed.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the `initialize` handshake may take.
pub const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How many transport events a slow consumer may fall behind.
const EVENT_BUFFER: usize = 256;

/// How to start the app-server.
#[derive(Debug, Clone)]
pub struct AppServerConfig {
    /// The executable. `codex` resolves from `PATH`.
    pub binary: String,
    /// Extra arguments placed before `app-server`.
    pub args: Vec<String>,
    /// Working directory for the child.
    pub cwd: Option<std::path::PathBuf>,
}

impl Default for AppServerConfig {
    fn default() -> Self {
        Self {
            binary: "codex".to_string(),
            args: Vec::new(),
            cwd: None,
        }
    }
}

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// No child is running.
    #[error("the Codex app-server is not running")]
    NotRunning,
    /// Codex answered with an error object.
    #[error("codex: {message} (code {code})")]
    Upstream { code: i64, message: String },
    /// The answer did not arrive in time.
    #[error("timed out waiting for {0}")]
    Timeout(String),
    /// The child could not be started, or the pipe broke.
    #[error("{0}")]
    Transport(String),
}

impl RpcError {
    /// The code a caller should branch on.
    ///
    /// `session/writer-held` is the one Codex failure a person has to act on: the
    /// thread is open in another window. Everything else is a message.
    pub fn is_writer_held(&self) -> bool {
        matches!(self, Self::Upstream { message, .. } if message.contains("locked") || message.contains("in use"))
    }
}

/// Something the server said that was not an answer.
#[derive(Debug, Clone)]
pub enum RpcEvent {
    /// A one-way notification: `turn/started`, `item/completed`, …
    Notification { method: String, params: Value },
    /// A request that expects an answer: approvals arrive this way.
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// The child exited, for any reason.
    Exited { reason: String },
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// One app-server child and the pipe to it.
pub struct AppServer {
    config: AppServerConfig,
    child: Mutex<Option<Child>>,
    stdin: AsyncMutex<Option<tokio::process::ChildStdin>>,
    alive: Arc<AtomicBool>,
    pending: Pending,
    next_id: AtomicU64,
    events: broadcast::Sender<RpcEvent>,
}

impl AppServer {
    /// Build a transport. Nothing is spawned until [`AppServer::start`].
    pub fn new(config: AppServerConfig) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Arc::new(Self {
            config,
            child: Mutex::new(None),
            stdin: AsyncMutex::new(None),
            alive: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            events,
        })
    }

    /// The event stream. One receiver per subscriber.
    pub fn events(&self) -> broadcast::Sender<RpcEvent> {
        self.events.clone()
    }

    /// Whether a child is running.
    pub fn running(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn pending(&self) -> MutexGuard<'_, HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Spawn the child and complete the `initialize` handshake.
    ///
    /// Returns the handshake result, which carries `userAgent` and `platformOs` —
    /// the only two facts Codex volunteers about itself.
    pub async fn start(&self) -> Result<Value, RpcError> {
        self.stop().await;

        let mut command = Command::new(&self.config.binary);
        command
            .args(&self.config.args)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &self.config.cwd {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().map_err(|error| {
            RpcError::Transport(format!("could not start {}: {error}", self.config.binary))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RpcError::Transport("the child has no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RpcError::Transport("the child has no stdout".to_string()))?;
        let stderr = child.stderr.take();

        *self.stdin.lock().await = Some(stdin);
        self.alive.store(true, Ordering::Relaxed);
        *self.child.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);

        {
            let pending = Arc::clone(&self.pending);
            let events = self.events.clone();
            let alive = Arc::clone(&self.alive);
            tokio::spawn(async move {
                read_stdout(stdout, pending, events, alive).await;
            });
        }

        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim();
                    // Codex is chatty about MCP startup and config warnings on
                    // stderr; only lines that look like failures are worth an
                    // operator's attention.
                    if line.is_empty() {
                        continue;
                    }
                    if line.contains("error") || line.contains("failed") || line.contains("panic") {
                        tracing::warn!("codex: {line}");
                    }
                }
            });
        }

        let initialized = self
            .request(
                "initialize",
                json!({
                    "clientInfo": { "name": "agi-mizar", "title": "AGI Mizar", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": {
                        "experimentalApi": true,
                        "requestAttestation": false,
                        "optOutNotificationMethods": [],
                    }
                }),
                START_TIMEOUT,
            )
            .await?;

        tracing::info!(
            "codex: app-server ready ({})",
            initialized
                .get("userAgent")
                .and_then(|agent| agent.as_str())
                .unwrap_or("unknown version")
        );
        Ok(initialized)
    }

    /// Stop the child, failing everything that was waiting on it.
    pub async fn stop(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let stdin = self.stdin.lock().await.take();
        drop(stdin);

        let child = self.child.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }

        let waiting: Vec<_> = self.pending().drain().map(|(_, sender)| sender).collect();
        for sender in waiting {
            let _ = sender.send(Err(RpcError::NotRunning));
        }
    }

    /// One request, correlated by id.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending().insert(id, tx);

        if let Err(error) = self
            .write(json!({ "id": id, "method": method, "params": params }))
            .await
        {
            self.pending().remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::Transport(format!("{method} was dropped"))),
            Err(_) => {
                self.pending().remove(&id);
                Err(RpcError::Timeout(method.to_string()))
            }
        }
    }

    /// One request with the default timeout.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.request(method, params, REQUEST_TIMEOUT).await
    }

    /// Answer a server-initiated request.
    pub async fn respond(&self, id: Value, result: Value) -> Result<(), RpcError> {
        self.write(json!({ "id": id, "result": result })).await
    }

    /// Refuse a server-initiated request.
    pub async fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<(), RpcError> {
        self.write(json!({ "id": id, "error": { "code": code, "message": message } }))
            .await
    }

    /// Write one JSON line to the child.
    async fn write(&self, value: Value) -> Result<(), RpcError> {
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(RpcError::NotRunning);
        };
        let mut line = serde_json::to_string(&value)
            .map_err(|error| RpcError::Transport(format!("could not encode a request: {error}")))?;
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| RpcError::Transport(format!("the pipe to Codex broke: {error}")))
    }
}

/// Read the child's stdout, routing each line by its shape.
async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    pending: Pending,
    events: broadcast::Sender<RpcEvent>,
    alive: Arc<AtomicBool>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!("codex: reading stdout failed — {error}");
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            tracing::debug!("codex: non-JSON line ignored");
            continue;
        };

        let id = value.get("id").cloned();
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);

        match (id, method) {
            // An answer to something we asked.
            (Some(id), None) => {
                let Some(key) = id.as_u64() else { continue };
                let sender = pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&key);
                let Some(sender) = sender else { continue };
                let outcome = match value.get("error") {
                    Some(error) => Err(RpcError::Upstream {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("Codex refused the request")
                            .to_string(),
                    }),
                    None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            }
            // A request from the server, which expects an answer.
            (Some(id), Some(method)) => {
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                let _ = events.send(RpcEvent::ServerRequest { id, method, params });
            }
            // A one-way notification.
            (None, Some(method)) => {
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                let _ = events.send(RpcEvent::Notification { method, params });
            }
            _ => {}
        }
    }

    // The pipe ended: everything still waiting will never be answered.
    alive.store(false, Ordering::Relaxed);
    let waiting: Vec<_> = pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drain()
        .map(|(_, sender)| sender)
        .collect();
    for sender in waiting {
        let _ = sender.send(Err(RpcError::Transport(
            "the Codex app-server closed its output".to_string(),
        )));
    }
    let _ = events.send(RpcEvent::Exited {
        reason: "the app-server closed its output".to_string(),
    });
}
