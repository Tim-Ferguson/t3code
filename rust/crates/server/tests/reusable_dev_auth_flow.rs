//! The reusable dev credential (`T3CODE_DEV_AUTH_TOKEN`, `auth/ReusableDevAuth.ts`) over a real socket: the token signs a browser in with a
//! cookie named for its hash, authenticates as the administrative session it stands for, exchanges for child sessions, and is
//! ignored by a server that is not a web dev server. Ids are those the unchanged module computes for the same token.
use chrono::Utc;
use serde_json::{Value, json};
use t3_server::{
    auth::{AuthService, REUSABLE_DEV_SESSION_PREFIX, ReusableDevAuth, resolve_dev_auth_token},
    persistence::Store,
    transport::{ApiState, router},
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
// `createHash("sha256").update(TOKEN).digest("hex")`, the id and cookie suffix the original computes (checked with Node).
const TOKEN_ID: &str = "a8ae6e6ee929abea3afcfc5258c8ccd6f85273e0d4626d26c7279f3250f77c8e";

async fn serve(auth: AuthService, store: Store) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let state = ApiState {
        store, auth, environment: json!({"environmentId":"dev","label":"Dev","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{}}),
        config: None, settings: None, cors_origins: None, assets: None, providers: None, execution: None, workspace: None, terminals: None, discovery: None,
        resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    tokio::spawn(async move { axum::serve(listener, router(state).into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap() });
    (address, stop)
}

#[test]
fn only_a_web_dev_server_with_a_long_enough_token_has_it() {
    assert_eq!(resolve_dev_auth_token(None), Ok(None));
    assert_eq!(resolve_dev_auth_token(Some("   ")), Ok(None));
    assert_eq!(resolve_dev_auth_token(Some("short")), Err("T3CODE_DEV_AUTH_TOKEN must contain at least 32 characters."));
    assert_eq!(resolve_dev_auth_token(Some(&format!("  {TOKEN}\n"))).unwrap().as_deref(), Some(TOKEN));
    assert!(ReusableDevAuth::resolve("web", true, Some(TOKEN)).is_some());
    for (mode, dev_url, token) in [("desktop", true, Some(TOKEN)), ("web", false, Some(TOKEN)), ("web", true, None), ("web", true, Some(""))] {
        assert!(ReusableDevAuth::resolve(mode, dev_url, token).is_none(), "{mode} {dev_url} {token:?}");
    }
    let dev = ReusableDevAuth::resolve("web", true, Some(TOKEN)).unwrap();
    assert!(dev.matches(TOKEN) && !dev.matches("nope") && !dev.matches(&TOKEN[1..]));
    assert!(dev.session_id.starts_with(REUSABLE_DEV_SESSION_PREFIX));
    assert_eq!(dev.cookie_name, format!("t3_dev_session_{}", &dev.session_id[REUSABLE_DEV_SESSION_PREFIX.len()..]));
    assert_eq!(dev.session_id.len(), REUSABLE_DEV_SESSION_PREFIX.len() + 64);
    assert_eq!((dev.session_id.as_str(), dev.cookie_name.as_str()), (format!("dev-auth-{TOKEN_ID}").as_str(), format!("t3_dev_session_{TOKEN_ID}").as_str()));
}

#[tokio::test]
async fn the_credential_signs_a_browser_in_and_authenticates_as_the_session_it_stands_for() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("rust-state.sqlite")).unwrap();
    let dev = ReusableDevAuth::resolve("web", true, Some(TOKEN)).unwrap();
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_dev".into(), "loopback-browser".into()).unwrap().with_reusable_dev_auth(dev.clone(), Utc::now()).unwrap();
    // The seeded session is administrative and does not expire in any lifetime that matters.
    let session = auth.verify_session(TOKEN, Utc::now()).unwrap();
    assert_eq!((session.session_id.as_str(), session.subject.as_str(), session.method.as_str()), (dev.session_id.as_str(), "reusable-dev-token", "browser-session-cookie"));
    assert_eq!(session.scopes.len(), t3_contracts::AUTH_ADMINISTRATIVE_SCOPES.len());
    assert!(session.expires_at.to_rfc3339().starts_with("9999-12-31"));
    // Seeding is once per database: a restart does not replace it (and so does not undo a revocation).
    auth.revoke(&dev.session_id, Utc::now()).unwrap();
    let again = AuthService::new(store.clone(), [7; 32], "t3_session_dev".into(), "loopback-browser".into()).unwrap().with_reusable_dev_auth(dev.clone(), Utc::now()).unwrap();
    assert!(again.verify_session(TOKEN, Utc::now()).is_err(), "a revoked dev session stays revoked");
    let store = Store::open(directory.path().join("second.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_dev".into(), "loopback-browser".into()).unwrap().with_reusable_dev_auth(dev.clone(), Utc::now()).unwrap();
    let (address, stop) = serve(auth.clone(), store).await;
    let client = reqwest::Client::new();
    let base = format!("http://{address}");

    // Pairing with the credential answers with the dev cookie, the ordinary one expired.
    let response = client.post(format!("{base}/api/auth/browser-session")).json(&json!({"credential":TOKEN})).send().await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let cookies: Vec<String> = response.headers().get_all("set-cookie").iter().map(|value| value.to_str().unwrap().to_owned()).collect();
    assert!(cookies.iter().any(|cookie| cookie.starts_with(&format!("{}={TOKEN};", dev.cookie_name)) && cookie.contains("HttpOnly") && cookie.contains("Max-Age=2592000")), "{cookies:?}");
    assert!(cookies.iter().any(|cookie| cookie.starts_with("t3_session_dev=;") && cookie.contains("Max-Age=0")), "{cookies:?}");
    let body: Value = response.json().await.unwrap();
    assert_eq!((body["authenticated"].clone(), body["sessionMethod"].clone()), (json!(true), json!("browser-session-cookie")));

    // The cookie authenticates; with an Authorization header of its own it is not consulted; a wrong value does nothing.
    let session_state = |cookie: String, authorization: Option<&str>| {
        let client = client.clone();
        let url = format!("{base}/api/auth/session");
        let authorization = authorization.map(str::to_owned);
        async move {
            let mut request = client.get(url).header("cookie", cookie);
            if let Some(authorization) = authorization {
                request = request.header("authorization", authorization);
            }
            request.send().await.unwrap().json::<Value>().await.unwrap()
        }
    };
    assert_eq!(session_state(format!("{}={TOKEN}", dev.cookie_name), None).await["authenticated"], json!(true));
    assert_eq!(session_state(format!("{}=wrong", dev.cookie_name), None).await["authenticated"], json!(false));
    assert_eq!(session_state(format!("{}={TOKEN}", dev.cookie_name), Some("Bearer wrong")).await["authenticated"], json!(false));
    assert_eq!(session_state(format!("{}={TOKEN}", dev.cookie_name), Some(&format!("Bearer {TOKEN}"))).await["authenticated"], json!(true), "the token is also an ordinary bearer");

    // A token exchange gives a child session of the same scopes, named for the child.
    let (child, _) = auth.exchange_pairing_bearer(TOKEN, None, json!({"deviceType":"unknown"}), Utc::now()).unwrap();
    assert_eq!(child.subject, "reusable-dev-token-child");
    assert_eq!(child.scopes.len(), t3_contracts::AUTH_ADMINISTRATIVE_SCOPES.len());

    // A websocket ticket of the dev session works here and is refused by a server whose credential is another.
    let ticket = auth.issue_websocket_ticket(&session, Utc::now()).unwrap();
    assert!(auth.verify_websocket_ticket(ticket["ticket"].as_str().unwrap(), Utc::now()).is_ok());
    let other_store = Store::open(directory.path().join("other.sqlite")).unwrap();
    let other = AuthService::new(other_store, [9; 32], "t3_session_dev".into(), "loopback-browser".into()).unwrap();
    assert!(other.verify_websocket_ticket(ticket["ticket"].as_str().unwrap(), Utc::now()).is_err());
    let _ = stop.send(());
}
