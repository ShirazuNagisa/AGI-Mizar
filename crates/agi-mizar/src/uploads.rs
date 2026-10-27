//! The staging area for files a phone sends.
//!
//! Ported from `dsh` v0.8.12 `src/bridge/uploads.ts`, with the parts that need a
//! composed host service left out (this desktop has no `fileUploads`): an image
//! is carried inline in the prompt, and a document is saved somewhere durable and
//! named in a text part, which is the one handle every agent already understands.
//!
//! Four properties hold the safety of it:
//!
//! - **An upload belongs to one device and one session.** The device comes from
//!   the connection, never from the payload, so no field a phone sends can name
//!   somebody else's upload.
//! - **Chunks are sequential and the size is the one declared.** A chunk out of
//!   order, a chunk that would push the file past its declared size, and a
//!   `finish` whose byte count disagrees are all refused.
//! - **Everything expires.** An upload lives 30 minutes from its first byte and a
//!   finished attachment 30 minutes from its finish; a phone that lost its
//!   connection must not leave a 32 MiB file on this disk forever.
//! - **The staging area is private.** Directories are created 0700 and files
//!   0600 before the first byte lands.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agi_protocol::domain::{AttachmentKind, PromptAttachment};
use serde::{Deserialize, Serialize};

/// Bytes per upload chunk; the app sends exactly this except for the last one.
pub const CHUNK_BYTES: u64 = 128 * 1024;

/// How long an upload or a finished attachment is kept.
pub const UPLOAD_TTL_MS: i64 = 30 * 60 * 1000;

/// Largest image, in bytes.
pub const IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// Largest document, in bytes.
pub const FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Largest total of all attachments in one prompt.
pub const PROMPT_BYTES: u64 = 64 * 1024 * 1024;

/// The image media types a prompt can carry inline.
pub const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// How many attachments one prompt may carry.
pub const PROMPT_ATTACHMENTS: usize = 8;

/// How many uploads one device may have live at once.
pub const LIVE_PER_DEVICE: usize = 4;

/// What `upload.begin` takes.
pub struct BeginRequest {
    pub session: String,
    pub kind: AttachmentKind,
    pub name: String,
    pub media_type: String,
    pub byte_size: u64,
}

/// What `upload.begin` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BeginResult {
    pub upload_id: String,
    pub chunk_bytes: u64,
}

/// What `upload.finish` answers — the record a prompt refers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRecord {
    pub attachment_id: String,
    pub kind: AttachmentKind,
    pub name: String,
    pub media_type: String,
    pub byte_size: u64,
}

/// Why an upload call was refused. Each maps to a wire code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadError {
    pub code: &'static str,
    pub message: String,
}

impl UploadError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// One upload in flight.
struct LiveUpload {
    device: String,
    session: String,
    kind: AttachmentKind,
    name: String,
    media_type: String,
    byte_size: u64,
    received: u64,
    /// Bytes accepted so far, so a short read at finish is explainable.
    written: u64,
    path: PathBuf,
    started_ms: i64,
}

/// One finished attachment waiting for its prompt.
struct Finished {
    record: AttachmentRecord,
    device: String,
    session: String,
    path: PathBuf,
    finished_ms: i64,
}

/// The staging area.
pub struct UploadStore {
    root: PathBuf,
    live: Mutex<HashMap<String, LiveUpload>>,
    finished: Mutex<HashMap<String, Finished>>,
    /// Injected so a test can move time without moving the whole process's.
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl UploadStore {
    /// Open a staging area at `root`, creating it private.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        Self::open_with_clock(root, Arc::new(now_ms))
    }

    /// Open with a clock the caller owns, which is what the tests do.
    fn open_with_clock(
        root: impl Into<PathBuf>,
        clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> std::io::Result<Self> {
        let root = root.into();
        private_dir(&root)?;
        private_dir(&root.join("staging"))?;
        private_dir(&root.join("attachments"))?;
        let store = Self {
            root,
            live: Mutex::new(HashMap::new()),
            finished: Mutex::new(HashMap::new()),
            clock,
        };
        // A restart forgets every in-memory record, so whatever a previous run
        // left behind has to be reclaimed from the disk itself.
        store.sweep_orphans();
        Ok(store)
    }

    /// The default staging area beside the device roster.
    pub fn default_root(agi_home: &Path) -> PathBuf {
        agi_home.join("uploads")
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, LiveUpload>> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn finished(&self) -> std::sync::MutexGuard<'_, HashMap<String, Finished>> {
        self.finished
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start an upload, reserving its declared size.
    pub fn begin(&self, device: &str, request: BeginRequest) -> Result<BeginResult, UploadError> {
        self.purge();
        let limit = match request.kind {
            AttachmentKind::Image => IMAGE_BYTES,
            AttachmentKind::File => FILE_BYTES,
        };
        if request.byte_size == 0 {
            return Err(UploadError::new(
                "MALFORMED_FRAME",
                "upload.begin: an upload needs at least one byte",
            ));
        }
        if request.byte_size > limit {
            return Err(UploadError::new(
                "UPLOAD_TOO_LARGE",
                format!(
                    "upload.begin: {} bytes is over the {} limit",
                    request.byte_size, limit
                ),
            ));
        }
        let media_type = sanitise_media_type(&request.media_type);
        if request.kind == AttachmentKind::Image
            && !IMAGE_MEDIA_TYPES.contains(&media_type.as_str())
        {
            return Err(UploadError::new(
                "ATTACHMENT_UNSUPPORTED",
                format!("upload.begin: {media_type} is not an image this host accepts"),
            ));
        }
        let name = safe_name(&request.name)?;
        {
            let live = self.live();
            let mine = live
                .values()
                .filter(|upload| upload.device == device)
                .count();
            if mine >= LIVE_PER_DEVICE {
                return Err(UploadError::new(
                    "UPLOAD_LIMITED",
                    "upload.begin: this device already has as many uploads as it may",
                ));
            }
        }

        let upload_id = uuid::Uuid::new_v4().to_string();
        let path = self.root.join("staging").join(&upload_id);
        std::fs::File::create(&path)
            .and_then(|file| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
                Ok(file)
            })
            .map_err(|error| UploadError::new("UPLOAD_FAILED", format!("upload.begin: {error}")))?;

        let upload = LiveUpload {
            device: device.to_string(),
            session: request.session,
            kind: request.kind,
            name,
            media_type,
            byte_size: request.byte_size,
            received: 0,
            written: 0,
            path,
            started_ms: (self.clock)(),
        };
        self.live().insert(upload_id.clone(), upload);
        Ok(BeginResult {
            upload_id,
            chunk_bytes: CHUNK_BYTES,
        })
    }

    /// Take one chunk. `index` must be the next one; a chunk that was already
    /// received, or one that would pass the declared size, is refused — this
    /// implementation writes sequentially and never compares bytes.
    pub fn chunk(
        &self,
        device: &str,
        upload_id: &str,
        index: u64,
        bytes: &[u8],
    ) -> Result<u64, UploadError> {
        self.purge();
        let mut live = self.live();
        let Some(upload) = live.get_mut(upload_id) else {
            return Err(UploadError::new(
                "UPLOAD_UNKNOWN",
                "upload.chunk: no such upload",
            ));
        };
        if upload.device != device {
            return Err(UploadError::new(
                "UPLOAD_UNKNOWN",
                "upload.chunk: no such upload",
            ));
        }
        if index < upload.received {
            return Err(UploadError::new(
                "UPLOAD_BAD_CHUNK",
                format!("upload.chunk: chunk {index} was already received"),
            ));
        }
        if index > upload.received {
            return Err(UploadError::new(
                "UPLOAD_BAD_CHUNK",
                format!(
                    "upload.chunk: expected chunk {}, got {index}",
                    upload.received
                ),
            ));
        }
        if bytes.is_empty() {
            return Err(UploadError::new(
                "UPLOAD_BAD_CHUNK",
                "upload.chunk: an empty chunk is not a chunk",
            ));
        }
        if bytes.len() as u64 > CHUNK_BYTES {
            return Err(UploadError::new(
                "UPLOAD_BAD_CHUNK",
                "upload.chunk: a chunk is larger than the agreed size",
            ));
        }
        if upload.written + bytes.len() as u64 > upload.byte_size {
            return Err(UploadError::new(
                "UPLOAD_TOO_LARGE",
                "upload.chunk: more bytes than the upload declared",
            ));
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&upload.path)
            .map_err(|error| UploadError::new("UPLOAD_FAILED", format!("upload.chunk: {error}")))?;
        file.write_all(bytes)
            .map_err(|error| UploadError::new("UPLOAD_FAILED", format!("upload.chunk: {error}")))?;
        upload.written += bytes.len() as u64;
        upload.received += 1;
        Ok(upload.received)
    }

    /// Close an upload and keep its bytes for a prompt.
    ///
    /// An upload that is short of its declared size stays open: the client can
    /// send the rest and finish again, and only success destroys the record.
    pub fn finish(&self, device: &str, upload_id: &str) -> Result<AttachmentRecord, UploadError> {
        self.purge();
        let upload = {
            let mut live = self.live();
            let Some(upload) = live.get(upload_id) else {
                return Err(UploadError::new(
                    "UPLOAD_UNKNOWN",
                    "upload.finish: no such upload",
                ));
            };
            if upload.device != device {
                return Err(UploadError::new(
                    "UPLOAD_UNKNOWN",
                    "upload.finish: no such upload",
                ));
            }
            if upload.written != upload.byte_size {
                return Err(UploadError::new(
                    "UPLOAD_INCOMPLETE",
                    format!(
                        "upload.finish: {} of {} bytes arrived",
                        upload.written, upload.byte_size
                    ),
                ));
            }
            live.remove(upload_id)
                .expect("the upload was just read from this map")
        };

        let attachment_id = uuid::Uuid::new_v4().to_string();
        let directory = self.root.join("attachments").join(&attachment_id);
        private_dir(&directory).map_err(|error| {
            UploadError::new("UPLOAD_FAILED", format!("upload.finish: {error}"))
        })?;
        let path = directory.join(&upload.name);
        std::fs::rename(&upload.path, &path).map_err(|error| {
            UploadError::new("UPLOAD_FAILED", format!("upload.finish: {error}"))
        })?;

        let record = AttachmentRecord {
            attachment_id: attachment_id.clone(),
            kind: upload.kind,
            name: upload.name,
            media_type: upload.media_type,
            byte_size: upload.byte_size,
        };
        self.finished().insert(
            attachment_id,
            Finished {
                record: record.clone(),
                device: upload.device,
                session: upload.session,
                path,
                finished_ms: (self.clock)(),
            },
        );
        Ok(record)
    }

    /// Throw a half-sent upload away.
    pub fn abort(&self, device: &str, upload_id: &str) -> bool {
        self.purge();
        let upload = {
            let mut live = self.live();
            match live.get(upload_id) {
                Some(upload) if upload.device == device => live.remove(upload_id),
                _ => None,
            }
        };
        match upload {
            Some(upload) => {
                let _ = std::fs::remove_file(&upload.path);
                true
            }
            None => false,
        }
    }

    /// The blobs a prompt named, in the order it named them.
    ///
    /// Read-only. The records are kept until [`consume`] is told the engine took
    /// the prompt, so a failure or a retry with the same `requestId` finds them
    /// still there — consuming first is how an attachment is lost when the host
    /// refuses a turn.
    pub fn resolve_for_prompt(
        &self,
        device: &str,
        session: &str,
        attachment_ids: &[String],
    ) -> Result<Vec<PromptAttachment>, UploadError> {
        self.purge();
        if attachment_ids.len() > PROMPT_ATTACHMENTS {
            return Err(UploadError::new(
                "MALFORMED_FRAME",
                format!("session.prompt: at most {PROMPT_ATTACHMENTS} attachments are allowed"),
            ));
        }
        let now = (self.clock)();
        let mut seen = std::collections::HashSet::new();
        let mut total = 0u64;
        let mut chosen = Vec::new();
        {
            let finished = self.finished();
            for id in attachment_ids {
                if !seen.insert(id) {
                    return Err(UploadError::new(
                        "MALFORMED_FRAME",
                        format!("session.prompt: attachment {id} is listed more than once"),
                    ));
                }
                let Some(entry) = finished.get(id) else {
                    return Err(UploadError::new(
                        "ATTACHMENT_UNKNOWN",
                        format!("session.prompt: attachment {id} is not available"),
                    ));
                };
                if entry.device != device || entry.session != session {
                    return Err(UploadError::new(
                        "ATTACHMENT_UNKNOWN",
                        format!(
                            "session.prompt: attachment {id} is not available for this session"
                        ),
                    ));
                }
                if now.saturating_sub(entry.finished_ms) > UPLOAD_TTL_MS {
                    return Err(UploadError::new(
                        "ATTACHMENT_UNKNOWN",
                        format!("session.prompt: attachment {id} expired"),
                    ));
                }
                total += entry.record.byte_size;
                if total > PROMPT_BYTES {
                    return Err(UploadError::new(
                        "ATTACHMENT_TOO_LARGE",
                        "session.prompt: the attachments are larger than one prompt may carry",
                    ));
                }
                chosen.push((
                    entry.record.kind,
                    entry.record.name.clone(),
                    entry.record.media_type.clone(),
                    entry.path.clone(),
                ));
            }
        }
        // The bytes are read outside the lock: a 10 MiB image must not stop the
        // store from answering anybody else.
        chosen
            .into_iter()
            .map(|(kind, name, media_type, path)| {
                let bytes = if kind == AttachmentKind::Image {
                    std::fs::read(&path).map_err(|error| {
                        UploadError::new("ATTACHMENT_FAILED", format!("session.prompt: {error}"))
                    })?
                } else {
                    Vec::new()
                };
                Ok(PromptAttachment {
                    kind,
                    name,
                    media_type,
                    bytes,
                    path: Some(path.to_string_lossy().to_string()),
                })
            })
            .collect()
    }

    /// Spend the attachments an accepted prompt named.
    ///
    /// An image travelled inline, so its staged copy has no reader left; a
    /// document is the thing the agent was told to open, so it stays until the
    /// sweep takes it.
    pub fn consume(&self, device: &str, session: &str, attachment_ids: &[String]) {
        let mut finished = self.finished();
        for id in attachment_ids {
            let owned = finished
                .get(id)
                .is_some_and(|entry| entry.device == device && entry.session == session);
            if !owned {
                continue;
            }
            if let Some(entry) = finished.remove(id) {
                if entry.record.kind == AttachmentKind::Image {
                    if let Some(directory) = entry.path.parent() {
                        let _ = std::fs::remove_dir_all(directory);
                    }
                }
            }
        }
    }

    /// Drop everything that has outlived its lease.
    fn purge(&self) {
        self.sweep_orphans();
        let now = (self.clock)();
        let expired_live: Vec<String> = {
            let live = self.live();
            live.iter()
                .filter(|(_, upload)| now - upload.started_ms > UPLOAD_TTL_MS)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in expired_live {
            self.abort_any(&id);
        }
        let expired: Vec<(String, PathBuf)> = {
            let mut finished = self.finished();
            let ids: Vec<String> = finished
                .iter()
                .filter(|(_, entry)| now - entry.finished_ms > UPLOAD_TTL_MS)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    finished.remove(&id).map(|entry| {
                        (
                            id,
                            entry
                                .path
                                .parent()
                                .map(Path::to_path_buf)
                                .unwrap_or_default(),
                        )
                    })
                })
                .collect()
        };
        for (_, directory) in expired {
            if !directory.as_os_str().is_empty() {
                let _ = std::fs::remove_dir_all(directory);
            }
        }
    }

    /// Reclaim directories no live record points at any more.
    ///
    /// A consumed image is deleted by [`consume`]; this is what catches the rest
    /// — staging files from a previous run, and attachments whose records were
    /// dropped with the process.
    fn sweep_orphans(&self) {
        let now = (self.clock)();
        let staging = self.root.join("staging");
        if let Ok(entries) = std::fs::read_dir(&staging) {
            for entry in entries.flatten() {
                let Some(age) = mtime_ms(&entry.path()) else {
                    continue;
                };
                if now.saturating_sub(age) > UPLOAD_TTL_MS {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let live: std::collections::HashSet<String> = self.live().keys().cloned().collect();
        let finished: std::collections::HashSet<String> = self.finished().keys().cloned().collect();
        let attachments = self.root.join("attachments");
        if let Ok(entries) = std::fs::read_dir(&attachments) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if live.contains(&name) || finished.contains(&name) {
                    continue;
                }
                let Some(age) = mtime_ms(&entry.path()) else {
                    continue;
                };
                if now.saturating_sub(age) > UPLOAD_TTL_MS {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }

    fn abort_any(&self, upload_id: &str) {
        let upload = self.live().remove(upload_id);
        if let Some(upload) = upload {
            let _ = std::fs::remove_file(&upload.path);
        }
    }
}

/// The current epoch milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

/// When a path was last written, in epoch milliseconds.
fn mtime_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since.as_millis() as i64)
}

/// Create a directory only the current user can enter.
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A file name that cannot leave its directory.
fn safe_name(name: &str) -> Result<String, UploadError> {
    let trimmed = name.trim();
    let base = Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if base.is_empty() || base == "." || base == ".." {
        return Err(UploadError::new(
            "MALFORMED_FRAME",
            "upload.begin: the name is not a file name",
        ));
    }
    // Bidirectional controls and separators can make a name read as another
    // name in the one place it is shown — the prompt the agent receives.
    let base: String = base
        .chars()
        .filter(
            |character| !matches!(*character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'),
        )
        .collect();
    let sanitised: String = base
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '/' | '\\' | ':' | '\0') {
                '_'
            } else {
                character
            }
        })
        .take(120)
        .collect();
    if sanitised.is_empty() {
        return Err(UploadError::new(
            "MALFORMED_FRAME",
            "upload.begin: the name is not a file name",
        ));
    }
    // Windows reserves these as device names; a prefixed name is still readable
    // and cannot name a device on a filesystem that treats them specially.
    let stem = sanitised
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        return Ok(format!("_{sanitised}"));
    }
    Ok(sanitised)
}

/// Keep a media type to something that fits a field.
fn sanitise_media_type(media_type: &str) -> String {
    let cleaned: String = media_type
        .chars()
        .filter(|character| character.is_ascii_graphic() || *character == '/')
        .take(120)
        .collect();
    if cleaned.is_empty() {
        "application/octet-stream".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn store_at(clock: Arc<AtomicI64>) -> (UploadStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = UploadStore::open_with_clock(
            dir.path().join("uploads"),
            Arc::new(move || clock.load(Ordering::Relaxed)),
        )
        .unwrap();
        (store, dir)
    }

    fn store() -> (UploadStore, tempfile::TempDir) {
        store_at(Arc::new(AtomicI64::new(1_000)))
    }

    fn begin(store: &UploadStore, size: u64) -> BeginResult {
        store
            .begin(
                "device-1",
                BeginRequest {
                    session: "dsh:s1".to_string(),
                    kind: AttachmentKind::Image,
                    name: "photo.png".to_string(),
                    media_type: "image/png".to_string(),
                    byte_size: size,
                },
            )
            .unwrap()
    }

    #[test]
    fn a_photo_arrives_in_order_and_becomes_an_attachment() {
        let (store, _dir) = store();
        let begun = begin(&store, 6);
        assert_eq!(
            store
                .chunk("device-1", &begun.upload_id, 0, b"abc")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .chunk("device-1", &begun.upload_id, 1, b"def")
                .unwrap(),
            2
        );
        let record = store.finish("device-1", &begun.upload_id).unwrap();
        assert_eq!(record.byte_size, 6);
        assert_eq!(record.kind, AttachmentKind::Image);

        let resolved = store
            .resolve_for_prompt(
                "device-1",
                "dsh:s1",
                std::slice::from_ref(&record.attachment_id),
            )
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].bytes, b"abcdef");
    }

    #[test]
    fn a_chunk_out_of_order_or_over_the_declared_size_is_refused() {
        let (store, _dir) = store();
        let begun = begin(&store, 4);
        assert_eq!(
            store
                .chunk("device-1", &begun.upload_id, 1, b"ab")
                .unwrap_err()
                .code,
            "UPLOAD_BAD_CHUNK"
        );
        assert_eq!(
            store
                .chunk("device-1", &begun.upload_id, 0, b"abc")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .chunk("device-1", &begun.upload_id, 1, b"de")
                .unwrap_err()
                .code,
            "UPLOAD_TOO_LARGE"
        );
    }

    #[test]
    fn a_short_upload_cannot_be_finished() {
        let (store, _dir) = store();
        let begun = begin(&store, 6);
        store
            .chunk("device-1", &begun.upload_id, 0, b"abc")
            .unwrap();
        assert_eq!(
            store.finish("device-1", &begun.upload_id).unwrap_err().code,
            "UPLOAD_INCOMPLETE"
        );
    }

    #[test]
    fn another_device_cannot_touch_an_upload() {
        let (store, _dir) = store();
        let begun = begin(&store, 3);
        assert_eq!(
            store
                .chunk("device-2", &begun.upload_id, 0, b"abc")
                .unwrap_err()
                .code,
            "UPLOAD_UNKNOWN"
        );
        assert_eq!(
            store.finish("device-2", &begun.upload_id).unwrap_err().code,
            "UPLOAD_UNKNOWN"
        );
    }

    #[test]
    fn an_attachment_survives_a_refused_prompt_and_is_spent_by_an_accepted_one() {
        let (store, _dir) = store();
        let begun = begin(&store, 3);
        store
            .chunk("device-1", &begun.upload_id, 0, b"abc")
            .unwrap();
        let record = store.finish("device-1", &begun.upload_id).unwrap();
        let id = record.attachment_id.clone();
        // A prompt that never reached the engine must not cost the attachment:
        // the client is allowed to retry with the same ids.
        assert!(store
            .resolve_for_prompt("device-1", "dsh:s1", std::slice::from_ref(&id))
            .is_ok());
        assert!(store
            .resolve_for_prompt("device-1", "dsh:s1", std::slice::from_ref(&id))
            .is_ok());
        store.consume("device-1", "dsh:s1", std::slice::from_ref(&id));
        assert_eq!(
            store
                .resolve_for_prompt("device-1", "dsh:s1", &[id])
                .unwrap_err()
                .code,
            "ATTACHMENT_UNKNOWN"
        );
    }

    #[test]
    fn a_consumed_image_leaves_no_bytes_behind() {
        let (store, _dir) = store();
        let begun = begin(&store, 3);
        store
            .chunk("device-1", &begun.upload_id, 0, b"abc")
            .unwrap();
        let record = store.finish("device-1", &begun.upload_id).unwrap();
        let path = store
            .finished()
            .get(&record.attachment_id)
            .unwrap()
            .path
            .clone();
        assert!(path.exists());
        store.consume(
            "device-1",
            "dsh:s1",
            std::slice::from_ref(&record.attachment_id),
        );
        assert!(!path.exists());
    }

    #[test]
    fn a_name_cannot_leave_the_staging_directory() {
        let (store, _dir) = store();
        let begun = store
            .begin(
                "device-1",
                BeginRequest {
                    session: "dsh:s1".to_string(),
                    kind: AttachmentKind::File,
                    name: "../../etc/passwd".to_string(),
                    media_type: "text/plain".to_string(),
                    byte_size: 3,
                },
            )
            .unwrap();
        store
            .chunk("device-1", &begun.upload_id, 0, b"abc")
            .unwrap();
        let record = store.finish("device-1", &begun.upload_id).unwrap();
        assert_eq!(record.name, "passwd");
    }

    #[test]
    fn an_upload_that_is_never_finished_expires() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let (store, _dir) = store_at(Arc::clone(&clock));
        let begun = begin(&store, 3);
        clock.store(1_000 + UPLOAD_TTL_MS + 1, Ordering::Relaxed);
        assert_eq!(
            store
                .chunk("device-1", &begun.upload_id, 0, b"abc")
                .unwrap_err()
                .code,
            "UPLOAD_UNKNOWN"
        );
    }
}
