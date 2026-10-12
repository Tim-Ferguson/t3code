//! The `preview.*` methods and `subscribePreviewEvents` over a real authenticated WebSocket. Every reply is also checked against the
//! schema the original encodes it with (the `outbound-check` feature is on for tests).
use crate::test_support::HANG_BOUND;
use crate::{
    auth::AuthService,
    persistence::Store,
    transport::{ApiState, router},
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use t3_contracts::{AuthEnvironmentScope, PreviewEvent, PreviewListResult, PreviewSessionSnapshot, RpcServerMessage};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Fixture {
    address: std::net::SocketAddr,
    state: ApiState,
    operator: String,
    reader: String,
    _directory: tempfile::TempDir,
    _server: tokio::task::JoinHandle<()>,
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [46; 32], "preview_rpc_fixture".into(), "loopback-browser".into()).unwrap();
    let grant = |scopes| auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let operator = grant(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::PreviewOperate]);
    let reader = grant(vec![AuthEnvironmentScope::OrchestrationRead]);
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"preview-fixture","label":"Preview fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{}}),
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
    let app = router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Fixture { address, state, operator, reader, _directory: directory, _server: server }
}

async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}
async fn send(socket: &mut Socket, id: u64, tag: &str, payload: Value) {
    socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}).to_string().into())).await.unwrap();
}
async fn next(socket: &mut Socket) -> Value {
    let frame = tokio::time::timeout(HANG_BOUND, socket.next()).await.expect("a frame").unwrap().unwrap();
    let value: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    let _: RpcServerMessage = serde_json::from_value(value.clone()).unwrap();
    value
}
async fn call(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    send(socket, id, tag, payload).await;
    next(socket).await
}
fn success(reply: &Value) -> &Value {
    assert_eq!(reply["exit"]["_tag"], "Success", "{reply}");
    &reply["exit"]["value"]
}
fn failure(reply: &Value) -> &Value {
    assert_eq!(reply["exit"]["_tag"], "Failure", "{reply}");
    &reply["exit"]["cause"][0]
}

#[tokio::test]
async fn the_preview_methods_follow_the_originals_scopes_results_and_errors() {
    tokio::time::timeout(HANG_BOUND, async {
        let fixture = fixture().await;
        let mut operator = connect(fixture.address, &fixture.operator).await;
        let mut reader = connect(fixture.address, &fixture.reader).await;

        // Everything but `preview.list` needs the operate scope; a payload that is not valid is refused after the scope.
        for (id, tag, payload) in [
            (1, "preview.open", json!({"threadId":"t1"})),
            (2, "preview.navigate", json!({"threadId":"t1","tabId":"x","url":"example.com"})),
            (3, "preview.close", json!({"threadId":"t1"})),
            (4, "preview.clearProfile", json!({"profileId":"work"})),
            (5, "preview.reportStatus", json!({"threadId":"t1","tabId":"x","navStatus":{"_tag":"Idle"},"canGoBack":false,"canGoForward":false})),
        ] {
            let denied = call(&mut reader, id, tag, payload).await;
            assert_eq!(failure(&denied)["error"]["requiredPermission"], "preview:operate", "{tag}");
        }
        let empty = call(&mut reader, 6, "preview.list", json!({"threadId":"t1"})).await;
        let listed: PreviewListResult = serde_json::from_value(success(&empty).clone()).unwrap();
        assert_eq!((listed.sessions.len(), listed.revision.0), (0, 0));

        // A null stands for an absent optional member, as the original's decoding reads it.
        let opened = call(&mut operator, 1, "preview.open", json!({"threadId":"t1","url":null,"viewport":null,"runtime":"desktop"})).await;
        let tab: PreviewSessionSnapshot = serde_json::from_value(success(&opened).clone()).unwrap();
        assert_eq!(success(&opened)["navStatus"], json!({"_tag":"Idle"}));
        assert_eq!(success(&opened)["viewport"], json!({"_tag":"fill"}));
        let id = tab.tab_id.as_str();

        let navigated = call(&mut operator, 2, "preview.navigate", json!({"threadId":"t1","tabId":id,"url":" localhost:5173 ","resolvedTitle":"App"})).await;
        assert_eq!(success(&navigated)["navStatus"], json!({"_tag":"Success","url":"http://localhost:5173/","title":"App"}));
        let reported = call(&mut operator, 3, "preview.reportStatus", json!({"threadId":"t1","tabId":id,"navStatus":{"_tag":"Loading","url":"http://localhost:5173/x","title":""},"canGoBack":true,"canGoForward":false})).await;
        assert_eq!(success(&reported), &Value::Null);
        let resized = call(&mut operator, 4, "preview.resize", json!({"threadId":"t1","tabId":id,"viewport":{"_tag":"preset","width":390,"height":844,"presetId":"iphone-12-pro"}})).await;
        assert_eq!(success(&resized)["viewport"]["presetId"], "iphone-12-pro");
        let adjusted = call(&mut operator, 5, "preview.adjust", json!({"threadId":"t1","tabId":id,"colorScheme":"dark","zoomFactor":1.25,"hardReload":true})).await;
        assert_eq!((success(&adjusted)["colorScheme"].as_str(), success(&adjusted)["zoomFactor"].as_f64()), (Some("dark"), Some(1.25)));
        assert_eq!(success(&call(&mut operator, 6, "preview.refresh", json!({"threadId":"t1","tabId":id})).await), &Value::Null);

        let listed = call(&mut reader, 7, "preview.list", json!({"threadId":"t1"})).await;
        let listed: PreviewListResult = serde_json::from_value(success(&listed).clone()).unwrap();
        assert_eq!(listed.sessions.len(), 1);
        assert_eq!(listed.sessions[0].can_go_back, true);
        assert_eq!(listed.revision.0, 5);

        // The typed failures reach the client as the original's errors.
        let missing = call(&mut operator, 8, "preview.navigate", json!({"threadId":"t1","tabId":"gone","url":"example.com"})).await;
        assert_eq!(failure(&missing)["error"], json!({"_tag":"PreviewSessionLookupError","threadId":"t1","tabId":"gone"}));
        let invalid = call(&mut operator, 9, "preview.navigate", json!({"threadId":"t1","tabId":id,"url":"ftp://example.com"})).await;
        let error = &failure(&invalid)["error"];
        assert_eq!((error["_tag"].as_str(), error["reason"].as_str(), error["protocol"].as_str()), (Some("PreviewInvalidUrlError"), Some("unsupported-protocol"), Some("ftp:")));
        let bad_url = call(&mut operator, 10, "preview.open", json!({"threadId":"t1","url":"https://user:pw@example.com:bad/"})).await;
        assert_eq!(failure(&bad_url)["error"]["reason"], "parse");

        // A server tab belongs to its browser: a client cannot navigate it or report for it.
        let server_tab = call(&mut operator, 11, "preview.open", json!({"threadId":"t1","runtime":"server"})).await;
        let server_id = success(&server_tab)["tabId"].as_str().unwrap().to_owned();
        for (request, tag, payload) in [
            (12, "preview.navigate", json!({"threadId":"t1","tabId":server_id,"url":"example.com"})),
            (13, "preview.reportStatus", json!({"threadId":"t1","tabId":server_id,"navStatus":{"_tag":"Idle"},"canGoBack":false,"canGoForward":false})),
        ] {
            let refused = call(&mut operator, request, tag, payload).await;
            assert_eq!(failure(&refused)["error"], json!({"_tag":"PreviewControlRequiredError","tabId":server_id}), "{tag}");
        }
        // Sizing and appearance need no control.
        let resized = call(&mut operator, 14, "preview.resize", json!({"threadId":"t1","tabId":server_id,"viewport":{"_tag":"freeform","width":1024,"height":768}})).await;
        assert_eq!(success(&resized)["viewport"]["width"], 1024);

        // Payloads the schema refuses are defects, and change nothing.
        let before = fixture.state.store.extension(crate::preview_manager::PreviewManager::default).list(serde_json::from_value(json!({"threadId":"t1"})).unwrap()).revision;
        for (request, tag, payload) in [
            (15, "preview.open", json!({"threadId":""})),
            (16, "preview.resize", json!({"threadId":"t1","tabId":id,"viewport":{"_tag":"freeform","width":3840,"height":2161}})),
            (17, "preview.adjust", json!({"threadId":"t1","tabId":id,"zoomFactor":1.3})),
            (18, "preview.open", json!({"threadId":"t1","runtime":"cloud"})),
            (19, "preview.navigate", json!({"threadId":"t1","tabId":id,"url":"x".repeat(2049)})),
        ] {
            let refused = call(&mut operator, request, tag, payload).await;
            assert_eq!(failure(&refused)["_tag"], "Die", "{tag}: {refused}");
        }
        let after = fixture.state.store.extension(crate::preview_manager::PreviewManager::default).list(serde_json::from_value(json!({"threadId":"t1"})).unwrap()).revision;
        assert_eq!(before, after);

        // Closing: one tab, then the rest, then nothing to close.
        assert_eq!(success(&call(&mut operator, 20, "preview.close", json!({"threadId":"t1","tabId":server_id})).await), &Value::Null);
        assert_eq!(success(&call(&mut operator, 21, "preview.close", json!({"threadId":"t1"})).await), &Value::Null);
        assert_eq!(success(&call(&mut operator, 22, "preview.close", json!({"threadId":"t1"})).await), &Value::Null);
        let listed = call(&mut reader, 23, "preview.list", json!({"threadId":"t1"})).await;
        assert_eq!(success(&listed)["sessions"], json!([]));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn subscribers_follow_every_change_in_order_and_a_late_one_sees_only_what_follows() {
    tokio::time::timeout(HANG_BOUND, async {
        let fixture = fixture().await;
        let manager = fixture.state.store.extension(crate::preview_manager::PreviewManager::default);
        let mut subscriptions = manager.subscriptions();
        let mut operator = connect(fixture.address, &fixture.operator).await;
        let mut watcher = connect(fixture.address, &fixture.reader).await;
        // The stream needs the read scope only, as `preview.list` does.
        send(&mut watcher, 1, "subscribePreviewEvents", json!({})).await;
        subscriptions.wait_for(|count| *count == 1).await.unwrap();

        let opened = call(&mut operator, 1, "preview.open", json!({"threadId":"t1","url":"example.com"})).await;
        let tab = success(&opened)["tabId"].as_str().unwrap().to_owned();
        call(&mut operator, 2, "preview.navigate", json!({"threadId":"t1","tabId":tab,"url":"example.com/a"})).await;
        call(&mut operator, 3, "preview.close", json!({"threadId":"t1"})).await;

        let mut seen = Vec::new();
        for _ in 0..3 {
            let chunk = next(&mut watcher).await;
            assert_eq!(chunk["_tag"], "Chunk");
            let event: PreviewEvent = serde_json::from_value(chunk["values"][0].clone()).unwrap();
            seen.push((chunk["values"][0]["type"].as_str().unwrap().to_owned(), chunk["values"][0]["revision"].as_u64().unwrap()));
            drop(event);
            watcher.send(Message::Text(json!({"_tag":"Ack","requestId":1}).to_string().into())).await.unwrap();
        }
        assert_eq!(seen, [("opened".to_owned(), 1), ("navigated".to_owned(), 2), ("closed".to_owned(), 3)]);

        // A subscriber that joins now is told nothing of the past: the next thing it hears is the next change.
        let mut late = connect(fixture.address, &fixture.reader).await;
        send(&mut late, 1, "subscribePreviewEvents", json!({})).await;
        subscriptions.wait_for(|count| *count == 2).await.unwrap();
        call(&mut operator, 4, "preview.open", json!({"threadId":"t2"})).await;
        let chunk = next(&mut late).await;
        assert_eq!((chunk["values"][0]["type"].as_str(), chunk["values"][0]["revision"].as_u64()), (Some("opened"), Some(4)));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn clearing_a_profile_removes_its_storage_and_nothing_outside_the_profiles_directory() {
    tokio::time::timeout(HANG_BOUND, async {
        let fixture = fixture().await;
        let state_dir = fixture.state.store.state_dir().unwrap().to_path_buf();
        let profiles = crate::server_browser_profiles::profiles_dir(&state_dir);
        let work = crate::server_browser_profiles::profile_directory(&profiles, "work");
        std::fs::create_dir_all(work.join("Default")).unwrap();
        std::fs::write(work.join("Default").join("Cookies"), "x").unwrap();
        let other = crate::server_browser_profiles::profile_directory(&profiles, "personal");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(state_dir.join("keep"), "x").unwrap();

        let mut operator = connect(fixture.address, &fixture.operator).await;
        assert_eq!(success(&call(&mut operator, 1, "preview.clearProfile", json!({"profileId":"work"})).await), &Value::Null);
        assert!(!work.exists() && other.exists());
        // A profile id that spells a way out of the directory names a directory inside it that is not there.
        for (id, profile) in [(2, ".."), (3, "../.."), (4, "../keep"), (5, "incognito")] {
            assert_eq!(success(&call(&mut operator, id, "preview.clearProfile", json!({"profileId":profile})).await), &Value::Null);
        }
        assert!(state_dir.join("keep").exists() && profiles.exists() && other.exists());
        // An id with a control character is not a profile id.
        let refused = call(&mut operator, 6, "preview.clearProfile", json!({"profileId":"a\u{0}b"})).await;
        assert_eq!(failure(&refused)["_tag"], "Die");
    })
    .await
    .unwrap();
}
