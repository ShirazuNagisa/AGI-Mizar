//! The per-agent half of `agi-remote install`.
//!
//! Agent interconnect is one MCP server plus one skill, and every agent this
//! application hosts reads them from its own place:
//!
//! | agent | MCP entry | skill |
//! |---|---|---|
//! | DSH | `profiles/<p>/cordis.patch.yml` (`- insert:` block) | `<dsh home>/skills/<name>/SKILL.md` |
//! | Claude Code | `<claude home>/../.claude.json` → `mcpServers` | `<claude home>/skills/<name>/SKILL.md` |
//! | Codex | `<codex home>/config.toml` → `[mcp_servers.<name>]` | `<codex home>/skills/<name>/SKILL.md` |
//!
//! The DSH block lives in `main.rs` because it shares the profile resolution with
//! the rest of the sidecar; the two files here are edited by hand-shaped rules
//! instead of a parser, because they belong to other products: a TOML or JSON
//! round trip would reformat an operator's whole config file to add one table.
//! Every edit is additive, idempotent and reversible, and a rewrite is backed up
//! first — the same contract the Claude hook installer keeps.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

/// The MCP server name Claude Code and Codex see.
///
/// DSH spells the same server `agi_mizar_remote` in its client config, so the
/// tool prefix (`mcp__agi_mizar_remote__*`) is the same in all three products
/// and one skill can name the tools once.
pub const MCP_SERVER_NAME: &str = "agi_mizar_remote";

/// Claude Code's configuration directory and user config file.
///
/// Resolved by [`agi_platform`] so the installer and the dependency check cannot
/// disagree about which file they are talking about.
pub fn claude_home() -> PathBuf {
    agi_platform::claude_home()
}

pub fn claude_config() -> PathBuf {
    agi_platform::claude_config()
}

/// Codex's configuration directory.
pub fn codex_home() -> PathBuf {
    agi_platform::codex_home()
}

/// Whether an agent looks installed on this machine.
///
/// A directory is the signal: every one of these products creates its home on
/// first run, and writing an MCP entry into an agent that is not there would
/// leave files nobody reads behind.
pub fn present(agent: &str) -> bool {
    match agent {
        "claude" => claude_home().is_dir(),
        "codex" => codex_home().is_dir(),
        "dsh" => agi_platform::dsh_home().join("profiles").is_dir(),
        _ => false,
    }
}

/// Write one skill directory, replacing only when the text differs.
pub fn write_skill(dir: &Path, skill: &str, dry: bool) -> Result<bool> {
    let path = dir.join("SKILL.md");
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() == Some(skill) {
        println!("skill: {} (unchanged)", path.display());
        return Ok(false);
    }
    println!("skill: {}", path.display());
    if dry {
        return Ok(true);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, skill).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Remove one skill directory, when it is ours.
pub fn remove_skill(dir: &Path, dry: bool) -> Result<bool> {
    if !dir.join("SKILL.md").is_file() {
        return Ok(false);
    }
    println!("skill: {} (removing)", dir.display());
    if dry {
        return Ok(true);
    }
    std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
    Ok(true)
}

// ------------------------------------------------------------------ Claude

/// Register the MCP server in `~/.claude.json`, preserving every other key.
pub fn claude_install(exe: &Path, dry: bool) -> Result<bool> {
    let path = claude_config();
    let mut root = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        // A machine that has Claude Code but never opened its user config: a
        // new object with the one key is what the product itself writes.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let Some(object) = root.as_object_mut() else {
        bail!("{} is not a JSON object", path.display());
    };
    let servers = object
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        bail!(
            "{} has an \"mcpServers\" key that is not an object",
            path.display()
        );
    };
    let desired = json!({
        "type": "stdio",
        "command": exe.to_string_lossy(),
        "args": ["mcp"],
    });
    if servers.get(MCP_SERVER_NAME) == Some(&desired) {
        println!("claude: {} (unchanged)", path.display());
        return Ok(false);
    }
    servers.insert(MCP_SERVER_NAME.to_string(), desired);
    println!("claude: {}", path.display());
    if dry {
        return Ok(true);
    }
    write_json(&path, &root)
}

/// Remove our entry from `~/.claude.json`, leaving the rest of the file alone.
pub fn claude_remove(dry: bool) -> Result<bool> {
    let path = claude_config();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let mut root: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let Some(servers) = root
        .as_object_mut()
        .and_then(|object| object.get_mut("mcpServers"))
        .and_then(Value::as_object_mut)
    else {
        return Ok(false);
    };
    if servers.remove(MCP_SERVER_NAME).is_none() {
        return Ok(false);
    }
    println!("claude: {} (removing entry)", path.display());
    if dry {
        return Ok(true);
    }
    write_json(&path, &root)
}

fn write_json(path: &Path, value: &Value) -> Result<bool> {
    let bytes = serde_json::to_vec_pretty(value).context("serialising the config")?;
    backup(path)?;
    atomic_write(path, &bytes)?;
    Ok(true)
}

// ------------------------------------------------------------------- Codex

/// Append `[mcp_servers.agi_mizar_remote]` to Codex's `config.toml`.
pub fn codex_install(exe: &Path, dry: bool) -> Result<bool> {
    let path = codex_home().join("config.toml");
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    let block = format!(
        "[mcp_servers.{MCP_SERVER_NAME}]\ncommand = {}\nargs = [\"mcp\"]\n",
        toml_literal(&exe.to_string_lossy())
    );
    // A repair replaces the table instead of adding a second one: two tables
    // with one name is a parse error, not a merge.
    let mut after = remove_codex_table(&before);
    if !after.is_empty() && !after.ends_with('\n') {
        after.push('\n');
    }
    if !after.trim().is_empty() {
        after.push('\n');
    }
    after.push_str(&block);
    if after == before {
        println!("codex: {} (unchanged)", path.display());
        return Ok(false);
    }
    println!("codex: {}", path.display());
    if dry {
        for line in block.lines() {
            println!("+{line}");
        }
        return Ok(true);
    }
    backup(&path)?;
    atomic_write(&path, after.as_bytes())?;
    Ok(true)
}

/// Drop our table from `config.toml`.
pub fn codex_remove(dry: bool) -> Result<bool> {
    let path = codex_home().join("config.toml");
    let Ok(before) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let after = remove_codex_table(&before);
    if after == before {
        return Ok(false);
    }
    println!("codex: {} (removing table)", path.display());
    if dry {
        return Ok(true);
    }
    backup(&path)?;
    atomic_write(&path, after.as_bytes())?;
    Ok(true)
}

/// A TOML literal string: single quotes, so a Windows path keeps its
/// backslashes instead of being read as escapes.
fn toml_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Remove `[mcp_servers.<name>]` and its sub-tables, keeping everything else
/// byte for byte.
fn remove_codex_table(text: &str) -> String {
    let header = format!("[mcp_servers.{MCP_SERVER_NAME}]");
    let prefix = format!("[mcp_servers.{MCP_SERVER_NAME}.");
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut skipping = false;
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            skipping = trimmed == header || trimmed.starts_with(&prefix);
            if skipping {
                // The blank line the installer wrote before the table is ours.
                if out.last().is_some_and(|last| last.trim().is_empty()) {
                    out.pop();
                }
                continue;
            }
        }
        if skipping {
            continue;
        }
        out.push(line);
    }
    let mut text = out.join("\n");
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

// ------------------------------------------------------------------ shared

/// Keep one copy of the file we are about to rewrite.
fn backup(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let backup = path.with_extension(format!(
        "{}.bak",
        path.extension()
            .map(|extension| extension.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    std::fs::copy(path, &backup).with_context(|| format!("backing up {}", path.display()))?;
    println!("backup: {}", backup.display());
    Ok(())
}

/// Write beside the file, then rename: a crash cannot leave half a config.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let temp = path.with_extension("tmp-agi-mizar");
    std::fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| {
        let _ = std::fs::remove_file(&temp);
        format!("replacing {}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::{remove_codex_table, toml_literal};

    #[test]
    fn a_windows_path_is_a_literal_toml_string() {
        let quoted = toml_literal(r"C:\Users\me\AGI Mizar\agi-remote.exe");
        assert_eq!(quoted, r"'C:\Users\me\AGI Mizar\agi-remote.exe'");
    }

    #[test]
    fn the_codex_table_is_removed_without_touching_its_neighbours() {
        let text = "[model]\nname = 'x'\n\n[mcp_servers.agi_mizar_remote]\ncommand = 'a'\nargs = [\"mcp\"]\n\n[mcp_servers.other]\ncommand = 'b'\n";
        let after = remove_codex_table(text);
        assert!(!after.contains("agi_mizar_remote"), "{after}");
        assert!(after.contains("[model]"), "{after}");
        assert!(after.contains("[mcp_servers.other]"), "{after}");
    }

    #[test]
    fn a_missing_table_leaves_the_file_alone() {
        let text = "[model]\nname = 'x'\n";
        assert_eq!(remove_codex_table(text), text);
    }

    #[test]
    fn a_sub_table_of_ours_goes_too() {
        let text = "[mcp_servers.agi_mizar_remote]\ncommand = 'a'\n\n[mcp_servers.agi_mizar_remote.env]\nA = '1'\n\n[other]\nx = 1\n";
        let after = remove_codex_table(text);
        assert!(!after.contains("agi_mizar_remote"), "{after}");
        assert!(after.contains("[other]"), "{after}");
    }
}
