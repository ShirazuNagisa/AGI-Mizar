//! The model seam.
//!
//! The kernel does not know what a "Mizarllm" is. It knows a [`ModelManifest`]:
//! a small JSON file that says where a model lives, what prompt dialect it
//! speaks, and — the part that keeps the two halves independent — which
//! [`SessionState`] code each of its labels means.
//!
//! ```json
//! {
//!   "id": "mizarllm-v1",
//!   "version": "1.0.0",
//!   "backend": { "kind": "http", "endpoint": "http://127.0.0.1:8791", "timeout_ms": 1500 },
//!   "labels": { "done": 11, "needs_user": 4, "stalled": 9, "waiting": 8 }
//! }
//! ```
//!
//! Upgrading the model is dropping a new file in the model directory and
//! flipping the active id. The kernel binary, the wire format and the phone are
//! all unchanged — including when the new model invents a label the old one did
//! not have, because the manifest says where it goes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use agi_protocol::SessionState;

use crate::{Facts, Judge, Layout, LlamaServerJudge, State};

/// Where the model runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Backend {
    /// A warm `llama-server` on loopback.
    Http {
        endpoint: String,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
        /// Name of an environment variable holding a bearer token, if any.
        #[serde(default)]
        api_key_env: Option<String>,
    },
    /// No model: the structural gate plus rules. Always available, never wrong
    /// about structure, occasionally wrong about semantics. Also the fallback
    /// when a manifest points at a sidecar that is not running.
    Rule,
}

fn default_timeout() -> u64 {
    1500
}

/// What the model calls each of the four verdicts, in state codes.
///
/// A manifest can point several labels at the same code (a future model may
/// split `waiting` into `subagent` and `background`, for instance) and can leave
/// one out, in which case that verdict falls back to the rule path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelMap {
    pub done: u8,
    pub needs_user: u8,
    pub stalled: u8,
    #[serde(default)]
    pub waiting: Option<u8>,
}

impl LabelMap {
    /// The state code for a verdict.
    fn code(&self, state: State) -> Option<SessionState> {
        let code = match state {
            State::Done => self.done,
            State::NeedsUser => self.needs_user,
            State::Stalled => self.stalled,
            State::Waiting => self.waiting?,
        };
        SessionState::from_code(code)
    }
}

/// One model, as described by its manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelManifest {
    pub id: String,
    pub version: String,
    pub backend: Backend,
    pub labels: LabelMap,
    /// Free-form note shown in the desktop UI. Not used by the kernel.
    #[serde(default)]
    pub note: String,
}

/// Why a manifest was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// The file could not be read.
    Read(String),
    /// The file is not a manifest.
    Parse(String),
    /// A code in `labels` is not one this build knows.
    UnknownStateCode(u8),
    /// `id` is empty, or two manifests claim the same id.
    BadId(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::Read(e) => write!(f, "cannot read manifest: {e}"),
            ManifestError::Parse(e) => write!(f, "not a model manifest: {e}"),
            ManifestError::UnknownStateCode(c) => {
                write!(f, "label maps to unknown state code {c}")
            }
            ManifestError::BadId(id) => write!(f, "bad model id {id:?}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl ModelManifest {
    /// Parse a manifest and check it against this build's state table.
    pub fn parse(raw: &str) -> Result<Self, ManifestError> {
        let manifest: ModelManifest =
            serde_json::from_str(raw).map_err(|e| ManifestError::Parse(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Load a manifest from disk.
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let raw = std::fs::read_to_string(path).map_err(|e| ManifestError::Read(e.to_string()))?;
        Self::parse(&raw)
    }

    /// Reject a manifest that this build cannot honour.
    ///
    /// Unknown state codes are the interesting case: a manifest from a newer
    /// release can name a state this build predates, and the honest answer is to
    /// refuse the model rather than draw the wrong thing on the phone.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.id.is_empty() || !self.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ManifestError::BadId(self.id.clone()));
        }
        for code in [
            Some(self.labels.done),
            Some(self.labels.needs_user),
            Some(self.labels.stalled),
            self.labels.waiting,
        ]
        .into_iter()
        .flatten()
        {
            SessionState::from_code(code).ok_or(ManifestError::UnknownStateCode(code))?;
        }
        Ok(())
    }

    /// Build the judge this manifest describes.
    pub fn judge(&self) -> Arc<dyn Judge> {
        match &self.backend {
            Backend::Http {
                endpoint,
                timeout_ms: _,
                api_key_env,
            } => Arc::new(LlamaServerJudge {
                endpoint: endpoint.clone(),
                api_key: api_key_env
                    .as_ref()
                    .and_then(|name| std::env::var(name).ok()),
            }),
            Backend::Rule => Arc::new(crate::RuleJudge),
        }
    }

    /// Translate a verdict into a state code.
    pub fn state_code(&self, state: State) -> Option<SessionState> {
        self.labels.code(state)
    }
}

/// The models this install can run, and which one is active.
///
/// The directory is read on every `list`, so a model dropped in while the app is
/// running shows up without a restart; only `activate` changes what the kernel
/// calls.
#[derive(Clone)]
pub struct ModelRegistry {
    dir: PathBuf,
    active: Arc<RwLock<Option<ModelManifest>>>,
}

impl ModelRegistry {
    /// Open a registry over a directory. Missing directories are not an error:
    /// the first install has no models.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            active: Arc::new(RwLock::new(None)),
        }
    }

    /// The directory being watched.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every valid manifest in the directory, sorted by id.
    ///
    /// An invalid file is skipped, not fatal: one bad manifest must not hide the
    /// working model next to it.
    pub fn list(&self) -> Vec<ModelManifest> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut found: Vec<ModelManifest> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|p| ModelManifest::load(&p).ok())
            .collect();
        found.sort_by(|a, b| a.id.cmp(&b.id));
        found.dedup_by(|a, b| a.id == b.id);
        found
    }

    /// Make a model the one the kernel calls. Returns the manifest it activated.
    pub fn activate(&self, id: &str) -> Result<ModelManifest, ManifestError> {
        let manifest = self
            .list()
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| ManifestError::BadId(id.to_string()))?;
        *self.active.write().unwrap_or_else(|e| e.into_inner()) = Some(manifest.clone());
        Ok(manifest)
    }

    /// The active manifest, if any.
    pub fn active(&self) -> Option<ModelManifest> {
        self.active
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Activate whatever `MIZAR_MODEL` names, else the only manifest present,
    /// else nothing. Called once at launch.
    pub fn activate_from_env(&self) -> Option<ModelManifest> {
        if let Ok(id) = std::env::var("MIZAR_MODEL") {
            if let Ok(m) = self.activate(&id) {
                return Some(m);
            }
        }
        let all = self.list();
        if all.len() == 1 {
            return self.activate(&all[0].id).ok();
        }
        None
    }
}

/// A judge driven by a manifest: the model answers in labels, the manifest says
/// which state code each label is.
pub struct ManifestJudge {
    manifest: ModelManifest,
    inner: Arc<dyn Judge>,
}

impl ManifestJudge {
    pub fn new(manifest: ModelManifest) -> Self {
        let inner = manifest.judge();
        Self { manifest, inner }
    }

    /// The manifest this judge was built from.
    pub fn manifest(&self) -> &ModelManifest {
        &self.manifest
    }
}

#[async_trait]
impl Judge for ManifestJudge {
    async fn judge(&self, facts: &Facts) -> Option<State> {
        self.inner.judge(facts).await
    }
}

/// Judge a turn and return both the verdict and the state code it maps to.
///
/// This is the one place where "what the model says" becomes "what the phone
/// shows", and the manifest is what sits between them.
pub async fn judge_turn(
    manifest: &ModelManifest,
    judge: &Arc<dyn Judge>,
    turn: Option<u32>,
    facts: &Facts,
) -> (Layout, Option<SessionState>) {
    let layout = crate::LayoutService::new(judge.clone())
        .judge_turn(turn, facts)
        .await;
    let code = manifest.state_code(layout.state);
    (layout, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json() -> &'static str {
        r#"{
            "id": "mizarllm-v1",
            "version": "1.0.0",
            "backend": { "kind": "http", "endpoint": "http://127.0.0.1:8791" },
            "labels": { "done": 11, "needs_user": 4, "stalled": 9, "waiting": 8 }
        }"#
    }

    #[test]
    fn a_manifest_maps_labels_onto_state_codes() {
        let m = ModelManifest::parse(manifest_json()).unwrap();
        assert_eq!(m.state_code(State::Done), Some(SessionState::Completed));
        assert_eq!(m.state_code(State::NeedsUser), Some(SessionState::AwaitingUser));
        assert_eq!(m.state_code(State::Stalled), Some(SessionState::Failed));
        assert_eq!(m.state_code(State::Waiting), Some(SessionState::Background));
    }

    #[test]
    fn a_label_pointing_at_a_state_this_build_lacks_is_refused() {
        let raw = manifest_json().replace("\"done\": 11", "\"done\": 200");
        let err = ModelManifest::parse(&raw).unwrap_err();
        assert_eq!(err, ManifestError::UnknownStateCode(200));
    }

    #[test]
    fn waiting_may_be_omitted() {
        let raw = manifest_json().replace(", \"waiting\": 8", "");
        let m = ModelManifest::parse(&raw).unwrap();
        assert_eq!(m.state_code(State::Waiting), None);
    }

    #[test]
    fn the_rule_backend_needs_no_sidecar() {
        let raw = manifest_json().replace(
            r#"{ "kind": "http", "endpoint": "http://127.0.0.1:8791" }"#,
            r#"{ "kind": "rule" }"#,
        );
        let m = ModelManifest::parse(&raw).unwrap();
        assert!(matches!(m.backend, Backend::Rule));
    }

    #[test]
    fn the_registry_activates_a_model_and_ignores_a_broken_neighbour() {
        let dir = std::env::temp_dir().join(format!("mizar-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), manifest_json()).unwrap();
        std::fs::write(dir.join("broken.json"), "{ not json").unwrap();

        let registry = ModelRegistry::open(&dir);
        let list = registry.list();
        assert_eq!(list.len(), 1, "the broken file must be skipped");
        assert_eq!(list[0].id, "mizarllm-v1");

        assert!(registry.active().is_none());
        let active = registry.activate("mizarllm-v1").unwrap();
        assert_eq!(active.version, "1.0.0");
        assert_eq!(registry.active().unwrap().id, "mizarllm-v1");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
