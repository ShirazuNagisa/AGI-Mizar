//! The file the DSH helper plugin reads to find us.
//!
//! It carries a loopback port and a per-launch token. Both are regenerated every
//! time this application starts, which is the point: a token that outlives a
//! launch is a token that can be replayed by anything that read the file once.
//!
//! Written atomically (temp file plus rename) and readable only by its owner
//! (`0600` on Unix; the profile ACL on Windows, which is the same guarantee
//! through a different mechanism).

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What the plugin reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointFile {
    pub port: u16,
    pub token: String,
    /// Advisory: lets a human tell which launch wrote the file.
    pub pid: u32,
}

/// `~/.agi-mizar/endpoint.json`.
pub fn default_endpoint_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".agi-mizar").join("endpoint.json")
}

/// Publish the endpoint atomically.
///
/// A reader that catches the file mid-write would see truncated JSON and treat it
/// as "no desktop", which is exactly the wrong answer while we are starting.
pub fn write_endpoint_file(path: &Path, port: u16, token: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let contents = serde_json::to_vec_pretty(&EndpointFile {
        port,
        token: token.to_string(),
        pid: std::process::id(),
    })?;

    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, &contents)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Before the rename, so the file is never briefly world-readable.
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }

    std::fs::rename(&temp, path)
}

/// Remove the endpoint, on shutdown.
///
/// Best effort: a file left behind makes the plugin dial a port nobody listens on
/// and back off, which is recoverable. Failing to shut down cleanly is not worth
/// an error path.
pub fn remove_endpoint_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_file_the_plugin_can_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("endpoint.json");
        write_endpoint_file(&path, 51_234, "tok").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: EndpointFile = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.port, 51_234);
        assert_eq!(parsed.token, "tok");
        assert!(parsed.pid > 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("endpoint.json");
        write_endpoint_file(&path, 1, "tok").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn removal_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("endpoint.json");
        write_endpoint_file(&path, 1, "tok").unwrap();
        remove_endpoint_file(&path);
        remove_endpoint_file(&path);
        assert!(!path.exists());
    }
}
