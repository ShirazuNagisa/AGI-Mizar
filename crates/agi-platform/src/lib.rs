//! The seam between the app and the platform it runs on.
//!
//! Everything in this crate answers a question whose right answer differs on
//! macOS and Windows: where "home" is, what counts as an executable, how a
//! `.cmd` shim has to be launched, which registry key holds the machine model,
//! and how to open a URL in the browser. Callers stay platform-free; when a new
//! platform appears, this is the file that grows a branch.
//!
//! The rules are deliberately explicit rather than clever: a wrong `home()`
//! writes the relay token into the install directory, a wrong `which()` reports
//! a CLI that is installed as "not found", and a wrong `command()` turns every
//! prompt into `os error 193`.

use std::path::{Path, PathBuf};

/// `macos`, `windows` or `linux` — the spelling the window shows and the relay
/// records.
pub fn platform() -> &'static str {
    std::env::consts::OS
}

/// The user's home directory.
///
/// Windows sets `USERPROFILE`, not `HOME`; a process launched from Explorer sees
/// no `HOME` at all. A Finder-launched Mac app sees `HOME`, never
/// `USERPROFILE`. Reading both in platform order is what keeps the endpoint,
/// model, account and shell files in the same place on both.
pub fn home() -> PathBuf {
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        if !profile.is_empty() {
            return PathBuf::from(profile);
        }
    }
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(key) {
            if !value.is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The app's private directory, beside the endpoint and the device roster.
///
/// `MIZAR_DATA_DIR` moves all of it, which is how a test or a portable install
/// keeps the machine's real `~/.agi-mizar` untouched. The daemon and the models
/// directory already honoured it; the endpoint files are the same data and must
/// follow the same switch or a test launch leaves a live token in the real
/// profile.
pub fn data_dir() -> PathBuf {
    if let Some(directory) = std::env::var_os("MIZAR_DATA_DIR") {
        if !directory.is_empty() {
            return PathBuf::from(directory);
        }
    }
    home().join(".agi-mizar")
}

/// The machine's name, as a person would recognise it.
pub fn host_name() -> String {
    #[cfg(windows)]
    if let Some(name) = std::env::var_os("COMPUTERNAME") {
        let name = name.to_string_lossy().trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    let mut command = std::process::Command::new("hostname");
    if let Ok(output) = command.output() {
        if let Ok(text) = String::from_utf8(output.stdout) {
            let text = text.trim().to_string();
            if !text.is_empty() {
                return text;
            }
        }
    }
    "this computer".to_string()
}

/// The model the relay and the phone show for this computer.
///
/// A Mac says "Mac" because that is what its users call it. A Windows PC has no
/// equally short name, so the BIOS product name is read where it exists and
/// "PC" is the honest fallback.
pub fn machine_model() -> String {
    #[cfg(windows)]
    {
        windows_model().unwrap_or_else(|| "PC".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        "Mac".to_string()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        "PC".to_string()
    }
}

#[cfg(windows)]
fn windows_model() -> Option<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"HARDWARE\DESCRIPTION\System\BIOS")
        .ok()?;
    let value: String = key.get_value("SystemProductName").ok()?;
    let value = value.trim().to_string();
    if value.is_empty() || value.eq_ignore_ascii_case("System Product Name") {
        return None;
    }
    Some(value)
}

/// Find an executable the way a shell would.
///
/// On Windows that means trying `PATHEXT` (`.EXE`, `.CMD`, `.BAT`, …) after the
/// bare name: npm installs `claude` as a *shell shim* named `claude.cmd` plus an
/// extension-less `claude` script, and an `is_file()` check on the bare name
/// finds the script, which cannot be spawned. The Unix branch keeps the old
/// behaviour (bare name in every `PATH` directory, plus the user-local prefixes
/// a Finder-launched app does not inherit).
pub fn which(binary: &str) -> Option<PathBuf> {
    if binary.trim().is_empty() {
        return None;
    }
    let candidate = PathBuf::from(binary);
    if candidate.is_absolute() || binary.contains('/') || binary.contains('\\') {
        return resolve_path(&candidate);
    }

    let mut directories: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    extend_search_path(&mut directories);
    for directory in directories {
        if let Some(found) = resolve_in(&directory, binary) {
            return Some(found);
        }
    }
    None
}

/// Directories a GUI-launched process does not inherit from its shell.
fn extend_search_path(directories: &mut Vec<PathBuf>) {
    let home = home();
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            directories.push(PathBuf::from(appdata).join("npm"));
        }
        directories.push(home.join(".local/bin"));
        directories.push(home.join("bin"));
    }
    #[cfg(not(windows))]
    {
        directories.push(home.join(".local/bin"));
        directories.push(home.join(".local/node_modules/.bin"));
        directories.push(home.join(".npm-global/bin"));
        directories.push(home.join("bin"));
    }
}

/// Check a path that the caller already spelled out (an absolute path, or one
/// with a separator in it).
fn resolve_path(candidate: &Path) -> Option<PathBuf> {
    if candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    #[cfg(windows)]
    {
        // `C:\...\claude` may mean `claude.exe` or `claude.cmd`.
        if candidate.extension().is_none() {
            for extension in pathext() {
                let mut path = candidate.as_os_str().to_os_string();
                path.push(&extension);
                let path = PathBuf::from(path);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Look one program up in one directory.
fn resolve_in(directory: &Path, name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let spelled_with_extension = Path::new(name).extension().is_some();
        if spelled_with_extension {
            let path = directory.join(name);
            return path.is_file().then_some(path);
        }
        for extension in pathext() {
            let path = directory.join(format!("{name}{extension}"));
            if path.is_file() {
                return Some(path);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        let path = directory.join(name);
        path.is_file().then_some(path)
    }
}

/// `PATHEXT`, in the order Windows tries it.
#[cfg(windows)]
fn pathext() -> Vec<String> {
    let mut extensions: Vec<String> = std::env::var("PATHEXT")
        .ok()
        .map(|value| {
            value
                .split(';')
                .map(|extension| extension.trim().to_ascii_lowercase())
                .filter(|extension| extension.starts_with('.') && extension.len() > 1)
                .collect()
        })
        .unwrap_or_default();
    if extensions.is_empty() {
        extensions = [".com", ".exe", ".bat", ".cmd"]
            .iter()
            .map(|extension| extension.to_string())
            .collect();
    }
    // npm puts `.cmd` before any extension-less shim; keep the usual order but
    // make sure the launchable kinds are present.
    for required in [".cmd", ".bat", ".exe"] {
        if !extensions.iter().any(|extension| extension == required) {
            extensions.push(required.to_string());
        }
    }
    extensions
}

/// Build a command for an executable found by [`which`].
///
/// A `.cmd`/`.bat` is not a Win32 executable: `CreateProcess` refuses it with
/// `os error 193`, so it has to go through `cmd /C`. The engines hand their
/// arguments (all short tokens; prompts travel on stdin) to the returned
/// command, which is why this returns a preconfigured [`Command`] rather than a
/// boolean.
pub fn command(binary: &Path) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let extension = binary
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if extension == "cmd" || extension == "bat" {
            let mut command = tokio::process::Command::new("cmd");
            command.arg("/C").arg(binary);
            return command;
        }
        if extension == "ps1" {
            let mut command = tokio::process::Command::new("powershell");
            command
                .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(binary);
            return command;
        }
        tokio::process::Command::new(binary)
    }
    #[cfg(not(windows))]
    {
        tokio::process::Command::new(binary)
    }
}

/// Open an `https://` URL in the user's browser.
pub fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
    }
    #[cfg(windows)]
    {
        // `start` treats the first quoted argument as the window title, so the
        // empty title is not decoration: without it a quoted URL would be taken
        // as a title and nothing would open.
        std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(url)
            .spawn()
            .map(|_| ())
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
    }
}

/// Resolve every symbolic link and `..` in a path, without the `\\?\` prefix
/// that `std::fs::canonicalize` adds on Windows.
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(windows)]
    {
        dunce::canonicalize(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
}

/// Whether `candidate` is `root` or lives underneath it.
///
/// Windows paths compare case-insensitively; a session's working directory
/// recorded as `C:\Project` must not become "outside" because the tool said
/// `c:\project`.
pub fn path_is_under(root: &Path, candidate: &Path) -> bool {
    #[cfg(windows)]
    {
        let root = normalize_components(root);
        let candidate = normalize_components(candidate);
        candidate.len() >= root.len()
            && root
                .iter()
                .zip(candidate.iter())
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
    }
    #[cfg(not(windows))]
    {
        candidate.starts_with(root)
    }
}

#[cfg(windows)]
fn normalize_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_never_empty() {
        assert!(!home().as_os_str().is_empty());
    }

    #[test]
    fn platform_is_a_known_spelling() {
        assert!(["macos", "windows", "linux"].contains(&platform()));
    }

    #[test]
    fn machine_model_is_never_blank() {
        assert!(!machine_model().trim().is_empty());
    }

    #[test]
    fn path_is_under_accepts_the_root_itself() {
        let root = home();
        assert!(path_is_under(&root, &root));
    }

    #[test]
    fn a_sibling_prefix_is_not_under_the_root() {
        let root = Path::new("/tmp/agi-mizar");
        assert!(!path_is_under(root, Path::new("/tmp/agi-mizar-other/file")));
    }

    #[test]
    fn canonicalize_keeps_paths_plain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested");
        std::fs::create_dir(&path).unwrap();
        let canonical = canonicalize(&path).unwrap();
        #[cfg(windows)]
        assert!(!canonical.to_string_lossy().starts_with(r"\\?\"));
        #[cfg(not(windows))]
        assert!(canonical.is_absolute());
    }

    /// A `.cmd` is a shell script, not a Win32 image: the wrapper has to route it
    /// through `cmd /C` or every engine dies with `os error 193`.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_cmd_shim_is_launched_through_the_command_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("shim.cmd");
        std::fs::write(&shim, "@echo off\r\necho hello %1\r\n").unwrap();
        let output = command(&shim)
            .arg("world")
            .output()
            .await
            .expect("the shim runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("hello world"), "stdout was {stdout:?}");
    }

    /// `which` must prefer the launchable `.cmd` shim over the extension-less
    /// npm shim, which `CreateProcess` cannot run.
    #[cfg(windows)]
    #[test]
    fn which_finds_the_launchable_shim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("npx-probe"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.path().join("npx-probe.cmd"), "@echo off\r\n").unwrap();
        let path = dir.path().to_path_buf();
        let found = path.join("npx-probe.cmd");
        // Exercise the same resolver the public function uses for one directory.
        assert_eq!(resolve_in(&path, "npx-probe"), Some(found));
    }
}
