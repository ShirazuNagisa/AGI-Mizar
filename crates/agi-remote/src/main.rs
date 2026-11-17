//! `agi-remote`: the bridge an AGI Mizar agent uses to drive a same-account peer.
//!
//! Two faces, one client:
//! - `agi-remote mcp` speaks MCP over stdio, one JSON-RPC object per line.
//! - the other subcommands are a CLI with the same calls, for tests and for
//!   engines that have not been wired to MCP yet.
//!
//! Every call goes through the local API with the scoped `agent-tool.json`
//! token. There is no direct relay or peer credential here: the host kernel
//! owns the account session and the peer token.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

mod install;

const SKILL: &str = include_str!("../skills/agi-mizar-remote/SKILL.md");
const MCP_ENTRY_ID: &str = "mcp-agi-mizar-remote";
const SKILL_NAME: &str = "agi-mizar-remote";
/// The agents an MCP entry and a skill can be written for, in the order the
/// report lists them.
const AGENTS: [(&str, &str); 3] = [
    ("dsh", "DSH"),
    ("claude", "Claude Code"),
    ("codex", "Codex"),
];

/// Which agent launched this MCP server.
///
/// The installer writes `--host <id>` into each agent's MCP entry because the
/// three products differ in one way that matters here: only DSH has a
/// settlement-notice bridge in AGI Mizar, so only DSH can be woken after its
/// turn ends. A delegate tool that returned immediately to Claude Code or
/// Codex left the caller with nobody to tell it the subagent had finished —
/// the work completed, the session was archived, and the turn was already over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    Dsh,
    Claude,
    Codex,
}

impl Host {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "dsh" => Some(Self::Dsh),
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The `--host` value this installer writes for the agent.
    fn id(self) -> &'static str {
        match self {
            Self::Dsh => "dsh",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Whether a settled delegation can wake this host's turn by itself.
    ///
    /// True for DSH only: `dsh-mizar` maps the `AGI_MIZAR_DELEGATION` marker
    /// back to the parent session and injects the settlement follow-up. The
    /// other two have no such channel, so the tool waits instead.
    fn wakes_on_notice(self) -> bool {
        matches!(self, Self::Dsh)
    }
}

/// How long a delegate call waits for settlement when the host cannot be woken.
///
/// Under the 60-second MCP tool timeout every one of these clients applies by
/// default, so the call answers "still running" instead of being killed. The
/// skill then keeps the caller in a `remote_wait` loop until the task settles.
const DELEGATE_WAIT_MS: u64 = 50_000;

fn main() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("a Tokio runtime")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");
    match command {
        "mcp" => mcp(&runtime, mcp_host(&args[1..])?),
        "agents" => runtime.block_on(cli_agents()),
        "options" => runtime.block_on(cli_options(&args[1..])),
        "delegate" => runtime.block_on(cli_delegate(&args[1..])),
        "local-options" => runtime.block_on(cli_local_options()),
        "local-delegate" => runtime.block_on(cli_local_delegate(&args[1..])),
        "result" | "status" => runtime.block_on(cli_result(&args[1..])),
        "wait" => runtime.block_on(cli_wait(&args[1..])),
        "cancel" => runtime.block_on(cli_cancel(&args[1..])),
        "answer" => runtime.block_on(cli_answer(&args[1..])),
        "install" => install(&args[1..]),
        "uninstall" | "remove" => uninstall(&args[1..]),
        "help" | "--help" | "-h" => {
            usage();
            Ok(())
        }
        other => bail!("unknown command {other}; run `agi-remote help`"),
    }
}

/// The host an MCP launch declares.
///
/// An entry written before `--host` existed is a DSH entry: that was the only
/// agent this installer served. Guessing "DSH" keeps those installs working as
/// they did, and `agi-remote install` rewrites the entry with the flag.
fn mcp_host(args: &[String]) -> Result<Host> {
    match flag(args, "--host") {
        None => Ok(Host::Dsh),
        Some(value) => Host::parse(&value)
            .ok_or_else(|| anyhow!("--host {value} is not one of dsh, claude, codex")),
    }
}

fn usage() {
    println!(
        "agi-remote — drive a same-account AGI Mizar computer\n\
\n\
  agi-remote agents\n\
  agi-remote options --peer <id>\n\
  agi-remote delegate --peer <id> [--workspace <path>] [--permission <preset>] \\\n\
      [--provider <id> --model <id> [--reasoning-effort <id>]] --prompt <text>\n\
  agi-remote local-options\n\
  agi-remote local-delegate [--workspace <path>] [--permission <preset>] \\\n\
      [--provider <id> --model <id> [--reasoning-effort <id>]] --prompt <text>\n\
  agi-remote result <delegationId>\n\
  agi-remote wait <delegationId> [--timeout-ms <ms>]\n\
  agi-remote cancel <delegationId>\n\
  agi-remote answer <delegationId> <approvalId> <outcome>\n\
  agi-remote mcp                 # MCP over stdio\n\
  agi-remote install [--profile <name>] [--dry-run]\n\
  agi-remote uninstall [--profile <name>] [--dry-run]\n\
\n\
The tool token is read from agent-tool.json beside the other Mizar state.\n\
Install writes the MCP entry and the agi-mizar-remote skill into every agent\n\
found here (DSH, Claude Code, Codex); restart the agent afterwards.\n\
`local-*` drives this computer's DSH, the rest drives another computer.\n\
after installing."
    );
}

// ---------------------------------------------------------------- local API

#[derive(Debug, Clone)]
struct Api {
    port: u16,
    token: String,
}

fn tool_path() -> PathBuf {
    agi_platform::data_dir().join("agent-tool.json")
}

fn read_api() -> Result<Api> {
    let path = tool_path();
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} — is AGI Mizar running with remote delegation enabled?",
            path.display()
        )
    })?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let port = value
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| anyhow!("{} has no port", path.display()))?;
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("{} has no token", path.display()))?
        .to_string();
    Ok(Api { port, token })
}

/// POST one command to the kernel, rereading the token once on a stale handle.
async fn command(body: Value) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(65))
        .build()
        .context("an HTTP client")?;
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 0..2 {
        let api = read_api()?;
        let response = match client
            .post(format!("http://127.0.0.1:{}/api/v1/command", api.port))
            .bearer_auth(&api.token)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(anyhow!("the local API is unreachable: {error}"));
                if attempt == 0 {
                    continue;
                }
                break;
            }
        };
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
            // The app may have restarted while this tool was in flight.
            continue;
        }
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("the local API answered {status} with non-JSON"))?;
        if !status.is_success() {
            bail!(
                "the local API answered {status}: {}",
                value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or(text.as_str())
            );
        }
        if value.get("ok").and_then(Value::as_bool) == Some(false) {
            let error = value.get("error").cloned().unwrap_or(Value::Null);
            let code = error.get("code").and_then(Value::as_str).unwrap_or("ERROR");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the command failed");
            bail!("{code}: {message}");
        }
        return Ok(value.get("value").cloned().unwrap_or(Value::Null));
    }
    Err(last_error.unwrap_or_else(|| anyhow!("the local API did not answer")))
}

fn banner(text: &str) {
    println!("{text}");
}

async fn cli_agents() -> Result<()> {
    let value = command(json!({ "op": "remote_peers" })).await?;
    banner(&serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn cli_options(args: &[String]) -> Result<()> {
    let peer = flag(args, "--peer").ok_or_else(|| anyhow!("--peer is required"))?;
    let value = command(json!({
        "op": "remote_options",
        "peer": peer,
    }))
    .await?;
    banner(&serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn cli_delegate(args: &[String]) -> Result<()> {
    let peer = flag(args, "--peer").ok_or_else(|| anyhow!("--peer is required"))?;
    let prompt = flag(args, "--prompt")
        .or_else(|| flag(args, "-p"))
        .ok_or_else(|| anyhow!("--prompt is required"))?;
    let cwd = flag(args, "--workspace").or_else(|| flag(args, "--cwd"));
    let permission = flag(args, "--permission");
    let provider = flag(args, "--provider");
    let model = flag(args, "--model");
    let reasoning_effort = flag(args, "--reasoning-effort").or_else(|| flag(args, "--effort"));
    let value = command(json!({
        "op": "remote_delegate",
        "peer": peer,
        "cwd": cwd,
        "permission": permission,
        "provider": provider,
        "model": model,
        "reasoning_effort": reasoning_effort,
        "prompt": prompt,
    }))
    .await?;
    let id = value
        .get("delegation")
        .and_then(|delegation| delegation.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the kernel returned no delegation id"))?;
    banner(&format!("AGI_MIZAR_DELEGATION {id}"));
    banner(&serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn cli_local_options() -> Result<()> {
    let value = command(json!({ "op": "local_options" })).await?;
    banner(&serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn cli_local_delegate(args: &[String]) -> Result<()> {
    let prompt = flag(args, "--prompt")
        .or_else(|| flag(args, "-p"))
        .ok_or_else(|| anyhow!("--prompt is required"))?;
    let cwd = flag(args, "--workspace").or_else(|| flag(args, "--cwd"));
    let value = command(json!({
        "op": "local_delegate",
        "cwd": cwd,
        "permission": flag(args, "--permission"),
        "provider": flag(args, "--provider"),
        "model": flag(args, "--model"),
        "reasoning_effort": flag(args, "--reasoning-effort").or_else(|| flag(args, "--effort")),
        "prompt": prompt,
    }))
    .await?;
    let id = value
        .get("delegation")
        .and_then(|delegation| delegation.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the kernel returned no delegation id"))?;
    banner(&format!("AGI_MIZAR_DELEGATION {id}"));
    banner(&serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn cli_result(args: &[String]) -> Result<()> {
    let id = positional(args).ok_or_else(|| anyhow!("a delegation id is required"))?;
    let value = command(json!({ "op": "remote_result", "id": id })).await?;
    banner(&serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn cli_wait(args: &[String]) -> Result<()> {
    let id = positional(args).ok_or_else(|| anyhow!("a delegation id is required"))?;
    let timeout = flag(args, "--timeout-ms")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30_000)
        .min(55_000);
    let value = wait_for(&id, timeout).await?;
    banner(&serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn wait_for(id: &str, timeout_ms: u64) -> Result<Value> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let value = command(json!({ "op": "remote_result", "id": id })).await?;
        let state = value
            .get("delegation")
            .and_then(|delegation| delegation.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if matches!(state, "done" | "failed" | "cancelled" | "interrupted")
            || std::time::Instant::now() >= deadline
        {
            return Ok(value);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn cli_cancel(args: &[String]) -> Result<()> {
    let id = positional(args).ok_or_else(|| anyhow!("a delegation id is required"))?;
    let value = command(json!({ "op": "remote_cancel", "id": id })).await?;
    banner(&serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn cli_answer(args: &[String]) -> Result<()> {
    let id = args
        .first()
        .ok_or_else(|| anyhow!("a delegation id is required"))?;
    let approval = args
        .get(1)
        .ok_or_else(|| anyhow!("an approval id is required"))?;
    let outcome = args
        .get(2)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("an outcome is required"))?;
    let value = command(json!({
        "op": "remote_answer",
        "id": id,
        "approval": approval,
        "outcome": outcome,
    }))
    .await?;
    banner(&serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn positional(args: &[String]) -> Option<String> {
    args.iter()
        .find(|arg| !arg.starts_with('-'))
        .map(|arg| arg.to_string())
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == name)?;
    args.get(index + 1).cloned()
}

// -------------------------------------------------------------------- MCP

fn mcp(runtime: &tokio::runtime::Runtime, host: Host) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                let response = json!({
                    "jsonrpc": "2.0", "id": Value::Null,
                    "error": { "code": -32700, "message": format!("parse error: {error}") }
                });
                writeln!(stdout, "{response}")?;
                stdout.flush()?;
                continue;
            }
        };
        // Notifications have no id and get no response.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let response = match method {
            "initialize" => {
                let version = request
                    .get("params")
                    .and_then(|params| params.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .unwrap_or("2024-11-05");
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": version,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "agi-mizar-remote", "version": env!("CARGO_PKG_VERSION") }
                    }
                })
            }
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "tools": tools() }
            }),
            "tools/call" => {
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                match runtime.block_on(call_tool(name, &arguments, host)) {
                    Ok(text) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "content": [{ "type": "text", "text": text }], "isError": false }
                    }),
                    Err(error) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": format!("{error:#}") }],
                            "isError": true
                        }
                    }),
                }
            }
            other => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("unknown method {other}") }
            }),
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

fn tools() -> Value {
    json!([
        {
            "name": "remote_agents",
            "description": "List same-account computers that can run remote work. Check online, routes, remote.enabled and remote.workspaces before delegating.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "remote_options",
            "description": "Ask one peer which workspaces it permits, and its permission and model catalogs. Call this for every candidate peer before the single intake question; pass the chosen provider/model/effort to remote_delegate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer": { "type": "string", "description": "deviceId from remote_agents" }
                },
                "required": ["peer"],
                "additionalProperties": false
            }
        },
        {
            "name": "remote_delegate",
            "description": "Start a self-contained task on a same-account peer. The remote agent cannot see this conversation. On DSH this returns immediately with a delegation id (marker AGI_MIZAR_DELEGATION <id>) and a settlement notice wakes you. On Claude Code and Codex it waits up to ~50 s and returns the answer; if it says STILL RUNNING, call remote_wait with the same delegationId until the task settles — do not end your turn.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer": { "type": "string", "description": "deviceId from remote_agents" },
                    "workspace": { "type": "string", "description": "one of the peer's allowed workspace paths; omit only if the peer allows its default" },
                    "permission": { "type": "string", "description": "permission preset value returned by remote_options" },
                    "provider": { "type": "string", "description": "provider id from remote_options.models" },
                    "model": { "type": "string", "description": "model id from remote_options.models" },
                    "reasoningEffort": { "type": "string", "description": "one of the selected model's efforts; omit for the model default" },
                    "description": { "type": "string", "description": "a short human-readable label" },
                    "prompt": { "type": "string", "description": "self-contained goal, inputs, constraints, deliverable and acceptance checks" }
                },
                "required": ["peer", "prompt"],
                "additionalProperties": false
            }
        },
        {
            "name": "remote_result",
            "description": "Read a delegation's state and final text. Prefer this after a settlement notice; do not poll in a loop.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "delegationId": { "type": "string" }
                },
                "required": ["delegationId"],
                "additionalProperties": false
            }
        },
        {
            "name": "remote_wait",
            "description": "Wait up to timeoutMs (max 55 s) for a delegation to settle and return its state, final text and archived flag. This is the loop for hosts without a settlement notice: call it again with the same delegationId until state is done, failed or cancelled.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "delegationId": { "type": "string" },
                    "timeoutMs": { "type": "integer", "minimum": 1000, "maximum": 55000 }
                },
                "required": ["delegationId"],
                "additionalProperties": false
            }
        },
        {
            "name": "remote_cancel",
            "description": "Cancel the remote turn and mark the delegation cancelled.",
            "inputSchema": {
                "type": "object",
                "properties": { "delegationId": { "type": "string" } },
                "required": ["delegationId"],
                "additionalProperties": false
            }
        },
        {
            "name": "remote_answer",
            "description": "Answer a low-risk remote approval. High-risk or unknown approvals must be left to the remote operator (HUMAN_REQUIRED). P4.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "delegationId": { "type": "string" },
                    "approvalId": { "type": "string" },
                    "outcome": { "type": "string", "enum": ["allowed-once", "allowed-always", "rejected"] }
                },
                "required": ["delegationId", "approvalId", "outcome"],
                "additionalProperties": false
            }
        },
        {
            "name": "remote_send",
            "description": "Send a follow-up to a delegation. Works for local and remote delegations; the session keeps its context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "delegationId": { "type": "string" },
                    "message": { "type": "string" }
                },
                "required": ["delegationId", "message"],
                "additionalProperties": false
            }
        },
        {
            "name": "local_options",
            "description": "Ask this computer's own DSH what it offers: whether it is connected, its workspaces, permission presets and model catalog. Call before local_delegate.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "local_delegate",
            "description": "Start a self-contained task on this computer's DSH as a subagent. On DSH this returns immediately with a delegation id (marker AGI_MIZAR_DELEGATION <id>) and a settlement notice wakes you. On Claude Code and Codex it waits up to ~50 s and returns the answer; if it says STILL RUNNING, call remote_wait with the same delegationId until the task settles — do not end your turn.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "absolute path from local_options.workspaces; omit for the DSH default" },
                    "permission": { "type": "string", "description": "permission preset value returned by local_options.permissions" },
                    "provider": { "type": "string", "description": "provider id from local_options.models" },
                    "model": { "type": "string", "description": "model id from local_options.models" },
                    "reasoningEffort": { "type": "string", "description": "one of the selected model's efforts; omit for the model default" },
                    "description": { "type": "string", "description": "a short human-readable label" },
                    "prompt": { "type": "string", "description": "self-contained goal, inputs, constraints, deliverable and acceptance checks" }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }
        },
        {
            "name": "local_send",
            "description": "Send a follow-up to a local delegation, keeping its DSH session. Prefer this over a second local_delegate in the same conversation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "delegationId": { "type": "string" },
                    "message": { "type": "string" }
                },
                "required": ["delegationId", "message"],
                "additionalProperties": false
            }
        }
    ])
}

async fn call_tool(name: &str, arguments: &Value, host: Host) -> Result<String> {
    match name {
        "remote_agents" => {
            let value = command(json!({ "op": "remote_peers" })).await?;
            Ok(serde_json::to_string_pretty(&value)?)
        }
        "remote_options" => {
            let peer = required(arguments, "peer")?;
            let value = command(json!({
                "op": "remote_options",
                "peer": peer,
            }))
            .await?;
            Ok(serde_json::to_string_pretty(&value)?)
        }
        "remote_delegate" => {
            let peer = required(arguments, "peer")?;
            let prompt = required(arguments, "prompt")?;
            let cwd = arguments
                .get("workspace")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let permission = arguments
                .get("permission")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let provider = arguments
                .get("provider")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let model = arguments
                .get("model")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let reasoning_effort = arguments
                .get("reasoningEffort")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let value = command(json!({
                "op": "remote_delegate",
                "peer": peer,
                "cwd": cwd,
                "permission": permission,
                "provider": provider,
                "model": model,
                "reasoning_effort": reasoning_effort,
                "prompt": prompt,
            }))
            .await?;
            let id = value
                .get("delegation")
                .and_then(|delegation| delegation.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the kernel returned no delegation id"))?
                .to_string();
            delegate_answer(host, value, &id).await
        }
        "remote_result" => {
            let id = required(arguments, "delegationId")?;
            let value = command(json!({ "op": "remote_result", "id": id })).await?;
            Ok(serde_json::to_string_pretty(&value)?)
        }
        "remote_wait" => {
            let id = required(arguments, "delegationId")?;
            let timeout = arguments
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(30_000)
                .clamp(1_000, 55_000);
            let value = wait_for(&id, timeout).await?;
            Ok(serde_json::to_string_pretty(&value)?)
        }
        "remote_cancel" => {
            let id = required(arguments, "delegationId")?;
            let value = command(json!({ "op": "remote_cancel", "id": id })).await?;
            Ok(serde_json::to_string_pretty(&value)?)
        }
        "remote_answer" => {
            let id = required(arguments, "delegationId")?;
            let approval = required(arguments, "approvalId")?;
            let outcome = required(arguments, "outcome")?;
            let value = command(json!({
                "op": "remote_answer",
                "id": id,
                "approval": approval,
                "outcome": outcome,
            }))
            .await?;
            Ok(serde_json::to_string_pretty(&value)?)
        }
        "remote_send" => {
            let id = required(arguments, "delegationId")?;
            let message = required(arguments, "message")?;
            let value = command(json!({
                "op": "remote_send",
                "id": id,
                "message": message,
            }))
            .await?;
            Ok(serde_json::to_string_pretty(&value)?)
        }
        "local_options" => {
            let value = command(json!({ "op": "local_options" })).await?;
            Ok(serde_json::to_string_pretty(&value)?)
        }
        "local_delegate" => {
            let prompt = required(arguments, "prompt")?;
            let value = command(json!({
                "op": "local_delegate",
                "cwd": optional(arguments, "workspace"),
                "permission": optional(arguments, "permission"),
                "provider": optional(arguments, "provider"),
                "model": optional(arguments, "model"),
                "reasoning_effort": optional(arguments, "reasoningEffort"),
                "prompt": prompt,
            }))
            .await?;
            let id = value
                .get("delegation")
                .and_then(|delegation| delegation.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("the kernel returned no delegation id"))?
                .to_string();
            delegate_answer(host, value, &id).await
        }
        "local_send" => {
            let id = required(arguments, "delegationId")?;
            let message = required(arguments, "message")?;
            let value = command(json!({
                "op": "local_send",
                "id": id,
                "message": message,
            }))
            .await?;
            Ok(serde_json::to_string_pretty(&value)?)
        }
        other => bail!("unknown tool {other}"),
    }
}

/// What a delegate tool answers.
///
/// A host that can be woken gets the delegation id and nothing else: its turn
/// ends and the settlement notice starts a new one, which is what keeps DSH
/// free to do other work. Every other host gets the answer itself, because
/// nothing will wake it once its turn ends — the subagent finished, the session
/// was archived, and the caller had already stopped.
async fn delegate_answer(host: Host, started: Value, id: &str) -> Result<String> {
    if host.wakes_on_notice() {
        return Ok(format!(
            "AGI_MIZAR_DELEGATION {id}\n{}",
            serde_json::to_string_pretty(&started)?
        ));
    }
    let settled = wait_for(id, DELEGATE_WAIT_MS).await?;
    let state = settled
        .get("delegation")
        .and_then(|delegation| delegation.get("state"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    // The sentence a model reads first when the task outlives one tool call.
    let note = if state == "running" {
        format!(
            "STILL RUNNING — the subagent has not finished. Call remote_wait with \
             delegationId {id} again (each call waits up to 55 s) until state is done, \
             failed or cancelled, then continue your own work. Do not end your turn.
"
        )
    } else {
        String::new()
    };
    Ok(format!(
        "AGI_MIZAR_DELEGATION {id}\n{note}{}",
        serde_json::to_string_pretty(&settled)?
    ))
}

/// A string argument that may be missing; an empty one is the same as missing.
fn optional(arguments: &Value, name: &str) -> Option<String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn required(arguments: &Value, name: &str) -> Result<String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{name} is required"))
}

// -------------------------------------------------------------- installer

/// Where DSH keeps its profiles. `DSH_HOME` wins, then the platform home
/// (on Windows `USERPROFILE`; a process launched from Explorer has no `HOME`).
fn dsh_home() -> PathBuf {
    agi_platform::dsh_home()
}

/// Quote a scalar for YAML without double-quote escapes.
///
/// A Windows path is `C:\Users\...`, and `\U` / `\w` inside a double-quoted
/// YAML scalar are not valid escapes, so the generated `cordis.patch.yml`
/// would fail to parse and DSH would reject the whole patch. Single-quoted
/// scalars are literal; the only escape is doubling an embedded `'`.
fn yaml_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn profile_dir(args: &[String]) -> Result<PathBuf> {
    let dsh = dsh_home();
    let explicit = flag(args, "--profile").or_else(|| std::env::var("DSH_PROFILE").ok());
    if let Some(name) = explicit {
        let dir = dsh.join("profiles").join(&name);
        if !dir.is_dir() {
            bail!(
                "no DSH profile {} under {}",
                name,
                dsh.join("profiles").display()
            );
        }
        return Ok(dir);
    }
    let profiles = dsh.join("profiles");
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&profiles) {
        for entry in entries.flatten() {
            if entry.path().join("cordis.patch.yml").is_file() {
                names.push(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    names.sort();
    if names.iter().any(|name| name == "desktop") {
        return Ok(profiles.join("desktop"));
    }
    match names.as_slice() {
        [only] => Ok(profiles.join(only)),
        [] => bail!(
            "no DSH profile with cordis.patch.yml under {}",
            profiles.display()
        ),
        _ => bail!("several DSH profiles ({names:?}); pass --profile"),
    }
}

/// Which agents to write into: `--agents` when given, else every one that is
/// installed here.
///
/// Explicit names are not filtered by detection: somebody who typed
/// `--agents claude` wants the entry written even if the directory was made by
/// hand, and the file edit is the same either way.
fn requested_agents(args: &[String]) -> Result<Vec<&'static str>> {
    if let Some(value) = flag(args, "--agents") {
        let mut picked = Vec::new();
        for name in value.split(',').map(str::trim).filter(|name| !name.is_empty()) {
            let Some((id, _)) = AGENTS.iter().find(|(id, _)| *id == name) else {
                bail!("unknown agent {name}; expected one of dsh, claude, codex");
            };
            if !picked.contains(id) {
                picked.push(*id);
            }
        }
        if picked.is_empty() {
            bail!("--agents names no agent");
        }
        return Ok(picked);
    }
    Ok(AGENTS
        .iter()
        .filter(|(id, _)| install::present(id))
        .map(|(id, _)| *id)
        .collect())
}

fn install(args: &[String]) -> Result<()> {
    let dry = args.iter().any(|arg| arg == "--dry-run");
    let explicit = flag(args, "--agents").is_some();
    let executable = std::env::current_exe().context("the agi-remote executable path")?;
    let agents = requested_agents(args)?;
    if agents.is_empty() {
        bail!(
            "no supported agent found on this machine (looked for DSH, Claude Code and Codex); \
             pass --agents dsh,claude,codex to write anyway"
        );
    }
    let mut done: Vec<&str> = Vec::new();
    for agent in &agents {
        match *agent {
            "dsh" => match profile_dir(args) {
                Ok(profile) => {
                    install_dsh(&profile, &executable, dry)?;
                    done.push("DSH");
                }
                // A detected-but-empty DSH is a machine that never finished
                // setting DSH up; refusing the whole install over it would take
                // Claude Code and Codex down with it.
                Err(error) if !explicit => println!("dsh: skipped — {error}"),
                Err(error) => return Err(error),
            },
            "claude" => {
                install::claude_install(&executable, Host::Claude.id(), dry)?;
                let dir = install::claude_home().join("skills").join(SKILL_NAME);
                install::write_skill(&dir, SKILL, dry)?;
                done.push("Claude Code");
            }
            "codex" => {
                install::codex_install(&executable, dry)?;
                let dir = install::codex_home().join("skills").join(SKILL_NAME);
                install::write_skill(&dir, SKILL, dry)?;
                done.push("Codex");
            }
            other => bail!("unknown agent {other}"),
        }
    }
    if dry {
        println!("dry run; nothing written");
        return Ok(());
    }
    println!(
        "installed for {}. Restart {} so the MCP client loads the new entry.",
        done.join(", "),
        done.join(" and ")
    );
    Ok(())
}

/// The DSH half: one `- insert:` block in the profile patch plus the skill.
fn install_dsh(profile: &std::path::Path, executable: &std::path::Path, dry: bool) -> Result<()> {
    let patch_path = profile.join("cordis.patch.yml");
    let before = std::fs::read_to_string(&patch_path)
        .with_context(|| format!("reading {}", patch_path.display()))?;
    let block = format!(
        "- insert:\n    - id: {MCP_ENTRY_ID}\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: agi_mizar_remote\n        transport: stdio\n        command: {}\n        args: ['mcp', '--host', 'dsh']\n        toolCallTimeoutMs: 60000\n",
        yaml_single_quoted(&executable.to_string_lossy())
    );
    let has_entry = before.contains(&format!("id: {MCP_ENTRY_ID}"));
    // A repair updates the sidecar path rather than accepting any old entry.
    let mut after = if has_entry {
        remove_entry(&before)
    } else {
        before.clone()
    };
    if !after.is_empty() && !after.ends_with('\n') {
        after.push('\n');
    }
    after.push_str(&block);
    println!("DSH profile: {}", profile.display());
    println!(
        "MCP entry: {}",
        if has_entry {
            "will replace"
        } else {
            "will append"
        }
    );
    if after != before {
        println!("--- {}", patch_path.display());
        println!("+++ after");
        for line in block.lines() {
            println!("+{line}");
        }
    }
    let skill_dir = dsh_home().join("skills").join(SKILL_NAME);
    if dry {
        println!("skill: {}", skill_dir.join("SKILL.md").display());
        println!("dry run; nothing written");
        return Ok(());
    }
    if after != before {
        let backup = patch_path.with_extension("yml.bak");
        std::fs::write(&backup, &before)
            .with_context(|| format!("writing {}", backup.display()))?;
        std::fs::write(&patch_path, &after)
            .with_context(|| format!("writing {}", patch_path.display()))?;
        println!("backup: {}", backup.display());
    }
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("creating {}", skill_dir.display()))?;
    std::fs::write(skill_dir.join("SKILL.md"), SKILL)
        .with_context(|| format!("writing {}", skill_dir.join("SKILL.md").display()))?;
    println!("skill: {}", skill_dir.join("SKILL.md").display());
    Ok(())
}

fn uninstall(args: &[String]) -> Result<()> {
    let dry = args.iter().any(|arg| arg == "--dry-run");
    let agents = requested_agents(args)?;
    let mut removed: Vec<&str> = Vec::new();
    for agent in &agents {
        match *agent {
            "dsh" => {
                if let Ok(profile) = profile_dir(args) {
                    uninstall_dsh(&profile, dry)?;
                    removed.push("DSH");
                }
            }
            "claude" => {
                install::claude_remove(dry)?;
                let dir = install::claude_home().join("skills").join(SKILL_NAME);
                install::remove_skill(&dir, dry)?;
                removed.push("Claude Code");
            }
            "codex" => {
                install::codex_remove(dry)?;
                let dir = install::codex_home().join("skills").join(SKILL_NAME);
                install::remove_skill(&dir, dry)?;
                removed.push("Codex");
            }
            other => bail!("unknown agent {other}"),
        }
    }
    if dry {
        println!("dry run; nothing removed");
        return Ok(());
    }
    if removed.is_empty() {
        println!("nothing to remove");
    } else {
        println!(
            "uninstalled from {}. Restart {} to drop the MCP client.",
            removed.join(", "),
            removed.join(" and ")
        );
    }
    Ok(())
}

/// The DSH half of an uninstall.
fn uninstall_dsh(profile: &std::path::Path, dry: bool) -> Result<()> {
    let patch_path = profile.join("cordis.patch.yml");
    let before = std::fs::read_to_string(&patch_path)
        .with_context(|| format!("reading {}", patch_path.display()))?;
    let after = remove_entry(&before);
    println!("DSH profile: {}", profile.display());
    if after != before {
        println!("--- {}", patch_path.display());
        println!("+++ after");
        for line in before.lines() {
            if !after.lines().any(|kept| kept == line) {
                println!("-{line}");
            }
        }
    } else {
        println!("MCP entry: not present");
    }
    let skill_dir = dsh_home().join("skills").join(SKILL_NAME);
    if dry {
        println!("skill: {}", skill_dir.display());
        return Ok(());
    }
    if after != before {
        let backup = patch_path.with_extension("yml.bak");
        std::fs::write(&backup, &before)?;
        std::fs::write(&patch_path, &after)?;
        println!("backup: {}", backup.display());
    }
    if skill_dir.join("SKILL.md").is_file() {
        std::fs::remove_dir_all(&skill_dir)
            .with_context(|| format!("removing {}", skill_dir.display()))?;
        println!("removed skill");
    }
    Ok(())
}

/// Remove the one `- insert:` block that carries our id, preserving the rest.
fn remove_entry(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(id_index) = lines
        .iter()
        .position(|line| line.contains(&format!("id: {MCP_ENTRY_ID}")))
    else {
        return text.to_string();
    };
    let mut start = id_index;
    while start > 0 {
        if lines[start - 1].trim_start().starts_with("- insert:") {
            start -= 1;
            break;
        }
        start -= 1;
    }
    let mut end = id_index + 1;
    while end < lines.len() {
        let trimmed = lines[end].trim_start();
        if trimmed.starts_with("- insert:")
            || (trimmed.starts_with("- ") && !lines[end].starts_with("    "))
        {
            break;
        }
        end += 1;
    }
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index >= start && index < end {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::yaml_single_quoted;

    /// The Windows bug this guards: a `C:\Users\...` path inside double quotes
    /// is an illegal YAML escape (`\U`, `\w`), and DSH refused the whole patch.
    #[test]
    fn a_windows_path_is_a_literal_yaml_scalar() {
        let quoted = yaml_single_quoted(r"C:\Users\me\AGI Mizar\agi-remote.exe");
        assert_eq!(quoted, r"'C:\Users\me\AGI Mizar\agi-remote.exe'");
        assert!(!quoted.contains('"'));
    }

    #[test]
    fn a_quote_in_the_path_is_doubled() {
        assert_eq!(yaml_single_quoted("a'b"), "'a''b'");
    }
}
