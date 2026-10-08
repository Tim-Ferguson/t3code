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
        eprintln!(
            "provider fixture signal: {} (waiting for {method})",
            value["method"]
        );
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
    exercise_with_device(driver, scenario, None).await;
}
async fn exercise_with_device(driver: &str, scenario: &str, device: Option<bool>) {
    eprintln!("provider fixture {driver}/{scenario}: discovery");
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
    let instance = if driver.starts_with("acp") {
        "agent"
    } else {
        "codex"
    };
    let config = if driver.starts_with("acp") {
        json!({"driver":"acpRegistry","config":{"source":"local","commandPath":"python3","commandArgs":[fixture,driver,scenario]},"environment":[{"name":"MCP_SIGNAL","value":socket_path}]})
    } else {
        json!({"driver":"codex","config":{"binaryPath":codex_binary},"environment":[{"name":"MCP_SIGNAL","value":socket_path}]})
    };
    let mut config = config;
    if let Some(device) = device {
        config["environment"].as_array_mut().unwrap().extend([
            json!({"name":"MCP_EXPECT_DEVICE","value":device.to_string()}),
            json!({"name":"MCP_DEVICE_DIR","value":cwd.join("scoped-device/bin")}),
            json!({"name":"MCP_PROVIDER_KEY","value":"preserved"}),
        ]);
    }
    let settings=serde_json::from_value(json!({"enableAgentBrowserAccess":false,"enableAgentDeviceAccess":device.unwrap_or(false),"providerInstances":{"codex":{"driver":"codex","enabled":false},instance:config}})).unwrap();
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
    let environment = device.map(|_| {
        indexmap::IndexMap::from([
            (
                "PATH".into(),
                cwd.join("scoped-device/bin").to_string_lossy().into_owned(),
            ),
            ("PATH_SEPARATOR".into(), ":".into()),
            ("MCP_SCOPED_DEVICE_MARKER".into(), "granted".into()),
        ])
    });
    let sessions = ProviderMcpSessions::new_with_device_environment(
        registry.clone(),
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_t3-server")),
        environment,
    );
    providers.set_mcp_sessions(sessions.clone());
    let http = McpHttpService::new(registry.clone(), None);
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = requests.clone();
    let (http_held, mut http_admitted) = tokio::sync::mpsc::unbounded_channel();
    let release_http = Arc::new(tokio::sync::Notify::new());
    let release = release_http.clone();
    let hold_http = scenario == "hold-http";
    let hold_once = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let router = http.router().layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let captured = captured.clone();
            let http_held = http_held.clone();
            let release = release.clone();
            let hold_once = hold_once.clone();
            async move {
                captured
                    .lock()
                    .unwrap()
                    .push((request.method().clone(), request.headers().clone()));
                let request = if hold_http && request.method() == axum::http::Method::POST {
                    let (parts, body) = request.into_parts();
                    let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap();
                    let value: Value = serde_json::from_slice(&bytes).unwrap();
                    if value["method"] == "tools/list"
                        && hold_once.swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        http_held.send(()).unwrap();
                        release.notified().await;
                    }
                    axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes))
                } else {
                    request
                };
                next.run(request).await
            }
        },
    ));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let thread=ThreadLaunchService::with_providers(store.clone(),providers.clone()).launch(json!({"commandId":"launch","projectId":"project","title":"MCP fixture","modelSelection":{"instanceId":instance,"model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}),Utc::now()).unwrap()["threadId"].as_str().unwrap().to_owned();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let mut events = store.subscribe();
    let dispatch = |id: &str| {
        execution.dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":id,"text":"Use MCP","attachments":[],"dispatchMode":{"type":"start_immediately"}}),Utc::now()).unwrap()
    };
    dispatch("first");
    eprintln!("provider fixture {driver}/{scenario}: first startup");
    let configured = signal(&socket, "configured").await;
    let pid = configured["pid"].as_i64().unwrap() as i32;
    let authorization = configured["authorization"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            requests
                .lock()
                .unwrap()
                .iter()
                .find_map(|(_, headers)| {
                    headers
                        .get("authorization")
                        .map(|value| value.to_str().unwrap().to_owned())
                })
                .unwrap()
        });
    let mut owned_pids = vec![pid];
    let mut issued_authorizations = vec![authorization.clone()];
    eprintln!("provider fixture {driver}/{scenario}: configured");
    if matches!(
        scenario,
        "normal"
            | "prompt-retry"
            | "workspace-same"
            | "workspace-rotate"
            | "workspace-busy"
            | "workspace-interrupt-error"
    ) {
        let first = signal(&socket, "prompt").await;
        assert_eq!(
            settled(&store, &mut events, &thread, 1).await["status"],
            if scenario == "prompt-retry" {
                "failed"
            } else {
                "completed"
            }
        );
        let original = registry
            .resolve(authorization.strip_prefix("Bearer ").unwrap())
            .unwrap()
            .request_namespace;
        // Reconfiguration replaces snapshots, while the admitted process and its
        // original MCP client remain alive between independently persisted turns.
        providers.reconfigure(&settings, &cwd).await.unwrap();
        eprintln!("provider fixture {driver}/{scenario}: second turn");
        dispatch("second");
        let second = signal(&socket, "prompt").await;
        if !matches!(scenario, "workspace-busy" | "workspace-interrupt-error") {
            assert_eq!(
                settled(&store, &mut events, &thread, 2).await["status"],
                "completed"
            );
        }
        if scenario == "prompt-retry" {
            dispatch("third");
            let third = signal(&socket, "prompt").await;
            assert_eq!(
                settled(&store, &mut events, &thread, 3).await["status"],
                "completed"
            );
            assert_eq!(second["pid"], third["pid"]);
        }
        assert_eq!(first["pid"], second["pid"]);
        assert_eq!(first["hash"], second["hash"]);
        assert_eq!(
            registry
                .resolve(authorization.strip_prefix("Bearer ").unwrap())
                .unwrap()
                .request_namespace,
            original
        );
        if scenario.starts_with("workspace-") {
            let workspace = cwd.join("reattached-workspace");
            std::fs::create_dir(&workspace).unwrap();
            let mut next_settings = settings.clone();
            if scenario == "workspace-rotate" {
                next_settings.enable_agent_browser_access = true;
                providers.reconfigure(&next_settings, &cwd).await.unwrap();
                // Source prepareMcpSession is skipped while already attached:
                // a settings edit must not revoke the live provider's token.
                dispatch("still-attached");
                eprintln!("provider fixture {driver}/{scenario}: settings changed while attached");
                let unchanged = signal(&socket, "prompt").await;
                assert_eq!(
                    settled(&store, &mut events, &thread, 3).await["status"],
                    "completed"
                );
                assert_eq!(unchanged["hash"], first["hash"]);
                assert!(
                    !registry
                        .resolve(authorization.strip_prefix("Bearer ").unwrap())
                        .unwrap()
                        .capabilities
                        .contains(&t3_server::mcp_invocation::McpCapability::Preview)
                );
            }
            let receipt = t3_server::thread::ThreadService::new(store.clone()).dispatch(&json!({"type":"thread.metadata.update","commandId":"workspace-change","threadId":thread,"worktreePath":workspace}), Utc::now()).unwrap();
            assert_eq!(receipt.status, "accepted");
            eprintln!("provider fixture {driver}/{scenario}: detach admitted");
            assert!(
                store.projection("thread", &thread).unwrap().unwrap()["providerSessions"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            if driver == "codex" {
                let unloaded = signal(&socket, "unloaded").await;
                assert_eq!(unloaded["pid"], first["pid"]);
                assert!(
                    registry
                        .resolve(authorization.strip_prefix("Bearer ").unwrap())
                        .is_some(),
                    "plain shared detach preserves its live process credential"
                );
            } else {
                execution.wait_provider_cleanup(&[instance.into()]).await;
                eprintln!("provider fixture {driver}/{scenario}: detached process reaped");
                assert!(
                    registry
                        .resolve(authorization.strip_prefix("Bearer ").unwrap())
                        .is_none(),
                    "exclusive detach reaps before revoking its credential"
                );
                assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
            }
            if matches!(scenario, "workspace-busy" | "workspace-interrupt-error") {
                assert_eq!(
                    settled(&store, &mut events, &thread, 2).await["status"],
                    "interrupted"
                );
            }
            dispatch("reattached");
            eprintln!("provider fixture {driver}/{scenario}: reattach admitted");
            let configured = signal(&socket, "configured").await;
            assert_eq!(configured["cwd"], json!(workspace));
            assert_eq!(
                configured["nativeMethod"],
                if driver == "codex" {
                    "thread/resume"
                } else {
                    "session/resume"
                }
            );
            let reattached = signal(&socket, "prompt").await;
            owned_pids.push(reattached["pid"].as_i64().unwrap() as i32);
            issued_authorizations.push(reattached["authorization"].as_str().unwrap().to_owned());
            let ordinal = if scenario == "workspace-rotate" { 4 } else { 3 };
            assert_eq!(
                settled(&store, &mut events, &thread, ordinal).await["status"],
                "completed"
            );
            let view = store.projection("thread", &thread).unwrap().unwrap();
            assert!(!view.to_string().contains("STALE DETACHED OUTPUT"));
            assert!(!view.to_string().contains("Stale detached turn"));
            if driver == "codex" {
                assert_eq!(reattached["pid"], first["pid"]);
                assert_eq!(
                    reattached["hash"] == first["hash"],
                    scenario != "workspace-rotate"
                );
            } else {
                assert_ne!(reattached["pid"], first["pid"]);
            }
            let token = reattached["authorization"]
                .as_str()
                .unwrap()
                .strip_prefix("Bearer ")
                .unwrap();
            assert_eq!(
                registry
                    .resolve(token)
                    .unwrap()
                    .capabilities
                    .contains(&t3_server::mcp_invocation::McpCapability::Preview),
                scenario == "workspace-rotate"
            );
            if scenario == "workspace-rotate" {
                assert!(
                    registry
                        .resolve(authorization.strip_prefix("Bearer ").unwrap())
                        .is_none()
                );
            }
        }
    } else if scenario == "fail-startup" {
        assert_eq!(
            settled(&store, &mut events, &thread, 1).await["status"],
            "failed"
        );
    } else {
        if scenario == "hold-http" {
            http_admitted.recv().await.unwrap();
        }
        let view = store.projection("thread", &thread).unwrap().unwrap();
        let run = &view["runs"][0];
        execution.dispatch(&json!({"type":"run.interrupt","commandId":"interrupt","threadId":thread,"runId":run["id"]}),Utc::now()).unwrap();
        assert_eq!(
            settled(&store, &mut events, &thread, 1).await["status"],
            "interrupted"
        );
    }
    if matches!(
        scenario,
        "normal"
            | "prompt-retry"
            | "workspace-same"
            | "workspace-rotate"
            | "workspace-busy"
            | "workspace-interrupt-error"
    ) {
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
    for authorization in &issued_authorizations {
        assert!(
            registry
                .resolve(authorization.strip_prefix("Bearer ").unwrap())
                .is_none()
        );
    }
    let response = reqwest::Client::new()
        .post(format!("http://{address}/mcp"))
        .header("Authorization", authorization)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    if driver == "acp-negotiated" {
        let requests = requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .filter(|(method, _)| *method == axum::http::Method::POST)
                .count()
                >= 3
        );
        assert!(
            requests
                .iter()
                .any(|(method, headers)| *method == axum::http::Method::DELETE
                    && headers.get("mcp-session-id").is_some())
        );
    }
    for pid in owned_pids {
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    release_http.notify_one();
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

#[tokio::test]
async fn negotiated_acp_callbacks_call_authenticated_mcp_and_cleanup_failed_setup() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for scenario in ["normal", "fail-startup", "hold-startup", "hold-http"] {
            exercise("acp-negotiated", scenario).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn optional_device_environment_and_capability_gated_context_reach_actual_provider_processes()
{
    tokio::time::timeout(Duration::from_secs(20), async {
        for driver in ["acp", "codex"] {
            for granted in [false, true] {
                exercise_with_device(driver, "normal", Some(granted)).await;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn acp_failed_prompt_retries_instructions_then_success_reuses_instruction_state() {
    tokio::time::timeout(
        Duration::from_secs(20),
        exercise("acp-negotiated", "prompt-retry"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn actual_workspace_detach_reattach_preserves_shared_credentials_and_rotates_only_on_reattach()
 {
    tokio::time::timeout(Duration::from_secs(30), async {
        for driver in ["codex", "acp"] {
            for scenario in ["workspace-same", "workspace-rotate"] {
                exercise(driver, scenario).await;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn busy_workspace_detach_interrupts_and_reaps_or_unloads_before_reattach() {
    tokio::time::timeout(Duration::from_secs(30), async {
        for driver in ["codex", "acp"] {
            exercise(driver, "workspace-busy").await;
        }
        exercise("codex", "workspace-interrupt-error").await;
    })
    .await
    .unwrap();
}
