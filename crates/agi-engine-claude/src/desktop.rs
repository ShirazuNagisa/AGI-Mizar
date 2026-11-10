//! The Claude desktop app's local Code tab.
//!
//! Claude Desktop's Code tab is not a second conversation store: it runs the
//! bundled Claude Code CLI against the same `~/.claude` directory. The only
//! desktop-specific fact is the session metadata it keeps beside its own
//! profile (`local_*.json`), which gives a nicer title and tells the window that
//! this transcript came from the GUI rather than a terminal.
//!
//! The file is a private app format. A missing or unreadable file is an answer,
//! not an error: the transcript is still readable without it, and an app update
//! that moves the file must only lose the badge, never the session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One `local_*.json` row.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSession {
    /// Claude Desktop's own id.
    #[serde(default)]
    pub session_id: String,
    /// The id of the transcript the bundled Claude Code CLI wrote.
    #[serde(default)]
    pub cli_session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub completed_turns: u32,
    #[serde(default)]
    pub last_activity_at: i64,
    #[serde(default)]
    pub is_archived: bool,
}

/// The app-data directories a Claude desktop install keeps its profile in.
///
/// The directory is deliberately not hard-coded to `Claude-3p`: normal Claude
/// installs use `Claude`, and a 3p deployment uses `Claude-3p`. Scanning both is
/// cheap and means the app works after either install path.
///
/// Where that directory lives is the platform's convention, not this app's
/// choice: Claude Desktop is an Electron app, so its profile sits in
/// `~/Library/Application Support` on macOS and in `%APPDATA%` on Windows. A
/// Windows port that kept the macOS spelling could never find a session — the
/// path does not exist there — and the Code tab's rows would silently lose their
/// desktop titles and the "Claude 桌面" source badge.
fn app_support_dirs() -> Vec<PathBuf> {
    let home = agi_platform::home();
    #[cfg(target_os = "macos")]
    let bases = [home.join("Library/Application Support")];
    #[cfg(windows)]
    let bases = [std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData").join("Roaming"))];
    // Linux has no product here, but the same two spellings are what an Electron
    // build would use, and guessing them costs one `read_dir` that finds nothing.
    #[cfg(not(any(target_os = "macos", windows)))]
    let bases = [std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))];

    let mut dirs = Vec::new();
    for base in bases {
        dirs.push(base.join("Claude-3p"));
        dirs.push(base.join("Claude"));
    }
    dirs
}

/// Every local Code session the desktop app knows, newest activity first.
pub fn read_all() -> Vec<DesktopSession> {
    let mut sessions = Vec::new();
    for root in app_support_dirs() {
        sessions.extend(read_from(&root.join("claude-code-sessions")));
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity_at));
    sessions
}

/// Read one root. Split out so tests can point at a temporary tree.
pub fn read_from(root: &Path) -> Vec<DesktopSession> {
    let mut sessions = Vec::new();
    let Ok(profiles) = std::fs::read_dir(root) else {
        return sessions;
    };
    for profile in profiles.flatten() {
        let profile_dir = profile.path();
        if !profile_dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&profile_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&dir) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                let name = file.file_name().to_string_lossy().to_string();
                if !name.starts_with("local_") || !name.ends_with(".json") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(mut session) = serde_json::from_str::<DesktopSession>(&text) else {
                    continue;
                };
                if session.cli_session_id.trim().is_empty() {
                    continue;
                }
                if session.session_id.trim().is_empty() {
                    session.session_id = name.trim_end_matches(".json").to_string();
                }
                sessions.push(session);
            }
        }
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity_at));
    sessions
}

/// Index by the transcript id the Claude engine already uses.
pub fn by_cli_session(sessions: Vec<DesktopSession>) -> HashMap<String, DesktopSession> {
    let mut out = HashMap::new();
    for session in sessions {
        out.entry(session.cli_session_id.clone()).or_insert(session);
    }
    out
}

/// The app-data root, for the log line and for damage control.
pub fn profile_root() -> Option<PathBuf> {
    app_support_dirs().into_iter().find(|dir| dir.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, profile: &str, session: &str, name: &str, json: &str) {
        let dir = root.join(profile).join(session);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), json).unwrap();
    }

    #[test]
    fn reads_local_sessions_and_maps_the_transcript_id() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "profile",
            "s1",
            "local_abc.json",
            r#"{
                "sessionId": "local_abc",
                "cliSessionId": "11111111-1111-4111-8111-111111111111",
                "cwd": "/tmp/project",
                "title": "A desktop session",
                "model": "claude-sonnet",
                "completedTurns": 4,
                "lastActivityAt": 200,
                "isArchived": false
            }"#,
        );
        write(
            dir.path(),
            "profile",
            "s2",
            "local_def.json",
            r#"{
                "sessionId": "local_def",
                "cliSessionId": "22222222-2222-4222-8222-222222222222",
                "title": "Older",
                "completedTurns": 1,
                "lastActivityAt": 100
            }"#,
        );
        // A file that looks like a session but is not one: no cliSessionId.
        write(
            dir.path(),
            "profile",
            "s3",
            "local_bad.json",
            r#"{"sessionId": "local_bad"}"#,
        );

        let sessions = read_from(dir.path());
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "local_abc");
        assert_eq!(sessions[0].completed_turns, 4);

        let index = by_cli_session(sessions);
        let found = index
            .get("11111111-1111-4111-8111-111111111111")
            .expect("the transcript id is the key");
        assert_eq!(found.title.as_deref(), Some("A desktop session"));
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_from(&dir.path().join("does-not-exist")).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_profile_lives_in_application_support() {
        let dirs = app_support_dirs();
        assert!(
            dirs.iter().any(|dir| dir
                .to_string_lossy()
                .ends_with("Library/Application Support/Claude-3p")),
            "{dirs:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_profile_lives_in_appdata() {
        // Claude Desktop is Electron: on Windows its profile is `%APPDATA%\Claude`,
        // not the macOS `Library/Application Support` spelling.
        let Some(appdata) = std::env::var_os("APPDATA") else {
            return;
        };
        let expected = std::path::Path::new(&appdata).join("Claude");
        let dirs = app_support_dirs();
        assert!(dirs.contains(&expected), "{dirs:?}");
        assert!(
            dirs.iter().any(|dir| dir.ends_with("Claude-3p")),
            "the 3p profile is scanned too: {dirs:?}"
        );
        assert!(
            !dirs
                .iter()
                .any(|dir| dir.to_string_lossy().contains("Application Support")),
            "a macOS path can never exist on Windows: {dirs:?}"
        );
    }
}
