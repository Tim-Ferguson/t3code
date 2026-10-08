//! In-process integration through the real native HTTP/Effect WebSocket server.
//! No browser, provider account, live T3 state, or persistent server is used.
use super::*;
use std::{cell::RefCell, rc::Rc, time::Duration};
use t3_server::{
    auth::AuthService,
    persistence::Store as EventStore,
    transport::{ApiState, router},
};

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
        cors_origins: None,
        assets: None,
        providers: None,
        execution: None,
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
    let settings: t3_contracts::ServerSettings = serde_json::from_value(
        json!({"providerInstances":{"codex":{"driver":"codex","config":{"binaryPath":binary}}}}),
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
    dom.in_scope(ScopeId::APP, || {
        launch_thread(
            &props.transport,
            state,
            "provider-project",
            "codex",
            "fixture-model",
        )
    })
    .unwrap();
    drive_until(&mut dom, state, "user-facing-provider-launch", || {
        state.peek().active_thread.is_some() && state.peek().thread.synchronized
    })
    .await;
    let thread_id = state.peek().active_thread.clone().unwrap();
    assert_eq!(
        state.peek().thread.projection.as_ref().unwrap()["thread"]["modelSelection"]["model"],
        "fixture-model"
    );

    state.draft().set("Native streamed prompt".into());
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    drive_until(&mut dom, state, "native-provider-completion", || {
        completed_runs(state) == 1
    })
    .await;
    assert!(state.peek().pending_messages.is_empty());
    assert!(state.peek().draft.is_empty());
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
                    && row["item"]["output"] == "fixture tool output\n"
                    && row["item"]["exitCode"] == 0)
        );
    }

    state.draft().set("Native [approval]".into());
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
    let destination = state.peek().destination.clone().unwrap();
    dom.in_scope(ScopeId::APP, || {
        forget_environment(&props.transport, state, &destination)
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
