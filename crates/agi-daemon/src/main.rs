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
//! Everything below `boot` is shared with the window; this file only decides what
//! to print and when to stop.

use std::path::PathBuf;

use agi_boot::{boot, BootConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "agi_core=info,agi_engine_dsh=info,agi_mizar=info",
                )
            }),
        )
        .init();

    // A positional argument is the endpoint file, so a dev run can sit beside a
    // packaged build without either stealing the other's plugin. Flags are not
    // paths.
    let endpoint_file = std::env::args()
        .skip(1)
        .find(|argument| !argument.starts_with('-'))
        .map(PathBuf::from);

    let install_hooks = std::env::args().any(|argument| argument == "--install-claude-hooks");

    let booted = boot(BootConfig {
        endpoint_file,
        install_claude_hooks: install_hooks,
        relay_url: std::env::var("AGI_RELAY_URL").ok(),
        relay_token: std::env::var("AGI_RELAY_TOKEN").ok(),
    })
    .await?;

    println!("AGI Mizar kernel {}", env!("CARGO_PKG_VERSION"));
    println!("  api    http://127.0.0.1:{}/api/v1/state", booted.api_port);
    println!(
        "  events ws://127.0.0.1:{}/api/v1/events?token={}",
        booted.api_port, booted.api_token
    );
    println!("  dsh    ws://127.0.0.1:{}/dsh", booted.api_port);
    println!("  file   {}", booted.endpoint_file.display());
    println!("  token  {}", booted.api_token);
    println!("  phone  ws://0.0.0.0:{}/mizar/v1/ws", booted.mizar.port());
    println!();
    println!("Pair a phone with this code (valid 5 minutes):");
    println!("  {}", booted.pairing_code.display);
    println!("  {}", booted.pairing_uri);
    println!();
    println!("Ctrl-C to stop.");

    tokio::signal::ctrl_c().await?;
    agi_core::endpoint::remove_endpoint_file(&booted.endpoint_file);
    Ok(())
}
