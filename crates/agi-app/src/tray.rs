//! The menu bar icon.
//!
//! The window is the whole product, but it is not the whole app: closing it
//! must leave the kernel (and the phone's connection) running, and the only way
//! back in is a status item. These three things belong together — the tray, the
//! window's close behaviour, and quitting — so they live in one file.

use tauri::menu::{CheckMenuItem, MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};

use crate::login_item;

/// The window's id, as the shell builds it.
const WINDOW: &str = "main";

/// Install the status item and the close-to-tray behaviour.
pub fn install(app: &AppHandle, window: &WebviewWindow) -> tauri::Result<()> {
    let show = MenuItemBuilder::with_id("show", "Show AGI Mizar").build(app)?;
    let at_login = CheckMenuItem::with_id(
        app,
        "login",
        "Start at login",
        true,
        login_item::is_enabled(),
        None::<&str>,
    )?;
    let update = MenuItemBuilder::with_id("update", "Check for Updates…").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit AGI Mizar").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&show, &at_login, &update, &quit])
        .build()?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("no window icon".into()))?;

    let builder = TrayIconBuilder::with_id("main").icon(icon);
    #[cfg(target_os = "macos")]
    let builder = builder.icon_as_template(true);
    builder
        .tooltip("AGI Mizar")
        .menu(&menu)
        // A left click is "show me the window"; the menu belongs on the right,
        // the way every other menu bar app on this platform behaves.
        .show_menu_on_left_click(false)
        .on_menu_event({
            let at_login = at_login.clone();
            move |app, event| match event.id.as_ref() {
                "show" => show_window(app),
                "update" => crate::updates::spawn_check(app.clone()),
                "quit" => app.exit(0),
                "login" => {
                    let enabled = !at_login.is_checked().unwrap_or(false);
                    match login_item::set_enabled(enabled) {
                        Ok(()) => {
                            let _ = at_login.set_checked(enabled);
                        }
                        Err(error) => {
                            eprintln!("AGI Mizar: start at login was not changed — {error}");
                            let _ = at_login.set_checked(login_item::is_enabled());
                        }
                    }
                }
                _ => {}
            }
        })
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        })
        .build(app)?;

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

    Ok(())
}

/// Bring the window back, from the tray or from a second launch.
pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.show();
        let _ = window.set_focus();
    }
}
