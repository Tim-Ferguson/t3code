//! Managed Codex's ChatGPT sign-in against a fake issuer on a loopback socket (OpenAI is never contacted): the authorization-code flow
//! with PKCE through the local callback and through a client-delivered one, the identity token's checks, saved registrations and
//! tokens under the original's secret names, refresh and sign-out, and the profile moved between two environments. The browser is a plain
//! HTTP client that follows the issuer's redirect.
use axum::{Form, Router, extract::{Query, State}, response::{IntoResponse, Redirect}, routing::{get, post}};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::{Arc, Mutex}, time::Duration};
use t3_contracts::{ProviderAuthCallbackMode, ProviderAuthInteraction, ProviderAuthPhase, ProviderAuthState, ProviderCredentialOwner};
use t3_server::{
    codex_chatgpt_auth::{ChatGptAuth, MemorySecrets, Options, REQUIRED_SCOPE, Secrets, verify_id_token},
    provider_auth_flow::{AuthFlow, RoutedStop},
    test_support::HANG_BOUND,
};

struct Issuer {
    origin: String,
    key: EcdsaKeyPair,
    /// The authorize requests it saw, the form posts it answered and the revocations it took.
    authorized: Mutex<Vec<HashMap<String, String>>>,
    tokens: Mutex<Vec<HashMap<String, String>>>,
    revoked: Mutex<u32>,
    /// What the next token answer does: `None` is a success; otherwise the OAuth error, for a refresh.
    refuse_refresh: Mutex<Option<&'static str>>,
    revocation_status: Mutex<u16>,
    scope: Mutex<String>,
    subject: Mutex<&'static str>,
}
type Shared = Arc<Issuer>;

fn b64(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn id_token(issuer: &Issuer, client_id: &str, nonce: &str) -> String {
    let now = chrono::Utc::now().timestamp();
    let header = b64(json!({"alg":"ES256","kid":"k1","typ":"JWT"}).to_string());
    let claims = b64(json!({"iss":issuer.origin,"aud":client_id,"sub":*issuer.subject.lock().unwrap(),"email":"a@example.com","nonce":nonce,"iat":now,"exp":now + 3600}).to_string());
    let signed = format!("{header}.{claims}");
    let signature = issuer.key.sign(&ring::rand::SystemRandom::new(), signed.as_bytes()).unwrap();
    format!("{signed}.{}", b64(signature.as_ref()))
}

async fn start_issuer() -> Shared {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let issuer = Arc::new(Issuer { origin: origin.clone(), key, authorized: Default::default(), tokens: Default::default(), revoked: Default::default(), refuse_refresh: Default::default(), revocation_status: Mutex::new(200), scope: Mutex::new(format!("openid offline_access {REQUIRED_SCOPE}")), subject: Mutex::new("user-1") });
    let router = Router::new()
        .route("/.well-known/openid-configuration", get(|State(issuer): State<Shared>| async move { axum::Json(json!({"issuer":issuer.origin,"authorization_endpoint":format!("{}/authorize", issuer.origin),"token_endpoint":format!("{}/token", issuer.origin),"jwks_uri":format!("{}/jwks", issuer.origin),"revocation_endpoint":format!("{}/revoke", issuer.origin)})) }))
        .route("/jwks", get(|State(issuer): State<Shared>| async move {
            let public = issuer.key.public_key().as_ref();
            axum::Json(json!({"keys":[{"kty":"EC","crv":"P-256","kid":"k1","use":"sig","alg":"ES256","x":b64(&public[1..33]),"y":b64(&public[33..65])}]}))
        }))
        // The "browser" arrives here with the authorization request and is sent back to the redirect with a code.
        .route("/authorize", get(|State(issuer): State<Shared>, Query(query): Query<HashMap<String, String>>| async move {
            // A request without a redirect is only noted: it is the client-delivered flow, whose browser never reaches this server.
            let redirect = query.get("redirect_uri").map(|redirect| format!("{redirect}?code=good-code&state={}&client_id=oaiapp_test", query["state"]));
            issuer.authorized.lock().unwrap().push(query);
            match redirect {
                Some(redirect) => Redirect::to(&redirect).into_response(),
                None => "noted".into_response(),
            }
        }))
        .route("/token", post(|State(issuer): State<Shared>, Form(form): Form<HashMap<String, String>>| async move {
            let nonce = issuer.authorized.lock().unwrap().last().and_then(|query| query.get("nonce").cloned()).unwrap_or_default();
            issuer.tokens.lock().unwrap().push(form.clone());
            let calls = issuer.tokens.lock().unwrap().len();
            let scope = issuer.scope.lock().unwrap().clone();
            match form["grant_type"].as_str() {
                "authorization_code" if form["code"] == "good-code" => {
                    // PKCE: the verifier must hash to the challenge the browser carried.
                    let challenge = issuer.authorized.lock().unwrap().last().unwrap()["code_challenge"].clone();
                    assert_eq!(b64(Sha256::digest(form["code_verifier"].as_bytes())), challenge);
                    axum::Json(json!({"access_token":"access-1","refresh_token":"refresh-1","id_token":id_token(&issuer, &form["client_id"], &nonce),"token_type":"Bearer","expires_in":3600,"scope":scope})).into_response()
                }
                "authorization_code" => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error":"invalid_grant"}))).into_response(),
                _ => match *issuer.refuse_refresh.lock().unwrap() {
                    Some(error) => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error":error}))).into_response(),
                    None => axum::Json(json!({"access_token":format!("access-{calls}"),"refresh_token":format!("refresh-{calls}"),"token_type":"bearer","expires_in":3600,"scope":scope,"earliest_refresh_at":0})).into_response(),
                },
            }
        }))
        // The account's model catalog, which managed Codex lists its models from.
        .route("/v1/models", get(|headers: axum::http::HeaderMap| async move {
            assert!(headers["authorization"].to_str().unwrap().starts_with("Bearer access-"), "the catalog is read with the account's token");
            axum::Json(json!({"models":[{"slug":"fixture-model","display_name":"Fixture Model","visibility":"list"},{"slug":"hidden-model","display_name":"Hidden","visibility":"hide"}]}))
        }))
        .route("/revoke", post(|State(issuer): State<Shared>| async move {
            *issuer.revoked.lock().unwrap() += 1;
            axum::http::StatusCode::from_u16(*issuer.revocation_status.lock().unwrap()).unwrap()
        }))
        .with_state(issuer.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    issuer
}

fn clock_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn auth_over(issuer: &Issuer, secrets: Arc<dyn Secrets>, now: Arc<dyn Fn() -> i64 + Send + Sync>) -> ChatGptAuth {
    ChatGptAuth::new(Options { instance_id: "codex".into(), environment_id: "environment-1".into(), discovery_url: Some(format!("{}/.well-known/openid-configuration", issuer.origin)), resource: None, default_return_url: Some("http://localhost:5173/welcome".into()), reconnect_profile: None, secrets, http: reqwest::Client::new(), clock: now }).unwrap()
}

fn flow_of(auth: &ChatGptAuth) -> AuthFlow {
    AuthFlow::new("codex".into(), Arc::new(auth.clone()), None, Duration::from_secs(60), ProviderCredentialOwner::T3, true)
}

async fn states_until(subscription: &mut t3_server::provider_auth_flow::AuthSubscription, done: impl Fn(&ProviderAuthState) -> bool) -> ProviderAuthState {
    tokio::time::timeout(HANG_BOUND, async {
        loop {
            let state = subscription.recv().await.expect("the flow ended");
            if done(&state) {
                return state;
            }
        }
    })
    .await
    .unwrap()
}

/// Starts a sign-in as `method` and returns the authorization URL the browser is sent to.
async fn begin(flow: &AuthFlow, method: &str, mode: ProviderAuthCallbackMode) -> (t3_server::provider_auth_flow::AuthSubscription, String, String) {
    let mut subscription = flow.subscribe("owner".into());
    flow.start("owner".into(), Some(method.into()), Some("http://localhost:5173/settings/providers".into()), Some(mode), RoutedStop::new(Arc::new(|| Box::pin(async { Ok(()) })))).await.unwrap();
    let waiting = states_until(&mut subscription, |state| state.phase == ProviderAuthPhase::Waiting).await;
    let Some(Some(Some(ProviderAuthInteraction::Browser { url, accepts_callback, .. }))) = waiting.interaction.clone() else { panic!("{waiting:?}") };
    assert_eq!(accepts_callback, Some(true));
    (subscription, url.0.to_string(), waiting.flow_id.unwrap().0.to_string())
}

fn query_of(url: &str) -> HashMap<String, String> {
    url::Url::parse(url).unwrap().query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())).collect()
}

async fn browser(url: &str) -> reqwest::Response {
    reqwest::Client::new().get(url).send().await.unwrap()
}

#[tokio::test]
async fn a_sign_in_through_the_local_callback_saves_a_registration_and_tokens_and_a_second_one_reuses_them() {
    let issuer = start_issuer().await;
    let secrets: Arc<MemorySecrets> = Default::default();
    let auth = auth_over(&issuer, secrets.clone(), Arc::new(clock_ms));
    let flow = flow_of(&auth);
    let (mut subscription, url, _) = begin(&flow, "chatgpt", ProviderAuthCallbackMode::Server).await;

    // The request a new environment makes: a dynamic client, this environment named as the host, PKCE, no hints yet.
    let first = query_of(&url);
    assert_eq!((first["client_id"].as_str(), first["agent_name_hint"].as_str(), first["response_type"].as_str(), first["code_challenge_method"].as_str()), ("dynamic_agent_client", "T3 Code", "code", "S256"));
    assert_eq!(first["ext_agent_host_id"], "urn:uuid:environment-1");
    assert_eq!(first["scope"], "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct");
    assert!(first["redirect_uri"].starts_with("http://127.0.0.1:") && first["redirect_uri"].ends_with("/auth/callback") && !first.contains_key("login_hint"));

    // The browser goes to the issuer, comes back to the local listener, and is told it is signed in.
    let page = browser(&url).await;
    assert_eq!(page.status(), 200);
    assert_eq!(page.headers()["cache-control"], "no-store");
    assert!(page.headers()["content-security-policy"].to_str().unwrap().contains("script-src 'nonce-"));
    let body = page.text().await.unwrap();
    assert!(body.contains("You&#39;re signed in") && body.contains("Return to T3 Code"), "{body}");
    states_until(&mut subscription, |state| state.phase == ProviderAuthPhase::Succeeded).await;

    let credentials = auth.read().unwrap().unwrap();
    assert_eq!((credentials.client_id.as_str(), credentials.access_token.as_str(), credentials.refresh_token.as_deref(), credentials.subject.as_str(), credentials.email.as_deref()), ("oaiapp_test", "access-1", Some("refresh-1"), "user-1", Some("a@example.com")));
    assert_eq!(credentials.issuer.as_deref(), Some(issuer.origin.as_str()));
    // The tokens and the registration sit under the names the original gives them.
    let key = |driver: &str| format!("provider-auth-{}", Sha256::digest(format!("{}:{driver}codex", driver.len()).as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>());
    let saved: Value = serde_json::from_slice(&secrets.get(&key("codex-chatgpt")).unwrap().unwrap()).unwrap();
    assert_eq!((saved["activeClientId"].as_str(), saved["sessions"][0]["clientId"].as_str()), (Some("oaiapp_test"), Some("oaiapp_test")));
    let registrations: Value = serde_json::from_slice(&secrets.get(&key("codex-chatgpt-registration")).unwrap().unwrap()).unwrap();
    assert_eq!((registrations["lastClientId"].as_str(), registrations["profiles"][0]["subject"].as_str(), registrations["profiles"][0]["connectionLabel"].as_str(), registrations["profiles"][0]["sharingEnabled"].as_bool()), (Some("oaiapp_test"), Some("user-1"), Some("Connection 1"), Some(true)));

    // The next sign-in reuses the registration: its client, the address of the account, the identity token as a hint.
    let (mut again, url, _) = begin(&flow, "chatgpt", ProviderAuthCallbackMode::Server).await;
    let second = query_of(&url);
    assert_eq!((second["client_id"].as_str(), second["login_hint"].as_str(), second.contains_key("agent_name_hint"), second.contains_key("prompt")), ("oaiapp_test", "a@example.com", false, false));
    assert!(second.contains_key("id_token_hint"));
    // A sign-in that returns another account is refused, and the saved connection stays.
    *issuer.subject.lock().unwrap() = "user-2";
    assert_eq!(browser(&url).await.text().await.unwrap().contains("couldn&#39;t finish"), true);
    let failed = states_until(&mut again, |state| state.phase == ProviderAuthPhase::Failed).await;
    assert!(failed.message.as_deref().unwrap().starts_with("This sign-in returned a different ChatGPT account."), "{failed:?}");
    assert_eq!(auth.read().unwrap().unwrap().subject, "user-1");
    // The menu offers the saved account, a different one and the profile.
    let methods = auth.methods().unwrap();
    let ids: Vec<&str> = methods.iter().map(|method| method.id.0.as_str()).collect();
    assert_eq!(ids, ["chatgpt", "chatgpt-change-account", "chatgpt-profile:oaiapp_test"]);
    assert_eq!(methods[0].description.as_deref(), Some("Reconnect a@example.com."));
    flow.shutdown().await;
}

#[tokio::test]
async fn a_callback_the_client_brings_is_held_to_the_same_checks_and_a_declined_sign_in_says_so() {
    let issuer = start_issuer().await;
    let auth = auth_over(&issuer, Arc::new(MemorySecrets::default()), Arc::new(clock_ms));
    let flow = flow_of(&auth);
    let (mut subscription, url, flow_id) = begin(&flow, "chatgpt", ProviderAuthCallbackMode::Client).await;
    let request = query_of(&url);
    // The client sends back what its browser received: wrong state, then the right one (once).
    let complete = |callback: String| {
        let flow = flow.clone();
        let flow_id = flow_id.clone();
        async move { flow.complete("owner", serde_json::from_value(json!({"instanceId":"codex","flowId":flow_id,"callbackUrl":callback})).unwrap()).await }
    };
    let redirect = &request["redirect_uri"];
    let wrong = complete(format!("{redirect}?code=good-code&state=other&client_id=oaiapp_test")).await.unwrap_err();
    assert_eq!(wrong.detail, "This redirect URL does not belong to the active ChatGPT sign-in.");
    // The issuer needs the request to answer with its nonce.
    browser(&url.replace("redirect_uri=", "unused=")).await;
    let callback = format!("{redirect}?code=good-code&state={}&client_id=oaiapp_test", request["state"]);
    complete(callback.clone()).await.unwrap();
    assert_eq!(complete(callback).await.unwrap_err().detail, "This redirect URL does not belong to the active ChatGPT sign-in.");
    states_until(&mut subscription, |state| state.phase == ProviderAuthPhase::Succeeded).await;
    assert_eq!(auth.read().unwrap().unwrap().access_token, "access-1");

    // A user who declines is told so.
    let (mut declined, url, _) = begin(&flow, "chatgpt-change-account", ProviderAuthCallbackMode::Server).await;
    let request = query_of(&url);
    let page = browser(&format!("{}?error=access_denied&state={}", request["redirect_uri"], request["state"])).await.text().await.unwrap();
    assert!(page.contains("couldn&#39;t finish"));
    let state = states_until(&mut declined, |state| state.phase == ProviderAuthPhase::Failed).await;
    assert_eq!(state.message.as_deref(), Some("ChatGPT sign-in was declined. Sign in again when you are ready."));
    flow.shutdown().await;
}

async fn signed_in(issuer: &Shared, secrets: Arc<dyn Secrets>, now: Arc<dyn Fn() -> i64 + Send + Sync>) -> ChatGptAuth {
    let auth = auth_over(issuer, secrets, now);
    let flow = flow_of(&auth);
    let (mut subscription, url, _) = begin(&flow, "chatgpt", ProviderAuthCallbackMode::Server).await;
    browser(&url).await;
    states_until(&mut subscription, |state| state.phase == ProviderAuthPhase::Succeeded).await;
    flow.shutdown().await;
    auth
}

#[tokio::test]
async fn tokens_are_renewed_when_they_lapse_and_a_refused_renewal_signs_out() {
    let issuer = start_issuer().await;
    // The clock is the test's: tokens last an hour from sign-in, and then it is later.
    let offset = Arc::new(Mutex::new(0i64));
    let now: Arc<dyn Fn() -> i64 + Send + Sync> = { let offset = offset.clone(); Arc::new(move || clock_ms() + *offset.lock().unwrap()) };
    let auth = signed_in(&issuer, Arc::new(MemorySecrets::default()), now).await;
    assert_eq!(auth.access().await.unwrap().access_token, "access-1", "fresh tokens are not renewed");
    *offset.lock().unwrap() = 3_550_000;
    let renewed = auth.access().await.unwrap();
    assert_eq!((renewed.access_token.as_str(), renewed.refresh_token.as_deref()), ("access-2", Some("refresh-2")));
    let refresh = issuer.tokens.lock().unwrap().last().unwrap().clone();
    assert_eq!((refresh["grant_type"].as_str(), refresh["refresh_token"].as_str(), refresh["client_id"].as_str()), ("refresh_token", "refresh-1", "oaiapp_test"));
    *offset.lock().unwrap() = 7_100_000;
    *issuer.refuse_refresh.lock().unwrap() = Some("invalid_grant");
    // The connection is forgotten, and the answer is the generic one: the original maps every renewal failure but a rejected client to it.
    let error = auth.access().await.unwrap_err();
    assert_eq!((error.operation.as_str(), error.detail.as_str()), ("refresh", "Could not renew the ChatGPT connection. Retry, or sign in again."));
    assert!(auth.read().unwrap().is_none(), "the connection is forgotten");
    assert_eq!(auth.access().await.unwrap_err().detail, "Sign in with ChatGPT to use managed Codex.");
    // The registration stays, so the next sign-in is the same account's.
    assert_eq!(auth.methods().unwrap().len(), 3);
}

#[tokio::test]
async fn signing_out_revokes_with_the_issuer_and_says_when_it_could_not() {
    let issuer = start_issuer().await;
    let auth = signed_in(&issuer, Arc::new(MemorySecrets::default()), Arc::new(clock_ms)).await;
    assert_eq!(auth.logout().await.unwrap(), None);
    assert_eq!(*issuer.revoked.lock().unwrap(), 1);
    assert!(auth.read().unwrap().is_none());

    let auth = signed_in(&issuer, Arc::new(MemorySecrets::default()), Arc::new(clock_ms)).await;
    *issuer.revocation_status.lock().unwrap() = 503;
    let warning = auth.logout().await.unwrap();
    assert_eq!(warning.as_deref(), Some("Signed out locally. Remote revocation could not be confirmed. Disconnect the app in ChatGPT Settings."));
    assert_eq!(*issuer.revoked.lock().unwrap(), 1 + 3, "a server error is tried three times");
    assert!(auth.read().unwrap().is_none());
}

/// A profile imported through a flow that has no sessions to end.
async fn import(auth: &ChatGptAuth, profile: &t3_contracts::ChatGptTransferredProfile) -> Result<ProviderAuthState, t3_contracts::ProviderSetupError> {
    let flow = flow_of(auth);
    let state = auth.import_profile(&flow, profile.clone(), Box::pin(async { Ok(()) })).await;
    flow.shutdown().await;
    state
}

#[tokio::test]
async fn a_profile_moves_to_another_environment_only_if_it_proves_itself() {
    let issuer = start_issuer().await;
    let primary = signed_in(&issuer, Arc::new(MemorySecrets::default()), Arc::new(clock_ms)).await;
    let exported = primary.export_profile().unwrap();
    let destination = auth_over(&issuer, Arc::new(MemorySecrets::default()), Arc::new(clock_ms));
    import(&destination, &exported).await.unwrap();
    assert_eq!(destination.read().unwrap().unwrap().access_token, "access-1");
    assert_eq!(destination.access().await.unwrap().subject, "user-1");
    // The registration the primary would hand over again carries the identity hint.
    let reconnect = destination.reconnect_profile("chatgpt").unwrap().unwrap();
    assert_eq!((reconnect.client_id.as_str(), reconnect.email.clone(), reconnect.id_token_hint.is_some()), ("oaiapp_test", Some(Some("a@example.com".to_owned())), true));
    assert!(destination.reconnect_profile("chatgpt-change-account").unwrap().is_none());
    assert_eq!(destination.reconnect_profile("chatgpt-profile:nope").unwrap_err().detail, "This saved connection is no longer available.");

    // A profile whose identity does not match, whose issuer is another, whose sharing is off or that has lapsed is refused.
    let mutate = |change: &dyn Fn(&mut Value)| -> t3_contracts::ChatGptTransferredProfile {
        let mut value = serde_json::to_value(&exported).unwrap();
        change(&mut value);
        serde_json::from_value(value).unwrap()
    };
    let cases: Vec<(&str, t3_contracts::ChatGptTransferredProfile, &str)> = vec![
        ("another subject", mutate(&|value| { value["credentials"]["subject"] = json!("user-9"); value["registration"]["subject"] = json!("user-9"); }), "The transferred ChatGPT identity does not match its profile."),
        ("another email", mutate(&|value| value["credentials"]["email"] = json!("b@example.com")), "The transferred ChatGPT identity does not match its profile."),
        ("another issuer", mutate(&|value| value["credentials"]["issuer"] = json!("https://evil.example")), "The transferred ChatGPT connection is invalid or expired."),
        ("no sharing", mutate(&|value| value["credentials"]["scopes"] = json!(["openid"])), "The transferred ChatGPT connection is invalid or expired."),
        ("lapsed", mutate(&|value| value["credentials"]["expiresAt"] = json!(1)), "The transferred ChatGPT connection is invalid or expired."),
        ("another client", mutate(&|value| value["registration"]["clientId"] = json!("oaiapp_other")), "The transferred ChatGPT connection is invalid or expired."),
        ("a forged identity token", mutate(&|value| { let token = value["credentials"]["idToken"].as_str().unwrap().to_owned(); value["credentials"]["idToken"] = json!(format!("{}x", token)); }), "The transferred ChatGPT identity could not be verified."),
    ];
    for (name, profile, detail) in cases {
        let fresh = auth_over(&issuer, Arc::new(MemorySecrets::default()), Arc::new(clock_ms));
        assert_eq!(import(&fresh, &profile).await.unwrap_err().detail, detail, "{name}");
        assert!(fresh.read().unwrap().is_none(), "{name}: nothing is kept");
    }
    // A profile for a client this environment already knows as another account conflicts.
    let known = auth_over(&issuer, Arc::new(MemorySecrets::default()), Arc::new(clock_ms));
    import(&known, &exported).await.unwrap();
    assert!(import(&known, &exported).await.is_ok(), "the same account again is fine");
}

#[test]
fn identity_tokens_are_checked_as_jose_checks_them() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/chatgpt-rs256.json")).unwrap();
    let (token, jwks, issuer, audience) = (fixture["token"].as_str().unwrap(), &fixture["jwks"], fixture["issuer"].as_str().unwrap(), fixture["audience"].as_str().unwrap());
    let now = 4_102_444_100;
    let claims = verify_id_token(token, jwks, issuer, audience, now).unwrap();
    assert_eq!((claims["sub"].as_str(), claims["nonce"].as_str()), (Some("subject-1"), Some("n-1")));
    assert_eq!(verify_id_token(token, &fixture["otherJwks"], issuer, audience, now).unwrap_err(), "signature");
    assert_eq!(verify_id_token(token, jwks, "https://other.example", audience, now).unwrap_err(), "issuer");
    assert_eq!(verify_id_token(token, jwks, issuer, "client-2", now).unwrap_err(), "audience");
    assert_eq!(verify_id_token(token, jwks, issuer, audience, 4_102_444_806).unwrap_err(), "expired", "five seconds of tolerance, not more");
    assert!(verify_id_token(token, jwks, issuer, audience, 4_102_444_804).is_ok());
    let mut parts: Vec<&str> = token.split('.').collect();
    let tampered = b64(json!({"iss":issuer,"aud":audience,"sub":"subject-2","exp":4_102_444_800i64}).to_string());
    parts[1] = &tampered;
    assert_eq!(verify_id_token(&parts.join("."), jwks, issuer, audience, now).unwrap_err(), "signature");
    let none = format!("{}.{}.", b64(json!({"alg":"none"}).to_string()), parts[1]);
    assert_eq!(verify_id_token(&none, jwks, issuer, audience, now).unwrap_err(), "algorithm");
    assert_eq!(verify_id_token("a.b", jwks, issuer, audience, now).unwrap_err(), "malformed");
}

// ---- over the socket ---------------------------------------------------------------------------------------------------------------

mod rpc_support;
use rpc_support::{Harness, failure, harness_with_config, next, rpc, send, success};

async fn managed(h: &Harness, issuer: &Issuer) -> String {
    h.settings.as_ref().unwrap().update(serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":true,"config":{"setupMode":"managed"}}}})).unwrap()).await.unwrap();
    t3_server::codex_chatgpt_rpc::install(&h.store, t3_server::codex_chatgpt_rpc::Environment { environment_id: "environment-1".into(), default_return_url: None, discovery_url: Some(format!("{}/.well-known/openid-configuration", issuer.origin)), resource: None, models_url: None, http: reqwest::Client::new() });
    h.auth.issue_session("manager", "bearer-access-token", vec![t3_contracts::AuthEnvironmentScope::OrchestrationRead, t3_contracts::AuthEnvironmentScope::ProvidersManage], json!({"deviceType":"unknown"}), chrono::Utc::now(), chrono::Duration::hours(1)).unwrap().1
}

#[tokio::test]
async fn the_primary_signs_in_for_a_remote_environment_which_then_keeps_the_profile() {
    let issuer = start_issuer().await;
    let h = harness_with_config().await;
    let manager = managed(&h, &issuer).await;
    let mut socket = h.connect(&manager).await;

    // Only a managed Codex has a ChatGPT sign-in.
    let refused = rpc(&mut socket, 1, "provider.chatgpt.reconnect-profile", json!({"instanceId":"claudeAgent","methodId":"chatgpt"})).await;
    assert_eq!(failure(&refused)["detail"], "This provider does not support sign-in in T3 Code.");
    let unknown = rpc(&mut socket, 2, "provider.chatgpt.reconnect-profile", json!({"instanceId":"nobody","methodId":"chatgpt"})).await;
    assert_eq!(failure(&unknown)["detail"], "This provider instance is no longer available.");
    // Nothing is registered yet.
    assert_eq!(success(&rpc(&mut socket, 3, "provider.chatgpt.reconnect-profile", json!({"instanceId":"codex","methodId":"chatgpt"})).await), &Value::Null);

    // The handoff: this environment runs the sign-in, the "browser" completes it, and the profile comes back as the last item.
    send(&mut socket, json!({"_tag":"Request","id":4,"tag":"provider.chatgpt.handoff.subscribe","payload":{"instanceId":"codex","environmentId":"environment-2","attemptId":"attempt-1","returnUrl":"http://localhost:5173/settings/providers","profile":null},"headers":[]})).await;
    let mut authorization_url = None;
    let profile = tokio::time::timeout(HANG_BOUND, async {
        loop {
            let message = next(&mut socket).await;
            match message["_tag"].as_str() {
                Some("Chunk") => {
                    for value in message["values"].as_array().unwrap() {
                        if value["phase"] == "finished" {
                            return value["profile"].clone();
                        }
                        if let Some(url) = value["state"]["authorizationUrl"].as_str() {
                            if authorization_url.is_none() {
                                authorization_url = Some(url.to_owned());
                                let url = url.to_owned();
                                tokio::spawn(async move { browser(&url).await });
                            }
                        }
                    }
                    send(&mut socket, json!({"_tag":"Ack","requestId":4})).await;
                }
                Some("Exit") => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    // OpenAI was told which environment the connection is for: the one the primary named, not this one.
    assert_eq!(query_of(&authorization_url.unwrap())["ext_agent_host_id"], "urn:uuid:environment-2");
    assert_eq!((profile["registration"]["clientId"].as_str(), profile["credentials"]["subject"].as_str()), (Some("oaiapp_test"), Some("user-1")));

    // The destination imports it and hands the registration back when asked.
    let imported = rpc(&mut socket, 5, "provider.chatgpt.import-profile", json!({"instanceId":"codex","profile":profile})).await;
    let state = success(&imported);
    assert_eq!((state["phase"].as_str(), state["credentialOwner"].as_str()), (Some("succeeded"), Some("t3")));
    assert_eq!(state["methods"][0]["id"], "chatgpt");
    let reconnect = rpc(&mut socket, 6, "provider.chatgpt.reconnect-profile", json!({"instanceId":"codex","methodId":"chatgpt"})).await;
    assert_eq!((success(&reconnect)["clientId"].as_str(), success(&reconnect)["email"].as_str()), (Some("oaiapp_test"), Some("a@example.com")));
    assert!(success(&reconnect)["idTokenHint"].is_string());
    // A profile the issuer does not vouch for is refused with the reason.
    let mut forged = profile.clone();
    forged["credentials"]["subject"] = json!("user-9");
    forged["registration"]["subject"] = json!("user-9");
    let refused = rpc(&mut socket, 7, "provider.chatgpt.import-profile", json!({"instanceId":"codex","profile":forged})).await;
    assert_eq!(failure(&refused)["detail"], "The transferred ChatGPT identity does not match its profile.");
    // The destination's tokens are in its secret store under the original's names, not on any other disk path.
    assert!(std::fs::read_dir(h.base.join("userdata").join("secrets")).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with("provider-auth-")));
    h.finish().await;
}

// ---- a session on the signed-in tokens --------------------------------------------------------------------------------------------

/// Managed Codex end to end against the fake issuer and a fake `codex` on PATH: the provider list follows the sign-in, a session starts
/// the executable with the access token and the token-sharing launch configuration in a shadow home, and a connection imported from
/// another environment ends that session and refuses to arrive in the middle of a sign-in.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_managed_codex_session_runs_on_the_signed_in_tokens_until_a_new_connection_replaces_them() {
    use chrono::Utc;
    use t3_server::{
        codex_chatgpt_rpc::Environment, codex_managed::ChatGptHost, execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService,
        provider_auth_service::{ProviderAuthChanges, ProviderAuthService}, provider_manage::Management, provider_registry::ProviderRegistry,
    };
    use std::os::unix::fs::PermissionsExt;

    let issuer = start_issuer().await;
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let (state, shared, workspace, bin) = (base.join("state"), base.join("shared"), base.join("workspace"), base.join("bin"));
    for path in [&state, &workspace, &bin] {
        std::fs::create_dir_all(path).unwrap();
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let executable = bin.join("codex");
    std::fs::write(&executable, format!("#!/bin/sh\ncase \"$1\" in\n  --version) echo 'codex-cli 0.156.1' ;;\n  app-server) if [ \"$2\" = \"--help\" ]; then exit 0; fi; exec python3 '{}' \"$@\" ;;\nesac\n", fixture.display())).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (launches, requests) = (base.join("launches.jsonl"), base.join("requests.jsonl"));

    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},"codex":{"driver":"codex","enabled":false},
        "work":{"driver":"codex","enabled":true,"config":{"setupMode":"managed","homePath":shared},"environment":[{"name":"FIXTURE_LAUNCH_RECORD","value":launches},{"name":"FIXTURE_RPC_RECORD","value":requests},{"name":"OPENAI_API_KEY","value":"sk-ambient"}]}}})).unwrap();
    let management = Management::new(Some(base.join("tools")));
    management.configure(|options| options.codex_path = Some(bin.to_string_lossy().into_owned()));
    let environment = Environment { environment_id: "environment-1".into(), default_return_url: None, discovery_url: Some(format!("{}/.well-known/openid-configuration", issuer.origin)), resource: None, models_url: Some(format!("{}/v1/models", issuer.origin)), http: reqwest::Client::new() };
    management.set_chatgpt(ChatGptHost { environment: Arc::new(environment), state_dir: state.clone() });
    let providers = ProviderRegistry::discover_with_management(&settings, &base, None, management).await.unwrap();
    let work = |providers: &ProviderRegistry| providers.snapshots().into_iter().find(|snapshot| snapshot["instanceId"] == "work").unwrap();

    // Installed, and nobody signed in.
    let snapshot = work(&providers);
    assert_eq!((snapshot["installed"].clone(), snapshot["auth"]["status"].clone(), snapshot["setup"]["canAuthenticate"].clone(), snapshot["message"].clone()), (json!(true), json!("unauthenticated"), json!(true), json!("Sign in with ChatGPT to use Codex.")), "{snapshot}");
    assert_eq!(snapshot["runtimePaths"], json!({"homePath":shared,"shadowHomePath":state.join("providers/codex/work/shadow")}));

    let store = Store::open(state.join("rust-state.sqlite")).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let service = ProviderAuthService::for_server(&execution, &providers, base.clone(), &state);

    // Signing in through the service: the provider list follows it.
    let mut changes = providers.subscribe_changes();
    let mut states: ProviderAuthChanges = service.subscribe("work", "owner".into()).await.unwrap();
    let start = |id: &str| serde_json::from_value::<t3_contracts::ProviderAuthStartInput>(json!({"instanceId":id,"methodId":"chatgpt","returnUrl":"http://localhost:5173/settings","callbackMode":"server"})).unwrap();
    service.start(start("work"), "owner".into()).await.unwrap();
    let waiting = phase_of(&mut states, ProviderAuthPhase::Waiting).await;
    let Some(Some(Some(ProviderAuthInteraction::Browser { url, .. }))) = waiting.interaction.clone() else { panic!("{waiting:?}") };
    browser(url.0.as_str()).await;
    phase_of(&mut states, ProviderAuthPhase::Succeeded).await;
    let signed_in = tokio::time::timeout(HANG_BOUND, async {
        loop {
            let snapshot = work(&providers);
            if snapshot["auth"]["status"] == "authenticated" && snapshot["models"].as_array().is_some_and(|models| !models.is_empty()) {
                return snapshot;
            }
            changes.recv().await.expect("the provider list is served");
        }
    })
    .await
    .unwrap();
    assert_eq!((signed_in["auth"]["type"].clone(), signed_in["auth"]["email"].clone(), signed_in["auth"]["subscriptionSharing"].clone(), signed_in["auth"]["profileId"].clone()), (json!("chatgpt"), json!("a@example.com"), json!(true), json!("oaiapp_test")), "{signed_in}");
    assert_eq!(signed_in["models"].as_array().unwrap().iter().map(|model| (model["slug"].as_str().unwrap(), model["name"].as_str().unwrap())).collect::<Vec<_>>(), [("fixture-model", "Fixture Model")], "only the account's visible models");
    assert_eq!(signed_in["usageLimits"]["externalUsage"]["label"], "ChatGPT usage");

    let primary = signed_in_primary(&issuer).await;
    let profile = serde_json::to_value(primary.export_profile().unwrap()).unwrap();
    let import = || serde_json::from_value::<t3_contracts::ChatGptImportProfileInput>(json!({"instanceId":"work","profile":profile})).unwrap();
    let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
        .launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":"work","model":"fixture-model","options":[{"id":"serviceTier","value":"fast"}]},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap()["threadId"].as_str().unwrap().to_owned();
    let view = |store: &Store| store.projection("thread", &thread).unwrap().unwrap();
    let settled = |runs: usize| {
        let (store, thread) = (store.clone(), thread.clone());
        async move {
            let mut events = store.subscribe();
            tokio::time::timeout(HANG_BOUND, async {
                loop {
                    let view = store.projection("thread", &thread).unwrap().unwrap();
                    if view["runs"].as_array().unwrap().len() >= runs && view["runs"].as_array().unwrap().iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))) {
                        return view;
                    }
                    let _ = events.recv().await;
                }
            })
            .await
            .expect("the run settles")
        }
    };
    let send = |text: &str, id: &str| execution.dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":id,"text":text,"attachments":[],"dispatchMode":{"type":"start_immediately"}}), Utc::now()).unwrap();

    // The session runs the executable on the access token, in the shadow home, with the token-sharing provider.
    send("after sign-in", "m1");
    let second = settled(1).await;
    assert_eq!(second["runs"][0]["status"], "completed", "{}", second["turnItems"]);
    let launched = |path: &std::path::Path| std::fs::read_to_string(path).unwrap_or_default().lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).collect::<Vec<_>>();
    let shadow = state.join("providers/codex/work/shadow");
    let records = launched(&launches);
    assert!(!records.is_empty());
    for record in &records {
        assert_eq!((record["accessToken"].as_str(), record["codexHome"].as_str()), (Some("access-1"), shadow.to_str()), "{record}");
        assert!(record["argv"].as_array().unwrap().windows(2).any(|pair| pair[0] == "-c" && pair[1] == "model_provider=\"openai_token_sharing\""), "{record}");
    }
    // The server's own environment is not what the instance configured: the instance's key is removed from the child's.
    assert!(records.iter().all(|record| record["openaiKey"].is_null()), "{records:?}");
    assert_eq!(std::fs::read_link(shadow.join("sessions")).unwrap(), shared.join("sessions"));
    assert!(!shadow.join("auth.json").exists(), "the tokens are not written to Codex's own auth file");
    let turn_start = launched(&requests).into_iter().find(|request| request["method"] == "turn/start").expect("a turn started");
    assert!(turn_start["params"].get("serviceTier").is_none(), "ChatGPT token sharing takes no service tier: {turn_start}");

    // A connection signed in elsewhere replaces these tokens and ends the session that runs on them.
    let before = launched(&launches).len();
    let state_after = service.import_chatgpt_profile(import()).await.unwrap();
    assert_eq!((state_after.phase, state_after.message), (ProviderAuthPhase::Succeeded, None));
    let stopped = view(&store)["providerSessions"].as_array().unwrap().iter().filter(|session| session["status"] != "stopped" && session["status"] != "error").count();
    assert_eq!(stopped, 0, "no session is left on the old tokens: {}", view(&store)["providerSessions"]);
    send("after import", "m2");
    let third = settled(2).await;
    assert_eq!(third["runs"][1]["status"], "completed", "{}", third["turnItems"]);
    // The sign-in changing also lists the provider again, which starts a process of its own; the turn started in another.
    assert!(launched(&launches).len() >= before + 1, "a new process was started");
    assert_eq!(launched(&requests).iter().filter(|request| request["method"] == "turn/start").count(), 2);

    service.shutdown().await;
    execution.shutdown().await;
}

async fn signed_in_primary(issuer: &Shared) -> ChatGptAuth {
    signed_in(issuer, Arc::new(MemorySecrets::default()), Arc::new(clock_ms)).await
}

async fn phase_of(states: &mut t3_server::provider_auth_service::ProviderAuthChanges, phase: ProviderAuthPhase) -> ProviderAuthState {
    tokio::time::timeout(HANG_BOUND, async {
        loop {
            let state = states.recv().await.expect("the sign-in service ended").expect("a state");
            if state.phase == phase {
                return state;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("waiting for {phase:?}"))
}
