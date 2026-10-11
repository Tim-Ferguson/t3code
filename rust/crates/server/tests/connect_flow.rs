//! T3 Connect over the server's real HTTP and WebSocket transport: the `/api/connect/*` routes with the original's scopes and error
//! bodies, linking, the relay-vouched client credential through DPoP, health and mint with replay rejection, preferences with
//! their rollback, startup with a stored link, and what shutdown releases. The relay is a loopback server written per test and the
//! connector a fake; see `connect_support`.
mod connect_support;
mod rpc_support;

use connect_support::*;
use rpc_support::{defect, failure, next, rpc, send, success};
use serde_json::{Value, json};
use t3_remote::{
    jwt::{
        KeyPair, RELAY_HEALTH_REQUEST_TYP, RELAY_HEALTH_RESPONSE_TYP, RELAY_LINK_PROOF_TYP, RELAY_MINT_REQUEST_TYP, RELAY_MINT_RESPONSE_TYP, VerifyOptions, sign_relay_jwt,
        verify_relay_jwt,
    },
    secrets::*,
};

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

async fn body(response: reqwest::Response) -> (u16, Value) {
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn get(server: &Server, path: &str, token: Option<&str>) -> (u16, Value) {
    let mut request = client().get(server.url(path));
    if let Some(token) = token {
        request = request.header("authorization", bearer(token));
    }
    body(request.send().await.unwrap()).await
}

async fn post(server: &Server, path: &str, token: Option<&str>, payload: Value) -> (u16, Value) {
    let mut request = client().post(server.url(path)).json(&payload);
    if let Some(token) = token {
        request = request.header("authorization", bearer(token));
    }
    body(request.send().await.unwrap()).await
}

fn config_body(server: &Server, runtime: Option<Value>) -> Value {
    json!({"relayUrl":server.relay.url,"cloudUserId":"user-123","environmentCredential":"environment-credential","cloudMintPublicKey":server.relay_keys.public_key,"endpointRuntime":runtime})
}

fn no_relay(_: &RecordedRequest) -> (u16, Value) {
    (404, json!({}))
}

#[tokio::test]
async fn the_routes_need_a_session_and_its_scope_and_answer_with_the_originals_bodies() {
    let server = start(Options::new(no_relay)).await;
    // No session, and the wrong scope.
    let (status, error) = get(&server, "/api/connect/link-state", None).await;
    assert_eq!((status, error["_tag"].as_str(), error["code"].as_str()), (401, Some("EnvironmentAuthInvalidError"), Some("auth_invalid")));
    let (status, error) = get(&server, "/api/connect/link-state", Some(&server.outsider)).await;
    assert_eq!((status, error["_tag"].as_str(), error["requiredScope"].as_str()), (403, Some("EnvironmentScopeRequiredError"), Some("relay:read")));
    for path in ["/api/connect/unlink", "/api/connect/preferences", "/api/connect/relay-config", "/api/connect/link-proof"] {
        let (status, error) = post(&server, path, Some(&server.reader), json!({})).await;
        // The body is read before the scope is: an empty one that is not a request is refused first.
        let expected = if path == "/api/connect/unlink" { (403, Some("relay:write")) } else { (400, None) };
        assert_eq!((status, error["requiredScope"].as_str()), expected, "{path}");
    }
    let (status, error) = post(&server, "/api/connect/preferences", Some(&server.reader), json!({"publishAgentActivity":true})).await;
    assert_eq!((status, error["requiredScope"].as_str()), (403, Some("relay:write")));

    // Not linked: nothing to show, and nothing to hold webhooks for.
    let (status, state) = get(&server, "/api/connect/link-state", Some(&server.reader)).await;
    assert_eq!(status, 200);
    assert_eq!(state, json!({"linked":false,"cloudUserId":null,"relayUrl":null,"relayIssuer":null,"managedTunnelActive":false,"publishAgentActivity":false,"holdWebhooksWhileOffline":false}));
    let (status, error) = post(&server, "/api/connect/preferences", Some(&server.writer), json!({"publishAgentActivity":true,"holdWebhooksWhileOffline":true})).await;
    assert_eq!((status, error["_tag"].as_str(), error["message"].as_str()), (400, Some("EnvironmentHttpBadRequestError"), Some("Link this environment to T3 Connect first.")));
    // Publishing alone is a local choice.
    let (status, state) = post(&server, "/api/connect/preferences", Some(&server.writer), json!({"publishAgentActivity":true})).await;
    assert_eq!((status, state["publishAgentActivity"].as_bool()), (200, Some(true)));
    assert_eq!(server.secret(PUBLISH_AGENT_ACTIVITY_SECRET).as_deref(), Some("true"));
    // Unlinking what is not linked succeeds and says the tunnel is off.
    let (status, result) = post(&server, "/api/connect/unlink", Some(&server.writer), Value::Null).await;
    assert_eq!((status, result), (200, json!({"ok":true,"endpointRuntimeStatus":{"status":"disabled"}})));
    // The signed routes take no session; without a link they have nothing to check a proof against.
    let (status, error) = post(&server, "/api/t3-connect/health", None, json!({"proof":"not-a-jwt"})).await;
    assert_eq!((status, error["_tag"].as_str(), error["message"].as_str()), (500, Some("EnvironmentHttpInternalServerError"), Some("Cloud mint public key is not installed for this environment.")));
    let (status, _) = post(&server, "/api/connect/mint-credential", None, json!({"proof":"not-a-jwt"})).await;
    assert_eq!(status, 500);
    assert_eq!(post(&server, "/api/connect/mint-credential", None, json!({})).await.0, 400);
    assert!(server.relay.recorded().is_empty());
    server.finish().await;
}

#[tokio::test]
async fn a_link_is_proved_applied_shown_and_removed_through_the_routes() {
    let relay_url = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let server = start(Options::new(|request| match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/v1/environments/env_123/tunnel/recovery") => (200, json!({"status":"ready"})),
        ("DELETE", _) => (200, json!({"ok":true})),
        _ => (404, json!({})),
    }))
    .await;
    relay_url.lock().unwrap().push_str(&server.relay.url);
    let port = server.address.port();
    let proof_request = json!({
        "challenge":"challenge-1","relayIssuer":"https://relay.example.test/",
        "endpoint":{"httpBaseUrl":format!("http://127.0.0.1:{port}"),"wsBaseUrl":format!("ws://127.0.0.1:{port}"),"providerKind":"cloudflare_tunnel"},
        "origin":{"localHttpHost":"127.0.0.1","localHttpPort":port}
    });
    let response = client().post(server.url("/api/connect/link-proof")).header("authorization", bearer(&server.writer)).json(&proof_request).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["pragma"], "no-cache");
    let proof: String = response.json().await.unwrap();
    let keys = get_or_create(&server);
    let claims = verify_relay_jwt(&VerifyOptions {
        public_key: &keys.public_key,
        token: &proof,
        typ: RELAY_LINK_PROOF_TYP,
        issuer: "t3-env:env_123",
        audience: "https://relay.example.test",
        now_epoch_seconds: NOW_MS / 1000,
        max_token_age_secs: None,
    })
    .unwrap();
    assert_eq!(claims["challenge"], "challenge-1");
    assert_eq!(claims["origin"], json!({"localHttpHost":"127.0.0.1","localHttpPort":port}));
    // A proof is for a request that reached this server itself: one that came through a proxy is refused.
    let proxied = client().post(server.url("/api/connect/link-proof")).header("authorization", bearer(&server.writer)).header("x-forwarded-host", "relay.example.test").json(&proof_request).send().await.unwrap();
    let (status, error) = body(proxied).await;
    assert_eq!((status, error["_tag"].as_str(), error["message"].as_str()), (400, Some("EnvironmentHttpBadRequestError"), Some("Invalid managed endpoint origin.")));

    // The relay's answer to the proof is applied: the link is stored, and the tunnel it names is registered and started.
    let (status, applied) = post(&server, "/api/connect/relay-config", Some(&server.writer), config_body(&server, Some(json!({"providerKind":"cloudflare_tunnel","connectorToken":"connector-1","tunnelId":"tunnel-1"})))).await;
    assert_eq!(status, 200, "{applied}");
    assert_eq!(applied["ok"], true);
    assert_eq!(applied["endpointRuntimeStatus"]["status"], "running");
    assert_eq!(server.spawner.last().token, "connector-1");
    let registered = server.relay.recorded_for("POST", "/v1/environments/env_123/tunnel/recovery");
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0].authorization.as_deref(), Some("Bearer environment-credential"));
    assert_eq!(registered[0].body["origin"], json!({"localHttpHost":"127.0.0.1","localHttpPort":port}));
    for (name, expected) in [(CLOUD_LINKED_USER_ID, "user-123"), (RELAY_ENVIRONMENT_CREDENTIAL_SECRET, "environment-credential")] {
        assert_eq!(server.secret(name).as_deref(), Some(expected), "{name}");
    }

    let (status, state) = get(&server, "/api/connect/link-state", Some(&server.reader)).await;
    assert_eq!(status, 200);
    assert_eq!((state["linked"].as_bool(), state["cloudUserId"].as_str(), state["managedTunnelActive"].as_bool()), (Some(true), Some("user-123"), Some(true)));
    assert_eq!(state["relayUrl"], json!(server.relay.url));

    // Another account cannot take the link over; the same one can unlink it.
    let mut other = config_body(&server, None);
    other["cloudUserId"] = "user-999".into();
    let (status, error) = post(&server, "/api/connect/relay-config", Some(&server.writer), other).await;
    assert_eq!((status, error["_tag"].as_str()), (409, Some("EnvironmentHttpConflictError")));
    // A malformed one is the caller's mistake.
    let mut bad = config_body(&server, None);
    bad["cloudMintPublicKey"] = "nope".into();
    let (status, error) = post(&server, "/api/connect/relay-config", Some(&server.writer), bad).await;
    assert_eq!((status, error["message"].as_str()), (400, Some("Cloud mint public key must be a valid Ed25519 public key.")));

    let (status, result) = post(&server, "/api/connect/unlink", Some(&server.writer), Value::Null).await;
    assert_eq!((status, result["ok"].as_bool()), (200, Some(true)));
    assert!(!server.spawner.last().is_running());
    let (_, state) = get(&server, "/api/connect/link-state", Some(&server.reader)).await;
    assert_eq!(state["linked"], false);
    assert!(server.secret(CLOUD_LINKED_USER_ID).is_none() && server.secret(CLOUD_ENDPOINT_RUNTIME_CONFIG).is_none());
    server.finish().await;
}

fn get_or_create(server: &Server) -> KeyPair {
    t3_remote::env_keys::get_or_create_environment_key_pair(server.connect.secrets.as_ref()).unwrap()
}

#[tokio::test]
async fn a_preference_the_relay_refuses_is_not_kept() {
    let server = start(Options::new(|request| if request.path.ends_with("/link-preferences") { (500, json!({})) } else { (404, json!({})) })).await;
    for (name, value) in connect_support::linked("http://127.0.0.1:1", &KeyPair::generate()) {
        server.connect.secrets.set(&name, value.as_bytes()).unwrap();
    }
    // The relay decides whether webhooks are held, so a refusal there stops the whole update, and the publishing choice made
    // first is put back.
    let (status, error) = post(&server, "/api/connect/preferences", Some(&server.writer), json!({"publishAgentActivity":true,"holdWebhooksWhileOffline":true})).await;
    assert_eq!((status, error["_tag"].as_str()), (500, Some("EnvironmentHttpInternalServerError")), "{error}");
    assert!(server.secret(PUBLISH_AGENT_ACTIVITY_SECRET).is_none(), "the publishing choice was rolled back");
    assert!(server.secret(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET).is_none());
    server.finish().await;
}

fn relay_claims(server: &Server, scope: &str, nonce: &str, jti: &str) -> Value {
    let now = NOW_MS / 1000;
    json!({"iss":server.relay.url,"aud":"t3-env:env_123","sub":"user-123","jti":jti,"iat":now,"exp":now + 120,"environmentId":"env_123","nonce":nonce,"scope":[scope]})
}

fn mint_proof(server: &Server, nonce: &str, jti: &str, thumbprint: &str) -> String {
    let mut claims = relay_claims(server, "environment:connect", nonce, jti);
    claims["clientProofKeyThumbprint"] = json!(thumbprint);
    claims["cnf"] = json!({"jkt":thumbprint});
    sign_relay_jwt(&server.relay_keys.private_key, RELAY_MINT_REQUEST_TYP, &claims).unwrap()
}

fn linked_server_options() -> Options {
    Options::new(no_relay)
}

async fn linked_server() -> Server {
    let server = start(Options { start: false, ..linked_server_options() }).await;
    for (name, value) in connect_support::linked(&server.relay.url, &server.relay_keys) {
        server.connect.secrets.set(&name, value.as_bytes()).unwrap();
    }
    server
}

#[tokio::test]
async fn health_is_answered_once_per_proof_with_a_signed_status_and_the_current_capabilities() {
    let server = linked_server().await;
    let proof = sign_relay_jwt(&server.relay_keys.private_key, RELAY_HEALTH_REQUEST_TYP, &relay_claims(&server, "environment:status", "n-1", "j-1")).unwrap();
    let response = client().post(server.url("/api/t3-connect/health")).json(&json!({"proof":proof})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let answer: Value = response.json().await.unwrap();
    assert_eq!((answer["status"].as_str(), answer["environmentId"].as_str()), (Some("online"), Some("env_123")));
    // The descriptor it carries is read now: publishing is not on yet.
    assert_eq!(answer["descriptor"]["capabilities"]["agentActivityPublishing"], false);
    let keys = get_or_create(&server);
    let claims = verify_relay_jwt(&VerifyOptions { public_key: &keys.public_key, token: answer["proof"].as_str().unwrap(), typ: RELAY_HEALTH_RESPONSE_TYP, issuer: "t3-env:env_123", audience: &server.relay.url, now_epoch_seconds: NOW_MS / 1000, max_token_age_secs: None }).unwrap();
    assert_eq!(claims["requestNonce"], "n-1");
    // The same proof, and another with the same nonce, are replays.
    let (status, error) = post(&server, "/api/t3-connect/health", None, json!({"proof":proof})).await;
    assert_eq!((status, error["_tag"].as_str(), error["message"].as_str()), (409, Some("EnvironmentHttpConflictError"), Some("Cloud health request was already consumed.")));
    // A proof signed by anyone else is refused, and so is the wrong kind of proof.
    let forged = sign_relay_jwt(&KeyPair::generate().private_key, RELAY_HEALTH_REQUEST_TYP, &relay_claims(&server, "environment:status", "n-2", "j-2")).unwrap();
    let (status, error) = post(&server, "/api/t3-connect/health", None, json!({"proof":forged})).await;
    assert_eq!((status, error["_tag"].as_str()), (401, Some("EnvironmentHttpUnauthorizedError")));
    let (status, _) = post(&server, "/api/t3-connect/health", None, json!({"proof":mint_proof(&server, "n-3", "j-3", "t")})).await;
    assert_eq!(status, 401);
    // Opting in to publishing, with the link in place, shows in the descriptor everywhere it is served.
    server.connect.secrets.set(PUBLISH_AGENT_ACTIVITY_SECRET, b"true").unwrap();
    let (_, descriptor) = get(&server, "/.well-known/t3/environment", None).await;
    assert_eq!(descriptor["capabilities"]["agentActivityPublishing"], true);
    server.finish().await;
}

#[tokio::test]
async fn a_credential_the_relay_vouches_for_becomes_a_session_bound_to_the_clients_key() {
    let server = linked_server().await;
    let key = DpopClient::new();
    let token_url = server.url("/oauth/token");

    // The relay asks this environment to mint a credential for the client's key; once per proof.
    let proof = mint_proof(&server, "nonce-1", "jti-1", &key.thumbprint());
    let response = client().post(server.url("/api/t3-connect/mint-credential")).json(&json!({"proof":proof})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let minted: Value = response.json().await.unwrap();
    let credential = minted["credential"].as_str().unwrap().to_owned();
    let keys = get_or_create(&server);
    let claims = verify_relay_jwt(&VerifyOptions { public_key: &keys.public_key, token: minted["proof"].as_str().unwrap(), typ: RELAY_MINT_RESPONSE_TYP, issuer: "t3-env:env_123", audience: &server.relay.url, now_epoch_seconds: NOW_MS / 1000, max_token_age_secs: None }).unwrap();
    assert_eq!((claims["requestNonce"].as_str(), claims["clientProofKeyThumbprint"].as_str(), claims["credential"].as_str()), (Some("nonce-1"), Some(key.thumbprint().as_str()), Some(credential.as_str())));
    let (status, error) = post(&server, "/api/connect/mint-credential", None, json!({"proof":proof})).await;
    assert_eq!((status, error["_tag"].as_str(), error["message"].as_str()), (409, Some("EnvironmentHttpConflictError"), Some("Cloud mint request was already consumed.")));

    let form = |token: &str| {
        [
            ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"),
            ("subject_token", token),
            ("subject_token_type", "urn:t3:params:oauth:token-type:environment-bootstrap"),
            ("requested_token_type", "urn:ietf:params:oauth:token-type:access_token"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
    };
    let exchange = |dpop: Option<String>| {
        let mut request = client().post(&token_url).form(&form(&credential));
        if let Some(dpop) = dpop {
            request = request.header("dpop", dpop);
        }
        async move {
            let response = request.send().await.unwrap();
            let challenge = response.headers().get("www-authenticate").map(|value| value.to_str().unwrap().to_owned());
            let (status, body) = body(response).await;
            (status, body, challenge)
        }
    };
    // The credential belongs to one key: another key, and no key, are refused and leave it unused.
    let other = DpopClient::new();
    let (status, error, challenge) = exchange(Some(other.proof("POST", &token_url, None, "x-1"))).await;
    assert_eq!((status, error["_tag"].as_str(), challenge.as_deref()), (401, Some("EnvironmentAuthInvalidError"), Some("DPoP")));
    assert_eq!(exchange(None).await.0, 401);
    // A proof for another URL or method says so.
    let (status, error, _) = exchange(Some(key.proof("POST", &server.url("/elsewhere"), None, "x-2"))).await;
    assert_eq!((status, error["dpopFailureReason"].as_str()), (401, Some("request_mismatch")));
    let (status, error, _) = exchange(Some(key.proof_at("POST", &token_url, None, "x-3", chrono::Utc::now().timestamp() - 400))).await;
    assert_eq!((status, error["dpopFailureReason"].as_str()), (401, Some("time_window")));
    let (status, error, _) = exchange(Some("garbage".into())).await;
    assert_eq!((status, error["dpopFailureReason"].as_str()), (401, Some("invalid_proof")));

    let (status, token, _) = exchange(Some(key.proof("POST", &token_url, None, "exchange-1"))).await;
    assert_eq!(status, 200, "{token}");
    assert_eq!(token["token_type"], "DPoP");
    assert!(token["expires_in"].as_i64().unwrap() <= 3600 && token["expires_in"].as_i64().unwrap() > 3500);
    let access = token["access_token"].as_str().unwrap().to_owned();
    assert!(token["scope"].as_str().unwrap().contains("relay:read"));
    // The credential was one-use, and so was the proof.
    assert_eq!(exchange(Some(key.proof("POST", &token_url, None, "exchange-2"))).await.0, 401);

    // Requests prove the key again, each proof once, and hash the token.
    let state_url = server.url("/api/connect/link-state");
    let call = |dpop: Option<String>, authorization: String| {
        let mut request = client().get(&state_url).header("authorization", authorization);
        if let Some(dpop) = dpop {
            request = request.header("dpop", dpop);
        }
        async move {
            let response = request.send().await.unwrap();
            let challenge = response.headers().get("www-authenticate").map(|value| value.to_str().unwrap().to_owned());
            let (status, body) = body(response).await;
            (status, body, challenge)
        }
    };
    let dpop = |value: &str| format!("DPoP {value}");
    let first = key.proof("GET", &state_url, Some(&access), "request-1");
    let (status, state, _) = call(Some(first.clone()), dpop(&access)).await;
    assert_eq!((status, state["linked"].as_bool()), (200, Some(true)));
    let (status, error, challenge) = call(Some(first), dpop(&access)).await;
    assert_eq!((status, error["dpopFailureReason"].as_str(), challenge.as_deref()), (401, Some("replay"), Some("DPoP")));
    let (status, error, _) = call(Some(other.proof("GET", &state_url, Some(&access), "request-2")), dpop(&access)).await;
    assert_eq!((status, error["dpopFailureReason"].as_str()), (401, Some("key_mismatch")));
    let (status, error, _) = call(Some(key.proof("GET", &state_url, Some("another-token"), "request-3")), dpop(&access)).await;
    assert_eq!((status, error["dpopFailureReason"].as_str()), (401, Some("token_mismatch")));
    let (status, error, _) = call(Some(key.proof("POST", &state_url, Some(&access), "request-4")), dpop(&access)).await;
    assert_eq!((status, error["dpopFailureReason"].as_str()), (401, Some("request_mismatch")));
    let (status, error, _) = call(None, dpop(&access)).await;
    assert_eq!((status, error["dpopFailureReason"].as_str()), (401, Some("invalid_proof")));
    // A bound token is not a bearer token, and nothing else takes the DPoP scheme.
    let (status, error, challenge) = call(None, bearer(&access)).await;
    assert_eq!((status, error["dpopFailureReason"].as_str(), challenge), (401, Some("invalid_proof"), None));
    let (status, error, _) = call(Some(key.proof("GET", &state_url, Some(&server.writer), "request-5")), dpop(&server.writer)).await;
    assert_eq!((status, error["dpopFailureReason"].as_str()), (401, Some("invalid_proof")));
    // A header a client sends under the name the server uses inside is not believed.
    let forged = client().get(&state_url).header("authorization", dpop(&access)).header("x-t3-internal-dpop-verified", &access).send().await.unwrap();
    assert_eq!(forged.status(), 401);
    // Where the route only wants to know whether there is a session, a refused proof is no session.
    let session = client().get(server.url("/api/auth/session")).header("authorization", dpop(&access)).header("dpop", "garbage").send().await.unwrap();
    let (status, state) = body(session).await;
    assert_eq!((status, state["authenticated"].as_bool()), (200, Some(false)));
    let session = client().get(server.url("/api/auth/session")).header("authorization", dpop(&access)).header("dpop", key.proof("GET", &server.url("/api/auth/session"), Some(&access), "session-1")).send().await.unwrap();
    let (_, state) = body(session).await;
    assert_eq!((state["authenticated"].as_bool(), state["sessionMethod"].as_str()), (Some(true), Some("dpop-access-token")));
    assert!(state["auth"]["sessionMethods"].as_array().unwrap().contains(&json!("dpop-access-token")));

    // The same key opens the WebSocket, and the session may use the methods its scopes allow.
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let ws_url = format!("ws://{}/ws?orchestrationProtocol=2", server.address);
    let mut upgrade = ws_url.clone().into_client_request().unwrap();
    upgrade.headers_mut().insert("authorization", dpop(&access).parse().unwrap());
    upgrade.headers_mut().insert("dpop", key.proof("GET", &format!("http://{}/ws", server.address), Some(&access), "socket-1").parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(upgrade).await.unwrap();
    let status = rpc(&mut socket, 1, "cloud.getRelayClientStatus", json!({})).await;
    assert_eq!(success(&status)["status"], "available");
    let write = rpc(&mut socket, 2, "cloud.installRelayClient", json!({})).await;
    // Streams answer with chunks; the minted session may not install (it holds `relay:read` only).
    assert_eq!(failure(&write)["_tag"], "EnvironmentAuthorizationError");
    server.finish().await;
}

#[tokio::test]
async fn the_relay_client_rpcs_report_and_install_with_progress() {
    let server = start(Options::new(no_relay)).await;
    let mut socket = {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", server.address).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", bearer(&server.writer).parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    };
    let status = rpc(&mut socket, 1, "cloud.getRelayClientStatus", json!({})).await;
    assert_eq!(success(&status), &json!({"status":"available","executablePath":"/fake/cloudflared","source":"override","version":"test"}));
    // A payload the original's schema refuses is refused before the handler, as a defect, for both methods.
    for (id, tag) in [(20, "cloud.getRelayClientStatus"), (21, "cloud.installRelayClient")] {
        let refused = rpc(&mut socket, id, tag, Value::Null).await;
        assert!(defect(&refused).as_str().unwrap().contains("Expected"), "{tag}: {refused}");
    }
    // A payload with keys is an object: unknown keys are dropped, not refused.
    assert_eq!(success(&rpc(&mut socket, 22, "cloud.getRelayClientStatus", json!({"extra":true})).await)["status"], "available");
    server.resolver.set(t3_remote::relay_client::RelayClientStatus::Missing { version: "2026.5.2".into() });
    assert_eq!(success(&rpc(&mut socket, 2, "cloud.getRelayClientStatus", json!({})).await), &json!({"status":"missing","version":"2026.5.2"}));
    // The installation is a stream: each event is acknowledged, and the stream ends with the last.
    send(&mut socket, json!({"_tag":"Request","id":3,"tag":"cloud.installRelayClient","payload":{},"headers":[]})).await;
    let mut events = Vec::new();
    loop {
        let frame = within(next(&mut socket)).await;
        match frame["_tag"].as_str() {
            Some("Chunk") => {
                events.extend(frame["values"].as_array().unwrap().iter().cloned());
                send(&mut socket, json!({"_tag":"Ack","requestId":3})).await;
            }
            Some("Exit") => {
                assert_eq!(frame["exit"]["_tag"], "Success", "{frame}");
                break;
            }
            _ => {}
        }
    }
    assert_eq!(events[0], json!({"type":"progress","stage":"checking"}));
    assert_eq!(events.last().unwrap()["type"], "complete");
    server.finish().await;
}

#[tokio::test]
async fn without_public_configuration_the_server_asks_nobody_for_anything() {
    // No relay URL, no Clerk keys: a fresh clone. The routes answer, the link is empty, and no request leaves the process: the
    // relay below would record one, and the CLI's sign-in has no endpoint to call.
    let server = start(Options { public_config: false, ..Options::new(no_relay) }).await;
    assert!(server.spawner.count() == 0);
    let (status, state) = get(&server, "/api/connect/link-state", Some(&server.reader)).await;
    assert_eq!((status, state["linked"].as_bool()), (200, Some(false)));
    let status = server.connect.services.status().borrow().clone();
    assert_eq!(format!("{:?}", status.phase), "Unlinked");
    let server = server.finish().await;
    assert!(server.relay.recorded().is_empty());
    assert_eq!(server.spawner.count(), 0);
}
