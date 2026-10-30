//! Updates from GitHub Releases.
//!
//! The release workflow publishes the bundle, its signature and a `latest.json`
//! that names it; the updater plugin compares the version there with this build
//! and hands back a download. What is left is the part a person has to agree to:
//! nobody wants a window replaced in the middle of a prompt, so the checking is
//! quiet and the installing happens where the version is shown.
//!
//! The signature is the security boundary, not the HTTPS. The public key is in
//! the bundle; the private key lives in the release runner's secrets, so a
//! compromised download host still cannot hand this app a payload it would take.

use std::sync::RwLock;

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

/// How often a long-running window looks again.
const INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// The answer to "is there a newer build", as the settings page draws it.
#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateInfo {
    /// The build this window is running.
    pub current: String,
    /// The newest release's version, when the check could read one.
    pub latest: Option<String>,
    /// Whether `latest` is newer than `current`.
    pub available: bool,
    /// Why the check could not answer, when it could not.
    pub error: Option<String>,
}

/// The last answer, so the page can draw before it asks again.
static LAST: RwLock<Option<UpdateInfo>> = RwLock::new(None);

/// Record one line about the updater, somewhere a person can read it after the
/// window is gone. The app is usually launched by Finder, so stderr goes nowhere.
fn note(line: &str) {
    eprintln!("AGI update: {line}");
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let directory = std::path::Path::new(&home).join("Library/Logs/AGI Mizar");
    let _ = std::fs::create_dir_all(&directory);
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("update.log"))
    {
        let _ = writeln!(file, "{line}");
    }
}

/// The last stored answer, without touching the network.
pub fn state(app: &AppHandle) -> UpdateInfo {
    if let Some(info) = LAST
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return info;
    }
    UpdateInfo {
        current: app.package_info().version.to_string(),
        ..Default::default()
    }
}

fn store(info: &UpdateInfo) {
    *LAST
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(info.clone());
}

/// Ask GitHub whether there is a newer build.
///
/// Quiet by design: the answer goes to the state the settings page reads and to
/// the log, and nowhere else. A window that interrupts a prompt to say "nothing
/// changed" teaches people to dismiss it without reading.
pub async fn check(app: AppHandle) -> UpdateInfo {
    let current = app.package_info().version.to_string();
    let updater = match app.updater() {
        Ok(updater) => updater,
        Err(error) => {
            let info = UpdateInfo {
                current,
                latest: None,
                available: false,
                error: Some(format!("the updater is not available — {error}")),
            };
            note(&format!("the updater is not available — {error}"));
            store(&info);
            return info;
        }
    };

    let info = match updater.check().await {
        Ok(Some(update)) => {
            note(&format!("{} is available", update.version));
            UpdateInfo {
                current,
                latest: Some(update.version),
                available: true,
                error: None,
            }
        }
        Ok(None) => {
            note("this is the newest version");
            UpdateInfo {
                current,
                latest: None,
                available: false,
                error: None,
            }
        }
        Err(error) => {
            note(&format!("the check failed — {error}"));
            UpdateInfo {
                current,
                latest: None,
                available: false,
                error: Some(format!("{error}")),
            }
        }
    };
    store(&info);
    info
}

/// Check once, then keep checking for as long as this process lives.
pub fn watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // A launch is busy enough without a network round trip in the first second.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        loop {
            check(app.clone()).await;
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// The tray's "Check for Updates…", which has to hand an async job to a sync
/// menu event.
///
/// This one answers out loud: the person just asked.
pub fn spawn_check(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let info = check(app.clone()).await;
        let message = if let Some(error) = info.error {
            format!("Could not check for updates.\n\n{error}")
        } else if info.available {
            format!(
                "AGI Mizar {} is available.",
                info.latest.unwrap_or_default()
            )
        } else {
            format!("AGI Mizar {} is the newest version.", info.current)
        };
        use tauri_plugin_dialog::DialogExt;
        app.dialog()
            .message(message)
            .title("AGI Mizar Update")
            .show(|_| {});
    });
}

/// Download, verify and install, then come back up as the new version.
pub async fn install(app: AppHandle) -> Result<(), String> {
    let updater = app
        .updater()
        .map_err(|error| format!("the updater is not available — {error}"))?;
    let update = updater
        .check()
        .await
        .map_err(|error| format!("{error}"))?
        .ok_or_else(|| "this build is already the newest".to_string())?;
    note(&format!("installing {}", update.version));
    update
        .download_and_install(|_chunk, _total| {}, || {})
        .await
        .map_err(|error| {
            note(&format!("the update was not installed — {error}"));
            format!("{error}")
        })?;
    app.restart();
}
