//! The window.
//!
//! A Tauri shell around the same kernel the daemon runs. It does three things:
//! start the kernel, open one window pointed at the kernel's own page, and stay
//! out of the way.
//!
//! ### Why the window loads a URL instead of bundled assets
//!
//! Because the front end talks to the kernel over loopback HTTP, not over Tauri's
//! IPC. Loading `http://127.0.0.1:<port>/?token=…` means the same page runs in a
//! browser during development and in the window afterwards, with no second
//! transport and no second build. When the React app lands it will be served from
//! that same route.
//!
//! ### Why the runtime is leaked
//!
//! The kernel's servers live on a Tokio runtime. `tauri::Builder::run` blocks for
//! the life of the window, so a runtime owned by `main`'s stack would be dropped
//! the moment the window opened — and every server with it. Leaking it is the
//! honest expression of "this lives as long as the process".

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ipc;
mod kernel;
#[cfg(target_os = "macos")]
mod login_item;
#[cfg(target_os = "macos")]
mod shell;
#[cfg(target_os = "macos")]
mod tray;
mod updates;

use agi_boot::{boot, BootConfig};

/// The Dock's mark, drawn from the same two images the bundle carries.
///
/// macOS reads one icon from the bundle's `Info.plist`, so a dark appearance would
/// otherwise get the light tile. The window already knows which appearance it is
/// in — that choice is the kernel's preference, not AppKit's — so the icon is
/// repainted from it, on the main thread, whenever it changes.
#[cfg(target_os = "macos")]
mod dock_icon {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    static LIGHT: &[u8] = include_bytes!("../icons/icon-light.png");
    static DARK: &[u8] = include_bytes!("../icons/icon-dark.png");

    /// Whether the system is in its dark appearance.
    fn system_is_dark(mtm: MainThreadMarker) -> bool {
        let appearance = NSApplication::sharedApplication(mtm).effectiveAppearance();
        appearance.name().to_string().contains("Dark")
    }

    /// Repaint the Dock icon for `theme` (`system`, `light` or `dark`).
    ///
    /// Must run on the main thread; callers that are not already there go through
    /// `AppHandle::run_on_main_thread`.
    pub fn follow(theme: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let dark = match theme {
            "dark" => true,
            "light" => false,
            _ => system_is_dark(mtm),
        };
        let bytes = if dark { DARK } else { LIGHT };
        unsafe {
            let data = NSData::dataWithBytes_length(bytes.as_ptr().cast(), bytes.len());
            let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
                return;
            };
            NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image));
        }
    }
}

/// What `--start-at-login` or `--no-start-at-login` asked for.
#[cfg(target_os = "macos")]
fn start_at_login_flag() -> Option<bool> {
    let mut wanted = None;
    for argument in std::env::args() {
        match argument.as_str() {
            "--start-at-login" => wanted = Some(true),
            "--no-start-at-login" => wanted = Some(false),
            _ => {}
        }
    }
    wanted
}

fn main() {
    // Start at login is also a thing an operator scripts: an MDM image, a
    // headless setup, a test. The tray checkbox and these flags are the same
    // call, so there is only one answer to what "enabled" means.
    #[cfg(target_os = "macos")]
    if let Some(enabled) = start_at_login_flag() {
        match login_item::set_enabled(enabled) {
            Ok(()) => {
                println!("start at login: {}", if enabled { "on" } else { "off" });
                return;
            }
            Err(error) => {
                eprintln!("AGI Mizar: start at login was not changed — {error}");
                std::process::exit(1);
            }
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("could not start the kernel runtime");

    let booted = runtime
        .block_on(boot(BootConfig {
            endpoint_file: None,
            // The hooks are how an approval reaches this window at all: without
            // them Claude Code runs tools unobserved. The installer is additive,
            // backed up and reversible, so the app installs them by default and
            // an operator who wants to manage the file themselves passes
            // `--no-install-claude-hooks`.
            // Installed by the guide, where the person can see it happen. A
            // launch that silently rewrites ~/.claude/settings.json is a
            // surprise, not a feature; `--install-claude-hooks` is the escape
            // hatch for a scripted setup.
            install_claude_hooks: std::env::args()
                .any(|argument| argument == "--install-claude-hooks"),
            // Where the tunnel dials. Two relays run side by side, and only the
            // account one publishes presence to the service the phone reads its
            // desktop list from: dialling the free relay connects the tunnel and
            // still leaves the computer invisible to the phone. Default to the
            // relay that shares a host with the account API, and let
            // `AGI_RELAY_URL` point somewhere else (a local relay, say).
            relay_url: Some(
                std::env::var("AGI_RELAY_URL")
                    .ok()
                    .filter(|url| !url.trim().is_empty())
                    .unwrap_or_else(|| agi_core::account::OFFICIAL_ACCOUNT_URL.to_string()),
            ),
            relay_token: std::env::var("AGI_RELAY_TOKEN").ok(),
        }))
        .expect("the kernel failed to start");

    kernel::remember(booted.api_port, booted.api_token.clone());
    // The run loop owns the shutdown: `kill_on_drop` does not fire when the
    // process exits, and the model runner is a gigabyte of resident weights.
    let core_for_exit = std::sync::Arc::clone(&booted.core);
    // Started by the login item: the point is the kernel behind the window, so
    // the window waits in the menu bar instead of stealing focus at login.
    let background = std::env::args().any(|argument| argument == "--background");

    let url = booted.window_url();
    println!("AGI Mizar — window at {url}");
    println!(
        "  pairing code {} ({})",
        booted.pairing_code.display, booted.pairing_uri
    );

    // Both must outlive the window: the runtime keeps the servers alive, and
    // `booted` owns the kernel the servers read from. The Dock icon reads the
    // theme out of the kernel, so it keeps its own handle.
    #[cfg(target_os = "macos")]
    let core = std::sync::Arc::clone(&booted.core);
    let _runtime = Box::leak(Box::new(runtime));
    let _kernel = booted;

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            ipc::app_update_check,
            ipc::app_update_state,
            ipc::app_update_install,
            ipc::shell_settings_get,
            ipc::shell_settings_set,
            ipc::shell_login_item,
            ipc::app_quit,
        ])
        // A menu bar of File/Edit/View is the default, and it is not what this
        // window is: it has one screen for each agent and nothing to open. macOS
        // still needs an application menu for About and Quit.
        .menu(|handle| {
            use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
            // Closing is hiding here, so it gets a real menu item: the platform's
            // own ⌘W has nothing to close once the window's close button is
            // intercepted, and a shortcut that silently does nothing is worse
            // than no shortcut.
            let close = MenuItemBuilder::with_id("close", "Close Window")
                .accelerator("CmdOrCtrl+W")
                .build(handle)?;
            let app = SubmenuBuilder::new(handle, "AGI Mizar")
                .about(None)
                .separator()
                .item(&close)
                .separator()
                .quit()
                .build()?;
            // The window is a web view, so ⌘C/⌘V are the Edit menu's items,
            // not the platform's: without this submenu paste into the login
            // code field does nothing at all.
            let edit = SubmenuBuilder::new(handle, "Edit")
                .undo()
                .redo()
                .separator()
                .cut()
                .copy()
                .paste()
                .select_all()
                .build()?;
            MenuBuilder::new(handle).items(&[&app, &edit]).build()
        })
        .on_menu_event(|app, event| {
            use tauri::Manager;
            if event.id().as_ref() == "close" {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
        })
        .setup(move |app| {
            let parsed = url.parse().expect("the kernel URL is not a URL");
            let mut builder =
                tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(parsed))
                    .title("AGI Mizar")
                    .inner_size(1100.0, 720.0)
                    .min_inner_size(720.0, 480.0)
                    .visible(!background);

            // The window draws its own chrome: the page reserves a strip for the
            // traffic lights, and everything below it is the app's. Windows has no
            // such overlay, so it keeps the system title bar.
            #[cfg(target_os = "macos")]
            {
                builder = builder
                    .title_bar_style(tauri::TitleBarStyle::Overlay)
                    .hidden_title(true)
                    .traffic_light_position(tauri::LogicalPosition::new(18.0, 20.0));
            }

            let window = builder.build()?;

            // The three process switches, as stored. Applied after the window
            // exists so the Dock icon is present for the first frame and then
            // removed, which is less jarring than a window with no Dock icon
            // that appears late.
            #[cfg(target_os = "macos")]
            {
                let settings = shell::current();
                if let Err(error) = shell::apply(app.handle(), &settings) {
                    tracing::warn!("shell settings: {error}");
                }
            }

            // The window is the product; the status item is what keeps it
            // reachable after it is closed.
            #[cfg(target_os = "macos")]
            tray::install(app.handle(), &window)?;
            updates::watch(app.handle().clone());

            // macOS draws the bundle's one icon; the window's theme (which can
            // differ from the system's) picks which of the two this is.
            #[cfg(target_os = "macos")]
            {
                dock_icon::follow(&core.theme());
                let handle = app.handle().clone();
                let core = std::sync::Arc::clone(&core);
                tauri::async_runtime::spawn(async move {
                    loop {
                        let theme = core.theme();
                        let handle = handle.clone();
                        let _ = handle.run_on_main_thread(move || dock_icon::follow(&theme));
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("the window failed to start")
        .run(move |_app, event| {
            // `kill_on_drop` does not run when the process exits, so a quit has
            // to stop the model runner explicitly or it stays resident with a
            // gigabyte of weights loaded.
            if let tauri::RunEvent::ExitRequested { .. } = event {
                // The quit animation is drawn from the icon the app last set.
                // Repaint it for the theme we actually have before the process
                // goes away, so leaving does not flash the other tile.
                #[cfg(target_os = "macos")]
                dock_icon::follow(&core_for_exit.theme());
                tauri::async_runtime::block_on(async {
                    core_for_exit.models().shutdown().await;
                });
            }
        });
}
