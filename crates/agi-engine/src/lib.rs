//! What an engine adapter has to provide.
//!
//! The trait is deliberately small. Every method here is something a person does
//! to an agent — read its sessions, read a conversation, send a prompt, stop it,
//! answer it, choose a model or a permission level. Anything an engine can do
//! beyond that is reached through the engine's own escape hatch, not through this
//! interface: a trait that grows one method per product feature becomes a union
//! of three products rather than an abstraction over them.
//!
//! ### Events, not polling
//!
//! An adapter pushes [`EngineEvent`]s; the core never asks "what changed". That
//! is what makes the UI feel immediate on three engines with three different
//! update mechanisms (a WebSocket firehose, a JSON-RPC notification stream, a
//! file being appended to).

use agi_protocol::domain::{
    ActivityEntry, Balance, DeliveredFile, FilePreview, FileSlice, FileStat, Interaction,
    ModelInfo, PermissionPreset, Record, Session, SessionKey, SessionStats, Workspace,
};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use serde_json::Value;

/// Something that happened in an engine.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A session appeared, or its summary changed.
    SessionUpdated(Session),
    /// A session is gone from this engine's list.
    SessionRemoved(SessionKey),
    /// One committed record.
    RecordAppended { session: SessionKey, record: Record },
    /// An agent is blocked on the operator.
    InteractionRequested(Interaction),
    /// An interaction was answered, here or elsewhere.
    InteractionResolved { id: String },
    /// The engine connected or disconnected.
    StatusChanged {
        connected: bool,
        detail: Option<String>,
    },
    /// Something to show that is not an error.
    Notice { level: String, message: String },
}

/// How an operator answered an interaction.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    /// An approval outcome: `allowed-once` or `rejected`.
    pub outcome: Option<String>,
    /// A question batch: one entry per question.
    pub answers: Option<Vec<Value>>,
}

/// Why an engine call failed.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The engine is not attached right now.
    #[error("{engine} is not connected")]
    NotConnected { engine: EngineId },
    /// The engine cannot do this, and its capabilities said so.
    #[error("{engine} does not support {feature}")]
    Unsupported {
        engine: EngineId,
        feature: &'static str,
    },
    /// The engine reported a failure. The code is the engine's own.
    #[error("{code}: {message}")]
    Upstream {
        code: String,
        message: String,
        details: Option<Value>,
    },
    /// The session is not one this engine knows.
    #[error("unknown session {0}")]
    UnknownSession(SessionKey),
    /// The call did not finish in time.
    #[error("timed out: {0}")]
    Timeout(&'static str),
}

impl EngineError {
    /// A failure with no engine code of its own.
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::Upstream {
            code: "REMOTE_ERROR".to_string(),
            message: message.into(),
            details: None,
        }
    }

    /// The code a client should branch on.
    pub fn code(&self) -> &str {
        match self {
            Self::NotConnected { .. } => "NOT_CONNECTED",
            Self::Unsupported { .. } => "UNSUPPORTED",
            Self::Upstream { code, .. } => code,
            Self::UnknownSession(_) => "UNKNOWN_SESSION",
            Self::Timeout(_) => "TIMEOUT",
        }
    }
}

/// One agent product, adapted to the shared model.
#[async_trait]
pub trait Engine: Send + Sync {
    /// Which product this is.
    fn id(&self) -> EngineId;

    /// What this adapter can do. The UI renders from this.
    fn capabilities(&self) -> Capabilities;

    /// Whether a prompt can be sent right now, and a one-line reason.
    fn status(&self) -> (bool, Option<String>);

    /// Every session this engine currently lists.
    async fn sessions(&self) -> Result<Vec<Session>, EngineError>;

    /// A page of records, newest last, at most `limit` of them.
    ///
    /// `before` pages backwards: only records with a smaller `seq`.
    async fn records(
        &self,
        session: &SessionKey,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<Record>, EngineError>;

    /// Send a prompt.
    async fn prompt(&self, session: &SessionKey, text: &str) -> Result<(), EngineError>;

    /// Stop the running turn.
    async fn cancel(&self, session: &SessionKey) -> Result<(), EngineError>;

    /// Answer a pending interaction.
    async fn answer(&self, id: &str, answer: Answer) -> Result<(), EngineError>;

    /// Choose a model.
    async fn set_model(&self, session: &SessionKey, model: &str) -> Result<(), EngineError>;

    /// Choose a permission preset.
    async fn set_permission(&self, session: &SessionKey, preset: &str) -> Result<(), EngineError>;

    /// The engine's event stream. One receiver per subscriber.
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent>;

    // ---- capabilities below are optional ----
    //
    // Each has a default that reports the gap rather than pretending. An adapter
    // implements the ones its product can actually do, and the capability flags
    // it reports are what the UI draws from.

    /// What the running turn is doing, oldest first.
    async fn activity(
        &self,
        _session: &SessionKey,
        _limit: usize,
    ) -> Result<Vec<ActivityEntry>, EngineError> {
        Err(self.unsupported("activity"))
    }

    /// What this session has cost so far.
    async fn stats(&self, _session: &SessionKey) -> Result<SessionStats, EngineError> {
        Err(self.unsupported("stats"))
    }

    /// The usage frame a phone's details panel draws, when the engine keeps the
    /// projections behind it.
    ///
    /// An empty frame rather than an error, because every field is optional: an
    /// engine with no projection model still lets the host answer with the
    /// title and context it derived itself, and the app merges what it gets.
    async fn usage(
        &self,
        _session: &SessionKey,
    ) -> Result<agi_protocol::usage::SessionUsage, EngineError> {
        Ok(agi_protocol::usage::SessionUsage::default())
    }

    /// Files the model said it produced.
    async fn deliverables(&self, _session: &SessionKey) -> Result<Vec<DeliveredFile>, EngineError> {
        Err(self.unsupported("deliverables"))
    }

    /// Every model this engine offers.
    async fn models(&self) -> Result<Vec<ModelInfo>, EngineError> {
        Err(self.unsupported("models"))
    }

    /// Every permission preset this engine offers.
    async fn permissions(&self) -> Result<Vec<PermissionPreset>, EngineError> {
        Err(self.unsupported("permissions"))
    }

    /// Start a new session in a directory.
    async fn create(&self, _cwd: Option<&str>) -> Result<SessionKey, EngineError> {
        Err(self.unsupported("create"))
    }

    /// Give a session a name.
    async fn rename(&self, _session: &SessionKey, _title: &str) -> Result<(), EngineError> {
        Err(self.unsupported("rename"))
    }

    /// Hide a session without deleting it.
    async fn archive(
        &self,
        _session: &SessionKey,
        _stop_activity: bool,
    ) -> Result<(), EngineError> {
        Err(self.unsupported("archive"))
    }

    /// The host's own workspace grouping.
    async fn workspaces(&self) -> Result<Vec<Workspace>, EngineError> {
        Err(self.unsupported("workspaces"))
    }

    /// What the provider account is worth.
    async fn balance(&self) -> Result<Balance, EngineError> {
        Err(self.unsupported("balance"))
    }

    /// Read a file a session delivered.
    ///
    /// Implemented here rather than per adapter, because "a file inside the
    /// session's directory" is the same question for all three products: they
    /// all report a working directory, and none of them needs its own answer.
    /// The fence is the point — a path outside the directory, or one that
    /// resolves outside it through a symlink, is refused, not served.
    async fn read_file(
        &self,
        session: &SessionKey,
        path: &str,
    ) -> Result<FilePreview, EngineError> {
        let cwd = self.session_cwd(session).await?;
        read_within(std::path::Path::new(&cwd), path)
    }

    /// What a path is, as the phone's preview page asks before reading.
    async fn file_stat(&self, session: &SessionKey, path: &str) -> Result<FileStat, EngineError> {
        let cwd = self.session_cwd(session).await?;
        stat_within(std::path::Path::new(&cwd), path)
    }

    /// One slice of a file, for a client that reads it a chunk at a time.
    async fn file_slice(
        &self,
        session: &SessionKey,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<FileSlice, EngineError> {
        let cwd = self.session_cwd(session).await?;
        slice_within(std::path::Path::new(&cwd), path, offset, length)
    }

    /// Fork a session at a completed turn boundary.
    async fn fork(&self, _session: &SessionKey, _at_seq: i64) -> Result<SessionKey, EngineError> {
        Err(self.unsupported("fork"))
    }

    /// The working directory a session's file reads are fenced to.
    async fn session_cwd(&self, session: &SessionKey) -> Result<String, EngineError> {
        self.sessions()
            .await?
            .into_iter()
            .find(|candidate| candidate.key == *session)
            .and_then(|candidate| candidate.cwd)
            .ok_or_else(|| EngineError::UnknownSession(session.clone()))
    }

    /// The failure for a capability this adapter does not have.
    fn unsupported(&self, feature: &'static str) -> EngineError {
        EngineError::Unsupported {
            engine: self.id(),
            feature,
        }
    }
}

/// The refusal for a path the fence does not allow.
fn forbidden(path: &str) -> EngineError {
    EngineError::Upstream {
        code: "FILE_FORBIDDEN".to_string(),
        message: format!("{path} is outside the session's directory"),
        details: None,
    }
}

/// Resolve `.` and `..` without touching the filesystem.
///
/// `canonicalize` cannot answer "is this inside" for a path that does not exist,
/// and the answer should not depend on whether the file is there: a fence that
/// only holds for existing files is not a fence.
fn lexical_normalize(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;

    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// How much of a file a preview may carry.
pub const PREVIEW_LIMIT: u64 = 256 * 1024;

/// The largest file the phone may read.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// The largest slice one `file.read` may ask for: 256 KiB of bytes is 341 KiB of
/// base64, which still fits one frame beside its envelope.
pub const MAX_SLICE_BYTES: u64 = 256 * 1024;

/// Read a file, refusing anything outside `cwd`.
///
/// `canonicalize` on both sides is what makes the fence real: a symlink inside
/// the directory that points outside it resolves to a path that no longer
/// starts with the directory, and is refused. Comparing the strings as written
/// would let `../` and symlinks through.
pub fn read_within(cwd: &std::path::Path, path: &str) -> Result<FilePreview, EngineError> {
    use std::io::Read;

    let resolved = resolve_within(cwd, path)?;
    let metadata = std::fs::metadata(&resolved).map_err(|error| unknown(path, &error))?;
    let name = file_name(&resolved, path);

    let mut buffer = Vec::new();
    std::fs::File::open(&resolved)
        .and_then(|file| file.take(PREVIEW_LIMIT + 1).read_to_end(&mut buffer))
        .map_err(|error| unknown(path, &error))?;
    let truncated = buffer.len() as u64 > PREVIEW_LIMIT;
    buffer.truncate(PREVIEW_LIMIT as usize);

    Ok(FilePreview {
        path: path.to_string(),
        name,
        size: metadata.len(),
        // Binary is not an error: it is a file a window cannot draw as text.
        text: String::from_utf8(buffer).ok(),
        truncated,
    })
}

/// Resolve a path inside `cwd`, refusing anything outside it.
///
/// The root is canonicalised first and the candidate is built *from it*, so a
/// lexical comparison is meaningful: on macOS `/var` is a symlink to
/// `/private/var`, and joining the raw cwd would compare two spellings of the
/// same directory and call every file outside.
fn resolve_within(cwd: &std::path::Path, path: &str) -> Result<std::path::PathBuf, EngineError> {
    let root = cwd
        .canonicalize()
        .map_err(|error| EngineError::upstream(format!("{}: {error}", cwd.display())))?;
    let candidate = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        root.join(path)
    };

    // Two checks, in this order. The lexical one answers "does this path even
    // point inside" for a file that may not exist, so a `../` is refused as a
    // fence violation rather than as a missing file. The canonical one is what
    // catches a symlink inside the directory that resolves outside it.
    if !lexical_normalize(&candidate).starts_with(&root) {
        return Err(forbidden(path));
    }
    let resolved = candidate
        .canonicalize()
        .map_err(|error| unknown(path, &error))?;
    if !resolved.starts_with(&root) {
        return Err(forbidden(path));
    }
    let metadata = std::fs::metadata(&resolved).map_err(|error| unknown(path, &error))?;
    if !metadata.is_file() {
        return Err(forbidden(path));
    }
    // A preview is a read of whatever the model pointed at, and "whatever" must
    // not include the operator's credentials because a sentence in a summary
    // asked for them.
    if denied_name(&file_name(&resolved, path)) {
        return Err(forbidden(path));
    }
    Ok(resolved)
}

/// The file's own name, falling back to the path as written.
fn file_name(resolved: &std::path::Path, path: &str) -> String {
    resolved
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// A file that is not there — or is there and cannot be opened.
fn unknown(path: &str, error: &std::io::Error) -> EngineError {
    EngineError::Upstream {
        code: "FILE_UNKNOWN".to_string(),
        message: format!("{path}: {error}"),
        details: None,
    }
}

/// A file beyond what the phone may read in one go.
fn too_large(path: &str, size: u64) -> EngineError {
    EngineError::Upstream {
        code: "FILE_TOO_LARGE".to_string(),
        message: format!("{path} is {size} bytes; the preview ceiling is {MAX_FILE_BYTES}"),
        details: None,
    }
}

/// Names refused even inside the workspace.
///
/// Deliberately short and about credentials only.
fn denied_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const EXACT: [&str; 12] = [
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        "id_rsa.pub",
        "id_dsa.pub",
        "id_ecdsa.pub",
        "id_ed25519.pub",
    ];
    if EXACT.contains(&lower.as_str()) {
        return true;
    }
    if lower == ".env" || lower.starts_with(".env.") {
        return true;
    }
    [".pem", ".key", ".p12", ".pfx", ".mobileprovision"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// The media type this host associates with a name, and whether the phone has a
/// viewer for it.
fn media_type(name: &str) -> (&'static str, bool) {
    let extension = name
        .rsplit_once('.')
        .map(|(_, tail)| tail.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "md" | "markdown" => ("text/markdown", true),
        "pdf" => ("application/pdf", true),
        "docx" => (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            true,
        ),
        "txt" => ("text/plain", false),
        _ => ("application/octet-stream", false),
    }
}

/// A file's facts, refused by the same fence as a read.
pub fn stat_within(cwd: &std::path::Path, path: &str) -> Result<FileStat, EngineError> {
    let resolved = resolve_within(cwd, path)?;
    let metadata = std::fs::metadata(&resolved).map_err(|error| unknown(path, &error))?;
    let name = file_name(&resolved, path);
    let (media, previewable) = media_type(&name);
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0);
    Ok(FileStat {
        path: path.to_string(),
        name,
        byte_size: metadata.len(),
        modified_at,
        media_type: media.to_string(),
        previewable,
    })
}

/// One slice of a file, read without keeping state between calls.
pub fn slice_within(
    cwd: &std::path::Path,
    path: &str,
    offset: u64,
    length: u64,
) -> Result<FileSlice, EngineError> {
    use std::io::{Read, Seek, SeekFrom};

    let resolved = resolve_within(cwd, path)?;
    let size = std::fs::metadata(&resolved)
        .map_err(|error| unknown(path, &error))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(too_large(path, size));
    }
    if offset > size {
        return Err(EngineError::Upstream {
            code: "FILE_SLICE_INVALID".to_string(),
            message: format!("offset {offset} is past the end of {path} ({size} bytes)"),
            details: None,
        });
    }

    let wanted = length.clamp(1, MAX_SLICE_BYTES).min(size - offset);
    let mut buffer = vec![0u8; wanted as usize];
    let mut file = std::fs::File::open(&resolved).map_err(|error| unknown(path, &error))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| unknown(path, &error))?;
    let mut read = 0;
    while read < buffer.len() {
        match file.read(&mut buffer[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) => return Err(unknown(path, &error)),
        }
    }
    buffer.truncate(read);

    Ok(FileSlice {
        offset,
        length: read as u64,
        byte_size: size,
        data: buffer,
        eof: offset + read as u64 >= size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_file_inside_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let preview = read_within(dir.path(), "a.txt").unwrap();
        assert_eq!(preview.text.as_deref(), Some("hello"));
        assert_eq!(preview.name, "a.txt");
        assert!(!preview.truncated);
    }

    #[test]
    fn refuses_a_path_that_climbs_out() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_within(dir.path(), "../outside.txt").unwrap_err();
        assert_eq!(error.code(), "FILE_FORBIDDEN");
    }

    #[test]
    fn refuses_an_absolute_path_outside() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_within(dir.path(), "/etc/hosts").unwrap_err();
        assert_eq!(error.code(), "FILE_FORBIDDEN");
    }

    #[test]
    fn a_binary_file_has_no_text_but_still_answers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let preview = read_within(dir.path(), "blob.bin").unwrap();
        assert!(preview.text.is_none());
        assert_eq!(preview.size, 4);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_points_out_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "nope").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();
        let error = read_within(dir.path(), "link.txt").unwrap_err();
        assert_eq!(error.code(), "FILE_FORBIDDEN");
    }

    #[test]
    fn credentials_inside_the_workspace_are_refused_too() {
        let dir = tempfile::tempdir().unwrap();
        for name in [".env", ".env.local", "server.pem", "id_ed25519", ".npmrc"] {
            std::fs::write(dir.path().join(name), "secret").unwrap();
            let error = read_within(dir.path(), name).unwrap_err();
            assert_eq!(error.code(), "FILE_FORBIDDEN", "{name} should be refused");
            let error = stat_within(dir.path(), name).unwrap_err();
            assert_eq!(error.code(), "FILE_FORBIDDEN", "{name} should be refused");
        }
        // A file that merely contains the word is nobody's credential.
        std::fs::write(dir.path().join("environment.md"), "notes").unwrap();
        assert!(read_within(dir.path(), "environment.md").is_ok());
    }

    #[test]
    fn stat_names_the_type_and_the_time() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.md"), "# hi").unwrap();
        let stat = stat_within(dir.path(), "report.md").unwrap();
        assert_eq!(stat.name, "report.md");
        assert_eq!(stat.byte_size, 4);
        assert_eq!(stat.media_type, "text/markdown");
        assert!(stat.previewable);
        assert!(stat.modified_at > 0);

        std::fs::write(dir.path().join("blob.bin"), [0u8; 4]).unwrap();
        let unknown_type = stat_within(dir.path(), "blob.bin").unwrap();
        assert_eq!(unknown_type.media_type, "application/octet-stream");
        assert!(!unknown_type.previewable);
    }

    #[test]
    fn a_file_is_read_in_slices_that_end_where_it_does() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"0123456789").unwrap();

        let first = slice_within(dir.path(), "a.txt", 0, 4).unwrap();
        assert_eq!(first.data, b"0123");
        assert_eq!(first.length, 4);
        assert_eq!(first.byte_size, 10);
        assert!(!first.eof);

        let rest = slice_within(dir.path(), "a.txt", 4, 100).unwrap();
        assert_eq!(rest.data, b"456789");
        assert!(rest.eof);

        // The end of a file is a zero-length slice that says so, not an error.
        let end = slice_within(dir.path(), "a.txt", 10, 4).unwrap();
        assert_eq!(end.length, 0);
        assert!(end.eof);

        // Past the end is a mistake the caller should see.
        let error = slice_within(dir.path(), "a.txt", 11, 4).unwrap_err();
        assert_eq!(error.code(), "FILE_SLICE_INVALID");
    }

    #[test]
    fn a_file_that_is_not_there_is_unknown_rather_than_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        let error = stat_within(dir.path(), "missing.md").unwrap_err();
        assert_eq!(error.code(), "FILE_UNKNOWN");
    }
}
