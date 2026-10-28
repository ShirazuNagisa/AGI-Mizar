//! Starting the kernel, once, for whoever needs it.
//!
//! Two things start this application: the daemon, from a terminal, and the
//! window, from a bundle. They must come up identically — the same engines, the
//! same three tokens, the same endpoint files, the same phone server — or a bug
//! will reproduce in one and not the other and cost an afternoon.
//!
//! So the sequence lives here and both callers are thin. What differs between
//! them is only what they do afterwards: print, or open a window.
//!
//! ### The order matters
//!
//! 1. Engines are built and started, so the core has something to aggregate.
//! 2. The local API binds — and only then is the endpoint file written, because
//!    a plugin that dials a port nobody is listening on retries with backoff and
//!    looks broken.
//! 3. The phone server binds on its own port, every interface, because a phone on
//!    the same network has to reach it.
//! 4. The tunnel dials last: it replays onto the phone server, so that server has
//!    to exist first.

use std::path::PathBuf;
use std::sync::Arc;

use agi_core::{endpoint, server, Core};
use agi_engine::Engine;
use agi_engine_claude::ClaudeEngine;
use agi_engine_codex::{AppServerConfig, CodexEngine};
use agi_engine_dsh::DshEngine;
use agi_mizar::pairing::PairingCode;
use agi_mizar::{DeviceStore, MizarServer, RelayClient};

/// What the caller wants.
#[derive(Debug, Clone, Default)]
pub struct BootConfig {
    /// Where to publish the plugin's endpoint. `None` uses the default path.
    pub endpoint_file: Option<PathBuf>,
    /// Edit `~/.claude/settings.json` to install the hooks.
    ///
    /// Off by default: that file belongs to the operator, and a process started
    /// to look at something should not change it.
    pub install_claude_hooks: bool,
    /// Dial this relay. `None` leaves the LAN-only path.
    pub relay_url: Option<String>,
    /// The tunnel secret. `None` invents one, which means a new room each launch.
    pub relay_token: Option<String>,
}

/// A running kernel.
pub struct Booted {
    pub core: Arc<Core>,
    pub mizar: Arc<MizarServer>,
    /// The local API's port and token — what a window needs to draw itself.
    pub api_port: u16,
    pub api_token: String,
    /// A code the operator can show a phone right now.
    pub pairing_code: PairingCode,
    pub pairing_uri: String,
    pub endpoint_file: PathBuf,
}

impl Booted {
    /// The URL a window should load.
    ///
    /// The theme rides along so the page paints itself correctly on the first
    /// frame instead of flashing the wrong background while it asks the kernel.
    pub fn window_url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/?token={}&theme={}",
            self.api_port,
            self.api_token,
            self.core.theme()
        )
    }
}

/// Start everything.
pub async fn boot(config: BootConfig) -> anyhow::Result<Booted> {
    // One subscriber for the process. Without it every `tracing` call below is
    // compiled to nothing, which is how "the tunnel never came up" stayed
    // invisible: the relay had said why on a channel nobody was listening to.
    // The default level stays quiet; `RUST_LOG=info` is the switch for watching
    // a link come up.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let endpoint_file = config
        .endpoint_file
        .clone()
        .unwrap_or_else(endpoint::default_endpoint_path);

    // Three tokens, because three callers with different reach hold them.
    let plugin_token = random_token();
    let api_token = random_token();
    let hook_token = random_token();

    let dsh = DshEngine::new(plugin_token.clone());
    let mut engines: Vec<Arc<dyn Engine>> = vec![dsh.clone()];

    // Codex is spawned, so it is only hosted when there is something to spawn: an
    // absent binary would otherwise mean a supervisor retrying a missing file
    // forever and a permanently red status on a machine that has no Codex.
    let codex_binary = std::env::var("AGI_CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
    match which(&codex_binary) {
        Some(path) => {
            let codex = CodexEngine::new(AppServerConfig {
                binary: path.to_string_lossy().to_string(),
                args: Vec::new(),
                cwd: None,
            });
            let supervisor = Arc::clone(&codex);
            tokio::spawn(async move { supervisor.run().await });
            engines.push(codex);
            tracing::info!("codex at {}", path.display());
        }
        None => tracing::info!("codex not found — set AGI_CODEX_BIN to point at one"),
    }

    // Claude Code is always hosted: with no `claude` executable there are still
    // transcripts to read, and a list is useful without the ability to prompt.
    let claude = ClaudeEngine::from_environment();
    match which(&std::env::var("AGI_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())) {
        Some(path) => tracing::info!("claude at {}", path.display()),
        None => tracing::info!("claude not found — transcripts readable, prompts are not"),
    }
    engines.push(claude.clone());

    let core = Core::new(engines);
    // The window's own preferences live beside the endpoint file, not in the
    // browser: the window's origin changes with the port on every launch.
    if let Some(home) = endpoint_file.parent() {
        core.load_preferences(home.join("preferences.json"));
        core.load_account(home.join("account.json"));
        dsh.set_usage_cache_path(home.join("usage-dsh.json"));
    }
    core.start().await;
    core.refresh_account_in_background();

    let dsh_home = std::env::var("DSH_HOME")
        .unwrap_or_else(|_| format!("{}/.dsh", std::env::var("HOME").unwrap_or_default()));

    let (api_port, _api) = server::bind(
        Arc::clone(&core),
        dsh,
        Some(Arc::clone(&claude)),
        server::Config {
            api_token: api_token.clone(),
            plugin_token: plugin_token.clone(),
            hook_token: hook_token.clone(),
        },
    )
    .await?;

    // Published only after the listener is up: a plugin that dials a port nobody
    // answers retries with backoff and looks broken.
    endpoint::write_endpoint_file(&endpoint_file, api_port, &plugin_token)?;
    endpoint::write_hook_file(&endpoint::default_hook_path(), api_port, &hook_token)?;

    if config.install_claude_hooks {
        install_hooks();
    }

    // The phone's server: every interface, because a phone on the same network
    // has to reach it, unlike the local API.
    let roster = DeviceStore::open(
        agi_mizar::store::default_store_path(),
        agi_mizar::store::host_name(),
    )?;
    let mizar = MizarServer::new(Arc::clone(&core), roster, dsh_home);
    match mizar.bind(agi_mizar::frames::DEFAULT_PORT).await {
        Ok(_handle) => {}
        Err(error) => {
            tracing::warn!(
                "mizar port {} unavailable ({error}) — using a free one",
                agi_mizar::frames::DEFAULT_PORT
            );
            mizar.bind(0).await?;
        }
    }

    if let Some(url) = config
        .relay_url
        .as_ref()
        .filter(|url| !url.trim().is_empty())
    {
        let token = config.relay_token.unwrap_or_else(random_token);
        let relay = RelayClient::new(url, token, mizar.port());
        mizar.attach_relay(Arc::clone(&relay));
        tokio::spawn(async move { relay.run().await });
    }

    // The window needs a code to show; the kernel asks whoever is composed.
    core.attach_pairing(Arc::clone(&mizar) as Arc<dyn agi_core::PairingSource>);

    let pairing_code = mizar.issue_code();
    let pairing_uri = mizar.pairing_uri(&pairing_code);

    Ok(Booted {
        core,
        mizar,
        api_port,
        api_token,
        pairing_code,
        pairing_uri,
        endpoint_file,
    })
}

/// Install the Claude Code hooks, reporting the outcome rather than failing.
fn install_hooks() {
    let Some(shim) = shim_path() else {
        tracing::warn!("hooks: agi-mizar-hook not found next to this binary");
        return;
    };
    match agi_engine_claude::install(&agi_engine_claude::paths::settings_path(), &shim) {
        Ok(outcome) => tracing::info!(
            "hooks: {} added, {} already present",
            outcome.added,
            outcome.present
        ),
        Err(error) => tracing::warn!("hooks: could not be installed — {error}"),
    }
}

/// The hook shim, installed beside this binary.
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

/// 32 bytes of randomness, hex encoded.
pub fn random_token() -> String {
    let mut token = String::with_capacity(64);
    for _ in 0..2 {
        token.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    token
}

/// Find an executable the way a shell would: an absolute path, or `PATH`.
pub fn which(binary: &str) -> Option<PathBuf> {
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
