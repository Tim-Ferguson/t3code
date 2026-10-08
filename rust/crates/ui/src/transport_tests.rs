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
    let rpc_record = directory.path().join("provider-rpc.jsonl");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(
        json!({"providerInstances":{"codex":{"driver":"codex","config":{"binaryPath":binary},"environment":[{"name":"FIXTURE_RPC_RECORD","value":rpc_record}]}}}),
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

// Find the mounted input/button by its accessible label, then feed a platform
// event through Dioxus's normal listener conversion and reactive scheduler.
// This verifies actual control state and native RPC behavior, not static markup.
fn control(
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
fn change_control(dom: &mut VirtualDom, label: &str, value: &str) {
    form_control_event(dom, label, value, "change");
}
fn input_control(dom: &mut VirtualDom, label: &str, value: &str) {
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
fn click_control(dom: &mut VirtualDom, label: &str) {
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
fn rendered_text(dom: &VirtualDom) -> String {
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
async fn wait_for_rendered_text(dom: &mut VirtualDom, expected: &str) {
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
