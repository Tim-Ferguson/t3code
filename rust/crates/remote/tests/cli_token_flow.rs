//! The CLI's OAuth credential against a token endpoint on loopback: refresh, the device
//! authorization grant, and the loopback authorization-code flow.
mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use support::*;
use t3_remote::cli_token::*;
use t3_remote::config::CloudCliOAuthConfig;
use t3_remote::secrets::*;

fn id_token(email: &str) -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(json!({"email": email, "sub": "user_1"}).to_string());
    format!("e30.{payload}.sig")
}

fn token_body(access: &str, refresh: Option<&str>, expires_in: u64, email: Option<&str>) -> Value {
    let mut body =
        json!({"access_token": access, "expires_in": expires_in, "token_type": "bearer"});
    if let Some(refresh) = refresh {
        body["refresh_token"] = json!(refresh);
    }
    if let Some(email) = email {
        body["id_token"] = json!(id_token(email));
    }
    body
}

/// A token endpoint that records form bodies.
async fn token_server(
    handler: impl Fn(&std::collections::BTreeMap<String, String>, &str) -> (u16, Value)
    + Send
    + Sync
    + 'static,
) -> FakeRelay {
    FakeRelay::start_raw_form(move |path, fields| handler(fields, path)).await
}

fn oauth(base: &str) -> CloudCliOAuthConfig {
    CloudCliOAuthConfig {
        token_endpoint: format!("{base}/oauth/token"),
        device_authorization_endpoint: format!("{base}/oauth/device_authorization"),
        client_id: "oauth_client".into(),
        loopback_port: 0,
        redirect_uri: "http://127.0.0.1:0/callback".into(),
        scopes: ["openid", "profile", "email", "offline_access"],
    }
}

fn manager(base: &str, secrets: Arc<MemorySecretStore>, clock: Arc<FixedClock>) -> CliTokenManager {
    CliTokenManager::new(secrets, clock, oauth(base), "https://app.t3.codes".into())
}

fn stored(secrets: &MemorySecretStore, token: &PersistedToken) {
    secrets
        .set(
            CLOUD_CLI_OAUTH_TOKEN_SECRET,
            serde_json::to_vec(token).unwrap().as_slice(),
        )
        .unwrap();
}

#[test]
fn identity_comes_from_email_then_username_then_subject() {
    let b64 = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    let token = |claims: Value| format!("h.{}.s", b64(claims));
    assert_eq!(
        id_token_identity(Some(&token(
            json!({"email": "a@b.c", "preferred_username": "u", "sub": "s"})
        )))
        .as_deref(),
        Some("a@b.c")
    );
    assert_eq!(
        id_token_identity(Some(&token(
            json!({"email": "", "preferred_username": "u", "sub": "s"})
        )))
        .as_deref(),
        Some("u")
    );
    assert_eq!(
        id_token_identity(Some(&token(json!({"sub": "s"})))).as_deref(),
        Some("s")
    );
    assert_eq!(id_token_identity(Some(&token(json!({"email": 5})))), None);
    assert_eq!(id_token_identity(Some("not-a-jwt")), None);
    assert_eq!(id_token_identity(Some("a.!!!.c")), None);
    assert_eq!(id_token_identity(None), None);
}

#[test]
fn pkce_material_matches_the_challenge_rule() {
    use sha2::Digest;
    let pkce = make_pkce_request();
    let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(pkce.verifier.as_bytes()));
    assert_eq!(pkce.challenge, expected);
    assert_eq!(pkce.verifier.len(), 43);
    assert_eq!(pkce.state.len(), 22);
    assert_ne!(make_pkce_request().verifier, pkce.verifier);
}

#[tokio::test]
async fn a_token_that_is_not_about_to_expire_is_returned_as_stored() {
    let server = token_server(|_, _| (500, json!({}))).await;
    let secrets = Arc::new(MemorySecretStore::new());
    let clock = FixedClock::at(NOW_MS);
    let current = PersistedToken {
        access_token: "a".into(),
        refresh_token: "r".into(),
        expires_at_epoch_ms: NOW_MS + 6 * 60 * 1000,
        identity: Some("me".into()),
    };
    stored(&secrets, &current);
    let manager = manager(&server.url, secrets, clock);
    assert_eq!(within(manager.get_existing()).await.unwrap(), Some(current));
    assert!(server.recorded().is_empty());
    assert!(within(manager.has_credential()).await.unwrap());
}

#[tokio::test]
async fn a_token_within_five_minutes_of_expiry_is_refreshed_and_keeps_its_identity() {
    let server = token_server(|fields, _| {
        assert_eq!(
            fields.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            fields.get("refresh_token").map(String::as_str),
            Some("r-old")
        );
        assert_eq!(
            fields.get("client_id").map(String::as_str),
            Some("oauth_client")
        );
        (200, token_body("a-new", None, 3600, None))
    })
    .await;
    let secrets = Arc::new(MemorySecretStore::new());
    let clock = FixedClock::at(NOW_MS);
    stored(
        &secrets,
        &PersistedToken {
            access_token: "a-old".into(),
            refresh_token: "r-old".into(),
            expires_at_epoch_ms: NOW_MS + 4 * 60 * 1000,
            identity: Some("me@x".into()),
        },
    );
    let manager = manager(&server.url, secrets.clone(), clock);
    let refreshed = within(manager.get_existing()).await.unwrap().unwrap();
    assert_eq!(refreshed.access_token, "a-new");
    // Clerk sent no new refresh token or id token: the old ones carry over.
    assert_eq!(refreshed.refresh_token, "r-old");
    assert_eq!(refreshed.identity.as_deref(), Some("me@x"));
    assert_eq!(refreshed.expires_at_epoch_ms, NOW_MS + 3_600_000);
    let persisted: PersistedToken =
        serde_json::from_str(&secrets.text(CLOUD_CLI_OAUTH_TOKEN_SECRET).unwrap()).unwrap();
    assert_eq!(persisted, refreshed);
    let sent = server.recorded();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
}

#[tokio::test]
async fn a_failed_refresh_is_reported_and_leaves_the_stored_token() {
    let server = token_server(|_, _| (400, json!({"error": "invalid_grant"}))).await;
    let secrets = Arc::new(MemorySecretStore::new());
    let old = PersistedToken {
        access_token: "a".into(),
        refresh_token: "r".into(),
        expires_at_epoch_ms: NOW_MS - 1,
        identity: None,
    };
    stored(&secrets, &old);
    let manager = manager(&server.url, secrets.clone(), FixedClock::at(NOW_MS));
    let error = within(manager.get_existing()).await.unwrap_err();
    assert!(matches!(error, CliTokenError::CredentialRefresh(_)));
    assert_eq!(
        error.to_string(),
        "Could not refresh the T3 Connect CLI credential."
    );
    let persisted: PersistedToken =
        serde_json::from_str(&secrets.text(CLOUD_CLI_OAUTH_TOKEN_SECRET).unwrap()).unwrap();
    assert_eq!(persisted, old);
    manager.clear().await.unwrap();
    assert!(!manager.has_credential().await.unwrap());
    assert!(manager.get_existing().await.unwrap().is_none());
}

#[tokio::test]
async fn a_stored_token_that_does_not_decode_is_a_read_error_for_status_and_a_refresh_error_for_use()
 {
    let secrets = Arc::new(MemorySecretStore::with(&[(
        CLOUD_CLI_OAUTH_TOKEN_SECRET,
        "garbage",
    )]));
    let manager = manager("http://127.0.0.1:1", secrets, FixedClock::at(NOW_MS));
    assert_eq!(
        manager.has_credential().await.unwrap_err(),
        CliTokenError::CredentialRead
    );
    assert!(matches!(
        manager.get_existing().await.unwrap_err(),
        CliTokenError::CredentialRefresh(_)
    ));
}

// ---- the device authorization grant ----------------------------------------------------------------

const DEVICE_CODE: &str = "device-code-1";

fn device_server(
    replies: Vec<(u16, Value)>,
) -> (
    impl Fn(&std::collections::BTreeMap<String, String>, &str) -> (u16, Value) + Send + Sync + 'static,
    Arc<Mutex<Vec<std::time::Instant>>>,
) {
    let times = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
    let seen = times.clone();
    (
        move |fields, path| {
            if path.ends_with("/oauth/device_authorization") {
                assert_eq!(
                    fields.get("client_id").map(String::as_str),
                    Some("oauth_client")
                );
                assert_eq!(
                    fields.get("scope").map(String::as_str),
                    Some("openid profile email offline_access")
                );
                return (
                    200,
                    json!({
                        "device_code": DEVICE_CODE, "user_code": "BCDF-GHJK", "verification_uri": "https://accounts.example.test/device",
                        "verification_uri_complete": "https://accounts.example.test/device?user_code=BCDF-GHJK",
                        "expires_in": 600, "interval": 0
                    }),
                );
            }
            seen.lock().unwrap().push(std::time::Instant::now());
            assert_eq!(
                fields.get("grant_type").map(String::as_str),
                Some(DEVICE_CODE_GRANT_TYPE)
            );
            assert_eq!(
                fields.get("device_code").map(String::as_str),
                Some(DEVICE_CODE)
            );
            let mut queue = queue.lock().unwrap();
            if queue.len() > 1 {
                queue.pop_front().unwrap()
            } else {
                queue.front().cloned().unwrap()
            }
        },
        times,
    )
}

fn oauth_error(error: &str) -> (u16, Value) {
    (400, json!({"error": error}))
}

fn granted() -> (u16, Value) {
    (
        200,
        token_body(
            "access-token-1",
            Some("refresh-token-1"),
            3600,
            Some("me@example.test"),
        ),
    )
}

async fn device_login(
    replies: Vec<(u16, Value)>,
    timing: DeviceTiming,
) -> (
    Result<(PersistedToken, Option<String>), CliTokenError>,
    Vec<std::time::Instant>,
    Vec<DeviceAuthorizationPrompt>,
) {
    let (handler, times) = device_server(replies);
    let server = token_server(handler).await;
    let mut manager = manager(
        &server.url,
        Arc::new(MemorySecretStore::new()),
        FixedClock::at(NOW_MS),
    );
    manager.device_timing = timing;
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let shown = prompts.clone();
    let result = within(
        manager
            .device_authorization_login(move |prompt| shown.lock().unwrap().push(prompt.clone())),
    )
    .await;
    let times = times.lock().unwrap().clone();
    let prompts = prompts.lock().unwrap().clone();
    (result, times, prompts)
}

const FAST: DeviceTiming = DeviceTiming {
    default_interval: Duration::from_millis(1),
    slow_down_increment: Duration::from_millis(40),
};

#[tokio::test]
async fn the_device_flow_shows_the_code_and_polls_until_clerk_grants_the_token() {
    let (result, times, prompts) = device_login(
        vec![
            oauth_error("authorization_pending"),
            oauth_error("authorization_pending"),
            granted(),
        ],
        FAST,
    )
    .await;
    let (token, identity) = result.unwrap();
    assert_eq!(token.access_token, "access-token-1");
    assert_eq!(token.refresh_token, "refresh-token-1");
    assert_eq!(token.expires_at_epoch_ms, NOW_MS + 3_600_000);
    assert_eq!(identity.as_deref(), Some("me@example.test"));
    assert_eq!(token.identity.as_deref(), Some("me@example.test"));
    assert_eq!(times.len(), 3);
    assert_eq!(prompts.len(), 1, "the prompt is shown once, before polling");
    assert_eq!(prompts[0].user_code, "BCDF-GHJK");
    assert_eq!(prompts[0].expires_in, Duration::from_secs(600));
    assert_eq!(
        prompts[0].verification_uri_complete.as_deref(),
        Some("https://accounts.example.test/device?user_code=BCDF-GHJK")
    );
}

#[tokio::test]
async fn slow_down_and_upstream_failures_widen_the_interval() {
    let (result, times, _) = device_login(
        vec![
            oauth_error("slow_down"),
            (503, json!({"error": "overloaded"})),
            granted(),
        ],
        FAST,
    )
    .await;
    result.unwrap();
    assert_eq!(times.len(), 3);
    // Each widening adds the increment; real elapsed time can only be longer than the minimum.
    assert!(
        times[1] - times[0] >= Duration::from_millis(40),
        "{:?}",
        times[1] - times[0]
    );
    assert!(
        times[2] - times[1] >= Duration::from_millis(80),
        "{:?}",
        times[2] - times[1]
    );
}

#[tokio::test]
async fn a_denied_expired_or_failed_device_flow_ends_with_the_matching_error() {
    let (result, _, _) = device_login(vec![oauth_error("access_denied")], FAST).await;
    assert_eq!(result.unwrap_err(), CliTokenError::AuthorizationDenied);
    assert_eq!(
        CliTokenError::AuthorizationDenied.to_string(),
        "T3 Connect authorization was denied in the browser."
    );
    let (result, _, _) = device_login(vec![oauth_error("expired_token")], FAST).await;
    assert_eq!(result.unwrap_err(), CliTokenError::AuthorizationTimeout);
    let (result, _, _) = device_login(
        vec![(
            400,
            json!({"error": "invalid_client", "error_description": "no such client"}),
        )],
        FAST,
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        CliTokenError::Authorization("no such client".into())
    );
    let (result, _, _) = device_login(vec![oauth_error("invalid_client")], FAST).await;
    assert_eq!(
        result.unwrap_err(),
        CliTokenError::Authorization("invalid_client".into())
    );
}

#[tokio::test]
async fn a_device_endpoint_that_is_not_enabled_fails_before_any_prompt() {
    let server = token_server(|_, path| {
        if path.ends_with("device_authorization") {
            (400, json!({"error": "unsupported_grant_type"}))
        } else {
            (500, json!({}))
        }
    })
    .await;
    let manager = manager(
        &server.url,
        Arc::new(MemorySecretStore::new()),
        FixedClock::at(NOW_MS),
    );
    let prompted = Arc::new(Mutex::new(false));
    let flag = prompted.clone();
    let error = within(manager.device_authorization_login(move |_| *flag.lock().unwrap() = true))
        .await
        .unwrap_err();
    assert!(matches!(error, CliTokenError::Authorization(_)));
    assert!(!*prompted.lock().unwrap());
}

// ---- the loopback flow ------------------------------------------------------------------------------

/// Plays the user: records the authorization URL, then "opens the browser" by calling the callback.
struct Browser {
    actions: Mutex<std::collections::VecDeque<LoginAction>>,
    urls: Mutex<Vec<String>>,
    launched: Mutex<Vec<String>>,
    launch_fails: bool,
    warnings: Mutex<Vec<String>>,
    callback: Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
}

#[async_trait]
impl LoginInteraction for Browser {
    fn show_authorization_url(&self, url: &str) {
        self.urls.lock().unwrap().push(url.to_owned());
    }
    async fn next_action(&self) -> LoginAction {
        let next = self.actions.lock().unwrap().pop_front();
        match next {
            Some(action) => action,
            None => std::future::pending().await,
        }
    }
    async fn launch_browser(&self, url: &str) -> Result<(), String> {
        self.launched.lock().unwrap().push(url.to_owned());
        if let Some(tx) = self.callback.lock().unwrap().as_ref() {
            let _ = tx.send(url.to_owned());
        }
        if self.launch_fails {
            Err("no browser".into())
        } else {
            Ok(())
        }
    }
    fn warn(&self, message: &str) {
        self.warnings.lock().unwrap().push(message.to_owned());
    }
}

fn browser(actions: Vec<LoginAction>, launch_fails: bool) -> Arc<Browser> {
    Arc::new(Browser {
        actions: Mutex::new(actions.into()),
        urls: Mutex::new(vec![]),
        launched: Mutex::new(vec![]),
        launch_fails,
        warnings: Mutex::new(vec![]),
        callback: Mutex::new(None),
    })
}

#[tokio::test]
async fn enter_opens_the_browser_and_h_switches_to_the_headless_flow() {
    let secrets = Arc::new(MemorySecretStore::new());
    // Port 0 would bind an ephemeral port the test cannot know; use a free one.
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let m = {
        let mut config = oauth("http://127.0.0.1:1");
        config.loopback_port = free;
        config.redirect_uri = format!("http://127.0.0.1:{free}/callback");
        CliTokenManager::new(
            secrets.clone(),
            FixedClock::at(NOW_MS),
            config,
            "https://app.t3.codes".into(),
        )
    };
    let user = browser(vec![LoginAction::OpenBrowser, LoginAction::Headless], true);
    let outcome = within(m.get(user.as_ref())).await.unwrap();
    assert_eq!(outcome, GetOutcome::HeadlessRequested);
    assert_eq!(
        user.launched.lock().unwrap().len(),
        1,
        "Enter opened the browser"
    );
    assert_eq!(
        user.launched.lock().unwrap()[0],
        user.urls.lock().unwrap()[0]
    );
    assert_eq!(
        user.warnings.lock().unwrap().len(),
        1,
        "a browser that will not open is only a warning"
    );
    assert!(secrets.text(CLOUD_CLI_OAUTH_TOKEN_SECRET).is_none());
    // The URL goes through the hosted /connect page and carries the loopback port.
    let url = url::Url::parse(&user.urls.lock().unwrap()[0]).unwrap();
    assert_eq!(url.path(), "/connect");
    let fragment: std::collections::BTreeMap<String, String> =
        url::form_urlencoded::parse(url.fragment().unwrap().as_bytes())
            .into_owned()
            .collect();
    assert_eq!(fragment["port"], free.to_string());
    assert_eq!(fragment["challenge"].len(), 43);
}

#[tokio::test]
async fn the_loopback_callback_with_the_right_state_completes_the_authorization_code_exchange() {
    let secrets = Arc::new(MemorySecretStore::new());
    let server = token_server(|fields, _| {
        assert_eq!(
            fields.get("grant_type").map(String::as_str),
            Some("authorization_code")
        );
        assert_eq!(fields.get("code").map(String::as_str), Some("the-code"));
        assert_eq!(
            fields.get("client_id").map(String::as_str),
            Some("oauth_client")
        );
        assert!(
            fields["redirect_uri"].starts_with("http://127.0.0.1:")
                && fields["redirect_uri"].ends_with("/callback")
        );
        assert_eq!(fields["code_verifier"].len(), 43);
        (
            200,
            token_body("fresh", Some("refresh"), 3600, Some("me@example.test")),
        )
    })
    .await;
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut config = oauth(&server.url);
    config.loopback_port = free;
    config.redirect_uri = format!("http://127.0.0.1:{free}/callback");
    let m = CliTokenManager::new(
        secrets.clone(),
        FixedClock::at(NOW_MS),
        config,
        "https://app.t3.codes".into(),
    );
    let user = browser(vec![LoginAction::OpenBrowser], false);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    *user.callback.lock().unwrap() = Some(tx);
    let login = {
        let user = user.clone();
        let m = Arc::new(m);
        let m2 = m.clone();
        tokio::spawn(async move { m2.get(user.as_ref()).await })
    };
    // The "browser" opened the URL: answer with a wrong state first, then the right one.
    let opened = within(rx.recv()).await.unwrap();
    let url = url::Url::parse(&opened).unwrap();
    let fragment: std::collections::BTreeMap<String, String> =
        url::form_urlencoded::parse(url.fragment().unwrap().as_bytes())
            .into_owned()
            .collect();
    let http = reqwest::Client::new();
    let callback = format!("http://127.0.0.1:{free}/callback");
    let bad = within(http.get(format!("{callback}?code=x&state=wrong")).send())
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert_eq!(
        bad.text().await.unwrap(),
        "Invalid T3 Connect authorization callback."
    );
    let missing = within(
        http.get(format!("{callback}?state={}", fragment["state"]))
            .send(),
    )
    .await
    .unwrap();
    assert_eq!(missing.status(), 400);
    let good = within(
        http.get(format!(
            "{callback}?code=the-code&state={}",
            fragment["state"]
        ))
        .send(),
    )
    .await
    .unwrap();
    assert_eq!(good.status(), 200);
    assert!(
        good.text()
            .await
            .unwrap()
            .contains("authorization complete")
    );
    let outcome = within(login).await.unwrap().unwrap();
    let GetOutcome::Authorized(token) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(token.access_token, "fresh");
    assert_eq!(token.identity.as_deref(), Some("me@example.test"));
    let persisted: PersistedToken =
        serde_json::from_str(&secrets.text(CLOUD_CLI_OAUTH_TOKEN_SECRET).unwrap()).unwrap();
    assert_eq!(persisted, token);
}

#[tokio::test]
async fn a_stored_credential_that_cannot_be_refreshed_falls_through_to_a_fresh_login() {
    let secrets = Arc::new(MemorySecretStore::new());
    stored(
        &secrets,
        &PersistedToken {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at_epoch_ms: NOW_MS - 1,
            identity: None,
        },
    );
    let server = token_server(|_, _| (400, json!({"error": "invalid_grant"}))).await;
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut config = oauth(&server.url);
    config.loopback_port = free;
    let m = CliTokenManager::new(
        secrets,
        FixedClock::at(NOW_MS),
        config,
        "https://app.t3.codes".into(),
    );
    let user = browser(vec![LoginAction::Headless], false);
    assert_eq!(
        within(m.get(user.as_ref())).await.unwrap(),
        GetOutcome::HeadlessRequested
    );
    assert_eq!(user.urls.lock().unwrap().len(), 1, "a login was started");
}
