//! In-process integration through the real native HTTP/Effect WebSocket server.
//! No browser, provider account, live T3 state, or persistent server is used.
use super::*;
use futures_util::Stream;
use std::{cell::RefCell, rc::Rc, time::Duration};
use t3_server::{
    auth::AuthService,
    persistence::Store as EventStore,
    transport::{ApiState, router},
};

async fn drive_future<F: std::future::Future>(
    dom: &mut VirtualDom,
    label: &str,
    future: F,
) -> F::Output {
    let mut future = std::pin::pin!(future);
    tokio::time::timeout(Duration::from_secs(8),async {
        loop {
            tokio::select! {
                result=&mut future=>return result,
                _=dom.wait_for_work()=>dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations),
            }
        }
    }).await.unwrap_or_else(|_|panic!("UI/native terminal milestone {label} timed out"))
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn held_terminal_consumer_defers_ack_without_blocking_unary_metadata_or_other_sessions() {
    terminal_stream_flow(16000, false).await;
}
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn held_terminal_continuity_loss_reattaches_snapshot_and_resets_renderer_cursor() {
    terminal_stream_flow(200000, true).await;
}
#[cfg(unix)]
async fn terminal_stream_flow(burst_bytes: usize, recover: bool) {
    use std::os::unix::fs::PermissionsExt;
    use t3_client::terminal_session::BufferState;
    use t3_server::terminal_manager::{TerminalManager, TerminalManagerOptions};
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("isolated-terminal.py");
    let fixture = r#"#!/usr/bin/env python3
import os,sys,tty
tty.setraw(0)
os.write(1,b'READY\r\n')
for line in sys.stdin:
 line=line.rstrip('\n')
 if line=='BURST': os.write(1,b'x'*BURST_BYTES+b'BURST_END\r\n')
 elif line.startswith('PART '):
  _,index,count=line.split(' ')
  tail=b'BURST_END\r\n' if index=='69' else b''
  os.write(1,b'x'*int(count)+('PART_END_'+index+'\r\n').encode()+tail)
 else: os.write(1,('ECHO:'+line+'\r\n').encode())
"#;
    std::fs::write(
        &script,
        fixture.replace("BURST_BYTES", &burst_bytes.to_string()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut options = TerminalManagerOptions::host(
        directory.path().join("logs"),
        &t3_contracts::ServerSettings::default(),
    );
    options.shell = Some(script.to_string_lossy().into_owned());
    options.kill_grace = Duration::ZERO;
    let terminals = TerminalManager::new(options).await.unwrap();
    let mut api = api_fixture(&directory, "terminal-native-ui-test");
    api.terminals = Some(terminals.clone());
    let credential = api
        .auth
        .create_pairing_credential(
            &[
                AuthEnvironmentScope::OrchestrationRead,
                AuthEnvironmentScope::TerminalRead,
                AuthEnvironmentScope::TerminalOperate,
            ],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;
    let props = Harness {
        state: Rc::new(RefCell::new(None)),
        transport: TransportHandle::default(),
        address,
        credential,
        initial: UiModel::default(),
    };
    let mut dom = VirtualDom::new_with_props(harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    drive_until(&mut dom, state, "terminal-connect", || {
        state.peek().shell.synchronized
    })
    .await;
    let input = json!({"threadId":"terminal-thread","terminalId":"term-1","cwd":directory.path()});
    let mut held = dom
        .in_scope(ScopeId::APP, || {
            request_stream(&props.transport, state, "terminal.attach", input)
        })
        .unwrap();
    let id = held._guard.id.clone();
    assert!(
        drive_future(
            &mut dom,
            "held-first-snapshot",
            std::pin::Pin::new(&mut held.receiver).peek()
        )
        .await
        .is_some()
    );
    // No next() call means no ACK. Even with a finished actual PTY burst the
    // stream remains at one admitted snapshot, while unrelated RPCs progress.
    let mut witness = terminals
        .observe(
            serde_json::from_value(json!({"threadId":"terminal-thread","terminalId":"term-1"}))
                .unwrap(),
        )
        .await
        .unwrap();
    // Attach's atomic snapshot can precede shell initialization. tty.setraw
    // uses TCSAFLUSH, so prove READY before sending bytes into the owned PTY.
    // This independent witness does not acknowledge the held UI stream.
    drive_future(&mut dom, "PTY-ready-before-input", async {
        let mut output = String::new();
        while let Some(event) = witness.recv().await.unwrap() {
            if let Some(history) = event["snapshot"]["history"].as_str() {
                output.push_str(history)
            }
            if let Some(data) = event["data"].as_str() {
                output.push_str(data)
            }
            if output.contains("READY") {
                return;
            }
        }
        panic!("PTY ended before READY")
    })
    .await;
    let mut distinct_output_events = 0;
    let parts = if recover { 70 } else { 1 };
    for index in 0..parts {
        let (data, marker) = if recover {
            let count = burst_bytes / parts + usize::from(index < burst_bytes % parts);
            (
                format!("PART {index} {count}\n"),
                format!("PART_END_{index}\r\n"),
            )
        } else {
            ("BURST\n".to_owned(), "BURST_END\r\n".to_owned())
        };
        let (write_sender, write) = futures_channel::oneshot::channel();
        let write_transport = props.transport.clone();
        dom.in_scope(ScopeId::APP, || {
            dioxus::dioxus_core::spawn_forever(async move {
                let result = request_value(
                    write_transport,
                    state,
                    "terminal.write",
                    json!({"threadId":"terminal-thread","terminalId":"term-1","data":data}),
                )
                .await;
                let _ = write_sender.send(result);
            });
        });
        drive_future(&mut dom, "PTY-observed-part", async {
            let mut output = String::new();
            while let Some(event) = witness.recv().await.unwrap() {
                if let Some(data) = event["data"].as_str() {
                    output.push_str(data);
                    distinct_output_events += 1;
                }
                if output.contains(&marker) {
                    return;
                }
            }
            panic!("PTY ended before observed part marker")
        })
        .await;
        drive_future(&mut dom, "write-receipt-with-held-PTY", write)
            .await
            .unwrap()
            .unwrap();
    }
    if recover {
        assert!(
            distinct_output_events > 64,
            "seventy distinct observed PTY writes overflow64-event server listener independently of OS read chunking"
        );
    }
    drive_future(
        &mut dom,
        "unrelated-config-with-held-PTY",
        request_value(
            props.transport.clone(),
            state,
            "server.getConfig",
            json!({}),
        ),
    )
    .await
    .unwrap();
    assert!(
        props.transport.borrow().stream_waiters.contains_key(&id),
        "no overflow/failure while held"
    );
    assert_eq!(
        held.receiver.size_hint().0,
        1,
        "one unacknowledged admitted chunk"
    );
    let mut metadata = dom
        .in_scope(ScopeId::APP, || {
            crate::terminal_stream::MetadataStream::subscribe(&props.transport, state)
        })
        .unwrap();
    let summaries = drive_future(&mut dom, "metadata-independent-progress", metadata.next())
        .await
        .unwrap()
        .unwrap();
    assert!(
        summaries
            .iter()
            .any(|summary| summary.terminal_id.as_str() == "term-1")
    );
    let mut other = dom
        .in_scope(ScopeId::APP, || {
            crate::terminal_stream::SessionStream::subscribe(
                &props.transport,
                state,
                serde_json::from_value(
                    json!({"threadId":"other-thread","terminalId":"term-1","cwd":directory.path()}),
                )
                .unwrap(),
            )
        })
        .unwrap();
    drive_future(&mut dom, "second-session-progress", other.next())
        .await
        .unwrap()
        .unwrap();
    let mut buffer = BufferState::next_attach_seed();
    for event in drive_future(&mut dom, "consume-and-ack-held-snapshot", held.next())
        .await
        .unwrap()
        .unwrap()
    {
        buffer.apply(&serde_json::from_value(event).unwrap())
    }
    let continuity_error = drive_future(&mut dom, "deferred-output-released-by-ACK", async {
        loop {
            let events = match held
                .next()
                .await
                .expect("server stream ends with explicit continuity failure")
            {
                Ok(events) => events,
                Err(error) => return Some(error),
            };
            for event in events {
                buffer.apply(&serde_json::from_value(event).unwrap())
            }
            if t3_client::terminal_output::text(&buffer.output).contains("BURST_END") {
                return None;
            }
        }
    })
    .await;
    if recover {
        assert!(
            continuity_error
                .as_deref()
                .is_some_and(|error| error.contains("continuity")),
            "a held200KB burst must report the native bounded stream gap, never silently drop data"
        );
        let old_cursor = t3_client::terminal_output::read(
            &buffer.output,
            t3_client::terminal_output::INITIAL_CURSOR,
        )
        .cursor();
        let mut restored=dom.in_scope(ScopeId::APP,||crate::terminal_stream::SessionStream::subscribe(&props.transport,state,serde_json::from_value(json!({"threadId":"terminal-thread","terminalId":"term-1","cwd":directory.path()})).unwrap())).unwrap();
        let snapshot = drive_future(&mut dom, "continuity-recovery-snapshot", restored.next())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(snapshot.output.generation, buffer.output.generation);
        let update = t3_client::terminal_output::read(&snapshot.output, old_cursor);
        let t3_client::terminal_output::OutputUpdate::Reset { data, .. } = update else {
            panic!("new attach must reset an old renderer cursor")
        };
        assert_eq!(
            data.matches("BURST_END").count(),
            1,
            "retained snapshot replay contains final output exactly once"
        );
        assert!(data.contains("READY"));
        buffer = snapshot;
        drop(restored);
    } else {
        assert!(continuity_error.is_none());
    }
    assert!(buffer.output.retained_bytes <= 512 * 1024);
    let mut abandoned=dom.in_scope(ScopeId::APP,||crate::terminal_stream::SessionStream::subscribe(&props.transport,state,serde_json::from_value(json!({"threadId":"terminal-thread","terminalId":"term-1","cwd":directory.path()})).unwrap())).unwrap();
    assert!(
        drive_future(
            &mut dom,
            "abandoned-queued-snapshot",
            abandoned.wait_ready()
        )
        .await
    );
    let original = state.peek().destination.clone();
    state.destination().set(Some(
        EnvironmentId::new("other-environment-same-terminal-ids").unwrap(),
    ));
    assert!(
        abandoned.next().await.unwrap().is_err(),
        "old destination delivery is rejected before a queued snapshot can alter the new pane"
    );
    assert_eq!(abandoned.buffer().version, 0);
    assert_eq!(abandoned.buffer().output.retained_bytes, 0);
    state.destination().set(original);
    drop(abandoned);
    drop(held);
    assert!(!props.transport.borrow().stream_waiters.contains_key(&id));
    drop(other);
    drop(metadata);
    drop(witness);
    drop(dom);
    terminals.shutdown().await;
    server.abort();
    let _ = server.await;
}

#[derive(Clone)]
struct Harness {
    state: Rc<RefCell<Option<Store<UiModel>>>>,
    transport: TransportHandle,
    address: String,
    credential: String,
    initial: UiModel,
}
fn harness(props: Harness) -> Element {
    let state = use_store(|| props.initial.clone());
    crate::themes::use_themes(state);
    *props.state.borrow_mut() = Some(state);
    let transport = props.transport.clone();
    use_future(move || {
        let transport = transport.clone();
        let address = props.address.clone();
        let credential = props.credential.clone();
        async move {
            pair_and_connect(transport, state, address, credential).await;
        }
    });
    rsx! {crate::Application {state,transport:props.transport}}
}

async fn drive_until(
    dom: &mut VirtualDom,
    state: Store<UiModel>,
    label: &str,
    predicate: impl Fn() -> bool,
) {
    tokio::time::timeout(Duration::from_secs(if label=="socket-reconnect"{15}else{8}),async {
        loop {
            if predicate(){return;}
            dom.wait_for_work().await;
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
    }).await.unwrap_or_else(|_|panic!("UI/native stage {label} timed out: status={:?},error={:?},shell_synced={},thread_synced={}",state.peek().status,state.peek().error,state.peek().shell.synchronized,state.peek().thread.synchronized));
}

fn api_fixture(directory: &tempfile::TempDir, environment_id: &str) -> ApiState {
    let store = EventStore::open(directory.path().join("isolated.sqlite")).unwrap();
    let auth = AuthService::new(
        store.clone(),
        [13; 32],
        "test_only_session".into(),
        "loopback-browser".into(),
    )
    .unwrap();
    let environment = json!({"environmentId":environment_id,"label":"Isolated integration","platform":{"os":"linux","arch":"x64"},"serverVersion":"rust-test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}});
    let config = json!({"environment":environment,"auth":auth.descriptor(),"cwd":directory.path().to_string_lossy(),"keybindingsConfigPath":"/isolated/keybindings.json","keybindings":t3_contracts::default_resolved_keybindings(),"issues":[],"providers":[],"availableEditors":[],"observability":{"logsDirectoryPath":"/isolated/logs","localTracingEnabled":false,"otlpTracesEnabled":false,"otlpMetricsEnabled":false},"settings":t3_contracts::ServerSettings::default()});
    let _: t3_contracts::ServerConfig = serde_json::from_value(config.clone()).unwrap();
    let api = ApiState {
        store: store.clone(),
        auth: auth.clone(),
        environment,
        config: Some(config),
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
    api
}

async fn start_fixture(api: ApiState) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router(api)).await.unwrap();
    });
    (address, server)
}

#[tokio::test(flavor = "current_thread")]
async fn native_pairing_project_thread_stream_and_reconnect_keep_unsent_draft() {
    let directory = tempfile::tempdir().unwrap();
    let api = api_fixture(&directory, "native-ui-test");
    let store = api.store.clone();
    let auth = api.auth.clone();
    let credential = auth
        .create_pairing_credential(
            &[
                AuthEnvironmentScope::OrchestrationRead,
                AuthEnvironmentScope::OrchestrationOperate,
            ],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;
    let props = Harness {
        state: Rc::new(RefCell::new(None)),
        transport: TransportHandle::default(),
        address,
        credential: credential.clone(),
        initial: UiModel::default(),
    };
    let mut dom = VirtualDom::new_with_props(harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    drive_until(&mut dom, state, "pair-shell", || {
        state.peek().shell.synchronized
    })
    .await;
    assert_eq!(state.peek().status, ConnectionStatus::Connected);
    assert!(state.peek().grants.authenticated);
    assert_eq!(
        state.peek().destination.as_ref().unwrap().as_str(),
        "native-ui-test"
    );
    assert!(
        auth.exchange_pairing_bearer(&credential, None, json!({}), chrono::Utc::now())
            .is_err(),
        "one-time credential must be consumed"
    );
    assert!(!props.transport.borrow().bearer_token.is_empty());
    let create = json!({"type":"project.create","commandId":"project-command","projectId":"project","title":"Native project","workspaceRoot":directory.path().to_string_lossy()});
    dom.in_scope(ScopeId::APP, || {
        request(
            &props.transport,
            state,
            "projects.mutate",
            create,
            RequestKind::Unary,
        )
    })
    .unwrap();
    drive_until(&mut dom, state, "project-create", || {
        state
            .peek()
            .shell
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.projects.len() == 1)
    })
    .await;
    assert!(store.projection("project", "project").unwrap().is_some());
    // The lifecycle command transport works before provider-backed launch lands.
    // The user-facing launch/provider/message scenario is added separately.
    dom.in_scope(ScopeId::APP,||command(&props.transport,state,"thread.create",json!({"threadId":"thread","projectId":"project","title":"Native thread","modelSelection":{"instanceId":"codex","model":"test-provider-model"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}))).unwrap();
    drive_until(&mut dom, state, "thread-create", || {
        state
            .peek()
            .shell
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.threads.len() == 1)
    })
    .await;
    dom.in_scope(ScopeId::APP, || {
        select_thread(&props.transport, state, "thread".into())
    });
    drive_until(&mut dom, state, "thread-subscribe", || {
        state.peek().thread.synchronized
    })
    .await;
    assert_eq!(
        state.peek().thread.projection.as_ref().unwrap()["thread"]["id"],
        "thread"
    );
    dom.in_scope(ScopeId::APP, || {
        command(
            &props.transport,
            state,
            "thread.metadata.update",
            json!({"threadId":"thread","title":"Streamed new title"}),
        )
    })
    .unwrap();
    drive_until(&mut dom, state, "thread-title-stream", || {
        state
            .peek()
            .thread
            .projection
            .as_ref()
            .is_some_and(|projection| projection["thread"]["title"] == "Streamed new title")
    })
    .await;
    state
        .draft()
        .set("Do not lose or send this draft on reconnect".into());
    let prior_selection_epoch = props.transport.borrow().thread_generation;
    dom.in_scope(ScopeId::APP, || {
        new_thread(&props.transport, state, Some("project".into()))
    });
    assert!(state.peek().active_thread.is_none());
    assert!(state.peek().thread.projection.is_none());
    assert!(state.peek().draft.is_empty());
    assert!(props.transport.borrow().thread_subscription.is_none());
    assert_eq!(
        props.transport.borrow().thread_generation,
        prior_selection_epoch + 1
    );
    dom.in_scope(ScopeId::APP, || {
        select_thread(&props.transport, state, "thread".into())
    });
    drive_until(&mut dom, state, "return-to-thread", || {
        state.peek().thread.synchronized
    })
    .await;
    assert_eq!(
        state.peek().draft,
        "Do not lose or send this draft on reconnect"
    );
    let prior_sequence = state.peek().thread.sequence;
    let prior_reconnect_epoch = props.transport.borrow().thread_generation;
    // ewebsock 0.8 native close drops its channel without a Closed event.
    // Recovery therefore needs two five-second heartbeat ticks plus backoff;
    // the reconnect phase deliberately allows fifteen seconds.
    props
        .transport
        .borrow_mut()
        .sender
        .as_mut()
        .unwrap()
        .close();
    drive_until(&mut dom, state, "socket-reconnect", || {
        state.peek().status == ConnectionStatus::Connected
            && state.peek().thread.synchronized
            && state.peek().thread.sequence >= prior_sequence
            && props.transport.borrow().thread_generation > prior_reconnect_epoch
    })
    .await;
    assert_eq!(
        state.peek().draft,
        "Do not lose or send this draft on reconnect"
    );
    assert_eq!(
        state.peek().thread.projection.as_ref().unwrap()["thread"]["title"],
        "Streamed new title"
    );
    assert!(state.peek().pending_messages.is_empty());
    dom.in_scope(ScopeId::APP, || {
        forget_environment(
            &props.transport,
            state,
            &EnvironmentId::new("native-ui-test").unwrap(),
        )
    });
    assert_eq!(state.peek().status, ConnectionStatus::Disconnected);
    assert!(state.peek().destination.is_none());
    assert!(state.peek().environments.records.is_empty());
    assert!(state.peek().draft.is_empty());
    assert!(props.transport.borrow().unary_waiters.is_empty());
    drop(dom);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "current_thread")]
async fn read_only_native_session_receives_updates_but_cannot_create_projects() {
    let directory = tempfile::tempdir().unwrap();
    let api = api_fixture(&directory, "native-ui-test");
    let auth = api.auth.clone();
    let store = api.store.clone();
    let credential = auth
        .create_pairing_credential(
            &[AuthEnvironmentScope::OrchestrationRead],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;
    let props = Harness {
        state: Rc::new(RefCell::new(None)),
        transport: TransportHandle::default(),
        address,
        credential,
        initial: UiModel::default(),
    };
    let mut dom = VirtualDom::new_with_props(harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    drive_until(&mut dom, state, "readonly-shell", || {
        state.peek().shell.synchronized
    })
    .await;
    assert_eq!(state.peek().status, ConnectionStatus::Connected);
    let denied=dom.in_scope(ScopeId::APP,||request(&props.transport,state,"projects.mutate",json!({"type":"project.create","commandId":"forbidden","projectId":"forbidden","title":"Forbidden","workspaceRoot":directory.path().to_string_lossy()}),RequestKind::Unary));
    assert!(denied.is_none());
    assert!(store.projection("project", "forbidden").unwrap().is_none());
    t3_server::project::ProjectService::new(store).mutate(json!({"type":"project.create","commandId":"admin","projectId":"admin","title":"Server update","workspaceRoot":directory.path().to_string_lossy()}),chrono::Utc::now()).unwrap();
    drive_until(&mut dom, state, "readonly-live-update", || {
        state
            .peek()
            .shell
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.projects.len() == 1)
    })
    .await;
    assert_eq!(
        state.peek().shell.snapshot.as_ref().unwrap().projects[0]
            .title
            .as_str(),
        "Server update"
    );
    dom.in_scope(ScopeId::APP, || {
        forget_environment(
            &props.transport,
            state,
            &EnvironmentId::new("native-ui-test").unwrap(),
        )
    });
    drop(dom);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "current_thread")]
async fn changed_saved_address_is_rejected_before_pairing_credential_is_consumed() {
    let directory = tempfile::tempdir().unwrap();
    let api = api_fixture(&directory, "replacement-environment");
    let auth = api.auth.clone();
    let credential = auth
        .create_pairing_credential(
            &[AuthEnvironmentScope::OrchestrationRead],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;
    let mut initial = UiModel::default();
    initial
        .environments
        .register(
            &EnvironmentEndpoint::new(&address).unwrap(),
            EnvironmentId::new("saved-environment").unwrap(),
            "Saved".into(),
            2,
        )
        .unwrap();
    let props = Harness {
        state: Rc::new(RefCell::new(None)),
        transport: TransportHandle::default(),
        address,
        credential: credential.clone(),
        initial,
    };
    let mut dom = VirtualDom::new_with_props(harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    drive_until(&mut dom, state, "changed-address", || {
        matches!(state.peek().status, ConnectionStatus::Blocked(_))
    })
    .await;
    assert!(props.transport.borrow().sender.is_none());
    assert!(props.transport.borrow().bearer_token.is_empty());
    assert!(!state.peek().grants.authenticated);
    assert!(state.peek().destination.is_none());
    assert!(
        state
            .peek()
            .error
            .as_ref()
            .is_some_and(|message| message.contains("different environment"))
    );
    assert!(
        auth.exchange_pairing_bearer(&credential, None, json!({}), chrono::Utc::now())
            .is_ok(),
        "credential must never have been sent to the changed saved address"
    );
    drop(dom);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_provider_thread_messages_tools_approvals_input_and_interrupt() {
    let directory = tempfile::tempdir().unwrap();
    let mut api = api_fixture(&directory, "native-provider-ui");
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../server/tests/fixtures/codex-provider.py")
        .canonicalize()
        .unwrap();
    let rpc_record = directory.path().join("provider-rpc.jsonl");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(
        json!({"providerInstances":{"codex":{"driver":"codex","config":{"binaryPath":binary},"environment":[{"name":"FIXTURE_RPC_RECORD","value":rpc_record},{"name":"FIXTURE_SECOND_MODEL","value":"1"}]}}}),
    )
    .unwrap();
    let native = t3_server::config::NativeConfig::from_settings(
        settings,
        directory.path(),
        directory.path(),
        &api.environment,
        &api.auth.descriptor(),
    )
    .await
    .unwrap();
    let execution =
        t3_server::execution::ExecutionService::start(api.store.clone(), native.providers.clone());
    api.config = Some(native.snapshot);
    api.providers = Some(native.providers);
    api.execution = Some(execution.clone());
    let store = api.store.clone();
    let credential = api
        .auth
        .create_pairing_credential(
            &[
                AuthEnvironmentScope::OrchestrationRead,
                AuthEnvironmentScope::OrchestrationOperate,
            ],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;
    let props = Harness {
        state: Rc::new(RefCell::new(None)),
        transport: TransportHandle::default(),
        address,
        credential,
        initial: UiModel::default(),
    };
    let mut dom = VirtualDom::new_with_props(harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    drive_until(&mut dom, state, "native-provider-discovery", || {
        state.peek().shell.synchronized && !state.peek().model_options().is_empty()
    })
    .await;
    let options = state.peek().model_options();
    assert!(
        options
            .iter()
            .any(|(instance, model, _)| instance == "codex" && model == "fixture-model")
    );
    dom.in_scope(ScopeId::APP,||request(&props.transport,state,"projects.mutate",json!({"type":"project.create","commandId":"provider-project","projectId":"provider-project","title":"Provider project","workspaceRoot":directory.path()}),RequestKind::Unary)).unwrap();
    drive_until(&mut dom, state, "provider-project-create", || {
        state
            .peek()
            .shell
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.projects.len() == 1)
    })
    .await;
    // Exercise the rendered controls after asynchronous discovery; no model
    // preselection is supplied by the harness or by calling the launch helper.
    let (_, model_value) = control(&dom, "Model").expect("model control after config arrival");
    assert_eq!(
        model_value.as_deref(),
        Some("[\"codex\",\"fixture-model\"]")
    );
    // First-paint defaults must dispatch faithfully without selecting a model,
    // option, or permission first. Low is a display default, not an override.
    input_control(&mut dom, "First message", "Native streamed prompt");
    click_control(&mut dom, "Create new thread");
    drive_until(&mut dom, state, "user-facing-provider-launch", || {
        state.peek().active_thread.is_some() && state.peek().thread.synchronized
    })
    .await;
    let thread_id = state.peek().active_thread.clone().unwrap();
    let launched = store.projection("thread", &thread_id).unwrap().unwrap();
    assert_eq!(launched["thread"]["runtimeMode"], "full-access");
    assert!(
        launched["thread"]["modelSelection"]
            .get("options")
            .is_none()
    );

    assert_eq!(
        state.peek().thread.projection.as_ref().unwrap()["thread"]["modelSelection"]["model"],
        "fixture-model"
    );

    drive_until(&mut dom, state, "native-provider-completion", || {
        completed_runs(state) == 1
    })
    .await;
    assert!(state.peek().pending_messages.is_empty());
    assert!(state.peek().draft.is_empty());
    assert!(state.peek().new_thread_drafts.is_empty());
    let frames: Vec<serde_json::Value> = std::fs::read_to_string(&rpc_record)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let turn = frames
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .expect("atomic launch starts native turn");
    assert_eq!(turn["params"]["model"], "fixture-model");
    assert!(turn["params"].get("effort").is_none());
    assert_eq!(turn["params"]["approvalPolicy"], "never");
    assert_eq!(turn["params"]["sandboxPolicy"]["type"], "dangerFullAccess");
    {
        let model = state.peek();
        let projection = model.thread.projection.as_ref().unwrap();
        assert!(
            projection["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "assistant"
                    && message["text"] == "Fixture response: Native streamed prompt"
                    && message["streaming"] == false)
        );
        assert!(
            projection["visibleTurnItems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["item"]["type"] == "command_execution"
                    && row["item"]["outputOmitted"] == true
                    && row["item"]["exitCode"] == 0)
        );
    }
    click_control(&mut dom, "Load full output");
    // Atomic initial-message launch includes a workspace preparation activity
    // before the provider command; both details are fetched through the UI.
    wait_for_rendered_text(&mut dom, "Workspace preparation completed.").await;
    click_control(&mut dom, "Load full output");
    wait_for_rendered_text(&mut dom, "fixture tool output\n").await;

    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    change_control(&mut dom, "Reasoning", "low");
    change_control(&mut dom, "Permissions", "full-access");
    let supported_modes = state
        .typed_config()
        .peek()
        .as_ref()
        .unwrap()
        .providers
        .0
        .iter()
        .find(|p| p.instance_id.as_str() == "codex")
        .unwrap()
        .supported_runtime_modes
        .clone();
    state
        .typed_config()
        .write()
        .as_mut()
        .unwrap()
        .providers
        .0
        .iter_mut()
        .find(|p| p.instance_id.as_str() == "codex")
        .unwrap()
        .supported_runtime_modes = Some(Some(t3_contracts::ForwardCompatibleArray(vec![
        t3_contracts::RuntimeMode::ApprovalRequired,
    ])));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(
        control(&dom, "Permissions").unwrap().1.as_deref(),
        Some("approval-required"),
        "the effective visible mode must be submitted instead of raw draft Full access; configured {:?}",
        state
            .typed_config()
            .peek()
            .as_ref()
            .unwrap()
            .providers
            .0
            .iter()
            .find(|p| p.instance_id.as_str() == "codex")
            .unwrap()
            .supported_runtime_modes
    );
    input_control(&mut dom, "Message", "Native [approval]");
    let destination = state.peek().destination.clone().unwrap();
    let saved_session = state.peek().environments.records[&destination]
        .session
        .clone();
    state
        .environments()
        .write()
        .set_session(&destination, t3_contracts::SessionGrantInput::default())
        .unwrap();
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    drive_until(&mut dom, state, "runtime-change-permission-failure", || {
        state.pending_messages().peek().is_empty() && state.error().peek().is_some()
    })
    .await;
    assert_eq!(
        completed_runs(state),
        1,
        "a rejected settings change must prevent dispatch"
    );
    assert_eq!(state.draft().peek().as_str(), "Native [approval]");
    assert_eq!(
        store.projection("thread", &thread_id).unwrap().unwrap()["thread"]["runtimeMode"],
        "full-access"
    );
    state
        .environments()
        .write()
        .set_session(&destination, saved_session)
        .unwrap();
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    drive_until(&mut dom, state, "native-approval-needed", || {
        !pending_ui_requests(state).approvals.is_empty()
    })
    .await;
    let approval = pending_ui_requests(state).approvals.remove(0);
    assert!(approval.live);
    assert!(
        approval
            .options
            .iter()
            .any(|option| option["decision"] == "accept")
    );
    dom.in_scope(ScopeId::APP, || {
        command(
            &props.transport,
            state,
            "runtime-request.respond",
            json!({"threadId":thread_id,"requestId":approval.id,"decision":"accept"}),
        )
    })
    .unwrap();
    drive_until(&mut dom, state, "native-approved-completion", || {
        completed_runs(state) == 2 && pending_ui_requests(state).approvals.is_empty()
    })
    .await;

    assert_eq!(
        store.projection("thread", &thread_id).unwrap().unwrap()["thread"]["runtimeMode"],
        "approval-required"
    );
    let recorded = std::fs::read_to_string(&rpc_record).unwrap();
    let latest: Value = recorded
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|frame| frame["method"] == "turn/start")
        .last()
        .unwrap();
    assert_eq!(latest["params"]["effort"], "low");
    assert_eq!(latest["params"]["approvalPolicy"], "untrusted");
    state
        .typed_config()
        .write()
        .as_mut()
        .unwrap()
        .providers
        .0
        .iter_mut()
        .find(|p| p.instance_id.as_str() == "codex")
        .unwrap()
        .supported_runtime_modes = supported_modes;
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    change_control(&mut dom, "Permissions", "approval-required");
    let key = (destination.clone(), thread_id.clone());
    assert_eq!(
        state.thread_choices().peek()[&key].runtime_mode,
        Some(t3_contracts::RuntimeMode::ApprovalRequired)
    );
    let target =
        t3_client::draft_storage::DraftTarget::thread(destination.to_string(), thread_id.clone());
    let (_, sidecar) = state
        .peek()
        .draft_storage
        .document
        .borrow()
        .prepare_write()
        .unwrap()
        .unwrap();
    let mut restored = t3_client::draft_storage::DraftStorage::default();
    restored.hydrate(None, Some(sidecar), "2026-10-08T12:00:00Z");
    assert!(restored.prompt(&target).is_none());
    assert_eq!(
        restored.changes(&target).unwrap().choices.as_ref().unwrap()["runtimeMode"],
        "approval-required"
    );
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    change_control(&mut dom, "Model", "[\"codex\",\"fixture-model-alt\"]");
    assert_eq!(
        control(&dom, "Reasoning").unwrap().1.as_deref(),
        Some(""),
        "switching models starts at that model's defaults"
    );
    assert_eq!(
        store.projection("thread", &thread_id).unwrap().unwrap()["thread"]["modelSelection"]["model"],
        "fixture-model",
        "picker edits remain drafts until the next send"
    );
    state.draft().set("Native [question]".into());
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    drive_until(&mut dom, state, "native-user-input-needed", || {
        !pending_ui_requests(state).user_inputs.is_empty()
    })
    .await;
    let input = pending_ui_requests(state).user_inputs.remove(0);
    assert_eq!(input.response_capability, "live");
    let question_id = input.questions[0]["id"].as_str().unwrap().to_owned();
    let mut answers = std::collections::BTreeMap::new();
    answers.insert(
        question_id,
        t3_client::requests::DraftAnswer {
            selected: vec!["One".into()],
            ..Default::default()
        },
    );
    let answers = t3_client::requests::build_answers(&input.questions, &answers).unwrap();
    dom.in_scope(ScopeId::APP, || {
        command(
            &props.transport,
            state,
            "runtime-request.respond",
            json!({"threadId":thread_id,"requestId":input.id,"answers":answers}),
        )
    })
    .unwrap();
    drive_until(&mut dom, state, "native-input-completion", || {
        completed_runs(state) == 3 && pending_ui_requests(state).user_inputs.is_empty()
    })
    .await;

    let recorded = std::fs::read_to_string(&rpc_record).unwrap();
    let latest: Value = recorded
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|frame| frame["method"] == "turn/start")
        .last()
        .unwrap();
    assert_eq!(latest["params"]["model"], "fixture-model-alt");
    assert!(
        latest["params"].get("effort").is_none(),
        "previous model's explicit effort must not bleed into the alternate model"
    );
    state.draft().set("Native [hold]".into());
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    drive_until(&mut dom, state, "native-running-held-turn", || {
        state
            .peek()
            .thread
            .projection
            .as_ref()
            .is_some_and(|projection| {
                projection["runs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|run| run["ordinal"] == 4 && run["status"] == "running")
            })
    })
    .await;
    let provider_session =
        state.peek().thread.projection.as_ref().unwrap()["providerSessions"][0]["id"].clone();
    let prior_reconnect_epoch = props.transport.borrow().thread_generation;
    state
        .draft()
        .set("Unsent draft during active provider work".into());
    // Native ewebsock close is silent; original five-second heartbeat recovery
    // must reconnect without starting a second provider turn or losing the draft.
    props
        .transport
        .borrow_mut()
        .sender
        .as_mut()
        .unwrap()
        .close();
    drive_until(&mut dom, state, "socket-reconnect", || {
        state.peek().status == ConnectionStatus::Connected
            && state.peek().thread.synchronized
            && props.transport.borrow().thread_generation > prior_reconnect_epoch
    })
    .await;
    assert_eq!(
        state.peek().draft,
        "Unsent draft during active provider work"
    );
    assert_eq!(
        state.peek().thread.projection.as_ref().unwrap()["providerSessions"][0]["id"],
        provider_session
    );
    assert_eq!(
        state.peek().thread.projection.as_ref().unwrap()["runs"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        state.peek().thread.projection.as_ref().unwrap()["runs"][3]["status"],
        "running"
    );
    dom.in_scope(ScopeId::APP, || {
        stop_thread(&props.transport, state, &thread_id)
    });
    drive_until(&mut dom, state, "native-interrupted-turn", || {
        state
            .peek()
            .thread
            .projection
            .as_ref()
            .is_some_and(|projection| {
                projection["runs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|run| run["ordinal"] == 4 && run["status"] == "interrupted")
            })
    })
    .await;
    assert!(state.peek().error.is_none(), "{:?}", state.peek().error);
    assert_eq!(
        store.projection("thread", &thread_id).unwrap().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .count(),
        4
    );
    dom.in_scope(ScopeId::APP, || {
        crate::runtime::new_thread(&props.transport, state, Some("provider-project".into()))
    });
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    change_control(&mut dom, "Reasoning", "low");
    change_control(&mut dom, "Permissions", "approval-required");
    change_control(&mut dom, "Workspace", "worktree");
    input_control(&mut dom, "Base branch", "owned-base");
    input_control(&mut dom, "First message", "Explicit launch choices");
    dom.in_scope(ScopeId::APP, || {
        select_thread(&props.transport, state, thread_id.clone())
    });
    drive_until(&mut dom, state, "draft-navigation-thread", || {
        state.peek().thread.synchronized
    })
    .await;
    dom.in_scope(ScopeId::APP, || {
        crate::runtime::new_thread(&props.transport, state, Some("provider-project".into()))
    });
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(
        control(&dom, "Permissions").unwrap().1.as_deref(),
        Some("approval-required")
    );
    assert_eq!(
        control(&dom, "Reasoning").unwrap().1.as_deref(),
        Some("low")
    );
    assert_eq!(
        control(&dom, "Workspace").unwrap().1.as_deref(),
        Some("worktree")
    );
    assert_eq!(
        control(&dom, "Base branch").unwrap().1.as_deref(),
        Some("owned-base")
    );
    assert_eq!(
        control(&dom, "First message").unwrap().1.as_deref(),
        Some("Explicit launch choices")
    );
    change_control(&mut dom, "Workspace", "local");
    click_control(&mut dom, "Create new thread");
    drive_until(&mut dom, state, "explicit-launch-completion", || {
        state
            .peek()
            .active_thread
            .as_ref()
            .is_some_and(|id| id != &thread_id)
            && completed_runs(state) == 1
    })
    .await;
    let explicit_id = state.peek().active_thread.clone().unwrap();
    let explicit = store.projection("thread", &explicit_id).unwrap().unwrap();
    assert_eq!(explicit["thread"]["runtimeMode"], "approval-required");
    assert_eq!(
        explicit["thread"]["modelSelection"]["options"],
        json!([{"id":"reasoningEffort","value":"low"}])
    );
    let recorded = std::fs::read_to_string(&rpc_record).unwrap();
    let latest: serde_json::Value = recorded
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|frame| frame["method"] == "turn/start")
        .last()
        .unwrap();
    assert_eq!(latest["params"]["effort"], "low");
    assert_eq!(latest["params"]["approvalPolicy"], "untrusted");
    dom.in_scope(ScopeId::APP, || {
        crate::runtime::new_thread(&props.transport, state, Some("provider-project".into()))
    });
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    change_control(&mut dom, "Reasoning", "low");
    change_control(&mut dom, "Reasoning", "");
    input_control(&mut dom, "First message", "Provider default after reset");
    click_control(&mut dom, "Create new thread");
    drive_until(&mut dom, state, "default-reset-completion", || {
        state
            .peek()
            .active_thread
            .as_ref()
            .is_some_and(|id| id != &explicit_id)
            && completed_runs(state) == 1
    })
    .await;
    let recorded = std::fs::read_to_string(&rpc_record).unwrap();
    let latest: serde_json::Value = recorded
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|frame| frame["method"] == "turn/start")
        .last()
        .unwrap();
    assert!(
        latest["params"].get("effort").is_none(),
        "reset must remove explicit effort from provider wire"
    );
    // First-poll ownership: a queued send must never resolve a new destination
    // merely because root-owned tasks survive the composer being unmounted.
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    change_control(&mut dom, "Permissions", "approval-required");
    input_control(
        &mut dom,
        "Message",
        "Never dispatch after switching destination",
    );
    let original_destination = state.peek().destination.clone().unwrap();
    let current_id = state.peek().active_thread.clone().unwrap();
    let before = store.latest_sequence().unwrap();
    state.error().set(None);
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    state.destination().set(Some(
        t3_contracts::EnvironmentId::new("same-ids-other-environment").unwrap(),
    ));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(state.pending_messages().peek().is_empty());
    assert!(props.transport.borrow().unary_waiters.is_empty());
    assert!(
        state.error().peek().is_none(),
        "stale task must not attempt a mutation through current destination permissions"
    );
    assert_eq!(
        store.latest_sequence().unwrap(),
        before,
        "no settings mutation before first poll"
    );
    state.destination().set(Some(original_destination.clone()));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(
        !props.transport.borrow().unary_waiters.is_empty(),
        "settings RPC has been sent and is awaiting its receipt"
    );
    state.destination().set(Some(
        t3_contracts::EnvironmentId::new("same-ids-other-environment").unwrap(),
    ));
    drive_until(
        &mut dom,
        state,
        "destination-change-during-settings",
        || props.transport.borrow().unary_waiters.is_empty(),
    )
    .await;
    // Await the persisted old-destination setting event, then drain the send
    // continuation: an accepted setting never dispatches content after switch.
    assert_eq!(
        store.projection("thread", &current_id).unwrap().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .count(),
        1
    );
    assert_eq!(
        state.draft().peek().as_str(),
        "Never dispatch after switching destination"
    );
    state.destination().set(Some(original_destination.clone()));
    dom.in_scope(ScopeId::APP, || {
        forget_environment(&props.transport, state, &original_destination)
    });
    drop(dom);
    server.abort();
    execution.shutdown().await;
}
fn pending_ui_requests(state: Store<UiModel>) -> t3_client::requests::PendingRequests {
    state
        .peek()
        .thread
        .projection
        .as_ref()
        .map(t3_client::requests::pending_requests)
        .unwrap_or_default()
}
fn completed_runs(state: Store<UiModel>) -> usize {
    state
        .peek()
        .thread
        .projection
        .as_ref()
        .and_then(|projection| projection["runs"].as_array())
        .map(|runs| {
            runs.iter()
                .filter(|run| run["status"] == "completed")
                .count()
        })
        .unwrap_or(0)
}

// Find the mounted input/button by its accessible label, then feed a platform
// event through Dioxus's normal listener conversion and reactive scheduler.
// This verifies actual control state and native RPC behavior, not static markup.
pub(crate) fn control(
    dom: &VirtualDom,
    label: &str,
) -> Option<(dioxus::dioxus_core::ElementId, Option<String>)> {
    control_attribute(dom, label, "value")
}
fn control_attribute(
    dom: &VirtualDom,
    label: &str,
    attribute_name: &str,
) -> Option<(dioxus::dioxus_core::ElementId, Option<String>)> {
    use dioxus::dioxus_core::{
        AttributeValue, DynamicNode, TemplateAttribute, TemplateNode, VNode,
    };
    fn find(
        dom: &VirtualDom,
        node: &VNode,
        label: &str,
        attribute_name: &str,
    ) -> Option<(dioxus::dioxus_core::ElementId, Option<String>)> {
        for (index, attributes) in node.dynamic_attrs.iter().enumerate() {
            let path = node.template.attr_paths[index];
            let mut template = &node.template.roots[usize::from(path[0])];
            for child in &path[1..] {
                let TemplateNode::Element { children, .. } = template else {
                    continue;
                };
                template = &children[usize::from(*child)];
            }
            let TemplateNode::Element { attrs, .. } = template else {
                continue;
            };
            let labeled=attrs.iter().any(|a|matches!(a,TemplateAttribute::Static {name:"aria-label",value,..} if *value==label)) || attributes.iter().any(|a|a.name=="aria-label" && matches!(&a.value,AttributeValue::Text(v) if v==label));
            if labeled {
                return Some((
                    node.mounted_dynamic_attribute(index, dom)?,
                    node.dynamic_attrs
                        .iter()
                        .enumerate()
                        .filter(|(other, _)| node.template.attr_paths[*other] == path)
                        .flat_map(|(_, attrs)| attrs.iter())
                        .find_map(|a| match (&a.name, &a.value) {
                            (name, AttributeValue::Text(v)) if *name == attribute_name => {
                                Some(v.clone())
                            }
                            (name, AttributeValue::Bool(v)) if *name == attribute_name => {
                                Some(v.to_string())
                            }
                            _ => None,
                        }),
                ));
            }
        }
        for (index, child) in node.dynamic_nodes.iter().enumerate() {
            match child {
                DynamicNode::Component(component) => {
                    if let Some(scope) = component.mounted_scope(index, node, dom) {
                        if let Some(found) = find(dom, scope.root_node(), label, attribute_name) {
                            return Some(found);
                        }
                    }
                }
                DynamicNode::Fragment(nodes) => {
                    for node in nodes {
                        if let Some(found) = find(dom, node, label, attribute_name) {
                            return Some(found);
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }
    find(dom, dom.base_scope().root_node(), label, attribute_name)
}
pub(crate) fn change_control(dom: &mut VirtualDom, label: &str, value: &str) {
    form_control_event(dom, label, value, "change");
}
pub(crate) fn input_control(dom: &mut VirtualDom, label: &str, value: &str) {
    form_control_event(dom, label, value, "input");
}
fn form_control_event(dom: &mut VirtualDom, label: &str, value: &str, event: &str) {
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(dom, label).unwrap_or_else(|| panic!("missing control {label}"));
    let input = dioxus_html::SerializedFormData::new(value.into(), vec![]);
    dom.runtime().handle_event(
        event,
        dioxus::dioxus_core::Event::new(
            Rc::new(dioxus_html::PlatformEventData::new(Box::new(input))) as Rc<dyn std::any::Any>,
            true,
        ),
        element,
    );
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}
fn blur_control(dom: &mut VirtualDom, label: &str, render: bool) {
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(dom, label).unwrap_or_else(|| panic!("missing control {label}"));
    dom.runtime().handle_event(
        "blur",
        dioxus::dioxus_core::Event::new(
            Rc::new(dioxus_html::PlatformEventData::new(Box::new(
                dioxus_html::SerializedFocusData::default(),
            ))) as Rc<dyn std::any::Any>,
            true,
        ),
        element,
    );
    if render {
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    }
}
pub(crate) fn click_control(dom: &mut VirtualDom, label: &str) {
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(dom, label).unwrap_or_else(|| panic!("missing control {label}"));
    let mouse: dioxus_html::SerializedMouseData = serde_json::from_value(
        serde_json::to_value(dioxus_html::point_interaction::SerializedPointInteraction::default())
            .unwrap(),
    )
    .unwrap();
    dom.runtime().handle_event(
        "click",
        dioxus::dioxus_core::Event::new(
            Rc::new(dioxus_html::PlatformEventData::new(Box::new(mouse))) as Rc<dyn std::any::Any>,
            true,
        ),
        element,
    );
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}
pub(crate) fn rendered_text(dom: &VirtualDom) -> String {
    use dioxus::dioxus_core::{DynamicNode, TemplateNode, VNode};
    fn static_text(node: &TemplateNode, text: &mut String) {
        match node {
            TemplateNode::Text { text: value } => text.push_str(value),
            TemplateNode::Element { children, .. } => {
                for child in *children {
                    static_text(child, text);
                }
            }
            _ => {}
        }
    }
    fn collect(dom: &VirtualDom, node: &VNode, text: &mut String) {
        for root in node.template.roots {
            static_text(root, text);
        }
        for (index, child) in node.dynamic_nodes.iter().enumerate() {
            match child {
                DynamicNode::Text(value) => text.push_str(&value.value),
                DynamicNode::Component(component) => {
                    if let Some(scope) = component.mounted_scope(index, node, dom) {
                        collect(dom, scope.root_node(), text);
                    }
                }
                DynamicNode::Fragment(nodes) => {
                    for node in nodes {
                        collect(dom, node, text);
                    }
                }
                _ => {}
            }
        }
    }
    let mut text = String::new();
    collect(dom, dom.base_scope().root_node(), &mut text);
    text
}
pub(crate) async fn wait_for_rendered_text(dom: &mut VirtualDom, expected: &str) {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if rendered_text(dom).contains(expected) {
                return;
            };
            dom.wait_for_work().await;
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "UI did not render expected detail: {expected}; rendered: {}",
            rendered_text(dom)
        )
    });
}

#[derive(Clone)]
struct ControlsHarness {
    state: Rc<RefCell<Option<Store<UiModel>>>>,
    initial: UiModel,
    transport: TransportHandle,
}
fn controls_harness(props: ControlsHarness) -> Element {
    let state = use_store(|| props.initial.clone());
    crate::themes::use_themes(state);
    *props.state.borrow_mut() = Some(state);
    rsx! {crate::Application {state,transport:props.transport}}
}
#[test]
fn new_thread_controls_reset_for_identical_ids_in_another_destination_or_project() {
    let directory = tempfile::tempdir().unwrap();
    let make_config = |environment: &str| {
        let mut config = api_fixture(&directory, environment).config.unwrap();
        config["providers"] = json!([{"instanceId":"codex","driver":"codex","enabled":true,"installed":true,"version":null,"status":"ready","auth":{"status":"authenticated"},"checkedAt":"now","models":[{"slug":"same-model","name":"Same model","isCustom":false,"isDefault":true,"capabilities":{"optionDescriptors":[{"id":"reasoningEffort","label":"Reasoning","type":"select","currentValue":"low","options":[{"id":"low","label":"Low","isDefault":true},{"id":"high","label":"High"}]}]}}],"slashCommands":[],"skills":[]}]);
        serde_json::from_value::<t3_contracts::ServerConfig>(config).unwrap()
    };
    let project:t3_contracts::ProjectShell=serde_json::from_value(json!({"id":"same-project","title":"Same project","workspaceRoot":"/isolated","defaultModelSelection":null,"scripts":[],"createdAt":"now","updatedAt":"now"})).unwrap();
    let mut model = UiModel::default();
    model.destination = Some(t3_contracts::EnvironmentId::new("environment-a").unwrap());
    model.typed_config = Some(make_config("environment-a"));
    model.shell.snapshot = Some(t3_contracts::ShellSnapshot {
        schema_version: 2,
        snapshot_sequence: 0,
        projects: vec![project.clone()],
        threads: vec![],
        archived_threads: vec![],
    });
    let props = ControlsHarness {
        state: Rc::new(RefCell::new(None)),
        initial: model,
        transport: TransportHandle::default(),
    };
    let mut dom = VirtualDom::new_with_props(controls_harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    assert_eq!(
        control(&dom, "Model").unwrap().1.as_deref(),
        Some("[\"codex\",\"same-model\"]")
    );
    change_control(&mut dom, "Reasoning", "high");
    change_control(&mut dom, "Permissions", "auto");
    change_control(&mut dom, "Workspace", "worktree");
    input_control(&mut dom, "First message", "Environment A draft");
    assert_eq!(
        control(&dom, "Reasoning").unwrap().1.as_deref(),
        Some("high")
    );
    let mut config = make_config("environment-b");
    let other_project = t3_contracts::ProjectId::new("other-project").unwrap();
    config.settings.project_settings_overrides.insert(
        other_project.clone(),
        serde_json::from_value(json!({"defaultRuntimeMode":"approval-required"})).unwrap(),
    );
    state.typed_config().set(Some(config));
    state.destination().set(Some(
        t3_contracts::EnvironmentId::new("environment-b").unwrap(),
    ));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(
        control(&dom, "Model").unwrap().1.as_deref(),
        Some("[\"codex\",\"same-model\"]")
    );
    assert_eq!(control(&dom, "Reasoning").unwrap().1.as_deref(), Some(""));
    assert_eq!(
        control(&dom, "Permissions").unwrap().1.as_deref(),
        Some("full-access")
    );
    assert_eq!(
        control(&dom, "Workspace").unwrap().1.as_deref(),
        Some("local")
    );
    assert!(control(&dom, "Base branch").is_none());
    assert_eq!(
        control(&dom, "First message").unwrap().1.as_deref(),
        Some("")
    );
    input_control(&mut dom, "First message", "Environment B draft");
    change_control(&mut dom, "Reasoning", "high");
    change_control(&mut dom, "Permissions", "auto");
    let mut other = project;
    other.id = other_project.clone();
    state
        .shell()
        .write()
        .snapshot
        .as_mut()
        .unwrap()
        .projects
        .push(other);
    state
        .selected_project()
        .set(Some(other_project.to_string()));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(
        control(&dom, "Permissions").unwrap().1.as_deref(),
        Some("approval-required")
    );
    assert_eq!(
        control(&dom, "First message").unwrap().1.as_deref(),
        Some("")
    );
    state.selected_project().set(Some("same-project".into()));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(
        control(&dom, "First message").unwrap().1.as_deref(),
        Some("Environment B draft")
    );
    // Sticky selections deliberately persist within their destination, and
    // Forget removes that destination's state even when another one is active.
    let a = t3_contracts::EnvironmentId::new("environment-a").unwrap();
    let b = t3_contracts::EnvironmentId::new("environment-b").unwrap();
    assert!(state.sticky_models().peek().contains_key(&a));
    assert!(state.sticky_models().peek().contains_key(&b));
    assert!(
        state
            .new_thread_choices()
            .peek()
            .keys()
            .any(|(owner, _)| owner == &a)
    );
    assert_eq!(
        state
            .new_thread_choices()
            .peek()
            .get(&(b.clone(), "same-project".into()))
            .unwrap()
            .runtime_mode,
        Some(t3_contracts::RuntimeMode::Auto)
    );
    dom.in_scope(ScopeId::APP, || {
        forget_environment(&props.transport, state, &a)
    });
    assert!(!state.sticky_models().peek().contains_key(&a));
    assert!(
        !state
            .new_thread_choices()
            .peek()
            .keys()
            .any(|(owner, _)| owner == &a)
    );
    assert!(
        !state
            .new_thread_drafts()
            .peek()
            .keys()
            .any(|(owner, _)| owner == &a)
    );
    assert_eq!(
        state
            .new_thread_drafts()
            .peek()
            .get(&(b.clone(), "same-project".into()))
            .map(String::as_str),
        Some("Environment B draft")
    );
    assert!(state.sticky_models().peek().contains_key(&b));
    assert_eq!(state.destination().peek().as_ref(), Some(&b));
}

#[test]
fn identical_question_request_and_thread_ids_in_another_environment_start_unanswered() {
    let a = t3_contracts::EnvironmentId::new("environment-a").unwrap();
    let b = t3_contracts::EnvironmentId::new("environment-b").unwrap();
    let mut model = crate::reactive_tests::large_model();
    model.shell.snapshot.as_mut().unwrap().projects.truncate(1);
    model.destination = Some(a.clone());
    model.status = ConnectionStatus::Connected;
    for (id, address) in [(&a, "http://127.0.0.1:4111"), (&b, "http://127.0.0.1:4222")] {
        model
            .environments
            .register(
                &EnvironmentEndpoint::new(address).unwrap(),
                id.clone(),
                id.to_string(),
                2,
            )
            .unwrap();
        model
            .environments
            .set_session(
                id,
                t3_contracts::SessionGrantInput {
                    authenticated: true,
                    permissions: Some(vec![AuthEnvironmentScope::OrchestrationOperate]),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let question = json!({"id":"same-question","header":"Choose","question":"Choose a response","allowCustomAnswer":false,"options":[{"label":"One"},{"label":"Two"}]});
    model.thread.projection = Some(
        json!({"thread":{"id":"thread"},"visibleTurnItems":[],"turnItems":[{"id":"same-item","type":"user_input_request","requestId":"same-request","questions":[question],"responseMode":"message"}],"runtimeRequests":[{"id":"same-request","kind":"user_input","status":"pending","responseCapability":{"type":"live"}}]}),
    );
    let props = ControlsHarness {
        state: Rc::new(RefCell::new(None)),
        initial: model,
        transport: TransportHandle::default(),
    };
    let mut dom = VirtualDom::new_with_props(controls_harness, props.clone());
    dom.rebuild_in_place();
    assert_eq!(
        control_attribute(&dom, "Submit answers", "disabled")
            .unwrap()
            .1
            .as_deref(),
        Some("true")
    );
    change_control(&mut dom, "One", "true");
    assert_eq!(
        control_attribute(&dom, "One", "checked")
            .unwrap()
            .1
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        control_attribute(&dom, "Submit answers", "disabled")
            .unwrap()
            .1
            .as_deref(),
        Some("false")
    );
    props.state.borrow().unwrap().destination().set(Some(b));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(
        control_attribute(&dom, "One", "checked")
            .unwrap()
            .1
            .as_deref(),
        Some("false")
    );
    assert_eq!(
        control_attribute(&dom, "Submit answers", "disabled")
            .unwrap()
            .1
            .as_deref(),
        Some("true")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn readonly_ui_loads_bounded_history_through_native_http_without_duplicates() {
    let directory = tempfile::tempdir().unwrap();
    let api = api_fixture(&directory, "history-ui-test");
    let store = api.store.clone();
    t3_server::project::ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"history-project","projectId":"project","title":"History","workspaceRoot":directory.path()}),chrono::Utc::now()).unwrap();
    let launched=t3_server::launch::ThreadLaunchService::new(store.clone()).launch(json!({"commandId":"history-thread","threadId":"thread","projectId":"project","title":"History thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}),chrono::Utc::now()).unwrap();
    let mut projection = launched["projection"].clone();
    // Seed an isolated durable projection with a long tool-only history. This
    // exercises source item-budget paging without running 130 provider tools.
    let items:Vec<_>=(0..130).map(|index|json!({"id":format!("item-{index}"),"threadId":"thread","runId":null,"nodeId":null,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":index,"status":"completed","title":null,"startedAt":null,"completedAt":null,"updatedAt":"2026-01-01T00:00:00Z","type":"command_execution","input":format!("echo fixture-{index}"),"output":format!("fixture-{index}"),"exitCode":0})).collect();
    projection["turnItems"] = json!(items);
    projection["visibleTurnItems"]=json!(items.iter().enumerate().map(|(index,item)|json!({"position":index,"visibility":"local","sourceThreadId":"thread","sourceItemId":item["id"],"item":item})).collect::<Vec<_>>());
    let _: t3_contracts::ThreadProjection = serde_json::from_value(projection.clone()).unwrap();
    store
        .transaction(|transaction| {
            t3_server::persistence::write_projection(transaction, "thread", "thread", &projection)
        })
        .unwrap();
    let credential = api
        .auth
        .create_pairing_credential(
            &[AuthEnvironmentScope::OrchestrationRead],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;
    let props = Harness {
        state: Rc::new(RefCell::new(None)),
        transport: TransportHandle::default(),
        address,
        credential,
        initial: UiModel::default(),
    };
    let mut dom = VirtualDom::new_with_props(harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    drive_until(&mut dom, state, "history-shell", || {
        state.peek().shell.synchronized
    })
    .await;
    dom.in_scope(ScopeId::APP, || {
        select_thread(&props.transport, state, "thread".into())
    });
    drive_until(&mut dom, state, "bounded-history", || {
        state.peek().thread.synchronized
    })
    .await;
    assert_eq!(
        state.peek().thread.projection.as_ref().unwrap()["visibleTurnItems"]
            .as_array()
            .unwrap()
            .len(),
        75
    );
    assert!(state.peek().thread.has_more_history);
    click_control(&mut dom, "Load earlier messages");
    drive_until(&mut dom, state, "history-page", || {
        !state.peek().thread.has_more_history
    })
    .await;
    let page = state.peek().thread.projection.as_ref().unwrap()["visibleTurnItems"].clone();
    let page = page.as_array().unwrap();
    assert_eq!(page.len(), 130);
    assert_eq!(page.first().unwrap()["sourceItemId"], "item-0");
    assert_eq!(page.last().unwrap()["sourceItemId"], "item-129");
    let identities: std::collections::BTreeSet<_> = page
        .iter()
        .map(|r| r["sourceItemId"].as_str().unwrap())
        .collect();
    assert_eq!(identities.len(), 130);
    assert!(control(&dom, "Load earlier messages").is_none());
    assert!(!state.environments().peek().allows(
        state.destination().peek().as_ref().unwrap(),
        AuthEnvironmentScope::OrchestrationOperate
    ));
    dom.in_scope(ScopeId::APP, || {
        let destination = state.peek().destination.clone().unwrap();
        forget_environment(&props.transport, state, &destination);
    });
    server.abort();
}

#[test]
fn existing_thread_choices_restore_and_remain_owned_by_destination_and_thread() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = api_fixture(&directory, "environment-a").config.unwrap();
    config["providers"] = json!([{"instanceId":"codex","driver":"codex","enabled":true,"installed":true,"version":null,"status":"ready","auth":{"status":"authenticated"},"checkedAt":"now","models":[{"slug":"model","name":"Model","isCustom":false,"isDefault":true,"capabilities":{"optionDescriptors":[{"id":"reasoningEffort","label":"Reasoning","type":"select","currentValue":"low","options":[{"id":"low","label":"Low","isDefault":true},{"id":"high","label":"High"}]}]}},{"slug":"other","name":"Other model","isCustom":false,"isDefault":false,"capabilities":null}],"slashCommands":[],"skills":[]}]);
    let mut model = crate::reactive_tests::large_model();
    model.typed_config = Some(serde_json::from_value(config).unwrap());
    let a = EnvironmentId::new("environment-a").unwrap();
    let b = EnvironmentId::new("environment-b").unwrap();
    model.destination = Some(a.clone());
    let source_bytes=json!({"version":9,"state":{"draftsByThreadKey":{"environment-a:thread":{"prompt":"Recovered existing text","attachments":[],"activeProvider":"codex","modelSelectionByProvider":{"codex":{"instanceId":"codex","model":"model","options":[{"id":"reasoningEffort","value":"high"}]}},"runtimeMode":"full-access"}}}}).to_string();
    model.draft_storage.document.borrow_mut().hydrate(
        Some(source_bytes.clone()),
        None,
        "2026-10-08T12:00:00Z",
    );
    let initial = model.clone();
    let props = ControlsHarness {
        state: Rc::new(RefCell::new(None)),
        initial: model,
        transport: TransportHandle::default(),
    };
    let mut dom = VirtualDom::new_with_props(controls_harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    crate::draft_storage::restore_destination(state);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(
        control(&dom, "Reasoning").unwrap().1.as_deref(),
        Some("high")
    );
    assert_eq!(
        control(&dom, "Permissions").unwrap().1.as_deref(),
        Some("full-access")
    );
    change_control(&mut dom, "Reasoning", "low");
    change_control(&mut dom, "Permissions", "auto");
    input_control(&mut dom, "Message", "Owned existing text");
    // Switching away and back restores this model's own option snapshot.
    change_control(&mut dom, "Model", "[\"codex\",\"other\"]");
    assert!(control(&dom, "Reasoning").is_none());
    change_control(&mut dom, "Model", "[\"codex\",\"model\"]");
    assert_eq!(
        control(&dom, "Reasoning").unwrap().1.as_deref(),
        Some("low")
    );
    let (_, sidecar) = state
        .peek()
        .draft_storage
        .document
        .borrow()
        .prepare_write()
        .unwrap()
        .unwrap();
    state.destination().set(Some(b));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(control(&dom, "Reasoning").unwrap().1.as_deref(), Some(""));
    assert_eq!(
        control(&dom, "Permissions").unwrap().1.as_deref(),
        Some("approval-required")
    );
    state.destination().set(Some(a));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(
        control(&dom, "Permissions").unwrap().1.as_deref(),
        Some("auto")
    );
    assert_eq!(
        state
            .peek()
            .draft_storage
            .document
            .borrow()
            .source_bytes
            .as_deref(),
        Some(source_bytes.as_str())
    );
    drop(dom);
    let mut reloaded = initial;
    reloaded.draft_storage = crate::draft_storage::DraftHandle::default();
    reloaded.draft_storage.document.borrow_mut().hydrate(
        Some(source_bytes),
        Some(sidecar),
        "2026-10-08T12:00:00Z",
    );
    let props = ControlsHarness {
        state: Rc::new(RefCell::new(None)),
        initial: reloaded,
        transport: TransportHandle::default(),
    };
    let mut dom = VirtualDom::new_with_props(controls_harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    crate::draft_storage::restore_destination(state);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(
        control(&dom, "Reasoning").unwrap().1.as_deref(),
        Some("low")
    );
    assert_eq!(
        control(&dom, "Permissions").unwrap().1.as_deref(),
        Some("auto")
    );
    assert_eq!(
        control(&dom, "Message").unwrap().1.as_deref(),
        Some("Owned existing text")
    );
}

async fn provider_settings_fixture(
    directory: &tempfile::TempDir,
) -> (ApiState, t3_server::server_settings::SettingsService) {
    use t3_server::{
        server_secret_store::ServerSecretStore,
        server_settings::{SettingsOptions, SettingsService},
    };
    let mut api = api_fixture(directory, "provider-settings-ui");
    let path = directory.path().join("settings.json");
    let mut value = serde_json::to_value(t3_contracts::ServerSettings::default()).unwrap();
    for (_, provider) in value["providers"].as_object_mut().unwrap() {
        provider["enabled"] = json!(false);
    }
    value["providerInstances"] = json!({
        "work_local":{"driver":"acpRegistry","displayName":"Work","enabled":false,"config":{"source":"local","commandPath":"fixture-command","opaque":{"items":[1,2]}}},
        "other_local":{"driver":"acpRegistry","displayName":"Other","enabled":false,"config":{"source":"local","commandPath":"other-command","opaque":{"retain":true}}}
    });
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let mut options = SettingsOptions::file(
        path,
        ServerSecretStore::open(directory.path().join("secrets")).unwrap(),
    );
    options.watch = false;
    let service = SettingsService::start(options).await.unwrap();
    api.config.as_mut().unwrap()["settings"] =
        serde_json::to_value(service.snapshot().await.unwrap()).unwrap();
    api.settings = Some(service.clone());
    (api, service)
}

#[tokio::test(flavor = "current_thread")]
async fn native_provider_settings_controls_mutate_one_instance_and_preserve_unknown_config() {
    let directory = tempfile::tempdir().unwrap();
    let (api, service) = provider_settings_fixture(&directory).await;
    let credential = api
        .auth
        .create_pairing_credential(
            &[
                AuthEnvironmentScope::OrchestrationRead,
                AuthEnvironmentScope::ProvidersManage,
            ],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;
    let props = Harness {
        state: Rc::new(RefCell::new(None)),
        transport: TransportHandle::default(),
        address,
        credential,
        initial: UiModel::default(),
    };
    let mut dom = VirtualDom::new_with_props(harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    drive_until(&mut dom, state, "provider-settings-config", || {
        state.peek().typed_config.is_some()
    })
    .await;
    state.view().set(View::Providers);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    click_control(&mut dom, "Select provider Work");
    assert_eq!(
        control(&dom, "Executable").unwrap().1.as_deref(),
        Some("fixture-command")
    );
    input_control(&mut dom, "Executable", "updated-command");
    assert_eq!(
        serde_json::to_value(service.snapshot().await.unwrap()).unwrap()["providerInstances"]["work_local"]
            ["config"]["commandPath"],
        "fixture-command",
        "typing stays local until source blur commit"
    );
    blur_control(&mut dom, "Executable", true);
    drive_until(&mut dom,state,"provider-settings-committed",||state.config().peek()["settings"]["providerInstances"]["work_local"]["config"]["commandPath"]=="updated-command").await;
    let stored = serde_json::to_value(service.snapshot().await.unwrap()).unwrap();
    assert_eq!(
        stored["providerInstances"]["work_local"]["config"]["opaque"],
        json!({"items":[1,2]})
    );
    assert_eq!(
        stored["providerInstances"]["other_local"]["config"],
        json!({"source":"local","commandPath":"other-command","opaque":{"retain":true}})
    );
    assert!(control(&dom, "Executable").is_some());
    let destination = state.destination().peek().clone().unwrap();
    let saved = state.environments().peek().records[&destination]
        .session
        .clone();
    {
        let mut environments_field = state.environments();
        let mut environments = environments_field.write();
        let session = &mut environments.records.get_mut(&destination).unwrap().session;
        session.scopes = Some(vec![AuthEnvironmentScope::OrchestrationRead]);
        // Current servers return granular permissions; those are authoritative
        // over legacy scopes, so revoke the actual grant rather than its alias.
        session.permissions = Some(vec![AuthEnvironmentScope::OrchestrationRead]);
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(
        control_attribute(&dom, "Executable", "disabled")
            .unwrap()
            .1
            .as_deref(),
        Some("true")
    );
    input_control(&mut dom, "Executable", "must-not-persist");
    blur_control(&mut dom, "Executable", true);
    assert_eq!(
        serde_json::to_value(service.snapshot().await.unwrap()).unwrap()["providerInstances"]["work_local"]
            ["config"]["commandPath"],
        "updated-command",
        "destination manage grant is authoritative even for synthetic disabled-field events"
    );
    assert!(!rendered_text(&dom).contains("Saving provider settings"));
    state
        .environments()
        .write()
        .records
        .get_mut(&destination)
        .unwrap()
        .session = saved;
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    click_control(&mut dom, "Delete instance");
    drive_until(&mut dom, state, "provider-instance-remove", || {
        state.config().peek()["settings"]["providerInstances"]
            .get("work_local")
            .is_none()
    })
    .await;
    assert!(
        serde_json::to_value(service.snapshot().await.unwrap()).unwrap()["providerInstances"]
            .get("other_local")
            .is_some()
    );
    drop(dom);
    server.abort();
    let _ = server.await;
    service.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_provider_settings_same_destination_replacement_before_first_poll_releases_saving() {
    let directory = tempfile::tempdir().unwrap();
    let (api, service) = provider_settings_fixture(&directory).await;
    let credential = api
        .auth
        .create_pairing_credential(
            &[
                AuthEnvironmentScope::OrchestrationRead,
                AuthEnvironmentScope::ProvidersManage,
            ],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;
    let props = Harness {
        state: Rc::new(RefCell::new(None)),
        transport: TransportHandle::default(),
        address,
        credential,
        initial: UiModel::default(),
    };
    let mut dom = VirtualDom::new_with_props(harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    drive_until(&mut dom, state, "provider-settings-config", || {
        state.peek().typed_config.is_some()
    })
    .await;
    state.view().set(View::Providers);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    click_control(&mut dom, "Select provider Work");
    input_control(&mut dom, "Executable", "never-send-old-edit");
    blur_control(&mut dom, "Executable", false);
    props.transport.borrow_mut().socket_generation += 1;
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(!rendered_text(&dom).contains("Saving provider settings"));
    assert_ne!(
        control_attribute(&dom, "Executable", "disabled")
            .unwrap()
            .1
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        serde_json::to_value(service.snapshot().await.unwrap()).unwrap()["providerInstances"]["work_local"]
            ["config"]["commandPath"],
        "fixture-command"
    );
    input_control(&mut dom, "Executable", "new-connection-edit");
    blur_control(&mut dom, "Executable", true);
    drive_until(&mut dom,state,"provider-settings-new-owner",||state.config().peek()["settings"]["providerInstances"]["work_local"]["config"]["commandPath"]=="new-connection-edit").await;
    drop(dom);
    server.abort();
    let _ = server.await;
    service.shutdown().await;
}

struct HeldProviderSettingsWrite {
    armed: std::sync::atomic::AtomicBool,
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    released: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
}
impl t3_server::server_settings::SettingsWriter for HeldProviderSettingsWrite {
    fn write(&self, path: &std::path::Path, contents: &str) -> std::io::Result<()> {
        use t3_server::server_settings::SettingsWriter;
        t3_server::server_settings::AtomicSettingsWriter.write(path, contents)?;
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            let held = self.released.lock().unwrap();
            drop(self.wake.wait_while(held, |released| !*released).unwrap());
        }
        Ok(())
    }
}
struct ReleaseProviderWrite(std::sync::Arc<HeldProviderSettingsWrite>);
impl Drop for ReleaseProviderWrite {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}
#[tokio::test(flavor = "current_thread")]
async fn native_provider_settings_same_destination_replacement_during_receipt_releases_saving() {
    use t3_server::{
        server_secret_store::ServerSecretStore,
        server_settings::{SettingsOptions, SettingsService},
    };
    let directory = tempfile::tempdir().unwrap();
    let (mut api, old) = provider_settings_fixture(&directory).await;
    old.shutdown().await;
    let (entered, mut milestone) = tokio::sync::oneshot::channel();
    let writer = std::sync::Arc::new(HeldProviderSettingsWrite {
        armed: false.into(),
        entered: std::sync::Mutex::new(Some(entered)),
        released: std::sync::Mutex::new(false),
        wake: std::sync::Condvar::new(),
    });
    let release = ReleaseProviderWrite(writer.clone());
    let mut options = SettingsOptions::file(
        directory.path().join("settings.json"),
        ServerSecretStore::open(directory.path().join("secrets")).unwrap(),
    );
    options.watch = false;
    options.writer = writer.clone();
    let service = SettingsService::start(options).await.unwrap();
    api.settings = Some(service.clone());
    let credential = api
        .auth
        .create_pairing_credential(
            &[
                AuthEnvironmentScope::OrchestrationRead,
                AuthEnvironmentScope::ProvidersManage,
            ],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;
    let props = Harness {
        state: Rc::new(RefCell::new(None)),
        transport: TransportHandle::default(),
        address,
        credential,
        initial: UiModel::default(),
    };
    let mut dom = VirtualDom::new_with_props(harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    drive_until(&mut dom, state, "provider-settings-config", || {
        state.peek().typed_config.is_some()
    })
    .await;
    state.view().set(View::Providers);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    click_control(&mut dom, "Select provider Work");
    input_control(&mut dom, "Executable", "committed-old-connection");
    writer
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    blur_control(&mut dom, "Executable", true);
    tokio::time::timeout(std::time::Duration::from_secs(8),async {
        loop {
            tokio::select! {
                result=&mut milestone=>{result.unwrap();break;},
                _=dom.wait_for_work()=>dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations),
            }
        }
    }).await.expect("actual durable write accepted with receipt held");
    assert!(rendered_text(&dom).contains("Saving provider settings"));
    {
        let mut transport = props.transport.borrow_mut();
        transport.socket_generation += 1;
        transport.fail_waiters();
    }
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            if !rendered_text(&dom).contains("Saving provider settings") {
                break;
            }
            dom.wait_for_work().await;
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
    })
    .await
    .expect("old owner cancellation releases mounted controls");
    assert_eq!(
        control_attribute(&dom, "Executable", "disabled")
            .unwrap()
            .1
            .as_deref(),
        Some("false")
    );
    assert_eq!(
        state.config().peek()["settings"]["providerInstances"]["work_local"]["config"]["commandPath"],
        "fixture-command",
        "old receipt does not paint new connection state"
    );
    assert!(!rendered_text(&dom).contains("Connection closed before the server replied"));
    drop(release);
    let stored = serde_json::to_value(service.snapshot().await.unwrap()).unwrap();
    assert_eq!(
        stored["providerInstances"]["work_local"]["config"]["commandPath"],
        "committed-old-connection",
        "accepted server mutation remains durable independently of client owner"
    );
    input_control(&mut dom, "Executable", "new-owner-after-receipt");
    blur_control(&mut dom, "Executable", true);
    drive_until(&mut dom,state,"provider-settings-new-owner-after-receipt",||state.config().peek()["settings"]["providerInstances"]["work_local"]["config"]["commandPath"]=="new-owner-after-receipt").await;
    drop(dom);
    server.abort();
    let _ = server.await;
    service.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_provider_settings_subscription_refreshes_delayed_registry_and_fences_invalid_or_replaced_delivery()
 {
    let directory = tempfile::tempdir().unwrap();
    let (mut api, service) = provider_settings_fixture(&directory).await;
    let missing = directory.path().join("missing-codex-fixture");
    let mut settings = serde_json::to_value(service.snapshot().await.unwrap()).unwrap();
    settings["providerInstances"]["codex"] = json!({"driver":"codex","enabled":true,"displayName":"Live status","config":{"binaryPath":missing}});
    service
        .update(
            serde_json::from_value(json!({"providerInstances":settings["providerInstances"]}))
                .unwrap(),
        )
        .await
        .unwrap();
    let native = t3_server::config::NativeConfig::from_settings(
        service.snapshot().await.unwrap(),
        directory.path(),
        directory.path(),
        &api.environment,
        &api.auth.descriptor(),
    )
    .await
    .unwrap();
    let old_snapshot = native.snapshot.clone();
    let registry = native.providers.clone();
    api.providers = Some(native.providers);
    api.config = Some(native.snapshot);
    let credential = api
        .auth
        .create_pairing_credential(
            &[
                AuthEnvironmentScope::OrchestrationRead,
                AuthEnvironmentScope::ProvidersManage,
            ],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;
    let props = Harness {
        state: Rc::new(RefCell::new(None)),
        transport: TransportHandle::default(),
        address,
        credential,
        initial: UiModel::default(),
    };
    let mut dom = VirtualDom::new_with_props(harness, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    drive_until(&mut dom, state, "live-config-initial", || {
        live_config_active(&props.transport, state)
    })
    .await;
    let status = || {
        state.config().peek()["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["instanceId"] == "codex")
            .unwrap()["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(status(), "error");
    state.view().set(View::Providers);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    click_control(&mut dom, "Select provider Live status");
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../server/tests/fixtures/codex-provider.py")
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    input_control(&mut dom, "Binary path", &binary);
    blur_control(&mut dom, "Binary path", true);
    drive_until(
        &mut dom,
        state,
        "live-config-settings-before-discovery",
        || {
            state.config().peek()["settings"]["providerInstances"]["codex"]["config"]["binaryPath"]
                == binary
        },
    )
    .await;
    assert_eq!(
        status(),
        "error",
        "the durable setting receipt precedes actual provider discovery"
    );
    // Drive the actual registry only after the saved settings are visible. The
    // stream's debounced status must update this already mounted panel, without
    // another getConfig, navigation or reload.
    registry
        .reconfigure(&service.snapshot().await.unwrap(), directory.path())
        .await
        .unwrap();
    drive_until(
        &mut dom,
        state,
        "live-config-delayed-provider-ready",
        || status() == "ready",
    )
    .await;
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let visible = rendered_text(&dom);
    assert!(visible.contains("ready"));
    assert!(!visible.contains("This provider has not been checked."));
    assert!(!visible.contains("provider probe failed"));
    apply_rpc_event(
        &props.transport,
        state,
        RpcEvent::Complete {
            id: "late-unary".into(),
            method: "server.getConfig".into(),
            value: old_snapshot,
        },
    );
    assert_eq!(
        status(),
        "ready",
        "a late unary snapshot cannot roll back the live stream"
    );
    let before = state.config().peek().clone();
    let id = props.transport.borrow().config_request_id.clone().unwrap();
    let mut altered = before["settings"].clone();
    altered["responseStreamingMode"] = json!("paragraph");
    let malformed = json!({"version":1,"type":"settingsUpdated","payload":{"settings":{"providers":{"codex":{"enabled":"invalid"}}}}});
    assert!(
        serde_json::from_value::<t3_contracts::ServerConfigStreamEvent>(malformed.clone()).is_err()
    );
    apply_rpc_event(
        &props.transport,
        state,
        RpcEvent::Values {
            id,
            method: "subscribeServerConfig".into(),
            values: vec![
                json!({"version":1,"type":"settingsUpdated","payload":{"settings":altered}}),
                malformed,
            ],
        },
    );
    drive_until(&mut dom, state, "live-config-malformed-chunk", || {
        props.transport.borrow().config_subscription.is_none()
    })
    .await;
    assert_eq!(
        *state.config().peek(),
        before,
        "malformed complete chunk must not apply its valid prefix"
    );
    assert!(
        state
            .error()
            .peek()
            .as_ref()
            .is_some_and(|message| message.starts_with("Invalid server configuration update:"))
    );
    dom.in_scope(ScopeId::APP, || {
        start_config_subscription(&props.transport, state)
    });
    drive_until(&mut dom, state, "live-config-resubscribe", || {
        live_config_active(&props.transport, state)
    })
    .await;
    let id = props.transport.borrow().config_request_id.clone().unwrap();
    apply_rpc_event(
        &props.transport,
        state,
        RpcEvent::Values {
            id,
            method: "subscribeServerConfig".into(),
            values: vec![
                json!({"version":1,"type":"settingsUpdated","payload":{"settings":altered}}),
            ],
        },
    );
    // A chunk is admitted but not polled. Replacing the same destination's
    // socket must fence it, and the old guard must not clear the new owner.
    {
        let mut transport = props.transport.borrow_mut();
        transport.socket_generation += 1;
        transport.fail_waiters();
    }
    dom.in_scope(ScopeId::APP, || {
        start_config_subscription(&props.transport, state)
    });
    drive_until(&mut dom, state, "live-config-replacement", || {
        live_config_active(&props.transport, state)
    })
    .await;
    assert_eq!(
        state.config().peek()["settings"]["responseStreamingMode"],
        before["settings"]["responseStreamingMode"]
    );
    assert_eq!(
        props.transport.borrow().config_subscription,
        connection_owner(&props.transport, state)
    );
    let before = state.config().peek().clone();
    let mut wrong = before.clone();
    wrong["environment"]["environmentId"] = json!("another-environment");
    let id = props.transport.borrow().config_request_id.clone().unwrap();
    apply_rpc_event(
        &props.transport,
        state,
        RpcEvent::Values {
            id,
            method: "subscribeServerConfig".into(),
            values: vec![
                json!({"version":1,"type":"providerStatuses","payload":{"providers":[]}}),
                json!({"version":1,"type":"snapshot","config":wrong}),
            ],
        },
    );
    drive_until(&mut dom, state, "live-config-identity-blocked", || {
        matches!(*state.status().peek(), ConnectionStatus::Blocked(_))
    })
    .await;
    assert_eq!(
        *state.config().peek(),
        before,
        "identity failure must not apply a preceding valid update"
    );
    assert!(props.transport.borrow().sender.is_none());
    assert!(request(&props.transport,state,"server.updateSettings",json!({"patch":{},"providerInstanceMutation":{"operation":"remove","instanceId":"codex"}}),RequestKind::Unary).is_none());
    assert!(
        service
            .snapshot()
            .await
            .unwrap()
            .provider_instances
            .contains_key(&"codex".parse().unwrap())
    );
    drop(dom);
    server.abort();
    let _ = server.await;
    service.shutdown().await;
}
