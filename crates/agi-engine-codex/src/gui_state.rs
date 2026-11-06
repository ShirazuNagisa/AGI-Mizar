//! Read-only facts from the Codex desktop app's shared store.
//!
//! The desktop app and this adapter each own a `codex app-server` child, but
//! they share `~/.codex`. `thread/list` gives us the history; it cannot tell us
//! that a desktop window is running a turn in a thread this child never loaded
//! (`notLoaded` is explicitly not a status). The app's own SQLite projection can.
//!
//! Everything here is read-only: `mode=ro`, no migrations, no writes. A missing,
//! locked, or renamed database is not an error — the engine keeps the
//! `thread/list` answer and only loses the live badge.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

/// A turn older than this cannot still be the one holding the lamp.
const MAX_TURN_AGE: Duration = Duration::from_secs(6 * 60 * 60);
/// The history database must have been written recently for its in-progress
/// rows to be believable. A closed desktop app stops writing here.
const DB_FRESH_FOR: Duration = Duration::from_secs(10 * 60);

/// A snapshot of the desktop store's thread names and live turns.
#[derive(Debug, Default, Clone)]
pub struct GuiState {
    /// Threads the desktop app created (`originator = Codex Desktop`).
    pub desktop: HashSet<String>,
    /// Thread ids with a turn the desktop app says is in progress.
    pub running: HashSet<String>,
    /// Titles the app-server's `thread/list` may not carry.
    pub titles: HashMap<String, String>,
}

/// Read the live store, if there is one.
pub fn read() -> GuiState {
    let Some(home) = codex_home() else {
        return GuiState::default();
    };
    read_from(
        &home.join("state_5.sqlite"),
        &home.join("thread_history_1.sqlite"),
    )
}

/// Read one pair of databases. Split out so tests can use temporary files.
pub fn read_from(state_path: &Path, history_path: &Path) -> GuiState {
    let mut out = GuiState::default();
    if let Ok(connection) = open_read_only(state_path) {
        if let Ok(mut statement) = connection.prepare(
            "SELECT id, COALESCE(originator, ''), COALESCE(NULLIF(name, ''), title, '') FROM threads",
        ) {
            if let Ok(rows) = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            }) {
                for row in rows.flatten() {
                    let (id, originator, title) = row;
                    if originator == "Codex Desktop" {
                        out.desktop.insert(id.clone());
                    }
                    if !title.trim().is_empty() {
                        out.titles.insert(id, title);
                    }
                }
            }
        }
    }
    // The history table is a projection, not a heartbeat: after a Codex
    // crash/close it keeps its last `inProgress` row forever. Only trust it
    // while the database itself is warm, and only for a turn young enough to
    // still be running.
    if is_fresh(history_path, DB_FRESH_FOR) {
        if let Ok(connection) = open_read_only(history_path) {
            let cutoff = now_secs().saturating_sub(MAX_TURN_AGE.as_secs() as i64);
            if let Ok(mut statement) = connection.prepare(
                "SELECT DISTINCT thread_id FROM thread_turns \
                 WHERE status IN ('inProgress', 'in_progress') \
                   AND COALESCE(started_at, 0) >= ?1",
            ) {
                if let Ok(rows) = statement.query_map([cutoff], |row| row.get::<_, String>(0)) {
                    out.running.extend(rows.flatten());
                }
            }
        }
    }
    out
}

fn is_fresh(path: &Path, within: Duration) -> bool {
    // SQLite in WAL mode writes the page changes to `<db>-wal` first; the main
    // file can stay untouched until a checkpoint. Treat either as the heartbeat.
    let wal = path.with_file_name(format!(
        "{}-wal",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    ));
    fn fresh(candidate: &Path, within: Duration) -> bool {
        std::fs::metadata(candidate)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age <= within)
    }
    fresh(path, within) || fresh(&wal, within)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

fn codex_home() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("CODEX_HOME") {
        let value = value.trim();
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    Some(agi_platform::home().join(".codex"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_db(path: &Path, rows: &[(&str, &str, &str)]) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, originator TEXT, name TEXT, title TEXT)",
                [],
            )
            .unwrap();
        for (id, originator, title) in rows {
            connection
                .execute(
                    "INSERT INTO threads (id, originator, name, title) VALUES (?1, ?2, ?3, ?3)",
                    rusqlite::params![id, originator, title],
                )
                .unwrap();
        }
    }

    fn history_db(path: &Path, rows: &[(&str, &str)]) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "CREATE TABLE thread_turns (thread_id TEXT, turn_id TEXT, status TEXT, started_at INTEGER)",
                [],
            )
            .unwrap();
        for (thread, status) in rows {
            let started = now_secs() - if *status == "inProgress" { 10 } else { 3600 };
            connection
                .execute(
                    "INSERT INTO thread_turns (thread_id, turn_id, status, started_at) VALUES (?1, 't', ?2, ?3)",
                    rusqlite::params![thread, status, started],
                )
                .unwrap();
        }
    }

    #[test]
    fn finds_desktop_threads_and_open_turns() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        let history = dir.path().join("history.sqlite");
        state_db(
            &state,
            &[
                ("desktop-1", "Codex Desktop", "Gui title"),
                ("cli-1", "agi-mizar", "CLI title"),
                ("guardian-1", "guardian", ""),
            ],
        );
        history_db(
            &history,
            &[("desktop-1", "inProgress"), ("cli-1", "completed")],
        );

        let gui = read_from(&state, &history);
        assert!(gui.desktop.contains("desktop-1"));
        assert!(!gui.desktop.contains("cli-1"));
        assert!(gui.running.contains("desktop-1"));
        assert!(!gui.running.contains("cli-1"));
        assert_eq!(
            gui.titles.get("desktop-1").map(String::as_str),
            Some("Gui title")
        );
    }

    #[test]
    fn an_old_open_turn_does_not_light_the_lamp() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        let history = dir.path().join("history.sqlite");
        state_db(&state, &[("stale", "Codex Desktop", "Old turn")]);
        let connection = Connection::open(&history).unwrap();
        connection
            .execute(
                "CREATE TABLE thread_turns (thread_id TEXT, turn_id TEXT, status TEXT, started_at INTEGER)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO thread_turns VALUES ('stale', 't', 'inProgress', ?1)",
                rusqlite::params![now_secs() - 7 * 60 * 60],
            )
            .unwrap();
        drop(connection);
        let gui = read_from(&state, &history);
        assert!(!gui.running.contains("stale"));
    }

    #[test]
    fn a_missing_database_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let gui = read_from(
            &dir.path().join("none.sqlite"),
            &dir.path().join("none2.sqlite"),
        );
        assert!(gui.desktop.is_empty());
        assert!(gui.running.is_empty());
        assert!(gui.titles.is_empty());
    }
}
