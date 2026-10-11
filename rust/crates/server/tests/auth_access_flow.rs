//! Pairing links and signed-in clients: what the original answers (`auth-access.jsonl`, written by
//! `rust/tools/generate_auth_access_fixtures.mjs` from its unchanged `EnvironmentAuth`), and the HTTP
//! routes and the `subscribeAuthAccess` stream over a real socket.
mod rpc_support;
use chrono::{DateTime, Duration, Utc};
use rpc_support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use t3_contracts::AuthEnvironmentScope::{self, *};
use t3_server::{auth::AuthService, auth::RevokeClientError, persistence::Store, test_support::HANG_BOUND};

fn scopes_of(value: &Value) -> Vec<AuthEnvironmentScope> {
    serde_json::from_value(value.clone()).unwrap()
}

fn sorted_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

/// Replays every step of the original's scenario against the service and compares what each answers,
/// with ids replaced by the names the steps gave them.
#[test]
fn the_service_answers_as_the_original_does() {
    let auth = AuthService::new(Store::memory().unwrap(), [7; 32], "t3_session_fixture".into(), "loopback-browser".into()).unwrap();
    let start: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
    let mut clock = start;
    let (mut links, mut credentials, mut sessions): (HashMap<String, String>, HashMap<String, String>, HashMap<String, String>) = Default::default();
    let name_of = |names: &HashMap<String, String>, id: &str| names.iter().find(|(_, value)| value.as_str() == id).map(|(name, _)| name.clone()).unwrap_or_else(|| format!("unknown:{id}"));
    let mut checked = 0;
    for line in include_str!("fixtures/auth-access.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let (step, expected) = (&row["step"], &row["result"]);
        let text = |key: &str| step[key].as_str().map(str::to_owned);
        let session_id = |sessions: &HashMap<String, String>, key: &str| sessions[step[key].as_str().unwrap()].clone();
        let actual = match step["op"].as_str().unwrap() {
            "link" => {
                let scopes = step.get("scopes").map_or_else(|| t3_contracts::AUTH_STANDARD_CLIENT_SCOPES.to_vec(), scopes_of);
                let subject = text("subject").unwrap_or_else(|| "one-time-token".into());
                let issued = auth.issue_pairing_link(&scopes, &subject, text("label").as_deref(), clock, Duration::minutes(5)).unwrap();
                links.insert(text("name").unwrap(), issued.id.clone());
                credentials.insert(text("name").unwrap(), issued.credential.clone());
                clock += Duration::milliseconds(3);
                json!({"credentialLength":issued.credential.len(),"expiresInMs":(issued.expires_at - issued.created_at).num_milliseconds()})
            }
            "listLinks" => Value::Array(
                auth.list_pairing_links(clock)
                    .unwrap()
                    .into_iter()
                    .map(|link| {
                        let wire = serde_json::to_value(&link).unwrap();
                        json!({"name":name_of(&links,link.id.as_str()),"subject":wire["subject"],"label":wire.get("label").cloned().unwrap_or(Value::Null),"scopes":wire["scopes"],"permissions":wire["permissions"],"keys":sorted_keys(&wire)})
                    })
                    .collect(),
            ),
            "revokeLink" => json!(auth.revoke_pairing_link(links.get(step["name"].as_str().unwrap()).map_or("missing", String::as_str), clock).unwrap()),
            "consume" => {
                let requested = (!step["scopes"].is_null()).then(|| scopes_of(&step["scopes"]));
                let (session, _) = auth
                    .exchange_pairing_bearer(&credentials[step["link"].as_str().unwrap()], requested.as_deref(), json!({"deviceType":step["device"]}), clock)
                    .unwrap();
                sessions.insert(text("session").unwrap(), session.session_id.clone());
                clock += Duration::milliseconds(3);
                let scope = session.scopes.iter().map(|scope| serde_json::to_value(scope).unwrap().as_str().unwrap().to_owned()).collect::<Vec<_>>().join(" ");
                json!({"scope":scope})
            }
            "issue" => {
                let (session, _) = auth
                    .issue_session(&text("session").unwrap(), "bearer-access-token", scopes_of(&step["scopes"]), json!({"label":step["label"],"deviceType":"bot"}), clock, Duration::days(30))
                    .unwrap();
                sessions.insert(text("session").unwrap(), session.session_id);
                clock += Duration::milliseconds(3);
                Value::Null
            }
            "connect" => {
                auth.mark_connected(&session_id(&sessions, "session"), clock);
                Value::Null
            }
            "disconnect" => {
                auth.mark_disconnected(&session_id(&sessions, "session"), clock);
                Value::Null
            }
            "listClients" => Value::Array(
                auth.list_client_sessions(&session_id(&sessions, "current"), clock)
                    .unwrap()
                    .into_iter()
                    .map(|client| {
                        let wire = serde_json::to_value(&client).unwrap();
                        json!({"name":name_of(&sessions,client.session_id.as_str()),"subject":wire["subject"],"method":wire["method"],"scopes":wire["scopes"],"permissions":wire["permissions"],"client":wire["client"],"connected":wire["connected"],"current":wire["current"],"hasLastConnectedAt":!wire["lastConnectedAt"].is_null(),"keys":sorted_keys(&wire)})
                    })
                    .collect(),
            ),
            "revokeClient" => match auth.revoke_client_session(&session_id(&sessions, "current"), &session_id(&sessions, "target"), clock) {
                Ok(revoked) => json!(revoked),
                Err(RevokeClientError::CurrentSession) => json!("forbidden"),
                Err(error) => panic!("{error}"),
            },
            "revokeOthers" => json!(auth.revoke_all_except(&session_id(&sessions, "current"), clock).unwrap()),
            other => panic!("unknown step {other}"),
        };
        assert_eq!(&actual, expected, "step {checked}: {step}");
        checked += 1;
    }
    assert_eq!(checked, 31);
}

async fn within<T>(wait: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, wait).await.expect("nothing arrived before the hang bound")
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}

/// The next stream chunk's first value, acknowledged.
async fn next_event(socket: &mut Socket, id: u64) -> Value {
    loop {
        let message = within(next(socket)).await;
        if message["_tag"] == "Chunk" && message["requestId"] == id {
            let event = message["values"][0].clone();
            send(socket, json!({"_tag":"Ack","requestId":id})).await;
            return event;
        }
        assert_ne!(message["_tag"], "Exit", "the stream ended: {message}");
    }
}

#[test]
fn pairing_links_and_clients_are_managed_over_http() {
    runtime().block_on(Box::pin(async {
        let h = harness().await;
        let now = Utc::now();
        let issue = |scopes: Vec<AuthEnvironmentScope>, label: &str| h.auth.issue_session("admin", "bearer-access-token", scopes, json!({"deviceType":"desktop","label":label}), now, Duration::hours(1)).unwrap();
        let (admin_session, admin) = issue(vec![OrchestrationRead, OrchestrationOperate, AccessRead, AccessWrite], "admin");
        let (_, reader) = issue(vec![OrchestrationRead], "reader");
        let (_, reader_and_lister) = issue(vec![OrchestrationRead, AccessRead], "lister");
        let base = format!("http://{}", h.address);
        let client = http();
        let call = |method: reqwest::Method, path: &str, token: Option<&str>, body: Option<Value>| {
            let mut request = client.request(method, format!("{base}{path}"));
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            if let Some(body) = body {
                request = request.body(body.to_string()).header("content-type", "application/json");
            }
            async move {
                let response = request.send().await.unwrap();
                let status = response.status().as_u16();
                let text = response.text().await.unwrap();
                (status, serde_json::from_str::<Value>(&text).unwrap_or(Value::Null), text)
            }
        };

        // Without a session, nothing; a session without the scope is told which scope it lacks.
        assert_eq!(call(reqwest::Method::GET, "/api/auth/pairing-links", None, None).await.0, 401);
        for (method, path, body) in [
            (reqwest::Method::POST, "/api/auth/pairing-token", Some(json!({}))),
            (reqwest::Method::POST, "/api/auth/pairing-links/revoke", Some(json!({"id":"x"}))),
            (reqwest::Method::POST, "/api/auth/clients/revoke", Some(json!({"sessionId":"x"}))),
            (reqwest::Method::POST, "/api/auth/clients/revoke-others", None),
        ] {
            let (status, error, _) = call(method, path, Some(&reader_and_lister), body).await;
            assert_eq!((status, &error["_tag"], &error["requiredScope"], &error["requiredPermission"]), (403, &json!("EnvironmentScopeRequiredError"), &json!("access:write"), &json!("access:write")), "{path}");
        }
        for path in ["/api/auth/pairing-links", "/api/auth/clients"] {
            let (status, error, _) = call(reqwest::Method::GET, path, Some(&reader), None).await;
            assert_eq!((status, &error["requiredScope"]), (403, &json!("access:read")), "{path}");
        }

        // A link names what it grants, and the caller can grant only what it holds.
        let (status, error, _) = call(reqwest::Method::POST, "/api/auth/pairing-token", Some(&admin), Some(json!({"scopes":["settings:write"]}))).await;
        assert_eq!((status, &error["requiredScope"], &error["requiredPermission"]), (403, &json!("orchestration:operate"), &json!("settings:write")));
        for scopes in [json!([]), json!(["orchestration:read", "orchestration:read"])] {
            let (status, error, _) = call(reqwest::Method::POST, "/api/auth/pairing-token", Some(&admin), Some(json!({"scopes":scopes}))).await;
            assert_eq!((status, &error["_tag"], &error["reason"]), (400, &json!("EnvironmentRequestInvalidError"), &json!("invalid_scope")));
        }
        // A body the schema does not accept is an empty 400, as the original's HTTP API renders it.
        for body in [json!({"scopes":["review:write"]}), json!({"scopes":"orchestration:read"}), json!({"label":""})] {
            let (status, _, text) = call(reqwest::Method::POST, "/api/auth/pairing-token", Some(&admin), Some(body.clone())).await;
            assert_eq!((status, text.as_str()), (400, "null"), "{body}");
        }

        let (status, issued, _) = call(reqwest::Method::POST, "/api/auth/pairing-token", Some(&admin), Some(json!({"label":"Kitchen iPad","scopes":["orchestration:read"]}))).await;
        assert_eq!(status, 200, "{issued}");
        let credential = issued["credential"].as_str().unwrap().to_owned();
        assert_eq!(credential.len(), 12);
        assert!(credential.bytes().all(|byte| b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ".contains(&byte)));
        assert_eq!(issued["label"], "Kitchen iPad");
        let expires: DateTime<Utc> = issued["expiresAt"].as_str().unwrap().parse().unwrap();
        assert!((expires - Utc::now()).num_seconds() > 5 * 60 - 30 && (expires - Utc::now()).num_seconds() <= 5 * 60);
        let t3_contracts::AuthPairingCredentialResult { .. } = serde_json::from_value(issued.clone()).unwrap();

        let (_, listed, _) = call(reqwest::Method::GET, "/api/auth/pairing-links", Some(&reader_and_lister), None).await;
        let links: Vec<t3_contracts::AuthPairingLink> = serde_json::from_value(listed.clone()).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!((links[0].id.as_str(), links[0].label.as_ref().map(|label| label.as_str()), links[0].subject.as_str()), (issued["id"].as_str().unwrap(), Some("Kitchen iPad"), "one-time-token"));
        assert!(!listed.to_string().contains(&credential), "a listed link never carries its credential");

        // The credential pairs a browser; the link is used up and the client carries its label.
        let (status, _, _) = call(reqwest::Method::POST, "/api/auth/browser-session", None, Some(json!({"credential":credential}))).await;
        assert_eq!(status, 200);
        assert_eq!(call(reqwest::Method::POST, "/api/auth/browser-session", None, Some(json!({"credential":credential}))).await.0, 401);
        let (_, listed, _) = call(reqwest::Method::GET, "/api/auth/pairing-links", Some(&admin), None).await;
        assert_eq!(listed, json!([]));
        let (_, revoked, _) = call(reqwest::Method::POST, "/api/auth/pairing-links/revoke", Some(&admin), Some(json!({"id":issued["id"]}))).await;
        assert_eq!(revoked, json!({"revoked":false}));

        let (_, listed, _) = call(reqwest::Method::GET, "/api/auth/clients", Some(&admin), None).await;
        let clients: Vec<t3_contracts::AuthClientSession> = serde_json::from_value(listed).unwrap();
        // The harness's own two sessions, the three made here, and the paired browser.
        assert_eq!(clients.len(), 6);
        let ipad = clients.iter().find(|client| client.client.label.as_ref().is_some_and(|label| label.as_str() == "Kitchen iPad")).expect("the paired browser is listed under its link's label");
        assert_eq!((ipad.subject.as_str(), ipad.current, ipad.permissions.as_deref()), ("one-time-token", false, Some(&[OrchestrationRead][..])));
        assert!(clients.iter().filter(|client| client.current).map(|client| client.session_id.as_str()).eq([admin_session.session_id.as_str()]));
        // The 127.0.0.1 peer is what the list shows for the address.
        assert_eq!(ipad.client.ip_address.as_ref().map(|ip| ip.as_str()), Some("127.0.0.1"));

        // Another client can be ended; the caller's own session cannot.
        let (status, error, _) = call(reqwest::Method::POST, "/api/auth/clients/revoke", Some(&admin), Some(json!({"sessionId":admin_session.session_id}))).await;
        assert_eq!((status, &error["_tag"], &error["reason"]), (403, &json!("EnvironmentOperationForbiddenError"), &json!("current_session_revoke_not_allowed")));
        let (_, revoked, _) = call(reqwest::Method::POST, "/api/auth/clients/revoke", Some(&admin), Some(json!({"sessionId":ipad.session_id}))).await;
        assert_eq!(revoked, json!({"revoked":true}));
        let (_, revoked, _) = call(reqwest::Method::POST, "/api/auth/clients/revoke", Some(&admin), Some(json!({"sessionId":ipad.session_id}))).await;
        assert_eq!(revoked, json!({"revoked":false}));
        // A revoked bearer is refused at once.
        assert_eq!(call(reqwest::Method::GET, "/api/auth/session", Some(&reader), None).await.0, 200);
        let (_, others, _) = call(reqwest::Method::POST, "/api/auth/clients/revoke-others", Some(&admin), None).await;
        assert_eq!(others, json!({"revokedCount":4}));
        assert_eq!(call(reqwest::Method::GET, "/api/auth/clients", Some(&reader_and_lister), None).await.0, 401);
        let (_, others, _) = call(reqwest::Method::POST, "/api/auth/clients/revoke-others", Some(&admin), None).await;
        assert_eq!(others, json!({"revokedCount":0}));
        h.finish().await;
    }));
}

#[test]
fn the_access_stream_follows_links_and_clients() {
    runtime().block_on(Box::pin(async {
        let h = harness().await;
        let now = Utc::now();
        let (admin_session, admin) = h.auth.issue_session("admin", "bearer-access-token", vec![OrchestrationRead, AccessRead, AccessWrite], json!({"deviceType":"desktop"}), now, Duration::hours(1)).unwrap();
        let (watched_session, watched) = h.auth.issue_session("watched", "bearer-access-token", vec![OrchestrationRead], json!({"deviceType":"mobile","label":"Watched"}), now, Duration::hours(1)).unwrap();
        let (_, no_access) = h.auth.issue_session("plain", "bearer-access-token", vec![OrchestrationRead], json!({"deviceType":"unknown"}), now, Duration::hours(1)).unwrap();
        let mut plain = h.connect(&no_access).await;
        send(&mut plain, json!({"_tag":"Request","id":1,"tag":"subscribeAuthAccess","payload":{},"headers":[]})).await;
        let refused = loop {
            let message = within(next(&mut plain)).await;
            if message["_tag"] == "Exit" {
                break message;
            }
        };
        assert_eq!(failure(&refused["exit"])["requiredScope"], "access:read");

        let mut socket = h.connect(&admin).await;
        send(&mut socket, json!({"_tag":"Request","id":2,"tag":"subscribeAuthAccess","payload":{},"headers":[]})).await;
        let snapshot = next_event(&mut socket, 2).await;
        assert_eq!((&snapshot["type"], &snapshot["revision"], &snapshot["version"]), (&json!("snapshot"), &json!(1), &json!(1)));
        let decoded: t3_contracts::AuthAccessStreamEvent = serde_json::from_value(snapshot.clone()).unwrap();
        let t3_contracts::AuthAccessStreamEvent::Snapshot { payload, .. } = decoded else { panic!("{snapshot}") };
        assert_eq!(payload.pairing_links.len(), 0);
        assert_eq!(payload.client_sessions.len(), 5);
        assert!(payload.client_sessions.iter().filter(|client| client.current).map(|client| client.session_id.as_str()).eq([admin_session.session_id.as_str()]));
        // The admin's own socket is open, and the stream said so.
        assert!(payload.client_sessions.iter().any(|client| client.session_id.as_str() == admin_session.session_id && client.connected));

        // A link made: upserted; revoked: removed.
        let link = h.auth.issue_pairing_link(&[OrchestrationRead], "one-time-token", Some("Spare"), Utc::now(), Duration::minutes(5)).unwrap();
        let upserted = next_event(&mut socket, 2).await;
        assert_eq!((&upserted["type"], &upserted["revision"], &upserted["payload"]["id"], &upserted["payload"]["label"]), (&json!("pairingLinkUpserted"), &json!(2), &json!(link.id), &json!("Spare")));
        serde_json::from_value::<t3_contracts::AuthAccessStreamEvent>(upserted).unwrap();
        assert!(h.auth.revoke_pairing_link(&link.id, Utc::now()).unwrap());
        let removed = next_event(&mut socket, 2).await;
        assert_eq!((&removed["type"], &removed["revision"], &removed["payload"]), (&json!("pairingLinkRemoved"), &json!(3), &json!({"id":link.id})));
        // The server's own startup link is not shown.
        h.auth.issue_pairing_link(&[OrchestrationRead], "administrative-bootstrap", None, Utc::now(), Duration::minutes(5)).unwrap();
        let used = h.auth.issue_pairing_link(&[OrchestrationRead], "one-time-token", None, Utc::now(), Duration::minutes(5)).unwrap();
        let upserted = next_event(&mut socket, 2).await;
        assert_eq!((&upserted["type"], &upserted["payload"]["id"], &upserted["revision"]), (&json!("pairingLinkUpserted"), &json!(used.id), &json!(4)));

        // A client that opens a socket is upserted as connected, and removed when revoked.
        let mut watched_socket = h.connect(&watched).await;
        let connected = next_event(&mut socket, 2).await;
        assert_eq!((&connected["type"], &connected["payload"]["sessionId"], &connected["payload"]["connected"], &connected["payload"]["current"]), (&json!("clientUpserted"), &json!(watched_session.session_id), &json!(true), &json!(false)));
        assert!(!connected["payload"]["lastConnectedAt"].is_null());
        serde_json::from_value::<t3_contracts::AuthAccessStreamEvent>(connected).unwrap();
        let (status, revoked) = {
            let response = http().post(format!("http://{}/api/auth/clients/revoke", h.address)).bearer_auth(&admin).header("content-type", "application/json").body(json!({"sessionId":watched_session.session_id}).to_string()).send().await.unwrap();
            (response.status().as_u16(), response.json::<Value>().await.unwrap())
        };
        assert_eq!((status, revoked), (200, json!({"revoked":true})));
        let gone = next_event(&mut socket, 2).await;
        assert_eq!((&gone["type"], &gone["payload"]), (&json!("clientRemoved"), &json!({"sessionId":watched_session.session_id})));
        // The original lets an open socket live until it closes; this server ends the session's requests
        // at once, so the revoked client's next request is refused.
        let refused = rpc(&mut watched_socket, 9, "server.probe", json!({})).await;
        assert_eq!(failure(&refused)["_tag"], "EnvironmentAuthorizationError");
        drop(watched_socket);

        // Closing the admin's own socket is seen by a second subscriber as the client no longer connected.
        let (_, second) = h.auth.issue_session("second", "bearer-access-token", vec![OrchestrationRead, AccessRead], json!({"deviceType":"desktop"}), Utc::now(), Duration::hours(1)).unwrap();
        let mut watcher = h.connect(&second).await;
        send(&mut watcher, json!({"_tag":"Request","id":3,"tag":"subscribeAuthAccess","payload":{},"headers":[]})).await;
        let first = next_event(&mut watcher, 3).await;
        assert_eq!(first["type"], "snapshot");
        let admin_view = first["payload"]["clientSessions"].as_array().unwrap().iter().find(|client| client["sessionId"] == admin_session.session_id).unwrap();
        assert_eq!(admin_view["connected"], true);
        drop(socket);
        loop {
            let event = next_event(&mut watcher, 3).await;
            if event["type"] == "clientUpserted" && event["payload"]["sessionId"] == admin_session.session_id && event["payload"]["connected"] == false {
                break;
            }
        }
        h.finish().await;
    }));
}
