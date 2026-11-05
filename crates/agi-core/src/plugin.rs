//! The `dsh-mizar` package that lives inside DSH.
//!
//! The desktop cannot install an engine the way it spawns Codex or Claude: DSH
//! owns its profiles, and the plugin is a package in one of them. This module is
//! the one place that knows how to read that installation, ask npm what the
//! newest compatible release is, and drive DSH's own `dsh plugin` command when
//! the operator asks for a repair or an update.
//!
//! ### Why the version comes from the profile, not the link
//!
//! A connected plugin tells us its version in the handshake, which is the
//! freshest answer while DSH runs — but the whole point of the check is to work
//! before a plugin is attached, and to notice an install that is on disk and
//! will not load. Reading the profile's `node_modules` answers both, and does so
//! even when DSH is not running to say hello.
//!
//! ### Why `dsh plugin add` rather than pnpm directly
//!
//! DSH's command does more than `pnpm add`: it registers the package in
//! `dsh.profile.bundles` (a dependency that is installed but not listed composes
//! nothing) and rolls the profile back when an install fails. Shelling out to
//! pnpm would reproduce those rules badly, and the rules are DSH's to change.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agi_protocol::api::{DshPluginView, DSH_PLUGIN_MIN_VERSION};

/// The npm package and profile bundle name.
pub const PACKAGE: &str = "dsh-mizar";

/// Whether `candidate` is a newer release than `current`.
///
/// Only the numeric release is compared. A prerelease suffix (`-rc.1`) and build
/// metadata (`+sha`) do not make a version newer: npm's `latest` tag normally
/// points at a release anyway, and a comparator that reads `0.9.0-rc.1` as four
/// segments would call a release candidate newer than the release it precedes.
pub fn release_is_newer(candidate: &str, current: &str) -> bool {
    fn parts(version: &str) -> Vec<u64> {
        let release = version
            .trim()
            .trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or_default();
        release
            .split('.')
            .map(|part| {
                part.chars()
                    .take_while(|character| character.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u64>()
                    .unwrap_or(0)
            })
            .collect()
    }
    let mut candidate = parts(candidate);
    let mut current = parts(current);
    let length = candidate.len().max(current.len());
    candidate.resize(length, 0);
    current.resize(length, 0);
    candidate > current
}

/// Where the plugin's latest release is published.
const REGISTRY_LATEST: &str = "https://registry.npmjs.org/dsh-mizar/latest";

/// How long one install may take.
///
/// DSH forwards the command to pnpm, and a fresh profile downloads the plugin
/// and its peers. Two minutes was too tight on a cold cache over hotel Wi-Fi;
/// the ceiling only exists so a hung package manager cannot leave the window
/// spinning for the rest of the session.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(240);

/// How much package-manager output is kept for an error line.
const OUTPUT_TAIL: usize = 1_200;

/// Where DSH keeps its profiles: `$DSH_HOME`, or `~/.dsh`.
pub fn dsh_home() -> PathBuf {
    std::env::var("DSH_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| agi_platform::home().join(".dsh"))
}

/// The profile the plugin is installed in.
///
/// `desktop` is the profile DSH's own desktop app uses, and the first choice
/// when it exists. A host that only ever made another profile still gets its
/// answer: the first profile that already names the plugin wins, so a repair
/// never installs a second copy beside the working one.
pub fn profile_name(home: &Path) -> String {
    let profiles = home.join("profiles");
    let desktop = profiles.join("desktop").join("package.json");
    if desktop.is_file() {
        return "desktop".to_string();
    }
    let mut found: Option<String> = None;
    if let Ok(entries) = std::fs::read_dir(&profiles) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            if !mentions_plugin(&entry.path().join("package.json")) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // Deterministic even when the directory order is not: several
            // profiles may name the plugin, and any stable answer is better
            // than a different one per launch.
            if found
                .as_deref()
                .is_none_or(|current| name.as_str() < current)
            {
                found = Some(name);
            }
        }
    }
    found.unwrap_or_else(|| "desktop".to_string())
}

/// The installed plugin's version, from the package the profile resolves.
///
/// A missing `node_modules` entry is "not installed"; a package that says it is
/// installed but has no readable version is folded into the same answer, because
/// neither can answer a handshake.
pub fn installed_version(profile_dir: &Path) -> Option<String> {
    let manifest = profile_dir
        .join("node_modules")
        .join(PACKAGE)
        .join("package.json");
    let text = std::fs::read_to_string(manifest).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_string)
}

/// Whether the profile's bundle list names the plugin.
///
/// `dsh plugin add` writes this list; a hand-copied `node_modules` does not, and
/// that install loads nothing at all. It is worth its own dependency row because
/// the fix is the same command and the symptom is otherwise invisible.
pub fn bundle_listed(profile_dir: &Path) -> bool {
    let text = match std::fs::read_to_string(profile_dir.join("package.json")) {
        Ok(text) => text,
        Err(_) => return false,
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    value
        .get("dsh")
        .and_then(|dsh| dsh.get("profile"))
        .and_then(|profile| profile.get("bundles"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|bundles| {
            bundles
                .iter()
                .any(|bundle| bundle.as_str() == Some(PACKAGE))
        })
}

/// Whether a profile's `package.json` already mentions the plugin.
fn mentions_plugin(manifest: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let dependency = value
        .get("dependencies")
        .and_then(|dependencies| dependencies.get(PACKAGE))
        .is_some();
    let bundled = value
        .get("dsh")
        .and_then(|dsh| dsh.get("profile"))
        .and_then(|profile| profile.get("bundles"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|bundles| {
            bundles
                .iter()
                .any(|bundle| bundle.as_str() == Some(PACKAGE))
        });
    dependency || bundled
}

/// Locate DSH's command line, which is what a one-click install drives.
///
/// A Finder-launched app gets launchd's `PATH`, so `which("dsh")` cannot be the
/// only answer: the macOS bundle ships a launcher at a fixed path, and Windows
/// installs into the user's Programs directory. `AGI_DSH_CLI` overrides both for
/// a development tree or a non-standard install.
pub fn find_cli() -> Option<PathBuf> {
    if let Some(spelled) = std::env::var_os("AGI_DSH_CLI") {
        let path = PathBuf::from(spelled);
        // An explicit override is authoritative: falling back to a different
        // DSH because the named one moved would install into the wrong tree.
        return path.is_file().then_some(path);
    }
    if let Some(found) = agi_platform::which("dsh") {
        return Some(found);
    }

    let home = agi_platform::home();
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from(
            "/Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh",
        ));
        candidates.push(
            home.join("Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh"),
        );
        candidates.push(PathBuf::from("/usr/local/bin/dsh"));
        candidates.push(home.join(".local/bin/dsh"));
    }
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(local)
                    .join("Programs")
                    .join("DeepSeek Harness")
                    .join("resources")
                    .join("runtime")
                    .join("cli")
                    .join("bin")
                    .join("dsh.cmd"),
            );
        }
        for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(programs) = std::env::var_os(variable) {
                candidates.push(
                    PathBuf::from(programs)
                        .join("DeepSeek Harness")
                        .join("resources")
                        .join("runtime")
                        .join("cli")
                        .join("bin")
                        .join("dsh.cmd"),
                );
            }
        }
        candidates.push(home.join(".local").join("bin").join("dsh.cmd"));
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        candidates.push(home.join(".local").join("bin").join("dsh"));
        candidates.push(PathBuf::from("/usr/local/bin/dsh"));
    }

    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// What this machine knows right now, before npm is asked anything.
pub fn initial_view() -> DshPluginView {
    let home = dsh_home();
    let profile = profile_name(&home);
    let installed = installed_version(&home.join("profiles").join(&profile));
    DshPluginView {
        installed,
        profile,
        cli: find_cli().is_some(),
        minimum: DSH_PLUGIN_MIN_VERSION.to_string(),
        ..DshPluginView::default()
    }
}

/// The view after a check, with the update decision made in one place.
///
/// `latest` is `None` when npm could not be reached; `error` then says why. An
/// installed version that is *ahead* of npm (a local build of the next release)
/// is deliberately not called an update: a popup that offers a downgrade would
/// be worse than no popup at all.
pub fn view(
    installed: Option<String>,
    latest: Option<String>,
    error: Option<String>,
    last_checked: Option<i64>,
) -> DshPluginView {
    let home = dsh_home();
    let profile = profile_name(&home);
    let compatible = latest
        .as_deref()
        .filter(|latest| !release_is_newer(DSH_PLUGIN_MIN_VERSION, latest));
    let update_available = compatible.is_some_and(|latest| {
        installed
            .as_deref()
            .is_none_or(|current| release_is_newer(latest, current))
    });
    DshPluginView {
        installed,
        latest,
        minimum: DSH_PLUGIN_MIN_VERSION.to_string(),
        update_available,
        profile,
        cli: find_cli().is_some(),
        error,
        last_checked,
    }
}

/// Ask npm for the newest published version.
pub async fn latest_version() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("AGI-Mizar/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("无法连接 npm：{error}"))?;
    let response = client
        .get(REGISTRY_LATEST)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| format!("无法连接 npm：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("npm 返回了 {}", response.status()));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("npm 的返回无法解析：{error}"))?;
    body.get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "npm 没有返回版本号".to_string())
}

/// What one successful install did.
#[derive(Debug, Clone)]
pub struct Installed {
    /// The version found on disk after the command finished.
    pub version: Option<String>,
    /// The package manager's own summary, trimmed for the window.
    pub output: String,
}

/// Install or update the plugin through DSH's own command.
///
/// Runs `dsh plugin --profile <name> add dsh-mizar@<version>`. The profile is
/// resolved from the same home the version reader uses, and `DSH_HOME` is passed
/// to the child so a host with the variable set cannot install into a different
/// tree than the one this app inspected.
pub async fn install(version: &str) -> Result<Installed, String> {
    let home = dsh_home();
    let profile = profile_name(&home);
    let cli = find_cli().ok_or_else(|| {
        "没有找到 dsh 命令行；请确认 DeepSeek Harness 已安装，或在终端手动运行安装命令".to_string()
    })?;
    let profile_dir = home.join("profiles").join(&profile);

    let mut first = run_install(&cli, &home, &profile, version).await?;
    // The Windows pnpm hoisted-install race is a known one-off: the same command
    // usually succeeds when repeated, and a repair that gives up on the first
    // ENOENT is not a one-click fix. Only that signature is retried, and only
    // on Windows (see dsh.plugin-windows-enoent in the workspace wiki).
    if cfg!(windows) && !first.status.success() && first.text.contains("ERR_PNPM_ENOENT") {
        tracing::warn!("dsh plugin: retrying after ERR_PNPM_ENOENT");
        first = run_install(&cli, &home, &profile, version).await?;
    }

    if !first.status.success() {
        let code = first
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "中断".to_string());
        return Err(format!(
            "dsh plugin 安装失败（退出码 {code}）：{}",
            tail(&first.text)
        ));
    }

    let installed = installed_version(&profile_dir);
    if installed.is_none() {
        return Err(format!(
            "dsh plugin 已结束，但 {} 下仍找不到插件：{}",
            profile_dir.display(),
            tail(&first.text)
        ));
    }
    Ok(Installed {
        version: installed,
        output: tail(&first.text),
    })
}

/// What one command produced, success or not.
struct Ran {
    status: std::process::ExitStatus,
    text: String,
}

async fn run_install(cli: &Path, home: &Path, profile: &str, version: &str) -> Result<Ran, String> {
    let mut command = agi_platform::command(cli);
    command
        .arg("plugin")
        .arg("--profile")
        .arg(profile)
        .arg("add")
        .arg(format!("{PACKAGE}@{version}"))
        // A timeout that leaves the installer running would be worse than the
        // hang it is there to prevent: the next click would race this one.
        .kill_on_drop(true)
        // The package manager must not wait on a prompt nobody can see.
        .env("DSH_HOME", home)
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = tokio::time::timeout(INSTALL_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            format!(
                "安装超时（{} 秒）：dsh plugin 仍在运行，可以稍后在终端运行 `dsh plugin --profile {profile} add {PACKAGE}@{version}`",
                INSTALL_TIMEOUT.as_secs()
            )
        })?
        .map_err(|error| format!("无法运行 dsh：{error}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(Ran {
        status: output.status,
        text,
    })
}

/// The end of a command's output, where the reason usually is.
fn tail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "没有输出".to_string();
    }
    let length = trimmed.chars().count();
    if length <= OUTPUT_TAIL {
        return trimmed.to_string();
    }
    let skip = length - OUTPUT_TAIL;
    format!("…{}", trimmed.chars().skip(skip).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_numeric_release_compares_as_a_release() {
        assert!(release_is_newer("0.9.1", "0.9.0"));
        assert!(release_is_newer("0.10.0", "0.9.9"));
        assert!(release_is_newer("1.0.0", "0.99.99"));
        assert!(!release_is_newer("0.9.0", "0.9.0"));
        assert!(!release_is_newer("0.9.0-rc.1", "0.9.0"));
        assert!(!release_is_newer("0.8.14", "0.9.0"));
    }

    #[test]
    fn the_profile_is_desktop_even_before_it_is_created() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(profile_name(dir.path()), "desktop");
    }

    #[test]
    fn a_profile_that_names_the_plugin_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles").join("web");
        write(
            &profile.join("package.json"),
            r#"{"dependencies":{"dsh-mizar":"0.9.2"}}"#,
        );
        write(
            &profile
                .join("node_modules")
                .join(PACKAGE)
                .join("package.json"),
            r#"{"name":"dsh-mizar","version":"0.9.2"}"#,
        );
        assert_eq!(profile_name(dir.path()), "web");
        assert_eq!(
            installed_version(&dir.path().join("profiles").join("web")).as_deref(),
            Some("0.9.2")
        );
    }

    #[test]
    fn the_bundle_list_is_what_dsh_actually_composes() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles").join("desktop");
        write(
            &profile.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","dsh-mizar"]}}}"#,
        );
        assert!(bundle_listed(&profile));

        write(
            &profile.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base"]}}}"#,
        );
        assert!(!bundle_listed(&profile));
    }

    #[test]
    fn a_latest_that_is_older_than_the_floor_is_not_an_update() {
        let view = view(
            Some("0.8.14".to_string()),
            Some("0.8.14".to_string()),
            None,
            Some(1),
        );
        assert!(!view.update_available);
        assert_eq!(view.minimum, DSH_PLUGIN_MIN_VERSION);
    }

    #[test]
    fn an_installed_version_ahead_of_npm_is_not_a_downgrade_prompt() {
        let view = view(
            Some("0.9.0".to_string()),
            Some("0.8.14".to_string()),
            None,
            Some(1),
        );
        assert!(!view.update_available);
    }

    #[test]
    fn a_newer_compatible_release_is_an_update() {
        let view = view(
            Some("0.9.0".to_string()),
            Some("0.9.3".to_string()),
            None,
            Some(1),
        );
        assert!(view.update_available);
        assert_eq!(view.latest.as_deref(), Some("0.9.3"));
    }

    #[test]
    fn a_missing_plugin_with_a_compatible_release_is_an_install() {
        let view = view(None, Some("0.9.0".to_string()), None, Some(1));
        assert!(view.update_available);
    }
}
