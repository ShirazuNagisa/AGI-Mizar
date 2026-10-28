//! The account this window signs in with, as the window shows it.
//!
//! The relay's `/account/v1` service is the authority; these are the fields the
//! window draws. The session token is deliberately absent: it is a live
//! credential, it stays in the kernel's own file, and no client of this API can
//! read it.

use serde::{Deserialize, Serialize};

/// One device signed in to the account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDevice {
    pub id: String,
    pub name: String,
    pub model: String,
    /// `ios`, `macos`, `windows` — free text, as the device reported it.
    pub platform: String,
    pub login_at: i64,
    pub last_seen_at: i64,
    /// Whether this is the device that made the request.
    pub current: bool,
}

/// The account, as the window draws it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUser {
    pub id: String,
    pub username: String,
    pub email: Option<String>,
    /// `free` or `paid`.
    pub plan: String,
    /// End of the paid period, epoch milliseconds, or `None`.
    pub paid_until: Option<i64>,
    pub devices: Vec<AccountDevice>,
}

impl AccountUser {
    /// The devices of one platform, in the order the relay sent them.
    pub fn devices_on(&self, platform: &str) -> Vec<&AccountDevice> {
        self.devices
            .iter()
            .filter(|device| device.platform == platform)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_split_the_way_the_settings_page_groups_them() {
        let user: AccountUser = serde_json::from_value(serde_json::json!({
            "id": "u1",
            "username": "shirazu",
            "email": null,
            "plan": "paid",
            "paidUntil": 1_800_000_000_000i64,
            "devices": [
                {"id": "d1", "name": "Mac mini", "model": "Mac16,10", "platform": "macos",
                 "loginAt": 1, "lastSeenAt": 2, "current": true},
                {"id": "d2", "name": "iPhone", "model": "iPhone17,1", "platform": "ios",
                 "loginAt": 3, "lastSeenAt": 4, "current": false}
            ]
        }))
        .unwrap();
        assert_eq!(user.devices_on("macos").len(), 1);
        assert_eq!(user.devices_on("ios").len(), 1);
        assert!(user.devices_on("windows").is_empty());
        assert_eq!(user.plan, "paid");
    }
}
