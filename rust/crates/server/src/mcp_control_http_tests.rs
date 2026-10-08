use super::*;
use crate::{execution, persistence::write_projection, provider_registry::ProviderRegistry};
use t3_contracts::{
    OrchestratorMcpThreadInterruptResult, OrchestratorMcpThreadListResult,
    OrchestratorMcpThreadReadResult, OrchestratorMcpThreadWaitResult, ServerSettings,
};
#[tokio::test]
async fn authenticated_cooperative_tools_use_persisted_threads_and_cancel_owned_waits() {
    tokio::time::timeout(std::time::Duration::from_secs(20), cooperative_fixture())
        .await
        .expect("cooperative HTTP fixture timed out");
}
async fn cooperative_fixture() {
    let (store, scope) = crate::mcp_invocation::tests::fixture();
    let root = tempfile::tempdir().unwrap();
    let mut settings = serde_json::to_value(ServerSettings::default()).unwrap();
    for provider in settings["providers"].as_object_mut().unwrap().values_mut() {
        provider["enabled"] = json!(false);
    }
    let settings: ServerSettings = serde_json::from_value(settings).unwrap();
    let providers = ProviderRegistry::discover(&settings, root.path())
        .await
        .unwrap();
    let execution = execution::ExecutionService::try_start(store.clone(), providers).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let registry = McpSessionRegistry::new(
        "environment-1".parse().unwrap(),
        Some(address),
        Arc::new(|| 1),
        1000,
    );
    let credential = registry
        .issue(crate::mcp_sessions::CredentialRequest {
            thread_id: "thread:1".parse().unwrap(),
            provider_instance_id: "codex".parse().unwrap(),
            browser_tools_available: Some(false),
            capabilities: None,
        })
        .unwrap();
    let tools = McpControlTools {
        store: store.clone(),
        execution: Some(execution.clone()),
        clock: Arc::new(|| "2026-01-01T00:00:00Z".parse().unwrap()),
    };
    let service = McpHttpService::new(registry.clone(), None).with_controls(tools);
    let router = service.router();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/mcp");
    let request = |body: Value| {
        client
            .post(&url)
            .header("authorization", &credential.authorization_header)
            .header("accept", "application/json,text/event-stream")
            .json(&body)
    };
    let initialize = request(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"native-controls","version":"1"}}})).send().await.unwrap();
    let session = initialize.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let initialized: Value = initialize.json().await.unwrap();
    assert_eq!(
        initialized["result"]["capabilities"]["tools"]["listChanged"],
        true
    );
    let call = |name: &str, args: Value| {
        request(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":args}})).header("mcp-session-id",&session).header("mcp-protocol-version",VERSION)
    };
    let catalog_response =
        request(json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}))
            .header("mcp-session-id", &session)
            .header("mcp-protocol-version", VERSION)
            .send()
            .await
            .unwrap();
    let catalog_status = catalog_response.status();
    let catalog_body = catalog_response.text().await.unwrap();
    assert_eq!(
        catalog_status,
        reqwest::StatusCode::OK,
        "catalog response: {catalog_body}"
    );
    let catalog: Value = serde_json::from_str(&catalog_body).expect("catalog returned JSON");
    assert_eq!(
        catalog["result"]["tools"],
        json!(McpControlTools::catalog())
    );
    assert_eq!(catalog["result"]["tools"].as_array().unwrap().len(), 4);
    for name in ["t3_thread_send", "t3_thread_update"] {
        let result: Value = call(name, json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            result["error"]["message"],
            format!("Tool '{name}' not found")
        );
    }
    let listed: Value = call(
        "t3_thread_list",
        json!({"threadId":"spoofed","providerInstanceId":"spoofed"}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let listed: OrchestratorMcpThreadListResult =
        serde_json::from_value(listed["result"]["structuredContent"].clone()).unwrap();
    assert_eq!(listed.project_id.as_str(), "project:1");
    assert_eq!(listed.current_thread_id.unwrap().as_str(), "thread:1");
    assert_eq!(listed.threads.len(), 1);
    // Reading/waiting does not require a currently executing calling run.
    let read: Value = call("t3_thread_read", json!({"threadId":"thread:1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    serde_json::from_value::<OrchestratorMcpThreadReadResult>(
        read["result"]["structuredContent"].clone(),
    )
    .unwrap();
    let idle: Value = call("t3_thread_wait", json!({"threadId":"thread:1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let idle: OrchestratorMcpThreadWaitResult =
        serde_json::from_value(idle["result"]["structuredContent"].clone()).unwrap();
    assert_eq!(serde_json::to_value(idle).unwrap()["status"], "idle");
    let denied: Value = call("t3_thread_interrupt", json!({"threadId":"thread:1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let denied: Value =
        serde_json::from_str(denied["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(denied["code"], "parent_not_active");
    let malformed: Value = call(
        "t3_thread_wait",
        json!({"threadId":"thread:1","runId":null}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(
        malformed["error"]["message"],
        "Invalid parameters for tool 't3_thread_wait': Expected string | undefined\n  at [\"runId\"]"
    );
    let missing: Value = call("t3_thread_read", json!({"threadId":"missing"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(missing["result"].get("structuredContent").is_none());
    let failure: Value =
        serde_json::from_str(missing["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        failure,
        json!({"_tag":"OrchestratorMcpFailure","code":"thread_not_found","message":"Thread missing was not found."})
    );
    // A prepared turn uses the real command planner/transaction and has no external provider effect.
    let command = json!({"type":"message.dispatch","commandId":"prepare","threadId":"thread:1","messageId":"message:prepared","text":"Prepared turn","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"defer_start","workspaceStrategy":{"type":"root"}}});
    store
        .dispatch(
            "prepare",
            "thread",
            "thread:1",
            "message.dispatch",
            chrono::Utc::now(),
            |tx| {
                execution::plan_message(
                    &command,
                    &crate::persistence::read_projection(tx, "thread", "thread:1")?.unwrap(),
                    chrono::Utc::now(),
                )
            },
            crate::thread::reduce,
        )
        .unwrap();
    let run_id = store.projection("thread", "thread:1").unwrap().unwrap()["runs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(scope.acts_as_caller(&store).is_ok());
    let interrupted: Value = call(
        "t3_thread_interrupt",
        json!({"threadId":"thread:1","runId":run_id,"clientRequestId":"stop-once"}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let interrupted: OrchestratorMcpThreadInterruptResult =
        serde_json::from_value(interrupted["result"]["structuredContent"].clone()).unwrap();
    assert_eq!(
        serde_json::to_value(interrupted).unwrap()["status"],
        "interrupt_requested"
    );
    assert_eq!(
        store.projection("thread", "thread:1").unwrap().unwrap()["runs"][0]["status"],
        "interrupted"
    );
    let waited: Value = call(
        "t3_thread_wait",
        json!({"threadId":"thread:1","runId":run_id}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(
        waited["result"]["structuredContent"]["status"],
        "interrupted"
    );
    assert_eq!(waited["result"]["structuredContent"]["timedOut"], false);
    // Hold an admitted wait. Shutdown must cancel it and await request release.
    let mut projection = store.projection("thread", "thread:1").unwrap().unwrap();
    projection["runs"][0]["status"] = json!("running");
    store
        .transaction(|tx| write_projection(tx, "thread", "thread:1", &projection))
        .unwrap();
    let subscribers = store.subscriber_count();
    let response = call(
        "t3_thread_wait",
        json!({"threadId":"thread:1","runId":run_id,"timeoutMs":600000}),
    )
    .send();
    tokio::pin!(response);
    let admitted = async {
        loop {
            if service.state.lock().unwrap().active == 1
                && store.subscriber_count() == subscribers + 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    };
    tokio::select! { result=&mut response=>panic!("wait returned before shutdown: {result:?}"), _=admitted=>{} }
    service.shutdown().await;
    assert_eq!(response.await.unwrap().status(), 503);
    assert_eq!(service.state.lock().unwrap().active, 0);
    assert_eq!(store.subscriber_count(), subscribers);
    execution.shutdown().await;
    let _ = stop.send(());
    while let Some(result) = server.join_next().await {
        result.unwrap();
    }
}
