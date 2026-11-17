//! What the launcher says when it cannot open.
//!
//! A release build is a GUI-subsystem executable: there is no console, `println!`
//! writes to a handle nobody holds, and a panic unwinds into silence. On the
//! machine this is developed on that costs nothing — the developer reads the
//! terminal. On every other machine the only symptom is the one sentence a
//! person can send: "I clicked the icon and nothing happened."
//!
//! So the shell gets the two things the kernel already had: a file to write, and
//! a box to show. Neither is pretty, and both have to work when nothing else
//! does — no window, no webview, no kernel.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The launcher's own log, beside the kernel's files.
///
/// The same directory as `endpoint.json` on purpose: one folder a person can be
/// asked to open, whether the report is about the window, the tunnel or the
/// model.
pub fn log_path() -> PathBuf {
    agi_platform::data_dir().join("launcher.log")
}

/// Past this the file is rotated once. A log that cannot be pasted into a chat
/// is not a log.
const LOG_LIMIT: u64 = 1024 * 1024;

/// The thread whose panics are worth a box: the one a person is looking at.
static MAIN_THREAD: OnceLock<std::thread::ThreadId> = OnceLock::new();

fn stamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

/// Append one line to the launcher log.
///
/// Best effort by design: a machine where `~/.agi-mizar` cannot be written is
/// exactly the machine that needs the box below, and failing to log must not
/// stand in the way of showing it.
pub fn note(text: &str) {
    // A development launch has a terminal; echoing there keeps `cargo run` as
    // useful as it was before the file existed.
    eprintln!("AGI Mizar: {text}");

    let path = log_path();
    if let Ok(metadata) = std::fs::metadata(&path) {
        if metadata.len() > LOG_LIMIT {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{} {}", stamp(), text);
    }
}

/// Record a failure and make sure a person sees it.
pub fn fail(title: &str, body: &str) {
    note(&format!("FATAL {title}: {}", body.replace('\n', " | ")));
    alert(
        title,
        &format!("{body}\n\n日志 / log: {}", log_path().display()),
    );
}

/// A modal box that does not need a window, a webview or an event loop.
#[cfg(windows)]
pub fn alert(title: &str, body: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };

    fn wide(text: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let body = wide(body);
    let title = wide(title);
    // SAFETY: both strings are NUL-terminated and outlive the call; the box owns
    // no state and returns when the person dismisses it.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

#[cfg(not(windows))]
pub fn alert(title: &str, body: &str) {
    eprintln!("{title}\n{body}");
}

/// Prepare the launcher for a machine that will not tell us what went wrong.
///
/// Called before anything else in `main`: the banner is the first line of every
/// report, and the panic hook has to be installed before the code that can
/// panic.
pub fn init() {
    let _ = MAIN_THREAD.set(std::thread::current().id());

    let arguments: Vec<String> = std::env::args().skip(1).collect();
    note(&format!(
        "launch {} ({}) arguments {:?}",
        env!("CARGO_PKG_VERSION"),
        agi_platform::platform(),
        arguments
    ));
    if cfg!(windows) {
        match webview2_version() {
            Some(version) => note(&format!("webview2 {version}")),
            None => note("webview2 not found"),
        }
    }

    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_else(|| "unknown".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic without a message".to_string());

        let on_main = MAIN_THREAD.get() == Some(&std::thread::current().id());
        note(&format!("panic at {location}: {payload}"));

        // A panic on a worker thread is a bug the kernel can survive; a panic
        // here is the launcher itself and there is nobody left to draw a window.
        if on_main {
            alert(
                "AGI Mizar 启动失败",
                &format!(
                    "启动器遇到内部错误，已停止。\n\nThe launcher stopped on an internal error.\n\n\
                     {payload}\n{location}\n\n日志 / log: {}",
                    log_path().display()
                ),
            );
        }
    }));
}

/// The WebView2 runtime's version, if this machine has one.
///
/// The window is a webview: no runtime, no window, and Tauri reports that as an
/// error from deep inside a window builder — which a GUI build has no way to
/// print. Asking first turns the failure into a sentence.
#[cfg(windows)]
pub fn webview2_version() -> Option<String> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    /// The WebView2 runtime's client id under EdgeUpdate.
    const CLIENT: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

    let candidates = [
        (
            HKEY_CURRENT_USER,
            format!(r"Software\Microsoft\EdgeUpdate\Clients\{CLIENT}"),
        ),
        (
            HKEY_LOCAL_MACHINE,
            format!(r"SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{CLIENT}"),
        ),
        (
            HKEY_LOCAL_MACHINE,
            format!(r"SOFTWARE\Microsoft\EdgeUpdate\Clients\{CLIENT}"),
        ),
    ];

    for (hive, path) in candidates {
        let Ok(key) = RegKey::predef(hive).open_subkey(&path) else {
            continue;
        };
        let Ok(version) = key.get_value::<String, _>("pv") else {
            continue;
        };
        // A machine that uninstalled the runtime leaves the key behind with this
        // version, which means "not installed".
        if !version.is_empty() && version != "0.0.0.0" {
            return Some(version);
        }
    }
    None
}

#[cfg(not(windows))]
pub fn webview2_version() -> Option<String> {
    None
}

/// Refuse to build a window this machine cannot host, in words a person can act
/// on, instead of an error a GUI build cannot show.
#[cfg(windows)]
pub fn require_webview2() -> Result<(), String> {
    if webview2_version().is_some() {
        return Ok(());
    }
    Err(
        "这台电脑缺少 Microsoft Edge WebView2 运行时，AGI Mizar 的窗口无法创建。\n\n\
         请安装 WebView2 运行时后重新打开 AGI Mizar：\n\
         https://developer.microsoft.com/microsoft-edge/webview2/\n\n\
         This computer has no Microsoft Edge WebView2 runtime, so the window \
         cannot be created. Install the runtime, then open AGI Mizar again."
            .to_string(),
    )
}

#[cfg(not(windows))]
pub fn require_webview2() -> Result<(), String> {
    Ok(())
}

/// Note a kernel that is taking longer than anyone expects.
///
/// The window is already on screen by then — this line exists so the next
/// report says *where* the time went instead of "it hung".
pub fn watch_boot(millis: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(millis));
        if !BOOT_DONE.load(std::sync::atomic::Ordering::Acquire) {
            note("boot: still starting after 20s — the window stays on its starting page until the kernel answers");
        }
    });
}

static BOOT_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark the kernel as up, so the watchdog above stays quiet.
pub fn boot_finished() {
    BOOT_DONE.store(true, std::sync::atomic::Ordering::Release);
}
