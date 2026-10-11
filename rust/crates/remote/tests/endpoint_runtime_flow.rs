//! The managed tunnel's connector supervisor, driven by a fake connector on tokio's paused clock.
mod support;

use std::sync::Arc;
use std::time::Duration;

use support::*;
use t3_remote::endpoint_runtime::*;
use t3_remote::relay_client::RelayClientStatus;
use t3_remote::types::RuntimeConfig;

fn config(token: &str) -> RuntimeConfig {
    RuntimeConfig::cloudflare(token, Some("tunnel-1"))
}

fn runtime() -> (ManagedEndpointRuntime, Arc<FakeSpawner>, Arc<FixedResolver>) {
    let spawner = FakeSpawner::new();
    let resolver = FixedResolver::available();
    (
        ManagedEndpointRuntime::new(resolver.clone(), spawner.clone()),
        spawner,
        resolver,
    )
}

const REGISTERED: &str = "2026-01-01T00:00:00Z INF Registered tunnel connection connIndex=0";

#[tokio::test(start_paused = true)]
async fn starts_deduplicates_rotates_and_stops_the_connector() {
    let (runtime, spawner, _) = runtime();
    let first = runtime.apply_config(Some(&config("token-1"))).await;
    assert!(matches!(first, EndpointStatus::Running { pid: 1000, .. }));
    assert_eq!(first.to_json()["tunnelId"], "tunnel-1");
    // The same config while it runs does not start another process.
    assert_eq!(runtime.apply_config(Some(&config("token-1"))).await, first);
    assert_eq!(spawner.count(), 1);
    assert_eq!(spawner.last().token, "token-1");
    assert_eq!(spawner.last().executable, "/fake/cloudflared");
    // A different token replaces it.
    let rotated = runtime.apply_config(Some(&config("token-2"))).await;
    assert!(matches!(rotated, EndpointStatus::Running { pid: 1001, .. }));
    assert!(!spawner.spawned.lock().unwrap()[0].is_running());
    assert!(spawner.spawned.lock().unwrap()[1].is_running());
    // None stops it.
    assert_eq!(runtime.apply_config(None).await, EndpointStatus::Disabled);
    assert!(!spawner.last().is_running());
    assert_eq!(runtime.current().status, EndpointStatus::Disabled);
}

#[tokio::test(start_paused = true)]
async fn a_non_cloudflare_runtime_stops_the_active_connector() {
    let (runtime, spawner, _) = runtime();
    runtime.apply_config(Some(&config("token-1"))).await;
    let other: RuntimeConfig =
        serde_json::from_str(r#"{"providerKind":"manual","connectorToken":"x"}"#).unwrap();
    let status = runtime.apply_config(Some(&other)).await;
    assert_eq!(
        status.to_json(),
        serde_json::json!({"status": "unsupported", "providerKind": "manual"})
    );
    assert!(!spawner.last().is_running());
}

#[tokio::test(start_paused = true)]
async fn a_connector_that_exited_is_started_again_by_the_next_apply() {
    let (runtime, spawner, _) = runtime();
    runtime.apply_config(Some(&config("token-1"))).await;
    // Take the supervisor out of the picture by letting the process die without it noticing.
    spawner.last().exit(1);
    spawner.wait_for_spawns(2).await;
    assert_eq!(spawner.count(), 2);
    spawner.last().exit(1);
    spawner.wait_for_spawns(3).await;
    assert!(matches!(
        runtime.apply_config(Some(&config("token-1"))).await,
        EndpointStatus::Running { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn reports_spawn_failures_and_a_missing_or_unsupported_client() {
    let (runtime, spawner, resolver) = runtime();
    *spawner.fail_with.lock().unwrap() = Some("no such file".into());
    let status = runtime.apply_config(Some(&config("t"))).await;
    assert_eq!(
        status.to_json(),
        serde_json::json!({"status": "failed", "providerKind": "cloudflare_tunnel", "failure": "spawn-failed", "reason": "no such file", "tunnelId": "tunnel-1"})
    );
    assert!(status.is_retryable());
    *spawner.fail_with.lock().unwrap() = None;

    resolver.set(RelayClientStatus::Missing {
        version: "v".into(),
    });
    let status = runtime.apply_config(Some(&config("t"))).await;
    assert_eq!(
        status.to_json(),
        serde_json::json!({"status": "failed", "providerKind": "cloudflare_tunnel", "failure": "not-installed", "reason": "The relay client is not installed.", "tunnelId": "tunnel-1"})
    );
    assert!(status.is_retryable());
    assert_eq!(spawner.count(), 0);

    resolver.set(RelayClientStatus::Unsupported {
        platform: "plan9".into(),
        arch: "mips".into(),
        version: "v".into(),
    });
    let status = runtime.apply_config(Some(&config("t"))).await;
    assert_eq!(status.to_json()["failure"], "unsupported-platform");
    assert_eq!(
        status.to_json()["reason"],
        "Relay client is unsupported on plan9-mips."
    );
    assert!(!status.is_retryable());
}

#[tokio::test(start_paused = true)]
async fn the_first_crash_restarts_at_once_and_a_crash_loop_backs_off() {
    let (runtime, spawner, _) = runtime();
    runtime.apply_config(Some(&config("t"))).await;
    for n in 1..=9 {
        spawner.last().exit(1);
        spawner.wait_for_spawns(n + 1).await;
    }
    // 0 s, then doubling from 1 s, capped at 60 s.
    assert_eq!(spawner.spawn_gaps(), [0, 1, 2, 4, 8, 16, 32, 60, 60]);
    assert_eq!(runtime.current().restarts, 9);
}

#[tokio::test(start_paused = true)]
async fn a_stable_run_resets_the_backoff() {
    let (runtime, spawner, _) = runtime();
    runtime.apply_config(Some(&config("t"))).await;
    for n in 1..=3 {
        spawner.last().exit(1);
        spawner.wait_for_spawns(n + 1).await;
    }
    assert_eq!(spawner.spawn_gaps(), [0, 1, 2]);
    tokio::time::advance(RELAY_RESTART_STABLE_UPTIME).await;
    spawner.last().exit(1);
    spawner.wait_for_spawns(5).await;
    assert_eq!(spawner.spawn_gaps()[3], 30);
    // The stable run restarted at once; the ladder starts over: immediate, then one second.
    spawner.last().exit(1);
    spawner.wait_for_spawns(6).await;
    spawner.last().exit(1);
    spawner.wait_for_spawns(7).await;
    assert_eq!(spawner.spawn_gaps()[4..], [0, 1]);
}

#[tokio::test(start_paused = true)]
async fn a_recovery_returning_the_same_config_keeps_the_crash_backoff() {
    let (runtime, spawner, _) = runtime();
    runtime.apply_config(Some(&config("t"))).await;
    for n in 1..=3 {
        spawner.last().exit(1);
        spawner.wait_for_spawns(n + 1).await;
    }
    // The relay hands back the same tunnel and token.
    runtime.apply_config(Some(&config("t"))).await;
    spawner.last().exit(1);
    spawner.wait_for_spawns(5).await;
    assert_eq!(spawner.spawn_gaps().last().copied(), Some(4));
}

#[tokio::test(start_paused = true)]
async fn an_explicit_config_change_clears_the_backoff_and_preempts_a_delayed_restart() {
    let (runtime, spawner, _) = runtime();
    runtime.apply_config(Some(&config("t"))).await;
    for n in 1..=3 {
        spawner.last().exit(1);
        spawner.wait_for_spawns(n + 1).await;
    }
    // The next crash would wait 4 s; a new token takes over before that.
    spawner.last().exit(1);
    tokio::task::yield_now().await;
    runtime.apply_config(Some(&config("other"))).await;
    spawner.wait_for_spawns(5).await;
    assert_eq!(spawner.last().token, "other");
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        spawner.count(),
        5,
        "the delayed restart of the old config did not run"
    );
    // And the ladder starts over for the new config: the first crash restarts at once.
    let crashed_at = tokio::time::Instant::now();
    spawner.last().exit(1);
    spawner.wait_for_spawns(6).await;
    assert_eq!(
        *spawner.spawn_times.lock().unwrap().last().unwrap(),
        crashed_at
    );
}

#[tokio::test(start_paused = true)]
async fn an_exit_asks_for_recovery_of_the_exited_tunnel() {
    let (runtime, spawner, _) = runtime();
    runtime.apply_config(Some(&config("t"))).await;
    spawner.last().exit(1);
    let requested = within(runtime.next_recovery_request()).await;
    assert_eq!(requested, config("t"));
}

#[tokio::test(start_paused = true)]
async fn repeated_tunnel_rejections_ask_for_recovery_without_waiting_for_an_exit() {
    let (runtime, spawner, _) = runtime();
    runtime.apply_config(Some(&config("t"))).await;
    let rejection =
        r#"ERR Register tunnel error from server side error="Unauthorized: Invalid tunnel secret""#;
    // Ordinary errors do not count; neither do fewer than four rejections.
    spawner
        .last()
        .say("ERR Failed to serve quic connection error=\"timeout\"");
    for _ in 0..3 {
        spawner.last().say(rejection);
    }
    tokio::task::yield_now().await;
    assert!(
        within_millis(runtime.next_recovery_request())
            .await
            .is_none()
    );
    spawner.last().say(rejection);
    assert_eq!(within(runtime.next_recovery_request()).await, config("t"));
    assert_eq!(spawner.count(), 1, "the connector keeps running");
}

async fn within_millis<T>(future: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::time::timeout(Duration::from_millis(50), future)
        .await
        .ok()
}

#[tokio::test(start_paused = true)]
async fn rejections_split_by_a_connection_do_not_add_up() {
    let (runtime, spawner, _) = runtime();
    runtime.apply_config(Some(&config("t"))).await;
    let rejection = r#"ERR Register tunnel error from server side error="Failed to get tunnel""#;
    for _ in 0..3 {
        spawner.last().say(rejection);
    }
    spawner.last().say(REGISTERED);
    spawner.last().say(rejection);
    tokio::task::yield_now().await;
    assert!(
        within_millis(runtime.next_recovery_request())
            .await
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn a_connector_that_never_registers_asks_for_recovery_every_three_minutes() {
    let (runtime, _spawner, _) = runtime();
    let started = tokio::time::Instant::now();
    runtime.apply_config(Some(&config("t"))).await;
    assert_eq!(within(runtime.next_recovery_request()).await, config("t"));
    assert_eq!(started.elapsed(), CONNECTOR_REGISTRATION_TIMEOUT);
    assert_eq!(within(runtime.next_recovery_request()).await, config("t"));
    assert_eq!(started.elapsed(), CONNECTOR_REGISTRATION_TIMEOUT * 2);
}

#[tokio::test(start_paused = true)]
async fn a_connected_tunnel_asks_for_no_recovery() {
    let (runtime, spawner, _) = runtime();
    runtime.apply_config(Some(&config("t"))).await;
    spawner.last().say(REGISTERED);
    within(runtime.next_tunnel_connection()).await;
    tokio::time::advance(CONNECTOR_REGISTRATION_TIMEOUT * 3).await;
    tokio::task::yield_now().await;
    assert!(
        within_millis(runtime.next_recovery_request())
            .await
            .is_none()
    );
    assert!(runtime.current().connected);
}

#[tokio::test(start_paused = true)]
async fn each_registered_connection_is_signalled_and_bursts_collapse() {
    let (runtime, spawner, _) = runtime();
    let mut snapshots = runtime.subscribe();
    runtime.apply_config(Some(&config("t"))).await;
    assert!(!runtime.current().connected);
    spawner.last().say(REGISTERED);
    spawner.last().say(REGISTERED);
    spawner.last().say(REGISTERED);
    within(runtime.next_tunnel_connection()).await;
    within(async {
        while !snapshots.borrow_and_update().connected {
            snapshots.changed().await.unwrap();
        }
    })
    .await;
    // The burst left one signal at most, and it was taken.
    tokio::task::yield_now().await;
    assert!(
        within_millis(runtime.next_tunnel_connection())
            .await
            .is_none()
    );
    spawner.last().say(REGISTERED);
    within(runtime.next_tunnel_connection()).await;
}

#[tokio::test(start_paused = true)]
async fn recovery_requests_sent_before_anyone_consumes_them_are_kept() {
    let (runtime, _, _) = runtime();
    runtime.request_recovery(config("first"));
    runtime.request_recovery(config("second"));
    // Sliding queue of one: the newest wins.
    assert_eq!(
        within(runtime.next_recovery_request()).await,
        config("second")
    );
}

#[tokio::test(start_paused = true)]
async fn concurrent_config_changes_are_serialised() {
    let (runtime, spawner, _) = runtime();
    let (a, b) = (runtime.clone(), runtime.clone());
    let (one, two) = tokio::join!(
        async move { a.apply_config(Some(&config("a"))).await },
        async move { b.apply_config(Some(&config("b"))).await },
    );
    assert!(one.is_running() && two.is_running());
    let running: Vec<String> = spawner
        .spawned
        .lock()
        .unwrap()
        .iter()
        .filter(|h| h.is_running())
        .map(|h| h.token.clone())
        .collect();
    assert_eq!(running.len(), 1, "{running:?}");
}

#[tokio::test(start_paused = true)]
async fn link_state_updates_do_not_overlap() {
    let (runtime, _, _) = runtime();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tasks = vec![];
    for _ in 0..4 {
        let (runtime, active, max) = (runtime.clone(), active.clone(), max.clone());
        tasks.push(tokio::spawn(async move {
            runtime
                .with_link_state_lock(async {
                    let now = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    max.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                })
                .await;
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(max.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn output_with_the_connector_token_is_redacted() {
    assert_eq!(
        redact_connector_token("ERR bad token secret-1 and secret-1", "secret-1"),
        "ERR bad token <redacted> and <redacted>"
    );
}

#[test]
fn the_connector_is_started_the_way_the_original_starts_it() {
    assert_eq!(
        CONNECTOR_ARGS,
        [
            "tunnel",
            "--no-autoupdate",
            "--loglevel",
            "info",
            "--output",
            "default",
            "run"
        ]
    );
}
