//! The window.
//!
//! A Tauri shell around the same kernel the daemon runs. It does three things:
//! open the window, start the kernel behind it, and stay out of the way.
//!
//! ### Why the window loads the kernel's URL
//!
//! Because the front end talks to the kernel over loopback HTTP, not over Tauri's
//! IPC. Loading `http://127.0.0.1:<port>/?token=…` means the same page runs in a
//! browser during development and in the window afterwards, with no second
//! transport and no second build.
//!
//! The window is built before that URL exists. Its first page is the bundle's
//! placeholder — "starting the kernel" — and [`attach_kernel`] navigates it to the
//! kernel once boot returns. Skipping that step is what made 0.2.0 look broken on
//! machines this was not developed on: a launch that could not finish booting had
//! no window either, so the only symptom anyone could report was "nothing
//! happens".
//!
//! ### Why the kernel still boots from `setup`
//!
//! A second launch is a person asking for the window they already have, not a
//! request for a second computer. The single-instance plugin ends the new process
//! while the Tauri builder is still being constructed — before any of `setup`
//! runs — so booting from here is what keeps that second process from binding a
//! second set of ports and rewriting the endpoint files the hooks and the DSH
//! plugin read. One process, one kernel, one set of files.
//!
//! It runs on its own thread rather than inline: the event loop has to be turning
//! for the window to draw, for the tray to answer, and for a second launch's
//! "show me the window" message to be delivered at all.
//!
//! ### Why the runtime is leaked
//!
//! The kernel's servers live on a Tokio runtime. `tauri::Builder::run` blocks for
//! the life of the window, so a runtime owned by `setup`'s stack would be dropped
//! the moment setup returned — and every server with it. Leaking it is the
//! honest expression of "this lives as long as the process".

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ipc;
mod kernel;
mod login_item;
mod shell;
mod startup;
mod tray;
mod updates;

use std::sync::{Arc, OnceLock};

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

    /// Repaint the Dock icon for `theme` (`system`, `light`, `dark` or `oled`).
    ///
    /// Must run on the main thread; callers that are not already there go through
    /// `AppHandle::run_on_main_thread`.
    pub fn follow(theme: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let dark = match theme {
            // The 1-bit palette is drawn on the dark tile whatever the system is.
            "dark" | "oled" => true,
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

/// The window's fixed width: the heat map's 53 columns plus the sidebar, which
/// is the narrowest the window can be without hiding a week of the year.
const WINDOW_WIDTH: f64 = 1034.0;

/// The tallest the window may get.
///
/// A maximum exists only because the width is fixed through `max_inner_size`,
/// which has no width-only variant; the height itself is a preference. It must
/// be finite: on Windows, tao converts the logical size to physical pixels
/// with a saturating cast to `u32`, so an infinite height becomes `u32::MAX`
/// and then `MINMAXINFO::ptMaxTrackSize.y == -1`. Every native resize drag
/// (the custom 3px edge handles and the DWM shadow's own borders alike) then
/// clamps the window to zero height: the window vanishes and only a restart
/// brings it back. Ten thousand logical pixels never binds on a real monitor
/// and still fits in the `i32` Windows wants.
const WINDOW_MAX_HEIGHT: f64 = 10_000.0;

/// The shortest the window may get: the dashboard and the guide were drawn for
/// this floor, and Windows only enforces it once the invisible frame is added
/// to it (see the Windows-only `compensate_window_frame` below).
const WINDOW_MIN_HEIGHT: f64 = 480.0;

/// The kernel this process booted, for the run loop's shutdown.
///
/// Set once in `setup`, right after the boot; the window owns none of it, and the
/// exit handler needs it after `setup` has returned.
static KERNEL: OnceLock<Arc<agi_core::Core>> = OnceLock::new();

/// What `--start-at-login` or `--no-start-at-login` asked for.
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

/// The tunnel token, kept between launches.
///
/// The relay derives the room from this token, so a fresh one every launch moves
/// the computer to a new room and every address a phone stored last time is
/// dead — the phone has to go back through the account to find it again. A token
/// on disk keeps the room, and the phone's record, valid across restarts.
fn stable_relay_token() -> String {
    let dir = std::env::var("MIZAR_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| agi_platform::data_dir());
    let path = dir.join("relay-token");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let token = text.trim();
        if token.len() >= 32 {
            return token.to_string();
        }
    }
    let token = agi_boot::random_token();
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = write_private(&path, &token);
    }
    token
}

/// Write a file only this user can read, without a window where it is wider.
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(text.as_bytes())?;
    }
    std::fs::rename(&temporary, path)
}

/// Restate the size limits in the terms Windows applies them in.
///
/// tao builds `WM_GETMINMAXINFO` from the *inner* size it was given and does not
/// add the frame back for a borderless window — but Windows applies the result
/// to the *outer* window. Left alone, an east-edge drag shrinks the page by the
/// invisible frame (about 15 logical pixels at 150%) and a south-edge drag stops
/// at 707 instead of the 480 floor. Measure the frame once the window exists and
/// add it to both limits, so the page keeps its exact width and its floor.
#[cfg(windows)]
fn compensate_window_frame(window: &tauri::WebviewWindow) {
    let (Ok(outer), Ok(inner), Ok(scale)) = (
        window.outer_size(),
        window.inner_size(),
        window.scale_factor(),
    ) else {
        return;
    };
    if !scale.is_finite() || scale <= 0.0 {
        return;
    }
    let inset_width = outer.width.saturating_sub(inner.width) as f64 / scale;
    let inset_height = outer.height.saturating_sub(inner.height) as f64 / scale;
    if inset_width <= 0.0 && inset_height <= 0.0 {
        return;
    }
    let _ = window.set_min_size(Some(tauri::LogicalSize::new(
        WINDOW_WIDTH + inset_width,
        WINDOW_MIN_HEIGHT + inset_height,
    )));
    let _ = window.set_max_size(Some(tauri::LogicalSize::new(
        WINDOW_WIDTH + inset_width,
        WINDOW_MAX_HEIGHT + inset_height,
    )));
}

/// Build the one window this app has, pointed at the bundle's starting page.
///
/// Until 0.2.0 the window was built after the kernel, which meant a launch that
/// could not finish booting had nothing on screen and no way to say so. The page
/// this one opens is the placeholder in `placeholder/` — a few lines that say the
/// kernel is starting — and [`attach_kernel`] replaces it with the real one.
fn build_main_window(app: &tauri::App, background: bool) -> tauri::Result<tauri::WebviewWindow> {
    // 802 logical pixels is the height the dashboard and the guide were drawn
    // for, but it is a preference: a 1080p laptop at 150% scaling has about 672
    // logical pixels of work area, and a first screen whose footer is off the
    // bottom is broken before it is used. Ask the monitor, keep 80 logical
    // pixels of breathing room, and never go below the 480 floor or above the
    // design height.
    let height = app
        .handle()
        .primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| {
            let area = monitor.work_area();
            let logical = area.size.height as f64 / monitor.scale_factor();
            (logical - 80.0).clamp(WINDOW_MIN_HEIGHT, 802.0)
        })
        .unwrap_or(802.0);

    let builder =
        tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
            .title("AGI Mizar")
            .inner_size(WINDOW_WIDTH, height)
            // The dashboard's year of squares is the widest thing the window ever
            // draws; at any narrower size the last weeks are cut off. The width is
            // therefore not a preference — the height still is, and keeps its own floor.
            .min_inner_size(WINDOW_WIDTH, WINDOW_MIN_HEIGHT)
            .max_inner_size(WINDOW_WIDTH, WINDOW_MAX_HEIGHT)
            .visible(!background);

    // The window draws its own chrome: on macOS the page reserves a strip for
    // the traffic lights and the system overlays them; on Windows the page draws
    // the same strip and its own traffic lights, because the system title bar is
    // removed below. Linux keeps the system title bar (and its own window menu).
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(18.0, 20.0));

    // Borderless on Windows: no caption, but `shadow(true)` gives the DWM drop
    // shadow and frame insets back, and tao still resizes the window from its
    // edges and keeps a maximized window off the taskbar (WM_NCCALCSIZE uses the
    // monitor work area).
    #[cfg(windows)]
    let builder = builder.decorations(false).shadow(true);

    let window = builder.build()?;

    // Windows reads the constraints off the outer window, tao wrote them from
    // the inner one: add the frame back before the person can grab an edge.
    #[cfg(windows)]
    compensate_window_frame(&window);

    Ok(window)
}

/// Point the window at the kernel, and add the tray page that needs its URL.
///
/// Runs on the main thread once the kernel is up. The placeholder is replaced by
/// the kernel's own page, which is also the thing that draws the window's chrome
/// — so until this call the window has a title bar's worth of nothing and must
/// not be presented as finished.
fn attach_kernel(
    app: &tauri::AppHandle,
    kernel_url: &str,
    core: &Arc<agi_core::Core>,
) -> Result<(), String> {
    use tauri::Manager;

    // The platform rides in the URL so the page can lay itself out for this
    // machine on its very first paint, before the snapshot (which also carries
    // it) has been fetched.
    let url = format!("{kernel_url}&platform={}", agi_platform::platform());

    let window = app
        .get_webview_window("main")
        .ok_or("the main window is gone before the kernel answered")?;
    let parsed: tauri::Url = url.parse().map_err(|error| format!("{error}"))?;
    window.navigate(parsed).map_err(|error| error.to_string())?;
    startup::note("window: kernel page loaded");

    // The tray page: a borderless window that lives under the tray icon.
    // A native menu cannot put an icon left of a label, pin a lamp to the
    // right, or hug the text's width — all three are the spec.
    let panel_url = format!("{url}&panel=tray");
    let panel_parsed: tauri::Url = panel_url.parse().map_err(|error| format!("{error}"))?;
    let panel = tauri::WebviewWindowBuilder::new(
        app,
        "tray-panel",
        tauri::WebviewUrl::External(panel_parsed),
    )
    .title("AGI Mizar")
    .inner_size(tray::PANEL_WIDTH, 300.0)
    .decorations(false)
    .resizable(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(false)
    .build()
    .map_err(|error| error.to_string())?;
    panel.hide().map_err(|error| error.to_string())?;
    // Looking at the tray page without a click is the only way to check
    // it on a machine that cannot script the tray.
    if std::env::var("MIZAR_SHOW_PANEL").is_ok() {
        let _ = panel.show();
    }

    // macOS draws the bundle's one icon; the window's theme (which can
    // differ from the system's) picks which of the two this is.
    #[cfg(target_os = "macos")]
    {
        dock_icon::follow(&core.theme());
        let handle = app.clone();
        let core = Arc::clone(core);
        tauri::async_runtime::spawn(async move {
            loop {
                let theme = core.theme();
                let handle = handle.clone();
                let _ = handle.run_on_main_thread(move || dock_icon::follow(&theme));
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = core;

    Ok(())
}

fn main() {
    // Before anything that can fail, and before anything that can panic: the
    // banner is the first line of every report from a machine we cannot see.
    startup::init();

    // Start at login is also a thing an operator scripts: an MDM image, a
    // headless setup, a test. The tray checkbox and these flags are the same
    // call, so there is only one answer to what "enabled" means. It runs before
    // the window exists on every platform, and before the single-instance check:
    // an administrator scripting this should not need the app to be running.
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

    let builder = tauri::Builder::default()
        // First, because it is the one plugin that must decide whether this
        // process is the app at all: a second launch hands its arguments to the
        // running window and exits before any of the work below happens.
        .plugin(tauri_plugin_single_instance::init(
            |app, arguments, _directory| {
                // A manual launch is a person asking for the window. The login
                // item also starts the app with `--background`, and that one
                // must not put a window on screen if a copy is already running.
                if !arguments.iter().any(|argument| argument == "--background") {
                    tray::show_window(app);
                }
            },
        ))
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
            ipc::app_export_text,
            ipc::tray_panel_resize,
            ipc::tray_panel_open_settings,
        ])
        .setup(move |app| {
            // Windows cannot draw a window without WebView2, and this build has
            // no console to say so: ask before the builder returns an error
            // nobody can read.
            if let Err(message) = startup::require_webview2() {
                startup::fail("AGI Mizar 无法启动", &message);
                std::process::exit(1);
            }

            let background = std::env::args().any(|argument| argument == "--background");

            // The window first, the kernel second.
            //
            // It used to be the other way round, and every delay below the
            // window — a first disk scan, a blocked proxy, an engine that never
            // answers — looked identical from the outside: an icon that does
            // nothing, and no way to tell why. A window that says "starting"
            // costs a frame and turns that into a sentence. It is also what
            // makes a second launch land: the single-instance plugin can only
            // raise a window that exists, so a click during boot used to be
            // swallowed by a process that had nothing to raise.
            let window = match build_main_window(app, background) {
                Ok(window) => window,
                Err(error) => {
                    startup::fail(
                        "AGI Mizar 无法打开窗口",
                        &format!("窗口创建失败 / the window could not be created: {error}"),
                    );
                    std::process::exit(1);
                }
            };
            startup::note("window: created");

            // A tray icon that failed is a degraded app, not a dead one: the
            // window is the product, and it is already on screen. Taking the
            // launch down over it is what turned one broken shell API into
            // "the app does not start".
            if let Err(error) = tray::install(app.handle(), &window) {
                startup::note(&format!("tray: install failed — {error}"));
            }
            updates::watch(app.handle().clone());

            // The three process switches, as stored. Applied after the window
            // exists so the Dock icon is present for the first frame and then
            // removed, which is less jarring than a window with no Dock icon
            // that appears late.
            let settings = shell::current();
            if let Err(error) = shell::apply(app.handle(), &settings) {
                tracing::warn!("shell settings: {error}");
            }

            // Read the stored server choice before boot, so the tunnel target is
            // decided from the preference file rather than from a constant in
            // this binary. Boot reads the same file again; passing the value
            // makes that agreement explicit and keeps a hand-edited file from
            // being ignored until the window has drawn once.
            let server =
                agi_core::read_preferences(&agi_core::endpoint::default_preferences_path())
                    .map(|preferences| preferences.server)
                    .unwrap_or_default();
            // Official mode keeps the ladder this build has always had:
            // `AGI_RELAY_URL` first (a local relay, a test), then the unified
            // account origin. Self-hosted mode leaves `relay_url` to boot, which
            // dials the stored origin; boot still lets `AGI_RELAY_URL` win for a
            // development relay.
            let self_hosted = server.is_self_hosted();
            let relay_url = if self_hosted {
                None
            } else {
                Some(
                    std::env::var("AGI_RELAY_URL")
                        .ok()
                        .filter(|url| !url.trim().is_empty())
                        .unwrap_or_else(|| agi_core::account::OFFICIAL_ACCOUNT_URL.to_string()),
                )
            };
            let config = BootConfig {
                endpoint_file: None,
                // The hooks are how an approval reaches this window at all:
                // without them Claude Code runs tools unobserved. The installer
                // is additive, backed up and reversible, so the app installs
                // them by default and an operator who wants to manage the file
                // themselves passes `--no-install-claude-hooks`. Installed by the
                // guide, where the person can see it happen — a launch that
                // silently rewrites ~/.claude/settings.json is a surprise, not a
                // feature; `--install-claude-hooks` is the escape hatch for a
                // scripted setup.
                install_claude_hooks: std::env::args()
                    .any(|argument| argument == "--install-claude-hooks"),
                relay_url,
                relay_token: Some(
                    std::env::var("AGI_RELAY_TOKEN")
                        .ok()
                        .filter(|token| !token.trim().is_empty())
                        .unwrap_or_else(stable_relay_token),
                ),
                server: Some(server),
            };

            // The kernel's servers live on a Tokio runtime. The event loop below
            // blocks for the life of the window, so a runtime owned by this
            // closure's stack would be dropped the moment it returned — and
            // every server with it. Leaking it is the honest expression of
            // "this lives as long as the process".
            let runtime: &'static tokio::runtime::Runtime = Box::leak(Box::new(runtime));

            startup::watch_boot(20_000);
            startup::note("boot: starting the kernel");

            // The kernel does not get to decide whether a window exists. It runs
            // on its own thread; the event loop keeps drawing, keeps the tray
            // answering and keeps a second launch able to raise the window.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let booted = match runtime.block_on(boot(config)) {
                    Ok(booted) => booted,
                    Err(error) => {
                        startup::fail(
                            "AGI Mizar 的内核无法启动",
                            &format!(
                                "内核启动失败 / the kernel failed to start:\n{error:#}\n\n\
                                 端口被占用、目录不可写、代理或防火墙拒绝连接都可能造成这一行。\n\
                                 把这份日志发给开发者即可定位。"
                            ),
                        );
                        handle.exit(1);
                        return;
                    }
                };
                startup::note(&format!(
                    "boot: kernel up in {} ms at port {}",
                    started.elapsed().as_millis(),
                    booted.api_port
                ));
                startup::note(&format!(
                    "boot: pairing code {} ({})",
                    booted.pairing_code.display, booted.pairing_uri
                ));
                startup::boot_finished();

                kernel::remember(booted.api_port, booted.api_token.clone());
                let core = Arc::clone(&booted.core);
                let url = booted.window_url();
                let _ = KERNEL.set(Arc::clone(&core));
                // The kernel lives as long as the process: dropping it when this
                // thread returns would take the servers down with it.
                Box::leak(Box::new(booted));

                let app = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    if let Err(error) = attach_kernel(&app, &url, &core) {
                        startup::fail(
                            "AGI Mizar 无法打开界面",
                            &format!(
                                "内核已启动，但窗口无法载入界面 / \
                                 the kernel is up but the window could not load it: {error}"
                            ),
                        );
                    }
                });
            });

            Ok(())
        });

    // A menu bar of File/Edit/View is the default on macOS, and it is not what
    // this window is: it has one screen for each agent and nothing to open.
    // macOS still needs an application menu for About and Quit — and for the
    // Edit items a web view does not provide by itself. Windows has no app menu;
    // its title bar is the system's, so there is nothing to build there.
    #[cfg(target_os = "macos")]
    let builder = builder
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
        });

    let app = match builder.build(tauri::generate_context!()) {
        Ok(app) => app,
        Err(error) => {
            // Not `expect`: a GUI build unwinds into a console nobody has, and
            // the person is left with an icon that did nothing — the exact
            // report this whole module exists to replace.
            startup::fail(
                "AGI Mizar 无法启动",
                &format!("应用初始化失败 / the app could not start: {error}"),
            );
            std::process::exit(1);
        }
    };

    app.run(move |_app, event| {
        // `kill_on_drop` does not run when the process exits, so a quit has
        // to stop the model runner explicitly or it stays resident with a
        // gigabyte of weights loaded.
        if let tauri::RunEvent::ExitRequested { .. } = event {
            // The quit animation is drawn from the icon the app last set.
            // Repaint it for the theme we actually have before the process
            // goes away, so leaving does not flash the other tile.
            #[cfg(target_os = "macos")]
            {
                if let Some(core) = KERNEL.get() {
                    dock_icon::follow(&core.theme());
                }
            }
            tauri::async_runtime::block_on(async {
                if let Some(core) = KERNEL.get() {
                    core.models().shutdown().await;
                }
            });
            // The endpoint files name the port other processes should dial.
            // Leaving them behind points the Claude hooks and the DSH plugin
            // at a port this process no longer listens on.
            agi_core::endpoint::remove_endpoint_file(&agi_core::endpoint::default_endpoint_path());
            agi_core::endpoint::remove_hook_file(&agi_core::endpoint::default_hook_path());
            agi_core::endpoint::remove_tool_file(&agi_core::endpoint::default_tool_path());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{WINDOW_MAX_HEIGHT, WINDOW_MIN_HEIGHT, WINDOW_WIDTH};

    /// Windows turns the limits into `MINMAXINFO` track sizes in `i32` pixels,
    /// after multiplying by the monitor scale. An infinite cap saturates to
    /// `u32::MAX` on the way in and reads back as `-1`, and every resize drag
    /// then clamps the window to no height at all — the window vanishes until
    /// the app is restarted.
    #[test]
    fn the_height_limits_survive_the_trip_to_a_windows_track_size() {
        assert!(WINDOW_WIDTH.is_finite() && WINDOW_WIDTH > 0.0);
        assert!(WINDOW_MIN_HEIGHT.is_finite() && WINDOW_MIN_HEIGHT > 0.0);
        assert!(WINDOW_MAX_HEIGHT.is_finite() && WINDOW_MAX_HEIGHT > WINDOW_MIN_HEIGHT);
        // Windows caps display scaling at 500%; leave room for future ones.
        let tallest = WINDOW_MAX_HEIGHT * 8.0;
        assert!(
            tallest < f64::from(i32::MAX),
            "{WINDOW_MAX_HEIGHT} logical pixels * 8 = {tallest} does not fit in an i32"
        );
    }
}
