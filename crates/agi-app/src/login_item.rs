//! Start at login, the way a Mac means it.
//!
//! A LaunchAgent in `~/Library/LaunchAgents` is the supported shape: the app
//! bundle's executable is named, `RunAtLoad` starts it, and nothing else on the
//! system is edited. It is deliberately opt-in — an app that adds itself to login
//! items without being asked is malware-shaped, and the tray menu is the ask.
//!
//! The file is only written for a real `.app` bundle: a debug binary running out
//! of `target/` would otherwise be registered and then removed by the next build.

use std::path::PathBuf;

/// The label the agent is filed under; reverse-DNS, as launchd expects.
const LABEL: &str = "me.shirazu.agi-mizar";

/// Where launchd looks for login items.
fn plist_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")),
    )
}

/// The bundle's executable, if this process is a bundled app.
fn bundle_executable() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    // …/AGI Mizar.app/Contents/MacOS/agi-mizar
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    if contents.parent()?.extension()? != "app" {
        return None;
    }
    Some(executable)
}

/// Whether the login item is currently installed.
pub fn is_enabled() -> bool {
    plist_path().is_some_and(|path| path.exists())
}

/// Install or remove the login item.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let Some(path) = plist_path() else {
        return Err("this account has no home directory".to_string());
    };
    if enabled {
        let executable = bundle_executable()
            .ok_or_else(|| "only a bundled app can start at login".to_string())?;
        let directory = path.parent().ok_or("no LaunchAgents directory")?;
        std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        std::fs::write(&path, plist(&executable)).map_err(|error| error.to_string())?;
        // `bootstrap` is the current verb; an agent that is already loaded is
        // not an error worth reporting.
        let domain = format!("gui/{}", uid());
        let _ = std::process::Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&path)
            .status();
    } else {
        let domain = format!("gui/{}", uid());
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &domain])
            .arg(&path)
            .status();
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

/// The current user's id, for `launchctl`'s gui domain.
fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// The property list that starts the app at login.
fn plist(executable: &std::path::Path) -> String {
    let executable = xml_escape(&executable.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{executable}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_names_this_app_and_starts_it_at_login() {
        let plist = plist(std::path::Path::new(
            "/Applications/AGI Mizar.app/Contents/MacOS/agi-mizar",
        ));
        assert!(plist.contains("<string>me.shirazu.agi-mizar</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("/Applications/AGI Mizar.app/Contents/MacOS/agi-mizar"));
    }

    #[test]
    fn a_path_with_xml_characters_is_escaped() {
        let plist = plist(std::path::Path::new("/tmp/a&b/<x>"));
        assert!(plist.contains("/tmp/a&amp;b/&lt;x&gt;"));
        assert!(!plist.contains("a&b"));
    }
}

/// The three characters a path can contain that XML cares about.
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
