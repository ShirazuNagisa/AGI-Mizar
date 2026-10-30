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

/// The repository the model comes from.
///
/// `MIZAR_MODEL_REPO` exists so a test can point the whole flow at a local
/// server; a release never sets it.
fn repo_base() -> String {
    std::env::var("MIZAR_MODEL_REPO")
        .unwrap_or_else(|_| "https://github.com/ShirazuNagisa/Mizarllm".to_string())
}

/// Where the newest release describes itself.
///
/// GitHub's `releases/latest/download/<asset>` alias is a CDN redirect: no API
/// call, no token, no hourly quota. One file, and the version is in it.
fn latest_manifest_url() -> String {
    format!("{}/releases/latest/download/manifest.json", repo_base())
}

/// Where the assets of one release live.
fn tag_asset_url() -> String {
    format!("{}/releases/download/v{{version}}/{{file}}", repo_base())
}

/// How often download progress is published.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// How long `llama-server` gets to answer `/health` before the start is a failure.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(20);

/// How long one judgement may take before the rule fallback takes over.
const JUDGE_TIMEOUT: Duration = Duration::from_millis(1800);

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
}

/// The model directory, the downloader, and the sidecar.
pub struct ModelService {
    dir: PathBuf,
    active: RwLock<Option<ModelManifest>>,
    download: Mutex<Option<Arc<Download>>>,
    runtime: Arc<Runtime>,
    events: broadcast::Sender<ModelEvent>,
    http: reqwest::Client,
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
        Arc::new(Self {
            dir,
            active: RwLock::new(active),
            download: Mutex::new(None),
            runtime: Arc::new(Runtime::new()),
            events,
            // `timeout` is the whole request, body included: on a 1 GB download
            // it fires at 60 s and the stream dies a third of the way through.
            // `read_timeout` is the silence between bytes, which is the thing
            // worth failing on.
            http: reqwest::Client::builder()
                .read_timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
        })
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

    /// Ask the repository what the newest release is.
    pub async fn check(&self) -> ModelCheckView {
        let installed = self.manifest().map(|m| m.version);
        let remote = match self.fetch_remote().await {
            Ok(remote) => remote,
            Err(error) => {
                return ModelCheckView {
                    installed,
                    latest: None,
                    error: Some(error),
                }
            }
        };
        let newer = installed
            .as_deref()
            .map(|current| is_newer(&remote.manifest.version, current))
            .unwrap_or(true);
        ModelCheckView {
            installed,
            latest: Some(RemoteModel {
                id: remote.manifest.id.clone(),
                version: remote.manifest.version.clone(),
                url: tag_asset_url()
                    .replace("{version}", &remote.manifest.version)
                    .replace("{file}", &remote.file),
                sha256: remote.sha256.clone(),
                size_bytes: remote.size_bytes,
                newer,
            }),
            error: None,
        }
    }

    async fn fetch_remote(&self) -> Result<RemoteManifest, String> {
        let response = self
            .http
            .get(latest_manifest_url())
            .send()
            .await
            .map_err(|error| format!("could not reach the model repository: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the model repository answered {}",
                response.status().as_u16()
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("could not read the manifest: {error}"))?;
        serde_json::from_slice::<RemoteManifest>(&bytes).map_err(|error| {
            format!("the repository's manifest is not one this build understands: {error}")
        })
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
        let remote = match self.fetch_remote().await {
            Ok(remote) => remote,
            Err(error) => {
                // The frame already exists, so the window sees the reason
                // instead of a button that appears to do nothing.
                download.fail(&error);
                self.publish(ModelEvent::Progress(download.snapshot()));
                return Err(error);
            }
        };
        if let Some(want) = want.as_deref() {
            if want != remote.manifest.version {
                let error = format!(
                    "the repository offers {} and this build asked for {want}",
                    remote.manifest.version
                );
                download.fail(&error);
                self.publish(ModelEvent::Progress(download.snapshot()));
                return Err(error);
            }
        }

        let version = remote.manifest.version.clone();
        let url = tag_asset_url()
            .replace("{version}", &version)
            .replace("{file}", &remote.file);

        let temp_dir = self.dir.join(".download");
        std::fs::create_dir_all(&temp_dir).map_err(|error| {
            let message = format!("could not create {}: {error}", temp_dir.display());
            download.fail(&message);
            self.publish(ModelEvent::Progress(download.snapshot()));
            message
        })?;
        let part_path = temp_dir.join(format!("{}.part", remote.file));

        let total = remote.size_bytes.unwrap_or(0);
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

        let fetched = self
            .fetch_to(Arc::clone(&download), &url, &part_path, total)
            .await;
        if let Err(error) = fetched {
            download.fail(&error);
            self.publish(ModelEvent::Progress(download.snapshot()));
            // The frame stays so the window can draw the reason; the next
            // attempt replaces it.
            return Err(error);
        }

        download.phase("verifying");
        self.publish(ModelEvent::Progress(download.snapshot()));
        match self.install(&remote, &download, &part_path).await {
            Ok(()) => {
                self.clear_download();
                self.publish(ModelEvent::Changed(Box::new(self.state())));
                Ok(())
            }
            Err(error) => {
                download.fail(&error);
                self.publish(ModelEvent::Progress(download.snapshot()));
                Err(error)
            }
        }
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
        let response = request
            .send()
            .await
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
    ) -> Result<(), String> {
        if let Some(expected) = remote.sha256.as_deref() {
            let actual = sha256_of(part_path).await?;
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = std::fs::remove_file(part_path);
                return Err(format!(
                    "checksum mismatch: expected {expected}, got {actual}"
                ));
            }
        }

        let id = &remote.manifest.id;
        let current = self.weights_of(id, false);
        let current_manifest = self.dir.join(format!("{id}.json"));
        if current.exists() {
            let previous_dir = self.dir.join(".previous");
            std::fs::create_dir_all(&previous_dir)
                .map_err(|error| format!("could not create {}: {error}", previous_dir.display()))?;
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
            format!(
                "could not move the download into {}: {error}",
                current.display()
            )
        })?;

        // The manifest goes in last: a half-written model must never look
        // installed just because its manifest is there.
        let text = serde_json::to_vec_pretty(&remote.manifest)
            .map_err(|error| format!("could not serialise the manifest: {error}"))?;
        write_atomic(&current_manifest, &text)?;

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
        tokio::time::timeout(JUDGE_TIMEOUT, judge.judge(facts))
            .await
            .unwrap_or_default()
    }
}

/// The `llama-server` process, and where it is.
pub struct Runtime {
    process: tokio::sync::Mutex<Option<tokio::process::Child>>,
    view: RwLock<RuntimeView>,
    /// One start at a time: two turns ending together must not race two
    /// `llama-server` processes into the same port.
    starting: tokio::sync::Mutex<()>,
}

impl Runtime {
    fn new() -> Self {
        Self {
            process: tokio::sync::Mutex::new(None),
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

    /// Find the binary: an explicit override, then the app bundle, then `PATH`.
    ///
    /// The bundle path is what ships; `PATH` is what a developer has.
    pub fn binary(&self) -> Option<PathBuf> {
        if let Ok(explicit) = std::env::var("AGI_LLAMA_BIN") {
            let path = PathBuf::from(explicit);
            if path.exists() {
                return Some(path);
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            // AGI Mizar.app/Contents/MacOS/agi-mizar -> Contents/Resources/bin/
            if let Some(contents) = exe.parent().and_then(|p| p.parent()) {
                let bundled = contents.join("Resources/bin/llama-server");
                if bundled.exists() {
                    return Some(bundled);
                }
            }
        }
        let path = std::env::var("PATH").ok()?;
        path.split(':')
            .map(|dir| PathBuf::from(dir).join("llama-server"))
            .find(|candidate| candidate.exists())
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
        let child = tokio::process::Command::new(&binary)
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
}
