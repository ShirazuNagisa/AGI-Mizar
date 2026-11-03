//! The tray icon: the menu bar item on macOS, the notification-area icon on
//! Windows.
//!
//! The window is the whole product, but it is not the whole app: closing it must
//! leave the kernel (and the phone's connection) running, and the only way back
//! in is a tray icon. These things belong together — the tray, the window's close
//! behaviour, the panel under the icon — so they live in one file.
//!
//! The two desktops ask for different shapes. macOS draws the icon in the menu
//! bar, where a template image, no menu and a panel hanging from the status
//! item's own rectangle are the native vocabulary. Windows draws it in the
//! notification area, where a right-click menu and a flyout above the pointer
//! are. Both end at the same three places: the window, its settings, and a quit
//! that actually quits.

use std::sync::Mutex;

use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow, Wry};

use crate::shell;

/// The window's id, as the shell builds it.
const WINDOW: &str = "main";

/// The tray page's window id.
const PANEL: &str = "tray-panel";

/// The panel's width in logical pixels, shared with the page.
pub(crate) const PANEL_WIDTH: f64 = 232.0;

/// The tray, so a refresh can replace its menu.
static TRAY: Mutex<Option<TrayIcon<Wry>>> = Mutex::new(None);

/// The point a Windows panel was opened from (the pointer), kept so the resize
/// that follows the first measurement can put the panel back under the icon
/// instead of leaving it floating mid-screen.
#[cfg(windows)]
static PANEL_ANCHOR: Mutex<Option<tauri::PhysicalPosition<f64>>> = Mutex::new(None);

/// Install the tray icon and the close-to-tray behaviour.
pub fn install(app: &AppHandle, window: &WebviewWindow) -> tauri::Result<()> {
    // The mark's white cut-out, not the coloured bundle icon: a status item is
    // drawn in the menu bar's own light, and a coloured square next to the
    // system's glyphs is the one place this app looks like a guest.
    #[cfg(target_os = "macos")]
    let builder = TrayIconBuilder::with_id("main")
        .icon(tauri::image::Image::from_bytes(include_bytes!(
            "../icons/tray-white.png"
        ))?)
        // Template would recolour the image to match the menu bar (black on
        // light, white on dark). This icon is already the white cut-out the
        // design calls for, so it stays as drawn.
        .icon_as_template(false)
        // A left click is "show me the window" — unless the operator asked for
        // the menu instead, which is the menu-bar-only way to work. The flag is
        // read now and re-applied whenever it changes.
        .show_menu_on_left_click(shell::tray_menu_only());

    // The notification area is drawn small and in colour, so the bundle's own
    // tile is the right mark. A right-click menu is where Windows puts the two
    // actions the flyout does not carry (show the window, quit); the flag says
    // whether a left click opens that menu too.
    #[cfg(windows)]
    let builder = TrayIconBuilder::with_id("main")
        .icon(tauri::image::Image::from_bytes(include_bytes!(
            "../icons/32x32.png"
        ))?)
        .menu(&tray_menu(app)?)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_window(app),
            "settings" => show_settings(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .show_menu_on_left_click(shell::tray_menu_only());

    // Tauri's tray also exists on the Unix desktops, where the panel simply
    // follows the pointer; nothing there needs a menu of its own.
    #[cfg(not(any(target_os = "macos", windows)))]
    let builder = TrayIconBuilder::with_id("main")
        .icon(tauri::image::Image::from_bytes(include_bytes!(
            "../icons/32x32.png"
        ))?)
        .show_menu_on_left_click(shell::tray_menu_only());

    let tray = builder
        .tooltip("AGI Mizar")
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                // On Windows, "left click opens the menu" is the menu's own
                // job; opening the panel as well would leave two surfaces
                // fighting for the focus. macOS keeps its one behaviour.
                #[cfg(windows)]
                {
                    if shell::tray_menu_only() {
                        return;
                    }
                }
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

/// The notification-area menu.
///
/// Three rows in the order Windows puts them: the thing the icon is for, the
/// place to change how it behaves, and the way to end the process.
#[cfg(windows)]
fn tray_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<Wry>> {
    tauri::menu::MenuBuilder::new(app)
        .text("open", "打开 AGI Mizar")
        .separator()
        .text("settings", "设置…")
        .text("quit", "退出 AGI Mizar")
        .build()
}

/// Show the page under the tray icon, or hide it if it is already open.
pub fn toggle_panel(app: &AppHandle) {
    let Some(panel) = app.get_webview_window(PANEL) else {
        return;
    };
    if panel.is_visible().unwrap_or(false) {
        let _ = panel.hide();
        return;
    }

    // macOS anchors to the status item's own rectangle, so the panel sits under
    // the icon wherever the person has dragged it.
    #[cfg(target_os = "macos")]
    {
        if let Some((x, y)) = tray_anchor(app) {
            let _ = panel.set_position(tauri::PhysicalPosition::new(x, y));
        }
    }

    // Windows has no rectangle to ask for: the notification area is drawn by
    // Explorer, and the pointer's position at the click is the honest anchor.
    // The pointer's monitor decides the work area, which is what keeps the
    // flyout off the taskbar and off the screen edge it is pinned to.
    #[cfg(windows)]
    {
        let anchor = app.cursor_position().ok();
        *PANEL_ANCHOR
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = anchor;
        let position = match (anchor, panel.outer_size()) {
            (Some(cursor), Ok(size)) => window_anchor(app, size, cursor),
            _ => None,
        };
        if let Some((x, y)) = position {
            let _ = panel.set_position(tauri::PhysicalPosition::new(x, y));
        }
    }

    // The Unix fallback has no status-item rectangle either; the pointer is the
    // anchor the desktop itself would use.
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        if let (Some(cursor), Ok(size)) = (app.cursor_position().ok(), panel.outer_size()) {
            let x = cursor.x as i32 - size.width as i32;
            let y = cursor.y as i32 - size.height as i32;
            let _ = panel.set_position(tauri::PhysicalPosition::new(x, y));
        }
    }

    let _ = panel.show();
    let _ = panel.set_focus();
}

/// Where the panel's top-left corner goes on Windows: above the pointer,
/// right-aligned to it, inside the monitor's work area.
///
/// The notification area can sit on any edge of any monitor, so the corner is
/// placed and then clamped: the panel opens above the pointer when there is
/// room and below it when there is not, and never past the work area's edge —
/// that is already the taskbar. Only the shell knows which monitor the click
/// landed on, which is why this is computed here and not in the page.
#[cfg(windows)]
fn window_anchor(
    app: &AppHandle,
    size: tauri::PhysicalSize<u32>,
    cursor: tauri::PhysicalPosition<f64>,
) -> Option<(i32, i32)> {
    let monitor = app
        .monitor_from_point(cursor.x, cursor.y)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let work = monitor.work_area();
    let margin = (8.0 * monitor.scale_factor()).round() as i32;
    let width = i32::try_from(size.width).ok()?;
    let height = i32::try_from(size.height).ok()?;

    let left = work.position.x + margin;
    let top = work.position.y + margin;
    let right = work.position.x + i32::try_from(work.size.width).ok()? - width - margin;
    let bottom = work.position.y + i32::try_from(work.size.height).ok()? - height - margin;
    // A panel larger than the work area cannot be placed inside it; keeping its
    // top-left corner on screen is better than a panicking clamp.
    let max_x = right.max(left);
    let max_y = bottom.max(top);

    let x = (cursor.x as i32 - width + margin).clamp(left, max_x);
    let above = cursor.y as i32 - height - margin;
    let y = if above < top {
        (cursor.y as i32 + margin).clamp(top, max_y)
    } else {
        above.clamp(top, max_y)
    };
    Some((x, y))
}

/// Re-place the panel after the tray page reports its real height.
///
/// The page is created at a placeholder size and measures itself on its first
/// frame. On Windows the panel opens from the notification area, so shrinking
/// it while leaving the top-left corner alone would lift it off the icon;
/// recomputing from the pointer keeps the bottom edge where the click put it.
#[cfg(windows)]
pub fn reposition_panel(app: &AppHandle, height: f64) {
    let Some(panel) = app.get_webview_window(PANEL) else {
        return;
    };
    if !panel.is_visible().unwrap_or(false) {
        return;
    }
    let anchor = *PANEL_ANCHOR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(cursor) = anchor else {
        return;
    };
    let scale = panel.scale_factor().unwrap_or(1.0);
    let width = (PANEL_WIDTH * scale).round() as u32;
    let height = (height.max(120.0) * scale).round() as u32;
    if let Some((x, y)) = window_anchor(app, tauri::PhysicalSize::new(width, height), cursor) {
        let _ = panel.set_position(tauri::PhysicalPosition::new(x, y));
    }
}

/// Where the macOS panel's top-left corner goes: the status item, right-aligned.
#[cfg(target_os = "macos")]
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
    let width = (PANEL_WIDTH * scale) as i32;
    Some((
        position.x + size.width as i32 - width,
        position.y + size.height as i32 + (4.0 * scale) as i32,
    ))
}

/// Bring the window up on its settings page, from the tray or the tray panel.
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
