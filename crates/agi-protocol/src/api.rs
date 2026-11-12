//! The local API between the core and any client of it.
//!
//! The front end is not the only possible client: the desktop UI is one, the
//! `agi-daemon` binary's CLI is another, and a future headless deployment is a
//! third. Keeping this surface engine-neutral and transport-neutral is what lets
//! all three exist without a second model layer.
//!
//! ### Why one command endpoint
//!
//! A REST surface would spread the contract over a dozen paths and make every
//! addition a routing decision. One `POST /api/v1/command` with a tagged enum
//! means the contract is a single Rust type, the generated TypeScript is a single
//! union, and an unknown `op` is a clear error instead of a 404 that might mean
//! "old client".

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{Interaction, Record, Session, SessionKey};
use crate::engine::{Capabilities, EngineId};

/// Everything a freshly opened window needs to draw itself.
#[derive(Debug, Clone, Serialize)]
pub struct StateSnapshot {
    /// The application version, for the about panel and bug reports.
    pub version: String,
    /// `macos`, `windows` or `linux`: which machine this window draws for.
    ///
    /// The page uses it for the words only one desktop uses (the Dock, the
    /// notification area) and for the layout only one needs (the strip macOS
    /// reserves for its traffic lights). Answering it once in the snapshot keeps
    /// every component from guessing from a build-time flag.
    pub platform: String,
    /// `system`, `light` or `dark` — the window paints itself before it has
    /// asked the kernel, and this is the stored answer for the next launch.
    pub theme: String,
    pub engines: Vec<EngineStatus>,
    pub sessions: Vec<Session>,
    pub interactions: Vec<Interaction>,
    /// Whether the first-run guide still has to be shown.
    pub onboarding: OnboardingView,
    /// What model this install judges turns with, and whether it is running.
    pub model: ModelView,
    /// The phones paired with this computer, and which of them are connected.
    pub devices: Vec<DeviceView>,
    /// Whether this install anonymously shares turns to improve Mizarllm.
    pub dataset: DatasetView,
    /// The DSH helper plugin's version, and whether npm has a newer one.
    #[serde(rename = "dshPlugin")]
    pub dsh_plugin: DshPluginView,
}

/// What this install knows about the DSH helper plugin.
///
/// The plugin is a package inside DSH's profile, not a binary this app owns, so
/// its version has to be read from DSH's own directory and compared with the
/// registry. The window uses this both for the plugin card on the DSH page and
/// for the one popup that asks to update it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DshPluginView {
    /// The version found in the DSH profile, when the plugin is installed.
    pub installed: Option<String>,
    /// The newest version npm published, once a check has succeeded.
    pub latest: Option<String>,
    /// The oldest plugin version this app speaks to.
    pub minimum: String,
    /// Whether npm has a newer, compatible version than the installed one.
    pub update_available: bool,
    /// The DSH profile the plugin is (or would be) installed in.
    pub profile: String,
    /// Whether a `dsh` command was found, so the one-click install can work.
    pub cli: bool,
    /// Why the last npm check failed, when it did.
    pub error: Option<String>,
    /// Epoch milliseconds of the last successful check.
    pub last_checked: Option<i64>,
}

impl Default for DshPluginView {
    fn default() -> Self {
        Self {
            installed: None,
            latest: None,
            minimum: DSH_PLUGIN_MIN_VERSION.to_string(),
            update_available: false,
            profile: "desktop".to_string(),
            cli: false,
            error: None,
            last_checked: None,
        }
    }
}

/// The oldest `dsh-mizar` this build understands.
///
/// Kept next to the wire types because it is a fact about the contract, not
/// about one platform: the plugin's 0.9.0 release is the first one that speaks
/// DSH Link v1 and dials the desktop instead of listening itself.
pub const DSH_PLUGIN_MIN_VERSION: &str = "0.9.0";

/// One dependency the agent needs before this app can talk to it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyItem {
    /// Stable id, for tests and for the window's `key`.
    pub id: String,
    pub label: String,
    /// `ok`, `missing`, `outdated`, or `unknown`.
    pub status: String,
    /// One sentence a person can act on.
    pub detail: String,
    /// What the one-click repair would do, when it can do anything.
    pub fix: Option<String>,
}

/// Whether one agent's side of the connection is complete.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyReport {
    pub engine: EngineId,
    /// Whether no item needs attention (`missing` or `outdated`).
    ///
    /// `unknown` is tolerated: an offline registry is not a broken connection.
    pub ok: bool,
    /// The sentence the agent page shows above the list.
    pub summary: String,
    pub items: Vec<DependencyItem>,
    /// What a repair changed; empty for a read-only check.
    pub changed: Vec<String>,
    /// Why a repair could not finish, when it could not.
    pub error: Option<String>,
}

/// Whether the remote-agent feature has everything it needs on this computer.
///
/// Separate from [`DependencyReport`] because there is no engine: the feature is
/// the whole product, and its pieces (the sidecar, the DSH entry and skill, the
/// scoped tool token, the account, the two routes) have their own owners. Same
/// item shape so the window can draw both lists the same way.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteHealthReport {
    /// Whether no item is `missing` or `outdated`; `unknown` is tolerated, as in
    /// a dependency report.
    pub ok: bool,
    /// The sentence above the list.
    pub summary: String,
    pub items: Vec<DependencyItem>,
    /// What a repair changed; empty for a read-only check.
    pub changed: Vec<String>,
    /// Why a repair could not finish, when it could not.
    pub error: Option<String>,
}

/// Whether this install anonymously shares its turns to help train Mizarllm.
///
/// The choice is per machine, stored beside the window's other preferences. The
/// kernel answers with the stored value and the copy version the operator
/// agreed to; nothing about the upload itself (counts, progress) is published,
/// because the sharing is meant to be invisible in normal use.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DatasetView {
    /// The operator's choice right now.
    pub enabled: bool,
    /// The consent copy they accepted; `0` means they never chose.
    pub policy_version: u32,
    /// Epoch milliseconds of the last change to `enabled`.
    pub since: Option<i64>,
    /// Project names whose turns are never shared.
    pub excluded_projects: Vec<String>,
}

/// One paired phone, as the menu bar lists it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    /// `iPhone17,1`, or absent when the phone never said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether this phone has a socket open right now.
    pub online: bool,
    /// Whether the phone registered live-activity push tokens.
    ///
    /// A backgrounded phone has no socket, but a running session still reaches
    /// it through the lock screen; the roster counts that as reachable too.
    /// The token is only a capability -- the window pairs it with "a session is
    /// running" before it draws the lamp.
    #[serde(default)]
    pub live_activity: bool,
    /// When it last reached this computer, in milliseconds since the epoch.
    #[serde(default)]
    pub last_seen_ms: i64,
}

/// One engine's connection state, as the UI's engine switch reads it.
#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub engine: EngineId,
    /// Whether prompts can be sent right now.
    pub connected: bool,
    /// Whether the agent is installed on this machine at all.
    ///
    /// `connected` answers "can I send a prompt"; this answers the earlier
    /// question. An agent that was never installed and one that is installed
    /// but silent are different problems for the person in front of the window,
    /// and only the second one is worth a reconnect button.
    pub available: bool,
    /// Whether the operator picked this agent in the guide (or configured it
    /// later) and its dependencies are in place.
    pub configured: bool,
    /// The lamp's answer: enabled here *and* live now.
    ///
    /// `connected` is the send-gate ("can a prompt go through"); this is the
    /// narrower claim the window draws. DSH is live while its plugin is
    /// attached; Claude Code and Codex have no standing socket of their own, so
    /// they are live only while a turn is running. Keeping the two apart means
    /// the lamp can go out without turning off prompting.
    pub engaged: bool,
    pub capabilities: Capabilities,
    /// A short human-readable fact: the plugin version, the app-server version,
    /// or why the engine is not usable.
    pub detail: Option<String>,
    pub sessions: usize,
}

/// Whether the first-run guide has been completed.
#[derive(Debug, Clone, Serialize, Default)]
pub struct OnboardingView {
    pub done: bool,
}

/// What model this install has, and whether the thing that runs it is up.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ModelView {
    /// Every model in the models directory.
    pub installed: Vec<InstalledModel>,
    /// The one the kernel would call, when one is set.
    pub active: Option<InstalledModel>,
    /// The model an upgrade replaced, kept for a rollback.
    pub previous: Option<InstalledModel>,
    pub runtime: RuntimeView,
    /// A download in flight, when one is.
    pub download: Option<DownloadView>,
}

/// One model on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledModel {
    pub id: String,
    pub version: String,
    pub path: String,
    pub size_bytes: u64,
}

/// The sidecar's own state, separate from the model's.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeView {
    /// `stopped`, `starting`, `ready` or `failed`.
    pub state: String,
    pub port: Option<u16>,
    /// Why it failed, or which binary was started.
    pub detail: Option<String>,
}

impl Default for RuntimeView {
    fn default() -> Self {
        Self {
            state: "stopped".to_string(),
            port: None,
            detail: None,
        }
    }
}

/// A download in flight.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadView {
    pub version: String,
    pub done: u64,
    pub total: u64,
    /// `running`, `verifying` or `failed`.
    pub state: String,
    pub error: Option<String>,
    /// The mirror this attempt reads from ("ModelScope", "GitHub"), when one is
    /// known. A download that is slow is only actionable if it says where from.
    pub source: Option<String>,
}

/// What the model repository says about the newest release.
#[derive(Debug, Clone, Serialize)]
pub struct ModelCheckView {
    /// The version installed on this machine, when one is.
    pub installed: Option<String>,
    pub latest: Option<RemoteModel>,
    /// Why the check failed, when it did.
    pub error: Option<String>,
}

/// A release the repository offers, resolved to a downloadable asset.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteModel {
    pub id: String,
    pub version: String,
    /// Direct asset URL, version-pinned.
    pub url: String,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
    /// Whether this is newer than what is installed.
    pub newer: bool,
}

/// One day of work, as the home page charts it.
///
/// A day is a local calendar day, because that is the unit an operator thinks
/// in: "what did I do on Tuesday" is not a question about UTC. Every engine
/// buckets into its own local day and the kernel adds the buckets up, so a host
/// whose engines disagree about the time zone cannot produce a day that no clock
/// showed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UsageDay {
    /// "YYYY-MM-DD", local time zone.
    pub date: String,
    /// Every user or assistant message counts once; tool calls and tool results
    /// are not messages and do not.
    pub messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 24 buckets, local hours, index 0 being midnight.
    ///
    /// Each bucket counts messages, not tokens: the window reads this as "when
    /// in the day do I work", and an hour that was active but cheap is still an
    /// hour somebody was at the machine.
    pub hours: [u64; 24],
    pub models: Vec<ModelUsage>,
}

/// What one model did on one day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub model: String,
    pub messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The whole usage history, one row per day that had any activity.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UsageOverview {
    /// Top-level, unarchived sessions, the three engines added together.
    ///
    /// A delegated child (a subagent, a fork's source) is not a session the
    /// operator started, so counting it would inflate the number they read as
    /// "how much have I been working".
    pub sessions: u64,
    /// Only days with activity, oldest first.
    pub days: Vec<UsageDay>,
}

/// One command from a client.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    /// Everything, again. Cheap: the core already holds it.
    State,
    /// Read a page of a session's records.
    Messages {
        session: SessionKey,
        #[serde(default)]
        limit: Option<usize>,
        /// Return records with a `seq` strictly below this.
        #[serde(default)]
        before: Option<i64>,
    },
    /// Send a prompt to a session.
    Prompt { session: SessionKey, text: String },
    /// Add a message to the turn that is already running.
    Steer { session: SessionKey, text: String },
    /// Fork a session into a new one at a completed turn boundary.
    Fork { session: SessionKey, at_seq: i64 },
    /// Stop the running turn.
    Cancel { session: SessionKey },
    /// Compact this session's context using the engine's own mechanism.
    Compact { session: SessionKey },
    /// Answer a pending approval or question batch.
    Answer {
        id: String,
        #[serde(default)]
        outcome: Option<String>,
        #[serde(default)]
        answers: Option<Vec<Value>>,
    },
    /// Choose a model for a session.
    SetModel { session: SessionKey, model: String },
    /// Choose a permission preset for a session.
    SetPermission { session: SessionKey, preset: String },
    /// What the running turn is doing.
    Activity {
        session: SessionKey,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// What a session has cost.
    Stats { session: SessionKey },
    /// Files the model said it produced.
    Deliverables { session: SessionKey },
    /// Every model an engine offers.
    Models { engine: EngineId },
    /// Every permission preset an engine offers.
    Permissions { engine: EngineId },
    /// Start a session in a directory.
    Create {
        engine: EngineId,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Give a session a name.
    Rename { session: SessionKey, title: String },
    /// Hide a session without deleting it.
    Archive {
        session: SessionKey,
        #[serde(default)]
        stop_activity: Option<bool>,
    },
    /// The host's own workspace grouping.
    Workspaces { engine: EngineId },
    /// What the provider account is worth.
    Balance { engine: EngineId },
    /// Read a file a session delivered.
    ReadFile { session: SessionKey, path: String },

    /// Every day of work, across every engine, as the home page charts it.
    UsageOverview,

    /// Drop an engine's current link and let it dial again.
    Reconnect { engine: EngineId },

    /// The pairing code to show a phone, and a QR for its URI.
    Pairing {
        /// Issue a fresh code instead of returning the one already on screen.
        #[serde(default)]
        refresh: bool,
    },
    /// Remember the window's appearance across launches.
    SetTheme { theme: String },

    /// Whether this window's tunnel to the relay is up.
    RelayStatus,
    /// The account this window is signed in to, and the devices under it.
    Account,
    /// Sign in to the relay's account service.
    AccountLogin {
        identifier: String,
        password: String,
    },
    /// Forget the account on this machine.
    AccountLogout,
    /// Recent delegation audit lines (UI token only).
    RemoteAudit,
    /// Check the pieces the remote-agent feature needs; `repair` also rewrites
    /// the DSH MCP entry and skill when they are missing or stale (UI token
    /// only).
    RemoteHealth {
        #[serde(default)]
        repair: bool,
    },
    /// Every remote-agent log line on disk, plus what the check found, as one
    /// text file for a bug report (UI token only).
    RemoteLogs,
    /// Install/repair the DSH MCP entry and skill (UI token only).
    RemoteInstall {
        #[serde(default)]
        profile: Option<String>,
        #[serde(default)]
        dry_run: bool,
    },
    /// Remove the DSH MCP entry and skill (UI token only).
    RemoteUninstall {
        #[serde(default)]
        profile: Option<String>,
        #[serde(default)]
        dry_run: bool,
    },
    /// The account's computers, with whether each is reachable now.
    AccountDesktops,
    /// This computer's LAN identity and certificate status.
    LanIdentity,
    /// Fetch/renew the account CA certificate for this LAN identity.
    LanCertificate,
    /// This computer's policy for accepting same-account peers.
    RemoteTargetSettings,
    /// Replace the target policy. UI token only; never the tool token.
    SetRemoteTargetSettings {
        enabled: bool,
        allow_lan: bool,
        allow_wan: bool,
        /// When true, every DSH workspace is available; the list is ignored.
        #[serde(default)]
        allow_all_workspaces: bool,
        #[serde(default)]
        workspaces: Vec<String>,
        #[serde(default)]
        blocked: Vec<String>,
    },
    /// Same-account computers this install may delegate to.
    RemotePeers,
    /// Ask a peer for its permitted workspaces and permission presets.
    RemoteOptions { peer: String },
    /// Start one remote delegation. Returns immediately with its id.
    RemoteDelegate {
        peer: String,
        #[serde(default)]
        cwd: Option<String>,
        /// Permission preset to apply to the new remote session.
        #[serde(default)]
        permission: Option<String>,
        /// Model route selected from `remote_options.models`.
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        model: Option<String>,
        /// Reasoning level selected from that model's `efforts`.
        #[serde(default)]
        reasoning_effort: Option<String>,
        prompt: String,
    },
    /// Every delegation this install knows about.
    RemoteDelegations,
    /// One delegation's state and final text.
    RemoteResult { id: String },
    /// Stop the remote turn and mark the delegation cancelled.
    RemoteCancel { id: String },
    /// Answer one remote approval (P4; P1 answers NOT_IMPLEMENTED).
    RemoteAnswer {
        id: String,
        approval: String,
        outcome: String,
    },
    /// Send another prompt to the same remote session.
    RemoteSend { id: String, message: String },
    /// Where the browser should go to sign in with GitHub.
    AccountGithubStart,
    /// Spend the handoff code GitHub's done page showed.
    AccountGithubExchange { handoff: String },
    /// Where the browser should go to sign in with Apple.
    AccountAppleStart,
    /// Spend the handoff code Apple's done page showed.
    AccountAppleExchange { handoff: String },
    /// Wait for the Apple browser round-trip and sign in when it lands.
    AccountAppleAwait { state: String },
    /// Wait for the GitHub browser round-trip and sign in when it lands.
    AccountGithubAwait { state: String },
    /// Open an https URL in the system browser.
    OpenExternal { url: String },

    /// What model this install has, and whether its sidecar is up.
    ModelState,
    /// Ask the model repository what the newest release is.
    ModelCheck,
    /// Download a model version; progress arrives as `model_progress` events.
    ModelDownload { version: String },
    /// Stop the download in flight.
    ModelCancel,
    /// Make an installed model the one the kernel calls.
    ModelActivate { id: String },
    /// Delete an installed model that is not the active one.
    ModelRemove { version: String },
    /// Go back to the model the last upgrade replaced.
    ModelRollback,

    /// Run engine discovery again, so an agent installed after launch shows up.
    EngineRescan,
    /// Remember whether an agent is configured on this machine.
    EngineConfigure { engine: EngineId, enabled: bool },

    /// Read one agent's connection dependencies without changing anything.
    Dependencies { engine: EngineId },
    /// Check one agent's dependencies and repair what can be repaired.
    DependenciesRepair { engine: EngineId },
    /// Ask npm what the newest `dsh-mizar` is.
    DshPluginCheck,
    /// Install or update `dsh-mizar` in the DSH profile npm reports as current.
    DshPluginUpdate,

    /// The guide finished; do not show it again.
    OnboardingComplete,
    /// Show the guide again on the next snapshot.
    OnboardingReset,

    /// Turn anonymous session-data sharing on or off.
    ///
    /// Enabling starts the background backfill of the last year's turns and the
    /// uploader; disabling stops both immediately. Nothing is ever deleted from
    /// the server, because the server never learns which account the turns came
    /// from.
    DatasetShare { enabled: bool },
    /// Replace the list of projects whose turns are never shared.
    DatasetExclude { projects: Vec<String> },

    /// Liveness probe, used by the shell before it shows a window.
    Ping,
}

impl Command {
    /// Whether this is one of the tool-token commands.
    ///
    /// The UI bearer answers everything; `agent-tool.json` is deliberately
    /// narrower. Keep this list next to the enum so a new Remote command cannot
    /// be forgotten in the server's token check.
    pub fn is_remote(&self) -> bool {
        matches!(
            self,
            Self::RemotePeers
                | Self::RemoteOptions { .. }
                | Self::RemoteDelegate { .. }
                | Self::RemoteDelegations
                | Self::RemoteResult { .. }
                | Self::RemoteCancel { .. }
                | Self::RemoteAnswer { .. }
                | Self::RemoteSend { .. }
        )
    }
}

/// What the window draws to pair a phone.
#[derive(Debug, Clone, Serialize)]
pub struct PairingView {
    /// The code as a person reads it, `K7QP-3M2X`.
    pub code: String,
    /// `mizar://pair?...`, which the QR encodes.
    pub uri: String,
    /// Every address a phone on this network could dial, best first.
    pub addresses: Vec<String>,
    /// An SVG QR of `uri`, drawn by the host so the window needs no library.
    pub svg: Option<String>,
    /// Epoch milliseconds after which the code is dead.
    pub expires_at: i64,
}

/// The answer to one command.
#[derive(Debug, Clone, Serialize)]
pub struct CommandResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl CommandResponse {
    /// A successful answer.
    pub fn ok(value: impl Serialize) -> Self {
        Self {
            ok: true,
            value: serde_json::to_value(value).ok(),
            error: None,
        }
    }

    /// An empty success.
    pub fn done() -> Self {
        Self {
            ok: true,
            value: None,
            error: None,
        }
    }

    /// A failure the client can branch on.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            value: None,
            error: Some(ApiError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

/// A failure at the API boundary.
#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

/// What the server pushes to connected clients.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiEvent {
    /// A session appeared or its summary changed.
    SessionUpdated { session: Session },
    /// A session is gone (archived, deleted, or its engine disconnected).
    SessionRemoved { session: SessionKey },
    /// One new record in a session.
    RecordAppended { session: SessionKey, record: Record },
    /// An agent is waiting on the operator.
    InteractionRequested { interaction: Interaction },
    /// An interaction was answered or withdrawn, wherever it was answered.
    InteractionResolved { id: String },
    /// An engine connected, disconnected, or changed its detail.
    EngineStatus { status: EngineStatus },
    /// A model download moved; sent throttled, so a 1 GB download is not a
    /// thousand frames a second.
    ModelProgress { download: DownloadView },
    /// The model set or its sidecar changed in a way worth redrawing.
    ModelChanged { model: ModelView },
    /// The first-run guide was finished or re-armed.
    OnboardingChanged { done: bool },
    /// The DSH helper plugin's version, or npm's newest, changed.
    DshPluginChanged { plugin: DshPluginView },
    /// Something the operator should see but that is not an error.
    Notice { level: String, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_day() -> UsageDay {
        let mut hours = [0u64; 24];
        hours[9] = 3;
        UsageDay {
            date: "2026-10-27".to_string(),
            messages: 3,
            input_tokens: 1_200,
            output_tokens: 340,
            hours,
            models: vec![ModelUsage {
                model: "deepseek-v4".to_string(),
                messages: 3,
                input_tokens: 1_200,
                output_tokens: 340,
            }],
        }
    }

    #[test]
    fn the_overview_wires_camel_case_and_a_full_day_of_hours() {
        // The window decodes this by key name, and twelve of the twenty-four
        // hours are `0`, which a serializer that skips defaults would drop.
        let wire = serde_json::to_value(UsageOverview {
            sessions: 12,
            days: vec![a_day()],
        })
        .unwrap();

        assert_eq!(wire["sessions"], 12);
        let day = &wire["days"][0];
        for key in [
            "date",
            "messages",
            "inputTokens",
            "outputTokens",
            "hours",
            "models",
        ] {
            assert!(day.get(key).is_some(), "missing {key} in {day}");
        }
        let hours = day["hours"].as_array().expect("hours is an array");
        assert_eq!(hours.len(), 24);
        assert!(hours.iter().all(serde_json::Value::is_u64));
        assert_eq!(hours[9], 3);

        let model = &day["models"][0];
        for key in ["model", "messages", "inputTokens", "outputTokens"] {
            assert!(model.get(key).is_some(), "missing {key} in {model}");
        }
        // No snake_case spelling anywhere: the app would read a missing key.
        assert!(wire.get("input_tokens").is_none());
        assert!(day.get("input_tokens").is_none());
    }

    #[test]
    fn an_empty_overview_is_still_a_full_shape() {
        let wire = serde_json::to_value(UsageOverview::default()).unwrap();
        assert_eq!(wire["sessions"], 0);
        assert_eq!(wire["days"], serde_json::json!([]));
    }

    #[test]
    fn the_command_is_spelled_like_its_neighbours() {
        let command: Command = serde_json::from_value(serde_json::json!({
            "op": "usage_overview"
        }))
        .unwrap();
        assert!(matches!(command, Command::UsageOverview));
    }

    #[test]
    fn the_plugin_view_is_camel_case_and_names_its_floor() {
        let wire = serde_json::to_value(DshPluginView {
            installed: Some("0.9.0".to_string()),
            latest: Some("0.9.2".to_string()),
            minimum: DSH_PLUGIN_MIN_VERSION.to_string(),
            update_available: true,
            profile: "desktop".to_string(),
            cli: true,
            error: None,
            last_checked: Some(1),
        })
        .unwrap();
        for key in [
            "installed",
            "latest",
            "minimum",
            "updateAvailable",
            "profile",
            "cli",
            "error",
            "lastChecked",
        ] {
            assert!(wire.get(key).is_some(), "missing {key} in {wire}");
        }
        assert!(wire.get("update_available").is_none());
        assert!(wire.get("last_checked").is_none());
        assert_eq!(wire["minimum"], DSH_PLUGIN_MIN_VERSION);
    }

    #[test]
    fn the_tool_scope_is_exactly_the_remote_commands() {
        assert!(Command::RemotePeers.is_remote());
        assert!(Command::RemoteOptions { peer: "p".into() }.is_remote());
        assert!(Command::RemoteDelegate {
            peer: "p".into(),
            cwd: None,
            permission: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            prompt: "t".into(),
        }
        .is_remote());
        assert!(Command::RemoteDelegations.is_remote());
        assert!(Command::RemoteResult { id: "d".into() }.is_remote());
        assert!(Command::RemoteCancel { id: "d".into() }.is_remote());
        assert!(Command::RemoteAnswer {
            id: "d".into(),
            approval: "a".into(),
            outcome: "allowed-once".into(),
        }
        .is_remote());
        assert!(Command::RemoteSend {
            id: "d".into(),
            message: "more".into(),
        }
        .is_remote());
        for command in [
            Command::State,
            Command::AccountDesktops,
            Command::Messages {
                session: "dsh:s".parse().unwrap(),
                limit: None,
                before: None,
            },
        ] {
            assert!(!command.is_remote());
        }
    }

    #[test]
    fn the_remote_check_and_log_commands_are_window_only() {
        // The tool token drives delegations; it has no business reading the log
        // export or rewriting the DSH profile.
        assert!(!Command::RemoteHealth { repair: false }.is_remote());
        assert!(!Command::RemoteLogs.is_remote());

        let command: Command = serde_json::from_value(serde_json::json!({
            "op": "remote_health"
        }))
        .unwrap();
        assert!(matches!(command, Command::RemoteHealth { repair: false }));
        let command: Command = serde_json::from_value(serde_json::json!({
            "op": "remote_health",
            "repair": true
        }))
        .unwrap();
        assert!(matches!(command, Command::RemoteHealth { repair: true }));
    }

    #[test]
    fn the_dependency_commands_parse_with_their_engine() {
        let command: Command = serde_json::from_value(serde_json::json!({
            "op": "dependencies_repair",
            "engine": "dsh"
        }))
        .unwrap();
        assert!(matches!(
            command,
            Command::DependenciesRepair {
                engine: EngineId::Dsh
            }
        ));
        let command: Command = serde_json::from_value(serde_json::json!({
            "op": "dsh_plugin_update"
        }))
        .unwrap();
        assert!(matches!(command, Command::DshPluginUpdate));
    }
}
