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

use tauri::menu::{IsMenuItem, Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow, Wry};

use crate::kernel::{self, Summary};
use crate::{login_item, shell};

/// The window's id, as the shell builds it.
const WINDOW: &str = "main";

/// How many phones the menu lists before it says "and the rest".
const PHONES_SHOWN: usize = 4;

/// The tray, so a refresh can replace its menu.
static TRAY: Mutex<Option<TrayIcon<Wry>>> = Mutex::new(None);
/// The last menu text, so a rebuild only happens when the text would differ.
static LAST: Mutex<Option<String>> = Mutex::new(None);

/// Install the status item and the close-to-tray behaviour.
pub fn install(app: &AppHandle, window: &WebviewWindow) -> tauri::Result<()> {
    let menu = build(app, None)?;

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
        .menu(&menu)
        // A left click is "show me the window" — unless the operator asked for
        // the menu instead, which is the menu-bar-only way to work. The flag is
        // read now and re-applied whenever it changes.
        .show_menu_on_left_click(shell::tray_menu_only())
        .on_menu_event(move |app, event| on_menu(app, event.id.as_ref()))
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

    *TRAY.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(tray);

    // The agents connect and the phones come and go without the window being
    // touched, so the menu is rebuilt on a slow tick rather than on an event
    // nobody publishes.
    let handle = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let _ = refresh(&handle);
    });

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

    let _ = refresh(app);
    Ok(())
}

/// Rebuild the menu when what it says has changed.
pub fn refresh(app: &AppHandle) -> tauri::Result<()> {
    let settings = shell::load();
    let summary = kernel::summary();
    let signature = signature_of(summary.as_ref(), &settings);
    {
        let last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.as_deref() == Some(signature.as_str()) {
            return Ok(());
        }
    }

    let menu = build(app, summary.as_ref())?;
    let tray = TRAY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(tray) = tray {
        tray.set_menu(Some(menu))?;
        let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *last = Some(signature);
    }
    Ok(())
}

/// Everything the menu draws, flattened, so two builds can be compared.
fn signature_of(summary: Option<&Summary>, settings: &shell::ShellSettings) -> String {
    let mut parts = Vec::new();
    parts.push(match summary {
        Some(summary) => summary.version.clone(),
        None => String::new(),
    });
    if let Some(summary) = summary {
        for (engine, connected, available) in &summary.agents {
            parts.push(format!("{engine}:{connected}:{available}"));
        }
        for (name, online, _) in &summary.phones {
            parts.push(format!("{name}:{online}"));
        }
    }
    // The menu no longer draws the switches themselves; they are here so a
    // behaviour change still rebuilds it (the check marks are gone, the tick
    // still tells the thread whether anything moved).
    parts.push(format!(
        "{}{}{}",
        settings.launch_at_login, settings.show_dock_icon, settings.tray_menu_only
    ));
    parts.join("|")
}

/// One line of the list: a name on the left, its lamp pinned to the right.
///
/// A menu item is left-aligned text, so the lamp is positioned by padding the
/// name to a fixed column count. Names are short and the widest one sets the
/// menu's width, so the dots line up in practice; a name longer than the column
/// pushes its own lamp right rather than wrapping.
fn row(name: &str, on: Option<bool>) -> String {
    let lamp = match on {
        Some(true) => "●",
        Some(false) => "○",
        None => "",
    };
    // Wide characters take two columns; the padding counts display width, not
    // bytes, or a Chinese name would sit further left than an English one.
    let width: usize = name
        .chars()
        .map(|c| if c as u32 > 0x2e7f { 2 } else { 1 })
        .sum();
    let pad = "\u{2007}".repeat(COLUMN.saturating_sub(width).max(1));
    format!("{name}{pad}{lamp}")
}

/// How many columns a row is padded to, dot included.
const COLUMN: usize = 26;

fn build(app: &AppHandle, summary: Option<&Summary>) -> tauri::Result<Menu<Wry>> {
    let mut items: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();

    // Agents: one row each, a filled lamp when the engine is connected and a
    // hollow one when it is installed but silent. An engine that is not
    // installed has no lamp at all.
    match summary {
        Some(summary) if !summary.agents.is_empty() => {
            for (engine, connected, available) in &summary.agents {
                let lamp = if *available { Some(*connected) } else { None };
                items.push(Box::new(
                    MenuItemBuilder::with_id(
                        format!("agent:{engine}"),
                        row(agent_name(engine), lamp),
                    )
                    .enabled(false)
                    .build(app)?,
                ));
            }
        }
        _ => items.push(Box::new(
            MenuItemBuilder::with_id("agent:none", row("读取中… · Reading…", None))
                .enabled(false)
                .build(app)?,
        )),
    }

    items.push(Box::new(PredefinedMenuItem::separator(app)?));

    // The phones on the same account, same shape: a filled lamp for one that is
    // talking to this computer right now.
    match summary {
        Some(summary) if !summary.phones.is_empty() => {
            for (name, online) in phone_rows(&summary.phones).shown {
                items.push(Box::new(
                    MenuItemBuilder::with_id(format!("phone:{name}"), row(&name, Some(online)))
                        .enabled(false)
                        .build(app)?,
                ));
            }
        }
        _ => items.push(Box::new(
            MenuItemBuilder::with_id("phone:none", row("没有同账户的设备 · No devices", None))
                .enabled(false)
                .build(app)?,
        )),
    }

    items.push(Box::new(PredefinedMenuItem::separator(app)?));
    items.push(Box::new(
        MenuItemBuilder::with_id("settings", "设置 · Settings").build(app)?,
    ));
    items.push(Box::new(
        MenuItemBuilder::with_id("quit", "退出 AGI Mizar · Quit").build(app)?,
    ));

    let refs: Vec<&dyn IsMenuItem<Wry>> = items.iter().map(|item| item.as_ref()).collect();
    MenuBuilder::new(app).items(&refs).build()
}

/// The rows the menu shows for the roster.
struct PhoneRows {
    shown: Vec<(String, bool)>,
}

/// What a person can act on: connected first, then most recently seen, one row
/// per name, capped.
///
/// The roster keeps every phone that ever paired, and a test rig can leave
/// dozens behind; listing them all turns a status menu into a log file.
fn phone_rows(phones: &[(String, bool, i64)]) -> PhoneRows {
    let mut ranked: Vec<&(String, bool, i64)> = phones.iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.2.cmp(&left.2)));

    let mut shown: Vec<(String, bool)> = Vec::new();
    for (name, online, _) in ranked {
        if shown.iter().any(|(seen, _)| seen == name) {
            continue;
        }
        if shown.len() == PHONES_SHOWN {
            break;
        }
        shown.push((name.clone(), *online));
    }
    PhoneRows { shown }
}

/// The agent's product name, which is the same word in both languages.
fn agent_name(engine: &str) -> &str {
    match engine {
        "dsh" => "DSH",
        "claude" => "Claude Code",
        "codex" => "Codex",
        other => other,
    }
}

fn on_menu(app: &AppHandle, id: &str) {
    match id {
        "show" => show_window(app),
        "settings" => open_settings(app),
        "update" => crate::updates::spawn_check(app.clone()),
        "quit" => app.exit(0),
        "login" => {
            let enabled = !login_item::is_enabled();
            if let Err(error) = login_item::set_enabled(enabled) {
                eprintln!("AGI Mizar: start at login was not changed — {error}");
            }
        }
        "dock" => {
            let mut next = shell::load();
            next.show_dock_icon = !next.show_dock_icon;
            if let Err(error) = shell::update(app, next) {
                eprintln!("AGI Mizar: the Dock icon was not changed — {error}");
            }
        }
        "menu_only" => {
            let mut next = shell::load();
            next.tray_menu_only = !next.tray_menu_only;
            match shell::update(app, next) {
                Ok(applied) => {
                    if let Some(tray) = TRAY
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone()
                    {
                        let _ = tray.set_show_menu_on_left_click(applied.tray_menu_only);
                    }
                }
                Err(error) => eprintln!("AGI Mizar: the tray behaviour was not changed — {error}"),
            }
        }
        other if other.starts_with("theme:") => {
            kernel::set_theme(other.trim_start_matches("theme:"));
        }
        _ => {}
    }
    // The check marks and the behaviour just changed; rebuild from what is now
    // true rather than from what the item believed.
    let _ = refresh(app);
}

/// Show the window on its settings page.
///
/// The page is React state, not a route, so the tray cannot navigate it from
/// outside; it asks the window instead, and the window decides.
fn open_settings(app: &AppHandle) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(name: &str, online: bool, last_seen: i64) -> (String, bool, i64) {
        (name.to_string(), online, last_seen)
    }

    #[test]
    fn a_connected_phone_outranks_a_recent_one() {
        let rows = phone_rows(&[
            phone("old", false, 900),
            phone("live", true, 100),
            phone("new", false, 1000),
        ]);
        assert_eq!(
            rows.shown,
            vec![
                ("live".to_string(), true),
                ("new".to_string(), false),
                ("old".to_string(), false)
            ]
        );
    }

    #[test]
    fn one_row_per_name_with_the_cap() {
        let phones: Vec<_> = (0..9)
            .map(|index| phone("iPhone", index == 0, index as i64))
            .chain([phone("iPad", false, 1), phone("node-sim", false, 2)])
            .collect();
        let rows = phone_rows(&phones);
        // Three names, so three rows: the cap only bites past four.
        assert_eq!(rows.shown.len(), 3);
        assert_eq!(rows.shown[0], ("iPhone".to_string(), true));
        // The rest by recency: node-sim was seen later than the iPad.
        assert_eq!(rows.shown[1].0, "node-sim");
        assert_eq!(rows.shown[2].0, "iPad");
        // Eleven entries, three names: under the cap, so all three are rows.
        assert_eq!(rows.shown.len(), PHONES_SHOWN.min(3));
    }

    #[test]
    fn an_empty_roster_has_nothing_to_show() {
        let rows = phone_rows(&[]);
        assert!(rows.shown.is_empty());
    }

    #[test]
    fn agents_keep_their_product_names() {
        assert_eq!(agent_name("claude"), "Claude Code");
        assert_eq!(agent_name("dsh"), "DSH");
        assert_eq!(agent_name("codex"), "Codex");
        assert_eq!(agent_name("other"), "other");
    }
}
