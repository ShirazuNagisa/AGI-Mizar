//! Putting the hooks into Claude Code's settings.
//!
//! Claude Code reads `~/.claude/settings.json` at startup. There is no plugin
//! registry and no install command we can lean on, so this module edits that file
//! — carefully:
//!
//! - **A backup first**, named for the moment it was taken.
//! - **Idempotent**: a second install adds nothing, because the entries are
//!   recognised by the command they run, not by position.
//! - **Additive**: every other key and every other hook is preserved. The
//!   operator's own hooks are not ours to tidy.
//! - **Reversible**: uninstall removes exactly the entries this module wrote and
//!   leaves empty arrays out rather than writing `"PreToolUse": []`.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// The marker that identifies our entries.
const MARKER: &str = "agi-mizar-hook";

/// The eight hooks, and the matcher each needs.
///
/// `PreToolUse` takes `*` because every tool may ask for permission; the rest
/// take none, and writing one where Claude Code does not expect it is a schema
/// error rather than a harmless extra.
pub const HOOK_EVENTS: [(&str, Option<&str>); 8] = [
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("PreToolUse", Some("*")),
    ("Stop", None),
    ("SubagentStop", None),
    ("SessionEnd", None),
    ("Notification", None),
    ("PreCompact", None),
];

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    /// Entries added.
    pub added: usize,
    /// Entries that were already there.
    pub present: usize,
    /// The backup written, when the file changed.
    pub backup: Option<PathBuf>,
}

/// Add the hooks to a settings file.
///
/// Creates the file when it does not exist, which is the state of a machine that
/// has run Claude Code but never customised it.
pub fn install(settings: &Path, shim: &Path) -> io::Result<InstallOutcome> {
    let mut root = read_settings(settings)?;
    let command = |event: &str| format!("{} claude {event}", quote(shim));

    let Some(object) = root.as_object_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "settings.json is not a JSON object",
        ));
    };
    let hooks = object
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "settings.json has a \"hooks\" key that is not an object",
        ));
    };

    let mut added = 0usize;
    let mut present = 0usize;
    for (event, matcher) in HOOK_EVENTS {
        let entries = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(entries) = entries.as_array_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("settings.json has a \"{event}\" hook that is not an array"),
            ));
        };

        let already = entries.iter().any(contains_marker);
        if already {
            present += 1;
            continue;
        }

        let mut entry = Map::new();
        if let Some(matcher) = matcher {
            entry.insert("matcher".to_string(), json!(matcher));
        }
        entry.insert(
            "hooks".to_string(),
            json!([{ "type": "command", "command": command(event), "timeout": 60 }]),
        );
        entries.push(Value::Object(entry));
        added += 1;
    }

    if added == 0 {
        return Ok(InstallOutcome {
            added: 0,
            present,
            backup: None,
        });
    }

    let backup = backup_of(settings)?;
    write_settings(settings, &root)?;
    Ok(InstallOutcome {
        added,
        present,
        backup: Some(backup),
    })
}

/// Remove every entry this module wrote.
///
/// Returns how many entries were removed. A settings file that does not exist is
/// a no-op: there is nothing of ours in it.
pub fn uninstall(settings: &Path) -> io::Result<usize> {
    if !settings.exists() {
        return Ok(0);
    }
    let mut root = read_settings(settings)?;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(0);
    };

    let mut removed = 0usize;
    for (event, _) in HOOK_EVENTS {
        let Some(entries) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
            continue;
        };
        let before = entries.len();
        entries.retain(|entry| !contains_marker(entry));
        removed += before - entries.len();
        // An empty array is not the same as an absent key: Claude Code would
        // report a hook with nothing in it.
        if entries.is_empty() {
            hooks.remove(event);
        }
    }

    if removed > 0 {
        write_settings(settings, &root)?;
    }
    Ok(removed)
}

/// Whether a hook entry is ours.
fn contains_marker(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(MARKER))
            })
        })
        .unwrap_or(false)
}

/// Read a settings file, or an empty object when there is none.
fn read_settings(settings: &Path) -> io::Result<Value> {
    match std::fs::read_to_string(settings) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not valid JSON: {error}", settings.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error),
    }
}

/// Write a settings file atomically, pretty-printed the way a person reads it.
fn write_settings(settings: &Path, root: &Value) -> io::Result<()> {
    if let Some(parent) = settings.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(root)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let temp = settings.with_extension("json.tmp");
    std::fs::write(&temp, format!("{text}\n"))?;
    std::fs::rename(&temp, settings)
}

/// Copy the settings file aside before changing it.
fn backup_of(settings: &Path) -> io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let backup = settings.with_extension(format!("json.bak-agi-{stamp}"));
    if settings.exists() {
        std::fs::copy(settings, &backup)?;
    }
    Ok(backup)
}

/// Quote a path for a shell command line, only when it needs it.
fn quote(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    if text.contains(' ') || text.contains('"') {
        format!("\"{}\"", text.replace('"', "\\\""))
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shim() -> PathBuf {
        PathBuf::from("/Applications/AGI Mizar.app/agi-mizar-hook")
    }

    #[test]
    fn installs_eight_hooks_into_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");

        let outcome = install(&settings, &shim()).unwrap();
        assert_eq!(outcome.added, 8);
        assert_eq!(outcome.present, 0);

        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        for (event, _) in HOOK_EVENTS {
            assert!(root["hooks"][event].is_array(), "{event} missing");
        }
        // A path with spaces must be quoted, or the shell splits it.
        assert!(root["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .starts_with('"'));
    }

    #[test]
    fn a_second_install_adds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");

        install(&settings, &shim()).unwrap();
        let second = install(&settings, &shim()).unwrap();
        assert_eq!(second.added, 0);
        assert_eq!(second.present, 8);
        assert!(second.backup.is_none(), "no change means no backup");
    }

    #[test]
    fn preserves_everything_that_was_there() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{
              "model": "opus",
              "hooks": {
                "PreToolUse": [
                  { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo mine" }] }
                ]
              }
            }"#,
        )
        .unwrap();

        install(&settings, &shim()).unwrap();
        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(root["model"], "opus");
        // The operator's own hook is still first.
        assert_eq!(
            root["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "echo mine"
        );
        assert_eq!(root["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo mine"}]}]}}"#,
        )
        .unwrap();

        install(&settings, &shim()).unwrap();
        let removed = uninstall(&settings).unwrap();
        assert_eq!(removed, 8);

        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(root["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(
            root["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "echo mine"
        );
        // Emptied arrays are dropped, not left behind.
        assert!(root["hooks"].get("Stop").is_none());
    }

    #[test]
    fn a_missing_settings_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(uninstall(&dir.path().join("nope.json")).unwrap(), 0);
    }

    #[test]
    fn refuses_to_clobber_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, "{ not json").unwrap();
        assert!(install(&settings, &shim()).is_err());
        // The original bytes are untouched.
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{ not json");
    }
}
