//! Startup, retry and recovery rules on tokio's paused clock, with an in-process relay.
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::json;
use support::*;
use t3_remote::backoff::{FixedRandom, Random};
use t3_remote::cli_state::*;
use t3_remote::cloud_link::CloudLinkError;
use t3_remote::secrets::*;
use t3_remote::startup::*;
use t3_remote::types::*;

type Fut<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

fn counting(
    attempts: Arc<AtomicU32>,
    script: impl Fn(u32) -> Result<&'static str, &'static str> + Send + Sync + 'static,
) -> impl FnMut() -> Fut<Result<&'static str, &'static str>> {
    let script = Arc::new(script);
    move || {
        let attempts = attempts.clone();
        let script = script.clone();
        Box::pin(async move {
            let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            script(n)
        })
    }
}

#[tokio::test(start_paused = true)]
async fn a_permanent_rejection_is_neither_retried_nor_falls_back() {
    let attempts = Arc::new(AtomicU32::new(0));
    let fallbacks = Arc::new(AtomicU32::new(0));
    let fallback = {
        let fallbacks = fallbacks.clone();
        Box::pin(async move {
            fallbacks.fetch_add(1, Ordering::SeqCst);
        }) as Fut<()>
    };
    let result = retry_managed_tunnel_registration(
        counting(attempts.clone(), |_| Err("not authorized")),
        |_| false,
        Some(fallback),
        &FixedRandom(0.5),
    )
    .await;
    assert_eq!(result, Err("not authorized"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(fallbacks.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn retrying_stops_after_the_window_so_startup_can_fall_back() {
    let attempts = Arc::new(AtomicU32::new(0));
    let fallbacks = Arc::new(AtomicU32::new(0));
    let started = tokio::time::Instant::now();
    let fallback = {
        let fallbacks = fallbacks.clone();
        Box::pin(async move {
            fallbacks.fetch_add(1, Ordering::SeqCst);
        }) as Fut<()>
    };
    // Fails for fifteen minutes, then succeeds: the fallback ran once, in between, and the
    // retrying went on without a bound until it worked.
    let result = retry_managed_tunnel_registration(
        counting(attempts.clone(), move |_| {
            if tokio::time::Instant::now() - started < Duration::from_secs(15 * 60) {
                Err("relay unavailable")
            } else {
                Ok("ready")
            }
        }),
        |_| true,
        Some(fallback),
        &FixedRandom(0.5),
    )
    .await;
    assert_eq!(result, Ok("ready"));
    assert_eq!(fallbacks.load(Ordering::SeqCst), 1);
    // Capped at 30 s between attempts, so fifteen minutes is a bounded run.
    let n = attempts.load(Ordering::SeqCst);
    assert!((6..60).contains(&n), "{n}");
}

#[tokio::test(start_paused = true)]
async fn without_a_fallback_the_window_just_ends_the_bounded_phase() {
    let attempts = Arc::new(AtomicU32::new(0));
    let started = tokio::time::Instant::now();
    let result = retry_managed_tunnel_registration(
        counting(attempts.clone(), move |_| {
            if tokio::time::Instant::now() - started < Duration::from_secs(11 * 60) {
                Err("down")
            } else {
                Ok("up")
            }
        }),
        |_| true,
        None,
        &FixedRandom(0.5),
    )
    .await;
    assert_eq!(result, Ok("up"));
}

#[tokio::test(start_paused = true)]
async fn registration_succeeds_after_a_transient_failure_without_provisioning() {
    let attempts = Arc::new(AtomicU32::new(0));
    let started = tokio::time::Instant::now();
    let result = retry_managed_tunnel_registration(
        counting(attempts.clone(), |n| {
            if n == 1 {
                Err("relay unavailable")
            } else {
                Ok("ready")
            }
        }),
        |_| true,
        None,
        &FixedRandom(0.5),
    )
    .await;
    assert_eq!(result, Ok("ready"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    // The first backoff is one second (jitter at 0.5 leaves it unchanged).
    assert_eq!(started.elapsed(), Duration::from_secs(1));
}

// ---- run_cloud_startup --------------------------------------------------------------------------

struct Startup {
    harness: Harness,
    plane: Arc<ScriptedControlPlane>,
}

fn startup(secrets: Vec<(&'static str, String)>, token: bool) -> Startup {
    let plane = ScriptedControlPlane::new();
    let mut options = HarnessOptions::new(plane.clone());
    options.secrets = secrets;
    if !token {
        options.token = None;
    }
    options.relay_url = Some("https://relay.example.test".into());
    Startup {
        harness: Harness::new(options),
        plane,
    }
}

fn stored_tunnel() -> Vec<(&'static str, String)> {
    vec![
        (RELAY_URL_SECRET, "https://relay.example.test".into()),
        (CLOUD_LINKED_USER_ID, "user-123".into()),
        (RELAY_ENVIRONMENT_CREDENTIAL_SECRET, "c".into()),
        (CLOUD_ENDPOINT_RUNTIME_CONFIG, config_json("tok", "tun")),
    ]
}

fn confirmed_marker() -> String {
    serde_json::to_string(&ConfirmedOrigin {
        config: RuntimeConfig::cloudflare("tok", Some("tun")),
        origin: ManagedEndpointOrigin::new("127.0.0.1", 3773),
    })
    .unwrap()
}

async fn run(s: &Startup, wants_public_config: bool, random: f64) {
    let random: Arc<dyn Random> = Arc::new(FixedRandom(random));
    run_cloud_startup(
        s.harness.link.clone(),
        s.harness.runtime.clone(),
        s.harness.secrets.clone(),
        random,
        StartupOptions {
            has_cloud_public_config: wants_public_config,
            local_origin: "http://127.0.0.1:3773".into(),
        },
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_confirmed_tunnel_starts_at_once_and_registration_follows_without_a_wait() {
    let mut secrets = stored_tunnel();
    secrets.push((CLOUD_ENDPOINT_CONFIRMED_ORIGIN, confirmed_marker()));
    let s = startup(secrets, true);
    let started = tokio::time::Instant::now();
    run(&s, true, 0.99).await;
    assert_eq!(s.harness.spawner.count(), 1);
    assert_eq!(s.plane.calls(), ["register"]);
    // Already confirmed: no first-registration jitter.
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn an_unconfirmed_tunnel_waits_a_random_moment_before_registering() {
    let s = startup(stored_tunnel(), true);
    let started = tokio::time::Instant::now();
    run(&s, true, 0.5).await;
    assert_eq!(started.elapsed(), Duration::from_secs(15));
    assert_eq!(s.plane.calls(), ["register"]);
    assert_eq!(s.harness.spawner.count(), 1, "started by the registration");
}

#[tokio::test(start_paused = true)]
async fn when_the_relay_stays_away_the_stored_tunnel_starts_anyway_and_registration_keeps_trying() {
    let s = startup(stored_tunnel(), true);
    *s.plane.register.lock().unwrap() = [Err(unavailable())].into();
    let plane = s.plane.clone();
    // After a while the relay recovers.
    let healing = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(25 * 60)).await;
        *plane.register.lock().unwrap() = [Ok(RecoveryRegistrationResponse {
            status: RegistrationStatus::Ready,
        })]
        .into();
    });
    run(&s, true, 0.0).await;
    healing.await.unwrap();
    // The fallback started the stored config once the ten-minute window was spent.
    assert_eq!(s.harness.spawner.count(), 1);
    assert!(s.plane.count("register") > 10);
    // The unconfirmed start did not record a confirmation; the final registration did.
    assert!(
        s.harness
            .secrets
            .text(CLOUD_ENDPOINT_CONFIRMED_ORIGIN)
            .is_some()
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_registration_starts_the_stored_tunnel_without_retrying() {
    let s = startup(stored_tunnel(), true);
    *s.plane.register.lock().unwrap() = [Err(forbidden())].into();
    run(&s, true, 0.0).await;
    assert_eq!(s.plane.count("register"), 1);
    assert_eq!(s.harness.spawner.count(), 1);
    assert!(
        s.harness
            .secrets
            .text(CLOUD_ENDPOINT_CONFIRMED_ORIGIN)
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn the_relay_asking_for_a_replacement_becomes_a_recovery_request() {
    let s = startup(stored_tunnel(), true);
    *s.plane.register.lock().unwrap() = [Ok(RecoveryRegistrationResponse {
        status: RegistrationStatus::RecoveryRequired,
    })]
    .into();
    run(&s, true, 0.0).await;
    assert_eq!(s.harness.spawner.count(), 0);
    let requested = within(s.harness.runtime.next_recovery_request()).await;
    assert_eq!(requested, RuntimeConfig::cloudflare("tok", Some("tun")));
}

fn link_response(runtime: bool) -> LinkResponse {
    let mut keys = None;
    let keys = keys.get_or_insert_with(t3_remote::jwt::KeyPair::generate);
    serde_json::from_value(json!({
        "ok": true, "cloudUserId": "user-123", "environmentId": "env_123",
        "endpoint": {"httpBaseUrl": "https://e.example.test", "wsBaseUrl": "wss://e.example.test/ws", "providerKind": "cloudflare_tunnel"},
        "endpointRuntime": if runtime { json!({"providerKind": "cloudflare_tunnel", "connectorToken": "issued", "tunnelId": "issued-tunnel"}) } else { json!(null) },
        "relayIssuer": "https://relay.example.test", "environmentCredential": "cred", "cloudMintPublicKey": keys.public_key
    }))
    .unwrap()
}

fn challenge() -> LinkChallengeResponse {
    serde_json::from_value(json!({"challenge": "c", "expiresAt": "x"})).unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_desired_cli_link_is_made_at_startup_and_then_registered() {
    let s = startup(vec![], true);
    set_cli_desired_cloud_link(s.harness.secrets.as_ref(), true, DesiredLinkMode::Managed).unwrap();
    s.plane.challenge.lock().unwrap().push_back(Ok(challenge()));
    s.plane
        .link
        .lock()
        .unwrap()
        .push_back(Ok(link_response(true)));
    run(&s, true, 0.0).await;
    // Not linked yet: nothing to register; the link was made, then the new tunnel registered.
    // Nothing stored to register, so the link is made and only then is its tunnel registered.
    assert_eq!(s.plane.calls(), ["challenge", "link", "register"]);
    assert_eq!(s.harness.spawner.last().token, "issued");
    assert_eq!(
        s.harness.secrets.text(CLOUD_LINKED_USER_ID).as_deref(),
        Some("user-123")
    );
}

#[tokio::test(start_paused = true)]
async fn a_publish_only_wish_links_without_a_tunnel_or_registration() {
    let s = startup(stored_tunnel(), true);
    set_cli_desired_cloud_link(
        s.harness.secrets.as_ref(),
        true,
        DesiredLinkMode::PublishOnly,
    )
    .unwrap();
    s.plane.challenge.lock().unwrap().push_back(Ok(challenge()));
    s.plane
        .link
        .lock()
        .unwrap()
        .push_back(Ok(link_response(false)));
    // A stale managed config from an earlier link is present but is not started.
    s.harness.secrets.remove(CLOUD_LINKED_USER_ID).unwrap();
    s.harness
        .secrets
        .remove(CLOUD_ENDPOINT_RUNTIME_CONFIG)
        .unwrap();
    let started = tokio::time::Instant::now();
    run(&s, true, 0.99).await;
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "no first-registration wait for publish-only"
    );
    assert_eq!(s.plane.calls(), ["challenge", "link"]);
    assert_eq!(s.harness.spawner.count(), 0);
}

#[tokio::test(start_paused = true)]
async fn the_desired_link_is_reconciled_through_a_relay_outage_but_not_through_a_refusal() {
    let s = startup(vec![], true);
    set_cli_desired_cloud_link(
        s.harness.secrets.as_ref(),
        true,
        DesiredLinkMode::PublishOnly,
    )
    .unwrap();
    s.plane.challenge.lock().unwrap().extend([
        Err(unavailable()),
        Err(unavailable()),
        Ok(challenge()),
    ]);
    s.plane
        .link
        .lock()
        .unwrap()
        .push_back(Ok(link_response(false)));
    let started = tokio::time::Instant::now();
    run(&s, true, 0.0).await;
    assert_eq!(s.plane.count("challenge"), 3);
    // Unjittered 1 s then 2 s.
    assert_eq!(started.elapsed(), Duration::from_secs(3));
    assert!(s.harness.secrets.text(CLOUD_LINKED_USER_ID).is_some());

    let refused = startup(vec![], true);
    set_cli_desired_cloud_link(
        refused.harness.secrets.as_ref(),
        true,
        DesiredLinkMode::PublishOnly,
    )
    .unwrap();
    refused
        .plane
        .challenge
        .lock()
        .unwrap()
        .push_back(Err(forbidden()));
    run(&refused, true, 0.0).await;
    assert_eq!(refused.plane.count("challenge"), 1);
    assert!(refused.harness.secrets.text(CLOUD_LINKED_USER_ID).is_none());
}

#[tokio::test(start_paused = true)]
async fn reconciling_gives_up_after_ten_minutes_of_outage() {
    let s = startup(vec![], true);
    set_cli_desired_cloud_link(
        s.harness.secrets.as_ref(),
        true,
        DesiredLinkMode::PublishOnly,
    )
    .unwrap();
    s.plane
        .challenge
        .lock()
        .unwrap()
        .push_back(Err(unavailable()));
    let started = tokio::time::Instant::now();
    run(&s, true, 0.0).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(10 * 60) && elapsed < Duration::from_secs(11 * 60),
        "{elapsed:?}"
    );
    assert!(s.plane.count("challenge") > 15);
}

#[tokio::test(start_paused = true)]
async fn the_cli_wish_is_not_even_read_without_public_config() {
    let s = startup(vec![], true);
    set_cli_desired_cloud_link(s.harness.secrets.as_ref(), true, DesiredLinkMode::Managed).unwrap();
    run(&s, false, 0.0).await;
    assert!(s.plane.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_wish_withdrawn_before_reconcile_is_honoured() {
    let s = startup(vec![], true);
    // Desired when read at the start of startup, gone when the reconcile takes the lock.
    set_cli_desired_cloud_link(s.harness.secrets.as_ref(), true, DesiredLinkMode::Managed).unwrap();
    let link = s.harness.link.clone();
    set_cli_desired_cloud_link(s.harness.secrets.as_ref(), false, DesiredLinkMode::Managed)
        .unwrap();
    assert_eq!(
        link.reconcile_desired_link_if_still_desired("http://127.0.0.1:3773")
            .await
            .unwrap(),
        None
    );
    assert!(s.plane.calls().is_empty());
}

// ---- the recovery loop ---------------------------------------------------------------------------

fn recovery_response_for(token: &str) -> RecoveryResponse {
    serde_json::from_value(json!({
        "endpoint": {"httpBaseUrl": "https://e.example.test", "wsBaseUrl": "wss://e.example.test/ws", "providerKind": "cloudflare_tunnel"},
        "endpointRuntime": {"providerKind": "cloudflare_tunnel", "connectorToken": token, "tunnelId": format!("{token}-tunnel")}
    }))
    .unwrap()
}

fn recovery_response() -> RecoveryResponse {
    recovery_response_for("recovered")
}

#[tokio::test(start_paused = true)]
async fn recovery_requests_are_served_one_per_cooldown() {
    let s = startup(stored_tunnel(), true);
    s.plane.recover.lock().unwrap().extend([
        Ok(recovery_response_for("recovered")),
        Ok(recovery_response_for("again")),
    ]);
    let random: Arc<dyn Random> = Arc::new(FixedRandom(0.5));
    let loop_task = tokio::spawn(run_recovery_loop(
        s.harness.link.clone(),
        s.harness.runtime.clone(),
        random,
        "http://127.0.0.1:3773".into(),
    ));
    let started = tokio::time::Instant::now();
    // The first request is served at once.
    s.harness
        .runtime
        .request_recovery(RuntimeConfig::cloudflare("tok", Some("tun")));
    s.harness.spawner.wait_for_spawns(1).await;
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(s.harness.spawner.last().token, "recovered");
    // A second one right after waits out the cooldown.
    s.harness
        .runtime
        .request_recovery(RuntimeConfig::cloudflare(
            "recovered",
            Some("recovered-tunnel"),
        ));
    s.harness.spawner.wait_for_spawns(2).await;
    assert_eq!(started.elapsed(), MANAGED_TUNNEL_RECOVERY_COOLDOWN);
    assert_eq!(s.plane.count("recover"), 2);
    loop_task.abort();
}

#[tokio::test(start_paused = true)]
async fn recovery_retries_a_relay_outage_but_not_a_refusal() {
    let s = startup(stored_tunnel(), true);
    s.plane.recover.lock().unwrap().extend([
        Err(unavailable()),
        Err(unavailable()),
        Ok(recovery_response()),
    ]);
    let random: Arc<dyn Random> = Arc::new(FixedRandom(0.5));
    let loop_task = tokio::spawn(run_recovery_loop(
        s.harness.link.clone(),
        s.harness.runtime.clone(),
        random.clone(),
        "http://127.0.0.1:3773".into(),
    ));
    s.harness
        .runtime
        .request_recovery(RuntimeConfig::cloudflare("tok", Some("tun")));
    s.harness.spawner.wait_for_spawns(1).await;
    assert_eq!(s.plane.count("recover"), 3);
    loop_task.abort();

    let refused = startup(stored_tunnel(), true);
    refused
        .plane
        .recover
        .lock()
        .unwrap()
        .push_back(Err(forbidden()));
    let loop_task = tokio::spawn(run_recovery_loop(
        refused.harness.link.clone(),
        refused.harness.runtime.clone(),
        random,
        "http://127.0.0.1:3773".into(),
    ));
    refused
        .harness
        .runtime
        .request_recovery(RuntimeConfig::cloudflare("tok", Some("tun")));
    refused.plane.wait_for("recover", 1).await;
    // A refusal ends that request; the next one is served after the cooldown.
    refused
        .harness
        .runtime
        .request_recovery(RuntimeConfig::cloudflare("tok", Some("tun")));
    refused.plane.wait_for("recover", 2).await;
    assert_eq!(refused.plane.count("recover"), 2, "one attempt per request");
    loop_task.abort();
}

// ---- held webhooks ---------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn held_webhooks_are_woken_once_per_burst_of_connections_and_only_when_opted_in() {
    let mut secrets = stored_tunnel();
    secrets.push((HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET, "true".into()));
    let s = startup(secrets, true);
    s.plane
        .wake
        .lock()
        .unwrap()
        .push_back(Ok(WakeHeldHooksResponse { pending: true }));
    let waker = tokio::spawn(run_held_hooks_waker(
        s.harness.runtime.clone(),
        s.harness.secrets.clone(),
        s.plane.clone(),
        "env_123".into(),
    ));
    s.harness
        .runtime
        .apply_config(Some(&RuntimeConfig::cloudflare("tok", Some("tun"))))
        .await;
    let connector = s.harness.spawner.last();
    // cloudflared registers several connections within a few seconds: one wake.
    let started = tokio::time::Instant::now();
    for _ in 0..4 {
        connector.say("INF Registered tunnel connection connIndex=0");
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    s.plane.wait_for("wake", 1).await;
    // Debounced: three quiet seconds after the last registration, not after the first.
    assert_eq!(started.elapsed(), Duration::from_secs(4 + 3));
    assert_eq!(s.plane.count("wake"), 1);
    waker.abort();

    // Without the opt-in nothing is sent.
    let quiet = startup(stored_tunnel(), true);
    assert!(
        !wake_held_hooks(
            quiet.harness.secrets.as_ref(),
            quiet.plane.as_ref(),
            "env_123"
        )
        .await
        .unwrap()
    );
    assert!(quiet.plane.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failed_wake_is_retried_three_times_with_a_doubling_wait() {
    let mut secrets = stored_tunnel();
    secrets.push((HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET, "true".into()));
    let s = startup(secrets, true);
    s.plane.wake.lock().unwrap().push_back(Err(unavailable()));
    let waker = tokio::spawn(run_held_hooks_waker(
        s.harness.runtime.clone(),
        s.harness.secrets.clone(),
        s.plane.clone(),
        "env_123".into(),
    ));
    s.harness
        .runtime
        .apply_config(Some(&RuntimeConfig::cloudflare("tok", Some("tun"))))
        .await;
    s.harness
        .spawner
        .last()
        .say("INF Registered tunnel connection connIndex=0");
    let started = tokio::time::Instant::now();
    s.plane.wait_for("wake", 4).await;
    // Debounce 3 s, then retries after 2, 4 and 8 s.
    assert_eq!(started.elapsed(), Duration::from_secs(3 + 2 + 4 + 8));
    waker.abort();
    let _ = CloudLinkError::NotLinked;
}
