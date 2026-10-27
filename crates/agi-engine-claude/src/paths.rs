//! Where Claude Code keeps its things.
//!
//! One rule governs everything here: **a missing directory is an answer, not an
//! error.** A machine that has never run Claude Code has no `~/.claude/projects`,
//! and that is a perfectly good reason for the engine to show nothing — not a
//! failure to report.

use std::path::{Path, PathBuf};

/// One transcript file, as the session list needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFile {
    /// The session id, which is the file stem.
    pub session_id: String,
    pub path: PathBuf,
    /// The project directory name — Claude Code's slug for the working directory.
    pub project: String,
    /// Last modification, epoch milliseconds.
    pub mtime: i64,
    pub size: u64,
}

/// Claude Code's configuration directory.
///
/// `CLAUDE_CONFIG_DIR` is Claude Code's own override and wins, so a user who
/// moved their home keeps working.
pub fn claude_home() -> PathBuf {
    if let Ok(configured) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = configured.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    home().join(".claude")
}

/// The user's home directory.
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The directory holding one subdirectory per project.
pub fn projects_dir() -> PathBuf {
    claude_home().join("projects")
}

/// Claude Code's own settings file.
pub fn settings_path() -> PathBuf {
    claude_home().join("settings.json")
}

/// Whether a file name looks like a session transcript.
///
/// Subagent transcripts live in a `subagents/` subdirectory and are excluded by
/// the directory walk, but the name check also skips `agent-*` files: those are
/// bookkeeping, and putting them next to the operator's conversations would make
/// the list lie about what happened.
fn is_transcript_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    if stem.starts_with("agent-") {
        return false;
    }
    // Claude Code names a transcript with a UUID.
    stem.len() >= 8 && stem.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Every session transcript under `root`, newest first.
///
/// Takes the root rather than reading the environment so tests can point it at a
/// temporary tree.
pub fn list_transcripts(root: &Path) -> Vec<TranscriptFile> {
    let Ok(projects) = std::fs::read_dir(root) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for project in projects.flatten() {
        let project_dir = project.path();
        if !project_dir.is_dir() {
            continue;
        }
        let project_name = project.file_name().to_string_lossy().to_string();
        let Ok(entries) = std::fs::read_dir(&project_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !is_transcript_name(&name) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                // A file that vanished between readdir and stat is not a row.
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            out.push(TranscriptFile {
                session_id: name.trim_end_matches(".jsonl").to_string(),
                path,
                project: project_name.clone(),
                mtime: mtime_ms(&metadata),
                size: metadata.len(),
            });
        }
    }

    out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    out
}

/// Modification time in epoch milliseconds.
fn mtime_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_transcript_names() {
        assert!(is_transcript_name("0f9c2b1a-1111-2222-3333-444455556666.jsonl"));
        assert!(!is_transcript_name("agent-abc.jsonl"));
        assert!(!is_transcript_name("notes.jsonl"));
        assert!(!is_transcript_name("0f9c2b1a.jsonl.txt"));
    }

    #[test]
    fn a_missing_root_is_an_empty_list() {
        assert!(list_transcripts(Path::new("/nonexistent/agi-mizar")).is_empty());
    }
}
