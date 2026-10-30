//! What the shell remembers about itself.
//!
//! Three switches, all of them facts about the *process*, not about the kernel:
//! whether the login item exists, whether the Dock icon is drawn, and what a
//! click on the status item does. They live in their own file because two writers
//! to one JSON file is how settings get lost, and the kernel already owns
//! `preferences.json` for the window's own appearance.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// What the shell remembers between launches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellSettings {
    /// Register a login item that starts the app without a window.
    #[serde(default)]
    pub launch_at_login: bool,
    /// Draw the icon in the Dock. Off means menu-bar-only.
    #[serde(default = "default_true")]
    pub show_dock_icon: bool,
    /// A click on the status item opens the menu instead of the window.
    #[serde(default)]
    pub tray_menu_only: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            launch_at_login: false,
            show_dock_icon: true,
            tray_menu_only: false,
        }
    }
}

/// Set once at launch and on every change, so the tray can ask without a lock.
static TRAY_MENU_ONLY: AtomicBool = AtomicBool::new(false);

/// Where the shell's own settings live: beside the kernel's, not inside them.
pub fn path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".agi-mizar").join("shell.json")
}

/// Read the file, falling back to the defaults.
///
/// A first launch has no file; `Default` is the answer then, and the defaults
/// are the ones that do not surprise anybody.
pub fn load() -> ShellSettings {
    let path = path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<ShellSettings>(&text).ok())
        .unwrap_or_default()
}

/// Write the whole file, atomically and privately.
fn save(settings: &ShellSettings) -> Result<(), String> {
    let path = path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("could not serialise the settings: {error}"))?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, bytes)
        .map_err(|error| format!("could not write {}: {error}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&temp, &path)
        .map_err(|error| format!("could not move {} into place: {error}", temp.display()))
}

/// Whether a click on the status item should open the menu.
pub fn tray_menu_only() -> bool {
    TRAY_MENU_ONLY.load(Ordering::Relaxed)
}

/// Read the file and make it true.
pub fn current() -> ShellSettings {
    let settings = load();
    TRAY_MENU_ONLY.store(settings.tray_menu_only, Ordering::Relaxed);
    settings
}

/// Apply a change and remember it.
///
/// Every field is applied before the file is written, so a failure to apply is a
/// failure to remember: a switch that looks on and does nothing is worse than one
/// that snaps back.
pub fn update(app: &AppHandle, change: ShellSettings) -> Result<ShellSettings, String> {
    apply(app, &change)?;
    save(&change)?;
    Ok(change)
}

/// Make the settings true for this process.
pub fn apply(app: &AppHandle, settings: &ShellSettings) -> Result<(), String> {
    TRAY_MENU_ONLY.store(settings.tray_menu_only, Ordering::Relaxed);

    // The login item only exists for a real bundle; in `cargo run` there is no
    // `.app` to point launchd at, and failing here would make the switch useless
    // in exactly the place it is developed.
    if let Err(error) = crate::login_item::set_enabled(settings.launch_at_login) {
        tracing::warn!("login item: {error}");
    }

    // The tray's click behaviour is a builder-time flag, but the tray can be
    // re-armed while it runs.
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_show_menu_on_left_click(settings.tray_menu_only);
    }

    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        let policy = if settings.show_dock_icon {
            ActivationPolicy::Regular
        } else {
            ActivationPolicy::Accessory
        };
        app.set_activation_policy(policy);
    }

    Ok(())
}
