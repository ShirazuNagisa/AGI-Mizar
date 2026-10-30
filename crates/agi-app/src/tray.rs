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

use tauri::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuBuilder, MenuItem, MenuItemBuilder, SubmenuBuilder,
};
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
    parts.push(format!(
        "{}{}{}",
        settings.launch_at_login, settings.show_dock_icon, settings.tray_menu_only
    ));
    parts.join("|")
}

fn build(app: &AppHandle, summary: Option<&Summary>) -> tauri::Result<Menu<Wry>> {
    let title = match summary {
        Some(summary) => format!("AGI Mizar {}", summary.version),
        None => "AGI Mizar".to_string(),
    };
    let header = MenuItemBuilder::with_id("header", title)
        .enabled(false)
        .build(app)?;

    // The two answers, above the controls: what is connected to this computer.
    let agents_label = MenuItemBuilder::with_id("agents", "Agent 状态 · Agents")
        .enabled(false)
        .build(app)?;
    let mut agent_items: Vec<MenuItem<Wry>> = Vec::new();
    match summary {
        Some(summary) if !summary.agents.is_empty() => {
            for (engine, connected, available) in &summary.agents {
                let status = if *connected {
                    "已连接 · Connected"
                } else if !*available {
                    "本机未安装 · Not installed"
                } else {
                    "未连接 · Not connected"
                };
                agent_items.push(
                    MenuItemBuilder::with_id(
                        format!("agent:{engine}"),
                        format!("{} — {status}", agent_name(engine)),
                    )
                    .enabled(false)
                    .build(app)?,
                );
            }
        }
        _ => agent_items.push(
            MenuItemBuilder::with_id("agent:none", "读取中… · Reading…")
                .enabled(false)
                .build(app)?,
        ),
    }

    let phones_label = MenuItemBuilder::with_id("phones", "手机 · Phones")
        .enabled(false)
        .build(app)?;
    let mut phone_items: Vec<MenuItem<Wry>> = Vec::new();
    match summary {
        Some(summary) if !summary.phones.is_empty() => {
            let rows = phone_rows(&summary.phones);
            for (name, online) in &rows.shown {
                let status = if *online {
                    "在线 · Online"
                } else {
                    "离线 · Offline"
                };
                phone_items.push(
                    MenuItemBuilder::with_id(format!("phone:{name}"), format!("{name} — {status}"))
                        .enabled(false)
                        .build(app)?,
                );
            }
            if rows.hidden > 0 {
                phone_items.push(
                    MenuItemBuilder::with_id(
                        "phone:more",
                        format!(
                            "另有 {} 台已配对 · {} more paired",
                            rows.hidden, rows.hidden
                        ),
                    )
                    .enabled(false)
                    .build(app)?,
                );
            }
        }
        _ => phone_items.push(
            MenuItemBuilder::with_id("phone:none", "尚无已配对设备 · No phones paired")
                .enabled(false)
                .build(app)?,
        ),
    }

    let show = MenuItemBuilder::with_id("show", "显示窗口 · Show Window").build(app)?;
    let settings = shell::load();
    let at_login = CheckMenuItem::with_id(
        app,
        "login",
        "开机启动 · Start at Login",
        true,
        login_item::is_enabled(),
        None::<&str>,
    )?;
    let dock = CheckMenuItem::with_id(
        app,
        "dock",
        "显示程序坞图标 · Show Dock Icon",
        true,
        settings.show_dock_icon,
        None::<&str>,
    )?;
    let menu_only = CheckMenuItem::with_id(
        app,
        "menu_only",
        "左键打开菜单 · Menu on Left Click",
        true,
        settings.tray_menu_only,
        None::<&str>,
    )?;

    // The window's theme belongs to the kernel, and the menu has no window to
    // ask. The kernel's address and token are remembered at launch for exactly
    // this kind of call.
    let system = MenuItemBuilder::with_id("theme:system", "跟随系统 · System").build(app)?;
    let light = MenuItemBuilder::with_id("theme:light", "浅色 · Light").build(app)?;
    let dark = MenuItemBuilder::with_id("theme:dark", "深色 · Dark").build(app)?;
    let theme = SubmenuBuilder::new(app, "主题 · Theme")
        .items(&[&system, &light, &dark])
        .build()?;

    let update = MenuItemBuilder::with_id("update", "检查更新 · Check for Updates…").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出 AGI Mizar · Quit").build(app)?;

    let mut items: Vec<&dyn IsMenuItem<Wry>> = vec![&header, &agents_label];
    for item in &agent_items {
        items.push(item);
    }
    items.push(&phones_label);
    for item in &phone_items {
        items.push(item);
    }
    items.push(&show);
    items.push(&at_login);
    items.push(&dock);
    items.push(&menu_only);
    items.push(&theme);
    items.push(&update);
    items.push(&quit);

    MenuBuilder::new(app).items(&items).build()
}

/// The rows the menu shows for the roster.
struct PhoneRows {
    shown: Vec<(String, bool)>,
    hidden: usize,
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
    PhoneRows {
        hidden: phones.len().saturating_sub(shown.len()),
        shown,
    }
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
        assert_eq!(rows.hidden, 0);
    }

    #[test]
    fn one_row_per_name_and_a_count_for_the_rest() {
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
        // Eleven entries, three names shown: the count is what is left over.
        assert_eq!(rows.hidden, phones.len() - rows.shown.len());
        assert!(rows.hidden > 0);
    }

    #[test]
    fn an_empty_roster_has_nothing_to_show() {
        let rows = phone_rows(&[]);
        assert!(rows.shown.is_empty());
        assert_eq!(rows.hidden, 0);
    }

    #[test]
    fn agents_keep_their_product_names() {
        assert_eq!(agent_name("claude"), "Claude Code");
        assert_eq!(agent_name("dsh"), "DSH");
        assert_eq!(agent_name("codex"), "Codex");
        assert_eq!(agent_name("other"), "other");
    }
}
