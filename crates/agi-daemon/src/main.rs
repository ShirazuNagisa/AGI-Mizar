//! The kernel, without a window.
//!
//! Three uses, all of them real:
//!
//! - **Development.** The React front end runs in a browser against this, which
//!   is what "the front end can be developed without Tauri" actually means.
//! - **Diagnosis.** When the packaged app misbehaves, the same kernel started
//!   from a terminal prints what it is doing.
//! - **A headless deployment.** Nothing here needs a display, so a machine that
//!   only hosts agents can run this and be controlled from elsewhere.
//!
//! It writes the same endpoint file the packaged app writes, so the DSH helper
//! plugin cannot tell the difference.

use std::path::PathBuf;
use std::sync::Arc;

use agi_core::{endpoint, server, Core};
use agi_engine::Engine;
use agi_engine_dsh::DshEngine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("agi_core=info,agi_engine_dsh=info")),
        )
        .init();

    // An explicit path lets a dev run sit beside a packaged build without either
    // one stealing the other's plugin.
    let endpoint_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(endpoint::default_endpoint_path);

    // Two tokens, not one. The plugin's token is written to a file any process
    // running as this user can read; the API token is only ever printed to this
    // terminal or handed to the window. Sharing one would make the file a way
    // into the UI.
    let plugin_token = random_token();
    let api_token = random_token();

    let dsh = DshEngine::new(plugin_token.clone());
    let engines: Vec<Arc<dyn Engine>> = vec![dsh.clone()];
    let core = Core::new(engines);
    core.start().await;

    let (port, _server) = server::bind(
        core,
        dsh,
        server::Config {
            api_token: api_token.clone(),
            plugin_token: plugin_token.clone(),
        },
    )
    .await?;

    // Published after the listener is up, so the plugin never dials a port that
    // is not answering yet.
    endpoint::write_endpoint_file(&endpoint_path, port, &plugin_token)?;

    println!("AGI Mizar kernel {}", env!("CARGO_PKG_VERSION"));
    println!("  api    http://127.0.0.1:{port}/api/v1/state");
    println!("  events ws://127.0.0.1:{port}/api/v1/events?token={api_token}");
    println!("  dsh    ws://127.0.0.1:{port}/dsh");
    println!("  file   {}", endpoint_path.display());
    println!("  token  {api_token}");
    println!();
    println!("Ctrl-C to stop.");

    tokio::signal::ctrl_c().await?;
    endpoint::remove_endpoint_file(&endpoint_path);
    Ok(())
}

/// 32 bytes of randomness, hex encoded, without a dependency for one call.
fn random_token() -> String {
    let mut token = String::with_capacity(64);
    for _ in 0..2 {
        token.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    token
}
