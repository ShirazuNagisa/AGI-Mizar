//! The few things the window may ask the shell to do.
//!
//! The page is served by the kernel over loopback, not by Tauri's asset
//! protocol, so everything it needs normally travels over HTTP. These are the
//! exceptions: actions that belong to the *process* — installing an app update,
//! flipping a login item, quitting — have no meaning inside the kernel and
//! cannot be routed through it.
//!
//! Each command is allowed by name in `permissions/app.toml`, and that file is
//! the list of everything the page can reach. Adding a command without adding a
//! permission means it is denied; that is the intended failure.

use tauri::AppHandle;

use crate::shell::{self, ShellSettings};
use crate::{login_item, updates};

/// Ask GitHub, and answer with what it said.
#[tauri::command]
pub async fn app_update_check(app: AppHandle) -> Result<updates::UpdateInfo, String> {
    Ok(updates::check(app).await)
}

/// The last answer, so the page can draw before asking again.
#[tauri::command]
pub fn app_update_state(app: AppHandle) -> updates::UpdateInfo {
    updates::state(&app)
}

/// Download and install the newer build, then restart into it.
#[tauri::command]
pub async fn app_update_install(app: AppHandle) -> Result<(), String> {
    updates::install(app).await
}

/// The three switches.
#[tauri::command]
pub fn shell_settings_get() -> ShellSettings {
    shell::current()
}

/// Change them: each one takes effect before it is written down.
#[tauri::command]
pub fn shell_settings_set(
    app: AppHandle,
    settings: ShellSettings,
) -> Result<ShellSettings, String> {
    shell::update(&app, settings)
}

/// Start at login, for a script or a test — the same call the switch makes.
#[tauri::command]
pub fn shell_login_item(enabled: bool) -> Result<bool, String> {
    login_item::set_enabled(enabled).map_err(|error| format!("{error}"))?;
    Ok(login_item::is_enabled())
}

/// Quit for real. Closing the window only hides it.
#[tauri::command]
pub fn app_quit(app: AppHandle) {
    app.exit(0);
}
