//! The computers this machine paired with on a self-hosted relay.
//!
//! A self-hosted relay has no accounts, so there is nobody to broker a peer
//! token: two computers recognise each other the way a phone recognises a
//! desktop. The target shows a single-use code, the caller types it, and the
//! target answers with a device token whose marker says "peer". That token is
//! the whole credential, so it lives in one file, `0600`, and never crosses the
//! API boundary: the window gets a [`RemotePeerView`] without it.
//!
//! ### What the record holds
//!
//! `id` is the device id the *target* issued for this machine's token; it is
//! also the window's id, because a second pairing with the same computer mints
//! a new one. `device_id` is this machine's own id as it was sent in the pairing
//! request — the target copied it into the token marker (`peer:selfhost:<id>:dsh`)
//! and it is what the connection presents as its client id. `room` is the
//! target's relay room, and `origin` is the relay it belongs to, so switching to
//! another server hides these records instead of dialling the wrong one.
//!
//! ### What this module does not do
//!
//! It does not talk to the relay's tunnel, mint tokens, or revoke anything. The
//! target is the only party that can revoke a token, which is why forgetting a
//! peer here says so in the window.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use agi_protocol::api::{RemotePeerView, SelfHostDesktop};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long one directory or pairing request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The longest room id this build will put in a URL.
const ROOM_MAX: usize = 128;

/// One computer this machine may delegate to, on a self-hosted relay.
///
/// The token never appears in the window's JSON, a log line or an error: this
/// type's `Debug` drops it, and [`RemotePeer::view`] is the only shape that
/// leaves the kernel.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePeer {
    /// The host-issued device id for this machine's token; also the window's id.
    pub id: String,
    /// The remote computer's name (`hostName` when it paired).
    pub name: String,
    /// The relay origin the token belongs to, `https://host[:port]`.
    pub origin: String,
    /// The remote computer's room, `/r/<room>`.
    pub room: String,
    /// This machine's own device id, as the target recorded it in the marker.
    pub device_id: String,
    /// The token the target minted. A live credential; never logged.
    pub token: String,
    /// Epoch milliseconds when this record was written.
    pub paired_at: i64,
}

impl std::fmt::Debug for RemotePeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemotePeer")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("room", &self.room)
            .field("device_id", &self.device_id)
            .field("paired_at", &self.paired_at)
            .finish_non_exhaustive()
    }
}

impl RemotePeer {
    /// The shape the window may see. Deliberately omits the token.
    pub fn view(&self) -> RemotePeerView {
        RemotePeerView {
            id: self.id.clone(),
            name: self.name.clone(),
            origin: self.origin.clone(),
            room: self.room.clone(),
            device_id: self.device_id.clone(),
            paired_at: self.paired_at,
        }
    }
}

/// A failure the window can branch on: a code and one sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerError {
    pub code: String,
    pub message: String,
}

impl PeerError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// `~/.agi-mizar/peers.json`.
pub fn default_path() -> PathBuf {
    agi_platform::data_dir().join("peers.json")
}

/// Epoch milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// What a person typed, spelled the way the host compares it.
///
/// The code itself is Crockford base32 shown as `K7QP-3M2X`: the host accepts
/// lower case, dashes and spaces, and maps the confusables (`O`→`0`, `I`/`L`→`1`).
/// This only strips the spaces a person types between groups and upper-cases
/// the rest; the host remains the judge of whether it is a code at all.
pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect()
}

/// Whether a room is safe to put in a URL path.
fn valid_room(room: &str) -> bool {
    !room.is_empty()
        && room.len() <= ROOM_MAX
        && room
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

/// One HTTP client with the timeouts and the no-redirect rule every call here
/// uses.
///
/// A redirect would let the server move a pairing request — and the code in it —
/// to a host the stored origin never named.
fn client() -> Result<reqwest::Client, PeerError> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| PeerError::new("NETWORK", format!("the HTTP client: {error}")))
}

/// The computers online on `origin`, from the relay's own directory.
///
/// `pin` is whatever the operator stored; an empty one is sent as an empty
/// header, and the relay answers 401 when its deployment requires one.
pub async fn desktops(origin: &str, pin: &str) -> Result<Vec<SelfHostDesktop>, PeerError> {
    let base = origin.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(PeerError::new(
            "NO_SERVER",
            "this install has no self-hosted server address",
        ));
    }
    let response = client()?
        .get(format!("{base}/selfhost/v1/desktops"))
        .header("x-mizar-server-pin", pin.trim())
        .send()
        .await
        .map_err(|error| {
            PeerError::new(
                "UNREACHABLE",
                format!("{base}/selfhost/v1/desktops: {error}"),
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 => PeerError::new(
                "PIN_REQUIRED",
                "this server asks for a PIN before it lists its computers",
            ),
            403 => PeerError::new("PIN_INVALID", "the server PIN is not correct"),
            429 => PeerError::new(
                "RATE_LIMITED",
                "too many wrong PINs; wait fifteen minutes and try again",
            ),
            _ => PeerError::new(
                "SERVER_ERROR",
                format!("{base}/selfhost/v1/desktops answered {status}"),
            ),
        });
    }

    #[derive(Deserialize)]
    struct Answer {
        #[serde(default)]
        desktops: Vec<SelfHostDesktop>,
    }
    let text = response.text().await.unwrap_or_default();
    let answer: Answer = serde_json::from_str(&text)
        .map_err(|_| PeerError::new("BAD_ANSWER", "the server's computer list was not readable"))?;
    Ok(answer.desktops)
}

/// Pair with one computer on `origin`, in `room`, using the code it is showing.
///
/// The request is the one a phone sends, plus `peer: true` and this machine's
/// `deviceId`: the target builds the token marker from those two itself, so a
/// caller cannot spell a marker that belongs to somebody else.
pub async fn pair(
    origin: &str,
    room: &str,
    code: &str,
    device_name: &str,
    device_id: &str,
) -> Result<RemotePeer, PeerError> {
    let base = origin.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(PeerError::new(
            "NO_SERVER",
            "this install has no self-hosted server address",
        ));
    }
    let room = room.trim();
    if !valid_room(room) {
        return Err(PeerError::new(
            "MALFORMED_ROOM",
            "that is not a room this server could have listed",
        ));
    }
    let code = normalize_code(code);
    if code.is_empty() {
        return Err(PeerError::new(
            "CODE_REQUIRED",
            "type the code the other computer is showing",
        ));
    }
    let url = format!("{base}/r/{room}/mizar/v1/pair");
    let response = client()?
        .post(&url)
        .json(&serde_json::json!({
            "code": code,
            "deviceName": device_name,
            "deviceId": device_id,
            "peer": true,
            "protocolVersion": 1,
        }))
        .send()
        .await
        .map_err(|error| PeerError::new("UNREACHABLE", format!("{url}: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            404 => PeerError::new(
                "HOST_OFFLINE",
                "that computer is not connected to your server any more",
            ),
            401 => PeerError::new("UNAUTHORIZED", "the server refused the pairing request"),
            403 => PeerError::new("FORBIDDEN", "the server refused the pairing request"),
            429 => PeerError::new(
                "RATE_LIMITED",
                "too many pairing attempts; wait a minute and try again",
            ),
            _ => PeerError::new("PAIR_FAILED", format!("{url} answered {status}")),
        });
    }
    let text = response.text().await.unwrap_or_default();
    let answer: Value = serde_json::from_str(&text)
        .map_err(|_| PeerError::new("BAD_ANSWER", "the host's answer was not readable"))?;

    // The host answers a refusal with HTTP 200 and an error frame: a wrong code
    // is the target's sentence to write, so it is passed through verbatim.
    if answer.get("type").and_then(Value::as_str) == Some("error") {
        let code = answer
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("PAIR_FAILED");
        let message = answer
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the other computer refused the pairing request");
        return Err(PeerError::new(code, message));
    }

    let token = answer
        .get("deviceToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PeerError::new("BAD_ANSWER", "the host returned no device token"))?;
    let id = answer
        .get("deviceId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| PeerError::new("BAD_ANSWER", "the host returned no device id"))?;
    let name = answer
        .get("hostName")
        .and_then(Value::as_str)
        .unwrap_or("Computer");
    Ok(RemotePeer {
        id: id.to_string(),
        name: name.to_string(),
        origin: base.to_string(),
        room: room.to_string(),
        device_id: device_id.to_string(),
        token: token.to_string(),
        paired_at: now_ms(),
    })
}

/// The credentials one stored peer is dialled with.
///
/// A self-hosted relay checks neither the account session nor the account id:
/// the token the target minted is the whole credential, so both are empty (or a
/// constant) and the code path below is the official one with those fields
/// blanked. Kept as a free function so what goes on the wire is testable without
/// a network.
pub fn credentials(peer: &RemotePeer, app_version: &str) -> agi_peer::Credentials {
    agi_peer::Credentials {
        base: peer.origin.clone(),
        room: peer.room.clone(),
        account_session: String::new(),
        device_id: peer.device_id.clone(),
        account_id: "selfhost".to_string(),
        peer_token: peer.token.clone(),
        app_version: app_version.to_string(),
    }
}

/// The peers this machine paired with on `origin`.
///
/// Records for another server stay on disk and are not listed: a machine that
/// switched to a different relay must not delegate through a token that belongs
/// to the old one.
pub fn on_origin(peers: Vec<RemotePeer>, origin: &str) -> Vec<RemotePeer> {
    let origin = origin.trim().trim_end_matches('/');
    peers
        .into_iter()
        .filter(|peer| peer.origin.trim_end_matches('/') == origin)
        .collect()
}

/// The peer file, kept in memory and written atomically.
///
/// A corrupt or missing file reads as "no peers" rather than failing the kernel:
/// a half-written JSON file must not stop the window from opening, and the next
/// pairing overwrites it. The previous file is never renamed away, so a bad
/// write leaves the old token usable.
pub struct PeerStore {
    state: Mutex<State>,
}

struct State {
    path: Option<PathBuf>,
    peers: Vec<RemotePeer>,
}

impl Default for PeerStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerStore {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                path: None,
                peers: Vec::new(),
            }),
        }
    }

    /// Read the file at `path`, remembering where the next write goes.
    pub fn load(&self, path: PathBuf) {
        let peers = read_file(&path);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.peers = peers;
        state.path = Some(path);
    }

    /// Every record, including ones that belong to another origin.
    pub fn list(&self) -> Vec<RemotePeer> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .peers
            .clone()
    }

    /// One record by the host-issued device id.
    pub fn find(&self, id: &str) -> Option<RemotePeer> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .peers
            .iter()
            .find(|peer| peer.id == id)
            .cloned()
    }

    /// Record a pairing, replacing the one it supersedes.
    ///
    /// A re-pairing with the same computer mints a new host-issued id, so the
    /// same `origin`+`room` is treated as the same peer: two rows for one
    /// computer would leave the old token reachable and the list ambiguous.
    pub fn upsert(&self, peer: RemotePeer) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let existing = state.peers.iter().position(|current| {
            current.id == peer.id
                || (current.origin.eq_ignore_ascii_case(&peer.origin) && current.room == peer.room)
        });
        match existing {
            Some(at) => state.peers[at] = peer,
            None => state.peers.push(peer),
        }
        write_state(&state)
    }

    /// Forget one record. Local only; the token stays live on the other computer.
    pub fn remove(&self, id: &str) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = state.peers.len();
        state.peers.retain(|peer| peer.id != id);
        if state.peers.len() == before {
            return false;
        }
        if let Err(error) = write_state(&state) {
            tracing::warn!("peers: could not rewrite the peer file — {error}");
        }
        true
    }
}

/// Read one peer file, tolerating a missing or unreadable one.
fn read_file(path: &Path) -> Vec<RemotePeer> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    if text.trim().is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<RemotePeer>>(&text) {
        Ok(peers) => peers,
        Err(error) => {
            tracing::warn!(
                "peers: {} is not a peer list — {error}; starting empty",
                path.display()
            );
            Vec::new()
        }
    }
}

/// Write the pressed file: private, atomic, and only when there is a path.
fn write_state(state: &State) -> io::Result<()> {
    let Some(path) = state.path.as_deref() else {
        return Ok(());
    };
    write_file(path, &state.peers)
}

/// Write one peer file the way the endpoint file is written.
fn write_file(path: &Path, peers: &[RemotePeer]) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(peers).map_err(io::Error::other)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            // Only the unix branch below cares, so the binding is scoped there:
            // on Windows it would be an unused variable warning.
            #[cfg(unix)]
            let existed = parent.exists();
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            if !existed {
                use std::os::unix::fs::PermissionsExt;
                // A directory this process created holds live tokens; 0700 is
                // the same promise the file itself gets.
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Before the rename, so the file is never briefly world-readable.
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str) -> RemotePeer {
        RemotePeer {
            id: id.to_string(),
            name: "Studio Mac".to_string(),
            origin: "https://relay.example.com".to_string(),
            room: format!("ROOM-{id}"),
            device_id: "this-device".to_string(),
            token: format!("mz_{id}"),
            paired_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn a_pairing_round_trips_through_the_file_with_its_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peers.json");
        let store = PeerStore::new();
        store.load(path.clone());
        assert!(store.list().is_empty());
        store.upsert(peer("p1")).unwrap();
        store.upsert(peer("p2")).unwrap();

        let reopened = PeerStore::new();
        reopened.load(path);
        let peers = reopened.list();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].token, "mz_p1");
        assert_eq!(peers[1].name, "Studio Mac");
        assert_eq!(peers[1].paired_at, 1_700_000_000_000);
        assert_eq!(reopened.find("p2").unwrap().room, "ROOM-p2");
        assert!(reopened.find("nope").is_none());
    }

    #[test]
    fn a_missing_or_corrupt_file_reads_as_no_peers() {
        let dir = tempfile::tempdir().unwrap();

        let missing = PeerStore::new();
        missing.load(dir.path().join("peers.json"));
        assert!(missing.list().is_empty());

        let path = dir.path().join("broken.json");
        std::fs::write(&path, "{\"not\": \"a list\"}").unwrap();
        let broken = PeerStore::new();
        broken.load(path.clone());
        assert!(broken.list().is_empty());
        // The next pairing must still be able to write over it.
        broken.upsert(peer("p1")).unwrap();
        assert_eq!(broken.list().len(), 1);

        std::fs::write(&path, "").unwrap();
        let empty = PeerStore::new();
        empty.load(path);
        assert!(empty.list().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_peer_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("peers.json");
        let store = PeerStore::new();
        store.load(path.clone());
        store.upsert(peer("p1")).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the token file must be owner-only");
        let parent = std::fs::metadata(dir.path().join("nested"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            parent & 0o777,
            0o700,
            "the directory it created must be 0700"
        );
    }

    #[test]
    fn pairing_again_replaces_the_same_computer_and_removing_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = PeerStore::new();
        store.load(dir.path().join("peers.json"));
        store.upsert(peer("first")).unwrap();

        // The same room on the same origin is the same computer, even though a
        // re-pairing mints a new host-issued id.
        let mut second = peer("second");
        second.token = "mz_second".to_string();
        // Re-pairing the same computer: same origin and room, new host id.
        second.room = peer("first").room;
        store.upsert(second).unwrap();
        assert_eq!(store.list().len(), 1);
        assert!(store.find("first").is_none());
        assert_eq!(store.find("second").unwrap().token, "mz_second");

        assert!(store.remove("second"));
        assert!(!store.remove("second"));
        assert!(store.list().is_empty());
    }

    #[test]
    fn the_view_carries_no_token() {
        let wire = serde_json::to_value(peer("p1").view()).unwrap();
        assert!(
            wire.get("token").is_none(),
            "the token must not cross the API"
        );
        assert!(wire.get("deviceToken").is_none());
        assert_eq!(wire["id"], "p1");
        assert_eq!(wire["deviceId"], "this-device");
        assert_eq!(wire["pairedAt"], 1_700_000_000_000i64);

        // And the Debug a log line would use drops it too.
        let text = format!("{:?}", peer("p1"));
        assert!(!text.contains("mz_p1"), "debug printed the token: {text}");
    }

    #[test]
    fn credentials_are_the_self_hosted_shape_without_an_account() {
        let credentials = credentials(&peer("p1"), "0.1.3");
        assert_eq!(credentials.base, "https://relay.example.com");
        assert_eq!(credentials.room, "ROOM-p1");
        assert_eq!(credentials.account_session, "");
        assert_eq!(credentials.device_id, "this-device");
        assert_eq!(credentials.account_id, "selfhost");
        assert_eq!(credentials.peer_token, "mz_p1");
        assert_eq!(credentials.app_version, "0.1.3");
    }

    #[test]
    fn a_person_can_type_the_code_however_they_read_it() {
        assert_eq!(normalize_code(" k7qp-3m2x "), "K7QP-3M2X");
        // A space is not a dash: both survive as typed, the host compares the
        // canonical eight characters either way.
        assert_eq!(normalize_code("k7qp 3m2x"), "K7QP3M2X");
        assert_eq!(normalize_code("K7QP-3M2X"), "K7QP-3M2X");
        assert_eq!(normalize_code(""), "");
    }

    #[test]
    fn records_for_another_server_stay_on_disk_but_out_of_the_list() {
        let mut other = peer("p2");
        other.origin = "https://other.example.com".to_string();
        let listed = on_origin(vec![peer("p1"), other], "https://relay.example.com/");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "p1");
    }

    #[test]
    fn a_room_that_could_not_come_from_the_relay_is_refused() {
        assert!(valid_room("AB12CD34EF56"));
        assert!(valid_room("a-b_c"));
        assert!(!valid_room(""));
        assert!(!valid_room("../etc"));
        assert!(!valid_room("a/b"));
        assert!(!valid_room(&"x".repeat(ROOM_MAX + 1)));
    }
}
