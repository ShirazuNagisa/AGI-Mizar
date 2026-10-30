//! The menu bar icon.
//!
//! The window is the whole product, but it is not the whole app: closing it
//! must leave the kernel (and the phone's connection) running, and the only way
//! back in is a status item. These things belong together — the tray, the
//! window's close behaviour, the three process switches — so they live in one
//! file.

use tauri::menu::{CheckMenuItem, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};

use crate::{login_item, shell};

/// The window's id, as the shell builds it.
const WINDOW: &str = "main";

/// Install the status item and the close-to-tray behaviour.
pub fn install(app: &AppHandle, window: &WebviewWindow) -> tauri::Result<()> {
    let settings = shell::load();

    let show = MenuItemBuilder::with_id("show", "Show AGI Mizar").build(app)?;
    let at_login = CheckMenuItem::with_id(
        app,
        "login",
        "Start at login",
        true,
        login_item::is_enabled(),
        None::<&str>,
    )?;
    let dock = CheckMenuItem::with_id(
        app,
        "dock",
        "Show Dock Icon",
        true,
        settings.show_dock_icon,
        None::<&str>,
    )?;
    let menu_only = CheckMenuItem::with_id(
        app,
        "menu_only",
        "Tray Opens Menu",
        true,
        settings.tray_menu_only,
        None::<&str>,
    )?;

    // The window's theme belongs to the kernel, and the menu has no window to
    // ask. The kernel's address and token are remembered at launch for exactly
    // this kind of call.
    let system = MenuItemBuilder::with_id("theme:system", "System").build(app)?;
    let light = MenuItemBuilder::with_id("theme:light", "Light").build(app)?;
    let dark = MenuItemBuilder::with_id("theme:dark", "Dark").build(app)?;
    let theme = SubmenuBuilder::new(app, "Theme")
        .items(&[&system, &light, &dark])
        .build()?;

    let update = MenuItemBuilder::with_id("update", "Check for Updates…").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit AGI Mizar").build(app)?;

    let menu = MenuBuilder::new(app)
        .items(&[
            &show, &at_login, &dock, &menu_only, &theme, &update, &quit,
        ])
        .build()?;

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

    builder
        .tooltip("AGI Mizar")
        .menu(&menu)
        // A left click is "show me the window" — unless the operator asked for
        // the menu instead, which is the menu-bar-only way to work. The flag is
        // read now and re-applied whenever it changes.
        .show_menu_on_left_click(shell::tray_menu_only())
        .on_menu_event({
            let at_login = at_login.clone();
            let dock = dock.clone();
            let menu_only = menu_only.clone();
            move |app, event| {
                let id = event.id.as_ref();
                match id {
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
                    "dock" => {
                        let wanted = !dock.is_checked().unwrap_or(true);
                        let mut next = shell::load();
                        next.show_dock_icon = wanted;
                        match shell::update(app, next) {
                            Ok(applied) => {
                                let _ = dock.set_checked(applied.show_dock_icon);
                            }
                            Err(error) => {
                                eprintln!("AGI Mizar: the Dock icon was not changed — {error}");
                                let _ = dock.set_checked(shell::load().show_dock_icon);
                            }
                        }
                    }
                    "menu_only" => {
                        let wanted = !menu_only.is_checked().unwrap_or(false);
                        let mut next = shell::load();
                        next.tray_menu_only = wanted;
                        match shell::update(app, next) {
                            Ok(applied) => {
                                let _ = menu_only.set_checked(applied.tray_menu_only);
                            }
                            Err(error) => {
                                eprintln!("AGI Mizar: the tray behaviour was not changed — {error}");
                                let _ = menu_only.set_checked(shell::load().tray_menu_only);
                            }
                        }
                    }
                    other if other.starts_with("theme:") => {
                        crate::kernel::set_theme(other.trim_start_matches("theme:"));
                    }
                    _ => {}
                }
            }
        })
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                // With the menu bound to the left click, the platform opens it;
                // there is nothing to do here but get out of the way.
                if !shell::tray_menu_only() {
                    show_window(tray.app_handle());
                }
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
