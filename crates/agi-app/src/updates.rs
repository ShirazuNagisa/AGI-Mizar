//! Updates from GitHub Releases.
//!
//! The release workflow publishes the bundle, its signature and a `latest.json`
//! that names it; the updater plugin compares the version there with this build
//! and hands back a download. What is left is the part a person has to agree to:
//! nobody wants a window replaced in the middle of a prompt, so the checking is
//! quiet and the installing happens where the version is shown.
//!
//! Not every release carries that `latest.json` — the early ones did not — so a
//! failed check falls back to the releases API, which can name a version and a
//! page but never a payload. The signature check still guards the only path that
//! installs anything: the fallback's answer is a link to open, not a download.
//!
//! The signature is the security boundary, not the HTTPS. The public key is in
//! the bundle; the private key lives in the release runner's secrets, so a
//! compromised download host still cannot hand this app a payload it would take.

use std::sync::RwLock;

use semver::Version;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_updater::{Error, Update, UpdaterExt};

/// How often a long-running window looks again.
const INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// The fallback's source: the newest published release, as the API describes it.
const RELEASES_API: &str = "https://api.github.com/repos/ShirazuNagisa/AGI-Mizar/releases/latest";

/// Where a release page lives; the tag is the version with a `v` in front.
const RELEASES_URL: &str = "https://github.com/ShirazuNagisa/AGI-Mizar/releases/tag/v";

/// A fallback request that has not answered by now will not answer usefully.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The answer to "is there a newer build", as the settings page draws it.
#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateInfo {
    /// The build this window is running.
    pub current: String,
    /// The newest release's version, when the check could read one.
    pub latest: Option<String>,
    /// Whether `latest` is newer than `current`.
    pub available: bool,
    /// Whether this build can download, verify and install `latest` itself.
    /// True only when the signed updater feed announced it.
    pub installable: bool,
    /// The release page to open when the update cannot be installed from here.
    pub release_url: Option<String>,
    /// Why the check could not answer: `feed_missing`, `network`, `unavailable`
    /// or `failed`, so the page can say it in its own language.
    pub error_code: Option<String>,
    /// The updater's own words about a failure; log and tooltip material.
    pub error: Option<String>,
}

/// The last answer, so the page can draw before it asks again.
static LAST: RwLock<Option<UpdateInfo>> = RwLock::new(None);

/// Record one line about the updater, somewhere a person can read it after the
/// window is gone. The app is usually launched by Finder or Explorer, so stderr
/// goes nowhere.
fn note(line: &str) {
    eprintln!("AGI update: {line}");
    let Some(directory) = log_directory() else {
        return;
    };
    let _ = std::fs::create_dir_all(&directory);
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("update.log"))
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Where the updater's log lives: the directory each desktop keeps app logs in.
///
/// macOS has a convention for this — `~/Library/Logs` — and Finder-launched apps
/// are read there. Windows has none for a per-user app, and the obvious
/// `%LOCALAPPDATA%\AGI Mizar` is *the install directory*: a log written there
/// survives as an unknown file when the uninstaller removes the files it knows
/// about, leaving an empty-looking directory behind. So the log lives beside the
/// WebView2 profile in the app's own data directory instead, where it is not part
/// of the installation and is removed with the user's data, not with the program.
fn log_directory() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(std::path::Path::new(&home).join("Library/Logs/AGI Mizar"))
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("LOCALAPPDATA") {
            return Some(
                std::path::Path::new(&appdata)
                    .join("me.shirazu.agimizar")
                    .join("logs"),
            );
        }
        Some(agi_platform::data_dir().join("logs"))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Some(agi_platform::data_dir().join("logs"))
    }
}

/// The last stored answer, without touching the network.
pub fn state(app: &AppHandle) -> UpdateInfo {
    if let Some(info) = LAST
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return info;
    }
    UpdateInfo {
        current: app.package_info().version.to_string(),
        ..Default::default()
    }
}

fn store(info: &UpdateInfo) {
    *LAST
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(info.clone());
}

/// Read a release tag as a version: the `v` is part of the tag, not the version.
///
/// Anything else — `latest`, `0.1`, a date — is not a version this build can
/// order, so it never becomes an offer.
fn parse_version(tag: &str) -> Option<Version> {
    let tag = tag.trim();
    let tag = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag);
    Version::parse(tag).ok()
}

/// Whether `latest` is newer than the running build, by semver precedence.
///
/// Build metadata never orders versions, so `0.1.4+7` is not newer than `0.1.4`.
fn is_newer(latest: &Version, current: &Version) -> bool {
    latest.cmp_precedence(current).is_gt()
}

/// The page for one release: where a build that cannot install itself sends the
/// person instead.
fn release_page(version: &str) -> String {
    format!("{RELEASES_URL}{version}")
}

/// One release, as much of it as the fallback reads.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    html_url: Option<String>,
    /// `/releases/latest` never answers with a draft; the flag is read so an
    /// unexpected one is ignored instead of offered.
    #[serde(default)]
    draft: bool,
}

/// Read the API's answer, letting only a published release through.
fn parse_release(body: &str) -> Option<Release> {
    let release: Release = serde_json::from_str(body).ok()?;
    (!release.draft).then_some(release)
}

/// Ask the GitHub releases API for the newest published release.
///
/// No signature is involved here, so this answer can only name a version and a
/// page. The user agent is what an unauthenticated GitHub request must carry,
/// and it names this build so a log line is traceable.
async fn latest_release(current: &Version) -> Result<Release, String> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(format!("AGI-Mizar/{current}"))
        .build()
        .map_err(|error| format!("the HTTP client could not be built — {error}"))?;
    let response = client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| format!("the releases API did not answer — {error}"))?
        .error_for_status()
        .map_err(|error| format!("the releases API refused the request — {error}"))?;
    let body = response
        .text()
        .await
        .map_err(|error| format!("the releases API could not be read — {error}"))?;
    parse_release(&body).ok_or_else(|| "the releases API did not return a release".to_string())
}

/// The page's stable code for a check that failed.
///
/// `feed_missing` is the common one: the release exists but ships no
/// `latest.json`, which is exactly why the fallback exists. The rest say whether
/// the check never reached GitHub (`network`), this platform has no feed
/// (`unavailable`), or something else broke (`failed`).
fn error_code(error: &Error) -> &'static str {
    match error {
        Error::ReleaseNotFound => "feed_missing",
        Error::Reqwest(_) | Error::Io(_) => "network",
        Error::EmptyEndpoints | Error::UnsupportedOs => "unavailable",
        _ => "failed",
    }
}

/// Ask the signed feed, the only path that can hand this build a payload.
async fn signed_check(app: &AppHandle) -> Result<Option<Update>, Error> {
    app.updater()?.check().await
}

/// Name the newest release from the GitHub API when the signed feed could not.
///
/// `code` and `detail` describe why the feed failed; they stay on the answer when
/// this fallback fails too. Nothing here can install: `installable` is false and
/// `release_url` is what the page offers instead.
async fn fallback(current: &Version, code: &'static str, detail: String) -> UpdateInfo {
    let current_text = current.to_string();
    let release = match latest_release(current).await {
        Ok(release) => release,
        Err(error) => {
            note(&format!("the releases API also failed — {error}"));
            return UpdateInfo {
                current: current_text,
                latest: None,
                available: false,
                installable: false,
                release_url: None,
                error_code: Some(code.to_string()),
                error: Some(detail),
            };
        }
    };

    match parse_version(&release.tag_name) {
        Some(latest) if is_newer(&latest, current) => {
            let version = latest.to_string();
            note(&format!("{version} is available from the releases API"));
            UpdateInfo {
                current: current_text,
                latest: Some(version.clone()),
                available: true,
                installable: false,
                release_url: Some(release.html_url.unwrap_or_else(|| release_page(&version))),
                error_code: None,
                error: None,
            }
        }
        Some(latest) => {
            let version = latest.to_string();
            note(&format!("the newest release is {version}"));
            UpdateInfo {
                current: current_text,
                latest: Some(version),
                available: false,
                installable: false,
                release_url: None,
                error_code: None,
                error: None,
            }
        }
        None => {
            note(&format!(
                "the releases API tag {:?} is not a version",
                release.tag_name
            ));
            UpdateInfo {
                current: current_text,
                latest: None,
                available: false,
                installable: false,
                release_url: None,
                error_code: Some(code.to_string()),
                error: Some(detail),
            }
        }
    }
}

/// Ask whether there is a newer build: the signed feed first, then the releases
/// API when the feed cannot answer.
///
/// Quiet by design: the answer goes to the state the settings page reads and to
/// the log, and nowhere else. A window that interrupts a prompt to say "nothing
/// changed" teaches people to dismiss it without reading.
pub async fn check(app: AppHandle) -> UpdateInfo {
    let current = app.package_info().version.clone();
    let info = match signed_check(&app).await {
        Ok(Some(update)) => {
            note(&format!("{} is available", update.version));
            UpdateInfo {
                current: current.to_string(),
                latest: Some(update.version.clone()),
                available: true,
                installable: true,
                release_url: Some(release_page(&update.version)),
                error_code: None,
                error: None,
            }
        }
        Ok(None) => {
            note("this is the newest version");
            UpdateInfo {
                current: current.to_string(),
                latest: None,
                available: false,
                installable: false,
                release_url: None,
                error_code: None,
                error: None,
            }
        }
        Err(error) => {
            let code = error_code(&error);
            note(&format!("the check failed ({code}) — {error}"));
            fallback(&current, code, error.to_string()).await
        }
    };
    store(&info);
    info
}

/// Check once, then keep checking for as long as this process lives.
pub fn watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // A launch is busy enough without a network round trip in the first second.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        loop {
            check(app.clone()).await;
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// Download, verify and install, then come back up as the new version.
///
/// Only the signed feed can install, so a build it cannot answer for gets the
/// same answer the page draws: no download, and the release page to open.
pub async fn install(app: AppHandle) -> Result<(), String> {
    let current = app.package_info().version.clone();
    let update = match signed_check(&app).await {
        Ok(Some(update)) => update,
        Ok(None) => {
            store(&UpdateInfo {
                current: current.to_string(),
                ..Default::default()
            });
            return Err("this build is already the newest".to_string());
        }
        Err(error) => {
            let code = error_code(&error);
            note(&format!("the update was not installed ({code}) — {error}"));
            store(&fallback(&current, code, error.to_string()).await);
            return Err("this build has no installable update from the signed channel".to_string());
        }
    };
    note(&format!("installing {}", update.version));
    update
        .download_and_install(|_chunk, _total| {}, || {})
        .await
        .map_err(|error| {
            note(&format!("the update was not installed — {error}"));
            format!("{error}")
        })?;
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `UpdateInfo::default()` is what a page draws before the first answer, so
    /// it must parse as "we know nothing yet" rather than "up to date".
    #[test]
    fn update_info_defaults_to_nothing_known() {
        let info = UpdateInfo::default();
        assert!(info.current.is_empty());
        assert!(info.latest.is_none());
        assert!(!info.available);
        assert!(!info.installable);
        assert!(info.release_url.is_none());
        assert!(info.error_code.is_none());
        assert!(info.error.is_none());
    }

    #[test]
    fn update_info_serializes_with_the_page_names() {
        let info = UpdateInfo {
            current: "0.1.3".to_string(),
            latest: Some("0.1.4".to_string()),
            available: true,
            installable: false,
            release_url: Some("https://example.invalid/release".to_string()),
            error_code: Some("feed_missing".to_string()),
            error: Some("detail".to_string()),
        };
        let json = serde_json::to_value(&info).expect("serialisable");
        assert_eq!(json["current"], "0.1.3");
        assert_eq!(json["latest"], "0.1.4");
        assert_eq!(json["available"], true);
        assert_eq!(json["installable"], false);
        assert_eq!(json["release_url"], "https://example.invalid/release");
        assert_eq!(json["error_code"], "feed_missing");
        assert_eq!(json["error"], "detail");
        assert_eq!(json.as_object().map(serde_json::Map::len), Some(7));
    }

    #[test]
    fn release_tags_parse_as_versions() {
        assert_eq!(parse_version("v0.1.3"), Some(Version::new(0, 1, 3)));
        assert_eq!(parse_version("0.1.3"), Some(Version::new(0, 1, 3)));
        assert_eq!(parse_version(" v0.2.0 "), Some(Version::new(0, 2, 0)));
        assert_eq!(
            parse_version("0.1.3-beta.1"),
            Some(Version::parse("0.1.3-beta.1").expect("a prerelease"))
        );
        assert_eq!(
            parse_version("0.1.3+build.7"),
            Some(Version::parse("0.1.3+build.7").expect("a build"))
        );
    }

    #[test]
    fn malformed_tags_are_not_versions() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("v"), None);
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version("0.1"), None);
        assert_eq!(parse_version("0.1.3.4"), None);
        assert_eq!(parse_version("release-0.1.3"), None);
    }

    #[test]
    fn newer_versions_win() {
        let current = Version::new(0, 1, 3);
        assert!(is_newer(&Version::new(0, 1, 4), &current));
        assert!(!is_newer(&Version::new(0, 1, 3), &current));
        assert!(!is_newer(&Version::new(0, 1, 2), &current));
    }

    #[test]
    fn prereleases_rank_below_their_release() {
        let beta = Version::parse("0.1.3-beta.1").expect("a prerelease");
        let stable = Version::new(0, 1, 3);
        assert!(is_newer(&stable, &beta));
        assert!(!is_newer(&beta, &stable));
    }

    #[test]
    fn build_metadata_does_not_order_versions() {
        let current = Version::new(0, 1, 3);
        let same = Version::parse("0.1.3+build.7").expect("a build");
        assert!(!is_newer(&same, &current));
    }

    #[test]
    fn release_pages_are_v_tags() {
        assert_eq!(
            release_page("0.1.3"),
            "https://github.com/ShirazuNagisa/AGI-Mizar/releases/tag/v0.1.3"
        );
    }

    #[test]
    fn release_json_parses() {
        let body = r#"{
            "url": "https://api.github.com/repos/ShirazuNagisa/AGI-Mizar/releases/1",
            "tag_name": "v0.1.4",
            "html_url": "https://github.com/ShirazuNagisa/AGI-Mizar/releases/tag/v0.1.4",
            "draft": false
        }"#;
        let release = parse_release(body).expect("a published release");
        assert_eq!(release.tag_name, "v0.1.4");
        assert_eq!(
            release.html_url.as_deref(),
            Some("https://github.com/ShirazuNagisa/AGI-Mizar/releases/tag/v0.1.4")
        );
        assert_eq!(
            parse_version(&release.tag_name),
            Some(Version::new(0, 1, 4))
        );
    }

    #[test]
    fn drafts_and_malformed_release_json_are_ignored() {
        assert!(parse_release(r#"{"tag_name": "v0.1.4", "draft": true}"#).is_none());
        assert!(parse_release(r#"{"draft": false}"#).is_none());
        assert!(parse_release("not json").is_none());
    }

    #[test]
    fn updater_errors_get_page_codes() {
        assert_eq!(error_code(&Error::ReleaseNotFound), "feed_missing");
        assert_eq!(
            error_code(&Error::Io(std::io::Error::other("offline"))),
            "network"
        );
        assert_eq!(error_code(&Error::EmptyEndpoints), "unavailable");
        assert_eq!(error_code(&Error::UnsupportedOs), "unavailable");
        assert_eq!(error_code(&Error::UnsupportedArch), "failed");
        assert_eq!(error_code(&Error::MissingSignedVersion), "failed");
    }
}
