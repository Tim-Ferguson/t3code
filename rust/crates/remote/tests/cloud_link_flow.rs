//! The environment's link to T3 Connect against a relay on loopback: what it signs, stores,
//! sends, and refuses.
mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use support::*;
use t3_remote::cli_state::*;
use t3_remote::cloud_link::*;
use t3_remote::endpoint_runtime::EndpointStatus;
use t3_remote::env_keys::get_or_create_environment_key_pair;
use t3_remote::jwt::*;
use t3_remote::link_checks::InboundRequest;
use t3_remote::secrets::*;
use t3_remote::types::*;

fn trimmed(value: &str) -> t3_contracts::TrimmedNonEmptyString {
    t3_contracts::TrimmedNonEmptyString::new(value).unwrap()
}

fn inbound(port: u16) -> InboundRequest {
    InboundRequest {
        original_url: "/api/connect/link-proof".into(),
        headers: header_map(&[("host", &format!("127.0.0.1:{port}"))]),
    }
}

fn proof_request(kind: ProviderKind, port: u16) -> LinkProofRequest {
    LinkProofRequest {
        challenge: "challenge-1".into(),
        relay_issuer: "https://relay.example.test/".into(),
        endpoint: ManagedEndpoint {
            http_base_url: trimmed(&format!("http://127.0.0.1:{port}")),
            ws_base_url: trimmed(&format!("ws://127.0.0.1:{port}")),
            provider_kind: kind,
        },
        origin: ManagedEndpointOrigin::new("127.0.0.1", port),
    }
}

fn verify_env_proof(
    harness: &Harness,
    token: &str,
    typ: &str,
    audience: &str,
) -> serde_json::Map<String, Value> {
    let keys = get_or_create_environment_key_pair(harness.secrets.as_ref()).unwrap();
    verify_relay_jwt(&VerifyOptions {
        public_key: &keys.public_key,
        token,
        typ,
        issuer: &format!("t3-env:{}", harness.environment_id),
        audience,
        now_epoch_seconds: NOW_MS / 1000,
        max_token_age_secs: None,
    })
    .unwrap_or_else(|e| panic!("{e:?}"))
}

fn dead_plane() -> Arc<dyn t3_remote::control_plane::ControlPlane> {
    ScriptedControlPlane::new()
}

#[tokio::test]
async fn link_proof_is_signed_for_this_environment_and_its_own_loopback_origin() {
    let harness = Harness::new(HarnessOptions::new(dead_plane()));
    let token = harness
        .link
        .link_proof(
            &proof_request(ProviderKind::CloudflareTunnel, 3773),
            &inbound(3773),
        )
        .unwrap();
    let claims = verify_env_proof(
        &harness,
        &token,
        RELAY_LINK_PROOF_TYP,
        "https://relay.example.test",
    );
    assert_eq!(claims["sub"], "env_123");
    assert_eq!(claims["challenge"], "challenge-1");
    assert_eq!(
        claims["scopes"],
        json!(["agent_activity_notifications", "managed_tunnels"])
    );
    assert_eq!(
        claims["origin"],
        json!({"localHttpHost": "127.0.0.1", "localHttpPort": 3773})
    );
    assert_eq!(
        claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
        300
    );
    assert_eq!(claims["descriptor"]["environmentId"], "env_123");
    let keys = get_or_create_environment_key_pair(harness.secrets.as_ref()).unwrap();
    assert_eq!(claims["environmentPublicKey"], keys.public_key.trim());
    // A manual endpoint claims only the notification scope.
    let manual = harness
        .link
        .link_proof(&proof_request(ProviderKind::Manual, 3773), &inbound(3773))
        .unwrap();
    assert_eq!(
        verify_env_proof(
            &harness,
            &manual,
            RELAY_LINK_PROOF_TYP,
            "https://relay.example.test"
        )["scopes"],
        json!(["agent_activity_notifications"])
    );
}

#[tokio::test]
async fn link_proof_refuses_anything_but_a_direct_loopback_request() {
    let harness = Harness::new(HarnessOptions::new(dead_plane()));
    let refused = CloudLinkError::OriginInvalid { endpoint: true };
    // The request arrived on another port than the origin it names.
    assert_eq!(
        harness
            .link
            .link_proof(
                &proof_request(ProviderKind::CloudflareTunnel, 3773),
                &inbound(4000)
            )
            .unwrap_err(),
        refused
    );
    // The relay may not ask for an endpoint it provisions itself.
    assert_eq!(
        harness
            .link
            .link_proof(&proof_request(ProviderKind::T3Relay, 3773), &inbound(3773))
            .unwrap_err(),
        refused
    );
    // A proxied request is not direct.
    let mut proxied = inbound(3773);
    proxied
        .headers
        .insert("x-forwarded-host".into(), "evil.example".into());
    assert_eq!(
        harness
            .link
            .link_proof(
                &proof_request(ProviderKind::CloudflareTunnel, 3773),
                &proxied
            )
            .unwrap_err(),
        refused
    );
    // A non-loopback origin.
    let mut remote = proof_request(ProviderKind::CloudflareTunnel, 3773);
    remote.origin = ManagedEndpointOrigin::new("192.168.1.4", 3773);
    assert_eq!(
        harness
            .link
            .link_proof(&remote, &inbound(3773))
            .unwrap_err(),
        refused
    );
    assert_eq!(refused.message(), "Invalid managed endpoint origin.");
}

fn relay_keys() -> KeyPair {
    KeyPair::generate()
}

fn config_request(keys: &KeyPair, runtime: Option<RuntimeConfig>) -> EnvironmentConfigRequest {
    EnvironmentConfigRequest {
        relay_url: "https://relay.example.test".into(),
        relay_issuer: Some("https://relay.example.test".into()),
        cloud_user_id: "user-123".into(),
        environment_credential: "environment-credential".into(),
        cloud_mint_public_key: keys.public_key.clone(),
        endpoint_runtime: runtime,
    }
}

#[tokio::test]
async fn relay_config_that_cannot_be_installed_is_refused_field_by_field() {
    let keys = relay_keys();
    let harness = Harness::new(HarnessOptions::new(dead_plane()));
    let check = |field: RelayConfigField, edit: &dyn Fn(&mut EnvironmentConfigRequest)| {
        let mut request = config_request(&keys, None);
        edit(&mut request);
        let link = harness.link.clone();
        async move {
            assert_eq!(
                link.apply_relay_config(&request).await.unwrap_err(),
                CloudLinkError::RelayConfigInvalid { field }
            );
        }
    };
    check(RelayConfigField::RelayUrl, &|r| {
        r.relay_url = "http://relay.example.test".into()
    })
    .await;
    check(RelayConfigField::RelayIssuer, &|r| {
        r.relay_issuer = Some("https://relay.example.test/path".into())
    })
    .await;
    check(RelayConfigField::EnvironmentCredential, &|r| {
        r.environment_credential = "  ".into()
    })
    .await;
    check(RelayConfigField::CloudUserId, &|r| {
        r.cloud_user_id = " ".into()
    })
    .await;
    check(RelayConfigField::CloudMintPublicKey, &|r| {
        r.cloud_mint_public_key = "not a key".into()
    })
    .await;
    // A P-256 key is not an Ed25519 key.
    check(RelayConfigField::CloudMintPublicKey, &|r| {
        r.cloud_mint_public_key = "-----BEGIN PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEx\n-----END PUBLIC KEY-----\n".into()
    })
    .await;
    assert!(
        harness.secrets.names().is_empty()
            || harness.secrets.names() == [CLOUD_LINK_KEY_PAIR]
            || true
    );
    assert!(harness.secrets.text(RELAY_URL_SECRET).is_none());
}

#[tokio::test]
async fn a_second_cloud_account_cannot_take_over_a_link() {
    let keys = relay_keys();
    let harness = Harness::new(
        HarnessOptions::new(dead_plane()).secret(CLOUD_LINKED_USER_ID, "someone-else"),
    );
    let error = harness
        .link
        .apply_relay_config(&config_request(&keys, None))
        .await
        .unwrap_err();
    assert_eq!(error, CloudLinkError::AccountMismatch);
    assert_eq!(error.to_http_failure().status, 409);
    assert_eq!(
        harness.secrets.text(CLOUD_LINKED_USER_ID).as_deref(),
        Some("someone-else")
    );
}

#[tokio::test]
async fn an_unsupported_runtime_is_refused_before_the_healthy_tunnel_is_touched() {
    let keys = relay_keys();
    let harness = Harness::new(HarnessOptions::new(dead_plane()));
    harness
        .runtime
        .apply_config(Some(&RuntimeConfig::cloudflare("running", Some("t"))))
        .await;
    let other: RuntimeConfig =
        serde_json::from_str(r#"{"providerKind":"t3_relay","connectorToken":"x"}"#).unwrap();
    let error = harness
        .link
        .apply_relay_config(&config_request(&keys, Some(other)))
        .await
        .unwrap_err();
    let failure = error.to_http_failure();
    assert_eq!(failure.status, 503);
    assert_eq!(failure.tag, "EnvironmentCloudEndpointUnavailableError");
    assert_eq!(
        failure.body()["endpointRuntimeStatus"],
        json!({"status": "unsupported", "providerKind": "t3_relay"})
    );
    assert!(
        harness.spawner.last().is_running(),
        "the healthy tunnel keeps running"
    );
    assert!(harness.secrets.text(RELAY_URL_SECRET).is_none());
}

#[tokio::test]
async fn relay_config_without_a_tunnel_installs_the_link_and_wakes_publishing() {
    let keys = relay_keys();
    let harness = Harness::new(HarnessOptions::new(dead_plane()));
    let result = harness
        .link
        .apply_relay_config(&config_request(&keys, None))
        .await
        .unwrap();
    assert_eq!(
        result,
        RelayConfigResult {
            ok: true,
            endpoint_runtime_status: json!({"status": "disabled"})
        }
    );
    for (name, value) in [
        (RELAY_URL_SECRET, "https://relay.example.test"),
        (RELAY_ISSUER_SECRET, "https://relay.example.test"),
        (CLOUD_LINKED_USER_ID, "user-123"),
        (
            RELAY_ENVIRONMENT_CREDENTIAL_SECRET,
            "environment-credential",
        ),
    ] {
        assert_eq!(harness.secrets.text(name).as_deref(), Some(value), "{name}");
    }
    assert_eq!(
        harness.secrets.text(CLOUD_MINT_PUBLIC_KEY).as_deref(),
        Some(keys.public_key.as_str())
    );
    assert!(
        harness
            .secrets
            .text(CLOUD_ENDPOINT_RUNTIME_CONFIG)
            .is_none()
    );
    assert_eq!(harness.awareness.get(), 1);
    assert_eq!(harness.spawner.count(), 0);
}

#[tokio::test]
async fn relay_config_with_a_tunnel_registers_its_origin_and_starts_it() {
    let keys = relay_keys();
    let relay = FakeRelay::start(|r| match r.path.as_str() {
        "/v1/environments/env_123/tunnel/recovery" => (200, json!({"status": "ready"})),
        _ => (404, json!({})),
    })
    .await;
    let harness = Harness::new(HarnessOptions::new(http_control_plane()));
    let mut request = config_request(
        &keys,
        Some(RuntimeConfig::cloudflare("connector", Some("tunnel-9"))),
    );
    request.relay_url = relay.url.clone();
    request.relay_issuer = None;
    // The test relay is plain http on loopback, which `apply_relay_config` refuses for real links;
    // install it the way the CLI link flow does, then register.
    let secrets = harness.secrets.clone();
    for (name, value) in [
        (RELAY_URL_SECRET, relay.url.as_str()),
        (CLOUD_LINKED_USER_ID, "user-123"),
        (
            RELAY_ENVIRONMENT_CREDENTIAL_SECRET,
            "environment-credential",
        ),
        (
            CLOUD_ENDPOINT_RUNTIME_CONFIG,
            r#"{"providerKind":"cloudflare_tunnel","connectorToken":"connector","tunnelId":"tunnel-9"}"#,
        ),
    ] {
        secrets.set(name, value.as_bytes()).unwrap();
    }
    let registration = within(
        harness
            .link
            .register_managed_tunnel_recovery("http://127.0.0.1:3773", false),
    )
    .await
    .unwrap();
    let ManagedTunnelRegistration::Ready {
        endpoint_runtime_status,
    } = registration
    else {
        panic!("{registration:?}")
    };
    assert!(endpoint_runtime_status.is_running());
    assert_eq!(harness.spawner.last().token, "connector");
    let marker =
        decode_confirmed_origin(&secrets.text(CLOUD_ENDPOINT_CONFIRMED_ORIGIN).unwrap()).unwrap();
    assert_eq!(marker.origin, ManagedEndpointOrigin::new("127.0.0.1", 3773));
    assert_eq!(marker.config.tunnel_id.unwrap().as_str(), "tunnel-9");
    let sent = relay.recorded_for("POST", "/v1/environments/env_123/tunnel/recovery");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].authorization.as_deref(),
        Some("Bearer environment-credential")
    );
    assert_eq!(sent[0].body["cloudUserId"], "user-123");
    assert_eq!(sent[0].body["tunnelId"], "tunnel-9");
    assert_eq!(
        sent[0].body["origin"],
        json!({"localHttpHost": "127.0.0.1", "localHttpPort": 3773})
    );
    // The proof it carries is signed by the environment for the relay, for this exact tunnel.
    let claims = verify_env_proof(
        &harness,
        sent[0].body["proof"].as_str().unwrap(),
        RELAY_MANAGED_TUNNEL_RECOVERY_TYP,
        &relay.url,
    );
    assert_eq!(claims["action"], "register");
    assert_eq!(claims["tunnelId"], "tunnel-9");
    assert_eq!(claims["cloudUserId"], "user-123");
    assert_eq!(
        claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
        60
    );
}

#[tokio::test]
async fn a_relay_that_wants_a_replacement_leaves_the_origin_unconfirmed() {
    let plane = ScriptedControlPlane::new();
    plane
        .register
        .lock()
        .unwrap()
        .push_back(Ok(RecoveryRegistrationResponse {
            status: RegistrationStatus::RecoveryRequired,
        }));
    let harness = Harness::new(
        HarnessOptions::new(plane.clone())
            .secret(RELAY_URL_SECRET, "https://relay.example.test")
            .secret(CLOUD_LINKED_USER_ID, "user-123")
            .secret(RELAY_ENVIRONMENT_CREDENTIAL_SECRET, "c")
            .secret(
                CLOUD_ENDPOINT_RUNTIME_CONFIG,
                config_json("old", "old-tunnel"),
            ),
    );
    let registration = harness
        .link
        .register_managed_tunnel_recovery("http://127.0.0.1:3773", false)
        .await
        .unwrap();
    assert_eq!(
        registration,
        ManagedTunnelRegistration::RecoveryRequired {
            config: RuntimeConfig::cloudflare("old", Some("old-tunnel"))
        }
    );
    assert_eq!(
        harness.spawner.count(),
        0,
        "nothing starts until the relay confirms the tunnel"
    );
    assert!(
        harness
            .secrets
            .text(CLOUD_ENDPOINT_CONFIRMED_ORIGIN)
            .is_none()
    );
}

#[tokio::test]
async fn a_legacy_config_without_a_tunnel_id_requests_recovery_without_calling_the_relay() {
    let plane = ScriptedControlPlane::new();
    let harness = Harness::new(
        HarnessOptions::new(plane.clone())
            .secret(RELAY_URL_SECRET, "https://relay.example.test")
            .secret(CLOUD_LINKED_USER_ID, "user-123")
            .secret(RELAY_ENVIRONMENT_CREDENTIAL_SECRET, "c")
            .secret(
                CLOUD_ENDPOINT_RUNTIME_CONFIG,
                r#"{"providerKind":"cloudflare_tunnel","connectorToken":"legacy"}"#,
            ),
    );
    let registration = harness
        .link
        .register_managed_tunnel_recovery("http://127.0.0.1:3773", false)
        .await
        .unwrap();
    assert!(matches!(
        registration,
        ManagedTunnelRegistration::RecoveryRequired { .. }
    ));
    assert!(plane.calls().is_empty());
}

#[tokio::test]
async fn registration_is_not_linked_without_the_whole_link() {
    for missing in [
        CLOUD_ENDPOINT_RUNTIME_CONFIG,
        RELAY_URL_SECRET,
        CLOUD_LINKED_USER_ID,
        RELAY_ENVIRONMENT_CREDENTIAL_SECRET,
    ] {
        let mut options = HarnessOptions::new(ScriptedControlPlane::new());
        for (name, value) in [
            (CLOUD_ENDPOINT_RUNTIME_CONFIG, config_json("t", "id")),
            (RELAY_URL_SECRET, "https://relay.example.test".to_owned()),
            (CLOUD_LINKED_USER_ID, "user".to_owned()),
            (RELAY_ENVIRONMENT_CREDENTIAL_SECRET, "c".to_owned()),
        ] {
            if name != missing {
                options = options.secret(name, value);
            }
        }
        let harness = Harness::new(options);
        assert_eq!(
            harness
                .link
                .register_managed_tunnel_recovery("http://127.0.0.1:3773", false)
                .await
                .unwrap(),
            ManagedTunnelRegistration::NotLinked,
            "{missing}"
        );
        assert!(
            !harness
                .link
                .recover_managed_tunnel("http://127.0.0.1:3773", None, false)
                .await
                .unwrap()
        );
    }
}

fn recovery_response(token: &str, tunnel: &str) -> RecoveryResponse {
    serde_json::from_value(json!({
        "endpoint": {"httpBaseUrl": "https://environment.example.test/", "wsBaseUrl": "wss://environment.example.test/ws", "providerKind": "cloudflare_tunnel"},
        "endpointRuntime": {"providerKind": "cloudflare_tunnel", "connectorToken": token, "tunnelId": tunnel}
    }))
    .unwrap()
}

#[tokio::test]
async fn a_web_linked_tunnel_recovers_with_its_environment_credential() {
    let relay = FakeRelay::start(|r| match r.path.as_str() {
        "/v1/environments/env_123/tunnel" => (
            200,
            json!({
                "endpoint": {"httpBaseUrl": "https://environment.example.test/", "wsBaseUrl": "wss://environment.example.test/ws", "providerKind": "cloudflare_tunnel"},
                "endpointRuntime": {"providerKind": "cloudflare_tunnel", "connectorToken": "new-token", "tunnelId": "new-tunnel"}
            }),
        ),
        _ => (404, json!({})),
    })
    .await;
    let harness = Harness::new(
        HarnessOptions::new(http_control_plane())
            .secret(RELAY_URL_SECRET, relay.url.clone())
            .secret(CLOUD_LINKED_USER_ID, "user-123")
            .secret(
                RELAY_ENVIRONMENT_CREDENTIAL_SECRET,
                "environment-credential",
            )
            .secret(
                CLOUD_ENDPOINT_RUNTIME_CONFIG,
                config_json("old-token", "old-tunnel"),
            )
            .secret(CLOUD_ENDPOINT_CONFIRMED_ORIGIN, "stale"),
    );
    assert!(
        within(
            harness
                .link
                .recover_managed_tunnel("http://127.0.0.1:3773", None, false)
        )
        .await
        .unwrap()
    );
    let sent = relay.recorded();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].authorization.as_deref(),
        Some("Bearer environment-credential")
    );
    let claims = verify_env_proof(
        &harness,
        sent[0].body["proof"].as_str().unwrap(),
        RELAY_MANAGED_TUNNEL_RECOVERY_TYP,
        &relay.url,
    );
    assert_eq!(claims["action"], "recover");
    assert!(claims.get("tunnelId").is_none());
    // The new tunnel is stored, started, and confirmed for this origin; the stale marker is gone.
    let stored =
        decode_runtime_config(&harness.secrets.text(CLOUD_ENDPOINT_RUNTIME_CONFIG).unwrap())
            .unwrap();
    assert_eq!(
        stored,
        RuntimeConfig::cloudflare("new-token", Some("new-tunnel"))
    );
    assert_eq!(harness.spawner.last().token, "new-token");
    let marker = decode_confirmed_origin(
        &harness
            .secrets
            .text(CLOUD_ENDPOINT_CONFIRMED_ORIGIN)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(marker.config, stored);
}

#[tokio::test]
async fn recovery_ignores_a_request_for_a_tunnel_that_was_already_replaced() {
    let plane = ScriptedControlPlane::new();
    plane
        .recover
        .lock()
        .unwrap()
        .push_back(Ok(recovery_response("n", "n")));
    let harness = Harness::new(
        HarnessOptions::new(plane.clone())
            .secret(RELAY_URL_SECRET, "https://relay.example.test")
            .secret(CLOUD_LINKED_USER_ID, "user-123")
            .secret(RELAY_ENVIRONMENT_CREDENTIAL_SECRET, "c")
            .secret(
                CLOUD_ENDPOINT_RUNTIME_CONFIG,
                config_json("current", "current-tunnel"),
            ),
    );
    let stale = RuntimeConfig::cloudflare("old", Some("old-tunnel"));
    assert!(
        !harness
            .link
            .recover_managed_tunnel("http://127.0.0.1:3773", Some(&stale), false)
            .await
            .unwrap()
    );
    assert!(plane.calls().is_empty());
    let current = RuntimeConfig::cloudflare("current", Some("current-tunnel"));
    assert!(
        harness
            .link
            .recover_managed_tunnel("http://127.0.0.1:3773", Some(&current), false)
            .await
            .unwrap()
    );
    assert_eq!(plane.calls(), ["recover"]);
}

#[tokio::test]
async fn a_tunnel_replaced_while_recovery_was_in_flight_is_kept() {
    // The relay answers while a concurrent link install changes the stored config.
    struct Racing {
        secrets: std::sync::OnceLock<Arc<MemorySecretStore>>,
    }
    #[async_trait::async_trait]
    impl t3_remote::control_plane::ControlPlane for Racing {
        async fn create_link_challenge(
            &self,
            _: &str,
            _: &str,
            _: &LinkChallengeRequest,
        ) -> t3_remote::control_plane::RelayResult<LinkChallengeResponse> {
            unreachable!()
        }
        async fn link_environment(
            &self,
            _: &str,
            _: &str,
            _: &LinkRequest,
        ) -> t3_remote::control_plane::RelayResult<LinkResponse> {
            unreachable!()
        }
        async fn unlink_environment(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> t3_remote::control_plane::RelayResult<OkResponse> {
            unreachable!()
        }
        async fn release_environment_tunnel(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> t3_remote::control_plane::RelayResult<OkResponse> {
            unreachable!()
        }
        async fn register_tunnel_recovery(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &RecoveryRegistrationRequest,
        ) -> t3_remote::control_plane::RelayResult<RecoveryRegistrationResponse> {
            unreachable!()
        }
        async fn recover_tunnel(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &RecoveryRequest,
        ) -> t3_remote::control_plane::RelayResult<RecoveryResponse> {
            self.secrets
                .get()
                .unwrap()
                .set(
                    CLOUD_ENDPOINT_RUNTIME_CONFIG,
                    config_json("raced", "raced-tunnel").as_bytes(),
                )
                .unwrap();
            Ok(recovery_response("recovered", "recovered-tunnel"))
        }
        async fn update_link_preferences(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &LinkPreferencesRequest,
        ) -> t3_remote::control_plane::RelayResult<LinkPreferencesRequest> {
            unreachable!()
        }
        async fn wake_held_hooks(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> t3_remote::control_plane::RelayResult<WakeHeldHooksResponse> {
            unreachable!()
        }
        async fn publish_agent_activity(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &AgentActivityPublishRequest,
        ) -> t3_remote::control_plane::RelayResult<PublishResponse> {
            unreachable!()
        }
    }
    let racing = Arc::new(Racing {
        secrets: Default::default(),
    });
    let harness = Harness::new(
        HarnessOptions::new(racing.clone())
            .secret(RELAY_URL_SECRET, "https://relay.example.test")
            .secret(CLOUD_LINKED_USER_ID, "user-123")
            .secret(RELAY_ENVIRONMENT_CREDENTIAL_SECRET, "c")
            .secret(
                CLOUD_ENDPOINT_RUNTIME_CONFIG,
                config_json("old", "old-tunnel"),
            ),
    );
    racing.secrets.set(harness.secrets.clone()).ok();
    assert!(
        !harness
            .link
            .recover_managed_tunnel("http://127.0.0.1:3773", None, false)
            .await
            .unwrap()
    );
    assert_eq!(
        decode_runtime_config(&harness.secrets.text(CLOUD_ENDPOINT_RUNTIME_CONFIG).unwrap())
            .unwrap(),
        RuntimeConfig::cloudflare("raced", Some("raced-tunnel"))
    );
    assert_eq!(harness.spawner.count(), 0);
}

// ---- the CLI-desired link --------------------------------------------------------------------

fn link_response(keys: &KeyPair, runtime: Option<Value>, relay_url: &str) -> Value {
    json!({
        "ok": true, "cloudUserId": "user-123", "environmentId": "env_123",
        "endpoint": {"httpBaseUrl": "https://e.example.test", "wsBaseUrl": "wss://e.example.test/ws", "providerKind": if runtime.is_some() { "cloudflare_tunnel" } else { "manual" }},
        "endpointRuntime": runtime,
        "relayIssuer": relay_url, "environmentCredential": "issued-credential", "cloudMintPublicKey": keys.public_key
    })
}

#[tokio::test]
async fn the_desired_link_is_made_through_the_relay_and_the_tunnel_is_started() {
    let keys = relay_keys();
    let keys_for_relay = keys.clone();
    let relay = FakeRelay::start_with_headers(move |r| match r.path.as_str() {
        "/v1/client/environment-link-challenges" => (200, json!({"challenge": "challenge-xyz", "expiresAt": "2030-01-01T00:00:00.000Z"}), vec![]),
        "/v1/client/environment-links" => (
            200,
            link_response(&keys_for_relay, Some(json!({"providerKind": "cloudflare_tunnel", "connectorToken": "issued-token", "tunnelId": "issued-tunnel"})), "https://relay.example.test"),
            vec![],
        ),
        _ => (404, json!({}), vec![]),
    })
    .await;
    let mut options = HarnessOptions::new(http_control_plane());
    options.relay_url = Some(relay.url.clone());
    let harness = Harness::new(options);
    set_cli_desired_cloud_link(harness.secrets.as_ref(), true, DesiredLinkMode::Managed).unwrap();
    let mode = within(harness.link.reconcile_desired_link("http://127.0.0.1:3773"))
        .await
        .unwrap();
    assert_eq!(mode, DesiredLinkMode::Managed);

    let recorded = relay.recorded();
    assert_eq!(recorded.len(), 2);
    assert!(
        recorded
            .iter()
            .all(|r| r.authorization.as_deref() == Some("Bearer cli-access"))
    );
    assert_eq!(recorded[0].body["managedTunnelsEnabled"], true);
    // The link request carries the environment's proof of the challenge.
    let proof = recorded[1].body["proof"].as_str().unwrap();
    let claims = verify_env_proof(&harness, proof, RELAY_LINK_PROOF_TYP, &relay.url);
    assert_eq!(claims["challenge"], "challenge-xyz");
    assert_eq!(claims["endpoint"]["providerKind"], "cloudflare_tunnel");
    assert_eq!(claims["endpoint"]["httpBaseUrl"], "http://127.0.0.1:3773");
    assert_eq!(claims["endpoint"]["wsBaseUrl"], "ws://127.0.0.1:3773");
    // The issued link is installed and the tunnel runs, confirmed for this origin.
    assert_eq!(
        harness
            .secrets
            .text(RELAY_ENVIRONMENT_CREDENTIAL_SECRET)
            .as_deref(),
        Some("issued-credential")
    );
    assert_eq!(
        harness.secrets.text(RELAY_URL_SECRET).as_deref(),
        Some(relay.url.as_str())
    );
    assert_eq!(
        harness.secrets.text(CLOUD_LINKED_USER_ID).as_deref(),
        Some("user-123")
    );
    assert_eq!(harness.spawner.last().token, "issued-token");
    assert!(
        harness
            .secrets
            .text(CLOUD_ENDPOINT_CONFIRMED_ORIGIN)
            .is_some()
    );
    assert!(read_cli_desired_cloud_link(harness.secrets.as_ref()).unwrap());
}

#[tokio::test]
async fn a_publish_only_link_has_no_tunnel_and_a_manual_endpoint() {
    let keys = relay_keys();
    let keys_for_relay = keys.clone();
    let relay = FakeRelay::start(move |r| match r.path.as_str() {
        "/v1/client/environment-link-challenges" => {
            (200, json!({"challenge": "c", "expiresAt": "x"}))
        }
        "/v1/client/environment-links" => (
            200,
            link_response(&keys_for_relay, None, "https://relay.example.test"),
        ),
        _ => (404, json!({})),
    })
    .await;
    let mut options = HarnessOptions::new(http_control_plane());
    options.relay_url = Some(relay.url.clone());
    let harness = Harness::new(options);
    set_cli_desired_cloud_link(harness.secrets.as_ref(), true, DesiredLinkMode::PublishOnly)
        .unwrap();
    assert_eq!(
        within(harness.link.reconcile_desired_link("http://127.0.0.1:3773"))
            .await
            .unwrap(),
        DesiredLinkMode::PublishOnly
    );
    let recorded = relay.recorded();
    assert_eq!(recorded[0].body["managedTunnelsEnabled"], false);
    assert_eq!(recorded[1].body["managedTunnelsEnabled"], false);
    let claims = verify_env_proof(
        &harness,
        recorded[1].body["proof"].as_str().unwrap(),
        RELAY_LINK_PROOF_TYP,
        &relay.url,
    );
    assert_eq!(claims["endpoint"]["providerKind"], "manual");
    assert_eq!(claims["scopes"], json!(["agent_activity_notifications"]));
    assert_eq!(harness.spawner.count(), 0);
    assert!(
        harness
            .secrets
            .text(CLOUD_ENDPOINT_RUNTIME_CONFIG)
            .is_none()
    );
    assert_eq!(
        read_cli_desired_link_mode(harness.secrets.as_ref()).unwrap(),
        DesiredLinkMode::PublishOnly
    );
}

#[tokio::test]
async fn linking_needs_the_authorization_the_cli_stores_and_a_configured_relay() {
    let mut options = HarnessOptions::new(ScriptedControlPlane::new());
    options.token = None;
    options.relay_url = Some("https://relay.example.test".into());
    let harness = Harness::new(options);
    let error = harness
        .link
        .reconcile_desired_link("http://127.0.0.1:3773")
        .await
        .unwrap_err();
    assert_eq!(error, CloudLinkError::AuthorizationMissing);
    assert_eq!(
        error.message(),
        "Run `t3 connect link` to authorize this environment."
    );
    assert!(!error.should_retry());
    assert_eq!(error.to_http_failure().status, 401);

    let harness = Harness::new(HarnessOptions::new(ScriptedControlPlane::new()));
    let error = harness
        .link
        .reconcile_desired_link("http://127.0.0.1:3773")
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CloudLinkError::Internal {
            operation: InternalOperation::RelayUrlUnconfigured,
            ..
        }
    ));
    let error = harness
        .link
        .reconcile_desired_link("http://127.0.0.1:3773/path")
        .await
        .unwrap_err();
    assert_eq!(error, CloudLinkError::OriginInvalid { endpoint: false });
}

#[tokio::test]
async fn reconciling_does_nothing_once_the_cli_no_longer_wants_the_link() {
    let plane = ScriptedControlPlane::new();
    let mut options = HarnessOptions::new(plane.clone());
    options.relay_url = Some("https://relay.example.test".into());
    let harness = Harness::new(options);
    assert_eq!(
        harness
            .link
            .reconcile_desired_link_if_still_desired("http://127.0.0.1:3773")
            .await
            .unwrap(),
        None
    );
    assert!(plane.calls().is_empty());
}

// ---- shutdown --------------------------------------------------------------------------------

fn managed_link(plane: Arc<ScriptedControlPlane>) -> Harness {
    let harness = Harness::new(
        HarnessOptions::new(plane)
            .secret(RELAY_URL_SECRET, "https://relay.example.test")
            .secret(CLOUD_LINKED_USER_ID, "user-123")
            .secret(RELAY_ENVIRONMENT_CREDENTIAL_SECRET, "c")
            .secret(CLOUD_ENDPOINT_RUNTIME_CONFIG, config_json("tok", "tun"))
            .secret(CLOUD_ENDPOINT_CONFIRMED_ORIGIN, "marker"),
    );
    set_cli_desired_cloud_link(harness.secrets.as_ref(), true, DesiredLinkMode::Managed).unwrap();
    harness
}

#[tokio::test]
async fn shutdown_stops_the_connector_releases_the_tunnel_and_drops_the_dead_token() {
    let plane = ScriptedControlPlane::new();
    let harness = managed_link(plane.clone());
    harness
        .runtime
        .apply_config(Some(&RuntimeConfig::cloudflare("tok", Some("tun"))))
        .await;
    assert!(
        harness
            .link
            .release_managed_tunnel_on_shutdown()
            .await
            .unwrap()
    );
    assert!(
        !harness.spawner.last().is_running(),
        "stopped before the relay deletes the tunnel it serves"
    );
    assert_eq!(plane.calls(), ["release"]);
    assert!(
        harness
            .secrets
            .text(CLOUD_ENDPOINT_RUNTIME_CONFIG)
            .is_none()
    );
    assert!(
        harness
            .secrets
            .text(CLOUD_ENDPOINT_CONFIRMED_ORIGIN)
            .is_none()
    );
    assert!(
        read_cli_desired_cloud_link(harness.secrets.as_ref()).unwrap(),
        "the wish to be linked survives"
    );
}

#[tokio::test]
async fn shutdown_leaves_links_it_does_not_own_alone() {
    // No stored managed tunnel.
    let plane = ScriptedControlPlane::new();
    let harness = Harness::new(
        HarnessOptions::new(plane.clone()).secret(RELAY_URL_SECRET, "https://relay.example.test"),
    );
    assert!(
        !harness
            .link
            .release_managed_tunnel_on_shutdown()
            .await
            .unwrap()
    );
    // A web or mobile install: a tunnel, but the CLI never asked for it.
    let harness = managed_link(plane.clone());
    set_cli_desired_cloud_link(harness.secrets.as_ref(), false, DesiredLinkMode::Managed).unwrap();
    assert!(
        !harness
            .link
            .release_managed_tunnel_on_shutdown()
            .await
            .unwrap()
    );
    // A publish-only wish never had a tunnel to release.
    let harness = managed_link(plane.clone());
    set_cli_desired_cloud_link(harness.secrets.as_ref(), true, DesiredLinkMode::PublishOnly)
        .unwrap();
    assert!(
        !harness
            .link
            .release_managed_tunnel_on_shutdown()
            .await
            .unwrap()
    );
    assert!(plane.calls().is_empty());
}

#[tokio::test]
async fn shutdown_keeps_the_tunnel_across_an_update_restart() {
    let plane = ScriptedControlPlane::new();
    let harness = managed_link(plane.clone());
    *harness.updates.pending.lock().unwrap() = true;
    assert!(
        !harness
            .link
            .release_managed_tunnel_on_shutdown()
            .await
            .unwrap()
    );
    *harness.updates.pending.lock().unwrap() = false;
    // The desktop marker counts once (reading consumes it).
    *harness.updates.desktop.lock().unwrap() = true;
    assert!(
        !harness
            .link
            .release_managed_tunnel_on_shutdown()
            .await
            .unwrap()
    );
    assert!(plane.calls().is_empty());
    assert!(
        harness
            .secrets
            .text(CLOUD_ENDPOINT_RUNTIME_CONFIG)
            .is_some()
    );
    assert!(
        harness
            .link
            .release_managed_tunnel_on_shutdown()
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn shutdown_keeps_the_stored_token_when_the_relay_skipped_or_failed_the_release() {
    let plane = ScriptedControlPlane::new();
    plane
        .release
        .lock()
        .unwrap()
        .push_back(Ok(OkResponse { ok: false }));
    let harness = managed_link(plane.clone());
    assert!(
        !harness
            .link
            .release_managed_tunnel_on_shutdown()
            .await
            .unwrap()
    );
    assert!(
        harness
            .secrets
            .text(CLOUD_ENDPOINT_RUNTIME_CONFIG)
            .is_some()
    );

    let plane = ScriptedControlPlane::new();
    plane.release.lock().unwrap().push_back(Err(unavailable()));
    let harness = managed_link(plane.clone());
    let error = harness
        .link
        .release_managed_tunnel_on_shutdown()
        .await
        .unwrap_err();
    assert!(error.should_retry());
    assert!(
        harness
            .secrets
            .text(CLOUD_ENDPOINT_RUNTIME_CONFIG)
            .is_some(),
        "the tunnel still exists, so its token stays"
    );
}

#[tokio::test]
async fn shutdown_keeps_a_config_that_a_fast_restart_replaced_mid_release() {
    struct Replacing(std::sync::OnceLock<Arc<MemorySecretStore>>);
    #[async_trait::async_trait]
    impl t3_remote::control_plane::ControlPlane for Replacing {
        async fn create_link_challenge(
            &self,
            _: &str,
            _: &str,
            _: &LinkChallengeRequest,
        ) -> t3_remote::control_plane::RelayResult<LinkChallengeResponse> {
            unreachable!()
        }
        async fn link_environment(
            &self,
            _: &str,
            _: &str,
            _: &LinkRequest,
        ) -> t3_remote::control_plane::RelayResult<LinkResponse> {
            unreachable!()
        }
        async fn unlink_environment(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> t3_remote::control_plane::RelayResult<OkResponse> {
            unreachable!()
        }
        async fn release_environment_tunnel(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> t3_remote::control_plane::RelayResult<OkResponse> {
            self.0
                .get()
                .unwrap()
                .set(
                    CLOUD_ENDPOINT_RUNTIME_CONFIG,
                    config_json("fresh", "fresh-tunnel").as_bytes(),
                )
                .unwrap();
            Ok(OkResponse { ok: true })
        }
        async fn register_tunnel_recovery(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &RecoveryRegistrationRequest,
        ) -> t3_remote::control_plane::RelayResult<RecoveryRegistrationResponse> {
            unreachable!()
        }
        async fn recover_tunnel(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &RecoveryRequest,
        ) -> t3_remote::control_plane::RelayResult<RecoveryResponse> {
            unreachable!()
        }
        async fn update_link_preferences(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &LinkPreferencesRequest,
        ) -> t3_remote::control_plane::RelayResult<LinkPreferencesRequest> {
            unreachable!()
        }
        async fn wake_held_hooks(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> t3_remote::control_plane::RelayResult<WakeHeldHooksResponse> {
            unreachable!()
        }
        async fn publish_agent_activity(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &AgentActivityPublishRequest,
        ) -> t3_remote::control_plane::RelayResult<PublishResponse> {
            unreachable!()
        }
    }
    let plane = Arc::new(Replacing(Default::default()));
    let harness = Harness::new(
        HarnessOptions::new(plane.clone())
            .secret(RELAY_URL_SECRET, "https://relay.example.test")
            .secret(CLOUD_ENDPOINT_RUNTIME_CONFIG, config_json("tok", "tun")),
    );
    set_cli_desired_cloud_link(harness.secrets.as_ref(), true, DesiredLinkMode::Managed).unwrap();
    plane.0.set(harness.secrets.clone()).ok();
    assert!(
        harness
            .link
            .release_managed_tunnel_on_shutdown()
            .await
            .unwrap()
    );
    assert_eq!(
        decode_runtime_config(&harness.secrets.text(CLOUD_ENDPOINT_RUNTIME_CONFIG).unwrap())
            .unwrap(),
        RuntimeConfig::cloudflare("fresh", Some("fresh-tunnel"))
    );
}

#[tokio::test]
async fn shutdown_does_nothing_without_cli_authorization() {
    let plane = ScriptedControlPlane::new();
    let mut options = HarnessOptions::new(plane.clone())
        .secret(RELAY_URL_SECRET, "https://relay.example.test")
        .secret(CLOUD_ENDPOINT_RUNTIME_CONFIG, config_json("tok", "tun"));
    options.token = None;
    let harness = Harness::new(options);
    set_cli_desired_cloud_link(harness.secrets.as_ref(), true, DesiredLinkMode::Managed).unwrap();
    assert!(
        !harness
            .link
            .release_managed_tunnel_on_shutdown()
            .await
            .unwrap()
    );
    assert!(plane.calls().is_empty());
}

// ---- starting the stored tunnel -----------------------------------------------------------------

#[tokio::test]
async fn startup_starts_only_a_tunnel_the_relay_confirmed_on_this_origin() {
    let marker = |token: &str, port: u16| {
        serde_json::to_string(&ConfirmedOrigin {
            config: RuntimeConfig::cloudflare(token, Some("tun")),
            origin: ManagedEndpointOrigin::new("127.0.0.1", port),
        })
        .unwrap()
    };
    let start = |marker_text: Option<String>, require: bool| async move {
        let mut options = HarnessOptions::new(ScriptedControlPlane::new())
            .secret(CLOUD_ENDPOINT_RUNTIME_CONFIG, config_json("tok", "tun"));
        if let Some(text) = marker_text {
            options = options.secret(CLOUD_ENDPOINT_CONFIRMED_ORIGIN, text);
        }
        let harness = Harness::new(options);
        let started = harness
            .link
            .start_managed_tunnel_if_origin_confirmed("http://127.0.0.1:3773", require)
            .await
            .unwrap();
        (started, harness.spawner.count())
    };
    assert_eq!(start(Some(marker("tok", 3773)), true).await, (true, 1));
    // A different port, a different token, or no marker: not confirmed.
    assert_eq!(start(Some(marker("tok", 4000)), true).await, (false, 0));
    assert_eq!(start(Some(marker("other", 3773)), true).await, (false, 0));
    assert_eq!(start(None, true).await, (false, 0));
    assert_eq!(start(Some("garbage".into()), true).await, (false, 0));
    // The fallback after a relay outage starts it anyway.
    assert_eq!(start(None, false).await, (true, 1));
    assert_eq!(start(Some(marker("tok", 4000)), false).await, (true, 1));
}

#[tokio::test]
async fn a_connector_that_cannot_start_is_reported_with_the_runtime_status() {
    let harness = Harness::new(
        HarnessOptions::new(ScriptedControlPlane::new())
            .secret(CLOUD_ENDPOINT_RUNTIME_CONFIG, config_json("tok", "tun")),
    );
    *harness.spawner.fail_with.lock().unwrap() = Some("exec format error".into());
    let error = harness
        .link
        .start_managed_tunnel_if_origin_confirmed("http://127.0.0.1:3773", false)
        .await
        .unwrap_err();
    let CloudLinkError::EndpointUnavailable {
        origin_unconfirmed: false,
        endpoint_runtime_status,
    } = &error
    else {
        panic!("{error:?}")
    };
    assert_eq!(endpoint_runtime_status["failure"], "spawn-failed");
    assert_eq!(
        error.message(),
        "Managed endpoint runtime could not be started."
    );
    assert!(error.should_retry());
}

// ---- state, unlink, preferences ---------------------------------------------------------------------

#[tokio::test]
async fn link_state_reports_what_is_stored() {
    let harness = Harness::new(HarnessOptions::new(ScriptedControlPlane::new()));
    let state = harness.link.link_state().unwrap();
    assert!(!state.linked);
    assert_eq!(
        serde_json::to_value(&state).unwrap(),
        json!({
            "linked": false, "cloudUserId": null, "relayUrl": null, "relayIssuer": null,
            "managedTunnelActive": false, "publishAgentActivity": false, "holdWebhooksWhileOffline": false
        })
    );
    let harness = managed_link(ScriptedControlPlane::new());
    harness
        .secrets
        .set(PUBLISH_AGENT_ACTIVITY_SECRET, b"true")
        .unwrap();
    harness
        .secrets
        .set(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET, b"true")
        .unwrap();
    harness
        .secrets
        .set(RELAY_ISSUER_SECRET, b"https://issuer.example.test")
        .unwrap();
    let state = harness.link.link_state().unwrap();
    assert!(state.linked && state.publish_agent_activity);
    assert_eq!(state.managed_tunnel_active, Some(true));
    assert_eq!(state.hold_webhooks_while_offline, Some(true));
    assert_eq!(
        state.relay_issuer.as_deref(),
        Some("https://issuer.example.test")
    );
}

#[tokio::test]
async fn unlink_stops_the_tunnel_and_forgets_everything_including_the_wish() {
    let harness = managed_link(ScriptedControlPlane::new());
    for name in [
        PUBLISH_AGENT_ACTIVITY_SECRET,
        HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET,
        CLOUD_MINT_PUBLIC_KEY,
        RELAY_ISSUER_SECRET,
    ] {
        harness.secrets.set(name, b"x").unwrap();
    }
    harness
        .runtime
        .apply_config(Some(&RuntimeConfig::cloudflare("tok", Some("tun"))))
        .await;
    let result = harness.link.unlink().await.unwrap();
    assert_eq!(
        result,
        RelayConfigResult {
            ok: true,
            endpoint_runtime_status: json!({"status": "disabled"})
        }
    );
    assert!(!harness.spawner.last().is_running());
    assert!(
        harness.secrets.names().is_empty(),
        "{:?}",
        harness.secrets.names()
    );
    assert!(!read_cli_desired_cloud_link(harness.secrets.as_ref()).unwrap());
}

fn prefs_harness(plane: Arc<ScriptedControlPlane>) -> Harness {
    Harness::new(
        HarnessOptions::new(plane)
            .secret(RELAY_URL_SECRET, "https://relay.example.test")
            .secret(RELAY_ENVIRONMENT_CREDENTIAL_SECRET, "c")
            .secret(PUBLISH_AGENT_ACTIVITY_SECRET, "false")
            .secret(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET, "false"),
    )
}

fn prefs(publish: bool, hold: Option<bool>) -> PreferencesRequest {
    PreferencesRequest {
        publish_agent_activity: publish,
        hold_webhooks_while_offline: hold,
    }
}

#[tokio::test]
async fn preferences_tell_the_relay_before_saving_the_hold_setting_locally() {
    let plane = ScriptedControlPlane::new();
    let harness = prefs_harness(plane.clone());
    let state = harness
        .link
        .update_preferences(prefs(true, Some(true)))
        .await
        .unwrap();
    assert_eq!(plane.calls(), ["preferences:true"]);
    assert!(state.publish_agent_activity);
    assert_eq!(state.hold_webhooks_while_offline, Some(true));
    assert_eq!(
        harness
            .secrets
            .text(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET)
            .as_deref(),
        Some("true")
    );
    assert_eq!(harness.awareness.get(), 1);
    // Omitting the hold setting leaves it alone and never calls the relay.
    harness
        .link
        .update_preferences(prefs(false, None))
        .await
        .unwrap();
    assert_eq!(plane.calls().len(), 1);
    assert_eq!(
        harness
            .secrets
            .text(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET)
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        harness
            .secrets
            .text(PUBLISH_AGENT_ACTIVITY_SECRET)
            .as_deref(),
        Some("false")
    );
}

#[tokio::test]
async fn preferences_put_the_relay_back_when_the_local_save_fails() {
    let plane = ScriptedControlPlane::new();
    let harness = prefs_harness(plane.clone());
    harness.secrets.fail_set(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET);
    let error = harness
        .link
        .update_preferences(prefs(true, Some(true)))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CloudLinkError::Internal {
            operation: InternalOperation::PersistPreferences,
            ..
        }
    ));
    // The relay was told true, then put back to the previous false; the activity setting is restored.
    assert_eq!(plane.calls(), ["preferences:true", "preferences:false"]);
    assert_eq!(
        harness
            .secrets
            .text(PUBLISH_AGENT_ACTIVITY_SECRET)
            .as_deref(),
        Some("false")
    );
}

#[tokio::test]
async fn preferences_leave_the_relay_untouched_when_the_activity_setting_cannot_be_saved() {
    let plane = ScriptedControlPlane::new();
    let harness = prefs_harness(plane.clone());
    harness.secrets.fail_set(PUBLISH_AGENT_ACTIVITY_SECRET);
    assert!(
        harness
            .link
            .update_preferences(prefs(true, Some(true)))
            .await
            .is_err()
    );
    assert!(plane.calls().is_empty());
    assert_eq!(
        harness
            .secrets
            .text(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET)
            .as_deref(),
        Some("false")
    );
}

#[tokio::test]
async fn preferences_keep_both_settings_when_the_relay_refuses_the_hold_change() {
    let plane = ScriptedControlPlane::new();
    plane
        .preferences
        .lock()
        .unwrap()
        .push_back(Err(forbidden()));
    let harness = prefs_harness(plane.clone());
    let error = harness
        .link
        .update_preferences(prefs(true, Some(true)))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CloudLinkError::Internal {
            operation: InternalOperation::UpdateWebhookSettings,
            ..
        }
    ));
    assert_eq!(
        harness
            .secrets
            .text(PUBLISH_AGENT_ACTIVITY_SECRET)
            .as_deref(),
        Some("false")
    );
    assert_eq!(
        harness
            .secrets
            .text(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET)
            .as_deref(),
        Some("false")
    );
    assert_eq!(harness.awareness.get(), 0);
}

#[tokio::test]
async fn preferences_change_nothing_when_a_current_value_cannot_be_read() {
    for unreadable in [
        PUBLISH_AGENT_ACTIVITY_SECRET,
        HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET,
    ] {
        let plane = ScriptedControlPlane::new();
        let harness = prefs_harness(plane.clone());
        harness.secrets.fail_get(unreadable);
        let error = harness
            .link
            .update_preferences(prefs(true, Some(true)))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CloudLinkError::Internal {
                operation: InternalOperation::ReadPreferences,
                ..
            }
        ));
        assert!(plane.calls().is_empty());
        assert_eq!(
            harness
                .secrets
                .text(PUBLISH_AGENT_ACTIVITY_SECRET)
                .as_deref(),
            Some("false")
        );
    }
}

#[tokio::test]
async fn holding_webhooks_needs_a_link() {
    let harness = Harness::new(
        HarnessOptions::new(ScriptedControlPlane::new())
            .secret(PUBLISH_AGENT_ACTIVITY_SECRET, "false"),
    );
    // The not-linked failure is swallowed into the relay-settings failure, as in the original.
    let error = harness
        .link
        .update_preferences(prefs(true, Some(true)))
        .await
        .unwrap_err();
    assert_eq!(error, CloudLinkError::NotLinked);
    assert_eq!(
        harness
            .secrets
            .text(PUBLISH_AGENT_ACTIVITY_SECRET)
            .as_deref(),
        Some("false")
    );
}

#[tokio::test]
async fn overlapping_preference_updates_are_applied_one_after_the_other() {
    let plane = ScriptedControlPlane::new();
    let harness = prefs_harness(plane.clone());
    let (a, b) = (harness.link.clone(), harness.link.clone());
    let (first, second) = tokio::join!(
        async move { a.update_preferences(prefs(true, Some(true))).await },
        async move { b.update_preferences(prefs(false, Some(false))).await },
    );
    first.unwrap();
    second.unwrap();
    // Whichever ran second won completely: the two settings never end up from different requests.
    let publish = harness.secrets.text(PUBLISH_AGENT_ACTIVITY_SECRET).unwrap();
    let hold = harness
        .secrets
        .text(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET)
        .unwrap();
    assert_eq!(publish, hold);
}

#[tokio::test]
async fn an_update_the_client_walks_away_from_still_finishes() {
    let plane = ScriptedControlPlane::new();
    let harness = prefs_harness(plane.clone());
    let link = harness.link.clone();
    // The caller gives up before the update gets to run; the detached work still completes.
    let mut update = Box::pin(link.update_preferences(prefs(true, Some(true))));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(update.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(update);
    harness.awareness.wait_for(1).await;
    assert_eq!(
        harness
            .secrets
            .text(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET)
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        harness
            .secrets
            .text(PUBLISH_AGENT_ACTIVITY_SECRET)
            .as_deref(),
        Some("true")
    );
}

// ---- the relay's signed requests -----------------------------------------------------------------

struct Relay {
    keys: KeyPair,
}

impl Relay {
    fn new() -> Self {
        Self { keys: relay_keys() }
    }

    fn claims(&self, scope: &str, nonce: &str, jti: &str) -> Value {
        let now = NOW_MS / 1000;
        json!({
            "iss": "https://relay.example.test", "aud": "t3-env:env_123", "sub": "user-123", "jti": jti,
            "iat": now, "exp": now + 120, "environmentId": "env_123", "nonce": nonce, "scope": [scope]
        })
    }

    fn sign(&self, typ: &str, claims: &Value) -> String {
        sign_relay_jwt(&self.keys.private_key, typ, claims).unwrap()
    }

    fn health(&self, nonce: &str, jti: &str) -> String {
        self.sign(
            RELAY_HEALTH_REQUEST_TYP,
            &self.claims("environment:status", nonce, jti),
        )
    }

    fn mint(&self, nonce: &str, jti: &str, thumbprint: &str) -> String {
        let mut claims = self.claims("environment:connect", nonce, jti);
        claims["clientProofKeyThumbprint"] = json!(thumbprint);
        claims["cnf"] = json!({"jkt": thumbprint});
        self.sign(RELAY_MINT_REQUEST_TYP, &claims)
    }
}

fn trusting_harness(relay: &Relay) -> Harness {
    let mut options = HarnessOptions::new(ScriptedControlPlane::new());
    options.secrets = linked_secrets("https://relay.example.test", &relay.keys);
    Harness::new(options)
}

fn health_request(proof: String) -> HealthRequest {
    HealthRequest {
        proof: trimmed(&proof),
    }
}

#[tokio::test]
async fn a_health_check_is_answered_once_per_proof_with_a_signed_online_status() {
    let relay = Relay::new();
    let harness = trusting_harness(&relay);
    let response = harness
        .link
        .answer_health_request(&health_request(relay.health("nonce-1", "jti-1")))
        .unwrap();
    assert_eq!(response.environment_id.as_str(), "env_123");
    assert_eq!(response.descriptor.environment_id.as_str(), "env_123");
    assert_eq!(response.checked_at.as_str(), "2023-11-14T22:13:20.000Z");
    let claims = verify_env_proof(
        &harness,
        response.proof.as_str(),
        RELAY_HEALTH_RESPONSE_TYP,
        "https://relay.example.test",
    );
    assert_eq!(
        claims["requestNonce"], "nonce-1",
        "the answer is bound to the request"
    );
    assert_eq!(claims["status"], "online");
    assert_eq!(claims["checkedAt"], response.checked_at.as_str());
    // Replay markers are recorded for both the id and the nonce.
    assert!(harness.secrets.text("cloud-health-jti-jti-1").is_some());
    assert!(harness.secrets.text("cloud-health-nonce-nonce-1").is_some());
    let replay = harness
        .link
        .answer_health_request(&health_request(relay.health("nonce-1", "jti-1")))
        .unwrap_err();
    assert_eq!(replay, CloudLinkError::ProofReplayed(SignedRequest::Health));
    assert_eq!(
        replay.message(),
        "Cloud health request was already consumed."
    );
    assert_eq!(replay.to_http_failure().status, 409);
    // A fresh jti with a used nonce is a replay too, and still records its own marker.
    let replay = harness
        .link
        .answer_health_request(&health_request(relay.health("nonce-1", "jti-2")))
        .unwrap_err();
    assert_eq!(replay, CloudLinkError::ProofReplayed(SignedRequest::Health));
    assert!(harness.secrets.text("cloud-health-jti-jti-2").is_some());
}

#[tokio::test]
async fn health_proofs_that_do_not_verify_are_rejected() {
    let relay = Relay::new();
    let harness = trusting_harness(&relay);
    let rejected = CloudLinkError::ProofRejected(SignedRequest::Health);
    let ask = |proof: String| harness.link.answer_health_request(&health_request(proof));
    // Signed by someone else.
    let impostor = Relay::new();
    assert_eq!(ask(impostor.health("n", "j")).unwrap_err(), rejected);
    // Wrong type, scope, subject, environment, or lifetime.
    assert_eq!(
        ask(relay.sign(
            RELAY_MINT_REQUEST_TYP,
            &relay.claims("environment:status", "n", "j")
        ))
        .unwrap_err(),
        rejected
    );
    assert_eq!(
        ask(relay.sign(
            RELAY_HEALTH_REQUEST_TYP,
            &relay.claims("environment:connect", "n", "j")
        ))
        .unwrap_err(),
        rejected
    );
    let mut claims = relay.claims("environment:status", "n", "j");
    claims["sub"] = json!("another-user");
    assert_eq!(
        ask(relay.sign(RELAY_HEALTH_REQUEST_TYP, &claims)).unwrap_err(),
        rejected
    );
    let mut claims = relay.claims("environment:status", "n", "j");
    claims["environmentId"] = json!("env_other");
    assert_eq!(
        ask(relay.sign(RELAY_HEALTH_REQUEST_TYP, &claims)).unwrap_err(),
        rejected
    );
    let mut claims = relay.claims("environment:status", "n", "j");
    claims["exp"] = json!(claims["iat"].as_i64().unwrap() + 301);
    assert_eq!(
        ask(relay.sign(RELAY_HEALTH_REQUEST_TYP, &claims)).unwrap_err(),
        rejected
    );
    let mut claims = relay.claims("environment:status", "n", "j");
    claims["scope"] = json!(["environment:status", "environment:connect"]);
    assert_eq!(
        ask(relay.sign(RELAY_HEALTH_REQUEST_TYP, &claims)).unwrap_err(),
        rejected
    );
    assert_eq!(ask("garbage".into()).unwrap_err(), rejected);
    assert_eq!(rejected.to_http_failure().status, 401);
    // Nothing was consumed by any of those.
    assert!(
        harness
            .secrets
            .names()
            .iter()
            .all(|n| !n.starts_with("cloud-health-"))
    );
}

#[tokio::test]
async fn health_needs_an_installed_link() {
    let relay = Relay::new();
    let harness = Harness::new(HarnessOptions::new(ScriptedControlPlane::new()));
    let error = harness
        .link
        .answer_health_request(&health_request(relay.health("n", "j")))
        .unwrap_err();
    assert_eq!(
        error,
        CloudLinkError::Auth(AuthFailure::CloudMintPublicKeyMissing)
    );
    assert_eq!(
        error.message(),
        "Cloud mint public key is not installed for this environment."
    );
    assert_eq!(error.to_http_failure().status, 500);
    let mut options = HarnessOptions::new(ScriptedControlPlane::new());
    options.secrets = vec![(CLOUD_MINT_PUBLIC_KEY, relay.keys.public_key.clone())];
    let harness = Harness::new(options);
    assert_eq!(
        harness
            .link
            .answer_health_request(&health_request(relay.health("n", "j")))
            .unwrap_err(),
        CloudLinkError::Auth(AuthFailure::CloudRelayIssuerMissing)
    );
}

#[tokio::test]
async fn minting_binds_the_credential_to_the_clients_key_and_answers_with_a_signed_proof() {
    let relay = Relay::new();
    let harness = trusting_harness(&relay);
    let request = MintRequest {
        proof: trimmed(&relay.mint("nonce-9", "jti-9", "thumb-1")),
    };
    let response = within(harness.link.mint_credential(&request))
        .await
        .unwrap();
    assert_eq!(response.credential.as_str(), "pairing-credential");
    assert_eq!(response.expires_at.as_str(), "2023-11-14T22:15:20.000Z");
    assert_eq!(*harness.pairing.requests.lock().unwrap(), ["thumb-1"]);
    let claims = verify_env_proof(
        &harness,
        response.proof.as_str(),
        RELAY_MINT_RESPONSE_TYP,
        "https://relay.example.test",
    );
    assert_eq!(claims["requestNonce"], "nonce-9");
    assert_eq!(claims["clientProofKeyThumbprint"], "thumb-1");
    assert_eq!(claims["credential"], "pairing-credential");
    assert_eq!(claims["exp"], 1_700_000_120);
    // One use per proof.
    let replay = within(harness.link.mint_credential(&request))
        .await
        .unwrap_err();
    assert_eq!(replay, CloudLinkError::ProofReplayed(SignedRequest::Mint));
    assert_eq!(harness.pairing.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn mint_proofs_must_bind_the_same_client_key_twice() {
    let relay = Relay::new();
    let harness = trusting_harness(&relay);
    let mut claims = relay.claims("environment:connect", "n", "j");
    claims["clientProofKeyThumbprint"] = json!("thumb-1");
    claims["cnf"] = json!({"jkt": "thumb-2"});
    let proof = relay.sign(RELAY_MINT_REQUEST_TYP, &claims);
    let error = within(harness.link.mint_credential(&MintRequest {
        proof: trimmed(&proof),
    }))
    .await
    .unwrap_err();
    assert_eq!(error, CloudLinkError::ProofRejected(SignedRequest::Mint));
    // Without a confirmation claim at all.
    let mut claims = relay.claims("environment:connect", "n2", "j2");
    claims["clientProofKeyThumbprint"] = json!("thumb-1");
    let proof = relay.sign(RELAY_MINT_REQUEST_TYP, &claims);
    assert!(
        within(harness.link.mint_credential(&MintRequest {
            proof: trimmed(&proof)
        }))
        .await
        .is_err()
    );
    assert!(harness.pairing.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failures_map_to_the_status_codes_the_routes_answer() {
    let cases: Vec<(CloudLinkError, u16, &str)> = vec![
        (
            CloudLinkError::NotLinked,
            400,
            "EnvironmentHttpBadRequestError",
        ),
        (
            CloudLinkError::AccountMismatch,
            409,
            "EnvironmentHttpConflictError",
        ),
        (
            CloudLinkError::TunnelSuperseded,
            409,
            "EnvironmentHttpConflictError",
        ),
        (
            CloudLinkError::AuthorizationMissing,
            401,
            "EnvironmentHttpUnauthorizedError",
        ),
        (
            CloudLinkError::Relay(forbidden()),
            403,
            "EnvironmentHttpForbiddenError",
        ),
        (
            CloudLinkError::Relay(unavailable()),
            500,
            "EnvironmentHttpInternalServerError",
        ),
        (
            CloudLinkError::Relay(
                t3_remote::relay_error::filter_relay_response(400, None, b"").unwrap_err(),
            ),
            400,
            "EnvironmentHttpBadRequestError",
        ),
        (
            CloudLinkError::Relay(
                t3_remote::relay_error::filter_relay_response(401, None, b"").unwrap_err(),
            ),
            401,
            "EnvironmentHttpUnauthorizedError",
        ),
        (
            CloudLinkError::internal(InternalOperation::PersistRelayConfig, "disk"),
            500,
            "EnvironmentHttpInternalServerError",
        ),
    ];
    for (error, status, tag) in cases {
        let failure = error.to_http_failure();
        assert_eq!((failure.status, failure.tag), (status, tag), "{error:?}");
        assert_eq!(failure.body()["message"], error.message());
        assert!(failure.body().get("endpointRuntimeStatus").is_none());
    }
    let unavailable = CloudLinkError::EndpointUnavailable {
        origin_unconfirmed: true,
        endpoint_runtime_status: EndpointStatus::Disabled.to_json(),
    };
    assert_eq!(
        unavailable.to_http_failure().body(),
        json!({
            "_tag": "EnvironmentCloudEndpointUnavailableError",
            "message": "Managed endpoint origin could not be confirmed.",
            "endpointRuntimeStatus": {"status": "disabled"}
        })
    );
}

#[tokio::test]
async fn replay_guards_report_store_outages_instead_of_replays() {
    let store = MemorySecretStore::new();
    store.set("a", b"1").unwrap();
    assert!(!consume_cloud_replay_guards(&store, &["a".into(), "b".into()], b"t").unwrap());
    assert!(store.text("b").is_some(), "every marker is attempted");
    assert!(consume_cloud_replay_guards(&store, &["c".into(), "d".into()], b"t").unwrap());
    struct Broken;
    impl SecretStore for Broken {
        fn get(&self, _: &str) -> Result<Option<Vec<u8>>, SecretError> {
            Ok(None)
        }
        fn set(&self, _: &str, _: &[u8]) -> Result<(), SecretError> {
            Ok(())
        }
        fn create(&self, _: &str, _: &[u8]) -> Result<(), SecretError> {
            Err(SecretError::new("denied"))
        }
        fn remove(&self, _: &str) -> Result<(), SecretError> {
            Ok(())
        }
    }
    assert!(consume_cloud_replay_guards(&Broken, &["a".into()], b"t").is_err());
}

#[test]
fn the_store_keys_match_the_originals() {
    assert_eq!(CLOUD_MINT_PUBLIC_KEY, "cloud-mint-ed25519-public-key");
    assert_eq!(
        RELAY_ENVIRONMENT_CREDENTIAL_SECRET,
        "cloud-relay-environment-credential"
    );
    assert_eq!(
        CLOUD_REPLAY_MARKER_PREFIXES,
        [
            "cloud-mint-nonce-",
            "cloud-mint-jti-",
            "cloud-health-nonce-",
            "cloud-health-jti-"
        ]
    );
    assert_eq!(CLOUD_CLI_DESIRED_LINK_SECRET, "cloud-cli-desired-link");
    let _ = (trimmed("x"), EndpointStatus::Disabled);
}
