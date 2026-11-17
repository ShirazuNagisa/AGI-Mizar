//! The built-in tools the toolbox lists.
//!
//! A tool is a capability of this application, not of an engine: Agent 互联
//! accepts work from other computers, computer use drives this machine's own
//! apps. Both are switches the operator flips in one page, and both have their
//! own settings surface behind a row.
//!
//! The switch for Agent 互联 is `remote.enabled`, the switch the settings page
//! already writes; keeping one source of truth means the toolbox and the
//! settings page can never disagree. Computer use gets its own preference
//! because nothing else in the app owns it yet.

use std::path::PathBuf;
use std::process::Command;

use agi_protocol::api::{DependencyItem, ToolDependencyReport, ToolView};
use serde::{Deserialize, Serialize};

use crate::Core;

/// Stable ids; the window routes on these and the wire carries them verbatim.
pub const INTERCONNECT: &str = "interconnect";
pub const COMPUTER_USE: &str = "computer-use";

/// Preferences this app owns for its built-in tools.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPrefs {
    /// Whether the computer-use tool is turned on.
    ///
    /// Off on a fresh install: the tool needs Accessibility and Screen
    /// Recording grants, and asking for those before the operator has chosen
    /// the tool would be a permission prompt with no context.
    #[serde(default)]
    pub computer_use: bool,
}

/// Whether this build can run computer use at all.
///
/// The kernel (see `computer-use-kernel/`) talks to Accessibility and
/// ScreenCaptureKit, so it is macOS-only today. Windows and Linux answers are
/// `false` with the reason, which is what the toolbox row shows instead of a
/// switch that would do nothing.
pub fn computer_use_available() -> bool {
    cfg!(target_os = "macos")
}

pub fn views(core: &Core) -> Vec<ToolView> {
    let remote = core.remote_prefs();
    let computer_use = core.preferences_tool_prefs().computer_use;
    let available = computer_use_available();
    vec![
        ToolView {
            id: INTERCONNECT.to_string(),
            name: "Agent 互联".to_string(),
            summary: "让这台电脑接受其他电脑的委派，或把任务派给它们。".to_string(),
            enabled: remote.enabled,
            available: true,
            detail: if remote.enabled {
                "正在接受远程委派".to_string()
            } else {
                "已关闭".to_string()
            },
        },
        ToolView {
            id: COMPUTER_USE.to_string(),
            name: "computer use".to_string(),
            summary: "让 agent 读屏幕、点按钮、打字，操作这台电脑上的应用。".to_string(),
            enabled: computer_use && available,
            available,
            detail: if available {
                if computer_use {
                    "已开启".to_string()
                } else {
                    "已关闭".to_string()
                }
            } else {
                "目前只在 macOS 上提供".to_string()
            },
        },
    ]
}

/// Flip one tool's switch. Returns the refreshed rows so the caller does not
/// have to re-read the whole snapshot.
pub fn set_enabled(core: &Core, tool: &str, enabled: bool) -> Result<Vec<ToolView>, String> {
    match tool {
        INTERCONNECT => {
            let mut prefs = core.remote_prefs();
            if prefs.enabled != enabled {
                prefs.enabled = enabled;
                core.set_remote_prefs(prefs);
            }
        }
        COMPUTER_USE => {
            if enabled && !computer_use_available() {
                return Err("computer use 目前只在 macOS 上提供".to_string());
            }
            {
                let mut preferences = core
                    .preferences
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if preferences.tools.computer_use == enabled {
                    return Ok(views(core));
                }
                preferences.tools.computer_use = enabled;
            }
            core.save_preferences();
        }
        other => return Err(format!("unknown tool: {other}")),
    }
    Ok(views(core))
}

/// Where the computer-use kernel may live, in the order a packaged app finds it.
fn kernel_candidates() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(value) = std::env::var("AGI_MIZAR_CUA_MCP") {
        if !value.trim().is_empty() {
            paths.push(PathBuf::from(value));
        }
    }
    // Beside the executable (a Tauri bundle puts sidecars in MacOS/).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("cua-mcp"));
            paths.push(dir.join("../Resources/cua-mcp"));
        }
    }
    // The operator's own install.
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        paths.push(home.join(".agi-mizar/tools/cua-mcp"));
        // Development trees: the workspace checkout.
        for dir in ["computer-use-kernel"] {
            paths.push(home.join("Documents/project/DSH-Mizar").join(dir).join(".build/release/cua-mcp"));
            paths.push(home.join("Documents/project/DSH-Mizar").join(dir).join(".build/debug/cua-mcp"));
        }
    }
    paths
}

fn resolve_kernel() -> Option<PathBuf> {
    kernel_candidates().into_iter().find(|path| path.is_file())
}

fn item(id: &str, label: &str, status: &str, detail: impl Into<String>, fix: Option<String>) -> DependencyItem {
    DependencyItem {
        id: id.to_string(),
        label: label.to_string(),
        status: status.to_string(),
        detail: detail.into(),
        fix,
    }
}

fn report(tool: &str, items: Vec<DependencyItem>, hint: Option<String>) -> ToolDependencyReport {
    let problems = items
        .iter()
        .filter(|item| matches!(item.status.as_str(), "missing" | "outdated"))
        .count();
    let ok = problems == 0;
    let summary = if ok {
        "依赖完整".to_string()
    } else {
        format!("{problems} 项需要处理")
    };
    ToolDependencyReport {
        tool: tool.to_string(),
        ok,
        summary,
        items,
        hint,
    }
}

/// Read one tool's dependencies. Nothing here changes the machine.
pub fn check(core: &Core, tool: &str) -> Result<ToolDependencyReport, String> {
    let _ = core;
    match tool {
        INTERCONNECT => {
            // The Agent 互联 page owns the detailed check (it knows about the
            // MCP entry, the skill and the account); the toolbox row only needs
            // the switch, so this report stays a pointer rather than a copy.
            let prefs = core.remote_prefs();
            let items = vec![item(
                "enabled",
                "接受远程委派",
                if prefs.enabled { "ok" } else { "missing" },
                if prefs.enabled {
                    "已开启"
                } else {
                    "开关是关的；打开后其他电脑才能把任务派过来"
                },
                Some("在这个页面上把开关打开".to_string()),
            )];
            Ok(report(tool, items, None))
        }
        COMPUTER_USE => {
            if !computer_use_available() {
                let items = vec![item(
                    "platform",
                    "平台",
                    "missing",
                    "computer use 依赖 macOS 的辅助功能与屏幕捕获，本平台没有实现",
                    None,
                )];
                return Ok(report(tool, items, None));
            }
            let mut items: Vec<DependencyItem> = Vec::new();
            items.push(item("platform", "平台", "ok", "macOS", None));

            let hint = "在工作区 computer-use-kernel/ 里执行 ./scripts/build.sh，或把 cua-mcp 放进 ~/.agi-mizar/tools/。".to_string();
            let Some(kernel) = resolve_kernel() else {
                items.push(item(
                    "kernel",
                    "内核（cua-mcp）",
                    "missing",
                    "没找到 cua-mcp",
                    Some("构建 computer-use-kernel 后把它放进 ~/.agi-mizar/tools/".to_string()),
                ));
                items.push(item("accessibility", "辅助功能权限", "unknown", "内核未就绪，无法检查", None));
                items.push(item("screen", "屏幕录制权限", "unknown", "内核未就绪，无法检查", None));
                return Ok(report(tool, items, Some(hint)));
            };
            items.push(item("kernel", "内核（cua-mcp）", "ok", kernel.display().to_string(), None));

            match run_doctor(&kernel) {
                Ok(doctor) => {
                    items.push(item(
                        "version",
                        "内核版本",
                        "ok",
                        doctor.version.unwrap_or_else(|| "未知".to_string()),
                        None,
                    ));
                    items.push(item(
                        "accessibility",
                        "辅助功能权限",
                        if doctor.accessibility { "ok" } else { "missing" },
                        if doctor.accessibility {
                            "已授权：可以读界面、合成点击"
                        } else {
                            "未授权：读不到界面树，也点不动"
                        },
                        if doctor.accessibility {
                            None
                        } else {
                            Some("系统设置 → 隐私与安全性 → 辅助功能，勾选 AGI Mizar".to_string())
                        },
                    ));
                    items.push(item(
                        "screen",
                        "屏幕录制权限",
                        if doctor.screen_recording { "ok" } else { "missing" },
                        if doctor.screen_recording {
                            "已授权：可以截图"
                        } else {
                            "未授权：截图会是空的"
                        },
                        if doctor.screen_recording {
                            None
                        } else {
                            Some("系统设置 → 隐私与安全性 → 屏幕录制，勾选 AGI Mizar".to_string())
                        },
                    ));
                }
                Err(error) => {
                    items.push(item(
                        "kernel-run",
                        "内核可执行",
                        "missing",
                        format!("cua-mcp 跑不起来：{error}"),
                        Some("确认它是可执行文件（chmod +x），并且是本机的架构".to_string()),
                    ));
                    items.push(item("accessibility", "辅助功能权限", "unknown", "内核未就绪，无法检查", None));
                    items.push(item("screen", "屏幕录制权限", "unknown", "内核未就绪，无法检查", None));
                }
            }
            Ok(report(tool, items, Some(hint)))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

#[derive(Deserialize)]
struct Doctor {
    #[serde(default)]
    accessibility: bool,
    #[serde(default, rename = "screenRecording")]
    screen_recording: bool,
    #[serde(default)]
    version: Option<String>,
}

fn run_doctor(kernel: &std::path::Path) -> Result<Doctor, String> {
    let output = Command::new(kernel)
        .arg("--doctor")
        .arg("--json")
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(if stderr.trim().is_empty() {
            format!("退出码 {}", output.status)
        } else {
            stderr.trim().to_string()
        });
    }
    serde_json::from_slice::<Doctor>(&output.stdout).map_err(|error| format!("输出无法解析：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads only: a fresh kernel lists both tools, both off.
    #[test]
    fn the_toolbox_lists_both_tools_and_both_start_off() {
        let core = Core::new(Vec::new());
        let rows = views(&core);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, INTERCONNECT);
        assert!(!rows[0].enabled);
        assert_eq!(rows[1].id, COMPUTER_USE);
        assert!(!rows[1].enabled);
        assert_eq!(rows[1].available, cfg!(target_os = "macos"));
    }

    #[test]
    fn an_unknown_tool_is_rejected_without_touching_preferences() {
        let core = Core::new(Vec::new());
        assert!(set_enabled(&core, "printer", true).is_err());
        assert!(check(&core, "printer").is_err());
    }
}
