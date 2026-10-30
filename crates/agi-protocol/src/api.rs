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
    /// Stop the running turn.
    Cancel { session: SessionKey },
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
    /// The account's computers, with whether each is reachable now.
    AccountDesktops,
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

    /// The guide finished; do not show it again.
    OnboardingComplete,
    /// Show the guide again on the next snapshot.
    OnboardingReset,

    /// Liveness probe, used by the shell before it shows a window.
    Ping,
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
}
