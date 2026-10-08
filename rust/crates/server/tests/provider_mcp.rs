#![cfg(unix)]
//! Real provider processes consume issued credentials against the HTTP service.
use chrono::Utc;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use t3_server::{
    execution::ExecutionService, launch::ThreadLaunchService, mcp_http::McpHttpService,
    mcp_sessions::McpSessionRegistry, persistence::Store, project::ProjectService,
    provider_mcp::ProviderMcpSessions, provider_registry::ProviderRegistry,
};

async fn signal(socket: &tokio::net::UnixDatagram, method: &str) -> Value {
    let mut bytes = [0u8; 4096];
    loop {
        let size = socket.recv(&mut bytes).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes[..size]).unwrap();
        if value["method"] == method {
            return value;
        }
    }
}
async fn settled(
    store: &Store,
    events: &mut tokio::sync::broadcast::Receiver<Vec<t3_server::persistence::StoredEvent>>,
    thread: &str,
    ordinal: usize,
) -> Value {
    loop {
        let view = store.projection("thread", thread).unwrap().unwrap();
        if view["runs"].as_array().unwrap().len() >= ordinal {
            let run = &view["runs"][ordinal - 1];
            if matches!(
                run["status"].as_str(),
                Some("completed" | "failed" | "interrupted")
            ) {
                return run.clone();
            }
        }
        events.recv().await.unwrap();
    }
}
async fn exercise(driver: &str, scenario: &str) {
    let root = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(root.path()).unwrap();
    let socket_path = cwd.join("mcp.sock");
    let socket = tokio::net::UnixDatagram::bind(&socket_path).unwrap();
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/provider-mcp.py");
    let codex_binary = cwd.join("codex-fixture");
    if driver == "codex" {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&codex_binary,format!("#!/bin/sh\n[ \"$1\" = app-server ] || exit 2\nexec python3 '{}' codex '{scenario}'\n",fixture.display())).unwrap();
        std::fs::set_permissions(&codex_binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let instance = if driver == "acp" { "agent" } else { "codex" };
    let config = if driver == "acp" {
        json!({"driver":"acpRegistry","config":{"source":"local","commandPath":"python3","commandArgs":[fixture,driver,scenario]},"environment":[{"name":"MCP_SIGNAL","value":socket_path}]})
    } else {
        json!({"driver":"codex","config":{"binaryPath":codex_binary},"environment":[{"name":"MCP_SIGNAL","value":socket_path}]})
    };
    let settings=serde_json::from_value(json!({"enableAgentBrowserAccess":false,"enableAgentDeviceAccess":false,"providerInstances":{"codex":{"driver":"codex","enabled":false},instance:config}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
    assert_eq!(
        providers
            .snapshots()
            .iter()
            .find(|row| row["instanceId"] == instance)
            .unwrap()["status"],
        "ready",
        "{driver}/{scenario} discovery: {}",
        providers
            .snapshots()
            .iter()
            .find(|row| row["instanceId"] == instance)
            .unwrap()
    );
    let store = Store::open(cwd.join("state.db")).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":cwd}),Utc::now()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let registry = McpSessionRegistry::new(
        "environment".parse().unwrap(),
        Some(address),
        Arc::new(|| Utc::now().timestamp_millis()),
        10000,
    );
    let sessions = ProviderMcpSessions::new(
        registry.clone(),
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_t3-server")),
    );
    providers.set_mcp_sessions(sessions.clone());
    let http = McpHttpService::new(registry.clone(), None);
    let router = http.router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let thread=ThreadLaunchService::with_providers(store.clone(),providers.clone()).launch(json!({"commandId":"launch","projectId":"project","title":"MCP fixture","modelSelection":{"instanceId":instance,"model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}),Utc::now()).unwrap()["threadId"].as_str().unwrap().to_owned();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let mut events = store.subscribe();
    let dispatch = |id: &str| {
        execution.dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":id,"text":"Use MCP","attachments":[],"dispatchMode":{"type":"start_immediately"}}),Utc::now()).unwrap()
    };
    dispatch("first");
    let configured = signal(&socket, "configured").await;
    let pid = configured["pid"].as_i64().unwrap() as i32;
    let authorization = configured["authorization"].as_str().unwrap().to_owned();
    if scenario == "normal" {
        let first = signal(&socket, "prompt").await;
        assert_eq!(
            settled(&store, &mut events, &thread, 1).await["status"],
            "completed"
        );
        let original = registry
            .resolve(authorization.strip_prefix("Bearer ").unwrap())
            .unwrap()
            .request_namespace;
        // Reconfiguration replaces snapshots, while the admitted process and its
        // original MCP client remain alive between independently persisted turns.
        providers.reconfigure(&settings, &cwd).await.unwrap();
        dispatch("second");
        let second = signal(&socket, "prompt").await;
        assert_eq!(
            settled(&store, &mut events, &thread, 2).await["status"],
            "completed"
        );
        assert_eq!(first["pid"], second["pid"]);
        assert_eq!(first["hash"], second["hash"]);
        assert_eq!(
            registry
                .resolve(authorization.strip_prefix("Bearer ").unwrap())
                .unwrap()
                .request_namespace,
            original
        );
    } else if scenario == "fail-startup" {
        assert_eq!(
            settled(&store, &mut events, &thread, 1).await["status"],
            "failed"
        );
    } else {
        let view = store.projection("thread", &thread).unwrap().unwrap();
        let run = &view["runs"][0];
        execution.dispatch(&json!({"type":"run.interrupt","commandId":"interrupt","threadId":thread,"runId":run["id"]}),Utc::now()).unwrap();
        assert_eq!(
            settled(&store, &mut events, &thread, 1).await["status"],
            "interrupted"
        );
    }
    if scenario == "normal" {
        execution.shutdown().await;
    } else {
        // Failed/interrupted startup itself closes the actor. Observe its
        // existing completion receipt without triggering a second stop path.
        execution.wait_provider_cleanup(&[instance.into()]).await;
    }
    assert!(
        registry
            .resolve(authorization.strip_prefix("Bearer ").unwrap())
            .is_none()
    );
    let response = reqwest::Client::new()
        .post(format!("http://{address}/mcp"))
        .header("Authorization", authorization)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    execution.shutdown().await;
    http.shutdown().await;
    server.abort();
    let _ = server.await;
}
#[tokio::test]
async fn real_acp_and_codex_sessions_reuse_authenticated_mcp_across_turns_and_revoke_after_reap() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for driver in ["acp", "codex"] {
            exercise(driver, "normal").await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn failed_and_interrupted_provider_startup_revoke_mcp_after_owned_child_cleanup() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for driver in ["acp", "codex"] {
            for scenario in ["fail-startup", "hold-startup"] {
                exercise(driver, scenario).await;
            }
        }
    })
    .await
    .unwrap();
}
