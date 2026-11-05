//! The dependencies each agent needs on this side of the connection.
//!
//! "Installed" is not the same as "able to talk to us": Claude Code needs eight
//! hooks in its settings file, DSH needs the `dsh-mizar` package installed *and*
//! listed in its bundle tree, and Codex needs an app-server that starts. The
//! agent page asks for one report per agent, and the one button next to it asks
//! for the same report after repairing whatever can be repaired.
//!
//! Everything here is either a read of an existing file or a call into the
//! engine that was already the owner of that file; no adapter gains a second
//! notion of "configured".

use agi_protocol::api::{DependencyItem, DependencyReport, DSH_PLUGIN_MIN_VERSION};
use agi_protocol::engine::EngineId;

use crate::plugin;
use crate::Core;

/// Read one agent's dependencies without changing anything.
pub fn check(core: &Core, engine: EngineId) -> DependencyReport {
    let items = match engine {
        EngineId::Dsh => dsh_items(core),
        EngineId::Claude => claude_items(core),
        EngineId::Codex => codex_items(core),
    };
    report(engine, items, Vec::new(), None)
}

/// Check, repair what this app can repair, and check again.
///
/// A repair never fails the command: the answer is the report after the attempt,
/// with [`DependencyReport::error`] carrying the reason it is still incomplete.
/// That is deliberate — the window's one button has to be able to say "I tried,
/// and here is the sentence from the package manager" instead of turning into a
/// bare error toast.
pub async fn repair(core: &Core, engine: EngineId) -> DependencyReport {
    let mut changed: Vec<String> = Vec::new();
    let mut error: Option<String> = None;

    match engine {
        EngineId::Dsh => repair_dsh(core, &mut changed, &mut error).await,
        EngineId::Claude => repair_claude(core, &mut changed, &mut error),
        EngineId::Codex => repair_codex(core, &mut changed, &mut error).await,
    }

    let items = match engine {
        EngineId::Dsh => dsh_items(core),
        EngineId::Claude => claude_items(core),
        EngineId::Codex => codex_items(core),
    };
    report(engine, items, changed, error)
}

fn report(
    engine: EngineId,
    items: Vec<DependencyItem>,
    changed: Vec<String>,
    error: Option<String>,
) -> DependencyReport {
    let problems = items
        .iter()
        .filter(|item| matches!(item.status.as_str(), "missing" | "outdated"))
        .count();
    let ok = problems == 0;
    let summary = if ok {
        "连接依赖完整".to_string()
    } else {
        format!("{problems} 项依赖需要处理")
    };
    DependencyReport {
        engine,
        ok,
        summary,
        items,
        changed,
        error,
    }
}

fn item(
    id: &str,
    label: &str,
    status: &str,
    detail: impl Into<String>,
    fix: Option<&str>,
) -> DependencyItem {
    DependencyItem {
        id: id.to_string(),
        label: label.to_string(),
        status: status.to_string(),
        detail: detail.into(),
        fix: fix.map(str::to_string),
    }
}

// ── DSH ────────────────────────────────────────────────────────────────────

fn dsh_items(core: &Core) -> Vec<DependencyItem> {
    let status = core.status_of(EngineId::Dsh);
    let view = core.dsh_plugin_view();
    let profile_dir = plugin::dsh_home().join("profiles").join(&view.profile);
    let mut items = Vec::new();

    items.push(match status {
        Some(status) if status.available => item(
            "dsh-app",
            "DeepSeek Harness",
            "ok",
            if status.connected {
                "已安装并连接"
            } else {
                "已安装，等待插件连接"
            },
            None,
        ),
        _ => item(
            "dsh-app",
            "DeepSeek Harness",
            "missing",
            "本机没有找到 DeepSeek Harness，先安装它",
            None,
        ),
    });

    items.push(match plugin::find_cli() {
        Some(path) => item(
            "dsh-cli",
            "dsh 命令行",
            "ok",
            path.display().to_string(),
            None,
        ),
        None => item(
            "dsh-cli",
            "dsh 命令行",
            "missing",
            "没有找到 dsh，只能手动运行安装命令",
            None,
        ),
    });

    let plugin_needs_fix = match view.installed.as_deref() {
        None => Some("安装插件"),
        Some(version) if plugin::release_is_newer(DSH_PLUGIN_MIN_VERSION, version) => {
            Some("更新插件")
        }
        Some(_) => None,
    };
    items.push(match (view.installed.as_deref(), plugin_needs_fix) {
        (None, _) => item(
            "dsh-plugin",
            "dsh-mizar 插件",
            "missing",
            format!("尚未安装（需要 ≥ {DSH_PLUGIN_MIN_VERSION}）"),
            Some("安装插件"),
        ),
        (Some(version), Some(_)) => item(
            "dsh-plugin",
            "dsh-mizar 插件",
            "outdated",
            format!("已安装 {version}，本应用需要 ≥ {DSH_PLUGIN_MIN_VERSION}"),
            Some("更新插件"),
        ),
        (Some(version), None) => item(
            "dsh-plugin",
            "dsh-mizar 插件",
            "ok",
            format!("已安装 {version}"),
            None,
        ),
    });

    let installed = view.installed.is_some();
    let listed = plugin::bundle_listed(&profile_dir);
    items.push(if !installed {
        item(
            "dsh-bundle",
            "DSH 组合树",
            "unknown",
            "安装插件时会自动写入 dsh.profile.bundles",
            None,
        )
    } else if listed {
        item(
            "dsh-bundle",
            "DSH 组合树",
            "ok",
            format!("已列入 {} 的 bundles", view.profile),
            None,
        )
    } else {
        item(
            "dsh-bundle",
            "DSH 组合树",
            "missing",
            "插件装在 node_modules 里，但不在 DSH 的 bundles 列表中，DSH 不会加载它",
            Some("重新安装以写入组合树"),
        )
    });

    let latest = view.latest.as_deref();
    let installed_compatible = view
        .installed
        .as_deref()
        .is_some_and(|installed| !plugin::release_is_newer(DSH_PLUGIN_MIN_VERSION, installed));
    let npm_item = match (latest, view.error.as_deref()) {
        // The registry's newest release is older than this app speaks. No
        // update exists to offer; the plugin row already says what is needed.
        (Some(latest), _)
            if !installed_compatible
                && plugin::release_is_newer(DSH_PLUGIN_MIN_VERSION, latest) =>
        {
            item(
                "dsh-npm",
                "npm 最新版本",
                "outdated",
                format!("npm 最新 {latest}，低于本应用要求的 {DSH_PLUGIN_MIN_VERSION}"),
                None,
            )
        }
        (Some(latest), _) if view.update_available => item(
            "dsh-npm",
            "npm 最新版本",
            "outdated",
            format!("可更新到 {latest}"),
            Some("更新到最新版本"),
        ),
        (Some(latest), _) => {
            let ahead = view
                .installed
                .as_deref()
                .is_some_and(|installed| plugin::release_is_newer(installed, latest));
            item(
                "dsh-npm",
                "npm 最新版本",
                "ok",
                if ahead {
                    format!("npm 最新 {latest}；本机版本较新")
                } else {
                    format!("已是最新 {latest}")
                },
                None,
            )
        }
        (None, Some(error)) => item(
            "dsh-npm",
            "npm 最新版本",
            "unknown",
            format!("无法检查 npm：{error}"),
            None,
        ),
        (None, None) => item(
            "dsh-npm",
            "npm 最新版本",
            "unknown",
            "还没有检查",
            Some("检查插件更新"),
        ),
    };
    items.push(npm_item);

    let endpoint = crate::endpoint::default_endpoint_path();
    items.push(if endpoint.is_file() {
        item(
            "dsh-endpoint",
            "连接地址",
            "ok",
            format!("已发布到 {}", endpoint.display()),
            None,
        )
    } else {
        item(
            "dsh-endpoint",
            "连接地址",
            "missing",
            "桌面端还没有写出 endpoint.json，重启应用后再试",
            None,
        )
    });

    items
}

async fn repair_dsh(core: &Core, changed: &mut Vec<String>, error: &mut Option<String>) {
    let available = core
        .status_of(EngineId::Dsh)
        .is_some_and(|status| status.available);
    if !available {
        *error =
            Some("本机没有找到 DeepSeek Harness，安装后点「检查依赖完整性」再来一次".to_string());
        return;
    }

    // A check first: the repair must act on a fresh npm answer, not on a
    // snapshot from hours ago.
    let view = core.refresh_dsh_plugin(true).await;
    let profile_dir = plugin::dsh_home().join("profiles").join(&view.profile);
    let installed = view.installed.clone();
    let compatible = view
        .latest
        .as_deref()
        .filter(|latest| !plugin::release_is_newer(DSH_PLUGIN_MIN_VERSION, latest))
        .map(str::to_string);
    let too_old = installed
        .as_deref()
        .is_some_and(|version| plugin::release_is_newer(DSH_PLUGIN_MIN_VERSION, version));
    let newer = compatible.as_deref().is_some_and(|latest| {
        plugin::release_is_newer(latest, installed.as_deref().unwrap_or("0.0.0"))
    });
    let bundle_missing = installed.is_some() && !plugin::bundle_listed(&profile_dir);

    if installed.is_some() && !too_old && !newer && !bundle_missing {
        return;
    }

    // What to install: npm's newest compatible release, or the version already
    // on disk when only the bundle registration is broken. Never a version older
    // than the one on disk — the button repairs and updates, it must not
    // downgrade a local build of the next release.
    let wanted = match (&compatible, &installed) {
        (Some(latest), Some(current)) if plugin::release_is_newer(current, latest) => {
            Some(current.clone())
        }
        (Some(latest), _) => Some(latest.clone()),
        (None, Some(current)) if !too_old => Some(current.clone()),
        _ => None,
    };
    // A missing or too-old plugin cannot be repaired from a registry that has no
    // compatible release yet. Re-installing the old version would claim success
    // and change nothing.
    let Some(wanted) = wanted else {
        *error = Some(match view.error.as_deref() {
            Some(reason) => format!("暂时无法安装插件：{reason}"),
            None => {
                format!("npm 上还没有 ≥ {DSH_PLUGIN_MIN_VERSION} 的 dsh-mizar，暂时无法自动安装")
            }
        });
        return;
    };

    match core.install_dsh_plugin(Some(wanted.clone())).await {
        Ok(after) => {
            let version = after.installed.unwrap_or(wanted);
            changed.push(format!(
                "已安装 dsh-mizar {version}，重启 DeepSeek Harness 后生效"
            ));
            if plugin::bundle_listed(&profile_dir) {
                changed.push("已写入 DSH 的 dsh.profile.bundles".to_string());
            }
        }
        Err(reason) => *error = Some(reason),
    }
}

// ── Claude Code ────────────────────────────────────────────────────────────

fn claude_items(core: &Core) -> Vec<DependencyItem> {
    let status = core.status_of(EngineId::Claude);
    let settings = agi_engine_claude::paths::settings_path();
    let installed = agi_engine_claude::installed_count(&settings).unwrap_or(0);
    let expected = agi_engine_claude::HOOK_EVENTS.len();
    let mut items = Vec::new();

    items.push(match status {
        Some(status) if status.available => item(
            "claude-cli",
            "Claude Code",
            "ok",
            status
                .detail
                .unwrap_or_else(|| "claude 命令行可用".to_string()),
            None,
        ),
        _ => item(
            "claude-cli",
            "Claude Code",
            "missing",
            "没有找到 claude 命令行；装好后点「检查依赖完整性」",
            None,
        ),
    });

    let detail = format!(
        "{installed}/{expected} 个 hooks 已写入 {}",
        settings.display()
    );
    items.push(match installed {
        0 => item(
            "claude-hooks",
            "Claude Code hooks",
            "missing",
            format!(
                "没有写入 {}；Claude Code 的审批不会到达这个窗口",
                settings.display()
            ),
            Some("写入 hooks（先备份 settings.json）"),
        ),
        count if count < expected => item(
            "claude-hooks",
            "Claude Code hooks",
            "outdated",
            format!("{count}/{expected} 个已写入；重新写入补齐"),
            Some("补齐 hooks"),
        ),
        _ => item("claude-hooks", "Claude Code hooks", "ok", detail, None),
    });

    items
}

fn repair_claude(core: &Core, changed: &mut Vec<String>, error: &mut Option<String>) {
    let settings = agi_engine_claude::paths::settings_path();
    let before = agi_engine_claude::installed_count(&settings).unwrap_or(0);
    match core.install_claude_hooks() {
        Ok(()) => {
            let after = agi_engine_claude::installed_count(&settings).unwrap_or(0);
            if after > before {
                changed.push(format!(
                    "已写入 {} 个 Claude Code hooks（原 {before} 个）",
                    after - before
                ));
            } else {
                changed.push("Claude Code hooks 已经是完整的".to_string());
            }
        }
        Err(reason) => *error = Some(reason),
    }
}

// ── Codex ──────────────────────────────────────────────────────────────────

fn codex_items(core: &Core) -> Vec<DependencyItem> {
    let status = core.status_of(EngineId::Codex);
    let available = status.as_ref().is_some_and(|status| status.available);
    let detail = status.as_ref().and_then(|status| status.detail.clone());
    let engine = core.engine(EngineId::Codex);
    let (connected, engine_detail) = engine
        .as_ref()
        .map(|engine| engine.status())
        .unwrap_or((false, None));
    let mut items = Vec::new();

    items.push(if available {
        item(
            "codex-app",
            "Codex app-server",
            "ok",
            engine_detail
                .or(detail)
                .unwrap_or_else(|| "已找到可执行文件".to_string()),
            None,
        )
    } else {
        item(
            "codex-app",
            "Codex app-server",
            "missing",
            "没有找到 codex；请安装 ChatGPT 应用或 codex 命令行",
            None,
        )
    });

    let connection = if connected {
        item(
            "codex-link",
            "app-server 连接",
            "ok",
            "已连接".to_string(),
            None,
        )
    } else if available {
        item(
            "codex-link",
            "app-server 连接",
            "missing",
            "可执行文件存在，但 app-server 还没有应答",
            Some("重新启动 app-server"),
        )
    } else {
        item(
            "codex-link",
            "app-server 连接",
            "unknown",
            "安装 Codex 后才能连接",
            None,
        )
    };
    items.push(connection);
    items
}

async fn repair_codex(core: &Core, changed: &mut Vec<String>, error: &mut Option<String>) {
    if let Some(engine) = core.engine(EngineId::Codex) {
        // A healthy link has nothing to repair. Codex's `reconnect` is only a
        // session refresh, so calling it unconditionally made a complete check
        // print "已请求 ... 重新连接" -- a change the person never asked for.
        let (connected, _) = engine.status();
        if connected {
            return;
        }
        match engine.reconnect().await {
            Ok(()) => changed.push("已请求 Codex app-server 重新连接".to_string()),
            Err(reason) => *error = Some(format!("{reason}")),
        }
        return;
    }

    let before = core
        .status_of(EngineId::Codex)
        .is_some_and(|status| status.available);
    let found = core
        .rescan_engines()
        .into_iter()
        .find(|status| status.engine == EngineId::Codex)
        .is_some_and(|status| status.available);
    if found && !before {
        changed.push("已重新检测到 Codex".to_string());
    } else {
        *error = Some("没有找到 Codex，请先安装 ChatGPT 应用或 codex 命令行".to_string());
    }
}
