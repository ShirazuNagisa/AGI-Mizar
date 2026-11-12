//! Where the model comes from, and the process that runs it.
//!
//! Three things live here because they only make sense together:
//!
//! - **the directory** — one place on disk where the installed model lives, so
//!   "which model am I running" has exactly one answer;
//! - **the downloader** — resumable, checksummed, atomic, because a 1 GB file
//!   over a home connection is interrupted more often than not;
//! - **the sidecar** — `llama-server`, shipped in the app, started lazily and
//!   killed with the process that started it.
//!
//! Nothing here talks to the window directly: state changes go out as
//! [`ModelEvent`]s and the kernel turns those into API events.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use agi_layout::model::{ModelManifest, ModelRegistry};
use agi_protocol::api::{
    DownloadView, InstalledModel, ModelCheckView, ModelView, RemoteModel, RuntimeView,
};
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

/// The release host the model has always come from.
///
/// `releases/latest/download/<asset>` is a CDN redirect: no API call, no token,
/// no hourly quota. One file, and the version is in it.
const DEFAULT_RELEASE: &str = "https://github.com/ShirazuNagisa/Mizarllm";

/// The hub that carries the same bytes, because the release CDN is ~10 KB/s
/// from mainland China and this one measured ~13 MiB/s from the same machine.
/// See `docs/desktop/agi-mizar-model-download-acceleration-2026-11-12.md`.
const DEFAULT_HUB: &str = "https://modelscope.cn/models/Sh1razu/Mizarllm";

/// Point the release download at another host with the same layout
/// (`<base>/releases/download/<tag>/<file>`), e.g. a self-hosted mirror.
const REPO_ENV: &str = "MIZAR_MODEL_REPO";

/// Keep only one named source (`modelscope`, `github`), for support and for a
/// machine whose network reaches exactly one of them.
const SOURCE_ENV: &str = "MIZAR_MODEL_SOURCE";

/// How long one source gets to start sending the file before the next one is
/// tried.
///
/// A stalled connection from mainland China to the release CDN costs the
/// client's 60 s read timeout before anything happens; five seconds is long
/// enough for a healthy host and short enough that the fallback is worth
/// having.
const SOURCE_START_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a source gets to answer with its manifest.
///
/// Longer than the file start, because the manifest is a kilobyte and the
/// sources are asked in parallel: the check can afford to wait for the slow one
/// (the release CDN measured 5.5 s to first byte from Shanghai) where a
/// download cannot.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(8);

/// One place the same model can be fetched from.
///
/// Two URL shapes cover every source we ship, and they differ only in where the
/// version appears:
///
/// - **release**: `<base>/releases/download/<tag>/<file>` — one directory per
///   version, which is what GitHub gives us;
/// - **hub**: `.../resolve/<branch>/<file>` — one branch overwritten per
///   version, which is what ModelScope and Hugging Face give us.
#[derive(Debug, Clone)]
pub struct Source {
    /// Shown in the UI and in errors: "ModelScope", "GitHub".
    pub name: String,
    /// Where this source describes its newest release.
    pub manifest: String,
    /// Where this source keeps the files. `{version}` and `{file}` are
    /// substituted; a hub ignores the version because its branch moves.
    pub asset: String,
}

impl Source {
    /// A host with one directory per release (GitHub, or a mirror of it).
    pub fn release(name: &str, base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            name: name.to_string(),
            manifest: format!("{base}/releases/latest/download/manifest.json"),
            asset: format!("{base}/releases/download/{{version}}/{{file}}"),
        }
    }

    /// A hub with one branch that is overwritten (ModelScope, Hugging Face).
    pub fn hub(name: &str, base: &str, branch: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            name: name.to_string(),
            manifest: format!("{base}/resolve/{branch}/manifest.json"),
            asset: format!("{base}/resolve/{branch}/{{file}}"),
        }
    }

    /// The URL of one release's file on this source.
    pub fn url_for(&self, version: &str, file: &str) -> String {
        self.asset
            .replace("{version}", version)
            .replace("{file}", file)
    }
}

/// The sources this install tries, in order.
///
/// ModelScope first: it is the one that is fast from China, and a mirror that
/// answers in a minute is worth more than a release host that answers in a day.
/// GitHub second, because it is the canonical copy and it is fast elsewhere.
/// `MIZAR_MODEL_SOURCE` pins one; `MIZAR_MODEL_REPO` moves the release host.
fn configured_sources() -> Vec<Source> {
    let release_base = std::env::var(REPO_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE.to_string());

    let sources = vec![
        Source::hub("ModelScope", DEFAULT_HUB, "master"),
        Source::release("GitHub", &release_base),
    ];

    let Ok(only) = std::env::var(SOURCE_ENV) else {
        return sources;
    };
    let only = only.trim().to_ascii_lowercase();
    let filtered: Vec<Source> = sources
        .iter()
        .filter(|source| source.name.to_ascii_lowercase() == only)
        .cloned()
        .collect();
    if filtered.is_empty() {
        // A typo must not leave the machine with no way to get a model.
        tracing::warn!("model: {SOURCE_ENV}={only} names no source; trying all of them");
        return sources;
    }
    filtered
}

/// How often download progress is published.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// How long `llama-server` gets to answer `/health` before the start is a failure.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(20);

/// How long one judgement may take before the rule fallback takes over.
///
/// The judge prompt is ~790 tokens. Measured on the pinned b12000 build
/// (2026-11-02, Windows): prompt eval ~5.0 s on the CPU build (`-t 4`) and
/// ~0.2 s with CUDA (`-ngl 99`). The old 1.8 s therefore timed out every CPU
/// judgement and the model silently never decided anything.
const DEFAULT_JUDGE_TIMEOUT: Duration = Duration::from_millis(10_000);

/// Environment variable overriding [`DEFAULT_JUDGE_TIMEOUT`] (milliseconds).
const JUDGE_TIMEOUT_ENV: &str = "MIZAR_JUDGE_TIMEOUT_MS";

/// The judge timeout for this process: `MIZAR_JUDGE_TIMEOUT_MS` when it holds a
/// positive integer, else ten seconds.
fn judge_timeout() -> Duration {
    judge_timeout_from(std::env::var(JUDGE_TIMEOUT_ENV).ok().as_deref())
}

/// Parse the override. Junk and zero keep the default instead of producing a
/// 0 ms timeout that would turn every judgement into a rule fallback.
fn judge_timeout_from(raw: Option<&str>) -> Duration {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_JUDGE_TIMEOUT)
}

/// A change worth telling the window about.
#[derive(Debug, Clone)]
pub enum ModelEvent {
    /// A download moved.
    Progress(DownloadView),
    /// The installed set, the active model, or the sidecar changed.
    Changed(Box<ModelView>),
}

/// What the repository's manifest says, plus the bits only the downloader needs.
///
/// It is a superset of [`ModelManifest`]: the kernel ignores the extra fields,
/// the downloader does not.
#[derive(Debug, Clone, Deserialize)]
struct RemoteManifest {
    #[serde(flatten)]
    manifest: ModelManifest,
    /// Asset filename inside the release.
    file: String,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    size_bytes: Option<u64>,
}

/// A download in flight.
struct Download {
    view: Mutex<DownloadView>,
    cancelled: Arc<AtomicBool>,
}

impl Download {
    fn snapshot(&self) -> DownloadView {
        self.view
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn phase(&self, state: &str) {
        let mut view = self
            .view
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        view.state = state.to_string();
    }

    fn progress(&self, done: u64, total: u64) {
        let mut view = self
            .view
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        view.done = done;
        if total > 0 {
            view.total = total;
        }
    }

    fn fail(&self, error: &str) {
        let mut view = self
            .view
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        view.state = "failed".to_string();
        view.error = Some(error.to_string());
    }

    /// Name the source this attempt reads from, so a slow download can say
    /// *which* host is slow.
    fn use_source(&self, name: &str) {
        let mut view = self
            .view
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        view.source = Some(name.to_string());
    }
}

/// One source's answer to "what is the newest release", or why there is none.
struct Answer {
    source: Source,
    manifest: Result<RemoteManifest, String>,
}

/// A manifest that won, and the source that offered it.
struct Found {
    source: Source,
    remote: RemoteManifest,
}

/// Whether two sources describe the same bytes for the same version.
///
/// A mirror that disagrees about the checksum is stale or wrong either way; the
/// first one is tried and verified, and the other stays a fallback.
fn same_release(left: &RemoteManifest, right: &RemoteManifest) -> bool {
    left.file == right.file
        && match (&left.sha256, &right.sha256) {
            (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
            _ => true,
        }
}

/// Why a downloaded file did not become the model.
///
/// The distinction matters across sources: bytes that fail the manifest's
/// checksum are the wrong model and the partial file must go, while a rename
/// that failed leaves a perfectly good gigabyte worth resuming.
enum InstallError {
    Checksum(String),
    Other(String),
}

impl InstallError {
    fn message(&self) -> &str {
        match self {
            Self::Checksum(message) => message,
            Self::Other(message) => message,
        }
    }
}

/// Read one source's manifest, giving up fast enough that a dead host does not
/// hold the whole check hostage.
async fn fetch_manifest(http: &reqwest::Client, url: &str) -> Result<RemoteManifest, String> {
    let response = tokio::time::timeout(MANIFEST_TIMEOUT, http.get(url).send())
        .await
        .map_err(|_| format!("no answer in {} s", MANIFEST_TIMEOUT.as_secs()))?
        .map_err(|error| format!("could not reach it: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("it answered {}", response.status().as_u16()));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("could not read the manifest: {error}"))?;
    serde_json::from_slice::<RemoteManifest>(&bytes)
        .map_err(|error| format!("its manifest is not one this build understands: {error}"))
}

/// The model directory, the downloader, and the sidecar.
pub struct ModelService {
    dir: PathBuf,
    active: RwLock<Option<ModelManifest>>,
    download: Mutex<Option<Arc<Download>>>,
    runtime: Arc<Runtime>,
    events: broadcast::Sender<ModelEvent>,
    http: reqwest::Client,
    /// Where releases come from, in the order they are tried. Tests set this to
    /// their own servers; a release gets it from `configured_sources`. A field
    /// rather than asking the environment per call, for the same reason the
    /// tests needed one: two tests in one process would overwrite each other.
    sources: RwLock<Vec<Source>>,
}

impl ModelService {
    /// Open the model directory and adopt whatever is already installed.
    ///
    /// A directory that does not exist is not an error: it is the machine that
    /// has not downloaded anything yet.
    pub fn open(dir: PathBuf) -> Arc<Self> {
        let registry = ModelRegistry::open(&dir);
        let mut active = registry.activate_from_env();
        if active.is_none() {
            // A directory that holds exactly one model has already answered the
            // question "which one"; the env var only overrides it.
            let installed = registry.list();
            if installed.len() == 1 {
                active = installed.into_iter().next();
            }
        }
        let (events, _) = broadcast::channel(64);
        let pid_file = dir.join(".runtime.pid");
        let service = Arc::new(Self {
            dir,
            active: RwLock::new(active),
            download: Mutex::new(None),
            runtime: Arc::new(Runtime::new(pid_file)),
            events,
            // `timeout` is the whole request, body included: on a 1 GB download
            // it fires at 60 s and the stream dies a third of the way through.
            // `read_timeout` is the silence between bytes, which is the thing
            // worth failing on.
            //
            // The user agent is not decoration: ModelScope's edge answers 403 to
            // a request without one, which reads exactly like "the mirror does
            // not have the weights" until a live test says otherwise.
            http: reqwest::Client::builder()
                .user_agent(concat!("AGI-Mizar/", env!("CARGO_PKG_VERSION")))
                .read_timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            sources: RwLock::new(configured_sources()),
        });
        // Before anything can start a new one: stop the one a dead launch left.
        service.sweep_stale_runtime();
        service
    }

    /// The directory models are installed into.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Follow state changes.
    pub fn subscribe(&self) -> broadcast::Receiver<ModelEvent> {
        self.events.subscribe()
    }

    fn publish(&self, event: ModelEvent) {
        let _ = self.events.send(event);
    }

    /// The manifest the kernel should judge with.
    pub fn manifest(&self) -> Option<ModelManifest> {
        self.active
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Stop the sidecar but keep the installed model.
    ///
    /// The kernel calls this when nobody is listening any more: a phone that
    /// went away is the only reason to have the judge at all, and a
    /// `llama-server` holding several hundred megabytes for a desktop nobody is
    /// watching is the memory the lease exists to give back. The next verdict,
    /// if one is ever asked for, restarts it through `ensure_endpoint`.
    pub async fn stop_sidecar(&self) {
        self.runtime.stop().await;
    }

    /// One model's weights, by the convention that keeps the directory readable:
    /// `<id>.gguf` beside `<id>.json`.
    pub fn weights_of(&self, id: &str, _previous: bool) -> PathBuf {
        self.dir.join(format!("{id}.gguf"))
    }

    /// Where a model that is not the active one keeps its weights.
    ///
    /// The version is part of the name so that "replace the current model" and
    /// "keep the old one aside" can never be the same path: a rename whose
    /// source and target are equal is how a rollback silently became a no-op.
    fn previous_weights(&self, manifest: &ModelManifest) -> PathBuf {
        self.dir
            .join(".previous")
            .join(format!("{}-{}.gguf", manifest.id, manifest.version))
    }

    fn installed_from(&self, manifest: &ModelManifest, previous: bool) -> Option<InstalledModel> {
        let path = if previous {
            self.previous_weights(manifest)
        } else {
            self.weights_of(&manifest.id, false)
        };
        let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        path.exists().then(|| InstalledModel {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            path: path.to_string_lossy().to_string(),
            size_bytes,
        })
    }

    /// Everything the window needs to draw the model block.
    /// Stop a sidecar left behind by a launch that died without cleaning up.
    ///
    /// `kill_on_drop` runs when the process ends normally. A force quit, a
    /// crash or `kill -9` leaves the child reparented to launchd, holding a port
    /// and a mapped gigabyte — one per abandoned launch.
    pub fn sweep_stale_runtime(&self) {
        let path = self.dir.join(".runtime.pid");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(pid) = text.trim().parse::<u32>() else {
            let _ = std::fs::remove_file(&path);
            return;
        };
        // Only a process that still calls itself llama-server: pids are reused,
        // and killing a stranger because it inherited a number is worse than
        // leaking a child.
        if stale_runtime_is_ours(pid) {
            stop_stale_runtime(pid);
            tracing::info!("model: stopped a sidecar left over from a previous launch ({pid})");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The sources this install tries, in order.
    fn sources(&self) -> Vec<Source> {
        self.sources
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Point releases at one local server. For tests: leaving the shipped
    /// sources in place would make a test reach the real ModelScope and GitHub.
    pub fn point_at(&self, base: impl Into<String>) {
        let base = base.into();
        self.route_sources(vec![Source::release("test", &base)]);
    }

    /// Use exactly these sources, in this order. For tests, and for an install
    /// that must not touch a host it did not ask for.
    pub fn route_sources(&self, sources: Vec<Source>) {
        *self
            .sources
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = sources;
    }

    pub fn state(&self) -> ModelView {
        let manifest = self.manifest();
        let active = manifest
            .as_ref()
            .and_then(|manifest| self.installed_from(manifest, false));
        let installed = active.iter().cloned().collect();
        let previous = self.read_previous_manifest().and_then(|manifest| {
            let model = self.installed_from(&manifest, true)?;
            Some(model)
        });

        ModelView {
            installed,
            active,
            previous,
            runtime: self.runtime.view(),
            download: self
                .download
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .map(|download| download.snapshot()),
        }
    }

    fn previous_manifest_path(&self) -> PathBuf {
        self.dir.join(".previous").join("manifest.json")
    }

    fn read_previous_manifest(&self) -> Option<ModelManifest> {
        let text = std::fs::read_to_string(self.previous_manifest_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Ask every source what the newest release is.
    ///
    /// The version is the maximum across sources, not the first answer: a mirror
    /// that was forgotten after an upload must not pin every client to the old
    /// model. A source that is down costs one line in the error message, which
    /// is the normal case this exists for.
    pub async fn check(&self) -> ModelCheckView {
        let installed = self.manifest().map(|m| m.version);
        match self.newest().await {
            Ok(found) => {
                let newer = installed
                    .as_deref()
                    .map(|current| is_newer(&found.remote.manifest.version, current))
                    .unwrap_or(true);
                ModelCheckView {
                    installed,
                    latest: Some(RemoteModel {
                        id: found.remote.manifest.id.clone(),
                        version: found.remote.manifest.version.clone(),
                        url: found
                            .source
                            .url_for(&found.remote.manifest.version, &found.remote.file),
                        sha256: found.remote.sha256.clone(),
                        size_bytes: found.remote.size_bytes,
                        newer,
                    }),
                    error: None,
                }
            }
            Err(error) => ModelCheckView {
                installed,
                latest: None,
                error: Some(error),
            },
        }
    }

    /// The newest manifest any source offers, with the source that offered it.
    async fn newest(&self) -> Result<Found, String> {
        let answers = self.ask_sources().await;
        let mut best: Option<Found> = None;
        let mut failures = Vec::new();
        for answer in answers {
            match answer.manifest {
                Ok(remote) => {
                    let newer = best.as_ref().is_none_or(|found| {
                        is_newer(&remote.manifest.version, &found.remote.manifest.version)
                    });
                    if newer {
                        best = Some(Found {
                            source: answer.source,
                            remote,
                        });
                    }
                }
                Err(error) => failures.push(format!("{}: {error}", answer.source.name)),
            }
        }
        best.ok_or_else(|| format!("no source answered ({})", failures.join("; ")))
    }

    /// Ask every source at once. Parallel because each answer is a kilobyte and
    /// a serial check would cost the sum of every fallback.
    async fn ask_sources(&self) -> Vec<Answer> {
        let requests = self.sources().into_iter().map(|source| {
            let http = self.http.clone();
            async move {
                let manifest = fetch_manifest(&http, &source.manifest).await;
                Answer { source, manifest }
            }
        });
        futures_util::future::join_all(requests).await
    }

    /// Record a failure the window can read, and hand the message back.
    fn give_up(&self, download: &Arc<Download>, error: String) -> String {
        download.fail(&error);
        self.publish(ModelEvent::Progress(download.snapshot()));
        error
    }

    /// Start downloading the newest release (or a named one).
    ///
    /// Returns as soon as the download is accepted; progress arrives on the
    /// event channel. A second call while one is running is refused, not queued:
    /// two writers to one file is how a download gets corrupted.
    pub fn start_download(self: &Arc<Self>, want: Option<String>) -> Result<String, String> {
        let claimed = Arc::new(Download {
            view: Mutex::new(DownloadView {
                version: want.clone().unwrap_or_else(|| "latest".to_string()),
                done: 0,
                total: 0,
                state: "starting".to_string(),
                error: None,
                source: None,
            }),
            cancelled: Arc::new(AtomicBool::new(false)),
        });

        {
            let mut slot = self
                .download
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slot.as_ref() {
                // A failure is kept on screen until the next attempt; a failure
                // that vanishes before it is read is one nobody can act on.
                Some(previous) if previous.snapshot().state == "failed" => {}
                Some(_) => return Err("a download is already running".to_string()),
                None => {}
            }
            // Claimed here, not inside the task: two tasks writing one `.part`
            // is how a resume corrupts itself.
            *slot = Some(Arc::clone(&claimed));
        }

        let service = Arc::clone(self);
        let wanted = want.clone();
        tokio::spawn(async move {
            if let Err(error) = service.download(&wanted, claimed.clone()).await {
                tracing::warn!("model download failed: {error}");
            }
        });
        Ok(want.unwrap_or_else(|| "latest".to_string()))
    }

    async fn download(
        self: Arc<Self>,
        want: &Option<String>,
        download: Arc<Download>,
    ) -> Result<(), String> {
        let mut offered: Vec<(Source, RemoteManifest)> = Vec::new();
        let mut failures = Vec::new();
        for answer in self.ask_sources().await {
            match answer.manifest {
                Ok(remote) => offered.push((answer.source, remote)),
                Err(error) => failures.push(format!("{}: {error}", answer.source.name)),
            }
        }
        if offered.is_empty() {
            // The frame already exists, so the window sees the reason instead of
            // a button that appears to do nothing.
            return Err(self.give_up(
                &download,
                format!("no source answered ({})", failures.join("; ")),
            ));
        }

        let chosen = match want.as_deref() {
            Some(want) => match offered
                .iter()
                .find(|(_, remote)| remote.manifest.version == want)
            {
                Some((_, remote)) => remote.clone(),
                None => {
                    let mut newest = &offered[0].1;
                    for (_, remote) in &offered {
                        if is_newer(&remote.manifest.version, &newest.manifest.version) {
                            newest = remote;
                        }
                    }
                    return Err(self.give_up(
                        &download,
                        format!(
                            "no source offers {want} (the newest is {})",
                            newest.manifest.version
                        ),
                    ));
                }
            },
            None => {
                let mut newest = offered[0].1.clone();
                for (_, remote) in &offered {
                    if is_newer(&remote.manifest.version, &newest.manifest.version) {
                        newest = remote.clone();
                    }
                }
                newest
            }
        };

        // Every source that advertises that version with the same bytes, in the
        // order they are configured: the first one is the one we want, the rest
        // are what a failure falls back to.
        let version = chosen.manifest.version.clone();
        let candidates: Vec<(Source, RemoteManifest)> = offered
            .into_iter()
            .filter(|(_, remote)| {
                remote.manifest.version == version && same_release(remote, &chosen)
            })
            .collect();

        let temp_dir = self.dir.join(".download");
        std::fs::create_dir_all(&temp_dir).map_err(|error| {
            let message = format!("could not create {}: {error}", temp_dir.display());
            self.give_up(&download, message)
        })?;
        let part_path = temp_dir.join(format!("{}.part", chosen.file));

        let total = chosen.size_bytes.unwrap_or(0);
        {
            let mut view = download
                .view
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            view.version = version.clone();
            view.total = total;
            view.state = "running".to_string();
        }
        self.publish(ModelEvent::Progress(download.snapshot()));

        let mut last_error = String::new();
        for (source, remote) in candidates {
            if download.cancelled.load(Ordering::Relaxed) {
                return Err("cancelled".to_string());
            }
            let url = source.url_for(&remote.manifest.version, &remote.file);
            download.phase("running");
            download.use_source(&source.name);
            self.publish(ModelEvent::Progress(download.snapshot()));

            match self
                .fetch_to(Arc::clone(&download), &url, &part_path, total)
                .await
            {
                Ok(()) => {
                    download.phase("verifying");
                    self.publish(ModelEvent::Progress(download.snapshot()));
                    match self.install(&remote, &download, &part_path).await {
                        Ok(()) => {
                            self.clear_download();
                            self.publish(ModelEvent::Changed(Box::new(self.state())));
                            return Ok(());
                        }
                        Err(error) => {
                            // Bytes that fail the manifest's checksum are not a
                            // head start, they are the wrong model: the next
                            // source has to start clean. Anything else keeps the
                            // file, because the download itself was fine.
                            if matches!(error, InstallError::Checksum(_)) {
                                let _ = std::fs::remove_file(&part_path);
                            }
                            last_error = format!("{}: {}", source.name, error.message());
                        }
                    }
                }
                Err(error) if error == "cancelled" => return Err(error),
                Err(error) => last_error = format!("{}: {error}", source.name),
            }
            tracing::warn!("model: {last_error}; trying the next source");
        }

        // The frame stays so the window can draw the reason; the next attempt
        // replaces it.
        Err(self.give_up(&download, format!("every source failed ({last_error})")))
    }

    fn clear_download(&self) {
        let mut slot = self
            .download
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = None;
    }

    /// Stream one file to `part_path`, resuming when a partial exists.
    async fn fetch_to(
        &self,
        download: Arc<Download>,
        url: &str,
        part_path: &Path,
        total: u64,
    ) -> Result<(), String> {
        let done = std::fs::metadata(part_path).map(|m| m.len()).unwrap_or(0);
        if done > 0 && total > 0 && done >= total {
            // A finished `.part` from a previous run: nothing to fetch.
            return Ok(());
        }

        let mut request = self.http.get(url);
        if done > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={done}-"));
        }
        let response = tokio::time::timeout(SOURCE_START_TIMEOUT, request.send())
            .await
            .map_err(|_| format!("no answer in {} s", SOURCE_START_TIMEOUT.as_secs()))?
            .map_err(|error| format!("could not reach the download: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("the download answered {}", status.as_u16()));
        }
        // A server that ignores Range answers 200 with the whole file; keeping the
        // partial would then duplicate the first bytes.
        let mut done = if status == reqwest::StatusCode::PARTIAL_CONTENT {
            done
        } else {
            0
        };

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(done > 0)
            .truncate(done == 0)
            .open(part_path)
            .await
            .map_err(|error| format!("could not open {}: {error}", part_path.display()))?;

        let mut stream = response.bytes_stream();
        let mut last = std::time::Instant::now();
        while let Some(chunk) = stream.next().await {
            if download.cancelled.load(Ordering::Relaxed) {
                return Err("cancelled".to_string());
            }
            let chunk = chunk.map_err(|error| format!("download interrupted: {error}"))?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(|error| format!("could not write the download: {error}"))?;
            done += chunk.len() as u64;
            if last.elapsed() >= PROGRESS_INTERVAL {
                last = std::time::Instant::now();
                download.progress(done, total);
                self.publish(ModelEvent::Progress(download.snapshot()));
            }
        }

        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(|error| format!("could not flush the download: {error}"))?;
        download.progress(done, total);
        self.publish(ModelEvent::Progress(download.snapshot()));
        Ok(())
    }

    /// Verify, keep the old one aside, then move into place: the model appears
    /// whole or not at all.
    async fn install(
        &self,
        remote: &RemoteManifest,
        download: &Arc<Download>,
        part_path: &Path,
    ) -> Result<(), InstallError> {
        if let Some(expected) = remote.sha256.as_deref() {
            let actual = sha256_of(part_path).await.map_err(InstallError::Other)?;
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = std::fs::remove_file(part_path);
                return Err(InstallError::Checksum(format!(
                    "checksum mismatch: expected {expected}, got {actual}"
                )));
            }
        }

        let id = &remote.manifest.id;
        let current = self.weights_of(id, false);
        let current_manifest = self.dir.join(format!("{id}.json"));
        if current.exists() {
            let previous_dir = self.dir.join(".previous");
            std::fs::create_dir_all(&previous_dir).map_err(|error| {
                InstallError::Other(format!(
                    "could not create {}: {error}",
                    previous_dir.display()
                ))
            })?;
            // Name the outgoing weights after the version they *are*, not the
            // one they are about to be replaced by.
            match std::fs::read(&current_manifest)
                .ok()
                .and_then(|text| serde_json::from_slice::<ModelManifest>(&text).ok())
            {
                Some(outgoing) => {
                    let _ = std::fs::rename(&current, self.previous_weights(&outgoing));
                    let _ = std::fs::write(
                        self.previous_manifest_path(),
                        serde_json::to_vec_pretty(&outgoing).unwrap_or_default(),
                    );
                }
                None => {
                    let _ = std::fs::remove_file(&current);
                }
            }
        }

        std::fs::rename(part_path, &current).map_err(|error| {
            InstallError::Other(format!(
                "could not move the download into {}: {error}",
                current.display()
            ))
        })?;

        // The manifest goes in last: a half-written model must never look
        // installed just because its manifest is there.
        let text = serde_json::to_vec_pretty(&remote.manifest).map_err(|error| {
            InstallError::Other(format!("could not serialise the manifest: {error}"))
        })?;
        write_atomic(&current_manifest, &text).map_err(InstallError::Other)?;

        {
            let mut active = self
                .active
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *active = Some(remote.manifest.clone());
        }
        // A new model needs a new sidecar: the old one holds the old weights.
        self.runtime.stop().await;
        let _ = download;
        Ok(())
    }

    /// Stop the download in flight, keeping the partial file for a resume.
    pub fn cancel(&self) -> bool {
        let slot = self
            .download
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match slot.as_ref() {
            Some(download) => {
                download.cancelled.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Make an installed model the one the kernel calls.
    pub fn activate(&self, id: &str) -> Result<ModelManifest, String> {
        let manifest = self
            .manifest_of(id)
            .ok_or_else(|| format!("no installed model has id {id}"))?;
        if !self.weights_of(&manifest.id, false).exists() {
            return Err(format!(
                "{} names weights that are not installed",
                manifest.id
            ));
        }
        {
            let mut active = self
                .active
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *active = Some(manifest.clone());
        }
        // The process holds the old weights; a switch that keeps it running
        // judges the next turn with the model it just replaced.
        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move { runtime.stop().await });
        self.publish(ModelEvent::Changed(Box::new(self.state())));
        Ok(manifest)
    }

    fn manifest_of(&self, id: &str) -> Option<ModelManifest> {
        let text = std::fs::read_to_string(self.dir.join(format!("{id}.json"))).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Delete a model that is not the active one.
    ///
    /// Only the kept-aside previous version qualifies: the model in use is
    /// removed by activating another first, which keeps "the kernel always has a
    /// model it can name" true.
    pub fn remove(&self, version: &str) -> Result<(), String> {
        let previous = self
            .read_previous_manifest()
            .ok_or_else(|| "there is no previous model to remove".to_string())?;
        if previous.version != version {
            return Err("that model is in use; activate another before removing it".to_string());
        }
        self.remove_previous()
    }

    /// Put the previous model back, if there is one.
    pub fn rollback(&self) -> Result<ModelManifest, String> {
        let previous = self
            .read_previous_manifest()
            .ok_or_else(|| "there is no previous model to go back to".to_string())?;
        let current = self
            .manifest()
            .ok_or_else(|| "no model is active".to_string())?;

        let previous_dir = self.dir.join(".previous");
        std::fs::create_dir_all(&previous_dir)
            .map_err(|error| format!("could not create {}: {error}", previous_dir.display()))?;

        let previous_weights = self.previous_weights(&previous);
        if !previous_weights.exists() {
            return Err("the previous model's weights are gone".to_string());
        }
        let current_weights = self.weights_of(&current.id, false);
        let stash = previous_dir.join(format!("{}-{}.restore", previous.id, previous.version));

        // Out of the way first: a rename whose target is a live file destroys it.
        std::fs::rename(&previous_weights, &stash)
            .map_err(|error| format!("could not move the previous weights: {error}"))?;
        if current_weights.exists() {
            std::fs::rename(&current_weights, self.previous_weights(&current))
                .map_err(|error| format!("could not set the current weights aside: {error}"))?;
        }
        std::fs::rename(&stash, &current_weights)
            .map_err(|error| format!("could not restore the previous weights: {error}"))?;

        write_atomic(
            &self.dir.join(format!("{}.json", previous.id)),
            &serde_json::to_vec_pretty(&previous)
                .map_err(|error| format!("could not serialise the manifest: {error}"))?,
        )?;
        write_atomic(
            &self.previous_manifest_path(),
            &serde_json::to_vec_pretty(&current)
                .map_err(|error| format!("could not serialise the manifest: {error}"))?,
        )?;

        {
            let mut active = self
                .active
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *active = Some(previous.clone());
        }
        // The sidecar holds the model that was just replaced.
        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move { runtime.stop().await });
        self.publish(ModelEvent::Changed(Box::new(self.state())));
        Ok(previous)
    }

    /// Delete the previous model (the current one is never removable).
    pub fn remove_previous(&self) -> Result<(), String> {
        let previous = self
            .read_previous_manifest()
            .ok_or_else(|| "there is no previous model".to_string())?;
        let _ = std::fs::remove_file(self.previous_weights(&previous));
        let _ = std::fs::remove_file(self.previous_manifest_path());
        self.publish(ModelEvent::Changed(Box::new(self.state())));
        Ok(())
    }

    /// The loopback endpoint the judge should call, starting the sidecar if it
    /// is not up yet.
    ///
    /// The weights on disk are the only input: the port is chosen here, so a
    /// manifest shipped by somebody else cannot point the kernel somewhere
    /// unplanned.
    pub async fn ensure_endpoint(self: &Arc<Self>) -> Result<String, String> {
        let manifest = self
            .manifest()
            .ok_or_else(|| "no model is active".to_string())?;
        let weights = self.weights_of(&manifest.id, false);
        if !weights.exists() {
            return Err(format!("{} has no weights on disk", manifest.id));
        }
        let port = self.runtime.ensure(&weights).await?;
        Ok(format!("http://127.0.0.1:{port}"))
    }

    /// Stop the sidecar. Called on shutdown.
    pub async fn shutdown(&self) {
        self.runtime.stop().await;
    }
}

async fn sha256_of(path: &Path) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Write a file so a reader never sees half of it.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes)
        .map_err(|error| format!("could not write {}: {error}", temp.display()))?;
    std::fs::rename(&temp, path)
        .map_err(|error| format!("could not move {} into place: {error}", temp.display()))
}

/// Whether `candidate` is a later version than `current`.
///
/// Deliberately simple: dot-separated numbers, missing parts read as zero. A
/// lexicographic compare would call `1.10.0` older than `1.9.0`, which is the
/// one bug this must not have.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |text: &str| -> Vec<u64> {
        text.trim_start_matches('v')
            .split('.')
            .map(|part| {
                part.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u64>()
                    .unwrap_or(0)
            })
            .collect()
    };
    let mut candidate = parse(candidate);
    let mut current = parse(current);
    let length = candidate.len().max(current.len());
    candidate.resize(length, 0);
    current.resize(length, 0);
    candidate > current
}

/// The judge the kernel actually runs: ask the service where the sidecar is,
/// then ask the sidecar.
///
/// The manifest's `endpoint` field is a placeholder on purpose. A port chosen at
/// package time is wrong the moment two apps want the same one, and a manifest
/// downloaded from the internet must not be able to point the kernel at an
/// arbitrary address. The port lives in memory, here.
pub struct SidecarJudge {
    service: Arc<ModelService>,
}

impl SidecarJudge {
    pub fn new(service: Arc<ModelService>) -> Self {
        Self { service }
    }
}

#[async_trait::async_trait]
impl agi_layout::Judge for SidecarJudge {
    async fn judge(&self, facts: &agi_layout::Facts) -> Option<agi_layout::State> {
        let endpoint = self.service.ensure_endpoint().await.ok()?;
        let judge = agi_layout::LlamaServerJudge {
            endpoint,
            api_key: None,
        };
        // A sidecar that accepted the request and then hung is worse than one
        // that is down: without the timeout the turn waits forever.
        tokio::time::timeout(judge_timeout(), judge.judge(facts))
            .await
            .unwrap_or_default()
    }
}

/// The sidecar's file name on this platform.
///
/// Windows release archives and installers ship `llama-server.exe`; looking for
/// the bare name there never finds it.
fn llama_server_name() -> &'static str {
    if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    }
}

/// Whether the process holding `pid` is still the sidecar a dead launch left.
///
/// This is a pid-to-image-name query and nothing more: pids are reused, so the
/// name is the only thing that says this is safe to kill. Windows answers with
/// `tasklist`; the BSD-style systems answer with `/bin/ps`.
fn stale_runtime_is_ours(pid: u32) -> bool {
    #[cfg(windows)]
    let output = agi_platform::std_command("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output();
    #[cfg(not(windows))]
    let output = agi_platform::std_command("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    output
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("llama-server")
        })
        .unwrap_or(false)
}

/// Stop one process by pid, hard.
fn stop_stale_runtime(pid: u32) {
    #[cfg(windows)]
    let _ = agi_platform::std_command("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status();
    #[cfg(not(windows))]
    let _ = agi_platform::std_command("/bin/kill")
        .args(["-9", &pid.to_string()])
        .status();
}

/// The `llama-server` process, and where it is.
pub struct Runtime {
    process: tokio::sync::Mutex<Option<tokio::process::Child>>,
    /// Where the running sidecar's pid is written, so a launch that follows a
    /// crash can find and stop it.
    pid_file: PathBuf,
    view: RwLock<RuntimeView>,
    /// One start at a time: two turns ending together must not race two
    /// `llama-server` processes into the same port.
    starting: tokio::sync::Mutex<()>,
}

impl Runtime {
    fn new(pid_file: PathBuf) -> Self {
        Self {
            process: tokio::sync::Mutex::new(None),
            pid_file,
            view: RwLock::new(RuntimeView::default()),
            starting: tokio::sync::Mutex::new(()),
        }
    }

    fn view(&self) -> RuntimeView {
        self.view
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set(&self, view: RuntimeView) {
        *self
            .view
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = view;
    }

    /// Find the binary: an explicit override, then beside the app, then `PATH`.
    ///
    /// The bundle path is what ships; `PATH` is what a developer has. The name
    /// carries `.exe` on Windows, and the PATH lookup goes through the platform
    /// seam so `PATHEXT` is honoured there — the old branch split `PATH` on `:`
    /// and looked for an extension-less name, which finds nothing on Windows.
    pub fn binary(&self) -> Option<PathBuf> {
        if let Ok(explicit) = std::env::var("AGI_LLAMA_BIN") {
            let path = PathBuf::from(explicit);
            if path.exists() {
                return Some(path);
            }
        }
        let name = llama_server_name();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(directory) = exe.parent() {
                // Beside the executable (a macOS bundle, an NSIS install) or in
                // the `bin/` directory a package manager or release archive uses.
                let beside = directory.join(name);
                if beside.is_file() {
                    return Some(beside);
                }
                let in_bin = directory.join("bin").join(name);
                if in_bin.is_file() {
                    return Some(in_bin);
                }
                // AGI Mizar.app/Contents/MacOS/agi-mizar -> Contents/Resources/bin/
                #[cfg(target_os = "macos")]
                if let Some(contents) = directory.parent() {
                    let bundled = contents.join("Resources/bin/llama-server");
                    if bundled.exists() {
                        return Some(bundled);
                    }
                }
            }
        }
        agi_platform::which("llama-server")
    }

    /// Start the sidecar if it is not already answering, and hand back its port.
    pub async fn ensure(&self, weights: &Path) -> Result<u16, String> {
        let _one_at_a_time = self.starting.lock().await;

        // Ready means "a child is still running and answering". A crashed
        // sidecar must not leave the kernel believing it is warm.
        if self.view().state == "ready" {
            let mut process = self.process.lock().await;
            let alive = match process.as_mut() {
                Some(child) => matches!(child.try_wait(), Ok(None)),
                None => false,
            };
            if alive {
                if let Some(port) = self.view().port {
                    return Ok(port);
                }
            }
            let _ = process.take();
            self.set(RuntimeView::default());
        }

        let binary = self
            .binary()
            .ok_or_else(|| "llama-server was not found in the app bundle or on PATH".to_string())?;

        self.set(RuntimeView {
            state: "starting".to_string(),
            port: None,
            detail: Some(binary.to_string_lossy().to_string()),
        });

        let port = free_port().await?;
        let child = agi_platform::command(&binary)
            .arg("-m")
            .arg(weights)
            .arg("--port")
            .arg(port.to_string())
            .arg("--ctx-size")
            .arg("4096")
            .arg("-t")
            .arg("4")
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", binary.to_string_lossy()))?;
        if let Some(pid) = child.id() {
            let _ = std::fs::write(&self.pid_file, pid.to_string());
        }
        *self.process.lock().await = Some(child);

        match wait_for_health(port).await {
            Ok(()) => {
                self.set(RuntimeView {
                    state: "ready".to_string(),
                    port: Some(port),
                    detail: Some(binary.to_string_lossy().to_string()),
                });
                Ok(port)
            }
            Err(error) => {
                self.stop().await;
                self.set(RuntimeView {
                    state: "failed".to_string(),
                    port: None,
                    detail: Some(error.clone()),
                });
                Err(error)
            }
        }
    }

    /// Stop the sidecar and wait for it, so the port is really free afterwards.
    pub async fn stop(&self) {
        if let Some(mut child) = self.process.lock().await.take() {
            let _ = child.kill().await;
        }
        let _ = std::fs::remove_file(&self.pid_file);
        let mut view = self.view();
        view.state = "stopped".to_string();
        view.port = None;
        self.set(view);
    }
}

/// A port nothing is listening on right now.
///
/// Bind-then-drop: the kernel chooses a port for a child process it starts
/// immediately. The alternative — let the child pick and read it out of stdout —
/// means parsing a log format that is not a contract.
async fn free_port() -> Result<u16, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("no loopback port available: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("could not read the bound port: {error}"))?
        .port();
    drop(listener);
    Ok(port)
}

async fn wait_for_health(port: u16) -> Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/health");
    let deadline = std::time::Instant::now() + HEALTH_TIMEOUT;
    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!("llama-server did not answer on port {port}"));
        }
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_not_lexicographically() {
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.1"));
    }

    #[test]
    fn a_v_prefix_and_short_versions_are_understood() {
        assert!(is_newer("v1.1", "1.0.0"));
        assert!(is_newer("1.0", "0.9"));
        assert!(!is_newer("1", "1.0.0"));
    }

    #[test]
    fn the_judge_timeout_defaults_to_ten_seconds_and_can_be_overridden() {
        // A CPU judge spends ~5 s on prompt eval: the old 1.8 s was the bug.
        assert_eq!(judge_timeout_from(None), Duration::from_millis(10_000));
        assert_eq!(
            judge_timeout_from(Some("25000")),
            Duration::from_millis(25_000)
        );
        assert_eq!(
            judge_timeout_from(Some(" 1500 ")),
            Duration::from_millis(1_500)
        );
        // Junk must not disable judging; zero must not make every turn a rule.
        assert_eq!(judge_timeout_from(Some("abc")), DEFAULT_JUDGE_TIMEOUT);
        assert_eq!(judge_timeout_from(Some("0")), DEFAULT_JUDGE_TIMEOUT);
        assert_eq!(judge_timeout_from(Some("-5")), DEFAULT_JUDGE_TIMEOUT);
    }
    /// The two URL shapes are the difference between a release host and a hub;
    /// a stray `{version}` in a hub path or a trailing slash in a base is a 404
    /// that only ever shows up in production.
    #[test]
    fn a_hub_ignores_the_version_and_a_release_uses_it() {
        let hub = Source::hub("ModelScope", "https://modelscope.cn/models/a/b/", "master");
        assert_eq!(
            hub.manifest,
            "https://modelscope.cn/models/a/b/resolve/master/manifest.json"
        );
        assert_eq!(
            hub.url_for("v1", "m.gguf"),
            "https://modelscope.cn/models/a/b/resolve/master/m.gguf"
        );

        let release = Source::release("GitHub", "https://github.com/a/b/");
        assert_eq!(
            release.manifest,
            "https://github.com/a/b/releases/latest/download/manifest.json"
        );
        assert_eq!(
            release.url_for("v2", "m.gguf"),
            "https://github.com/a/b/releases/download/v2/m.gguf"
        );
    }
}
