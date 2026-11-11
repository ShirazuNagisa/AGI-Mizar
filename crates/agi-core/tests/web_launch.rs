//! `/launch` exchanges the URL token for a cookie, and the cookie authorizes
//! the same requests a bearer token does.

use agi_core::server::{bind, Config};
use agi_core::Core;
use agi_engine_dsh::DshEngine;
use reqwest::header::{LOCATION, SET_COOKIE};
use reqwest::redirect::Policy;
use reqwest::StatusCode;

#[tokio::test]
async fn launch_sets_cookie_and_cookie_authorizes() {
    let dsh = DshEngine::new("plugin-token");
    let core = Core::new(vec![dsh.clone() as std::sync::Arc<dyn agi_engine::Engine>]);
    let (port, _server) = bind(
        core,
        dsh,
        None,
        Config {
            api_token: "ui-token".to_string(),
            plugin_token: "plugin-token".to_string(),
            hook_token: "hook-token".to_string(),
            tool_token: "tool-token".to_string(),
            api_host: "127.0.0.1".to_string(),
            api_port: 0,
        },
    )
    .await
    .expect("the loopback API binds");

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("a client");
    let base = format!("http://127.0.0.1:{port}");

    let bare = client
        .get(format!("{base}/api/v1/state"))
        .send()
        .await
        .expect("state without auth");
    assert_eq!(bare.status(), StatusCode::UNAUTHORIZED);

    let launch = client
        .get(format!("{base}/launch?token=ui-token&next=/settings"))
        .send()
        .await
        .expect("launch");
    assert_eq!(launch.status(), StatusCode::SEE_OTHER);
    assert_eq!(launch.headers()[LOCATION], "/settings");
    let set_cookie = launch.headers()[SET_COOKIE]
        .to_str()
        .expect("a cookie header");
    assert!(set_cookie.contains("agi_mizar_session=ui-token"));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
    let cookie = set_cookie
        .split(';')
        .next()
        .expect("the cookie pair")
        .to_string();

    let state = client
        .get(format!("{base}/api/v1/state"))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await
        .expect("state with cookie");
    assert_eq!(state.status(), StatusCode::OK);

    let evil = client
        .get(format!("{base}/launch?token=ui-token&next=//evil.example"))
        .send()
        .await
        .expect("open-redirect launch");
    assert_eq!(evil.headers()[LOCATION], "/");

    let logout = client
        .post(format!("{base}/api/v1/logout"))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await
        .expect("logout");
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    assert!(logout.headers()[SET_COOKIE]
        .to_str()
        .expect("clearing cookie")
        .contains("Max-Age=0"));
}
