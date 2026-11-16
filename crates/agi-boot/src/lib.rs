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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agi_core::{endpoint, server, Core, ServerPrefs};
use agi_engine::Engine;
use agi_engine_claude::ClaudeEngine;
use agi_engine_codex::{AppServerConfig, CodexEngine};
use agi_engine_dsh::DshEngine;
use agi_mizar::pairing::PairingCode;
use agi_mizar::{DeviceStore, MizarServer, RelayClient};
use agi_protocol::engine::EngineId;

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
    /// The relay the caller would dial in official mode. `None` leaves the
    /// LAN-only path (the window passes the official default; the daemon leaves
    /// it to `AGI_RELAY_URL`).
    ///
    /// A stored self-hosted choice replaces this; see [`effective_relay_url`].
    pub relay_url: Option<String>,
    /// The tunnel secret. `None` invents one, which means a new room each launch.
    pub relay_token: Option<String>,
    /// The server choice the caller read before boot.
    ///
    /// The window reads `preferences.json` first so the tunnel decision is not
    /// made before the file exists in the kernel. Boot adopts this value (and
    /// writes it back only when it differs), so the snapshot and the file the
    /// next launch reads cannot disagree.
    pub server: Option<ServerPrefs>,
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

    // Three tokens, because three callers with different reach hold them. The
    // API token is the one a Linux service pins: a stable value is what lets a
    // bookmark exchange it for a cookie instead of changing every launch.
    let plugin_token = random_token();
    let api_token = configured_api_token().unwrap_or_else(random_token);
    let hook_token = random_token();
    let tool_token = random_token();

    // Where the browser-facing API listens. The packaged window keeps the
    // historical loopback/ephemeral default; a headless install names both so
    // the launcher and its systemd unit can agree on one address.
    let api_host = std::env::var("AGI_API_HOST")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let api_port = match std::env::var("AGI_API_PORT") {
        Ok(value) => match value.trim().parse::<u16>() {
            Ok(port) => port,
            Err(_) => {
                tracing::warn!("AGI_API_PORT is not a port number; asking the OS for one");
                0
            }
        },
        Err(_) => 0,
    };

    let dsh = DshEngine::new(plugin_token.clone());
    let mut engines: Vec<Arc<dyn Engine>> = vec![dsh.clone()];

    // Claude is built once and kept: the HTTP server hands it the hook events,
    // so it needs the concrete engine, not the trait object.
    let claude = build_claude();
    engines.push(Arc::clone(&claude) as Arc<dyn Engine>);

    // Everything else that is a binary on this machine. DSH is not here: it
    // dials in, and Claude is already above.
    let (hosted, missing) = discover_local(&[EngineId::Dsh, EngineId::Claude]);
    engines.extend(hosted);

    let core = Core::new(engines);
    {
        let dsh_for_notice = Arc::clone(&dsh);
        core.set_remote_notice_sink(Arc::new(move |record, notice_kind| {
            let dsh = Arc::clone(&dsh_for_notice);
            let id = record.id.clone();
            let kind = if notice_kind == "approval" {
                "approval".to_string()
            } else {
                format!("{:?}", record.state).to_lowercase()
            };
            let text = if notice_kind == "approval" {
                let approval = record.approval.clone();
                match approval {
                    Some(approval) => format!(
                        "Remote approval needed on {} (risk: {}): {}\n\
                         Delegation {}. Answer with remote_answer only if the risk is low; \
                         otherwise leave it to the remote operator.",
                        record.peer_name, approval.risk, approval.reason, record.id
                    ),
                    None => "A remote approval was withdrawn.".to_string(),
                }
            } else {
                // The same notice covers both routes; the wording follows the
                // record so a local subagent's failure is not reported as a
                // failure on another computer.
                let subject = if record.route == agi_core::remote::ROUTE_LOCAL {
                    "the local subagent"
                } else {
                    "the remote turn"
                };
                match record.state {
                    agi_core::remote::RemoteState::Done => record
                        .final_text
                        .clone()
                        .unwrap_or_else(|| format!("{subject} finished without text")),
                    agi_core::remote::RemoteState::Failed => record
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("{subject} failed")),
                    agi_core::remote::RemoteState::Cancelled => {
                        format!("{subject} was cancelled")
                    }
                    agi_core::remote::RemoteState::Interrupted => {
                        format!("{subject} was interrupted")
                    }
                    agi_core::remote::RemoteState::Running => return,
                }
            };
            tokio::spawn(async move {
                dsh.notify_delegation(&id, &kind, &text).await;
            });
        }));
    }
    core.set_unavailable(missing.clone().into_iter().collect());
    core.set_rescan(Arc::new(discover_local));
    core.set_claude_hooks(Arc::new(install_hooks));
    // The window's own preferences live beside the endpoint file, not in the
    // browser: the window's origin changes with the port on every launch.
    if let Some(home) = endpoint_file.parent() {
        core.load_preferences(home.join("preferences.json"));
        core.load_account(home.join("account.json"));
        core.load_lan_certificate(home.join("lan-certificate.json"));
        core.load_remote(agi_core::remote::default_path());
        core.load_peers(agi_core::peer::default_path());
        core.set_remote_audit_path(agi_core::remote::default_audit_path());
        dsh.set_usage_cache_path(home.join("usage-dsh.json"));

        // An install that has already signed in has been configured by hand, in
        // public builds that predate the guide. Do not lock those people behind
        // a flow they never saw; a genuinely new machine has neither file.
        //
        // Only "never finished any guide" is grandfathered. A file whose
        // `onboarding.done` was written by an older guide has to replay: the
        // server step and the self-hosted flow arrived after it, and the stored
        // settings are carried into the replay.
        if home.join("account.json").exists() && !core.onboarding_finished() {
            core.set_onboarding_done(true);
            tracing::info!("onboarding: existing install, marked done");
        }
    }

    // The caller may have read the preference file before boot (the window does,
    // so the tunnel target is decided before the kernel exists). Adopt it here so
    // the in-memory choice, the snapshot and the file all agree.
    if let Some(server) = config.server.clone() {
        if let Err(error) = core.set_server_prefs(server) {
            tracing::warn!("preferences: ignoring the caller's server choice — {error}");
        }
    }
    core.start().await;

    // Claude has no socket that pushes a desktop Code tab's new transcript. A
    // ten-second disk refresh is the fallback: it reads files that are already
    // the source of truth and emits only rows whose title/state changed.
    let claude_refresh = Arc::clone(&claude);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            claude_refresh.refresh_from_disk();
        }
    });

    core.refresh_account_in_background();
    // The DSH plugin lives in DSH's own profile, so the kernel cannot discover
    // it the way it discovers a binary. This asks npm once at launch and every
    // six hours after; the window turns an available update into a popup.
    core.start_dsh_plugin_watch();

    // The DSH home, decided the way the DSH CLI itself decides it: the CLI's own
    // override first, then a `.dsh` under the user's home. `home()` is the
    // platform seam — a Windows process started from Explorer has no `HOME`, and
    // the old fallback put the path at the filesystem root.
    let dsh_home = std::env::var("DSH_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            agi_platform::home()
                .join(".dsh")
                .to_string_lossy()
                .to_string()
        });

    let (api_port, _api) = server::bind(
        Arc::clone(&core),
        dsh,
        Some(Arc::clone(&claude)),
        server::Config {
            api_token: api_token.clone(),
            plugin_token: plugin_token.clone(),
            hook_token: hook_token.clone(),
            tool_token: tool_token.clone(),
            api_host,
            api_port,
        },
    )
    .await?;

    // Published only after the listener is up: a plugin that dials a port nobody
    // answers retries with backoff and looks broken.
    endpoint::write_endpoint_file(&endpoint_file, api_port, &plugin_token)?;
    endpoint::write_hook_file(&endpoint::default_hook_path(), api_port, &hook_token)?;
    endpoint::write_tool_file(&endpoint::default_tool_path(), api_port, &tool_token)?;

    if config.install_claude_hooks {
        if let Err(error) = install_hooks() {
            tracing::warn!("hooks: {error}");
        }
    }

    // The phone's server: every interface, because a phone on the same network
    // has to reach it, unlike the local API.
    let roster = DeviceStore::open(
        agi_mizar::store::default_store_path(),
        agi_mizar::store::host_name(),
    )?;
    // The roster's name is written once, at first launch. A computer that was
    // renamed since then should hand the phone the name it has now: the device
    // list and the pairing reply both read this field.
    if let Err(error) = roster.set_host_name(&agi_platform::host_name()) {
        tracing::warn!("roster: could not refresh the host name — {error}");
    }
    // The tunnel mints phone tokens from the same roster the phone server
    // authenticates against.
    let roster_for_relay = Arc::clone(&roster);
    let mizar = MizarServer::new(Arc::clone(&core), roster, dsh_home);
    mizar.load_peer_sessions(agi_platform::data_dir().join("peer-sessions.json"));

    // The LAN listener starts only when the operator enabled LAN and the
    // account issued a certificate. A failed bind is retried; a disabled host
    // never opens the port at all.
    {
        let core_for_lan = Arc::clone(&core);
        let mizar_for_lan = Arc::clone(&mizar);
        tokio::spawn(async move {
            loop {
                let prefs = core_for_lan.remote_prefs();
                if !prefs.enabled || !prefs.allow_lan {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    continue;
                }
                let credentials = match core_for_lan.lan_credentials().await {
                    Ok(credentials) => credentials,
                    Err(error) => {
                        tracing::debug!("lan: credentials not ready — {error}");
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        continue;
                    }
                };
                let lan_account = credentials.trust.account_id.clone();
                let core_for_discovery = Arc::clone(&core_for_lan);
                let mdns_beacon = agi_lan::LanBeacon::new(
                    &credentials.trust.account_id,
                    credentials.certificate.body.device_id.clone(),
                    credentials.certificate.body.device_id.clone(),
                    agi_lan::LAN_PORT,
                    &credentials.certificate,
                );
                let lan_account_tag = agi_lan::account_tag(&credentials.trust.account_id);
                let beacon = agi_lan::LanBeacon::new(
                    &credentials.trust.account_id,
                    credentials.certificate.body.device_id.clone(),
                    credentials.certificate.body.device_id.clone(),
                    agi_lan::LAN_PORT,
                    &credentials.certificate,
                );
                let listener =
                    match agi_lan::LanListener::bind(agi_lan::LAN_PORT, credentials).await {
                        Ok(listener) => listener,
                        Err(error) => {
                            tracing::warn!("lan: cannot bind {} — {error}", agi_lan::LAN_PORT);
                            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                            continue;
                        }
                    };
                tracing::info!("lan: listening on {}", agi_lan::LAN_PORT);
                tokio::spawn(async move {
                    let _advertiser = agi_lan::MdnsAdvertiser::start(
                        &mdns_beacon,
                        &format!("{}.local.", agi_platform::host_name()),
                    )
                    .map_err(|error| tracing::warn!("lan: mDNS advertise failed — {error}"))
                    .ok();
                    let browser = agi_lan::MdnsBrowser::start(&lan_account_tag)
                        .map_err(|error| tracing::warn!("lan: mDNS browse failed — {error}"))
                        .ok();
                    if let Some(browser) = browser {
                        let core = Arc::clone(&core_for_discovery);
                        tokio::task::spawn_blocking(move || {
                            for (beacon, ip) in browser.receiver().iter() {
                                core.record_lan_beacon_ip(&beacon, ip);
                            }
                        });
                    }
                    let discovery = match agi_lan::LanDiscovery::bind(agi_lan::DISCOVERY_PORT).await
                    {
                        Ok(discovery) => discovery,
                        Err(error) => {
                            tracing::warn!("lan: discovery bind failed — {error}");
                            return;
                        }
                    };
                    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
                    loop {
                        tokio::select! {
                            _ = ticker.tick() => {
                                if let Err(error) = discovery.announce(&beacon).await {
                                    tracing::debug!("lan: announce failed — {error}");
                                }
                            }
                            found = discovery.recv_for_account(&lan_account) => {
                                match found {
                                    Ok((beacon, from)) => {
                                        core_for_discovery.record_lan_beacon(&beacon, from);
                                    }
                                    Err(error) => {
                                        tracing::debug!("lan: receive failed — {error}");
                                    }
                                }
                            }
                        }
                    }
                });
                loop {
                    match listener.accept().await {
                        Ok(session) => {
                            let remote = session.remote().clone();
                            let mizar = Arc::clone(&mizar_for_lan);
                            tokio::spawn(async move {
                                if let Err(error) = mizar.serve_lan(remote, session).await {
                                    tracing::debug!("lan: session ended — {error}");
                                }
                            });
                        }
                        Err(error) => {
                            tracing::warn!("lan: accept failed — {error}");
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    }
                }
            }
        });
    }
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

    // Which relay this install dials. The decision belongs here, after the
    // preference file has been read: a stored self-hosted origin replaces the
    // caller's default, and `AGI_RELAY_URL` is the escape hatch for a local
    // relay in tests and development and wins over both.
    let requested_relay = config.relay_url.clone();
    let explicit_relay = std::env::var("AGI_RELAY_URL").ok();
    let effective_relay = effective_relay_url(
        &core.server_prefs(),
        explicit_relay.as_deref(),
        requested_relay.as_deref(),
    );
    let token = config.relay_token.unwrap_or_else(random_token);
    let tunnels = Tunnels::new(
        token.clone(),
        mizar.port(),
        Arc::clone(&roster_for_relay),
        Arc::clone(&core),
        Arc::clone(&mizar),
    );

    // The window's status lamp reads this even when no tunnel is configured: the
    // answer is then "not connected", which is the truth.
    core.attach_relay_status(Arc::clone(&mizar) as Arc<dyn agi_core::RelayStatusSource>);
    // The phone server refuses a phone signed in to another account, and the
    // answer has to be current rather than taken at launch.
    mizar.set_account_user(
        core.account_user()
            .map(|user| user.id)
            .filter(|id| !id.is_empty()),
    );
    match effective_relay.as_deref() {
        Some(url) => tunnels.move_to(url),
        None => tracing::info!("relay: no tunnel configured"),
    }

    // Signing in or out changes what the tunnel presents and which presence
    // tunnels may exist: the account instance refuses anonymous tunnels.
    {
        let mut changes = core.subscribe_account();
        let mizar_for_changes = Arc::clone(&mizar);
        let roster_for_changes = Arc::clone(&roster_for_relay);
        let tunnels = Arc::clone(&tunnels);
        tokio::spawn(async move {
            while let Ok(event) = changes.recv().await {
                mizar_for_changes
                    .set_account_user(event.user_id.clone().filter(|id| !id.is_empty()));
                if !event.signed_in {
                    if let Some(user) = event.user_id.as_deref() {
                        agi_mizar::relay::revoke_account_tokens(&roster_for_changes, user);
                    }
                }
                // A redial carries the new (or no) account headers; the presence
                // tunnel follows the session the same way.
                if let Some(relay) = tunnels.main_relay() {
                    relay.recycle();
                }
                tunnels.sync_legacy();
            }
        });
    }

    // The server page replaces the tunnel without a restart. The old client is
    // stopped and a new one dials the chosen origin; the account source and the
    // peer policy are attached by `Tunnels`, so nothing else has to be rebuilt.
    {
        let mut changes = core.subscribe_server_prefs();
        let core_for_changes = Arc::clone(&core);
        let tunnels = Arc::clone(&tunnels);
        tokio::spawn(async move {
            while changes.recv().await.is_ok() {
                let next = effective_relay_url(
                    &core_for_changes.server_prefs(),
                    explicit_relay.as_deref(),
                    requested_relay.as_deref(),
                );
                match next {
                    Some(url) if tunnels.current_origin().as_deref() != Some(url.as_str()) => {
                        tracing::info!("relay: moving the tunnel to {url}");
                        tunnels.move_to(&url);
                    }
                    Some(_) => {}
                    None => {
                        tracing::warn!(
                            "relay: the stored server has no usable origin; the tunnel is stopped"
                        );
                        tunnels.stop();
                    }
                }
                tunnels.mizar.set_account_user(
                    core_for_changes
                        .account_user()
                        .map(|user| user.id)
                        .filter(|id| !id.is_empty()),
                );
            }
        });
    }

    if let Some(url) = effective_relay.as_deref() {
        // The direct path. Its socket belongs to the process, so it is bound
        // here rather than per connection; the accept loop hands each punched
        // stream to the same method dispatch a WebSocket uses.
        //
        // Bound once, on the origin this launch dialled: a runtime server switch
        // moves the tunnel but not this socket, so the candidate refresh keeps
        // asking the launch-time STUN host until the next start. That is the
        // same rule the tunnel's own room follows across a move.
        let identity = agi_mizar::net::identity::Identity::derive(
            token.as_bytes(),
            agi_mizar::net::identity::Purpose::Host,
        );
        // The reflexive candidate comes from the relay's own STUN server: the
        // same host the tunnel already trusts, so no new party learns where this
        // computer connects from.
        let stun_servers = stun_servers_for(url);
        let reach = agi_mizar::net::engine::Reach::default();
        match agi_mizar::net::engine::Engine::bind(identity, 0, reach.clone(), stun_servers).await {
            Ok((mut engine, address)) => {
                let endpoint_id = engine.endpoint_id().to_string();
                tracing::info!("p2p: direct path on {address} ({endpoint_id})");
                mizar.attach_direct_path(endpoint_id, reach, engine.punch_sender());
                let server = Arc::clone(&mizar);
                tokio::spawn(async move {
                    while let Some(stream) = engine.accept().await {
                        let server = Arc::clone(&server);
                        tokio::spawn(async move { server.serve_punched(stream).await });
                    }
                });
            }
            Err(error) => tracing::warn!("p2p: no direct path — {error}"),
        }
    }

    // The window needs a code to show; the kernel asks whoever is composed.
    core.attach_pairing(Arc::clone(&mizar) as Arc<dyn agi_core::PairingSource>);
    // The menu bar lists the phones; the roster and the live sockets both live
    // in the server, so it answers for both.
    core.attach_devices(Arc::clone(&mizar) as Arc<dyn agi_core::DeviceStatusSource>);

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

/// The relay half of boot: the tunnel, the official-only presence tunnel, and
/// the rules for replacing either without restarting the kernel.
///
/// The tunnel token and room survive a move — the room derives from the token —
/// so a phone that paired on one origin keeps finding this computer on the new
/// one. What changes is the origin the socket dials and the account headers it
/// may present.
struct Tunnels {
    token: String,
    port: u16,
    roster: Arc<DeviceStore>,
    core: Arc<Core>,
    mizar: Arc<MizarServer>,
    main: Mutex<Option<Arc<RelayClient>>>,
    /// The account instance's presence tunnel. Official-only, and only while
    /// signed in: that deployment refuses anonymous tunnels.
    legacy: Mutex<Option<Arc<RelayClient>>>,
    /// The origin the main tunnel is on right now.
    origin: Mutex<Option<String>>,
}

impl Tunnels {
    fn new(
        token: String,
        port: u16,
        roster: Arc<DeviceStore>,
        core: Arc<Core>,
        mizar: Arc<MizarServer>,
    ) -> Arc<Self> {
        Arc::new(Self {
            token,
            port,
            roster,
            core,
            mizar,
            main: Mutex::new(None),
            legacy: Mutex::new(None),
            origin: Mutex::new(None),
        })
    }

    /// One client for `base`, with the account source and peer policy attached.
    ///
    /// Both read the kernel on every dial, not at construction: a sign-in, a
    /// server switch or a policy change is visible to the very next redial.
    fn client(&self, base: &str) -> Arc<RelayClient> {
        let client = RelayClient::new(base, self.token.clone(), self.port);
        client.attach_store(Arc::clone(&self.roster));
        let core = Arc::clone(&self.core);
        client.set_account_source(Arc::new(move || core.relay_account()));
        let core = Arc::clone(&self.core);
        client.set_peer_policy(Arc::new(move || {
            let prefs = core.remote_prefs();
            prefs.enabled && prefs.allow_wan
        }));
        client
    }

    fn main_relay(&self) -> Option<Arc<RelayClient>> {
        self.main
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn current_origin(&self) -> Option<String> {
        self.origin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Point the tunnel at `base`, replacing whatever was there.
    fn move_to(&self, base: &str) {
        let next = self.client(base);
        {
            let mut slot = self
                .main
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(old) = slot.replace(Arc::clone(&next)) {
                old.stop();
            }
        }
        *self
            .origin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(base.trim_end_matches('/').to_string());
        // The phone server's `host.info` must name the new origin before the
        // socket comes back up, so a phone that asks during the move is not sent
        // to a room on the old relay.
        self.mizar.attach_relay(Arc::clone(&next));
        let running = Arc::clone(&next);
        tokio::spawn(async move { running.run().await });
        self.sync_legacy();
    }

    /// Stop the tunnel entirely, leaving the phone server without one.
    fn stop(&self) {
        {
            let mut slot = self
                .main
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(old) = slot.take() {
                old.stop();
            }
        }
        *self
            .origin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.sync_legacy();
    }

    /// Keep the official account instance's presence tunnel in step.
    ///
    /// It exists because the account gate routes `/desktops` and `/connect`
    /// there, and a desktop that only dials the unified 443 origin is invisible
    /// to it. A self-hosted install must never dial it: that deployment is not
    /// this operator's server.
    fn sync_legacy(&self) {
        let origin = self.current_origin();
        let wanted = origin
            .as_deref()
            .and_then(legacy_relay_url)
            .filter(|_| self.core.accounts_enabled() && self.core.relay_account().is_some());
        let mut slot = self
            .legacy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match wanted {
            Some(url) => {
                if let Some(client) = slot.as_ref() {
                    client.recycle();
                } else {
                    let client = self.client(url);
                    *slot = Some(Arc::clone(&client));
                    tokio::spawn(async move { client.run().await });
                }
            }
            None => {
                if let Some(client) = slot.take() {
                    client.recycle();
                    client.stop();
                }
            }
        }
    }
}

/// The one origin whose account deployment keeps its own presence tunnel.
///
/// Kept as a literal rather than read from a preference: it is a fact about the
/// official service, and a typo there would silently drop every computer from
/// the account's desktop list.
fn legacy_relay_url(base: &str) -> Option<&'static str> {
    (base.trim_end_matches('/') == agi_core::account::OFFICIAL_ACCOUNT_URL)
        .then_some("https://dshmizar-1.shirazu-nagisa.com:8443")
}

/// Which relay this launch should dial.
///
/// The order is the contract: an explicit `AGI_RELAY_URL` wins (tests and a
/// developer's local relay), then the stored self-hosted origin, then whatever
/// the caller asked for in official mode. A self-hosted choice with no usable
/// origin returns `None` rather than falling back to the official service: the
/// operator asked not to talk to it, and dialling it anyway would leak the
/// tunnel to a party they did not choose.
fn effective_relay_url(
    server: &ServerPrefs,
    explicit: Option<&str>,
    requested: Option<&str>,
) -> Option<String> {
    let clean = |value: &str| {
        let trimmed = value.trim().trim_end_matches('/');
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    if let Some(explicit) = explicit.and_then(clean) {
        return Some(explicit);
    }
    if server.is_self_hosted() {
        return clean(server.origin_trimmed());
    }
    requested.and_then(clean)
}

/// Install the Claude Code hooks, reporting the outcome rather than failing.
fn install_hooks() -> Result<(), String> {
    let Some(shim) = shim_path() else {
        return Err("agi-mizar-hook was not found next to this binary".to_string());
    };
    match agi_engine_claude::install(&agi_engine_claude::paths::settings_path(), &shim) {
        Ok(outcome) => {
            tracing::info!(
                "hooks: {} added, {} already present",
                outcome.added,
                outcome.present
            );
            Ok(())
        }
        Err(error) => Err(format!("{error}")),
    }
}

/// Find the Claude Code CLI.
///
/// `which` is the normal path, but a Finder-launched app does not inherit the
/// shell PATH and npm sometimes installs the platform binary without a working
/// `.bin` shim. The last two candidates cover those machines without requiring
/// `AGI_CLAUDE_BIN` in launchd.
fn claude_binary() -> Option<std::path::PathBuf> {
    let configured = std::env::var("AGI_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    if let Some(found) = which(&configured) {
        return Some(found);
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    [
        home.join(".local/bin/claude"),
        home.join(".local/node_modules/@anthropic-ai/claude-code-darwin-arm64/claude"),
        home.join(".claude/local/claude"),
        home.join(".cargo/bin/claude"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

/// The Claude engine, discovered or transcript-only.
///
/// Always built: with no `claude` executable there are still transcripts to
/// read, and a list is useful without the ability to prompt.
fn build_claude() -> Arc<ClaudeEngine> {
    match claude_binary() {
        Some(path) => {
            tracing::info!("claude at {}", path.display());
            // The discovered path, not the bare name: a Finder-launched app's
            // PATH is launchd's, and "claude" would not resolve there even
            // though the file exists.
            agi_engine_claude::ClaudeEngine::new(
                agi_engine_claude::paths::projects_dir(),
                path.to_string_lossy().to_string(),
            )
        }
        None => {
            tracing::info!("claude not found — transcripts readable, prompts are not");
            ClaudeEngine::from_environment()
        }
    }
}

/// Find the agents that are actual binaries on this machine.
///
/// Called at launch and again whenever the operator asks the window to look
/// again, which is the case that matters: they just installed the CLI in another
/// terminal and want the agent row to stop being grey.
fn discover_local(already: &[EngineId]) -> (Vec<Arc<dyn Engine>>, HashMap<EngineId, String>) {
    let mut hosted: Vec<Arc<dyn Engine>> = Vec::new();
    let mut missing: HashMap<EngineId, String> = HashMap::new();

    // Codex is spawned, so it is only hosted when there is something to spawn:
    // an absent binary would otherwise mean a supervisor retrying a missing file
    // forever and a permanently red status on a machine that has no Codex.
    let codex_binary = std::env::var("AGI_CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
    // The ChatGPT app ships the same app-server binary; when nobody installed a
    // `codex` on PATH, that copy is the one the GUI itself uses. Only the macOS
    // bundle has that layout; on Windows the npm shim is the real answer and
    // `which` finds it through PATHEXT.
    let codex_path = {
        #[cfg(target_os = "macos")]
        {
            // ChatGPT 26.1029 moved the bundled CLI from `Resources/codex` to
            // `Resources/codex-cli`. Keep the old path as a fallback: people
            // update the desktop app independently of this binary, and either
            // layout can be present during the transition.
            which(&codex_binary).or_else(|| {
                [
                    "/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex",
                    "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
                    "/Applications/ChatGPT.app/Contents/Resources/codex",
                ]
                .into_iter()
                .find(|path| std::path::Path::new(path).is_file())
                .map(std::path::PathBuf::from)
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            which(&codex_binary)
        }
    };
    match codex_path {
        Some(path) => {
            if !already.contains(&EngineId::Codex) {
                let codex = CodexEngine::new(AppServerConfig {
                    binary: path.to_string_lossy().to_string(),
                    args: Vec::new(),
                    cwd: None,
                });
                let supervisor = Arc::clone(&codex);
                tokio::spawn(async move { supervisor.run().await });
                hosted.push(codex);
                tracing::info!("codex at {}", path.display());
            }
        }
        None => {
            missing.insert(
                EngineId::Codex,
                "codex was not found; install the ChatGPT app or the codex CLI".to_string(),
            );
            tracing::info!("codex not found — set AGI_CODEX_BIN to point at one");
        }
    }

    // Claude Code is always hosted: with no `claude` executable there are still
    // transcripts to read, and a list is useful without the ability to prompt.
    if !already.contains(&EngineId::Claude) {
        hosted.push(Arc::clone(&build_claude()) as Arc<dyn Engine>);
    }
    if claude_binary().is_none() {
        missing.insert(
            EngineId::Claude,
            "the claude CLI was not found; transcripts are still readable".to_string(),
        );
        tracing::info!("claude not found — transcripts readable, prompts are not");
    }

    // DSH is a link, not a binary, but "is DeepSeek Harness installed" still has
    // an answer the window needs.
    if !dsh_installed() {
        missing.insert(
            EngineId::Dsh,
            "DeepSeek Harness was not found on this machine".to_string(),
        );
    }

    (hosted, missing)
}

/// Whether the DSH host exists: the app, its CLI, or its home directory.
fn dsh_installed() -> bool {
    // Each platform installs the same thing somewhere else. A bundle on macOS,
    // a per-user Programs directory on Windows, and — on any platform — a CLI
    // on PATH or a home directory DSH has already written to.
    #[cfg(target_os = "macos")]
    if std::path::Path::new("/Applications/DeepSeek Harness.app").exists() {
        return true;
    }
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let programs = PathBuf::from(local)
                .join("Programs")
                .join("DeepSeek Harness");
            if programs.join("DeepSeek Harness.exe").exists() {
                return true;
            }
            if programs.exists() {
                return true;
            }
        }
    }
    if which("dsh").is_some() {
        return true;
    }
    agi_platform::home().join(".dsh").exists()
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
    if candidate.is_file() {
        return Some(candidate);
    }
    // A packaged build signs and notarizes resources with the bundle, so that is
    // where the shim lives when it is not beside the executable.
    let resource = directory.parent()?.join("Resources").join(name);
    resource.is_file().then_some(resource)
}

/// Where to ask what this computer looks like from outside.
///
/// The relay that carries this tunnel also runs a STUN server on the standard
/// port, so the answer comes from the party the pairing already trusts and no
/// new one learns where this machine connects from. A self-hosted relay answers
/// on its own host; an unreachable one costs the direct path its outside
/// candidate and nothing else.
///
/// `AGI_STUN_SERVERS` (comma separated) overrides the list, which is what a
/// machine behind a network that blocks the relay's UDP responder uses to point
/// at another STUN server.
///
/// - Parameter relay: the tunnel's base URL.
/// - Returns: the servers to ask, best first; empty when the URL is unusable.
fn stun_servers_for(relay: &str) -> Vec<String> {
    if let Ok(explicit) = std::env::var("AGI_STUN_SERVERS") {
        let servers: Vec<String> = explicit
            .split(',')
            .map(|server| server.trim().to_string())
            .filter(|server| !server.is_empty())
            .collect();
        if !servers.is_empty() {
            return servers;
        }
    }
    // Hand-parsed rather than through a URL crate: the only part wanted is the
    // host, and this file has no business depending on an HTTP client for it.
    let rest = relay
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("wss://")
        .trim_start_matches("ws://");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(authority);
    if host.is_empty() {
        return Vec::new();
    }
    let mut servers = vec![format!("{host}:3478")];
    // The unified 443 relay lives on a host whose deployment does not run a
    // STUN responder. The main host is the one this project operates, and the
    // pairing already trusts it, so it is the fallback rather than no outside
    // candidate at all. A self-hosted relay has no such fallback: adding this
    // host would send its address to a party its operator never chose.
    const CANONICAL: &str = "dshmizar.shirazu-nagisa.com:3478";
    if host.ends_with("shirazu-nagisa.com") && !servers.iter().any(|server| server == CANONICAL) {
        servers.push(CANONICAL.to_string());
    }
    servers
}

/// 32 bytes of randomness, hex encoded.
pub fn random_token() -> String {
    let mut token = String::with_capacity(64);
    for _ in 0..2 {
        token.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    token
}

/// The API token a service manager pinned for this install.
///
/// A launched window can use a per-launch secret because it hands the page a
/// fresh URL. A browser bookmark cannot; a Linux service therefore writes a
/// token once and every launch exchanges it for a cookie. Only URL-safe visible
/// ASCII is accepted, because the value goes into a cookie header and a query
/// string verbatim.
fn configured_api_token() -> Option<String> {
    match std::env::var("AGI_API_TOKEN") {
        Ok(token)
            if token.len() >= 32
                && token.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
                }) =>
        {
            Some(token)
        }
        Ok(_) => {
            tracing::warn!(
                "AGI_API_TOKEN must be at least 32 URL-safe visible ASCII characters; \
                 using a per-launch token"
            );
            None
        }
        Err(_) => None,
    }
}

/// Find an executable the way a shell would: an absolute path, or `PATH`.
///
/// The answer differs enough per platform — launchd's PATH on macOS, `PATHEXT`
/// and unused npm shims on Windows — that it lives in [`agi_platform::which`],
/// which both this crate and the engines consult.
pub fn which(binary: &str) -> Option<PathBuf> {
    agi_platform::which(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFICIAL: &str = "https://dshmizar-1.shirazu-nagisa.com";

    fn self_hosted(origin: &str) -> ServerPrefs {
        ServerPrefs {
            mode: "selfHosted".to_string(),
            origin: origin.to_string(),
            pin: String::new(),
        }
    }

    #[test]
    fn the_official_default_is_kept_when_the_stored_choice_is_official() {
        assert_eq!(
            effective_relay_url(&ServerPrefs::default(), None, Some(OFFICIAL)),
            Some(OFFICIAL.to_string())
        );
        // The daemon's "no tunnel" request is still no tunnel.
        assert_eq!(
            effective_relay_url(&ServerPrefs::default(), None, None),
            None
        );
    }

    #[test]
    fn a_stored_self_hosted_origin_replaces_the_official_default() {
        assert_eq!(
            effective_relay_url(
                &self_hosted("https://relay.example.com/"),
                None,
                Some(OFFICIAL)
            ),
            Some("https://relay.example.com".to_string())
        );
        assert_eq!(
            effective_relay_url(
                &self_hosted("https://relay.example.com:8443"),
                None,
                Some(OFFICIAL)
            ),
            Some("https://relay.example.com:8443".to_string())
        );
    }

    #[test]
    fn an_explicit_relay_url_wins_over_the_stored_choice() {
        assert_eq!(
            effective_relay_url(
                &self_hosted("https://relay.example.com"),
                Some("http://127.0.0.1:9000/"),
                Some(OFFICIAL)
            ),
            Some("http://127.0.0.1:9000".to_string())
        );
    }

    #[test]
    fn a_self_hosted_choice_without_an_origin_never_falls_back_to_official() {
        // A hand-edited file can carry `mode: selfHosted` and no origin. Dialling
        // the official service instead would hand the tunnel to a party the
        // operator explicitly moved away from.
        assert_eq!(
            effective_relay_url(&self_hosted("   "), None, Some(OFFICIAL)),
            None
        );
    }

    #[test]
    fn only_the_official_origin_keeps_the_presence_tunnel() {
        assert_eq!(
            legacy_relay_url("https://dshmizar-1.shirazu-nagisa.com/"),
            Some("https://dshmizar-1.shirazu-nagisa.com:8443")
        );
        assert_eq!(legacy_relay_url("https://relay.example.com"), None);
        assert_eq!(
            legacy_relay_url("https://dshmizar.shirazu-nagisa.com"),
            None
        );
    }

    #[test]
    fn a_guide_finished_before_this_build_is_not_done() {
        // No stored version at all: the file predates the field.
        let old = agi_core::Onboarding {
            done: true,
            guide_version: String::new(),
        };
        assert!(!old.is_complete(), "the new steps still have to be shown");
        // A version from another older release is the same answer.
        let older = agi_core::Onboarding {
            done: true,
            guide_version: "0.1.9".to_string(),
        };
        assert!(!older.is_complete());
        // Finishing the shipped guide is what turns the flag off.
        let current = agi_core::Onboarding {
            done: true,
            guide_version: agi_core::GUIDE_VERSION.to_string(),
        };
        assert!(current.is_complete());
        // A version without the flag is still an unfinished guide.
        let unfinished = agi_core::Onboarding {
            done: false,
            guide_version: agi_core::GUIDE_VERSION.to_string(),
        };
        assert!(!unfinished.is_complete());
    }
}
