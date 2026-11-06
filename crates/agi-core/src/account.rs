//! The relay's account service, from this window's side.
//!
//! The phone already signs in to `/account/v1`; this makes the desktop a device
//! of the same account, so the devices page is a list rather than a placeholder.
//!
//! ### What is stored, and what is not
//!
//! One file, `account.json`, 0600, holding the session token, this device's id
//! and the relay it belongs to. The token is a live credential: it never leaves
//! the kernel (no client command returns it) and is deleted on sign-out. The
//! account object is fetched again on every launch instead of being cached, so a
//! revoked session or a lapsed plan shows up as the relay says it is.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agi_protocol::account::{AccountDesktop, AccountDevice, AccountUser};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Where the relay's account service lives unless a launch says otherwise.
/// Where the account service lives.
///
/// The account-gated relay: same host and certificate as the paid relay, but the
/// process that also owns this desktop's tunnel. Sign-in and `/me` are proxied
/// to the origin account service from there, so the account is the same one;
/// only `/desktops` and `/connect` are answered locally, because those are the
/// two questions that need to see the tunnel.
pub const OFFICIAL_ACCOUNT_URL: &str = "https://dshmizar-1.shirazu-nagisa.com:8443";

/// A failure the window can show: a code to branch on and a sentence to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountError {
    pub code: String,
    pub message: String,
}

impl AccountError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// This machine, as the relay records it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceIdentity {
    id: String,
    name: String,
    model: String,
    platform: String,
    app_version: String,
}

/// The file this window keeps between launches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAccount {
    token: Option<String>,
    device: DeviceIdentity,
}

/// What `/account/v1/login` and `/account/v1/me` answer with.
#[derive(Debug, Deserialize)]
struct AuthAnswer {
    #[serde(default)]
    token: Option<String>,
    user: AccountUser,
}

/// Something the rest of the kernel may need to react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountEvent {
    /// `true` after a sign-in, `false` after a sign-out or a refused session.
    pub signed_in: bool,
    /// The account the event is about; `None` when nothing was signed in.
    pub user_id: Option<String>,
}

/// The account this window is signed in to.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanCertificateAnswer {
    pub body: agi_lan::CertificateBody,
    pub signature: String,
    pub ca_public_key: String,
}

/// A peer token the target minted, and the relay room it lives in.
#[derive(Clone)]
pub struct DesktopConnection {
    /// The account service origin, so the caller can build the ws(s) URL.
    pub base: String,
    /// The target's relay room.
    pub room: String,
    /// The target-minted device token (a live credential; never log it).
    pub token: String,
    /// The account session token the relay gate checks (never log it).
    pub account_session: String,
    /// This desktop's account device id.
    pub device_id: String,
    /// This account's user id.
    pub account_id: String,
}

pub struct AccountService {
    http: reqwest::Client,
    base: String,
    path: Mutex<Option<PathBuf>>,
    state: Mutex<State>,
    events: tokio::sync::broadcast::Sender<AccountEvent>,
}

struct State {
    token: Option<String>,
    user: Option<AccountUser>,
    device: DeviceIdentity,
}

impl AccountService {
    /// Build the client. Nothing is read until [`AccountService::load`].
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base: base.into().trim_end_matches('/').to_string(),
            path: Mutex::new(None),
            events: tokio::sync::broadcast::channel(16).0,
            state: Mutex::new(State {
                token: None,
                user: None,
                device: identity("id-unset"),
            }),
        }
    }

    /// Read what the last launch stored, keeping this machine's device id.
    pub fn load(&self, path: PathBuf) {
        let stored = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<StoredAccount>(&text).ok());
        let device = stored
            .as_ref()
            .map(|account| account.device.clone())
            .unwrap_or_else(|| identity(&new_device_id()));
        let token = stored.and_then(|account| account.token);
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.device = device;
            state.token = token;
        }
        *self
            .path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
    }

    /// The session and this machine's device id, for the tunnel to present.
    ///
    /// This is the one credential the relay client needs; it is deliberately not
    /// reachable through the command API.
    pub fn session(&self) -> Option<(String, String)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Some((state.token.clone()?, state.device.id.clone()))
    }

    /// Follow sign-ins and sign-outs.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AccountEvent> {
        self.events.subscribe()
    }

    fn announce(&self, signed_in: bool, user_id: Option<String>) {
        let _ = self.events.send(AccountEvent { signed_in, user_id });
    }

    /// Whether a session token is held.
    pub fn signed_in(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .token
            .is_some()
    }

    /// The account last read from the relay, if any.
    pub fn user(&self) -> Option<AccountUser> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .user
            .clone()
    }

    /// Sign in, store the token, and answer with the account.
    pub async fn login(
        &self,
        identifier: &str,
        password: &str,
    ) -> Result<AccountUser, AccountError> {
        let device = self.device();
        let body = json!({
            "identifier": identifier,
            "password": password,
            "device": device,
        });
        let answer = self
            .send(
                self.http
                    .post(format!("{}/account/v1/login", self.base))
                    .header(
                        "x-mizar-client-id",
                        device["id"].as_str().unwrap_or_default(),
                    )
                    .json(&body),
            )
            .await?;
        let token = answer
            .token
            .clone()
            .ok_or_else(|| AccountError::new("NO_TOKEN", "the relay did not return a session"))?;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.token = Some(token);
            state.user = Some(answer.user.clone());
        }
        self.save();
        self.announce(true, Some(answer.user.id.clone()));
        Ok(answer.user)
    }

    /// Where the browser should go to sign in with GitHub.
    ///
    /// The relay owns the OAuth app and its secret; this side only asks for the
    /// authorize URL and, later, spends the handoff code the callback page shows.
    pub async fn github_start(&self) -> Result<(String, String), AccountError> {
        let device = self.device();
        let response = self
            .http
            .post(format!("{}/account/v1/github/start", self.base))
            .header(
                "x-mizar-client-id",
                device["id"].as_str().unwrap_or_default(),
            )
            .json(&json!({ "device": device }))
            .send()
            .await
            .map_err(|error| AccountError::new("UNREACHABLE", format!("the relay: {error}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(AccountError::new(
                "GITHUB_START",
                format!("the relay answered {status}: {text}"),
            ));
        }
        #[derive(Deserialize)]
        struct Answer {
            #[serde(rename = "authorizeUrl")]
            authorize_url: String,
            state: String,
        }
        response
            .json::<Answer>()
            .await
            .map(|answer| (answer.authorize_url, answer.state))
            .map_err(|_| AccountError::new("BAD_ANSWER", "the relay's answer was not readable"))
    }

    /// Spend the handoff code the GitHub callback page showed.
    pub async fn github_exchange(&self, handoff: &str) -> Result<AccountUser, AccountError> {
        let device = self.device();
        let answer = self
            .send(
                self.http
                    .post(format!("{}/account/v1/github/exchange", self.base))
                    .header(
                        "x-mizar-client-id",
                        device["id"].as_str().unwrap_or_default(),
                    )
                    .json(&json!({ "handoff": handoff.trim(), "device": device })),
            )
            .await?;
        let token = answer
            .token
            .clone()
            .ok_or_else(|| AccountError::new("NO_TOKEN", "the relay did not return a session"))?;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.token = Some(token);
            state.user = Some(answer.user.clone());
        }
        self.save();
        self.announce(true, Some(answer.user.id.clone()));
        Ok(answer.user)
    }

    /// Where the browser should go to sign in with Apple.
    pub async fn apple_start(&self) -> Result<(String, String), AccountError> {
        let device = self.device();
        let response = self
            .http
            .post(format!("{}/account/v1/apple/start", self.base))
            .header(
                "x-mizar-client-id",
                device["id"].as_str().unwrap_or_default(),
            )
            .json(&json!({ "device": device }))
            .send()
            .await
            .map_err(|error| AccountError::new("UNREACHABLE", format!("the relay: {error}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(AccountError::new(
                "APPLE_START",
                format!("the relay answered {status}: {text}"),
            ));
        }
        #[derive(Deserialize)]
        struct Answer {
            #[serde(rename = "authorizeUrl")]
            authorize_url: String,
            state: String,
        }
        response
            .json::<Answer>()
            .await
            .map(|answer| (answer.authorize_url, answer.state))
            .map_err(|_| AccountError::new("BAD_ANSWER", "the relay's answer was not readable"))
    }

    /// Wait for the browser round-trip and sign in as soon as it lands.
    ///
    /// The browser cannot call back into a window it did not open, so the
    /// desktop asks the relay every two seconds instead; the state it got from
    /// the start call is the only key, and the code is single-use.
    async fn await_login(&self, provider: &str, state: &str) -> Result<String, AccountError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            let response = self
                .http
                .post(format!("{}/account/v1/{provider}/poll", self.base))
                .header(
                    "x-mizar-client-id",
                    self.device()["id"].as_str().unwrap_or_default(),
                )
                .json(&json!({ "state": state }))
                .send()
                .await
                .map_err(|error| AccountError::new("UNREACHABLE", format!("the relay: {error}")))?;
            #[derive(Deserialize)]
            struct Answer {
                pending: bool,
                code: Option<String>,
            }
            let answer: Answer = response.json().await.map_err(|_| {
                AccountError::new("BAD_ANSWER", "the relay's answer was not readable")
            })?;
            if !answer.pending {
                return answer.code.ok_or_else(|| {
                    AccountError::new("NO_CODE", "the relay finished without a code")
                });
            }
            if std::time::Instant::now() >= deadline {
                return Err(AccountError::new(
                    "TIMEOUT",
                    "the browser sign-in did not finish in five minutes",
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    /// Wait for the Apple browser round-trip, then sign in.
    pub async fn apple_await(&self, state: &str) -> Result<AccountUser, AccountError> {
        let code = self.await_login("apple", state).await?;
        self.apple_exchange(&code).await
    }

    /// Wait for the GitHub browser round-trip, then sign in.
    pub async fn github_await(&self, state: &str) -> Result<AccountUser, AccountError> {
        let code = self.await_login("github", state).await?;
        self.github_exchange(&code).await
    }

    /// Spend the handoff code the Apple callback page showed.
    pub async fn apple_exchange(&self, handoff: &str) -> Result<AccountUser, AccountError> {
        let device = self.device();
        let answer = self
            .send(
                self.http
                    .post(format!("{}/account/v1/apple/exchange", self.base))
                    .header(
                        "x-mizar-client-id",
                        device["id"].as_str().unwrap_or_default(),
                    )
                    .json(&json!({ "handoff": handoff.trim(), "device": device })),
            )
            .await?;
        let token = answer
            .token
            .clone()
            .ok_or_else(|| AccountError::new("NO_TOKEN", "the relay did not return a session"))?;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.token = Some(token);
            state.user = Some(answer.user.clone());
        }
        self.save();
        self.announce(true, Some(answer.user.id.clone()));
        Ok(answer.user)
    }

    /// Read the account again. A session the relay no longer accepts is dropped.
    pub async fn refresh(&self) -> Result<Option<AccountUser>, AccountError> {
        let Some(token) = self.token() else {
            return Ok(None);
        };
        let device_id = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .device
            .id
            .clone();
        let response = self
            .http
            .get(format!("{}/account/v1/me", self.base))
            .header("x-mizar-client-id", &device_id)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|error| AccountError::new("UNREACHABLE", format!("the relay: {error}")))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let previous = self.user().map(|user| user.id);
            self.forget();
            self.announce(false, previous);
            return Ok(None);
        }
        let answer = self.read(response).await?;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.user = Some(answer.user.clone());
        }
        Ok(Some(answer.user))
    }

    /// This account's user id, when signed in.
    pub fn user_id(&self) -> Option<String> {
        self.user().map(|user| user.id)
    }

    /// This device's account id, as the relay knows it.
    pub fn device_id(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (!state.device.id.is_empty() && state.device.id != "id-unset")
            .then(|| state.device.id.clone())
    }

    /// Ask a computer on this account to mint a peer device token for it.
    ///
    /// The relay verifies that both ends belong to the same account and that the
    /// target is connected; the target mints the token. This is the same broker
    /// the phone uses, but the desktop presents its own device id and may ask
    /// for the `peer` kind once the relay learns to distinguish it.
    pub async fn connect(
        &self,
        target_device_id: &str,
        engine: &str,
    ) -> Result<DesktopConnection, AccountError> {
        let Some(token) = self.token() else {
            return Err(AccountError::new(
                "SIGN_IN_REQUIRED",
                "sign in to the account before delegating to another computer",
            ));
        };
        let Some(caller) = self.device_id() else {
            return Err(AccountError::new(
                "SIGN_IN_REQUIRED",
                "this install has no account device id",
            ));
        };
        let response = self
            .http
            .post(format!("{}/account/v1/connect", self.base))
            .header("x-mizar-client-id", caller.clone())
            .bearer_auth(&token)
            .json(&json!({ "deviceId": target_device_id, "engine": engine, "kind": "peer" }))
            .send()
            .await
            .map_err(|error| AccountError::new("UNREACHABLE", format!("the relay: {error}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let (code, message, contact) = failure(&text, status);
            return Err(AccountError {
                code,
                message: match contact {
                    Some(contact) => format!("{message} ({contact})"),
                    None => message,
                },
            });
        }
        let answer: Value = serde_json::from_str(&text)
            .map_err(|_| AccountError::new("BAD_ANSWER", "the relay's answer was not readable"))?;
        let room = answer
            .get("room")
            .and_then(Value::as_str)
            .filter(|room| !room.is_empty())
            .ok_or_else(|| AccountError::new("BAD_ANSWER", "the relay returned no room"))?;
        let peer = answer
            .get("token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| AccountError::new("BAD_ANSWER", "the relay returned no token"))?;
        let account_id = match self.user_id() {
            Some(id) => id,
            // A launch that signs in and delegates before the background refresh
            // has answered still needs the account id in the hello frame. One
            // `/me` here is cheaper than a delegation that fails on a race.
            None => match self.refresh().await {
                Ok(Some(user)) => user.id,
                Ok(None) => {
                    return Err(AccountError::new(
                        "SIGN_IN_REQUIRED",
                        "the account session is no longer valid",
                    ))
                }
                Err(error) => return Err(error),
            },
        };
        Ok(DesktopConnection {
            base: self.base.clone(),
            room: room.to_string(),
            token: peer.to_string(),
            account_session: token,
            device_id: caller,
            account_id,
        })
    }

    /// The account's computers, with their current reachability.
    pub async fn desktops(&self) -> Result<Vec<AccountDesktop>, AccountError> {
        let Some(token) = self.token() else {
            return Ok(Vec::new());
        };
        #[derive(Deserialize)]
        struct Answer {
            desktops: Vec<AccountDesktop>,
        }
        let response = self
            .http
            .get(format!("{}/account/v1/desktops", self.base))
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|error| AccountError::new("UNREACHABLE", format!("the relay: {error}")))?;
        let text = response
            .text()
            .await
            .map_err(|error| AccountError::new("UNREACHABLE", format!("the relay: {error}")))?;
        let answer: Answer = serde_json::from_str(&text)
            .map_err(|_| AccountError::new("BAD_ANSWER", "the relay's answer was not readable"))?;
        Ok(answer.desktops)
    }

    /// Ask the account CA to sign this device's LAN public keys.
    pub async fn lan_certificate(
        &self,
        ed25519_public: &str,
        x25519_public: &str,
    ) -> Result<LanCertificateAnswer, AccountError> {
        let token = self
            .token()
            .ok_or_else(|| AccountError::new("SIGN_IN_REQUIRED", "no account session"))?;
        let device = self
            .device_id()
            .ok_or_else(|| AccountError::new("SIGN_IN_REQUIRED", "no device id"))?;
        let response = self
            .http
            .post(format!("{}/account/v1/lan/certificate", self.base))
            .header("x-mizar-client-id", &device)
            .bearer_auth(&token)
            .json(&json!({
                "ed25519Public": ed25519_public,
                "x25519Public": x25519_public,
            }))
            .send()
            .await
            .map_err(|error| AccountError::new("UNREACHABLE", format!("the relay: {error}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let (code, message, _) = failure(&text, status);
            return Err(AccountError::new(code, message));
        }
        serde_json::from_str(&text)
            .map_err(|_| AccountError::new("BAD_ANSWER", "the LAN certificate was not readable"))
    }

    /// Ask one online computer what it offers, through the relay's room proxy.
    ///
    /// `hello` is the only unauthenticated method on the target; the relay's
    /// room gate still wants an account session, which is why this goes through
    /// the account service rather than a bare HTTP client.
    pub async fn probe(&self, room: &str) -> Result<serde_json::Value, AccountError> {
        let token = self
            .token()
            .ok_or_else(|| AccountError::new("SIGN_IN_REQUIRED", "no account session"))?;
        let device = self.device_id().unwrap_or_default();
        let response = self
            .http
            .get(format!("{}/r/{room}/mizar/v1/hello", self.base))
            .header("x-mizar-session", &token)
            .header("x-mizar-client-id", &device)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|error| AccountError::new("UNREACHABLE", format!("the relay: {error}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AccountError::new(
                format!("HTTP_{}", status.as_u16()),
                "the computer did not answer its hello",
            ));
        }
        serde_json::from_str(&text)
            .map_err(|_| AccountError::new("BAD_ANSWER", "the computer's hello was not readable"))
    }

    /// Sign out: tell the relay, then forget the token whatever it said.
    pub async fn logout(&self) -> Result<(), AccountError> {
        let previous = self.user().map(|user| user.id);
        if let Some(token) = self.token() {
            let _ = self
                .http
                .post(format!("{}/account/v1/logout", self.base))
                .bearer_auth(&token)
                .send()
                .await;
        }
        self.forget();
        self.announce(false, previous);
        Ok(())
    }

    /// The devices the account has, split the way the settings page draws them.
    pub fn devices(&self) -> (Vec<AccountDevice>, Vec<AccountDevice>) {
        let user = self.user();
        let Some(user) = user else {
            return (Vec::new(), Vec::new());
        };
        let desktops = user
            .devices
            .iter()
            .filter(|device| device.platform != "ios")
            .cloned()
            .collect();
        let phones = user
            .devices
            .iter()
            .filter(|device| device.platform == "ios")
            .cloned()
            .collect();
        (desktops, phones)
    }

    fn token(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .token
            .clone()
    }

    fn device(&self) -> Value {
        let device = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .device
            .clone();
        json!({
            "id": device.id,
            "name": device.name,
            "model": device.model,
            "platform": device.platform,
            "appVersion": device.app_version,
        })
    }

    /// Drop the session and the file that held it.
    fn forget(&self) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.token = None;
            state.user = None;
        }
        self.save();
    }

    /// Write the token and this device's identity; never the account object.
    fn save(&self) {
        let path = self
            .path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(path) = path else {
            return;
        };
        let stored = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            StoredAccount {
                token: state.token.clone(),
                device: state.device.clone(),
            }
        };
        if let Some(parent) = path.parent() {
            let _ = private_dir(parent);
        }
        let _ = write_private(
            &path,
            &serde_json::to_vec_pretty(&stored).unwrap_or_default(),
        );
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<AuthAnswer, AccountError> {
        let response = request
            .send()
            .await
            .map_err(|error| AccountError::new("UNREACHABLE", format!("the relay: {error}")))?;
        self.read(response).await
    }

    /// Read one answer, turning the relay's error envelope into a code.
    async fn read(&self, response: reqwest::Response) -> Result<AuthAnswer, AccountError> {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let (code, message, contact) = failure(&text, status);
            return Err(AccountError {
                code,
                message: match contact {
                    Some(contact) => format!("{message} ({contact})"),
                    None => message,
                },
            });
        }
        serde_json::from_str::<AuthAnswer>(&text)
            .map_err(|_| AccountError::new("BAD_ANSWER", "the relay's answer was not readable"))
    }
}

/// The relay's `{error, message, contact?}` envelope, or the status as a code.
fn failure(text: &str, status: reqwest::StatusCode) -> (String, String, Option<String>) {
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    let code = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| status.as_str().to_string());
    let message = parsed
        .as_ref()
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("the relay answered {status}"));
    let contact = parsed
        .as_ref()
        .and_then(|value| value.get("contact"))
        .and_then(Value::as_str)
        .map(str::to_string);
    (code, message, contact)
}

/// This machine's stable id, generated once and kept in the account file.
fn new_device_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The identity the relay records for this machine.
fn identity(id: &str) -> DeviceIdentity {
    DeviceIdentity {
        id: id.to_string(),
        name: agi_platform::host_name(),
        model: agi_platform::machine_model(),
        platform: agi_platform::platform().to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// A directory only this user can enter.
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A file only this user can read.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_session_is_read_back_without_the_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        let service = AccountService::new("http://127.0.0.1:1");
        service.load(path.clone());
        assert!(!service.signed_in());

        let device = service.device();
        let stored = StoredAccount {
            token: Some("t0ken".to_string()),
            device: serde_json::from_value(device).unwrap(),
        };
        // The writer is what the service uses; a test that wrote the file its own
        // way would not be testing the permissions the real path gets.
        write_private(&path, &serde_json::to_vec(&stored).unwrap()).unwrap();

        let again = AccountService::new("http://127.0.0.1:1");
        again.load(path.clone());
        assert!(again.signed_in());
        assert_eq!(again.device()["id"], stored.device.id);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn the_relay_error_envelope_becomes_a_code_and_a_sentence() {
        let (code, message, contact) = failure(
            r#"{"error":"ACCOUNT_DISABLED","message":"disabled","contact":"x@example.com"}"#,
            reqwest::StatusCode::FORBIDDEN,
        );
        assert_eq!(code, "ACCOUNT_DISABLED");
        assert_eq!(message, "disabled");
        assert_eq!(contact.as_deref(), Some("x@example.com"));

        let (code, _, _) = failure("not json", reqwest::StatusCode::BAD_GATEWAY);
        assert_eq!(code, "502");
    }

    #[test]
    fn devices_split_into_desktops_and_phones() {
        let dir = tempfile::tempdir().unwrap();
        let service = AccountService::new("http://127.0.0.1:1");
        service.load(dir.path().join("account.json"));
        service.state.lock().unwrap().user = Some(AccountUser {
            id: "u".into(),
            username: "s".into(),
            email: None,
            plan: "free".into(),
            paid_until: None,
            devices: vec![
                AccountDevice {
                    id: "d1".into(),
                    name: "Mac".into(),
                    model: "Mac".into(),
                    platform: "macos".into(),
                    login_at: 0,
                    last_seen_at: 0,
                    current: true,
                },
                AccountDevice {
                    id: "d2".into(),
                    name: "iPhone".into(),
                    model: "iPhone".into(),
                    platform: "ios".into(),
                    login_at: 0,
                    last_seen_at: 0,
                    current: false,
                },
            ],
        });
        let (desktops, phones) = service.devices();
        assert_eq!(desktops.len(), 1);
        assert_eq!(phones.len(), 1);
    }

    /// A fake relay: `/login` and `/me`, with the envelope the real one speaks.
    async fn fake_relay() -> (String, tokio::task::JoinHandle<()>) {
        use axum::routing::{get, post};
        use axum::{extract::State, Json, Router};
        use std::sync::Arc;

        #[derive(Clone, Default)]
        struct Seen(Arc<Mutex<Vec<String>>>);

        async fn login(
            State(seen): State<Seen>,
            headers: axum::http::HeaderMap,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let device_id = headers
                .get("x-mizar-client-id")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            seen.0
                .lock()
                .unwrap()
                .push(format!("login {} {}", body["identifier"], device_id));
            Json(json!({
                "token": "session-token",
                "user": {
                    "id": "u1", "username": body["identifier"], "email": null,
                    "plan": "free", "paidUntil": null,
                    "devices": [{
                        "id": device_id, "name": "Mac", "model": "Mac", "platform": "macos",
                        "loginAt": 1, "lastSeenAt": 2, "current": true
                    }]
                }
            }))
        }

        async fn me(
            State(seen): State<Seen>,
            headers: axum::http::HeaderMap,
        ) -> (axum::http::StatusCode, Json<Value>) {
            let token = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            seen.0.lock().unwrap().push(format!("me {token}"));
            if token != "Bearer session-token" {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "UNAUTHENTICATED", "message": "no session"})),
                );
            }
            (
                axum::http::StatusCode::OK,
                Json(json!({
                    "user": {
                        "id": "u1", "username": "shirazu", "email": null,
                        "plan": "paid", "paidUntil": 1_800_000_000_000i64,
                        "devices": [
                            {"id": "d1", "name": "Mac", "model": "Mac", "platform": "macos",
                             "loginAt": 1, "lastSeenAt": 2, "current": true},
                            {"id": "d2", "name": "iPhone", "model": "iPhone", "platform": "ios",
                             "loginAt": 3, "lastSeenAt": 4, "current": false}
                        ]
                    }
                })),
            )
        }

        async fn connect(Json(body): Json<Value>) -> Json<Value> {
            Json(json!({
                "room": "FAKE-ROOM",
                "token": format!("peer-for-{}", body["deviceId"].as_str().unwrap_or("?")),
            }))
        }

        async fn lan_certificate(Json(_body): Json<Value>) -> Json<Value> {
            Json(json!({
                "body": {
                    "v": 1, "accountId": "u1", "deviceId": "d1",
                    "ed25519Public": "ed", "x25519Public": "x",
                    "issuedAt": 1, "expiresAt": 2
                },
                "signature": "sig",
                "caPublicKey": "ca"
            }))
        }

        let seen = Seen::default();
        let app = Router::new()
            .route("/account/v1/login", post(login))
            .route("/account/v1/me", get(me))
            .route("/account/v1/connect", post(connect))
            .route("/account/v1/lan/certificate", post(lan_certificate))
            .with_state(seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), handle)
    }

    #[tokio::test]
    async fn signing_in_registers_this_machine_and_reads_the_account_back() {
        let (base, server) = fake_relay().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        let service = AccountService::new(base);
        service.load(path.clone());

        let user = service.login("shirazu", "hunter2").await.unwrap();
        assert_eq!(user.username, "shirazu");
        assert_eq!(user.devices[0].platform, "macos");
        assert!(service.signed_in());

        // The token is on disk for the next launch, and nowhere in the account
        // object this side hands out.
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(stored.contains("session-token"));

        let refreshed = service.refresh().await.unwrap().unwrap();
        assert_eq!(refreshed.plan, "paid");
        let (desktops, phones) = service.devices();
        assert_eq!(desktops.len(), 1);
        assert_eq!(phones.len(), 1);

        server.abort();
    }

    #[tokio::test]
    async fn connecting_to_a_desktop_returns_both_credentials() {
        let (base, server) = fake_relay().await;
        let dir = tempfile::tempdir().unwrap();
        let service = AccountService::new(base.clone());
        service.load(dir.path().join("account.json"));
        service.login("shirazu", "hunter2").await.unwrap();

        let connection = service.connect("d1", "dsh").await.unwrap();
        assert_eq!(connection.base, base);
        assert_eq!(connection.room, "FAKE-ROOM");
        assert_eq!(connection.token, "peer-for-d1");
        assert_eq!(connection.account_session, "session-token");
        assert_eq!(connection.account_id, "u1");
        assert_eq!(connection.device_id, service.device_id().unwrap());
        server.abort();
    }

    #[tokio::test]
    async fn a_lan_certificate_is_read_back() {
        let (base, server) = fake_relay().await;
        let dir = tempfile::tempdir().unwrap();
        let service = AccountService::new(base);
        service.load(dir.path().join("account.json"));
        service.login("shirazu", "hunter2").await.unwrap();
        let answer = service.lan_certificate("ed", "x").await.unwrap();
        assert_eq!(answer.body.account_id, "u1");
        assert_eq!(answer.body.device_id, "d1");
        assert_eq!(answer.signature, "sig");
        assert_eq!(answer.ca_public_key, "ca");
        server.abort();
    }

    #[tokio::test]
    async fn a_revoked_session_is_forgotten_not_kept() {
        let (base, server) = fake_relay().await;
        let dir = tempfile::tempdir().unwrap();
        let service = AccountService::new(base);
        service.load(dir.path().join("account.json"));
        service.login("shirazu", "hunter2").await.unwrap();
        service.state.lock().unwrap().token = Some("revoked".to_string());

        assert!(service.refresh().await.unwrap().is_none());
        assert!(!service.signed_in());
        server.abort();
    }
}
