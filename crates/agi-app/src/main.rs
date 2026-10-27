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

use agi_boot::{boot, BootConfig};

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("could not start the kernel runtime");

    let booted = runtime
        .block_on(boot(BootConfig {
            endpoint_file: None,
            // Installing hooks edits a file the operator owns; a first launch
            // asks, it does not decide. The daemon's flag is the way in for now.
            install_claude_hooks: std::env::args()
                .any(|argument| argument == "--install-claude-hooks"),
            relay_url: std::env::var("AGI_RELAY_URL").ok(),
            relay_token: std::env::var("AGI_RELAY_TOKEN").ok(),
        }))
        .expect("the kernel failed to start");

    let url = booted.window_url();
    println!("AGI Mizar — window at {url}");
    println!(
        "  pairing code {} ({})",
        booted.pairing_code.display, booted.pairing_uri
    );

    // Both must outlive the window: the runtime keeps the servers alive, and
    // `booted` owns the kernel the servers read from.
    let _runtime = Box::leak(Box::new(runtime));
    let _kernel = booted;

    tauri::Builder::default()
        // A menu bar of File/Edit/View is the default, and it is not what this
        // window is: it has one screen for each agent and nothing to open. macOS
        // still needs an application menu for About and Quit.
        .menu(|handle| {
            use tauri::menu::{MenuBuilder, SubmenuBuilder};
            let app = SubmenuBuilder::new(handle, "AGI Mizar")
                .about(None)
                .separator()
                .quit()
                .build()?;
            MenuBuilder::new(handle).items(&[&app]).build()
        })
        .setup(move |app| {
            let parsed = url.parse().expect("the kernel URL is not a URL");
            let mut builder =
                tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(parsed))
                    .title("AGI Mizar")
                    .inner_size(1100.0, 720.0)
                    .min_inner_size(720.0, 480.0);

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

            builder.build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("the window failed to start");
}
