//! The device roster.
//!
//! One JSON file in the state directory, and one rule that matters: **only a hash
//! of each device token is written down**. A state file that leaks — a backup, a
//! synced folder, somebody's screenshot of a directory listing — does not hand
//! out access to the computer. The token itself exists exactly once, in the
//! response to the pairing request that created it.
//!
//! ### Why a file and not SQLite
//!
//! Three hundred devices would be a lot. A file is readable by a person debugging
//! at two in the morning, which is worth more than the query planner.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One paired phone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// SHA-256 of the token, hex. Never the token.
    #[serde(rename = "tokenHash")]
    pub token_hash: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "lastSeenAt", default)]
    pub last_seen_at: i64,
    /// Set when the operator removed this device.
    #[serde(rename = "revokedAt", default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<i64>,
}

impl Device {
    /// Whether this device may still connect.
    pub fn is_live(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// What the file holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileContents {
    #[serde(rename = "hostId")]
    host_id: String,
    #[serde(rename = "hostName", default)]
    host_name: String,
    #[serde(default)]
    devices: Vec<Device>,
}

/// The roster.
pub struct DeviceStore {
    path: PathBuf,
    state: Mutex<FileContents>,
}

/// A token handed out once, at pairing time.
#[derive(Debug, Clone)]
pub struct Issued {
    pub device: Device,
    /// The plaintext token. Not stored anywhere.
    pub token: String,
}

impl DeviceStore {
    /// Open the roster, creating it — with a new host id — when it is missing.
    pub fn open(path: impl Into<PathBuf>, host_name: impl Into<String>) -> io::Result<Arc<Self>> {
        let path = path.into();
        let state = match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                serde_json::from_str(&text).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} is not a device roster: {error}", path.display()),
                    )
                })?
            }
            _ => FileContents {
                host_id: uuid::Uuid::new_v4().to_string(),
                host_name: host_name.into(),
                devices: Vec::new(),
            },
        };

        let store = Arc::new(Self {
            path,
            state: Mutex::new(state),
        });
        // Written immediately so a host id exists before the first phone asks.
        store.save()?;
        Ok(store)
    }

    fn state(&self) -> MutexGuard<'_, FileContents> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// This computer's stable identity.
    pub fn host_id(&self) -> String {
        self.state().host_id.clone()
    }

    /// The name shown on the phone.
    pub fn host_name(&self) -> String {
        self.state().host_name.clone()
    }

    /// Replace the display name, when the machine was renamed.
    pub fn set_host_name(&self, name: &str) -> io::Result<()> {
        {
            let mut state = self.state();
            if state.host_name == name {
                return Ok(());
            }
            state.host_name = name.to_string();
        }
        self.save()
    }

    /// Pair one device, returning its token exactly once.
    pub fn issue(&self, name: &str, model: Option<String>) -> io::Result<Issued> {
        let token = format!("mz_{}", uuid::Uuid::new_v4().simple());
        let device = Device {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            model,
            token_hash: hash_token(&token),
            created_at: crate::pairing::now_ms(),
            last_seen_at: 0,
            revoked_at: None,
        };
        {
            self.state().devices.push(device.clone());
        }
        self.save()?;
        Ok(Issued { device, token })
    }

    /// Find the live device behind a token, and record that it was seen.
    ///
    /// Returns `None` for an unknown token **and** for a revoked one: the caller
    /// must not be able to tell those apart, or a revoked device learns that its
    /// old token was once real.
    pub fn authenticate(&self, token: &str) -> Option<Device> {
        let hash = hash_token(token);
        let mut state = self.state();
        let device = state
            .devices
            .iter_mut()
            .find(|device| device.is_live() && device.token_hash == hash)?;
        device.last_seen_at = crate::pairing::now_ms();
        let snapshot = device.clone();
        drop(state);
        // A failed write is not a reason to refuse a valid token.
        let _ = self.save();
        Some(snapshot)
    }

    /// Look a device up by token without recording a visit.
    pub fn lookup(&self, token: &str) -> Option<Device> {
        let hash = hash_token(token);
        self.state()
            .devices
            .iter()
            .find(|device| device.is_live() && device.token_hash == hash)
            .cloned()
    }

    /// Revoke one device. Returns whether it was live.
    pub fn revoke(&self, id: &str) -> io::Result<bool> {
        let revoked = {
            let mut state = self.state();
            match state.devices.iter_mut().find(|device| device.id == id) {
                Some(device) if device.is_live() => {
                    device.revoked_at = Some(crate::pairing::now_ms());
                    true
                }
                _ => false,
            }
        };
        if revoked {
            self.save()?;
        }
        Ok(revoked)
    }

    /// Every device, revoked ones included.
    pub fn devices(&self) -> Vec<Device> {
        self.state().devices.clone()
    }

    /// Write the roster atomically, owner-readable only.
    fn save(&self) -> io::Result<()> {
        let text = {
            let state = self.state();
            serde_json::to_string_pretty(&*state)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, format!("{text}\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&temp, &self.path)
    }
}

/// SHA-256 of a token, hex encoded.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The default roster path.
pub fn default_store_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".agi-mizar").join("devices.json")
}

/// The machine's name, as a person would recognise it.
pub fn host_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "this computer".to_string())
}

/// The address a phone on the same network would dial.
///
/// Found by asking the OS which local address it would use to reach the outside
/// world. `UdpSocket::connect` sends nothing; it only resolves the route.
pub fn local_address() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Arc<DeviceStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(dir.path().join("devices.json"), "test-host").unwrap();
        (store, dir)
    }

    #[test]
    fn creates_a_host_identity_on_first_open() {
        let (store, _dir) = store();
        assert!(!store.host_id().is_empty());
        assert_eq!(store.host_name(), "test-host");

        // Reopening keeps the same identity.
        let path = store.path.clone();
        let again = DeviceStore::open(&path, "another-name").unwrap();
        assert_eq!(again.host_id(), store.host_id());
    }

    #[test]
    fn a_token_is_returned_once_and_stored_only_as_a_hash() {
        let (store, dir) = store();
        let issued = store
            .issue("iPhone", Some("iPhone17,1".to_string()))
            .unwrap();

        let text = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(
            !text.contains(&issued.token),
            "the token must not be on disk"
        );
        assert!(text.contains(&hash_token(&issued.token)));

        assert!(store.authenticate(&issued.token).is_some());
        assert!(store.authenticate("mz_nope").is_none());
    }

    #[test]
    fn a_revoked_device_cannot_authenticate() {
        let (store, _dir) = store();
        let issued = store.issue("iPhone", None).unwrap();
        assert!(store.revoke(&issued.device.id).unwrap());

        assert!(store.authenticate(&issued.token).is_none());
        // Revoking twice reports that nothing changed.
        assert!(!store.revoke(&issued.device.id).unwrap());
    }

    #[test]
    fn authentication_records_when_the_device_was_seen() {
        let (store, _dir) = store();
        let issued = store.issue("iPhone", None).unwrap();
        assert_eq!(store.devices()[0].last_seen_at, 0);

        store.authenticate(&issued.token).unwrap();
        assert!(store.devices()[0].last_seen_at > 0);
    }

    #[test]
    fn two_devices_get_different_tokens() {
        let (store, _dir) = store();
        let a = store.issue("iPhone", None).unwrap();
        let b = store.issue("Pixel", None).unwrap();
        assert_ne!(a.token, b.token);
        assert_ne!(a.device.id, b.device.id);
        assert_eq!(store.devices().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn the_roster_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (store, dir) = store();
        let mode = std::fs::metadata(dir.path().join("devices.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(store);
    }
}
