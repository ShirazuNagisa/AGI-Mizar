//! Live P1 smoke: the Rust client against the real account relay.
//!
//! Not product code and not part of the test suite: it reads the signed-in
//! `~/.agi-mizar/account.json`, mints a peer token through the account service,
//! and runs one small turn against an online desktop. It deletes nothing; the
//! session is archived when it succeeds.
//!
//! ```sh
//! cargo run -p agi-peer --example live
//! cargo run -p agi-peer --example live -- --prompt "Reply with exactly: P1-OK"
//! ```

use std::path::PathBuf;
use std::time::Duration;

use agi_peer::{Credentials, PeerClient};
use serde_json::{json, Value};
use tokio::sync::watch;

fn data_dir() -> PathBuf {
    std::env::var("MIZAR_DATA_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(agi_platform::data_dir)
}

fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let index = args.iter().position(|arg| arg == name)?;
    args.get(index + 1).cloned()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let file = data_dir().join("account.json");
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
    let token = stored
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{} has no account token", file.display()))?;
    let device = stored
        .get("device")
        .ok_or_else(|| anyhow::anyhow!("{} has no device identity", file.display()))?;
    let device_id = device
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{} has no device id", file.display()))?;
    let base = std::env::var("AGI_ACCOUNT_URL")
        .unwrap_or_else(|_| "https://dshmizar-1.shirazu-nagisa.com:8443".to_string());
    let http = reqwest::Client::new();
    let hash = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(token.as_bytes()))
    };
    let headers = reqwest::header::HeaderMap::from_iter([
        (
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))?,
        ),
        (
            reqwest::header::HeaderName::from_static("x-mizar-client-id"),
            reqwest::header::HeaderValue::from_str(device_id)?,
        ),
        (
            reqwest::header::HeaderName::from_static("x-mizar-account"),
            reqwest::header::HeaderValue::from_str(&hash)?,
        ),
    ]);

    let me: Value = http
        .get(format!("{base}/account/v1/me"))
        .headers(headers.clone())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let account_id = me["user"]["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("the account answered no user id"))?
        .to_string();
    let desktops: Value = http
        .get(format!("{base}/account/v1/desktops"))
        .headers(headers.clone())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let target = argument("--target")
        .or_else(|| {
            desktops["desktops"]
                .as_array()
                .and_then(|rows| rows.first())
                .and_then(|row| row["deviceId"].as_str())
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow::anyhow!("this account has no desktop to drive"))?;
    let connected: Value = http
        .post(format!("{base}/account/v1/connect"))
        .headers(headers)
        .json(&json!({ "deviceId": target, "engine": "dsh" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let room = connected["room"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("the account answered no room"))?;
    let peer_token = connected["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("the account answered no token"))?;

    println!("connecting to {target} in room {room}");
    let client = PeerClient::connect(Credentials {
        base,
        room: room.to_string(),
        account_session: token.to_string(),
        device_id: device_id.to_string(),
        account_id,
        peer_token: peer_token.to_string(),
        app_version: "agi-peer-live".to_string(),
    })
    .await?;
    println!("welcome from {}", client.welcome().host_name);
    let session = client.create_session(None).await?;
    println!("session {session}");
    let prompt = argument("--prompt")
        .unwrap_or_else(|| "Reply with exactly: P1-OK. Do not use any tools.".to_string());
    let (_cancel_tx, cancel) = watch::channel(false);
    let result = client
        .run_turn(&session, &prompt, Duration::from_secs(120), cancel)
        .await?;
    println!(
        "turn {} {}: {}",
        result.turn,
        result.reason,
        result.final_text.unwrap_or_else(|| "(no text)".to_string())
    );
    if argument("--keep-session").is_none() {
        let _ = client.archive(&session).await;
    }
    Ok(())
}
