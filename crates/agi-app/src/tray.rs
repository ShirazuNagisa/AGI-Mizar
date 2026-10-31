//! The menu bar icon.
//!
//! The window is the whole product, but it is not the whole app: closing it
//! must leave the kernel (and the phone's connection) running, and the only way
//! back in is a status item. These things belong together — the tray, the
//! window's close behaviour, the three process switches — so they live in one
//! file.
//!
//! The menu answers two questions without opening the window: are the agents
//! connected, and is any phone connected to this computer. Both come from the
//! kernel's state, refreshed while the app runs; the menu is rebuilt only when
//! something in that answer changes, so an open menu does not flicker under the
//! pointer.
//!
//! Every line is bilingual, because the menu bar is the one surface a person
//! sees before they have chosen a language.

use std::sync::Mutex;

use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow, Wry};

use crate::shell;

/// The window's id, as the shell builds it.
const WINDOW: &str = "main";

/// The tray, so a refresh can replace its menu.
static TRAY: Mutex<Option<TrayIcon<Wry>>> = Mutex::new(None);

/// Install the status item and the close-to-tray behaviour.
pub fn install(app: &AppHandle, window: &WebviewWindow) -> tauri::Result<()> {
    // The mark's white cut-out, not the coloured bundle icon: a status item is
    // drawn in the menu bar's own light, and a coloured square next to the
    // system's glyphs is the one place this app looks like a guest.
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray-white.png"))?;

    let builder = TrayIconBuilder::with_id("main").icon(icon);
    // Template would recolour the image to match the menu bar (black on light,
    // white on dark). This icon is already the white cut-out the design calls
    // for, so it stays as drawn.
    #[cfg(target_os = "macos")]
    let builder = builder.icon_as_template(false);

    let tray = builder
        .tooltip("AGI Mizar")
        // The page is a window drawn under the status item, so there is no menu
        // for the platform to open on either button.
        .show_menu_on_left_click(false)
        // A left click is "show me the window" — unless the operator asked for
        // the menu instead, which is the menu-bar-only way to work. The flag is
        // read now and re-applied whenever it changes.
        .show_menu_on_left_click(shell::tray_menu_only())
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_panel(tray.app_handle());
            }
        })
        .build(app)?;

    *TRAY.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(tray);

    // Closing the window hides it. The kernel keeps running, the tray keeps the
    // way back, and the only thing that ends the process is Quit.
    let handle = app.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            if let Some(window) = handle.get_webview_window(WINDOW) {
                let _ = window.hide();
            }
        }
    });

    if let Some(panel) = app.get_webview_window(PANEL) {
        let handle = panel.clone();
        panel.on_window_event(move |event| {
            if let tauri::WindowEvent::Focused(false) = event {
                let _ = handle.hide();
            }
        });
    }

    Ok(())
}

/// The tray page's window id.
const PANEL: &str = "tray-panel";

/// Show the page under the status item, or hide it if it is already open.
pub fn toggle_panel(app: &AppHandle) {
    let Some(panel) = app.get_webview_window(PANEL) else {
        return;
    };
    if panel.is_visible().unwrap_or(false) {
        let _ = panel.hide();
        return;
    }

    // Anchored to the status item's own rectangle, so it sits under the icon
    // wherever the person has dragged it.
    if let Some((x, y)) = tray_anchor(app) {
        let _ = panel.set_position(tauri::PhysicalPosition::new(x, y));
    }
    let _ = panel.show();
    let _ = panel.set_focus();
}

/// Where the panel's top-left corner goes: the status item, right-aligned.
fn tray_anchor(app: &AppHandle) -> Option<(i32, i32)> {
    let tray = TRAY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()?;
    let rect = tray.rect().ok().flatten()?;
    let position = rect.position.to_physical::<i32>(1.0);
    let size = rect.size.to_physical::<u32>(1.0);
    let scale = app
        .get_webview_window(PANEL)
        .and_then(|panel| panel.scale_factor().ok())
        .unwrap_or(1.0);
    let width = (232.0 * scale) as i32;
    Some((
        position.x + size.width as i32 - width,
        position.y + size.height as i32 + (4.0 * scale) as i32,
    ))
}

pub fn show_settings(app: &AppHandle) {
    show_window(app);
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.eval(
            "window.dispatchEvent(new CustomEvent('mizar:navigate', { detail: 'settings' }))",
        );
    }
}

/// Bring the window back, from the tray or from a second launch.
pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.show();
        let _ = window.set_focus();
    }
}
