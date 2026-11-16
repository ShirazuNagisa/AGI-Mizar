//! What the remote-agent feature needs on this computer.
//!
//! The feature has two sides with different owners. Being used by a peer needs
//! an account, a listener policy, and a relay for the paid route; delegating to
//! a peer needs the `agi-remote` sidecar, the DSH MCP entry and skill, and the
//! scoped tool token this kernel publishes. A check that only looked at files
//! would call a machine complete while the sidecar is missing; a check that only
//! ran the sidecar could say nothing on the machine where it is missing. So both
//! are read here, and the one repair the app can perform is the installer it
//! already ships.
//!
//! The log export is next to the check because it is the same question asked
//! afterwards: what did this feature do, and what was in place when it did it.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use agi_protocol::api::{DependencyItem, RemoteHealthReport};
use serde_json::json;

use crate::Core;

/// The skill the installer writes. Reading it back is how "outdated" is known:
/// existence alone would keep an old copy after an app update.
const SKILL: &str = include_str!("../../agi-remote/skills/agi-mizar-remote/SKILL.md");
const SKILL_NAME: &str = "agi-mizar-remote";
const MCP_ENTRY_ID: &str = "mcp-agi-mizar-remote";
/// The MCP server name Claude Code and Codex register.
const MCP_SERVER_NAME: &str = "agi_mizar_remote";

/// How much of one audit file the export keeps. The file rotates at five MiB;
/// pointing a support thread at two megabytes of JSON lines is already a lot.
const LOG_TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// Check the feature, and optionally rewrite the one part this app owns.
pub fn check(core: &Core, repair: bool) -> RemoteHealthReport {
    let mut items = collect(core);
    let mut changed = Vec::new();
    let mut error = None;

    let repairable = items.iter().any(|item| {
        matches!(
            item.id.as_str(),
            "mcp" | "skill" | "claude-mcp" | "claude-skill" | "codex-mcp" | "codex-skill"
        ) && matches!(item.status.as_str(), "missing" | "outdated")
    });
    // The profile item is a hard gate: without one the installer has nowhere to
    // write, and running it would only turn the report into an error message.
    let profile_ok = items
        .iter()
        .any(|item| item.id == "profile" && item.status == "ok");
    if repair && repairable && profile_ok {
        if sidecar().is_none() {
            error = Some("agi-remote 不在应用旁边，无法重写 DSH 配置".to_string());
        } else {
            let answer = crate::run_remote_installer(false, None, false);
            if answer.ok {
                changed.push("已重写 DSH MCP 条目与 agi-mizar-remote skill".to_string());
            } else {
                error = answer.error.map(|failure| failure.message);
            }
        }
        items = collect(core);
    }

    report(core, items, changed, error)
}

fn report(
    core: &Core,
    items: Vec<DependencyItem>,
    changed: Vec<String>,
    error: Option<String>,
) -> RemoteHealthReport {
    let problems = items
        .iter()
        .filter(|item| matches!(item.status.as_str(), "missing" | "outdated"))
        .count();
    let ok = problems == 0;
    let summary = if !ok {
        format!("{problems} 项依赖需要处理")
    } else if !core.remote_prefs().enabled {
        "依赖完整；被远程使用未开启".to_string()
    } else {
        "Agent 互联依赖完整".to_string()
    };
    RemoteHealthReport {
        ok,
        summary,
        items,
        changed,
        error,
    }
}

fn collect(core: &Core) -> Vec<DependencyItem> {
    let sidecar = sidecar();
    let profile = profile_dir();
    let patch = profile
        .as_ref()
        .ok()
        .map(|dir| dir.join("cordis.patch.yml"));
    let patch_text = patch
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok());

    let mcp_entry = patch_text
        .as_deref()
        .and_then(|text| mcp_command(text, MCP_ENTRY_ID));
    let skill_path = agi_platform::dsh_home()
        .join("skills")
        .join(SKILL_NAME)
        .join("SKILL.md");
    let skill_text = std::fs::read_to_string(&skill_path).ok();
    let prefs = core.remote_prefs();
    let dsh_home = agi_platform::dsh_home();

    let mut items = vec![
        sidecar_item(sidecar.as_deref()),
        profile_item(profile.as_ref(), &dsh_home),
        mcp_item(
            patch.as_deref(),
            mcp_entry.as_deref(),
            sidecar.as_deref(),
            patch_text.is_some(),
            profile.is_ok(),
        ),
        skill_item(&skill_path, skill_text.as_deref(), sidecar.is_some()),
    ];
    items.extend(claude_items(sidecar.as_deref()));
    items.extend(codex_items(sidecar.as_deref()));
    items.extend([
        token_item(),
        account_item(core),
        policy_item(&prefs),
        relay_item(core, &prefs),
    ]);
    items
}

/// Claude Code's two files, as two rows.
///
/// An agent that is not installed is `unknown`, not `missing`: a machine that
/// never ran Claude Code has no dependency to fix, and marking it red would put
/// a permanent problem on the page.
fn claude_items(sidecar: Option<&Path>) -> Vec<DependencyItem> {
    let home = agi_platform::claude_home();
    if !home.is_dir() {
        return vec![
            item(
                "claude-mcp",
                "Claude Code 里的 MCP 条目",
                "unknown",
                "这台电脑没有 Claude Code".to_string(),
                None,
            ),
            item(
                "claude-skill",
                "Claude Code 里的 agi-mizar-remote skill",
                "unknown",
                "这台电脑没有 Claude Code".to_string(),
                None,
            ),
        ];
    }
    let config = agi_platform::claude_config();
    let entry = std::fs::read_to_string(&config)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|root| {
            root.get("mcpServers")
                .and_then(|servers| servers.get(MCP_SERVER_NAME))
                .and_then(|entry| entry.get("command"))
                .and_then(|command| command.as_str())
                .map(str::to_string)
        });
    vec![
        agent_mcp_item("claude-mcp", "Claude Code 里的 MCP 条目", config.as_path(), entry.as_deref(), sidecar),
        agent_skill_item(
            "claude-skill",
            "Claude Code 里的 agi-mizar-remote skill",
            &home.join("skills").join(SKILL_NAME).join("SKILL.md"),
            sidecar,
        ),
    ]
}

/// Codex's two files, as two rows.
fn codex_items(sidecar: Option<&Path>) -> Vec<DependencyItem> {
    let home = agi_platform::codex_home();
    if !home.is_dir() {
        return vec![
            item(
                "codex-mcp",
                "Codex 里的 MCP 条目",
                "unknown",
                "这台电脑没有 Codex".to_string(),
                None,
            ),
            item(
                "codex-skill",
                "Codex 里的 agi-mizar-remote skill",
                "unknown",
                "这台电脑没有 Codex".to_string(),
                None,
            ),
        ];
    }
    let config = home.join("config.toml");
    let entry = std::fs::read_to_string(&config)
        .ok()
        .and_then(|text| toml_command(&text, MCP_SERVER_NAME));
    vec![
        agent_mcp_item("codex-mcp", "Codex 里的 MCP 条目", config.as_path(), entry.as_deref(), sidecar),
        agent_skill_item(
            "codex-skill",
            "Codex 里的 agi-mizar-remote skill",
            &home.join("skills").join(SKILL_NAME).join("SKILL.md"),
            sidecar,
        ),
    ]
}

/// One "this agent has an MCP entry" row for the two non-DSH agents.
fn agent_mcp_item(
    id: &str,
    label: &str,
    config: &Path,
    command: Option<&str>,
    sidecar: Option<&Path>,
) -> DependencyItem {
    let Some(command) = command else {
        return item(
            id,
            label,
            "missing",
            format!("{} 里没有 agi_mizar_remote", config.display()),
            Some("写入 MCP 条目"),
        );
    };
    match sidecar {
        Some(expected) if !same_path(command, expected) => item(
            id,
            label,
            "outdated",
            format!("条目指向 {command}，不是这一个安装"),
            Some("把条目改为指向当前的 agi-remote"),
        ),
        None => item(
            id,
            label,
            "unknown",
            format!("条目指向 {command}；应用旁边没有 agi-remote，无法比对"),
            Some("重装应用，或用 AGI_REMOTE_BIN 指定 agi-remote"),
        ),
        _ => item(id, label, "ok", command.to_string(), None),
    }
}

/// One "this agent has our skill" row.
fn agent_skill_item(
    id: &str,
    label: &str,
    path: &Path,
    sidecar: Option<&Path>,
) -> DependencyItem {
    match std::fs::read_to_string(path) {
        Ok(text) if text == SKILL => item(id, label, "ok", path.display().to_string(), None),
        Ok(_) => item(
            id,
            label,
            "outdated",
            format!("{} 与这个版本附带的不同", path.display()),
            Some("写入这个版本的 skill"),
        ),
        Err(_) => item(
            id,
            label,
            "missing",
            format!("{} 不存在", path.display()),
            sidecar.map(|_| "写入 skill"),
        ),
    }
}

/// The `command` a Codex `[mcp_servers.<name>]` table gives, read by lines.
///
/// The same reason as [`mcp_command`]: a TOML parser would have to accept every
/// other table in the operator's file, and one line is all this needs.
fn toml_command(text: &str, name: &str) -> Option<String> {
    let header = format!("[mcp_servers.{name}]");
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == header;
            continue;
        }
        if !inside {
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("command") {
            let value = value.trim_start().strip_prefix('=')?.trim();
            return Some(unquote(value));
        }
    }
    None
}

fn item(id: &str, label: &str, status: &str, detail: String, fix: Option<&str>) -> DependencyItem {
    DependencyItem {
        id: id.to_string(),
        label: label.to_string(),
        status: status.to_string(),
        detail,
        fix: fix.map(str::to_string),
    }
}

fn sidecar_item(path: Option<&Path>) -> DependencyItem {
    match path {
        Some(path) => item(
            "sidecar",
            "agi-remote 工具",
            "ok",
            path.display().to_string(),
            None,
        ),
        None => item(
            "sidecar",
            "agi-remote 工具",
            "missing",
            "应用旁边没有 agi-remote；安装包会带上它，也可以设置 AGI_REMOTE_BIN".to_string(),
            None,
        ),
    }
}

fn profile_item(profile: Result<&PathBuf, &String>, home: &Path) -> DependencyItem {
    match profile {
        Ok(dir) => item(
            "profile",
            "DSH 配置档",
            "ok",
            dir.display().to_string(),
            None,
        ),
        Err(reason) => {
            let fix = format!(
                "安装并启动一次 DSH，在 {} 下创建 desktop 配置档",
                home.display()
            );
            item(
                "profile",
                "DSH 配置档",
                "missing",
                reason.clone(),
                Some(&fix),
            )
        }
    }
}

fn mcp_item(
    patch: Option<&Path>,
    command: Option<&str>,
    sidecar: Option<&Path>,
    readable: bool,
    profile_ok: bool,
) -> DependencyItem {
    if !profile_ok {
        return item(
            "mcp",
            "DSH 里的 MCP 条目",
            "unknown",
            "没有可读的 DSH 配置档，无法判断".to_string(),
            None,
        );
    }
    if !readable {
        return item(
            "mcp",
            "DSH 里的 MCP 条目",
            "missing",
            format!(
                "{} 读不到",
                patch
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            ),
            Some("重写 DSH MCP 条目"),
        );
    }
    let Some(command) = command else {
        return item(
            "mcp",
            "DSH 里的 MCP 条目",
            "missing",
            "cordis.patch.yml 里没有 agi-mizar-remote 客户端".to_string(),
            Some("写入 DSH MCP 条目"),
        );
    };
    match sidecar {
        Some(expected) if !same_path(command, expected) => item(
            "mcp",
            "DSH 里的 MCP 条目",
            "outdated",
            format!("条目指向 {command}，不是这一个安装"),
            Some("把条目改为指向当前的 agi-remote"),
        ),
        // Without a sidecar beside this app there is nothing to compare the entry
        // against. Calling it "ok" would hide the failure a Windows install is
        // most likely to have — `bin\agi-remote.exe` missing from the bundle —
        // behind a green row, so the answer is "cannot tell" and what to do.
        None => item(
            "mcp",
            "DSH 里的 MCP 条目",
            "unknown",
            format!("条目指向 {command}；应用旁边没有 agi-remote，无法比对"),
            Some("重装应用，或用 AGI_REMOTE_BIN 指定 agi-remote"),
        ),
        _ => item("mcp", "DSH 里的 MCP 条目", "ok", command.to_string(), None),
    }
}

fn skill_item(path: &Path, text: Option<&str>, sidecar_present: bool) -> DependencyItem {
    match text {
        Some(text) if text == SKILL => item(
            "skill",
            "agi-mizar-remote skill",
            "ok",
            path.display().to_string(),
            None,
        ),
        Some(_) => item(
            "skill",
            "agi-mizar-remote skill",
            "outdated",
            format!("{} 与这个版本附带的不同", path.display()),
            Some("写入这个版本的 skill"),
        ),
        None => item(
            "skill",
            "agi-mizar-remote skill",
            "missing",
            format!("{} 不存在", path.display()),
            sidecar_present.then_some("写入 skill 并登记 MCP 条目"),
        ),
    }
}

fn token_item() -> DependencyItem {
    let path = crate::endpoint::default_tool_path();
    if path.is_file() {
        item(
            "token",
            "本地工具令牌",
            "ok",
            path.display().to_string(),
            None,
        )
    } else {
        item(
            "token",
            "本地工具令牌",
            "missing",
            format!("{} 不存在；内核没有把它发布到本机", path.display()),
            Some("重启 AGI Mizar"),
        )
    }
}

fn account_item(core: &Core) -> DependencyItem {
    // A self-hosted relay runs no account service; listing "not signed in" as a
    // missing dependency would make a working install look broken.
    if !core.accounts_enabled() {
        return item(
            "account",
            "账户",
            "ok",
            "自建服务器：不需要账户，用配对码互认".to_string(),
            None,
        );
    }
    match core.account_user() {
        Some(user) => item(
            "account",
            "账户",
            "ok",
            format!("已登录 {}", user.username),
            None,
        ),
        None => item(
            "account",
            "账户",
            "missing",
            "未登录；同账户的其他电脑找不到这台电脑".to_string(),
            Some("在设置 → 账户里登录"),
        ),
    }
}

fn policy_item(prefs: &crate::RemotePrefs) -> DependencyItem {
    let routes = match (prefs.allow_lan, prefs.allow_wan) {
        (true, true) => "局域网 + 跨网",
        (true, false) => "局域网",
        (false, true) => "跨网",
        (false, false) => "两条线路都关着",
    };
    if !prefs.enabled {
        return item(
            "policy",
            "被远程使用",
            "unknown",
            "远程使用未开启；其他电脑不能使用这台电脑".to_string(),
            Some("打开下面的「允许其他电脑把任务交给本机」"),
        );
    }
    if !prefs.allow_lan && !prefs.allow_wan {
        return item(
            "policy",
            "被远程使用",
            "missing",
            "已开启，但两条线路都关着，任何 peer 都进不来".to_string(),
            Some("打开局域网或跨网至少一条"),
        );
    }
    item(
        "policy",
        "被远程使用",
        "ok",
        format!(
            "{routes}；{} 个可远程使用的工作区，{} 台被拒绝的设备",
            prefs.workspaces.len(),
            prefs.blocked.len()
        ),
        None,
    )
}

fn relay_item(core: &Core, prefs: &crate::RemotePrefs) -> DependencyItem {
    // On a self-hosted install "跨网" means "through the operator's own relay";
    // there is no account behind it and no paid tier to mention.
    let self_hosted = !core.accounts_enabled();
    if !prefs.allow_wan {
        return item(
            "relay",
            "中继连接",
            "unknown",
            if self_hosted {
                "未开启跨网，不经过自建服务器".to_string()
            } else {
                "未开启跨网，不经过中继".to_string()
            },
            None,
        );
    }
    if core.relay_connected() {
        item(
            "relay",
            "中继连接",
            "ok",
            if self_hosted {
                "已连接到自建服务器".to_string()
            } else {
                "已连接到账户中继".to_string()
            },
            None,
        )
    } else {
        item(
            "relay",
            "中继连接",
            "missing",
            if self_hosted {
                "跨网已开启，但自建服务器没有连上".to_string()
            } else {
                "跨网已开启，但中继没有连上".to_string()
            },
            Some("检查网络后重启，或等它自动重连"),
        )
    }
}

/// The sidecar this install would run, if it is there.
fn sidecar() -> Option<PathBuf> {
    std::env::var("AGI_REMOTE_BIN")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| crate::find_remote_installer(&exe))
        })
        .filter(|path| path.is_file())
}

/// The DSH profile the installer writes, by the same rules it uses.
///
/// `DSH_PROFILE` comes first because `agi-remote install` honours it: a check
/// that resolved a different directory would call the wrong profile ready and
/// gate the repair button against a profile the installer would never touch.
fn profile_dir() -> Result<PathBuf, String> {
    let home = agi_platform::dsh_home();
    let profiles = home.join("profiles");
    if let Some(name) = std::env::var("DSH_PROFILE")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
    {
        let dir = profiles.join(&name);
        return if dir.is_dir() {
            Ok(dir)
        } else {
            Err(format!("DSH_PROFILE 指向的配置档 {} 不存在", dir.display()))
        };
    }
    let has_patch = |dir: &Path| dir.join("cordis.patch.yml").is_file();
    let desktop = profiles.join("desktop");
    if has_patch(&desktop) {
        return Ok(desktop);
    }
    let mut names: Vec<String> = std::fs::read_dir(&profiles)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir() && has_patch(&entry.path()))
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    match names.as_slice() {
        [] => Err(format!(
            "{} 下没有带 cordis.patch.yml 的 DSH 配置档",
            profiles.display()
        )),
        [only] => Ok(profiles.join(only)),
        several => Err(format!(
            "有 {} 个 DSH 配置档（{}），无法确定要写哪一个",
            several.len(),
            several.join("、")
        )),
    }
}

/// The `command:` a `- insert:` block carrying `id` gives the MCP client.
///
/// The file is hand-edited by operators, so this reads the block by indentation
/// rather than parsing YAML: a full parser would buy nothing for one line and
/// would have to fail on whatever else the file holds.
fn mcp_command(text: &str, id: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    // The installer writes a list item (`- id: …`); a hand-edited file may spell
    // it without the dash, so both are accepted.
    let index = lines.iter().position(|line| {
        let trimmed = line.trim_start();
        let rest = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        rest.starts_with(&format!("id: {id}"))
    })?;
    for line in lines.iter().skip(index + 1) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("- insert:") {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("command:") {
            return Some(unquote(value.trim()));
        }
    }
    None
}

/// A YAML single-quoted scalar, or a bare one, as typed.
fn unquote(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value[1..value.len() - 1].replace("''", "'");
    }
    value.trim_matches('"').to_string()
}

/// Whether two spellings name the same executable.
///
/// The installer writes the path it was started from; the check finds the
/// sidecar through the bundle layout and canonicalises it. A symlink or a
/// `/private` prefix on macOS would otherwise read as "outdated" on a perfectly
/// good install, so both sides are canonicalised first when they resolve.
fn same_path(left: &str, right: &Path) -> bool {
    // `agi_platform::canonicalize` is `dunce` on Windows, which strips the
    // `\\?\` verbatim prefix `std::fs::canonicalize` adds; two spellings of the
    // same file must not read as different installs.
    let resolved =
        |path: &Path| agi_platform::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let left = resolved(Path::new(left))
        .to_string_lossy()
        .replace('\\', "/");
    let right = resolved(right).to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        left.eq_ignore_ascii_case(&right)
    } else {
        left == right
    }
}

// ------------------------------------------------------------------- export

/// The whole feature's paper trail as one text file.
///
/// Newest state first (what was in place when the export was taken), then the
/// audit files, then the delegation records without their content: the audit
/// deliberately never held a prompt or an answer, and the export must not start
/// carrying them.
pub fn log_text(core: &Core) -> (PathBuf, String) {
    let path = core
        .remote
        .audit_path()
        .unwrap_or_else(crate::remote::default_audit_path);
    let prefs = core.remote_prefs();
    let account = core
        .account_user()
        .map(|user| format!("{}（已登录）", user.username))
        .unwrap_or_else(|| "未登录".to_string());
    let health = check(core, false);

    let mut text = String::new();
    text.push_str("# AGI Mizar Agent 互联日志\n");
    text.push_str(&format!(
        "# 导出时间：{}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z")
    ));
    text.push_str(&format!("# 版本：{}\n", core.version()));
    text.push_str(&format!(
        "# 平台：{} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    text.push_str(&format!(
        "# 数据目录：{}\n",
        agi_platform::data_dir().display()
    ));
    text.push_str(&format!(
        "# 远程策略：启用={} 局域网={} 跨网={} 工作区={} 拒绝设备={}\n",
        prefs.enabled,
        prefs.allow_lan,
        prefs.allow_wan,
        prefs.workspaces.len(),
        prefs.blocked.len()
    ));
    text.push_str(&format!("# 账户：{account}\n"));
    text.push_str(&format!("# 依赖：{}\n", health.summary));
    for item in &health.items {
        text.push_str(&format!(
            "#   {}（{}）：{}\n",
            item.label, item.status, item.detail
        ));
    }

    for (label, file) in [
        (
            "delegations-audit.jsonl.1（较早）",
            path.with_extension("jsonl.1"),
        ),
        ("delegations-audit.jsonl", path.clone()),
    ] {
        text.push_str(&format!("\n=== {file_label} ===\n", file_label = label));
        match tail(&file, LOG_TAIL_BYTES) {
            Some((body, truncated)) => {
                if truncated {
                    text.push_str("# 文件较大，只保留最后 2 MiB\n");
                }
                text.push_str(&body);
                if !body.ends_with('\n') {
                    text.push('\n');
                }
            }
            None => text.push_str("（没有这个文件）\n"),
        }
    }

    let records: Vec<serde_json::Value> = core
        .remote
        .list()
        .iter()
        .map(|delegation| {
            json!({
                "id": delegation.id,
                "peer": delegation.peer,
                "peerName": delegation.peer_name,
                "route": delegation.route,
                "state": delegation.state,
                "cwd": delegation.cwd,
                "startedAt": delegation.started_at,
                "endedAt": delegation.ended_at,
                "error": delegation.error,
                "reason": delegation.reason,
            })
        })
        .collect();
    text.push_str("\n=== 委派记录（不含 prompt 与回复正文）===\n");
    text.push_str(&serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".to_string()));
    text.push('\n');

    (path, text)
}

/// The end of a log file, on a line boundary when it had to be cut.
fn tail(path: &Path, limit: u64) -> Option<(String, bool)> {
    let size = std::fs::metadata(path).ok()?.len();
    let mut file = std::fs::File::open(path).ok()?;
    if size > limit {
        file.seek(SeekFrom::Start(size - limit)).ok()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        // The first line is probably half a JSON object; a person reading this
        // does not need it.
        let cut = text.find('\n').map(|at| &text[at + 1..]).unwrap_or(&text);
        return Some((cut.to_string(), true));
    }
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some((text, false))
}

#[cfg(test)]
mod tests {
    use super::{mcp_command, toml_command, unquote};

    #[test]
    fn reads_the_command_from_the_codex_table() {
        let text = "[model]\nname = 'x'\n\n[mcp_servers.agi_mizar_remote]\ncommand = 'C:////Users////me////agi-remote.exe'\nargs = [\"mcp\"]\n\n[mcp_servers.other]\ncommand = 'b'\n";
        assert_eq!(
            toml_command(text, "agi_mizar_remote").as_deref(),
            Some("C:////Users////me////agi-remote.exe")
        );
        assert_eq!(toml_command(text, "missing"), None);
    }

    #[test]
    fn reads_the_command_from_our_block() {
        let text = "- insert:\n\
                    \x20   - id: mcp-agi-mizar-remote\n\
                    \x20     config:\n\
                    \x20       command: '/opt/AGI Mizar/agi-remote'\n\
                    \x20       args: ['mcp']\n\
                    - insert:\n\
                    \x20   - id: something-else\n\
                    \x20     config:\n\
                    \x20       command: '/other'\n";
        assert_eq!(
            mcp_command(text, "mcp-agi-mizar-remote").as_deref(),
            Some("/opt/AGI Mizar/agi-remote")
        );
        assert_eq!(unquote("'C:\\Users\\me\\a.exe'"), "C:\\Users\\me\\a.exe");
        assert_eq!(unquote("'it''s'"), "it's");
    }

    #[test]
    fn a_missing_entry_is_not_a_command() {
        assert_eq!(mcp_command("", "mcp-agi-mizar-remote"), None);
        assert_eq!(
            mcp_command(
                "- insert:\n    - id: other\n      command: 'x'\n",
                "mcp-agi-mizar-remote"
            ),
            None
        );
    }
}
