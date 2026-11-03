//! Start at login, the way each desktop means it.
//!
//! macOS registers a LaunchAgent in `~/Library/LaunchAgents`: the app bundle's
//! executable is named, `RunAtLoad` starts it, and nothing else on the system is
//! edited. Windows writes one value under the current user's `Run` key, which
//! starts the app at sign-in with no elevation and no service.
//!
//! It is deliberately opt-in — an app that adds itself to login items without
//! being asked is malware-shaped, and the tray menu is the ask.
//!
//! Only an installed app can register. A debug binary running out of `target/`
//! would otherwise be registered and then removed by the next build, so the
//! switch refuses there; `AGI_ALLOW_LOGIN_ITEM=1` lifts that for the one machine
//! that tests the switch itself.

#[cfg(target_os = "macos")]
mod platform {
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
        <string>--background</string>
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

    /// The three characters a path can contain that XML cares about.
    fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
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
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;

    /// The value name the login item is filed under. Product name, not binary
    /// name: it is the row a person sees in Task Manager's Startup tab.
    const VALUE: &str = "AGI Mizar";

    /// The per-user key Windows reads at sign-in.
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    /// The command Windows should run at sign-in.
    ///
    /// Quoted, because the install directory is `AGI Mizar` — a space in an
    /// unquoted `Run` value would mean "run `C:\Program` …" — and the flag
    /// matches the macOS agent, so the login launch waits in the tray instead of
    /// stealing focus.
    fn login_command(executable: &Path) -> String {
        format!("\"{}\" --background", executable.display())
    }

    /// The program a `Run` value names, with the quotes this app writes removed.
    fn program_of(command: &str) -> &str {
        let command = command.trim();
        if let Some(rest) = command.strip_prefix('"') {
            rest.split('"').next().unwrap_or_default()
        } else {
            command.split_whitespace().next().unwrap_or_default()
        }
    }

    /// Whether a `Run` value points at this executable.
    ///
    /// Windows paths compare case-insensitively, and the registry hands back
    /// exactly what was written, so a plain ASCII comparison is the honest one.
    fn points_at(command: &str, executable: &Path) -> bool {
        program_of(command).eq_ignore_ascii_case(&executable.display().to_string())
    }

    /// Whether this executable is a build artifact rather than an installed app.
    ///
    /// `cargo build` puts the binary under `target/debug` (or
    /// `target/<triple>/debug`); registering that would make the next build the
    /// app that starts at login, and then delete under it.
    fn is_development_build(executable: &Path) -> bool {
        let profile = executable
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str());
        let is_profile = profile.is_some_and(|profile| {
            profile.eq_ignore_ascii_case("debug") || profile.eq_ignore_ascii_case("release")
        });
        if !is_profile {
            return false;
        }
        executable
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .take(3)
            .any(|ancestor| {
                ancestor
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("target"))
            })
    }

    /// The escape hatch for the machine testing this switch from a debug build.
    fn development_build_allowed() -> bool {
        std::env::var("AGI_ALLOW_LOGIN_ITEM").is_ok_and(|value| value == "1")
    }

    /// Whether the login item is currently installed for this app.
    pub fn is_enabled() -> bool {
        let Ok(executable) = std::env::current_exe() else {
            return false;
        };
        let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(RUN_KEY) else {
            return false;
        };
        let Ok(command) = key.get_value::<String, _>(VALUE) else {
            return false;
        };
        points_at(&command, &executable)
    }

    /// Install or remove the login item.
    pub fn set_enabled(enabled: bool) -> Result<(), String> {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        // Writing is the only elevated-in-name operation here and it is still
        // the current user's own hive: no UAC prompt, no machine-wide change.
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
            .map_err(|error| format!("the Run key: {error}"))?;
        if enabled {
            if is_development_build(&executable) && !development_build_allowed() {
                return Err("only an installed app can start at login".to_string());
            }
            key.set_value(VALUE, &login_command(&executable))
                .map_err(|error| format!("the login item: {error}"))?;
        } else {
            match key.delete_value(VALUE) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("the login item: {error}")),
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_command_quotes_the_executable_and_starts_in_the_tray() {
            let command = login_command(Path::new(
                r"C:\Users\a\AppData\Local\AGI Mizar\agi-mizar.exe",
            ));
            assert_eq!(
                command,
                "\"C:\\Users\\a\\AppData\\Local\\AGI Mizar\\agi-mizar.exe\" --background"
            );
            assert_eq!(
                program_of(&command),
                r"C:\Users\a\AppData\Local\AGI Mizar\agi-mizar.exe"
            );
        }

        #[test]
        fn an_unquoted_value_still_yields_its_program() {
            assert_eq!(
                program_of(r"C:\AGI-Mizar\agi-mizar.exe --background"),
                r"C:\AGI-Mizar\agi-mizar.exe"
            );
        }

        #[test]
        fn a_build_artifact_is_not_an_installed_app() {
            assert!(is_development_build(Path::new(
                r"D:\project\AGI Mizar\target\debug\agi-mizar.exe"
            )));
            assert!(is_development_build(Path::new(
                r"D:\project\AGI Mizar\target\release\agi-mizar.exe"
            )));
            assert!(!is_development_build(Path::new(
                r"C:\Users\a\AppData\Local\AGI Mizar\agi-mizar.exe"
            )));
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    /// Whether the login item is currently installed.
    pub fn is_enabled() -> bool {
        false
    }

    /// Install or remove the login item.
    pub fn set_enabled(_enabled: bool) -> Result<(), String> {
        Err("starting at login is not implemented on this platform".to_string())
    }
}

pub use platform::{is_enabled, set_enabled};
