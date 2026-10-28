//! Updates from GitHub Releases.
//!
//! The release workflow publishes the bundle, its signature and a `latest.json`
//! that names it; the updater plugin compares the version there with this build
//! and hands back a download. What is left is the part a person has to agree to:
//! nobody wants a window replaced in the middle of a prompt, so the check is
//! quiet and the install is offered.
//!
//! The signature is the security boundary, not the HTTPS. The public key below
//! is in the bundle; the private key lives in the release runner's secrets, so a
//! compromised download host still cannot hand this app a payload it would take.

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_updater::UpdaterExt;

/// How often a long-running window looks again.
const INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// The tray's "Check for Updates…", which has to hand an async job to a sync
/// menu event.
pub fn spawn_check(app: AppHandle) {
    tauri::async_runtime::spawn(check(app));
}

/// Check once, then keep checking for as long as this process lives.
pub fn watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // A launch is busy enough without a dialog in the first second.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        loop {
            check(app.clone()).await;
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// Ask GitHub whether there is a newer build, and offer it.
pub async fn check(app: AppHandle) {
    let Ok(updater) = app.updater() else {
        return;
    };
    // No update is the ordinary answer, and a failed check is not worth a
    // dialog: the network will be there at the next interval.
    let Ok(Some(update)) = updater.check().await else {
        return;
    };
    let version = update.version.clone();
    let notes = update.body.clone().unwrap_or_default();
    let message = if notes.trim().is_empty() {
        format!("AGI Mizar {version} is available. Install it now?")
    } else {
        format!("AGI Mizar {version} is available.\n\n{notes}")
    };
    let handle = app.clone();
    app.dialog()
        .message(message)
        .title("AGI Mizar Update")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Install and Restart".to_string(),
            "Later".to_string(),
        ))
        .show(move |install| {
            if install {
                tauri::async_runtime::spawn(install_latest(handle.clone()));
            }
        });
}

/// Download, verify and install, then come back up as the new version.
async fn install_latest(app: AppHandle) {
    let Ok(updater) = app.updater() else {
        return;
    };
    let Ok(Some(update)) = updater.check().await else {
        return;
    };
    let installed = update
        .download_and_install(|_chunk, _total| {}, || {})
        .await;
    match installed {
        Ok(()) => app.restart(),
        Err(error) => {
            eprintln!("AGI Mizar: the update was not installed — {error}");
        }
    }
}
