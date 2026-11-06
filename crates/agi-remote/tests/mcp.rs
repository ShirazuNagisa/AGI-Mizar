//! The MCP face over a fake local API: the host kernel is not involved.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

async fn command(
    State(seen): State<Seen>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let auth = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    seen.0.lock().await.push(format!(
        "{} {auth}",
        body["op"].as_str().unwrap_or_default()
    ));
    if auth != "Bearer tool-token" {
        return Json(json!({ "ok": false, "error": { "code": "UNAUTHORIZED", "message": "no" } }));
    }
    let value = match body["op"].as_str().unwrap_or_default() {
        "remote_peers" => json!({
            "thisDeviceId": "a",
            "peers": [{ "deviceId": "b", "name": "B", "online": true,
                        "remote": { "enabled": true, "workspaces": ["/work"] } }]
        }),
        "remote_delegate" => json!({
            "delegation": { "id": "d1", "state": "running", "peer": "b" }
        }),
        "remote_result" => json!({
            "delegation": { "id": "d1", "state": "done", "finalText": "DONE" },
            "finalText": "DONE"
        }),
        other => json!({ "error": format!("unknown {other}") }),
    };
    Json(json!({ "ok": true, "value": value }))
}

async fn fake_api() -> (u16, Seen, tokio::task::JoinHandle<()>) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/api/v1/command", post(command))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (port, seen, task)
}

async fn send(stdin: &mut ChildStdin, stdout: &mut BufReader<ChildStdout>, value: Value) -> Value {
    stdin
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), stdout.read_line(&mut line))
        .await
        .expect("MCP answered")
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

fn spawn(dir: &std::path::Path) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agi-remote"))
        .arg("mcp")
        .env("MIZAR_DATA_DIR", dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

#[tokio::test]
async fn the_mcp_face_lists_seven_tools_and_calls_the_kernel() {
    let (port, seen, server) = fake_api().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agent-tool.json"),
        json!({ "port": port, "token": "tool-token" }).to_string(),
    )
    .unwrap();
    let (mut child, mut stdin, mut stdout) = spawn(dir.path());

    let init = send(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2024-11-05" }
        }),
    )
    .await;
    assert_eq!(init["result"]["serverInfo"]["name"], "agi-mizar-remote");

    let list = send(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list"
        }),
    )
    .await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 7, "{names:?}");
    for expected in [
        "remote_agents",
        "remote_delegate",
        "remote_result",
        "remote_wait",
        "remote_cancel",
        "remote_answer",
        "remote_send",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }

    let agents = send(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "remote_agents", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(agents["result"]["isError"], false);
    assert!(agents["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("\"deviceId\": \"b\""));

    let delegate = send(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "remote_delegate",
                        "arguments": { "peer": "b", "workspace": "/work", "prompt": "say done" } }
        }),
    )
    .await;
    let text = delegate["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("AGI_MIZAR_DELEGATION d1"), "{text}");
    assert_eq!(
        *seen.0.lock().await,
        vec![
            "remote_peers Bearer tool-token",
            "remote_delegate Bearer tool-token",
        ]
    );

    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    server.abort();
}

#[tokio::test]
async fn dry_run_does_not_touch_the_dsh_profile() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join(".dsh/profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    let patch = profile.join("cordis.patch.yml");
    std::fs::write(&patch, "# user comments stay\n- id: keep-me\n").unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agi-remote"))
        .args(["install", "--profile", "desktop", "--dry-run"])
        .env("DSH_HOME", home.path().join(".dsh"))
        .env_remove("HOME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&patch).unwrap(),
        "# user comments stay\n- id: keep-me\n"
    );
    assert!(!home
        .path()
        .join(".dsh/skills/agi-mizar-remote/SKILL.md")
        .exists());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mcp-agi-mizar-remote"), "{stdout}");
    let _ = dir;
}

#[tokio::test]
async fn install_is_idempotent_and_uninstall_keeps_user_entries() {
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join(".dsh/profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    let patch = profile.join("cordis.patch.yml");
    let original = "# user comments stay\n- id: keep-me\n  name: '@example/keep'\n";
    std::fs::write(&patch, original).unwrap();

    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_agi-remote"))
            .args(args)
            .env("DSH_HOME", home.path().join(".dsh"))
            .env_remove("HOME")
            .output()
            .unwrap()
    };
    let first = run(&["install", "--profile", "desktop"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let installed = std::fs::read_to_string(&patch).unwrap();
    assert_eq!(installed.matches("id: mcp-agi-mizar-remote").count(), 1);
    assert!(installed.contains("id: keep-me"));
    assert!(home
        .path()
        .join(".dsh/skills/agi-mizar-remote/SKILL.md")
        .is_file());

    let again = run(&["install", "--profile", "desktop"]);
    assert!(again.status.success());
    let installed_again = std::fs::read_to_string(&patch).unwrap();
    assert_eq!(
        installed_again.matches("id: mcp-agi-mizar-remote").count(),
        1
    );

    let removed = run(&["uninstall", "--profile", "desktop"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let after = std::fs::read_to_string(&patch).unwrap();
    assert!(!after.contains("mcp-agi-mizar-remote"));
    assert!(after.contains("id: keep-me"));
    assert!(!home.path().join(".dsh/skills/agi-mizar-remote").exists());
}
