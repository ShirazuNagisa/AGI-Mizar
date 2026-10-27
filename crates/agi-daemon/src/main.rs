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
use agi_engine_claude::ClaudeEngine;
use agi_engine_codex::{AppServerConfig, CodexEngine};
use agi_engine_dsh::DshEngine;
use agi_mizar::{DeviceStore, MizarServer};

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
    // A third token, for the hooks. The shim runs on every tool call and is
    // invoked by Claude Code, not by the UI: a token that can answer an approval
    // has no business also being able to drive the window.
    let hook_token = random_token();

    let dsh = DshEngine::new(plugin_token.clone());
    let mut engines: Vec<Arc<dyn Engine>> = vec![dsh.clone()];

    // Codex is spawned, so it is only hosted when there is something to spawn.
    // An absent binary would otherwise mean a supervisor retrying a missing file
    // forever, and a permanent red status on a machine that simply has no Codex.
    let codex_binary = std::env::var("AGI_CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
    if let Some(path) = which(&codex_binary) {
        let codex = CodexEngine::new(AppServerConfig {
            binary: path.to_string_lossy().to_string(),
            args: Vec::new(),
            cwd: None,
        });
        let supervisor = Arc::clone(&codex);
        tokio::spawn(async move { supervisor.run().await });
        engines.push(codex);
        println!("codex  {}", path.display());
    } else {
        println!("codex  not found — set AGI_CODEX_BIN to point at one");
    }

    // Claude Code is always hosted: even with no `claude` executable there are
    // transcripts to read, and a session list is useful without the ability to
    // start a turn.
    let claude = ClaudeEngine::from_environment();
    let claude_binary = which(&std::env::var("AGI_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string()));
    match &claude_binary {
        Some(path) => println!("claude {}", path.display()),
        None => println!("claude not found — transcripts are readable, prompts are not"),
    }
    engines.push(claude.clone());

    let core = Core::new(engines);
    core.start().await;

    let dsh_home = std::env::var("DSH_HOME")
        .unwrap_or_else(|_| format!("{}/.dsh", std::env::var("HOME").unwrap_or_default()));
    let (port, _server) = server::bind(
        core.clone(),
        dsh,
        Some(claude),
        server::Config {
            api_token: api_token.clone(),
            plugin_token: plugin_token.clone(),
            hook_token: hook_token.clone(),
        },
    )
    .await?;

    // Published after the listener is up, so the plugin never dials a port that
    // is not answering yet.
    endpoint::write_endpoint_file(&endpoint_path, port, &plugin_token)?;
    endpoint::write_hook_file(&endpoint::default_hook_path(), port, &hook_token)?;

    // Opt-in: editing `~/.claude/settings.json` is the user's file, and a daemon
    // started for a quick look should not touch it.
    if std::env::args().any(|argument| argument == "--install-claude-hooks") {
        match shim_path() {
            Some(shim) => {
                match agi_engine_claude::install(&agi_engine_claude::paths::settings_path(), &shim) {
                    Ok(outcome) => println!(
                        "hooks  {} added, {} already present (backup: {})",
                        outcome.added,
                        outcome.present,
                        outcome
                            .backup
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "none".to_string())
                    ),
                    Err(error) => println!("hooks  could not be installed — {error}"),
                }
            }
            None => println!("hooks  agi-mizar-hook not found next to this binary"),
        }
    }

    // The phone's server. A separate listener from the local API: this one binds
    // every interface, because a phone on the same network has to reach it, and
    // the local API deliberately does not.
    let roster = DeviceStore::open(agi_mizar::store::default_store_path(), agi_mizar::store::host_name())?;
    let mizar = MizarServer::new(core.clone(), roster, dsh_home.clone());
    let mizar_handle = match mizar.bind(agi_mizar::frames::DEFAULT_PORT).await {
        Ok(handle) => handle,
        Err(error) => {
            println!("mizar port {} unavailable ({error}) — using a free one", agi_mizar::frames::DEFAULT_PORT);
            mizar.bind(0).await?
        }
    };
    let _ = mizar_handle;
    let code = mizar.issue_code();

    println!("AGI Mizar kernel {}", env!("CARGO_PKG_VERSION"));
    println!("  api    http://127.0.0.1:{port}/api/v1/state");
    println!("  events ws://127.0.0.1:{port}/api/v1/events?token={api_token}");
    println!("  dsh    ws://127.0.0.1:{port}/dsh");
    println!("  file   {}", endpoint_path.display());
    println!("  token  {api_token}");
    println!("  phone  ws://0.0.0.0:{}/mizar/v1/ws", mizar.port());
    println!();
    println!("Pair a phone with this code (valid 5 minutes):");
    println!("  {}", code.display);
    println!("  {}", mizar.pairing_uri(&code));
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

/// The hook shim, which is installed beside this binary.
fn shim_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let directory = exe.parent()?;
    let name = if cfg!(windows) {
        "agi-mizar-hook.exe"
    } else {
        "agi-mizar-hook"
    };
    let candidate = directory.join(name);
    candidate.is_file().then_some(candidate)
}

/// Find an executable the way a shell would: an absolute path, or `PATH`.
fn which(binary: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(binary);
    if candidate.is_absolute() {
        return candidate.exists().then_some(candidate);
    }
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let full = directory.join(binary);
        if full.is_file() {
            return Some(full);
        }
        #[cfg(windows)]
        {
            let exe = directory.join(format!("{binary}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}
