#![cfg(unix)]
//! Antigravity's Google sign-in through the provider-auth service, against `fixtures/antigravity-turn-agent.py`,
//! whose "Google" is a loopback server of its own: the agent prints the authorization URL, listens for the redirect
//! and answers once it has had the code. Nothing contacts Google and nothing opens a browser; the URL the user would
//! open is only read from the sign-in state. Tests wait on the service's own state changes, never on sleeps.
mod antigravity_support;
use antigravity_support::{install_fake_agent, registry, settings};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use t3_contracts::{ProviderAuthPhase, ProviderAuthState};
use t3_server::provider_auth_service::{ProviderAuthChanges, ProviderAuthService};

const AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/antigravity-turn-agent.py");

struct Fixture {
    _root: tempfile::TempDir,
    service: ProviderAuthService,
    registry: t3_server::provider_registry::ProviderRegistry,
    log: std::path::PathBuf,
    state: std::path::PathBuf,
}
async fn fixture(script: Value, config: Value, stream: Option<&str>) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    let agent = install_fake_agent(&root);
    let (script_path, log) = (root.join("script.json"), root.join("agent.jsonl"));
    std::fs::write(&script_path, script.to_string()).unwrap();
    let mut environment = vec![json!({"name":"ANTIGRAVITY_FIXTURE_AGENT","value":AGENT}), json!({"name":"ANTIGRAVITY_FIXTURE_SCRIPT","value":script_path}), json!({"name":"ANTIGRAVITY_FIXTURE_LOG","value":log}), json!({"name":"ANTIGRAVITY_FIXTURE_LAUNCH_LOG","value":root.join("launch.jsonl")})];
    if let Some(stream) = stream {
        environment.push(json!({"name":"ANTIGRAVITY_FIXTURE_URL_STREAM","value":stream}));
    }
    let settings = settings(&root, &agent, config, environment);
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let (registry, state) = registry(&settings, &root, &workspace).await;
    let service = ProviderAuthService::new(registry.clone(), workspace, root.join("caches"), Arc::new(|_| Box::pin(async { Ok(()) })));
    Fixture { _root: directory, service, registry, log, state }
}
fn start_input() -> t3_contracts::ProviderAuthStartInput {
    serde_json::from_value(json!({"instanceId":"antigravity"})).unwrap()
}
async fn phase(states: &mut ProviderAuthChanges, wanted: ProviderAuthPhase) -> ProviderAuthState {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let state = states.recv().await.expect("the state stream ended").expect("a state");
            if state.phase == wanted {
                return state;
            }
            assert!(!(state.phase == ProviderAuthPhase::Failed && wanted != ProviderAuthPhase::Failed), "sign-in failed: {:?}", state.message);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {wanted:?}"))
}
fn authorization(state: &ProviderAuthState) -> (String, String) {
    let value = serde_json::to_value(state).unwrap();
    let interaction = &value["interaction"];
    assert_eq!(interaction["type"], "browser");
    assert_eq!(interaction["acceptsCallback"], true);
    assert_eq!(interaction["requiresConsent"], false);
    let url = interaction["url"].as_str().unwrap().to_owned();
    let parsed = url::Url::parse(&url).unwrap();
    assert_eq!(parsed.host_str(), Some("accounts.google.com"));
    let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    (pairs["redirect_uri"].clone(), pairs["state"].clone())
}
fn provider(registry: &t3_server::provider_registry::ProviderRegistry) -> Value {
    registry.snapshots().into_iter().find(|snapshot| snapshot["instanceId"] == "antigravity").unwrap()
}
fn logged(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}
fn gone(log: &[Value]) {
    for pid in log.iter().filter_map(|row| row["pid"].as_i64()) {
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1, "owned process {pid} still present");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pasted_redirect_is_checked_forwarded_to_the_agent_and_signs_the_account_in() {
    let fixture = fixture(json!({"sign_in":"loopback"}), json!({}), None).await;
    let mut states = fixture.service.subscribe("antigravity", "owner".into()).await.unwrap();
    let started = fixture.service.start(start_input(), "owner".into()).await.unwrap();
    let waiting = phase(&mut states, ProviderAuthPhase::Waiting).await;
    let (redirect, state) = authorization(&waiting);
    assert!(redirect.starts_with("http://127.0.0.1:"));
    let flow = started.flow_id.clone().unwrap();
    let complete = |url: String| serde_json::from_value::<t3_contracts::ProviderAuthCompleteInput>(json!({"instanceId":"antigravity","flowId":flow,"callbackUrl":url})).unwrap();
    // Another client cannot complete it, and a redirect that is not this sign-in's is refused before anything is sent.
    assert!(fixture.service.complete(complete(format!("{redirect}?state={state}&code=fixture-code")), "someone-else").await.is_err());
    for wrong in [format!("{redirect}?state=other&code=fixture-code"), format!("{}other?state={state}&code=fixture-code", redirect), "http://127.0.0.1:1/?state=x&code=y".to_owned(), "not a url".to_owned(), format!("{redirect}?state={state}")] {
        let refused = fixture.service.complete(complete(wrong.clone()), "owner").await.unwrap_err();
        assert_eq!(refused.operation, "complete", "{wrong}");
    }
    assert!(logged(&fixture.log).iter().all(|row| row.get("in").is_none_or(|message| message.get("callback").is_none())), "nothing reached the agent's loopback");
    fixture.service.complete(complete(format!("{redirect}?state={state}&code=fixture-code")), "owner").await.unwrap();
    let done = phase(&mut states, ProviderAuthPhase::Succeeded).await;
    assert!(matches!(done.interaction, Some(Some(None))), "no interaction is left to answer");
    // The agent had exactly the one request, with the code T3 forwarded; the code appears in no state.
    let callbacks: Vec<Value> = logged(&fixture.log).into_iter().filter(|row| row["in"]["callback"].is_object()).collect();
    assert_eq!(callbacks.len(), 1);
    assert_eq!(callbacks[0]["in"]["callback"]["code"], json!(["fixture-code"]));
    // The account is signed in: the snapshot says so and lists what the session offered.
    let snapshot = provider(&fixture.registry);
    assert_eq!((snapshot["status"].clone(), snapshot["auth"]["status"].clone(), snapshot["auth"]["label"].clone()), (json!("ready"), json!("authenticated"), json!("Google account")));
    assert_eq!(snapshot["models"].as_array().unwrap().iter().map(|model| model["slug"].clone()).collect::<Vec<_>>(), [json!("gemini-3.8-flash-high"), json!("gemini-3.8-flash-low")]);
    assert!(!snapshot.to_string().contains("fixture-code") && !format!("{done:?}").contains("fixture-code"));
    // The agent that signed in has ended, and the temporary session it made is gone from the profile.
    gone(&logged(&fixture.log));
    assert!(fixture.state.join("providers").exists(), "the profile is T3's own, under its state directory");
    fixture.service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_url_printed_on_stderr_is_the_same_sign_in_and_a_second_start_is_not_a_second_flow() {
    let fixture = fixture(json!({"sign_in":"loopback"}), json!({}), Some("stderr")).await;
    let mut states = fixture.service.subscribe("antigravity", "owner".into()).await.unwrap();
    let started = fixture.service.start(start_input(), "owner".into()).await.unwrap();
    // The same owner starting again gets the flow it has; another client is told one is in progress.
    assert_eq!(fixture.service.start(start_input(), "owner".into()).await.unwrap().flow_id, started.flow_id);
    assert!(fixture.service.start(start_input(), "intruder".into()).await.is_err());
    let waiting = phase(&mut states, ProviderAuthPhase::Waiting).await;
    let (redirect, state) = authorization(&waiting);
    let url = format!("{redirect}?state={state}&code=fixture-code");
    fixture.service.complete(serde_json::from_value(json!({"instanceId":"antigravity","flowId":started.flow_id,"callbackUrl":url})).unwrap(), "owner").await.unwrap();
    phase(&mut states, ProviderAuthPhase::Succeeded).await;
    fixture.service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_sign_in_ends_the_agent_and_leaves_the_account_unchecked() {
    let fixture = fixture(json!({"sign_in":"loopback"}), json!({}), None).await;
    let mut states = fixture.service.subscribe("antigravity", "owner".into()).await.unwrap();
    let started = fixture.service.start(start_input(), "owner".into()).await.unwrap();
    phase(&mut states, ProviderAuthPhase::Waiting).await;
    let cancelled = fixture.service.cancel(serde_json::from_value(json!({"instanceId":"antigravity","flowId":started.flow_id})).unwrap(), "owner").await.unwrap();
    assert_eq!(cancelled.phase, ProviderAuthPhase::Cancelled);
    gone(&logged(&fixture._root.path().join("launch.jsonl")));
    assert_eq!(provider(&fixture.registry)["auth"]["status"], "unknown");
    fixture.service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_out_asks_the_agent_to_log_out_and_forgets_the_account() {
    let fixture = fixture(json!({"sign_in":"loopback"}), json!({}), None).await;
    let mut states = fixture.service.subscribe("antigravity", "owner".into()).await.unwrap();
    let started = fixture.service.start(start_input(), "owner".into()).await.unwrap();
    let (redirect, state) = authorization(&phase(&mut states, ProviderAuthPhase::Waiting).await);
    fixture.service.complete(serde_json::from_value(json!({"instanceId":"antigravity","flowId":started.flow_id,"callbackUrl":format!("{redirect}?state={state}&code=fixture-code")})).unwrap(), "owner").await.unwrap();
    phase(&mut states, ProviderAuthPhase::Succeeded).await;
    assert_eq!(provider(&fixture.registry)["auth"]["status"], "authenticated");
    let signed_out = fixture.service.logout("antigravity").await.unwrap();
    assert_eq!(signed_out.phase, ProviderAuthPhase::Idle);
    assert!(logged(&fixture.log).iter().any(|row| row["in"]["method"] == "logout"), "the agent was asked to log out");
    let snapshot = provider(&fixture.registry);
    assert_eq!((snapshot["auth"]["status"].clone(), snapshot["models"].clone(), snapshot["status"].clone()), (json!("unauthenticated"), json!([]), json!("warning")));
    gone(&logged(&fixture.log));
    fixture.service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_without_sign_out_and_a_method_the_agent_rejects_fail_with_safe_messages() {
    let fixture = fixture(json!({"no_logout":true}), json!({}), None).await;
    let error = fixture.service.logout("antigravity").await.unwrap_err();
    assert_eq!(error.detail, "This Antigravity version does not support sign-out. Update the provider.");
    fixture.service.shutdown().await;
    // A rejected credential never shows what the agent said about it.
    let fixture = fixture_with_key(json!({"credentials_rejected":true})).await;
    let mut states = fixture.service.subscribe("antigravity", "owner".into()).await.unwrap();
    fixture.service.start(start_input(), "owner".into()).await.unwrap();
    let failed = phase(&mut states, ProviderAuthPhase::Failed).await;
    assert_eq!(failed.message.as_deref(), Some("Antigravity rejected the configured credentials. Check the provider settings."));
    assert!(!format!("{failed:?}").contains("sk-secret"));
    fixture.service.shutdown().await;
}
async fn fixture_with_key(script: Value) -> Fixture {
    fixture(script, json!({"authMethod":"gemini-api-key","apiKey":"sk-configured"}), None).await
}
