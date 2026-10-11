//! The pieces together as the server uses them: `RemoteServices` against a relay on loopback,
//! agent-activity publishing, and webhook delivery proofs.
mod support;

use std::sync::Arc;

use serde_json::json;
use support::*;
use t3_remote::awareness::*;
use t3_remote::backoff::FixedRandom;
use t3_remote::cli_state::*;
use t3_remote::delivery_proof::*;
use t3_remote::jwt::*;
use t3_remote::relay_client::{InstallProgressEvent, InstallProgressStage, RelayClientStatus};
use t3_remote::runtime::*;
use t3_remote::secrets::*;
use t3_remote::types::*;

fn trimmed(value: &str) -> t3_contracts::TrimmedNonEmptyString {
    t3_contracts::TrimmedNonEmptyString::new(value).unwrap()
}

struct Services {
    services: RemoteServices,
    secrets: Arc<MemorySecretStore>,
    spawner: Arc<FakeSpawner>,
    resolver: Arc<FixedResolver>,
}

fn services(
    relay_url: &str,
    control_plane: Arc<dyn t3_remote::control_plane::ControlPlane>,
) -> Services {
    let secrets = Arc::new(MemorySecretStore::new());
    let spawner = FakeSpawner::new();
    let resolver = FixedResolver::available();
    let services = RemoteServices::new(RemoteConfig {
        secrets: secrets.clone(),
        environment: Arc::new(TestEnvironment("env_123".into())),
        pairing: Arc::new(FakePairing::default()),
        awareness: Arc::new(CountingAwareness::default()),
        updates: Arc::new(FakeUpdates::default()),
        credentials: Arc::new(FakeCredentials(Some(token("cli-access")))),
        control_plane,
        relay_client: resolver.clone(),
        connector_spawner: spawner.clone(),
        clock: FixedClock::at(NOW_MS),
        random: Arc::new(FixedRandom(0.0)),
        relay_url: Some(relay_url.to_owned()),
        has_cloud_public_config: true,
        local_port: Arc::new(|| Some(3773)),
        allow_loopback_http_relay: true,
    });
    Services {
        services,
        secrets,
        spawner,
        resolver,
    }
}

async fn wait_for_phase(services: &RemoteServices, phase: LinkPhase) -> RemoteStatus {
    let mut status = services.status();
    within(async {
        loop {
            let current = status.borrow_and_update().clone();
            if current.phase == phase {
                return current;
            }
            status.changed().await.unwrap();
        }
    })
    .await
}

fn relay_that_links(
    keys: &KeyPair,
) -> impl Fn(&RecordedRequest) -> (u16, serde_json::Value) + Send + Sync + 'static {
    let public = keys.public_key.clone();
    move |request| match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/v1/client/environment-link-challenges") => {
            (200, json!({"challenge": "c-1", "expiresAt": "x"}))
        }
        ("POST", "/v1/client/environment-links") => (
            200,
            json!({
                "ok": true, "cloudUserId": "user-123", "environmentId": "env_123",
                "endpoint": {"httpBaseUrl": "https://e.example.test", "wsBaseUrl": "wss://e.example.test/ws", "providerKind": "cloudflare_tunnel"},
                "endpointRuntime": {"providerKind": "cloudflare_tunnel", "connectorToken": "issued-token", "tunnelId": "issued-tunnel"},
                "relayIssuer": "http://127.0.0.1", "environmentCredential": "issued-credential", "cloudMintPublicKey": public
            }),
        ),
        ("POST", "/v1/environments/env_123/tunnel/recovery") => (200, json!({"status": "ready"})),
        ("POST", "/v1/environments/env_123/hooks/wake") => (200, json!({"pending": false})),
        ("DELETE", "/v1/client/environment-links/env_123/tunnel") => (200, json!({"ok": true})),
        _ => (404, json!({})),
    }
}

#[tokio::test]
async fn a_cli_wish_becomes_a_running_tunnel_and_shutdown_releases_it() {
    let keys = KeyPair::generate();
    let relay = FakeRelay::start(relay_that_links(&keys)).await;
    let s = services(&relay.url, http_control_plane());
    set_cli_desired_cloud_link(s.secrets.as_ref(), true, DesiredLinkMode::Managed).unwrap();

    // Before the server starts it is pending.
    s.services.refresh_status();
    assert_eq!(s.services.status().borrow().phase, LinkPhase::Pending);
    assert_eq!(
        s.services.status().borrow().desired,
        Some(DesiredMode::Managed)
    );

    s.services.start();
    let starting = wait_for_phase(&s.services, LinkPhase::EndpointStarting).await;
    assert!(starting.link.linked && starting.link.managed_tunnel_active == Some(true));
    assert!(starting.endpoint.status.is_running());
    // The relay client says the tunnel is carrying traffic.
    s.spawner
        .last()
        .say("2026-01-01T00:00:00Z INF Registered tunnel connection connIndex=0");
    let up = wait_for_phase(&s.services, LinkPhase::EndpointUp).await;
    assert!(up.endpoint.connected);
    assert_eq!(s.spawner.last().token, "issued-token");

    // The relay saw: challenge, link (both with the CLI token), then the environment-credential
    // registration of the new tunnel.
    let paths: Vec<_> = relay
        .recorded()
        .iter()
        .map(|r| {
            (
                r.method.clone(),
                r.path.clone(),
                r.authorization.clone().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        paths[0],
        (
            "POST".into(),
            "/v1/client/environment-link-challenges".into(),
            "Bearer cli-access".into()
        )
    );
    assert_eq!(paths[1].1, "/v1/client/environment-links");
    assert!(paths.iter().any(|(m, p, a)| m == "POST"
        && p == "/v1/environments/env_123/tunnel/recovery"
        && a == "Bearer issued-credential"));

    // Shutdown releases the CLI-managed tunnel (with the CLI token), stops the connector and
    // forgets the dead connector token.
    assert!(s.services.shutdown().await);
    assert!(!s.spawner.last().is_running());
    assert_eq!(
        relay
            .recorded_for("DELETE", "/v1/client/environment-links/env_123/tunnel")
            .len(),
        1
    );
    assert!(s.secrets.text(CLOUD_ENDPOINT_RUNTIME_CONFIG).is_none());
    // The link still exists but its tunnel config is gone: shown as linked without a tunnel.
    assert_eq!(s.services.status().borrow().phase, LinkPhase::Linked);
}

#[tokio::test]
async fn unlinking_through_the_wrappers_updates_the_status_stream() {
    let keys = KeyPair::generate();
    let relay = FakeRelay::start(relay_that_links(&keys)).await;
    let s = services(&relay.url, http_control_plane());
    for (name, value) in linked_secrets(&relay.url, &keys) {
        s.secrets.set(name, value.as_bytes()).unwrap();
    }
    s.services.refresh_status();
    assert_eq!(s.services.status().borrow().phase, LinkPhase::Linked);
    let result = s.services.unlink().await.unwrap();
    assert!(result.ok);
    assert_eq!(s.services.status().borrow().phase, LinkPhase::Unlinked);
    assert!(!s.services.status().borrow().link.linked);
}

#[tokio::test]
async fn preferences_through_the_wrappers_are_reflected_in_the_status() {
    let keys = KeyPair::generate();
    let relay = FakeRelay::start(|request| {
        if request.path.ends_with("/link-preferences") {
            (200, request.body.clone())
        } else {
            (404, json!({}))
        }
    })
    .await;
    let s = services(&relay.url, http_control_plane());
    for (name, value) in linked_secrets(&relay.url, &keys) {
        s.secrets.set(name, value.as_bytes()).unwrap();
    }
    let state = s
        .services
        .update_preferences(PreferencesRequest {
            publish_agent_activity: true,
            hold_webhooks_while_offline: Some(true),
        })
        .await
        .unwrap();
    assert!(state.publish_agent_activity && state.hold_webhooks_while_offline == Some(true));
    assert!(s.services.status().borrow().link.publish_agent_activity);
    assert!(s.services.agent_activity_publishing_active());
    assert_eq!(
        relay.recorded()[0].body,
        json!({"holdWebhooksWhileOffline": true})
    );
}

#[tokio::test]
async fn the_relay_client_rpcs_report_and_install() {
    let s = services("http://127.0.0.1:1", ScriptedControlPlane::new());
    assert!(matches!(
        s.services.relay_client_status().await,
        RelayClientStatus::Available { .. }
    ));
    s.resolver.set(RelayClientStatus::Missing {
        version: "v".into(),
    });
    assert_eq!(
        s.services.relay_client_status().await,
        RelayClientStatus::Missing {
            version: "v".into()
        }
    );
    let mut events = s.services.install_relay_client();
    assert_eq!(
        within(events.recv()).await.unwrap().unwrap(),
        InstallProgressEvent::Progress {
            stage: InstallProgressStage::Checking
        }
    );
    assert!(matches!(
        within(events.recv()).await.unwrap().unwrap(),
        InstallProgressEvent::Complete { .. }
    ));
    assert!(within(events.recv()).await.is_none());
}

// ---- agent activity ---------------------------------------------------------------------------------

fn activity_state(phase: AgentAwarenessPhase) -> AgentActivityState {
    AgentActivityState {
        environment_id: t3_contracts::EnvironmentId::new("env_123").unwrap(),
        thread_id: t3_contracts::ThreadId::new("thread-1").unwrap(),
        project_title: trimmed("Project"),
        thread_title: trimmed("Thread"),
        phase,
        headline: trimmed("Working"),
        detail: Some(trimmed("Reading files")),
        model_title: trimmed("Model"),
        updated_at: trimmed("2026-01-01T00:00:00.000Z"),
        deep_link: trimmed("t3://thread/1"),
    }
}

#[tokio::test]
async fn a_published_state_carries_a_proof_signed_by_the_environment_for_that_thread_and_state() {
    let relay = FakeRelay::start(|r| {
        if r.path.ends_with("/agent-activity") {
            (200, json!({"ok": true, "deliveries": []}))
        } else {
            (404, json!({}))
        }
    })
    .await;
    let secrets = MemorySecretStore::new();
    let keys = t3_remote::env_keys::get_or_create_environment_key_pair(&secrets).unwrap();
    let config = PublishRelayConfig {
        url: relay.url.clone(),
        issuer: "https://relay.example.test/".into(),
        environment_credential: "cred".into(),
    };
    let clock = FixedClock::at(NOW_MS);
    let state = activity_state(AgentAwarenessPhase::Running);
    let plane = t3_remote::control_plane::HttpControlPlane::new();
    let response = within(publish_thread_state(
        &plane,
        clock.as_ref(),
        &keys,
        &config,
        "env_123",
        "thread-1",
        Some(state.clone()),
    ))
    .await
    .unwrap();
    assert!(response.ok);
    let sent = relay.recorded();
    assert_eq!(
        sent[0].path,
        "/v1/environments/env_123/threads/thread-1/agent-activity"
    );
    assert_eq!(sent[0].authorization.as_deref(), Some("Bearer cred"));
    assert_eq!(sent[0].body["state"], serde_json::to_value(&state).unwrap());
    let claims = verify_relay_jwt(&VerifyOptions {
        public_key: &keys.public_key,
        token: sent[0].body["proof"].as_str().unwrap(),
        typ: RELAY_ACTIVITY_PUBLISH_TYP,
        issuer: "t3-env:env_123",
        audience: "https://relay.example.test",
        now_epoch_seconds: NOW_MS / 1000,
        max_token_age_secs: None,
    })
    .unwrap();
    assert_eq!(claims["threadId"], "thread-1");
    assert_eq!(claims["state"], serde_json::to_value(&state).unwrap());
    assert_eq!(
        claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
        300
    );
    // A tombstone proof carries a null state.
    within(publish_thread_state(
        &plane,
        clock.as_ref(),
        &keys,
        &config,
        "env_123",
        "thread-1",
        None,
    ))
    .await
    .unwrap();
    assert_eq!(relay.recorded()[1].body["state"], json!(null));
}

#[test]
fn the_publishing_relay_config_needs_a_url_and_a_credential_and_defaults_the_issuer() {
    let store = MemorySecretStore::new();
    assert_eq!(read_publish_relay_config(&store), None);
    store
        .set(RELAY_URL_SECRET, b"https://relay.example.test")
        .unwrap();
    assert_eq!(read_publish_relay_config(&store), None);
    store
        .set(RELAY_ENVIRONMENT_CREDENTIAL_SECRET, b"cred")
        .unwrap();
    let config = read_publish_relay_config(&store).unwrap();
    assert_eq!(config.issuer, "https://relay.example.test");
    store.set(RELAY_ISSUER_SECRET, b"").unwrap();
    assert_eq!(
        read_publish_relay_config(&store).unwrap().issuer,
        "",
        "an empty stored issuer is kept, as in the original"
    );
    assert!(!read_publish_agent_activity_enabled(&store));
    store.set(PUBLISH_AGENT_ACTIVITY_SECRET, b"true").unwrap();
    assert!(read_publish_agent_activity_enabled(&store));
    assert!(read_agent_activity_publishing_active(&store));
    store.set(RELAY_ENVIRONMENT_CREDENTIAL_SECRET, b"").unwrap();
    assert!(
        !read_agent_activity_publishing_active(&store),
        "an empty credential is as unconfigured as a missing one"
    );
}

// ---- webhook delivery proofs -------------------------------------------------------------------------------

#[test]
fn a_webhook_is_trusted_as_relay_delivered_only_with_a_matching_signed_proof() {
    let relay = KeyPair::generate();
    let secrets = MemorySecretStore::with(&[
        (CLOUD_MINT_PUBLIC_KEY, relay.public_key.as_str()),
        (RELAY_URL_SECRET, "https://relay.example.test"),
    ]);
    let clock = FixedClock::at(NOW_MS);
    let now = NOW_MS / 1000;
    let claims = |overrides: serde_json::Value| {
        let mut base = json!({
            "iss": "https://relay.example.test", "aud": "t3-env:env_123", "sub": "env_123", "jti": "j", "iat": now - 3600, "exp": now + 3600,
            "environmentId": "env_123", "deliveryId": "delivery-1", "receivedAt": "2026-01-01T00:00:00.000Z", "hookId": "hook-1"
        });
        for (k, v) in overrides.as_object().unwrap() {
            base[k] = v.clone();
        }
        sign_relay_jwt(&relay.private_key, RELAY_HOOK_DELIVERY_TYP, &base).unwrap()
    };
    let headers = |proof: String| {
        header_map(&[
            (RELAY_HOOK_DELIVERY_HEADER, proof.as_str()),
            ("x-t3-relay-delivery-id", "delivery-1"),
            ("x-t3-relay-received-at", "2026-01-01T00:00:00.000Z"),
        ])
    };
    let verify = |headers, hook: &str| {
        verify_relay_delivery(&secrets, "env_123", clock.as_ref(), &headers, hook)
    };
    // Held for hours is fine (up to 25 h)...
    assert_eq!(
        verify(headers(claims(json!({}))), "hook-1"),
        Some(VerifiedDelivery {
            delivery_id: "delivery-1".into(),
            received_at: "2026-01-01T00:00:00.000Z".into()
        })
    );
    // ...but the proof cannot be lifted onto another delivery, hook, environment, or header set.
    assert_eq!(verify(headers(claims(json!({}))), "hook-2"), None);
    assert_eq!(
        verify(headers(claims(json!({"deliveryId": "other"}))), "hook-1"),
        None
    );
    assert_eq!(
        verify(headers(claims(json!({"receivedAt": "other"}))), "hook-1"),
        None
    );
    assert_eq!(
        verify(
            headers(claims(json!({"environmentId": "env_other"}))),
            "hook-1"
        ),
        None
    );
    assert_eq!(
        verify(headers(claims(json!({"iat": now - 26 * 3600}))), "hook-1"),
        None
    );
    let mut missing = headers(claims(json!({})));
    missing.remove("x-t3-relay-received-at");
    assert_eq!(verify(missing, "hook-1"), None);
    // Someone else's signature, and an environment that is not linked, are direct requests.
    let impostor = KeyPair::generate();
    let forged = sign_relay_jwt(
        &impostor.private_key,
        RELAY_HOOK_DELIVERY_TYP,
        &json!({"iss": "https://relay.example.test", "aud": "t3-env:env_123", "iat": now}),
    )
    .unwrap();
    assert_eq!(verify(headers(forged), "hook-1"), None);
    let unlinked = MemorySecretStore::new();
    assert_eq!(
        verify_relay_delivery(
            &unlinked,
            "env_123",
            clock.as_ref(),
            &headers(claims(json!({}))),
            "hook-1"
        ),
        None
    );
}
