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

/// How much of one audit file the export keeps. The file rotates at five MiB;
/// pointing a support thread at two megabytes of JSON lines is already a lot.
const LOG_TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// Check the feature, and optionally rewrite the one part this app owns.
pub fn check(core: &Core, repair: bool) -> RemoteHealthReport {
    let mut items = collect(core);
    let mut changed = Vec::new();
    let mut error = None;

    let repairable = items.iter().any(|item| {
        matches!(item.id.as_str(), "mcp" | "skill")
            && matches!(item.status.as_str(), "missing" | "outdated")
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
        "依赖完整；远程使用未开启".to_string()
    } else {
        "远程 Agent 依赖完整".to_string()
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

    vec![
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
        token_item(),
        account_item(core),
        policy_item(&prefs),
        relay_item(core, &prefs),
    ]
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
            "远程 Agent 工具",
            "ok",
            path.display().to_string(),
            None,
        ),
        None => item(
            "sidecar",
            "远程 Agent 工具",
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
            Some("打开下面的「允许远程 Agent」"),
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
    if !prefs.allow_wan {
        return item(
            "relay",
            "中继连接",
            "unknown",
            "未开启跨网，不经过中继".to_string(),
            None,
        );
    }
    if core.relay_connected() {
        item(
            "relay",
            "中继连接",
            "ok",
            "已连接到账户中继".to_string(),
            None,
        )
    } else {
        item(
            "relay",
            "中继连接",
            "missing",
            "跨网已开启，但中继没有连上".to_string(),
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

/// The DSH profile the installer writes, by the same rule it uses.
fn profile_dir() -> Result<PathBuf, String> {
    let home = agi_platform::dsh_home();
    let profiles = home.join("profiles");
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
    let resolved = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
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
    text.push_str("# AGI Mizar 远程 Agent 日志\n");
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
    use super::{mcp_command, unquote};

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
