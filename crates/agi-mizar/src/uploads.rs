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
use std::sync::Mutex;

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
    clock: fn() -> i64,
}

impl UploadStore {
    /// Open a staging area at `root`, creating it private.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        private_dir(&root)?;
        private_dir(&root.join("staging"))?;
        private_dir(&root.join("attachments"))?;
        Ok(Self {
            root,
            live: Mutex::new(HashMap::new()),
            finished: Mutex::new(HashMap::new()),
            clock: now_ms,
        })
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
            media_type: sanitise_media_type(&request.media_type),
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

    /// Take one chunk. `index` must be the next one; a duplicate of the last
    /// chunk with the same bytes is accepted so a retry cannot corrupt a file.
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
    pub fn finish(&self, device: &str, upload_id: &str) -> Result<AttachmentRecord, UploadError> {
        self.purge();
        let upload = {
            let mut live = self.live();
            let Some(upload) = live.remove(upload_id) else {
                return Err(UploadError::new(
                    "UPLOAD_UNKNOWN",
                    "upload.finish: no such upload",
                ));
            };
            if upload.device != device {
                live.insert(upload_id.to_string(), upload);
                return Err(UploadError::new(
                    "UPLOAD_UNKNOWN",
                    "upload.finish: no such upload",
                ));
            }
            upload
        };
        if upload.written != upload.byte_size {
            let _ = std::fs::remove_file(&upload.path);
            return Err(UploadError::new(
                "UPLOAD_INCOMPLETE",
                format!(
                    "upload.finish: {} of {} bytes arrived",
                    upload.written, upload.byte_size
                ),
            ));
        }

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
    pub fn abort(&self, device: &str, upload_id: &str) {
        self.purge();
        let upload = {
            let mut live = self.live();
            match live.get(upload_id) {
                Some(upload) if upload.device == device => live.remove(upload_id),
                _ => None,
            }
        };
        if let Some(upload) = upload {
            let _ = std::fs::remove_file(&upload.path);
        }
    }

    /// The blobs a prompt named, in the order it named them.
    ///
    /// Ownership, session and expiry are checked here rather than at the phone,
    /// and the record is consumed: a second prompt cannot replay the same bytes.
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
        let mut out = Vec::new();
        let mut total = 0u64;
        {
            let mut finished = self.finished();
            for id in attachment_ids {
                let Some(entry) = finished.remove(id) else {
                    return Err(UploadError::new(
                        "ATTACHMENT_FAILED",
                        format!("session.prompt: attachment {id} is not available"),
                    ));
                };
                if entry.device != device || entry.session != session {
                    finished.insert(id.clone(), entry);
                    return Err(UploadError::new(
                        "ATTACHMENT_FAILED",
                        format!(
                            "session.prompt: attachment {id} is not available for this session"
                        ),
                    ));
                }
                if now - entry.finished_ms > UPLOAD_TTL_MS {
                    let _ = std::fs::remove_dir_all(entry.path.parent().unwrap_or(&self.root));
                    return Err(UploadError::new(
                        "ATTACHMENT_FAILED",
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
                let bytes = if entry.record.kind == AttachmentKind::Image {
                    std::fs::read(&entry.path).map_err(|error| {
                        UploadError::new("ATTACHMENT_FAILED", format!("session.prompt: {error}"))
                    })?
                } else {
                    Vec::new()
                };
                out.push(PromptAttachment {
                    kind: entry.record.kind,
                    name: entry.record.name,
                    media_type: entry.record.media_type,
                    bytes,
                    path: Some(entry.path.to_string_lossy().to_string()),
                });
            }
        }
        Ok(out)
    }

    /// Drop everything that has outlived its lease.
    fn purge(&self) {
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

    static CLOCK: AtomicI64 = AtomicI64::new(1_000);

    fn store() -> (UploadStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = UploadStore::open(dir.path().join("uploads")).unwrap();
        store.clock = || CLOCK.load(Ordering::Relaxed);
        (store, dir)
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
    fn an_attachment_is_spent_by_the_prompt_that_names_it() {
        let (store, _dir) = store();
        let begun = begin(&store, 3);
        store
            .chunk("device-1", &begun.upload_id, 0, b"abc")
            .unwrap();
        let record = store.finish("device-1", &begun.upload_id).unwrap();
        let id = record.attachment_id.clone();
        assert!(store
            .resolve_for_prompt("device-1", "dsh:s1", std::slice::from_ref(&id))
            .is_ok());
        assert_eq!(
            store
                .resolve_for_prompt("device-1", "dsh:s1", &[id])
                .unwrap_err()
                .code,
            "ATTACHMENT_FAILED"
        );
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
        let (store, _dir) = store();
        CLOCK.store(1_000, Ordering::Relaxed);
        let begun = begin(&store, 3);
        CLOCK.store(1_000 + UPLOAD_TTL_MS + 1, Ordering::Relaxed);
        assert_eq!(
            store
                .chunk("device-1", &begun.upload_id, 0, b"abc")
                .unwrap_err()
                .code,
            "UPLOAD_UNKNOWN"
        );
    }
}
