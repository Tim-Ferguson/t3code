//! The outbound HTTP client against a relay on loopback: routes, credentials, bodies, and how
//! each kind of failure is classified.
mod support;

use serde_json::json;
use support::*;
use t3_contracts::TrimmedNonEmptyString;
use t3_remote::control_plane::{ControlPlane, HttpControlPlane};
use t3_remote::relay_error::Rejection;
use t3_remote::types::*;

fn trimmed(value: &str) -> TrimmedNonEmptyString {
    TrimmedNonEmptyString::new(value).unwrap()
}

#[tokio::test]
async fn client_routes_use_the_cli_token_and_json() {
    let relay = FakeRelay::start(|request| match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/v1/client/environment-link-challenges") => (200, json!({"challenge": "c-1", "expiresAt": "2026-01-01T00:00:00.000Z"})),
        ("POST", "/v1/client/environment-links") => (
            200,
            json!({
                "ok": true, "cloudUserId": "user-1", "environmentId": "env_123",
                "endpoint": {"httpBaseUrl": "https://e.example.test", "wsBaseUrl": "wss://e.example.test/ws", "providerKind": "cloudflare_tunnel"},
                "endpointRuntime": {"providerKind": "cloudflare_tunnel", "connectorToken": "tok", "tunnelId": "t-1"},
                "relayIssuer": "https://relay.example.test", "environmentCredential": "env-cred", "cloudMintPublicKey": "pem"
            }),
        ),
        ("DELETE", "/v1/client/environment-links/env%20id") => (200, json!({"ok": true})),
        ("DELETE", "/v1/client/environment-links/env_123/tunnel") => (200, json!({"ok": false})),
        _ => (404, json!({})),
    })
    .await;
    let plane = HttpControlPlane::new();
    let challenge = within(plane.create_link_challenge(
        &relay.url,
        "cli-access",
        &LinkChallengeRequest {
            notifications_enabled: true,
            live_activities_enabled: true,
            managed_tunnels_enabled: false,
        },
    ))
    .await
    .unwrap();
    assert_eq!(challenge.challenge.as_str(), "c-1");
    let linked = within(plane.link_environment(
        &relay.url,
        "cli-access",
        &LinkRequest {
            device_id: None,
            proof: trimmed("proof"),
            notifications_enabled: true,
            live_activities_enabled: true,
            managed_tunnels_enabled: true,
        },
    ))
    .await
    .unwrap();
    assert_eq!(linked.cloud_user_id.as_str(), "user-1");
    assert_eq!(
        linked.endpoint_runtime.unwrap().tunnel_id.unwrap().as_str(),
        "t-1"
    );
    assert!(
        within(plane.unlink_environment(&relay.url, "cli-access", "env id"))
            .await
            .unwrap()
            .ok
    );
    assert!(
        !within(plane.release_environment_tunnel(&relay.url, "cli-access", "env_123"))
            .await
            .unwrap()
            .ok
    );

    let recorded = relay.recorded();
    assert_eq!(recorded.len(), 4);
    assert!(
        recorded
            .iter()
            .all(|r| r.authorization.as_deref() == Some("Bearer cli-access"))
    );
    assert_eq!(
        recorded[0].body,
        json!({"notificationsEnabled": true, "liveActivitiesEnabled": true, "managedTunnelsEnabled": false})
    );
    assert_eq!(
        recorded[0].content_type.as_deref(),
        Some("application/json")
    );
    assert_eq!(
        recorded[1].body,
        json!({"proof": "proof", "notificationsEnabled": true, "liveActivitiesEnabled": true, "managedTunnelsEnabled": true})
    );
}

#[tokio::test]
async fn environment_routes_use_the_environment_credential() {
    let relay = FakeRelay::start(|request| match request.path.as_str() {
        "/v1/environments/env_123/tunnel/recovery" => (200, json!({"status": "recovery_required"})),
        "/v1/environments/env_123/tunnel" => (
            200,
            json!({
                "endpoint": {"httpBaseUrl": "https://e.example.test/", "wsBaseUrl": "wss://e.example.test/ws", "providerKind": "cloudflare_tunnel"},
                "endpointRuntime": {"providerKind": "cloudflare_tunnel", "connectorToken": "new-token", "tunnelId": "new-tunnel"}
            }),
        ),
        "/v1/environments/env_123/link-preferences" => (200, json!({"holdWebhooksWhileOffline": true})),
        "/v1/environments/env_123/hooks/wake" => (200, json!({"pending": true})),
        "/v1/environments/env_123/threads/thread%2F1/agent-activity" => (200, json!({"ok": true, "deliveries": []})),
        _ => (404, json!({})),
    })
    .await;
    let plane = HttpControlPlane::new();
    let origin = ManagedEndpointOrigin::new("127.0.0.1", 3773);
    let registered = within(plane.register_tunnel_recovery(
        &relay.url,
        "env-cred",
        "env_123",
        &RecoveryRegistrationRequest {
            cloud_user_id: trimmed("user-1"),
            tunnel_id: trimmed("t-1"),
            origin: origin.clone(),
            proof: trimmed("p"),
        },
    ))
    .await
    .unwrap();
    assert_eq!(registered.status, RegistrationStatus::RecoveryRequired);
    let recovered = within(plane.recover_tunnel(
        &relay.url,
        "env-cred",
        "env_123",
        &RecoveryRequest {
            cloud_user_id: trimmed("user-1"),
            origin,
            proof: trimmed("p"),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        recovered.endpoint_runtime.connector_token.as_str(),
        "new-token"
    );
    let prefs = within(plane.update_link_preferences(
        &relay.url,
        "env-cred",
        "env_123",
        &LinkPreferencesRequest {
            hold_webhooks_while_offline: true,
        },
    ))
    .await
    .unwrap();
    assert!(prefs.hold_webhooks_while_offline);
    assert!(
        within(plane.wake_held_hooks(&relay.url, "env-cred", "env_123"))
            .await
            .unwrap()
            .pending
    );
    let published = within(plane.publish_agent_activity(
        &relay.url,
        "env-cred",
        "env_123",
        "thread/1",
        &AgentActivityPublishRequest {
            state: None,
            proof: trimmed("proof"),
        },
    ))
    .await
    .unwrap();
    assert!(published.ok);
    let recorded = relay.recorded();
    assert!(
        recorded
            .iter()
            .all(|r| r.authorization.as_deref() == Some("Bearer env-cred") && r.method == "POST")
    );
    assert_eq!(
        recorded[0].body,
        json!({"cloudUserId": "user-1", "tunnelId": "t-1", "origin": {"localHttpHost": "127.0.0.1", "localHttpPort": 3773}, "proof": "p"})
    );
    assert_eq!(recorded[4].body, json!({"state": null, "proof": "proof"}));
}

#[tokio::test]
async fn refusals_are_classified_with_the_relays_explanation() {
    let relay = FakeRelay::start_with_headers(|request| match request.path.as_str() {
        "/v1/client/environment-link-challenges" => (
            401,
            json!({"_tag": "RelayAuthInvalidError", "code": "auth_invalid", "reason": "invalid_bearer", "traceId": "trace-9"}),
            vec![],
        ),
        "/v1/client/environment-links" => (
            403,
            json!({"_tag": "RelayEnvironmentLinkLimitExceededError", "code": "environment_link_limit_exceeded", "maxTunnels": 3, "traceId": "trace-limit"}),
            vec![],
        ),
        "/v1/client/environment-links/env_123" => (502, json!({}), vec![("cf-ray".into(), "abc-IAD".into())]),
        "/v1/client/environment-links/env_123/tunnel" => (200, json!({"unexpected": "shape"}), vec![]),
        _ => (400, json!("bad"), vec![]),
    })
    .await;
    let plane = HttpControlPlane::new();
    let request = LinkChallengeRequest {
        notifications_enabled: true,
        live_activities_enabled: true,
        managed_tunnels_enabled: true,
    };
    let error = within(plane.create_link_challenge(&relay.url, "t", &request))
        .await
        .unwrap_err();
    assert_eq!(error.rejection, Rejection::Unauthorized);
    assert!(
        error.description.contains("invalid_bearer")
            && error.description.contains("Trace ID: trace-9")
    );
    assert!(!error.should_retry());

    let error = within(plane.link_environment(
        &relay.url,
        "t",
        &LinkRequest {
            device_id: None,
            proof: trimmed("p"),
            notifications_enabled: true,
            live_activities_enabled: true,
            managed_tunnels_enabled: true,
        },
    ))
    .await
    .unwrap_err();
    assert_eq!(error.rejection, Rejection::Forbidden);
    assert!(error.description.contains("at most 3 tunnels"));

    let error = within(plane.unlink_environment(&relay.url, "t", "env_123"))
        .await
        .unwrap_err();
    assert_eq!(error.rejection, Rejection::Unavailable);
    assert!(error.description.contains("HTTP 502") && error.description.contains("abc-IAD"));
    assert!(error.should_retry());

    // A 200 that does not decode is an unexpected response, and may be retried.
    let error = within(plane.release_environment_tunnel(&relay.url, "t", "env_123"))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        t3_remote::relay_error::RelayRequestError::unexpected_response()
    );
}

#[tokio::test]
async fn an_unreachable_relay_is_a_retryable_transport_failure() {
    // Bind and drop a listener to get a loopback port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let plane = HttpControlPlane::new();
    let error = within(plane.wake_held_hooks(&format!("http://127.0.0.1:{port}"), "c", "env_123"))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        t3_remote::relay_error::RelayRequestError::transport("TransportError")
    );
    assert!(error.should_retry());
}
