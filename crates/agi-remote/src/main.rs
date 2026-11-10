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

const SKILL: &str = include_str!("../skills/agi-mizar-remote/SKILL.md");
const MCP_ENTRY_ID: &str = "mcp-agi-mizar-remote";
const SKILL_NAME: &str = "agi-mizar-remote";

fn main() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("a Tokio runtime")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");
    match command {
        "mcp" => mcp(&runtime),
        "agents" => runtime.block_on(cli_agents()),
        "options" => runtime.block_on(cli_options(&args[1..])),
        "delegate" => runtime.block_on(cli_delegate(&args[1..])),
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

fn usage() {
    println!(
        "agi-remote — drive a same-account AGI Mizar computer\n\
\n\
  agi-remote agents\n\
  agi-remote options --peer <id>\n\
  agi-remote delegate --peer <id> [--workspace <path>] [--permission <preset>] \\\n\
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
Install writes the DSH MCP entry and the agi-mizar-remote skill; restart DSH\n\
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

fn mcp(runtime: &tokio::runtime::Runtime) -> Result<()> {
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
                match runtime.block_on(call_tool(name, &arguments)) {
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
            "description": "Start a self-contained task on a same-account peer. Returns immediately with a delegation id whose marker is AGI_MIZAR_DELEGATION <id>. The remote agent cannot see this conversation.",
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
            "description": "Wait up to timeoutMs for a delegation to settle. Fallback for hosts without the notice bridge.",
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
            "description": "Send a follow-up to a delegation. P4.",
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

async fn call_tool(name: &str, arguments: &Value) -> Result<String> {
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
                .ok_or_else(|| anyhow!("the kernel returned no delegation id"))?;
            Ok(format!(
                "AGI_MIZAR_DELEGATION {id}\n{}",
                serde_json::to_string_pretty(&value)?
            ))
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
        other => bail!("unknown tool {other}"),
    }
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

fn install(args: &[String]) -> Result<()> {
    let dry = args.iter().any(|arg| arg == "--dry-run");
    let profile = profile_dir(args)?;
    let patch_path = profile.join("cordis.patch.yml");
    let before = std::fs::read_to_string(&patch_path)
        .with_context(|| format!("reading {}", patch_path.display()))?;
    let executable = std::env::current_exe().context("the agi-remote executable path")?;
    let block = format!(
        "- insert:\n    - id: {MCP_ENTRY_ID}\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: agi_mizar_remote\n        transport: stdio\n        command: {}\n        args: ['mcp']\n        toolCallTimeoutMs: 60000\n",
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
    let skill_path = skill_dir.join("SKILL.md");
    println!("skill: {}", skill_path.display());
    if dry {
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
    std::fs::write(&skill_path, SKILL)
        .with_context(|| format!("writing {}", skill_path.display()))?;
    println!("installed. Restart DSH so the mcp client loads the new entry.");
    Ok(())
}

fn uninstall(args: &[String]) -> Result<()> {
    let dry = args.iter().any(|arg| arg == "--dry-run");
    let profile = profile_dir(args)?;
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
    println!("skill: {}", skill_dir.display());
    if dry {
        println!("dry run; nothing removed");
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
    println!("uninstalled. Restart DSH to drop the mcp client.");
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
