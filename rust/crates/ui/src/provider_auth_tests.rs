//! Mounted sign-in controls using isolated native Effect RPCs and real ACP/PTY
//! processes. The injected document is only the local popup/emulator ABI seam.
use super::*;
use crate::runtime::transport_tests::{
    change_control, click_control, control, input_control, provider_settings_fixture,
    rendered_text, start_fixture,
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll, Waker},
};
use t3_contracts::AuthEnvironmentScope;
use t3_server::provider_auth_service::ProviderAuthService;
#[derive(Default)]
struct Browser {
    popups: Vec<bool>,
    terminal_events: VecDeque<Value>,
    waker: Option<Waker>,
    output: String,
    disposed: bool,
    mounts: usize,
    changed: Option<tokio::sync::watch::Sender<u64>>,
}
impl Browser {
    fn event(&mut self, event: Value) {
        self.terminal_events.push_back(event);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}
struct Eval {
    browser: Rc<RefCell<Browser>>,
    kind: &'static str,
}
impl document::Evaluator for Eval {
    fn send(&self, value: Value) -> Result<(), document::EvalError> {
        let mut browser = self.browser.borrow_mut();
        match self.kind {
            "popup" => browser.popups.push(value == true),
            "terminal" => match value["type"].as_str() {
                Some("reset") => browser.output = value["data"].as_str().unwrap_or_default().into(),
                Some("append") => browser
                    .output
                    .push_str(value["data"].as_str().unwrap_or_default()),
                Some("dispose") => browser.disposed = true,
                _ => {}
            },
            _ => {}
        }
        if let Some(changed) = &browser.changed {
            changed.send_modify(|value| *value += 1);
        }
        Ok(())
    }
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Value, document::EvalError>> {
        let mut browser = self.browser.borrow_mut();
        if self.kind == "terminal" {
            if let Some(value) = browser.terminal_events.pop_front() {
                return Poll::Ready(Ok(value));
            }
            browser.waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
    fn poll_join(&mut self, _: &mut Context<'_>) -> Poll<Result<Value, document::EvalError>> {
        if self.kind == "clipboard" {
            Poll::Ready(Ok(json!(true)))
        } else {
            Poll::Pending
        }
    }
}
struct BrowserDocument {
    owner: dioxus::signals::Owner,
    browser: Rc<RefCell<Browser>>,
}
impl document::Document for BrowserDocument {
    fn eval(&self, script: String) -> document::Eval {
        let kind = if script.contains("const registry = (window.__t3RustTerminals") {
            let mut b = self.browser.borrow_mut();
            b.mounts += 1;
            b.disposed = false;
            b.event(json!({"type":"ready"}));
            "terminal"
        } else if script.contains("DOM activation only. Rust owns consent") {
            "popup"
        } else if script.contains("navigator.clipboard.writeText") {
            "clipboard"
        } else {
            "other"
        };
        document::Eval::new(self.owner.insert(Box::new(Eval {
            browser: self.browser.clone(),
            kind,
        }) as Box<dyn document::Evaluator>))
    }
}
#[derive(Clone)]
struct Props {
    state: Rc<RefCell<Option<Store<UiModel>>>>,
    transport: TransportHandle,
    address: String,
    credential: String,
    show: Rc<RefCell<Option<Signal<bool>>>>,
}
fn root(props: Props) -> Element {
    let state = use_store(UiModel::default);
    *props.state.borrow_mut() = Some(state);
    let mut show = use_signal(|| true);
    *props.show.borrow_mut() = Some(show);
    let transport = props.transport.clone();
    use_future(move || {
        let transport = transport.clone();
        let address = props.address.clone();
        let credential = props.credential.clone();
        async move {
            pair_and_connect(transport, state, address, credential).await;
        }
    });
    let provider = state.config().read()["providers"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["instanceId"] == "agent"))
        .cloned();
    let status = state.status().read().clone();
    let dest = state.destination().read().clone();
    let allowed = status == t3_client::connection::ConnectionStatus::Connected
        && dest.as_ref().is_some_and(|id| {
            state
                .environments()
                .read()
                .allows(id, AuthEnvironmentScope::ProvidersManage)
        });
    rsx! {button {"aria-label":"Hide auth",onclick:move |_|show.set(false),"Hide"}if *show.read(){if let Some(provider)=provider{crate::provider_auth::Authentication{state,transport:props.transport,provider,environment:"Fixture environment",allowed}}}}
}
struct Fixture {
    directory: tempfile::TempDir,
    service: ProviderAuthService,
    settings: t3_server::server_settings::SettingsService,
    server: tokio::task::JoinHandle<()>,
    dom: VirtualDom,
    props: Props,
    browser: Rc<RefCell<Browser>>,
    probe: crate::provider_auth::AuthProbe,
    changed: tokio::sync::watch::Receiver<u64>,
}
impl Fixture {
    async fn new() -> Self {
        Self::configured(false).await
    }
    async fn configured(token: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let (mut api, settings) = provider_settings_fixture(&directory).await;
        let log = directory.path().join("auth.log");
        let mut instances = json!({"codex":{"driver":"codex","enabled":false},"agent":{"driver":"acpRegistry","enabled":true,"displayName":"Fixture agent","config":{"source":"local","commandPath":"python3","commandArgs":[format!("{}/../server/tests/fixtures/acp-auth-provider.py",env!("CARGO_MANIFEST_DIR")),log,"2"]}}});
        if token {
            instances["agent"]["environment"] =
                json!([{"name":"FIXTURE_TOKEN","value":"configured-only","sensitive":true}]);
        }
        settings
            .update(serde_json::from_value(json!({"providerInstances":instances})).unwrap())
            .await
            .unwrap();
        let native = t3_server::config::NativeConfig::from_settings(
            settings.snapshot().await.unwrap(),
            directory.path(),
            directory.path(),
            &api.environment,
            &api.auth.descriptor(),
        )
        .await
        .unwrap();
        let service = ProviderAuthService::new(
            native.providers.clone(),
            directory.path().into(),
            directory.path().join("caches"),
            Arc::new(|_| Box::pin(async { Ok(()) })),
        );
        api.providers = Some(native.providers);
        api.config = Some(native.snapshot);
        api.provider_auth = Some(service.clone());
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
        let props = Props {
            state: Default::default(),
            transport: Default::default(),
            address,
            credential,
            show: Default::default(),
        };
        let (changes, changed) = tokio::sync::watch::channel(0u64);
        let browser = Rc::new(RefCell::new(Browser {
            changed: Some(changes),
            ..Default::default()
        }));
        let mut dom = VirtualDom::new_with_props(root, props.clone());
        let probe = crate::provider_auth::AuthProbe::default();
        dom.provide_root_context(probe.clone());
        dom.provide_root_context(Rc::new(BrowserDocument {
            owner: Default::default(),
            browser: browser.clone(),
        }) as Rc<dyn document::Document>);
        dom.rebuild_in_place();
        let mut fixture = Self {
            directory,
            service,
            settings,
            server,
            dom,
            props,
            browser,
            probe,
            changed,
        };
        fixture
            .until("methods", |dom| control(dom, "Sign-in method").is_some())
            .await;
        fixture
    }
    async fn until(&mut self, label: &str, predicate: impl Fn(&VirtualDom) -> bool) {
        let dom = &mut self.dom;
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
                if predicate(dom) {
                    return;
                }
                tokio::select! {_=dom.wait_for_work()=>{},_=self.changed.changed()=>{}}
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "auth stage {label}; rendered={}; terminal_output={:?}",
                rendered_text(dom),
                self.browser.borrow().output
            )
        });
    }
    fn event(&mut self, label: &str, event: &str) {
        dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
        let (element, _) = control(&self.dom, label).unwrap_or_else(|| panic!("missing {label}"));
        let payload = dioxus_html::SerializedFormData::new(String::new(), vec![]);
        self.dom.runtime().handle_event(
            event,
            dioxus::dioxus_core::Event::new(
                Rc::new(dioxus_html::PlatformEventData::new(Box::new(payload)))
                    as Rc<dyn std::any::Any>,
                true,
            ),
            element,
        );
        self.dom
            .render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    }
    async fn stop(self) {
        drop(self.dom);
        self.service.shutdown().await;
        self.settings.shutdown().await;
        self.server.abort();
        let _ = self.server.await;
    }
    fn state(&self) -> Store<UiModel> {
        self.props.state.borrow().unwrap()
    }
}
#[tokio::test(flavor = "current_thread")]
async fn mounted_provider_auth_credentials_browser_consent_cancel_and_logout_use_advertised_methods()
 {
    let mut f = Fixture::new().await;
    change_control(&mut f.dom, "Sign-in method", "credentials");
    click_control(&mut f.dom, "Sign in");
    f.until("missing configured token", |dom| {
        rendered_text(dom).contains("The provider could not create a session after sign-in.")
    })
    .await;
    assert!(
        control(&f.dom, "FIXTURE_TOKEN").is_none(),
        "environment methods verify configured values, never invent a credential prompt"
    );
    change_control(&mut f.dom, "Sign-in method", "agent");
    click_control(&mut f.dom, "Retry sign-in");
    f.until("browser consent", |dom| {
        control(dom, "Open browser").is_some()
    })
    .await;
    click_control(&mut f.dom, "Open browser");
    f.until("browser signed in", |dom| {
        rendered_text(dom).contains("Signed in.")
    })
    .await;
    assert_eq!(
        f.browser.borrow().popups,
        vec![true],
        "consent is accepted by server before popup navigation"
    );
    assert!(f.directory.path().join("auth.log.credentials").exists());
    click_control(&mut f.dom, "Sign out");
    assert!(rendered_text(&f.dom).contains("Thread history is kept."));
    click_control(&mut f.dom, "Cancel sign out");
    assert!(rendered_text(&f.dom).contains("Signed in."));
    click_control(&mut f.dom, "Sign out");
    click_control(&mut f.dom, "Confirm sign out");
    f.until("second logout", |dom| control(dom, "Sign in").is_some())
        .await;
    change_control(&mut f.dom, "Sign-in method", "agent");
    click_control(&mut f.dom, "Sign in");
    f.until("cancel ready", |dom| {
        control(dom, "Cancel sign-in").is_some()
    })
    .await;
    click_control(&mut f.dom, "Cancel sign-in");
    f.until("cancelled", |dom| control(dom, "Retry sign-in").is_some())
        .await;
    assert!(!f.directory.path().join("auth.log.credentials").exists());
    f.stop().await;
}
#[tokio::test(flavor = "current_thread")]
async fn mounted_provider_auth_terminal_owns_serialized_pty_input_and_disposes_on_navigation() {
    let mut f = Fixture::new().await;
    change_control(&mut f.dom, "Sign-in method", "terminal");
    click_control(&mut f.dom, "Sign in");
    f.until("terminal", |dom| {
        control(dom, "Provider sign-in terminal").is_some()
    })
    .await;
    f.event("Provider sign-in terminal", "mounted");
    let browser = f.browser.clone();
    f.until("terminal mount", move |_| browser.borrow().mounts == 1)
        .await;
    let browser = f.browser.clone();
    f.until("terminal prompt", move |_| {
        browser.borrow().output.contains("Enter fixture code:")
    })
    .await;
    f.browser
        .borrow_mut()
        .event(json!({"type":"resize","cols":100,"rows":20}));
    f.browser
        .borrow_mut()
        .event(json!({"type":"write","data":"fixture-"}));
    f.browser
        .borrow_mut()
        .event(json!({"type":"write","data":"code\n"}));
    f.until("terminal signed in", |dom| {
        rendered_text(dom).contains("Signed in.")
    })
    .await;
    assert!(f.directory.path().join("auth.log.credentials").exists());
    assert!(
        f.browser.borrow().disposed,
        "successful flow unmount disposes engine"
    );
    f.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn mounted_provider_auth_configured_environment_verifies_without_a_credential_editor() {
    let mut f = Fixture::configured(true).await;
    change_control(&mut f.dom, "Sign-in method", "credentials");
    click_control(&mut f.dom, "Sign in");
    f.until("configured sign-in", |dom| {
        rendered_text(dom).contains("Signed in.")
    })
    .await;
    assert!(control(&f.dom, "FIXTURE_TOKEN").is_none());
    let rows = std::fs::read_to_string(f.directory.path().join("auth.log")).unwrap();
    assert!(!rows.lines().any(|line| {
        let value: Value = serde_json::from_str(line).unwrap();
        value["method"] == "auth/login" || value["method"] == "authenticate"
    }));
    f.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn mounted_provider_auth_typed_credentials_reset_between_interactions_and_revocation_blocks_dispatch()
 {
    let mut f = Fixture::new().await;
    let state = f.state();
    let id = f
        .probe
        .stream_id
        .borrow()
        .clone()
        .expect("actual provider auth RPC ID");
    let make = |interaction: &str| json!({"instanceId":"agent","phase":"waiting","flowId":"typed-future-driver","authorizationUrl":null,"expiresAt":null,"message":null,"interaction":{"type":"credentials","id":interaction,"fields":[{"name":"TOKEN","label":"Secret token","secret":true}]}});
    let _validated: t3_contracts::ProviderAuthState = serde_json::from_value(make("one")).unwrap();
    apply_rpc_event(
        &f.props.transport,
        state,
        RpcEvent::Values {
            id: id.clone(),
            method: "provider.auth.subscribe".into(),
            values: vec![make("one")],
        },
    );
    f.until("typed credential input", |dom| {
        control(dom, "Secret token").is_some()
    })
    .await;
    input_control(&mut f.dom, "Secret token", "ephemeral-one");
    f.until("typed first edit", |dom| {
        control(dom, "Secret token").unwrap().1.as_deref() == Some("ephemeral-one")
    })
    .await;
    apply_rpc_event(
        &f.props.transport,
        state,
        RpcEvent::Values {
            id: id.clone(),
            method: "provider.auth.subscribe".into(),
            values: vec![make("two")],
        },
    );
    f.until("second interaction blank", |dom| {
        control(dom, "Secret token").is_some_and(|(_, value)| value.as_deref() == Some(""))
    })
    .await;
    input_control(&mut f.dom, "Secret token", "ephemeral-two");
    let before = std::fs::read_to_string(f.directory.path().join("auth.log")).unwrap();
    let destination = state.peek().destination.clone().unwrap();
    state
        .environments()
        .write()
        .records
        .get_mut(&destination)
        .unwrap()
        .session
        .scopes = Some(vec![AuthEnvironmentScope::OrchestrationRead]);
    state
        .environments()
        .write()
        .records
        .get_mut(&destination)
        .unwrap()
        .session
        .permissions = Some(vec![AuthEnvironmentScope::OrchestrationRead]);
    f.event("Provider credentials", "submit");
    f.until("revoked secret editor", |dom| {
        control(dom, "Secret token").is_none()
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(f.directory.path().join("auth.log")).unwrap(),
        before,
        "revocation before first poll performs no provider work"
    );
    assert!(f.props.transport.borrow().unary_waiters.is_empty());
    assert!(
        !format!("{:?}", state.peek()).contains("ephemeral-"),
        "secret drafts do not enter persistent UiModel"
    );
    f.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn mounted_provider_auth_socket_replacement_before_first_poll_closes_reserved_popup_and_resubscribes()
 {
    let mut f = Fixture::new().await;
    change_control(&mut f.dom, "Sign-in method", "agent");
    click_control(&mut f.dom, "Sign in");
    f.until("browser before replacement", |dom| {
        control(dom, "Open browser").is_some()
    })
    .await;
    let state = f.state();
    let old = f.probe.subscriptions.get();
    let token = f.props.transport.borrow().bearer_token.clone();
    // Admit the click without driving its spawned mutation. Replacing the socket
    // now must close the preopened popup before any consent RPC can be sent.
    let (element, _) = control(&f.dom, "Open browser").unwrap();
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let mouse: dioxus_html::SerializedMouseData = serde_json::from_value(
        serde_json::to_value(dioxus_html::point_interaction::SerializedPointInteraction::default())
            .unwrap(),
    )
    .unwrap();
    f.dom.runtime().handle_event(
        "click",
        dioxus::dioxus_core::Event::new(
            Rc::new(dioxus_html::PlatformEventData::new(Box::new(mouse))) as Rc<dyn std::any::Any>,
            true,
        ),
        element,
    );
    {
        let mut transport = f.props.transport.borrow_mut();
        transport.generation += 1;
        transport.socket_generation += 1;
        transport.fail_waiters();
    }
    state
        .status()
        .set(t3_client::connection::ConnectionStatus::Disconnected);
    let browser = f.browser.clone();
    f.until("abandoned popup closed", move |_| {
        browser.borrow().popups == [false]
    })
    .await;
    assert!(
        !f.directory.path().join("auth.log.credentials").exists(),
        "abandoned consent cannot authenticate"
    );
    let transport = f.props.transport.clone();
    let address = f.props.address.clone();
    f.dom.in_scope(ScopeId::APP, || {
        spawn(async move { connect(transport, state, address, token).await });
    });
    let probe = f.probe.clone();
    f.until("replacement subscription", move |dom| {
        // A fresh RpcSession may reuse the old request ID. Observe actual new
        // subscription admission, not an ID inequality across connections.
        probe.subscriptions.get() > old && control(dom, "Open browser").is_some()
    })
    .await;
    click_control(&mut f.dom, "Open browser");
    f.until("new owner consent", |dom| {
        rendered_text(dom).contains("Signed in.")
    })
    .await;
    assert_eq!(f.browser.borrow().popups, vec![false, true]);
    f.stop().await;
}
