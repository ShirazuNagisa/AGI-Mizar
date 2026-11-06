//! The tool token is narrower than the UI token, and the local API enforces it.

use agi_core::server::{bind, Config};
use agi_core::Core;
use agi_engine_dsh::DshEngine;
use serde_json::{json, Value};

#[tokio::test]
async fn the_tool_token_may_only_call_remote_commands() {
    let dsh = DshEngine::new("plugin-token");
    let core = Core::new(vec![dsh.clone() as std::sync::Arc<dyn agi_engine::Engine>]);
    let (port, server) = bind(
        core,
        dsh,
        None,
        Config {
            api_token: "ui-token".to_string(),
            plugin_token: "plugin-token".to_string(),
            hook_token: "hook-token".to_string(),
            tool_token: "tool-token".to_string(),
        },
    )
    .await
    .expect("the loopback API binds");

    let client = reqwest::Client::new();
    let call = |token: &'static str, body: Value| {
        let client = client.clone();
        async move {
            client
                .post(format!("http://127.0.0.1:{port}/api/v1/command"))
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .expect("the loopback API answers")
        }
    };

    let allowed = call("tool-token", json!({ "op": "remote_delegations" })).await;
    assert_eq!(allowed.status(), reqwest::StatusCode::OK);
    let body: Value = allowed.json().await.unwrap();
    assert_eq!(body["ok"], true);

    let denied = call("tool-token", json!({ "op": "state" })).await;
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    let body: Value = denied.json().await.unwrap();
    assert_eq!(body["error"]["code"], "FORBIDDEN");

    let ui = call("ui-token", json!({ "op": "state" })).await;
    assert_eq!(ui.status(), reqwest::StatusCode::OK);

    let missing = call("wrong-token", json!({ "op": "remote_delegations" })).await;
    assert_eq!(missing.status(), reqwest::StatusCode::UNAUTHORIZED);

    server.abort();
}
