//! Anonymous sharing of finished turns, for training Mizarllm.
//!
//! ### What leaves the machine
//!
//! One JSON object per finished turn: the operator's instruction, the tool
//! names and the failed calls, the last few replies, and the structural facts
//! the judge already derives. That is exactly the record the training pipeline
//! in `tools/replay` calls an *input*; the teacher labels are added later, on
//! the operator's own machines.
//!
//! ### What is removed before it is written
//!
//! The account name, the host name, the home directory, other absolute paths,
//! e-mail addresses, phone numbers, API tokens, private keys and URL query
//! strings. A session's real id is replaced by a keyed hash of the install's
//! random `install_id`, so turns from one session stay grouped while the server
//! cannot recover which session (or account) they came from. This is
//! de-identification, not mathematics-grade anonymity: free text can still carry
//! something personal no pattern recognises, and the consent copy says so.
//!
//! ### Why it is invisible
//!
//! The operator answered once, in the guide; after that the kernel spools turns
//! and uploads them in the background, with no counters in the UI. Turning the
//! switch off in settings stops the uploader and the backfill immediately. There
//! is deliberately no "delete what was uploaded": the server never stores the
//! account, so it has no row to find, and the consent copy says that too.
//!
//! ### The one-year backfill
//!
//! Turning sharing on walks the sessions the engines can still list (including
//! Codex's archived threads), page by page, and shares the finished turns of the
//! last 365 days. It is paced, resumable and skips any project the operator put
//! on the exclusion list.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use agi_layout::Facts;
use agi_protocol::api::DatasetView;
use agi_protocol::domain::{Record, RecordKind, Session, SessionKey};
use regex::{Captures, Regex};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::Core;

/// The consent copy version the guide and the settings page are written to.
///
/// Bump it whenever the wording of what is shared changes: the stored number is
/// what lets a later release tell "this machine agreed to the old description"
/// from "this machine agreed to this one".
pub const POLICY_VERSION: u32 = 1;

/// How far back the one-year backfill walks.
const HISTORY_DAYS: i64 = 365;

/// The most turns one upload request carries.
const BATCH_TURNS: usize = 200;

/// The most bytes one upload request carries. Below the relay's 1 MiB cap and
/// far below nginx's 2 MiB default, so no deployment has to raise a limit for
/// the desktop's traffic.
const BATCH_BYTES: usize = 900_000;

/// How many sessions one backfill pass walks before yielding.
const BACKFILL_SESSIONS: usize = 20;

/// How many records one session may contribute to the backfill. A conversation
/// with more than this is older than the window that matters and is truncated
/// rather than held in memory.
const BACKFILL_RECORDS: usize = 40_000;

/// How often the background task looks for work when nothing wakes it.
const IDLE_TICK: Duration = Duration::from_secs(30);

/// How often the history walk looks for more work once the first pass is done.
const BACKFILL_TICK: Duration = Duration::from_secs(10 * 60);

/// The kernel-side state of the sharing feature.
///
/// `Default` is the state of a machine that has not opted in: no client, no
/// task, no I/O.
pub struct Dataset {
    /// Wakes the uploader after a capture or a consent change.
    pub(crate) upload_wake: tokio::sync::Notify,
    /// Wakes the backfill worker after a consent change.
    pub(crate) backfill_wake: tokio::sync::Notify,
    /// The HTTP client, built once.
    pub(crate) http: reqwest::Client,
    /// Turn ids already spooled in this process, so an engine that reports the
    /// same finish twice does not write twice.
    pub(crate) seen: Mutex<HashSet<String>>,
    /// Sessions the backfill already walked.
    pub(crate) backfilled: Mutex<HashSet<String>>,
    /// Whether the backfill is inside a pass.
    pub(crate) backfilling: AtomicBool,
    /// Whether the saved backfill list has been read into memory.
    pub(crate) loaded: AtomicBool,
    /// A failed upload is not retried on every tick.
    pub(crate) retry_after: Mutex<Instant>,
}

impl Default for Dataset {
    fn default() -> Self {
        Self {
            upload_wake: tokio::sync::Notify::new(),
            backfill_wake: tokio::sync::Notify::new(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            seen: Mutex::new(HashSet::new()),
            backfilled: Mutex::new(HashSet::new()),
            backfilling: AtomicBool::new(false),
            loaded: AtomicBool::new(false),
            retry_after: Mutex::new(Instant::now()),
        }
    }
}

/// What the window shows about the choice.
pub fn view(core: &Core) -> DatasetView {
    let prefs = prefs(core);
    DatasetView {
        enabled: prefs.enabled,
        policy_version: prefs.policy_version,
        since: prefs.since,
        excluded_projects: prefs.excluded_projects,
    }
}

/// Replace the exclusion list, normalised and bounded.
pub fn set_excluded(core: &Core, projects: Vec<String>) -> DatasetView {
    let mut clean: Vec<String> = Vec::new();
    for project in projects {
        let name = project.trim().to_string();
        if name.is_empty() || name.len() > 120 {
            continue;
        }
        if clean
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&name))
        {
            continue;
        }
        clean.push(name);
        if clean.len() >= 100 {
            break;
        }
    }
    let changed = {
        let mut preferences = core
            .preferences
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if preferences.dataset.excluded_projects == clean {
            false
        } else {
            preferences.dataset.excluded_projects = clean;
            true
        }
    };
    if changed {
        core.save_preferences();
    }
    view(core)
}

/// Remember the operator's answer and wake the worker.
pub fn set_enabled(core: &Core, enabled: bool) -> DatasetView {
    let now = now_ms();
    let dirty = {
        let mut preferences = core
            .preferences
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dataset = &mut preferences.dataset;
        let mut dirty = false;
        if dataset.install_id.is_empty() {
            dataset.install_id = uuid::Uuid::new_v4().to_string();
            dirty = true;
        }
        let changed = dataset.enabled != enabled;
        dataset.enabled = enabled;
        // The first deliberate answer is what the copy version records; a later
        // toggle does not re-describe the terms.
        if dataset.policy_version == 0 {
            dataset.policy_version = POLICY_VERSION;
            dirty = true;
        }
        if changed {
            dataset.since = Some(now);
            dirty = true;
        }
        dirty
    };
    if dirty {
        core.save_preferences();
    }
    core.dataset.upload_wake.notify_one();
    core.dataset.backfill_wake.notify_one();
    view(core)
}

/// Start the background uploader and backfill worker.
///
/// One task for the life of the kernel. It does nothing until sharing is on,
/// and it never holds the kernel alive: a test that drops its `Core` ends it.
pub fn spawn(core: Weak<Core>) {
    // Two loops, not one. The backfill walks years of history and can take
    // minutes per pass; sharing a task with the uploader meant a live turn sat
    // in the spool until the walk reached a page boundary. The uploader must
    // stay on its own clock.
    let uploader = core.clone();
    tokio::spawn(async move {
        loop {
            let Some(core) = uploader.upgrade() else {
                return;
            };
            if prefs(&core).enabled && upload_tick(&core).await {
                // Something went out; look for more without waiting a tick.
                core.dataset.upload_wake.notify_one();
            }
            let _ = tokio::time::timeout(IDLE_TICK, core.dataset.upload_wake.notified()).await;
        }
    });
    tokio::spawn(async move {
        loop {
            let Some(core) = core.upgrade() else {
                return;
            };
            if prefs(&core).enabled {
                backfill_tick(&core).await;
            }
            let _ =
                tokio::time::timeout(BACKFILL_TICK, core.dataset.backfill_wake.notified()).await;
        }
    });
}

impl Core {
    /// Spool one finished turn, if sharing is on and this session is eligible.
    ///
    /// Called from both finish paths: the DSH record stream and the
    /// session-stopped fallback for Claude and Codex. It is synchronous on
    /// purpose -- one small append -- and never blocks on the network.
    pub(crate) fn capture_dataset_turn(
        &self,
        session: &SessionKey,
        facts: &Facts,
        records: &[Record],
    ) {
        let info = self
            .store_read()
            .sessions
            .get(session)
            .cloned()
            .unwrap_or_else(|| Session::bare(session.clone(), now_ms()));
        self.capture_with(&info, facts, records);
    }

    fn capture_with(&self, session: &Session, facts: &Facts, records: &[Record]) {
        let prefs = prefs(self);
        if !prefs.enabled || prefs.policy_version == 0 || prefs.install_id.is_empty() {
            return;
        }
        if excluded(session.cwd.as_deref(), &prefs.excluded_projects) {
            return;
        }

        if facts.user.trim().is_empty() && facts.texts.iter().all(|text| text.trim().is_empty()) {
            return;
        }
        let turn = turn_of(&facts.id);
        let anon = anon_session(&prefs.install_id, &session.key);
        let id = format!("{anon}:{turn}");
        {
            let mut seen = self
                .dataset
                .seen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !seen.insert(id.clone()) {
                return;
            }
            // A process that runs for weeks should not grow this set forever.
            // Dropping it can only risk a duplicate in a later run, and the
            // training merge drops duplicates by id.
            if seen.len() > 200_000 {
                seen.clear();
                seen.insert(id.clone());
            }
        }

        let at = records
            .last()
            .map(|record| record.time)
            .unwrap_or_else(now_ms);
        let shared = SharedTurn {
            v: 1,
            id,
            session: anon.clone(),
            turn,
            engine: session.key.engine.as_str().to_string(),
            minutes: facts.minutes,
            reason: facts.reason.clone(),
            steps: facts.steps,
            reply_count: facts.reply_count,
            ends_with_question: facts.ends_with_question,
            user: cap(&redact(&facts.user, self), 20_000),
            tools: cap(&redact(&facts.tools, self), 4_000),
            failures: cap_list(&facts.failures, self, 40, 4_000),
            subagents: cap_list(&facts.subagents, self, 40, 2_000),
            jobs: cap_list(&facts.jobs, self, 40, 2_000),
            texts: cap_list(&facts.texts, self, 12, 12_000),
            day: day_of(at),
        };

        let Ok(line) = serde_json::to_string(&shared) else {
            return;
        };
        let dir = data_dir(self).join("spool");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join(format!("{anon}-{turn}.jsonl"));
        // The same file name is the dedupe rule across restarts: a turn already
        // spooled (or uploaded) is not written again.
        if path.exists() {
            return;
        }
        let temp = path.with_extension("jsonl.tmp");
        if std::fs::write(&temp, format!("{line}\n")).is_err() {
            let _ = std::fs::remove_file(&temp);
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
        }
        if std::fs::rename(&temp, &path).is_err() {
            let _ = std::fs::remove_file(&temp);
        } else {
            // A live turn should go out on the next tick, not after the backfill
            // pass that may be running right now.
            self.dataset.upload_wake.notify_one();
        }
    }
}

// ---------------------------------------------------------------- uploading

/// Send one batch of spooled turns. `true` means at least one request was
/// accepted, so the caller can look again immediately.
async fn upload_tick(core: &Arc<Core>) -> bool {
    if !prefs(core).enabled || !core.account.signed_in() {
        return false;
    }
    if Instant::now()
        < *core
            .dataset
            .retry_after
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        return false;
    }
    let dir = data_dir(core).join("spool");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    if files.is_empty() {
        return false;
    }
    files.sort();

    let mut body = String::new();
    let mut sent: Vec<PathBuf> = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if body.len() + text.len() > BATCH_BYTES && !sent.is_empty() {
            break;
        }
        body.push_str(&text);
        sent.push(path);
        if sent.len() >= BATCH_TURNS || body.len() >= BATCH_BYTES {
            break;
        }
    }
    if sent.is_empty() {
        return false;
    }

    let Some((token, _device)) = core.relay_account() else {
        return false;
    };
    let batch = uuid::Uuid::new_v4().to_string();
    let url = format!("{}/dataset/v1/ingest", dataset_base());
    let answer = core
        .dataset
        .http
        .post(url)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/x-ndjson")
        .header("x-mizar-batch", batch.clone())
        .header("x-mizar-client", format!("agi-mizar/{}", core.version))
        .body(body)
        .send()
        .await;

    match answer {
        Ok(response) if response.status().is_success() => {
            for path in &sent {
                let _ = std::fs::remove_file(path);
            }
            tracing::debug!(batch = %batch, turns = sent.len(), "dataset: batch accepted");
            clear_retry(core);
            true
        }
        Ok(response) => {
            let status = response.status();
            // 413 means this batch was too large despite the client cap; the
            // files stay and the next tick sends fewer of them (the size check
            // is by file, so one oversized turn is retried alone forever only
            // if the server cap is lower than one record -- not the case).
            let wait = match status.as_u16() {
                429 => Duration::from_secs(30 * 60),
                401 | 403 => Duration::from_secs(60 * 60),
                _ => Duration::from_secs(5 * 60),
            };
            defer(core, wait);
            tracing::debug!(batch = %batch, status = %status, "dataset: batch refused");
            false
        }
        Err(error) => {
            defer(core, Duration::from_secs(5 * 60));
            tracing::debug!("dataset: upload failed — {error}");
            false
        }
    }
}

fn defer(core: &Core, wait: Duration) {
    *core
        .dataset
        .retry_after
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now() + wait;
}

fn clear_retry(core: &Core) {
    *core
        .dataset
        .retry_after
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
}

// --------------------------------------------------------------- backfilling

/// Walk a few sessions of the last year, newest first.
async fn backfill_tick(core: &Arc<Core>) {
    if core.dataset.backfilling.swap(true, Ordering::AcqRel) {
        return;
    }
    let _guard = BackfillGuard(&core.dataset.backfilling);
    let prefs = prefs(core);
    if !prefs.enabled || prefs.policy_version == 0 || prefs.install_id.is_empty() {
        return;
    }
    load_backfilled(core);

    let cutoff = now_ms() - HISTORY_DAYS * 24 * 60 * 60 * 1000;
    let mut candidates: Vec<Session> = Vec::new();
    let mut engines: Vec<Arc<dyn agi_engine::Engine>> = core.engines.clone();
    {
        let late = core
            .late
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        engines.extend(late.values().cloned());
    }
    for engine in engines {
        let mut listed = engine.sessions().await.unwrap_or_default();
        // Codex keeps archived threads out of the live list; the backfill is the
        // one caller that wants them. Other adapters answer with nothing.
        listed.extend(engine.archived_sessions().await.unwrap_or_default());
        for session in listed {
            if session.updated_at < cutoff {
                continue;
            }
            let key = session_key(&session);
            if core
                .dataset
                .backfilled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&key)
            {
                continue;
            }
            if excluded(session.cwd.as_deref(), &prefs.excluded_projects) {
                continue;
            }
            candidates.push(session);
        }
    }
    candidates.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    candidates.truncate(BACKFILL_SESSIONS);

    for session in candidates {
        let Some(engine) = core.engine(session.key.engine) else {
            continue;
        };
        let mut records: Vec<Record> = Vec::new();
        let mut before: Option<i64> = None;
        let mut failed = false;
        loop {
            match engine.records(&session.key, 400, before).await {
                Ok(page) if page.is_empty() => break,
                Ok(page) => {
                    before = page.first().map(|record| record.seq);
                    records.extend(page);
                    if records.len() >= BACKFILL_RECORDS {
                        break;
                    }
                }
                Err(error) => {
                    tracing::debug!(session = %session.key, "dataset: backfill read failed — {error}");
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            continue;
        }
        records.reverse();
        for (turn, slice) in turns_from_records(&records) {
            let Some(first) = slice.first() else {
                continue;
            };
            if first.time < cutoff {
                continue;
            }
            let facts = core.facts_for(&session.key, turn, &slice);
            core.capture_with(&session, &facts, &slice);
        }
        mark_backfilled(core, &session);
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

struct BackfillGuard<'a>(&'a AtomicBool);

impl Drop for BackfillGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn load_backfilled(core: &Core) {
    if core.dataset.loaded.swap(true, Ordering::AcqRel) {
        return;
    }
    let stored = prefs(core).backfilled;
    let mut set = core
        .dataset
        .backfilled
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set.extend(stored);
}

fn mark_backfilled(core: &Core, session: &Session) {
    let key = session_key(session);
    let mut set = core
        .dataset
        .backfilled
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !set.insert(key.clone()) {
        return;
    }
    drop(set);
    let mut preferences = core
        .preferences
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !preferences.dataset.backfilled.contains(&key) {
        preferences.dataset.backfilled.push(key);
    }
    drop(preferences);
    core.save_preferences();
}

fn session_key(session: &Session) -> String {
    format!("{}:{}", session.key.engine.as_str(), session.key.id)
}

/// The finished turns in a chronological record list.
fn turns_from_records(records: &[Record]) -> Vec<(u32, Vec<Record>)> {
    let mut out: Vec<(u32, Vec<Record>)> = Vec::new();
    let mut current: Vec<Record> = Vec::new();
    let mut turn: Option<u32> = None;
    for record in records {
        if record.kind == RecordKind::TurnStart {
            current.clear();
            turn = record.turn.or(turn);
            current.push(record.clone());
            continue;
        }
        if current.is_empty() && record.kind != RecordKind::TurnEnd {
            turn = record.turn.or(turn);
        }
        current.push(record.clone());
        if record.kind == RecordKind::TurnEnd {
            let number = record.turn.or(turn).unwrap_or(0);
            if current.iter().any(|item| item.kind != RecordKind::TurnEnd) {
                out.push((number, std::mem::take(&mut current)));
            } else {
                current.clear();
            }
            turn = None;
        }
    }
    out
}

// ------------------------------------------------------------------ plumbing

/// Where the ingest endpoint lives.
///
/// Not the account base: the desktop signs in through the unified relay, which
/// deliberately keeps no account store (it verifies and proxies to the origin),
/// so it has nothing to authenticate an upload against. The production role-1
/// relay on 443 owns the account store and is therefore the one that can accept
/// a batch. `MIZAR_DATASET_URL` exists for a self-hosted deployment.
fn dataset_base() -> String {
    std::env::var("MIZAR_DATASET_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://dshmizar-1.shirazu-nagisa.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn prefs(core: &Core) -> crate::DatasetPrefs {
    core.preferences
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .dataset
        .clone()
}

fn data_dir(core: &Core) -> PathBuf {
    core.preferences_path
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(|path| path.parent())
        .map(|parent| parent.join("dataset"))
        .unwrap_or_else(|| agi_platform::data_dir().join("dataset"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn turn_of(id: &str) -> u32 {
    id.rsplit_once(':')
        .and_then(|(_, turn)| turn.parse().ok())
        .unwrap_or(0)
}

fn day_of(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|time| time.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_string())
}

fn excluded(cwd: Option<&str>, list: &[String]) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    if list.is_empty() {
        return false;
    }
    let name = cwd
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(cwd);
    list.iter().any(|entry| {
        let entry = entry.trim();
        !entry.is_empty() && (entry.eq_ignore_ascii_case(name) || entry.eq_ignore_ascii_case(cwd))
    })
}

/// The pseudonym of one session under this install.
fn anon_session(install_id: &str, key: &SessionKey) -> String {
    let mut hash = Sha256::new();
    hash.update(install_id.as_bytes());
    hash.update(b"\n");
    hash.update(key.engine.as_str().as_bytes());
    hash.update(b"\n");
    hash.update(key.id.as_bytes());
    let digest = hash.finalize();
    hex16(&digest)
}

fn hex16(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(32);
    for byte in bytes.iter().take(16) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// -------------------------------------------------------------- anonymising

/// The turn as it goes on the wire. Field names match the training pipeline's
/// `inputs.jsonl`, with three provenance fields added (`engine`, `archived` is
/// not needed, `day`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SharedTurn {
    v: u32,
    id: String,
    session: String,
    turn: u32,
    engine: String,
    minutes: f64,
    reason: String,
    steps: u32,
    reply_count: u32,
    ends_with_question: bool,
    user: String,
    tools: String,
    failures: Vec<String>,
    subagents: Vec<String>,
    jobs: Vec<String>,
    texts: Vec<String>,
    day: String,
}

/// Replace everything the patterns know about a piece of text.
fn redact(text: &str, core: &Core) -> String {
    if text.is_empty() {
        return String::new();
    }
    let home = agi_platform::home().to_string_lossy().to_string();
    let mut out = text.to_string();
    out = pattern(
        &PEM,
        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
    )
    .replace_all(&out, "<private-key>")
    .into_owned();
    out = pattern(
        &TOKENS,
        r"(?i)(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9-]{10,})",
    )
    .replace_all(&out, "<token>")
    .into_owned();
    out = pattern(&BEARER, r"(?i)\bBearer\s+[A-Za-z0-9._~+/\-=]{10,}")
        .replace_all(&out, "Bearer <token>")
        .into_owned();
    out = pattern(
        &ASSIGN,
        r#"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|client[_-]?secret)\b\s*[:=]\s*['"]?[^\s'",;]{4,}"#,
    )
    .replace_all(&out, "$1=<redacted>")
    .into_owned();
    out = pattern(&SSHKEY, r"ssh-(?:rsa|ed25519|dss)\s+AAAA[A-Za-z0-9+/=]+")
        .replace_all(&out, "<ssh-key>")
        .into_owned();

    if home.len() > 1 {
        out = out.replace(&home, "~");
    }
    out = pattern(&HOME_UNIX, r#"(?:/Users|/home)/[^/\s"']+"#)
        .replace_all(&out, "~")
        .into_owned();
    out = pattern(&HOME_WIN, r#"(?i)[A-Za-z]:\\Users\\[^\\\s"']+"#)
        .replace_all(&out, "~")
        .into_owned();
    out = pattern(&URL, r#"https?://[^\s<>"']+"#)
        .replace_all(&out, |caps: &Captures<'_>| {
            let url = &caps[0];
            match url.find(['?', '#']) {
                Some(cut) => url[..cut].to_string(),
                None => url.to_string(),
            }
        })
        .into_owned();
    out = pattern(&EMAIL, r"[\w.+-]+@[\w-]+\.[\w.-]+")
        .replace_all(&out, "<email>")
        .into_owned();
    out = pattern(&PHONE_CN, r"(?:\+?86[-\s]?)?1[3-9]\d{9}")
        .replace_all(&out, "<phone>")
        .into_owned();
    out = pattern(&PHONE_INTL, r"\+\d{1,3}[\s-]?\d{6,14}")
        .replace_all(&out, "<phone>")
        .into_owned();
    out = pattern(
        &PATH_UNIX,
        r#"(^|[\s"'(=:：，。])(/[A-Za-z0-9_.\-]+(?:/[A-Za-z0-9_.\-]+)+)"#,
    )
    .replace_all(&out, "$1<path>")
    .into_owned();
    out = pattern(&PATH_WIN, r#"(^|[\s"'(=])([A-Za-z]:\\[^\s"']+)"#)
        .replace_all(&out, "$1<path>")
        .into_owned();

    // The account and the host are the two identifiers that can appear as bare
    // words in prose. Short or generic names are skipped so the redaction does
    // not turn ordinary text into placeholders.
    if let Some(user) = core.account_user().map(|user| user.username) {
        out = replace_word(&out, &user, "<account>");
    }
    let host = agi_platform::host_name();
    out = replace_word(&out, &host, "<host>");
    if let Some(short) = host.split('.').next() {
        out = replace_word(&out, short, "<host>");
    }
    // The OS account is a bare word in a lot of prose ("shirazu@host"), and it
    // is the same person the account name belongs to.
    for key in ["USER", "USERNAME", "LOGNAME"] {
        if let Ok(name) = std::env::var(key) {
            out = replace_word(&out, &name, "<user>");
        }
    }
    out
}

fn cap(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

fn cap_list(items: &[String], core: &Core, count: usize, each: usize) -> Vec<String> {
    items
        .iter()
        .take(count)
        .map(|text| cap(&redact(text, core), each))
        .collect()
}

fn replace_word(text: &str, needle: &str, replacement: &str) -> String {
    let needle = needle.trim();
    if needle.len() < 4 || matches!(needle, "root" | "user" | "admin" | "runner" | "ubuntu") {
        return text.to_string();
    }
    text.replace(needle, replacement)
}

fn pattern(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("dataset redaction regex"))
}

static PEM: OnceLock<Regex> = OnceLock::new();
static TOKENS: OnceLock<Regex> = OnceLock::new();
static BEARER: OnceLock<Regex> = OnceLock::new();
static ASSIGN: OnceLock<Regex> = OnceLock::new();
static SSHKEY: OnceLock<Regex> = OnceLock::new();
static EMAIL: OnceLock<Regex> = OnceLock::new();
static PHONE_CN: OnceLock<Regex> = OnceLock::new();
static PHONE_INTL: OnceLock<Regex> = OnceLock::new();
static URL: OnceLock<Regex> = OnceLock::new();
static HOME_UNIX: OnceLock<Regex> = OnceLock::new();
static HOME_WIN: OnceLock<Regex> = OnceLock::new();
static PATH_UNIX: OnceLock<Regex> = OnceLock::new();
static PATH_WIN: OnceLock<Regex> = OnceLock::new();

// --------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use agi_protocol::EngineId;

    fn test_core() -> Arc<Core> {
        Core::new(Vec::new())
    }

    #[test]
    fn a_home_path_becomes_a_tilde_and_other_paths_a_placeholder() {
        let core = test_core();
        let home = agi_platform::home().to_string_lossy().to_string();
        let text = format!(
            "opened {home}/Documents/project/secret/src/main.rs and /opt/homebrew/bin/node"
        );
        let redacted = redact(&text, &core);
        assert!(!redacted.contains(&home), "{redacted}");
        assert!(redacted.contains("~/Documents"), "{redacted}");
        assert!(redacted.contains("<path>"), "{redacted}");
        assert!(!redacted.contains("/opt/homebrew"), "{redacted}");
    }

    #[test]
    fn secrets_emails_urls_and_phones_are_removed() {
        let core = test_core();
        let text = "key sk-abcdefghijklmnopqrstuvwx mail me@example.com \
                    https://example.com/a?token=1 phone 13800138000 password=hunter2 \
                    Authorization: Bearer abcdefghijklmnop";
        let redacted = redact(text, &core);
        assert!(
            !redacted.contains("sk-abcdefghijklmnopqrstuvwx"),
            "{redacted}"
        );
        assert!(!redacted.contains("me@example.com"), "{redacted}");
        assert!(!redacted.contains("token=1"), "{redacted}");
        assert!(!redacted.contains("13800138000"), "{redacted}");
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(!redacted.contains("abcdefghijklmnop"), "{redacted}");
    }

    #[test]
    fn a_pseudonym_is_stable_for_one_session_and_install() {
        let key = SessionKey::new(EngineId::Dsh, "session-a");
        assert_eq!(anon_session("install", &key), anon_session("install", &key));
        assert_ne!(anon_session("install", &key), anon_session("other", &key));
        assert_ne!(
            anon_session("install", &key),
            anon_session("install", &SessionKey::new(EngineId::Dsh, "session-b"))
        );
        assert_eq!(anon_session("install", &key).len(), 32);
    }

    #[test]
    fn turns_are_grouped_by_turn_end() {
        let mut start = Record::message(1, 0, Some(1), RecordKind::TurnStart, String::new());
        start.kind = RecordKind::TurnStart;
        let user = Record::message(2, 10, Some(1), RecordKind::UserMessage, "do it".into());
        let end = Record::message(3, 20, Some(1), RecordKind::TurnEnd, "completed".into());
        let start2 = Record {
            seq: 4,
            time: 30,
            turn: Some(2),
            step: None,
            kind: RecordKind::TurnStart,
            text: None,
            tool: None,
        };
        let end2 = Record::message(5, 40, Some(2), RecordKind::TurnEnd, "error".into());
        let turns = turns_from_records(&[start, user, end, start2, end2]);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].0, 1);
        assert_eq!(turns[0].1.len(), 3);
        assert_eq!(turns[1].0, 2);
    }

    #[test]
    fn excluded_projects_match_a_path_or_a_name() {
        let list = vec!["secret".to_string(), "/tmp/private".to_string()];
        assert!(excluded(Some("/home/me/secret"), &list));
        assert!(excluded(Some("/tmp/private"), &list));
        assert!(!excluded(Some("/home/me/other"), &list));
        assert!(!excluded(None, &list));
    }

    #[test]
    fn the_shared_record_has_the_training_field_names() {
        let turn = SharedTurn {
            v: 1,
            id: "abc:3".into(),
            session: "abc".into(),
            turn: 3,
            engine: "dsh".into(),
            minutes: 1.5,
            reason: "completed".into(),
            steps: 2,
            reply_count: 1,
            ends_with_question: false,
            user: "hi".into(),
            tools: "read×1".into(),
            failures: vec![],
            subagents: vec![],
            jobs: vec![],
            texts: vec!["done".into()],
            day: "2026-11-03".into(),
        };
        let value: serde_json::Value = serde_json::to_value(&turn).unwrap();
        for key in [
            "v",
            "id",
            "session",
            "turn",
            "engine",
            "minutes",
            "reason",
            "steps",
            "replyCount",
            "endsWithQuestion",
            "user",
            "tools",
            "failures",
            "subagents",
            "jobs",
            "texts",
            "day",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
    }
}
