//! The MCP OAuth server (`apps/server/src/auth/McpOAuth.test.ts`, `McpHttpServer.test.ts`'s challenge) over a real loopback
//! socket: discovery derived from the origin the client reached, registration, the authorize hand-off, approval by pairing code
//! and by browser session, the token endpoint, and `/mcp` accepting the token while the rest of the API refuses it. The signed
//! client id, the CSRF token, the S256 challenge and `redirectMatches` are checked against the unchanged original's output
//! (`tools/generate_mcp_oauth_fixtures.mjs`). Requests come as they arrive behind an https proxy: plain http with the public Host.
//! No network beyond loopback; everything lives in a temp directory.
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use t3_contracts::{AuthEnvironmentScope, AuthMcpClientAccess, AuthMcpClientRegistration};
use t3_server::{
    auth::AuthService,
    mcp_http::McpHttpService,
    mcp_oauth::{self, McpOAuth},
    mcp_sessions::{DEFAULT_LIVENESS_WINDOW_MS, McpSessionRegistry},
    persistence::Store,
    transport::{ApiState, router},
};

const ORIGIN: &str = "https://box.example.ts.net";
const HOST: &str = "box.example.ts.net";
const REDIRECT: &str = "http://localhost/callback";
const VERIFIER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-verifier-for-tests";
/// base64url(SHA-256(verifier)), the S256 challenge for `VERIFIER` (the original test's own constant).
const CHALLENGE: &str = "DeB41nTVkPwpbbYecrnqtVq7VXLezustdHAK4SWt13c";

struct Flow {
    _directory: tempfile::TempDir,
    address: SocketAddr,
    auth: AuthService,
    store: Store,
    http: reqwest::Client,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

async fn start() -> Flow {
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-mcp-oauth-").tempdir().unwrap();
    let store = Store::open(directory.path().join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_oauth".into(), "loopback-browser".into()).unwrap();
    let secrets = t3_server::server_secret_store::ServerSecretStore::open(directory.path().join("secrets")).unwrap();
    let oauth = Arc::new(McpOAuth::new(auth.clone(), &secrets).unwrap());
    mcp_oauth::install(&store, oauth.clone());
    let environment = "env-oauth";
    let registry = McpSessionRegistry::new(environment.parse().unwrap(), None, Arc::new(|| Utc::now().timestamp_millis()), DEFAULT_LIVENESS_WINDOW_MS);
    let mcp = McpHttpService::new(registry, None).with_clients(oauth, environment.parse().unwrap());
    let state = ApiState {
        store: store.clone(),
        auth: auth.clone(),
        environment: json!({"environmentId":environment,"label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None,
        settings: None,
        cors_origins: None,
        assets: None,
        providers: None,
        execution: None,
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let app = router(state).merge(mcp.router());
    let server = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    Flow { _directory: directory, address, auth, store, http, stop: Some(stop), server: Some(server) }
}

impl Flow {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, format!("http://{}{path}", self.address)).header("host", HOST).header("x-forwarded-proto", "https")
    }
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::GET, path)
    }
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, path)
    }
    async fn register(&self, redirect: &str) -> reqwest::Response {
        self.post("/oauth/mcp/register")
            .json(&json!({"client_name":"Claude Code","redirect_uris":[redirect],"grant_types":["authorization_code","refresh_token"],"token_endpoint_auth_method":"none"}))
            .send()
            .await
            .unwrap()
    }
    async fn registered_client_id(&self) -> String {
        self.register(REDIRECT).await.json::<Value>().await.unwrap()["client_id"].as_str().unwrap().to_owned()
    }
    /// What the approval page posts: the agent's request plus the user's choice.
    async fn decide(&self, authorization: &Value, decision: Value, cookie: Option<&str>) -> (u16, Value) {
        let mut request = self.post("/oauth/mcp/decision").json(&json!({"authorization":authorization,"decision":decision}));
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.json().await.unwrap())
    }
    async fn details(&self, authorization: &Value, cookie: Option<&str>) -> Value {
        let mut request = self.post("/oauth/mcp/approval").json(authorization);
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        request.send().await.unwrap().json().await.unwrap()
    }
    async fn exchange(&self, fields: &[(&str, &str)]) -> reqwest::Response {
        self.post("/oauth/mcp/token").form(fields).send().await.unwrap()
    }
    fn pairing(&self, scopes: &[AuthEnvironmentScope]) -> String {
        self.auth.create_pairing_credential(scopes, Utc::now(), Duration::minutes(5)).unwrap()
    }
    fn operator_pairing(&self) -> String {
        self.pairing(&[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate])
    }
    /// Signs a browser in through the real route and returns its session cookie.
    async fn browser_cookie(&self, scopes: &[AuthEnvironmentScope]) -> String {
        let response = self.post("/api/auth/browser-session").json(&json!({"credential":self.pairing(scopes)})).send().await.unwrap();
        response.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_owned()
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
    }
}

fn authorize_params(client_id: &str, redirect: &str) -> Value {
    json!({"response_type":"code","client_id":client_id,"redirect_uri":redirect,"code_challenge":CHALLENGE,"code_challenge_method":"S256","state":"state-1","resource":format!("{ORIGIN}/mcp")})
}
fn query_of(params: &Value) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(params.as_object().unwrap().iter().map(|(name, value)| (name.as_str(), value.as_str().unwrap()))).finish()
}
fn callback(redirect_to: &Value) -> url::Url {
    url::Url::parse(redirect_to.as_str().unwrap()).unwrap()
}
fn param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_metadata_is_derived_from_the_origin_the_client_reached() {
    let flow = start().await;
    let resource: Value = flow.get("/.well-known/oauth-protected-resource/mcp").send().await.unwrap().json().await.unwrap();
    assert_eq!(resource["resource"], format!("{ORIGIN}/mcp"));
    assert_eq!(resource["authorization_servers"], json!([ORIGIN]));
    assert_eq!(resource["scopes_supported"], json!(["orchestration:read", "orchestration:operate"]));
    assert_eq!(resource["bearer_methods_supported"], json!(["header"]));
    assert_eq!(resource["resource_name"], "T3 Code");
    let same: Value = flow.get("/.well-known/oauth-protected-resource").send().await.unwrap().json().await.unwrap();
    assert_eq!(same, resource);
    let server: Value = flow.get("/.well-known/oauth-authorization-server").send().await.unwrap().json().await.unwrap();
    assert_eq!(
        server,
        json!({"issuer":ORIGIN,"authorization_endpoint":format!("{ORIGIN}/oauth/mcp/authorize"),"token_endpoint":format!("{ORIGIN}/oauth/mcp/token"),"registration_endpoint":format!("{ORIGIN}/oauth/mcp/register"),
            "response_types_supported":["code"],"grant_types_supported":["authorization_code"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"],
            "scopes_supported":["orchestration:read","orchestration:operate"],"authorization_response_iss_parameter_supported":true})
    );
    // Without a proxy header the origin is plain http on the Host that was asked for; an unusable Host falls back to localhost.
    let plain: Value = flow.http.get(format!("http://{}/.well-known/oauth-authorization-server", flow.address)).header("host", "127.0.0.1:9").send().await.unwrap().json().await.unwrap();
    assert_eq!(plain["issuer"], "http://127.0.0.1:9");
    let broken: Value = flow.http.get(format!("http://{}/.well-known/oauth-authorization-server", flow.address)).header("host", "bad host").send().await.unwrap().json().await.unwrap();
    assert_eq!(broken["issuer"], "http://localhost");
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn registration_refuses_unsafe_redirects_and_the_authorize_page_never_redirects_for_an_unverified_client() {
    let flow = start().await;
    for refused in ["http://bot.example/callback", "https://user:pass@bot.example/callback", "https://bot.example/callback#", "http://localhost/callback#", "javascript:alert(1)"] {
        let response = flow.register(refused).await;
        assert_eq!(response.status(), 400, "{refused}");
        assert_eq!(response.json::<Value>().await.unwrap()["error"], "invalid_redirect_uri");
    }
    let registered = flow.register(REDIRECT).await;
    assert_eq!(registered.status(), 201);
    assert_eq!(registered.headers()["cache-control"], "no-store");
    let body: Value = registered.json().await.unwrap();
    assert_eq!(body["client_name"], "Claude Code");
    assert_eq!(body["redirect_uris"], json!([REDIRECT]));
    assert_eq!((body["grant_types"].clone(), body["response_types"].clone(), body["token_endpoint_auth_method"].clone()), (json!(["authorization_code"]), json!(["code"]), json!("none")));
    let client_id = body["client_id"].as_str().unwrap().to_owned();

    let forged = authorize_params(&format!("{}.forged", client_id.split('.').next().unwrap()), "http://localhost:51234/callback");
    let response = flow.get(&format!("/oauth/mcp/authorize?{}", query_of(&forged))).send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert!(response.headers().get("location").is_none());
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    assert!(response.headers()["content-security-policy"].to_str().unwrap().contains("frame-ancestors 'none'"));
    assert!(response.text().await.unwrap().contains("This sign-in link names an unknown app."));

    let unregistered = authorize_params(&client_id, "http://localhost:51234/elsewhere");
    let response = flow.get(&format!("/oauth/mcp/authorize?{}", query_of(&unregistered))).send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert!(response.headers().get("location").is_none());

    // A valid request is handed to the web app's approval page, query intact.
    let valid = authorize_params(&client_id, "http://localhost:51234/callback");
    let query = query_of(&valid);
    let handoff = flow.get(&format!("/oauth/mcp/authorize?{query}")).send().await.unwrap();
    assert_eq!(handoff.status(), 302);
    assert_eq!(handoff.headers()["location"], format!("/connect-agent?{query}").as_str());
    assert_eq!(handoff.headers()["referrer-policy"], "no-referrer");
    let details = flow.details(&valid, None).await;
    assert_eq!(details, json!({"clientName":"Claude Code","redirectHost":"localhost:51234","environmentHost":HOST}));

    // The approval endpoints re-check the request: a forged client gets a message, not a URL.
    let response = flow.post("/oauth/mcp/approval").json(&forged).send().await.unwrap();
    assert_eq!(response.status(), 400);
    let message: Value = response.json().await.unwrap();
    assert_eq!(message["_tag"], "AuthMcpApprovalError");
    assert!(message.get("redirectTo").is_none());

    // Protocol problems go back to the agent through its verified redirect.
    for (change, error) in [(("response_type", "token"), "unsupported_response_type"), (("code_challenge_method", "plain"), "invalid_request"), (("code_challenge", "short"), "invalid_request"), (("resource", "https://other.example/mcp"), "invalid_target")] {
        let mut broken = valid.clone();
        broken[change.0] = json!(change.1);
        let response = flow.get(&format!("/oauth/mcp/authorize?{}", query_of(&broken))).send().await.unwrap();
        assert_eq!(response.status(), 302, "{change:?}");
        let to = url::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
        assert_eq!((param(&to, "error").as_deref(), param(&to, "state").as_deref(), param(&to, "iss").as_deref()), (Some(error), Some("state-1"), Some(ORIGIN)), "{change:?}");
        let page: Value = flow.post("/oauth/mcp/approval").json(&broken).send().await.unwrap().json().await.unwrap();
        assert_eq!(page["redirectTo"], to.as_str(), "{change:?}");
    }
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pairing_code_signs_an_agent_in_and_the_token_is_only_accepted_by_mcp() {
    let flow = start().await;
    let client_id = flow.registered_client_id().await;
    let params = authorize_params(&client_id, "http://localhost:51234/callback");

    let (status, wrong) = flow.decide(&params, json!({"_tag":"pairing-code","access":"auto","code":"nope"}), None).await;
    assert_eq!(status, 400);
    assert!(wrong["message"].as_str().unwrap().contains("unknown, expired, or already used"), "{wrong}");

    let (status, approved) = flow.decide(&params, json!({"_tag":"pairing-code","access":"auto","code":flow.operator_pairing()}), None).await;
    assert_eq!(status, 200);
    let redirected = callback(&approved["redirectTo"]);
    assert_eq!(redirected.origin().ascii_serialization(), "http://localhost:51234");
    assert_eq!((param(&redirected, "state").as_deref(), param(&redirected, "iss").as_deref()), (Some("state-1"), Some(ORIGIN)));
    let code = param(&redirected, "code").unwrap();
    let resource = format!("{ORIGIN}/mcp");
    let fields = |verifier: &'static str| vec![("grant_type", "authorization_code"), ("code", code.as_str()), ("redirect_uri", "http://localhost:51234/callback"), ("client_id", client_id.as_str()), ("code_verifier", verifier), ("resource", resource.as_str())];
    // Rebuilt per attempt: the form borrows the code.
    let first = flow.exchange(&fields(VERIFIER)).await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["cache-control"], "no-store");
    assert_eq!(first.headers()["pragma"], "no-cache");
    let token: Value = first.json().await.unwrap();
    assert_eq!((token["token_type"].clone(), token["scope"].clone(), token["expires_in"].clone()), (json!("Bearer"), json!("orchestration:read orchestration:operate"), json!(30 * 24 * 3600)));
    let bearer = token["access_token"].as_str().unwrap().to_owned();

    // Codes are single use.
    assert_eq!(flow.exchange(&fields(VERIFIER)).await.status(), 400);

    // /mcp accepts the token, with the label and access that were approved.
    let client = flow.auth.authenticate_mcp_client(&bearer, Utc::now()).unwrap();
    assert_eq!((client.label.as_str(), client.access), ("Claude Code", AuthMcpClientAccess::Auto));
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}});
    let mcp = flow.post("/mcp").header("authorization", format!("Bearer {bearer}")).header("accept", "application/json, text/event-stream").json(&initialize).send().await.unwrap();
    assert_eq!(mcp.status(), 200, "{}", mcp.text().await.unwrap());

    // The same token is refused by the rest of the environment.
    let session: Value = flow.get("/api/auth/session").header("authorization", format!("Bearer {bearer}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(session["authenticated"], false);
    assert_eq!(flow.post("/api/auth/websocket-ticket").header("authorization", format!("Bearer {bearer}")).send().await.unwrap().status(), 401);
    assert_eq!(flow.get("/api/orchestration/shell").header("authorization", format!("Bearer {bearer}")).send().await.unwrap().status(), 401);
    // And a browser session is no MCP client.
    let cookie_token = flow.auth.issue_session("fixture", "bearer-access-token", vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], json!({}), Utc::now(), Duration::hours(1)).unwrap().1;
    assert!(flow.auth.authenticate_mcp_client(&cookie_token, Utc::now()).is_err());
    assert_eq!(flow.post("/mcp").header("authorization", format!("Bearer {cookie_token}")).header("accept", "application/json, text/event-stream").json(&initialize).send().await.unwrap().status(), 401);

    // The agent shows in the list of clients, as a bot, and revoking it ends the token.
    let sessions = flow.auth.list_client_sessions("someone-else", Utc::now()).unwrap();
    let listed = sessions.iter().find(|session| session.client.label.as_ref().is_some_and(|label| label.as_str() == "Claude Code")).expect("the agent is listed");
    assert_eq!(serde_json::to_value(&listed.client.device_type).unwrap(), "bot");
    assert!(flow.auth.revoke(&client.session_id, Utc::now()).unwrap());
    assert_eq!(flow.post("/mcp").header("authorization", format!("Bearer {bearer}")).header("accept", "application/json, text/event-stream").json(&initialize).send().await.unwrap().status(), 401);
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hosted_agent_with_an_https_callback_that_asked_for_a_client_secret_is_signed_in_as_a_public_client() {
    let flow = start().await;
    let callback_url = "https://bot.example/oauth/callback";
    let registered = flow.post("/oauth/mcp/register").json(&json!({"client_name":"Hosted bot","redirect_uris":[callback_url],"token_endpoint_auth_method":"client_secret_post"})).send().await.unwrap();
    assert_eq!(registered.status(), 201);
    let client: Value = registered.json().await.unwrap();
    assert_eq!(client["token_endpoint_auth_method"], "none");
    let client_id = client["client_id"].as_str().unwrap();
    let params = authorize_params(client_id, callback_url);
    let details = flow.details(&params, None).await;
    assert_eq!((details["clientName"].clone(), details["redirectHost"].clone()), (json!("Hosted bot"), json!("bot.example")));
    let (_, approved) = flow.decide(&params, json!({"_tag":"pairing-code","access":"approval-required","code":flow.operator_pairing()}), None).await;
    let redirected = callback(&approved["redirectTo"]);
    assert_eq!(format!("{}{}", redirected.origin().ascii_serialization(), redirected.path()), callback_url);
    let code = param(&redirected, "code").unwrap();
    let response = flow
        .exchange(&[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", callback_url), ("client_id", client_id), ("client_secret", "unused"), ("code_verifier", VERIFIER)])
        .await;
    assert_eq!(response.status(), 200);
    let token: Value = response.json().await.unwrap();
    let signed_in = flow.auth.authenticate_mcp_client(token["access_token"].as_str().unwrap(), Utc::now()).unwrap();
    assert_eq!((signed_in.label.as_str(), signed_in.access), ("Hosted bot", AuthMcpClientAccess::ApprovalRequired));
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_verifier_and_every_other_mismatch_spend_the_code() {
    let flow = start().await;
    let client_id = flow.registered_client_id().await;
    let params = authorize_params(&client_id, "http://localhost:51234/callback");
    let resource = format!("{ORIGIN}/mcp");
    let approve = || async {
        let (_, approved) = flow.decide(&params, json!({"_tag":"pairing-code","access":"approval-required","code":flow.operator_pairing()}), None).await;
        param(&callback(&approved["redirectTo"]), "code").unwrap()
    };
    let wrong_verifier = "b".repeat(64);
    let cases: Vec<(&str, Vec<(&str, String)>)> = vec![
        ("wrong verifier", vec![("redirect_uri", "http://localhost:51234/callback".into()), ("client_id", client_id.clone()), ("code_verifier", wrong_verifier)]),
        ("other redirect", vec![("redirect_uri", "http://localhost:51235/callback".into()), ("client_id", client_id.clone()), ("code_verifier", VERIFIER.into())]),
        ("verifier too short", vec![("redirect_uri", "http://localhost:51234/callback".into()), ("client_id", client_id.clone()), ("code_verifier", "a".repeat(42))]),
        ("other resource", vec![("redirect_uri", "http://localhost:51234/callback".into()), ("client_id", client_id.clone()), ("code_verifier", VERIFIER.into()), ("resource", "https://other.example/mcp".into())]),
    ];
    for (name, extra) in cases {
        let code = approve().await;
        let mut fields: Vec<(&str, String)> = vec![("grant_type", "authorization_code".into()), ("code", code.clone())];
        fields.extend(extra);
        let response = flow.post("/oauth/mcp/token").form(&fields).send().await.unwrap();
        assert_eq!(response.status(), 400, "{name}");
        assert_eq!(response.json::<Value>().await.unwrap(), json!({"error":"invalid_grant","error_description":"The authorization code is invalid or expired."}), "{name}");
        // The code is gone: the right request after the wrong one fails too.
        let retry = flow.exchange(&[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", "http://localhost:51234/callback"), ("client_id", &client_id), ("code_verifier", VERIFIER), ("resource", &resource)]).await;
        assert_eq!(retry.status(), 400, "{name}");
    }
    // The other requests the token endpoint refuses.
    let refused = |fields: Vec<(&'static str, &'static str)>| {
        let flow = &flow;
        async move {
            let response = flow.exchange(&fields).await;
            (response.status().as_u16(), response.json::<Value>().await.unwrap())
        }
    };
    assert_eq!(refused(vec![("grant_type", "refresh_token")]).await.1["error"], "unsupported_grant_type");
    assert_eq!(refused(vec![]).await.1["error"], "unsupported_grant_type");
    assert_eq!(refused(vec![("grant_type", "authorization_code"), ("code", "x")]).await.1["error"], "invalid_request");
    assert_eq!(refused(vec![("grant_type", "authorization_code"), ("code", "x"), ("redirect_uri", "r"), ("client_id", "nobody"), ("code_verifier", "v")]).await.1["error"], "invalid_client");
    // A token request that is not a form does not decode.
    let json_body = flow.post("/oauth/mcp/token").json(&json!({"grant_type":"authorization_code"})).send().await.unwrap();
    assert_eq!(json_body.status(), 400);
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn denial_and_pairing_codes_that_cannot_approve_are_refused() {
    let flow = start().await;
    let client_id = flow.registered_client_id().await;
    let params = authorize_params(&client_id, "http://localhost:51234/callback");

    let (_, denied) = flow.decide(&params, json!({"_tag":"deny"}), None).await;
    let denied = callback(&denied["redirectTo"]);
    assert_eq!((param(&denied, "error").as_deref(), param(&denied, "state").as_deref()), (Some("access_denied"), Some("state-1")));

    let approve_with = |code: String, access: &'static str| {
        let (flow, params) = (&flow, &params);
        async move { flow.decide(params, json!({"_tag":"pairing-code","access":access,"code":code}), None).await }
    };
    // A T3 Connect code is bound to a device key: refused, and still usable by its device.
    let bound = flow.auth.issue_pairing_link_bound(&[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], "one-time-token", None, Utc::now(), Duration::minutes(5), Some("device-key-thumbprint")).unwrap();
    assert_eq!(approve_with(bound.credential.clone(), "auto").await.0, 400);
    assert!(flow.auth.exchange_pairing_dpop(&bound.credential, None, json!({"deviceType":"mobile"}), Utc::now(), "device-key-thumbprint").is_ok(), "the code was not spent");

    // A code without the scopes the access needs is refused, and used up.
    let read_only_code = flow.pairing(&[AuthEnvironmentScope::OrchestrationRead]);
    let (status, refused) = approve_with(read_only_code.clone(), "auto").await;
    assert_eq!(status, 400);
    assert!(refused["message"].as_str().unwrap().contains("cannot grant this access"), "{refused}");
    assert!(approve_with(read_only_code, "read-only").await.1["message"].as_str().unwrap().contains("unknown, expired, or already used"));

    // A read-only code can approve read-only access, and the token holds that alone.
    let (status, approved) = approve_with(flow.pairing(&[AuthEnvironmentScope::OrchestrationRead]), "read-only").await;
    assert_eq!(status, 200);
    let code = param(&callback(&approved["redirectTo"]), "code").unwrap();
    let token: Value = flow
        .exchange(&[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", "http://localhost:51234/callback"), ("client_id", &client_id), ("code_verifier", VERIFIER), ("resource", &format!("{ORIGIN}/mcp"))])
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(token["scope"], "orchestration:read");
    let client = flow.auth.authenticate_mcp_client(token["access_token"].as_str().unwrap(), Utc::now()).unwrap();
    assert_eq!(client.access, AuthMcpClientAccess::ReadOnly);
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_click_approval_covers_only_the_access_the_browser_session_holds_the_scopes_for() {
    let flow = start().await;
    let client_id = flow.registered_client_id().await;
    let params = authorize_params(&client_id, "http://localhost:51234/callback");

    let admin = flow.browser_cookie(t3_contracts::AUTH_ADMINISTRATIVE_SCOPES).await;
    let admin_details = flow.details(&params, Some(&admin)).await;
    assert!(admin_details["csrfToken"].is_string());
    assert_eq!(admin_details["oneClickAccess"], json!(["read-only", "approval-required", "auto-accept-edits", "auto", "full-access"]));
    let (status, one_click) = flow.decide(&params, json!({"_tag":"browser-session","access":"auto","csrfToken":admin_details["csrfToken"]}), Some(&admin)).await;
    assert_eq!(status, 200);
    assert!(param(&callback(&one_click["redirectTo"]), "code").is_some());
    // The token belongs to this request: another request's CSRF token does not approve this one.
    let other = authorize_params(&client_id, "http://localhost:9999/callback");
    let other_details = flow.details(&other, Some(&admin)).await;
    assert_ne!(other_details["csrfToken"], admin_details["csrfToken"]);
    let (status, _) = flow.decide(&params, json!({"_tag":"browser-session","access":"auto","csrfToken":other_details["csrfToken"]}), Some(&admin)).await;
    assert_eq!(status, 400);

    // access:write alone cannot hand an agent thread control it does not hold.
    let access_only = flow.browser_cookie(&[AuthEnvironmentScope::AccessRead, AuthEnvironmentScope::AccessWrite]).await;
    assert!(flow.details(&params, Some(&access_only)).await.get("csrfToken").is_none());
    let (status, forged) = flow.decide(&params, json!({"_tag":"browser-session","access":"auto","csrfToken":"forged"}), Some(&access_only)).await;
    assert_eq!(status, 400);
    assert!(forged["message"].as_str().unwrap().contains("Enter a pairing code instead"));
    // No cookie at all is the same.
    assert_eq!(flow.decide(&params, json!({"_tag":"browser-session","access":"read-only","csrfToken":"x"}), None).await.0, 400);

    // A session that can read but not operate threads may approve read-only access only.
    let reader = flow.browser_cookie(&[AuthEnvironmentScope::AccessWrite, AuthEnvironmentScope::OrchestrationRead]).await;
    let reader_details = flow.details(&params, Some(&reader)).await;
    assert!(reader_details["csrfToken"].is_string());
    assert_eq!(reader_details["oneClickAccess"], json!(["read-only"]));
    let (status, _) = flow.decide(&params, json!({"_tag":"browser-session","access":"auto","csrfToken":reader_details["csrfToken"]}), Some(&reader)).await;
    assert_eq!(status, 400);
    let (status, read_only) = flow.decide(&params, json!({"_tag":"browser-session","access":"read-only","csrfToken":reader_details["csrfToken"]}), Some(&reader)).await;
    assert_eq!(status, 200);
    assert!(param(&callback(&read_only["redirectTo"]), "code").is_some());
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unauthorized_mcp_request_is_pointed_at_the_oauth_metadata_unless_it_looks_like_a_provider_token() {
    let flow = start().await;
    let challenge = |response: &reqwest::Response| response.headers()["www-authenticate"].to_str().unwrap().to_owned();
    let metadata = format!("resource_metadata=\"{ORIGIN}/.well-known/oauth-protected-resource/mcp\"");
    let send = |authorization: Option<&'static str>| {
        let flow = &flow;
        async move {
            let mut request = flow.post("/mcp").header("accept", "application/json, text/event-stream").json(&json!({}));
            if let Some(authorization) = authorization {
                request = request.header("authorization", authorization);
            }
            request.send().await.unwrap()
        }
    };
    let missing = send(None).await;
    assert_eq!((missing.status().as_u16(), challenge(&missing)), (401, format!("Bearer {metadata}")));
    assert_eq!(missing.headers()["cache-control"], "no-store");
    // A token with a dot looks like a session token: an expired client sign-in is sent to OAuth again.
    let stale = send(Some("Bearer payload.signature")).await;
    assert_eq!(challenge(&stale), format!("Bearer {metadata}, error=\"invalid_token\""));
    // A bare secret is a provider session's: it is never sent into an OAuth flow.
    let provider = send(Some("Bearer bareprovidersecret")).await;
    assert_eq!(challenge(&provider), "Bearer error=\"invalid_token\"");
    assert_eq!(stale.json::<Value>().await.unwrap()["error"], "invalid_mcp_credential");
    flow.finish().await;
}

#[test]
fn the_signed_pieces_are_the_originals_byte_for_byte() {
    // A key of the bytes 0 to 31, as the generator signs with.
    let key: Vec<u8> = (0..32).collect();
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-mcp-oauth-fixture-").tempdir().unwrap();
    let store = Store::open(directory.path().join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store, [9; 32], "t3_session_oauth".into(), "loopback-browser".into()).unwrap();
    let oauth = McpOAuth::with_clock(auth, key, Arc::new(Utc::now));
    let rows: Vec<Value> = include_str!("fixtures/mcp-oauth.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let (mut ids, mut csrf, mut pkce, mut redirects) = (0, 0, 0, 0);
    for row in &rows {
        match row["kind"].as_str().unwrap() {
            "clientId" => {
                let uris: Vec<String> = row["redirectUris"].as_array().unwrap().iter().map(|uri| uri.as_str().unwrap().to_owned()).collect();
                let registration: AuthMcpClientRegistration = serde_json::from_value(json!({"client_name":row["name"],"redirect_uris":uris})).unwrap();
                let client = oauth.register(&registration).unwrap_or_else(|error| panic!("{} was not registered: {error:?}", row["name"]));
                assert_eq!(client.client_id, row["clientId"], "{}", row["name"]);
                ids += 1;
            }
            "pkce" => {
                assert!(mcp_oauth::pkce_verifies(row["verifier"].as_str().unwrap(), row["challenge"].as_str().unwrap()), "{row}");
                assert!(!mcp_oauth::pkce_verifies(row["verifier"].as_str().unwrap(), "x".repeat(43).as_str()));
                pkce += 1;
            }
            "csrf" => {
                // Parsed through the client id, so the request is one the server would have validated.
                let client_id = row["clientId"].as_str().unwrap();
                let registered = rows.iter().find(|candidate| candidate["clientId"] == row["clientId"]).unwrap();
                let authorization = mcp_oauth::AuthorizationRequest {
                    client: mcp_oauth::McpOAuthClient { client_id: client_id.to_owned(), name: registered["name"].as_str().unwrap().to_owned(), redirect_uris: vec![] },
                    redirect_uri: row["redirectUri"].as_str().unwrap().to_owned(),
                    code_challenge: row["codeChallenge"].as_str().unwrap().to_owned(),
                    state: None,
                    resource: String::new(),
                    issuer: String::new(),
                };
                assert_eq!(oauth.csrf_token(row["sessionId"].as_str().unwrap(), &authorization), row["token"]);
                csrf += 1;
            }
            "redirectMatches" => {
                assert_eq!(mcp_oauth::redirect_matches(row["registered"].as_str().unwrap(), row["presented"].as_str().unwrap()), row["result"], "{row}");
                redirects += 1;
            }
            other => panic!("{other}"),
        }
    }
    assert!(ids >= 6 && csrf >= 2 && pkce >= 3 && redirects >= 20, "{ids} {csrf} {pkce} {redirects}");
    // A client id signed with another key, or edited, names no app.
    let foreign = McpOAuth::with_clock(
        AuthService::new(Store::open(directory.path().join("other.sqlite")).unwrap(), [9; 32], "t3_session_oauth".into(), "loopback-browser".into()).unwrap(),
        vec![7; 32],
        Arc::new(Utc::now),
    );
    let registration: AuthMcpClientRegistration = serde_json::from_value(json!({"client_name":"A","redirect_uris":[REDIRECT]})).unwrap();
    let issued = oauth.register(&registration).unwrap().client_id;
    let request = |client_id: &str| serde_json::from_value(json!({"client_id":client_id,"redirect_uri":"http://localhost:5/callback","response_type":"code","code_challenge":CHALLENGE,"code_challenge_method":"S256"})).unwrap();
    let urls = mcp_oauth::McpOAuthUrls { issuer: ORIGIN.into(), resource: format!("{ORIGIN}/mcp") };
    assert!(oauth.validate_authorization(&urls, &request(&issued)).is_ok());
    assert!(foreign.validate_authorization(&urls, &request(&issued)).is_err());
    let (body, signature) = issued.split_once('.').unwrap();
    for edited in [format!("{body}x.{signature}"), format!("{body}.{signature}x"), format!("{body}."), body.to_owned(), format!("{issued}.extra"), String::new()] {
        assert!(oauth.validate_authorization(&urls, &request(&edited)).is_err(), "{edited}");
    }
}

#[test]
fn registration_names_and_limits_follow_the_original() {
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-mcp-oauth-register-").tempdir().unwrap();
    let auth = AuthService::new(Store::open(directory.path().join("rust-state.sqlite")).unwrap(), [9; 32], "t3_session_oauth".into(), "loopback-browser".into()).unwrap();
    let oauth = McpOAuth::with_clock(auth, vec![1; 32], Arc::new(Utc::now));
    let register = |value: Value| oauth.register(&serde_json::from_value::<AuthMcpClientRegistration>(value).unwrap());
    assert_eq!(register(json!({"client_name":"  Padded name \n","redirect_uris":[REDIRECT]})).unwrap().name, "Padded name");
    assert_eq!(register(json!({"client_name":"   ","redirect_uris":[REDIRECT]})).unwrap().name, "MCP client");
    assert_eq!(register(json!({"redirect_uris":[REDIRECT]})).unwrap().name, "MCP client");
    assert_eq!(register(json!({"client_name":"y".repeat(150),"redirect_uris":[REDIRECT]})).unwrap().name, "y".repeat(100));
    let many = |count: usize| json!({"redirect_uris":(0..count).map(|index| format!("http://localhost/{index}")).collect::<Vec<_>>()});
    assert!(register(many(5)).is_ok());
    for refused in [many(0), many(6), json!({})] {
        let error = register(refused).unwrap_err();
        assert_eq!(error.error_description, "Register between 1 and 5 redirect URIs.");
    }
    let error = register(json!({"redirect_uris":["http://bot.example/cb"]})).unwrap_err();
    assert_eq!(error.error_description, "Redirect URIs must be https, or http on localhost, 127.0.0.1 or [::1].");
    // A redirect URI longer than 512 UTF-16 units is refused.
    assert!(register(json!({"redirect_uris":[format!("https://bot.example/{}", "a".repeat(500))]})).is_err());
}

/// A clock a test moves.
struct Moving(Mutex<DateTime<Utc>>);
impl Moving {
    fn at(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

#[test]
fn an_authorization_code_lives_sixty_seconds_and_a_session_thirty_days_on_the_injected_clock() {
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-mcp-oauth-clock-").tempdir().unwrap();
    let start = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
    let clock = Arc::new(Moving(Mutex::new(start)));
    let auth = AuthService::new(Store::open(directory.path().join("rust-state.sqlite")).unwrap(), [9; 32], "t3_session_oauth".into(), "loopback-browser".into()).unwrap();
    let reading = clock.clone();
    let oauth = McpOAuth::with_clock(auth.clone(), vec![3; 32], Arc::new(move || reading.at()));
    let registration: AuthMcpClientRegistration = serde_json::from_value(json!({"client_name":"Timed","redirect_uris":[REDIRECT]})).unwrap();
    let client = oauth.register(&registration).unwrap();
    let urls = mcp_oauth::McpOAuthUrls { issuer: ORIGIN.into(), resource: format!("{ORIGIN}/mcp") };
    let request: t3_contracts::AuthMcpAuthorizationRequest = serde_json::from_value(json!({"client_id":client.client_id,"redirect_uri":"http://localhost:5/callback","response_type":"code","code_challenge":CHALLENGE,"code_challenge_method":"S256","state":"s"})).unwrap();
    let authorization = oauth.validate_authorization(&urls, &request).unwrap();
    let approve = |oauth: &McpOAuth, auth: &AuthService, now: DateTime<Utc>| {
        let credential = auth.create_pairing_credential(&[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], now, Duration::hours(2)).unwrap();
        let to = oauth.approve(None, &authorization, mcp_oauth::Approval::PairingCode { access: AuthMcpClientAccess::Auto, code: &credential }).unwrap();
        url::Url::parse(&to).unwrap().query_pairs().find(|(name, _)| name == "code").unwrap().1.into_owned()
    };
    let token_request = |code: &str| -> t3_contracts::AuthMcpTokenRequest {
        serde_json::from_value(json!({"grant_type":"authorization_code","code":code,"redirect_uri":"http://localhost:5/callback","client_id":client.client_id,"code_verifier":VERIFIER})).unwrap()
    };
    // Used at exactly its lifetime it is already expired; a second before, it is good.
    let code = approve(&oauth, &auth, start);
    *clock.0.lock().unwrap() = start + Duration::milliseconds(60_000);
    assert_eq!(oauth.exchange_code(json!({}), &urls, &token_request(&code)).unwrap_err().error, t3_contracts::AuthMcpTokenErrorCode::InvalidGrant);
    *clock.0.lock().unwrap() = start;
    let code = approve(&oauth, &auth, start);
    *clock.0.lock().unwrap() = start + Duration::milliseconds(59_999);
    let token = oauth.exchange_code(json!({}), &urls, &token_request(&code)).unwrap();
    assert_eq!(token.expires_in, serde_json::Number::from(30 * 24 * 3600));
    // The session lasts thirty days from the exchange, and not a moment longer.
    let bearer = token.access_token.as_str();
    assert!(auth.authenticate_mcp_client(bearer, start + Duration::days(30) - Duration::seconds(1)).is_ok());
    assert!(auth.authenticate_mcp_client(bearer, start + Duration::days(31)).is_err());
    // Expired codes are dropped when the next one is minted: they cannot be presented later even with the clock set back.
    *clock.0.lock().unwrap() = start;
    let stale = approve(&oauth, &auth, start);
    *clock.0.lock().unwrap() = start + Duration::seconds(120);
    let _fresh = approve(&oauth, &auth, start + Duration::seconds(120));
    *clock.0.lock().unwrap() = start;
    assert!(oauth.exchange_code(json!({}), &urls, &token_request(&stale)).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn client_ids_survive_a_restart_because_the_signing_key_is_a_secret_of_the_state_directory() {
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-mcp-oauth-key-").tempdir().unwrap();
    let make = || {
        let store = Store::open(directory.path().join("rust-state.sqlite")).unwrap();
        let auth = AuthService::new(store, [9; 32], "t3_session_oauth".into(), "loopback-browser".into()).unwrap();
        McpOAuth::new(auth, &t3_server::server_secret_store::ServerSecretStore::open(directory.path().join("secrets")).unwrap()).unwrap()
    };
    let registration: AuthMcpClientRegistration = serde_json::from_value(json!({"client_name":"Persistent","redirect_uris":[REDIRECT]})).unwrap();
    let first = make().register(&registration).unwrap().client_id;
    assert_eq!(make().register(&registration).unwrap().client_id, first);
    assert_eq!(std::fs::read(directory.path().join("secrets").join("mcp-oauth-signing-key.bin")).unwrap().len(), 32);
}
