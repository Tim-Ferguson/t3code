//! Several environments connected at once, against two real in-process servers: both stay connected and keep their shell stream live
//! while the window is on one of them, what arrives for the other is applied to its own state and never to the one on screen,
//! switching loses nothing, a reply is never taken for another connection's, and ending one connection leaves the other as it was.
use super::diff_tests::until;
use super::transport_tests::{api_fixture, click_control, control, control_attribute, drive_future, rendered_text, start_fixture};
use super::fixture_server::Server;
use super::*;
use t3_server::persistence::Store as EventStore;

struct Remote {
    id: EnvironmentId,
    address: String,
    credential: String,
    token: String,
    directory: tempfile::TempDir,
    _store: EventStore,
    server: Server,
}
impl Remote {
    async fn start(environment: &str, label: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut api = api_fixture(&directory, environment);
        api.environment["label"] = json!(label);
        api.config.as_mut().unwrap()["environment"]["label"] = json!(label);
        let scopes = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];
        let credential = api.auth.create_pairing_credential(&scopes, chrono::Utc::now(), chrono::Duration::minutes(5)).unwrap();
        let token = api.auth.issue_session("test", "bearer-access-token", scopes.to_vec(), json!({"deviceType":"unknown"}), chrono::Utc::now(), chrono::Duration::hours(1)).unwrap().1;
        let store = api.store.clone();
        let (address, server) = start_fixture(api).await;
        Self { id: EnvironmentId::new(environment).unwrap(), address, credential, token, directory, _store: store, server }
    }
    /// A project in a folder of its own: two projects do not share a workspace root.
    fn project(&self, id: &str, title: &str) -> Value {
        let root = self.directory.path().join(id);
        std::fs::create_dir_all(&root).unwrap();
        json!({"type":"project.create","commandId":format!("create-{id}"),"projectId":id,"title":title,"workspaceRoot":root.to_string_lossy()})
    }
}

#[derive(Clone)]
struct App {
    state: Rc<RefCell<Option<Store<UiModel>>>>,
    transport: TransportHandle,
    address: String,
    credential: String,
}
fn app(props: App) -> Element {
    let state = use_store(|| UiModel { view: View::Connections, ..UiModel::default() });
    crate::themes::use_themes(state);
    *props.state.borrow_mut() = Some(state);
    let transport = props.transport.clone();
    use_future(move || {
        let (transport, address, credential) = (transport.clone(), props.address.clone(), props.credential.clone());
        async move { pair_and_connect(transport, state, address, credential).await }
    });
    rsx! { ParkedConsumer { state } crate::Application { state, transport: props.transport } }
}

/// `drive_until` re-checks its predicate when the page has work, and nothing the page draws reads what a parked environment's
/// connection receives (it must not: it would repaint on every frame of an environment nobody is looking at). Reading it here models
/// the consumer every wait on such state has, as `ConfigConsumer` does for the configuration.
#[component]
fn ParkedConsumer(state: Store<UiModel>) -> Element {
    let _ = state.parked().read();
    rsx! {}
}

/// The window on `open`, with `other` connected in the background, both synchronized.
async fn two(open: &Remote, other: &Remote) -> (VirtualDom, App, Store<UiModel>) {
    let props = App { state: Rc::new(RefCell::new(None)), transport: TransportHandle::default(), address: open.address.clone(), credential: open.credential.clone() };
    let mut dom = VirtualDom::new_with_props(app, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    until(&mut dom, "the first environment", |_| state.peek().status == ConnectionStatus::Connected && state.peek().shell.synchronized).await;
    let (transport, address, token) = (props.transport.clone(), other.address.clone(), other.token.clone());
    dom.in_scope(ScopeId::APP, || {
        spawn(async move { connect_in_background(transport, state, address, token).await });
    });
    let other_id = other.id.clone();
    until(&mut dom, "the second environment", |_| state.peek().parked.get(&other_id).is_some_and(|slice| slice.status == ConnectionStatus::Connected && slice.shell.synchronized)).await;
    (dom, props, state)
}

fn titles(shell: &t3_client::shell::ShellState) -> Vec<String> {
    shell.snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| project.title.to_string()).collect()).unwrap_or_default()
}
fn open_titles(state: Store<UiModel>) -> Vec<String> {
    titles(&state.peek().shell)
}
fn parked_titles(state: Store<UiModel>, environment: &EnvironmentId) -> Vec<String> {
    state.peek().parked.get(environment).map(|slice| titles(&slice.shell)).unwrap_or_default()
}

/// A command on the environment's own connection, whichever one the window is on.
fn mutate(dom: &mut VirtualDom, props: &App, state: Store<UiModel>, environment: &EnvironmentId, payload: Value) {
    let link = props.transport.link_of(environment).expect("a connected environment");
    dom.in_scope(ScopeId::APP, || request(&link, state, "projects.mutate", payload, RequestKind::Unary)).expect("accepted");
}

#[tokio::test(flavor = "current_thread")]
async fn a_second_environment_stays_connected_in_the_background_and_each_shell_is_applied_to_its_own_state() {
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, props, state) = two(&a, &b).await;

    // The window is on the first; the second is connected and parked, with its own status and shell.
    assert_eq!(state.peek().destination.as_ref(), Some(&a.id));
    assert_eq!(state.peek().live.get(&a.id), Some(&ConnectionStatus::Connected));
    assert_eq!(state.peek().live.get(&b.id), Some(&ConnectionStatus::Connected));
    assert!(state.peek().grants.authenticated && state.peek().parked[&b.id].grants.authenticated);
    assert!(props.transport.link_of(&a.id).is_some() && props.transport.link_of(&b.id).is_some());

    // Something happens on the second: the window, which is on the first, does not show it, and the second's own state has it.
    mutate(&mut dom, &props, state, &b.id, b.project("project-b", "Beta project"));
    until(&mut dom, "the second's shell", |_| parked_titles(state, &b.id) == ["Beta project"]).await;
    assert_eq!(open_titles(state), Vec::<String>::new(), "nothing of the second leaks into the open environment");

    // And the other way round: the open one's projects go to the window and never to the parked one.
    mutate(&mut dom, &props, state, &a.id, a.project("project-a", "Alpha project"));
    until(&mut dom, "the first's shell", |_| open_titles(state) == ["Alpha project"]).await;
    assert_eq!(parked_titles(state, &b.id), ["Beta project"]);
    assert_eq!(rendered_text(&dom).contains("Beta project"), false);

    // Each connection answers its own requests, even though both number them from one: the configuration that comes back is the
    // environment's own.
    let (first, second) = (props.transport.link_of(&a.id).unwrap(), props.transport.link_of(&b.id).unwrap());
    let (config_a, config_b) = (
        dom.in_scope(ScopeId::APP, || spawn_request(&first, state)),
        dom.in_scope(ScopeId::APP, || spawn_request(&second, state)),
    );
    let (config_a, config_b) = drive_future(&mut dom, "the configurations", async { (config_a.await.unwrap().unwrap(), config_b.await.unwrap().unwrap()) }).await;
    assert_eq!((config_a["environment"]["environmentId"].as_str(), config_b["environment"]["environmentId"].as_str()), (Some("env-a"), Some("env-b")));
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

/// `server.getConfig` on a link, as a future the test waits on while the page runs.
fn spawn_request(link: &TransportHandle, state: Store<UiModel>) -> futures_channel::oneshot::Receiver<Result<Value, String>> {
    let (sender, receiver) = futures_channel::oneshot::channel();
    let link = link.clone();
    spawn(async move {
        let _ = sender.send(request_value(link, state, "server.getConfig", json!({})).await);
    });
    receiver
}

#[tokio::test(flavor = "current_thread")]
async fn switching_the_open_environment_is_instant_and_loses_neither_the_shell_the_thread_nor_the_draft() {
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, props, state) = two(&a, &b).await;
    mutate(&mut dom, &props, state, &a.id, a.project("project-a", "Alpha project"));
    mutate(&mut dom, &props, state, &b.id, b.project("project-b", "Beta project"));
    until(&mut dom, "both shells", |_| open_titles(state) == ["Alpha project"] && parked_titles(state, &b.id) == ["Beta project"]).await;

    // A thread open on the first, with words typed that were not sent.
    let first = props.transport.link_of(&a.id).unwrap();
    dom.in_scope(ScopeId::APP, || command(&first, state, "thread.create", json!({"createdBy":"user","creationSource":"web","threadId":"thread-a","projectId":"project-a","title":"Alpha thread","modelSelection":{"instanceId":"codex","model":"test-provider-model"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}))).unwrap();
    until(&mut dom, "the thread", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.len() == 1)).await;
    dom.in_scope(ScopeId::APP, || select_thread(&props.transport, state, "thread-a".into()));
    until(&mut dom, "the thread to open", |_| state.peek().thread.synchronized).await;
    state.draft().set("words not sent yet".into());

    // The first environment keeps receiving while the window is on the second.
    state.view().set(View::Connections);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    click_control(&mut dom, "Open Beta host");
    assert_eq!(state.peek().destination.as_ref(), Some(&b.id));
    assert_eq!(open_titles(state), ["Beta project"], "the second's shell is on screen at once, with nothing awaited");
    assert_eq!((state.peek().active_thread.clone(), state.peek().draft.clone()), (None, String::new()));
    assert!(state.peek().parked.contains_key(&a.id) && !state.peek().parked.contains_key(&b.id));
    mutate(&mut dom, &props, state, &a.id, a.project("project-a2", "Alpha second"));
    until(&mut dom, "the first's shell while away", |_| parked_titles(state, &a.id) == ["Alpha project", "Alpha second"]).await;
    assert_eq!(open_titles(state), ["Beta project"]);

    // Back on the first, from the sidebar's list of the connected environments: all of it is there as it was left, before anything
    // is asked of the server again.
    assert_eq!(control_attribute(&dom, "Switch to Beta host", "aria-current").and_then(|(_, value)| value).as_deref(), Some("true"));
    click_control(&mut dom, "Switch to Alpha host");
    assert_eq!(state.peek().destination.as_ref(), Some(&a.id));
    assert_eq!(open_titles(state), ["Alpha project", "Alpha second"]);
    assert_eq!((state.peek().active_thread.as_deref(), state.peek().draft.as_str()), (Some("thread-a"), "words not sent yet"));
    assert!(state.peek().thread.projection.is_some(), "the thread is drawn at once from what the window held");
    until(&mut dom, "the thread to follow its server again", |_| state.peek().thread.synchronized).await;
    assert_eq!(parked_titles(state, &b.id), ["Beta project"]);
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn two_environments_with_one_name_are_told_apart_by_their_addresses_in_the_sidebar() {
    let (a, b) = (Remote::start("env-a", "My laptop").await, Remote::start("env-b", "My laptop").await);
    let (mut dom, props, state) = two(&a, &b).await;
    let named = |remote: &Remote| format!("Switch to My laptop ({})", remote.address.trim_start_matches("http://"));
    assert!(control(&dom, &named(&a)).is_some() && control(&dom, &named(&b)).is_some(), "{}", rendered_text(&dom));
    click_control(&mut dom, &named(&b));
    assert_eq!(state.peek().destination.as_ref(), Some(&b.id));
    drop(props);
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_reply_on_one_connection_is_never_applied_to_another_with_the_same_request_number() {
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (dom, props, state) = two(&a, &b).await;
    let (first, second) = (props.transport.link_of(&a.id).unwrap(), props.transport.link_of(&b.id).unwrap());
    // A message in flight on each, under the same request number: the first's is the open environment's, the second's is parked.
    let message = |text: &str, environment: &EnvironmentId| PendingMessage { destination: environment.clone(), thread_id: "thread".into(), text: text.into(), attachments: vec![], contexts: vec![] };
    state.pending_messages().write().insert("7".into(), message("to alpha", &a.id));
    state.parked().write().get_mut(&b.id).unwrap().pending_messages.insert("7".into(), message("to beta", &b.id));

    dom.in_scope(ScopeId::APP, || apply_rpc_event(&second, state, RpcEvent::Complete { id: "7".into(), method: "orchestration.dispatchCommand".into(), value: json!({"sequence":1}) }));
    assert!(state.peek().parked[&b.id].pending_messages.is_empty(), "the second's reply settles the second's message");
    assert_eq!(state.peek().pending_messages.get("7").map(|pending| pending.text.as_str()), Some("to alpha"), "and not the first's");

    dom.in_scope(ScopeId::APP, || apply_rpc_event(&first, state, RpcEvent::Failed { id: "7".into(), method: "orchestration.dispatchCommand".into(), cause: json!({"message":"refused"}) }));
    assert!(state.peek().pending_messages.is_empty());
    assert_eq!(state.peek().error.as_deref(), Some("refused"), "a failure on the open environment is the window's");

    // A failure on the parked one is kept for it, and is not the window's.
    state.error().set(None);
    dom.in_scope(ScopeId::APP, || apply_rpc_event(&second, state, RpcEvent::Failed { id: "9".into(), method: "orchestration.dispatchCommand".into(), cause: json!({"message":"beta refused"}) }));
    assert_eq!(state.peek().error, None);
    assert_eq!(state.peek().parked[&b.id].error.as_deref(), Some("beta refused"));
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn ending_one_connection_leaves_the_other_untouched_and_ending_the_open_one_leaves_the_window_on_nothing() {
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, props, state) = two(&a, &b).await;
    mutate(&mut dom, &props, state, &a.id, a.project("project-a", "Alpha project"));
    mutate(&mut dom, &props, state, &b.id, b.project("project-b", "Beta project"));
    until(&mut dom, "both shells", |_| open_titles(state) == ["Alpha project"] && parked_titles(state, &b.id) == ["Beta project"]).await;

    // The parked one is disconnected: the open one is as it was, and still answers.
    click_control(&mut dom, "Disconnect Beta host");
    assert!(props.transport.link_of(&b.id).is_none() && !state.peek().parked.contains_key(&b.id) && !state.peek().live.contains_key(&b.id));
    assert_eq!((state.peek().status.clone(), open_titles(state)), (ConnectionStatus::Connected, vec!["Alpha project".to_owned()]));
    let first = props.transport.link_of(&a.id).unwrap();
    let answer = dom.in_scope(ScopeId::APP, || spawn_request(&first, state));
    let config = drive_future(&mut dom, "the open environment's answer", async { answer.await.unwrap().unwrap() }).await;
    assert_eq!(config["environment"]["environmentId"], "env-a");
    // It stays known, and can be connected again.
    assert!(control(&dom, "Disconnect Alpha host").is_some() && control(&dom, "Disconnect Beta host").is_none());

    // Ending the open one leaves nothing open; the connection that remains is not touched.
    let (transport, address, token) = (props.transport.clone(), b.address.clone(), b.token.clone());
    dom.in_scope(ScopeId::APP, || {
        spawn(async move { connect_in_background(transport, state, address, token).await });
    });
    let b_id = b.id.clone();
    until(&mut dom, "the second again", |_| state.peek().parked.get(&b_id).is_some_and(|slice| slice.shell.synchronized)).await;
    click_control(&mut dom, "Disconnect Alpha host");
    assert_eq!((state.peek().destination.clone(), state.peek().status.clone()), (None, ConnectionStatus::Disconnected));
    assert!(open_titles(state).is_empty());
    assert_eq!(parked_titles(state, &b.id), ["Beta project"], "the other connection kept what it had");
    click_control(&mut dom, "Open Beta host");
    assert_eq!(open_titles(state), ["Beta project"]);
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

// ---- the supervisor: backoff, routes and retrying now, on a clock the test moves by hand ------------------------------------------

/// The connections' time, moved by hand: `advance` is the only way it passes, so an hour of waiting takes no time, and the waits the
/// supervisor asks for are there to read (a wait ends when the clock reaches it, or when the supervisor is told to retry now).
#[derive(Clone, Default)]
struct Manual(Rc<ManualInner>);
#[derive(Default)]
struct ManualInner {
    now: std::cell::Cell<u64>,
    sleepers: RefCell<Vec<(u64, futures_channel::oneshot::Sender<()>)>>,
    requested: RefCell<std::collections::VecDeque<u64>>,
    asked: tokio::sync::Notify,
}
impl Manual {
    fn clock(&self) -> links::Clock {
        let (for_now, for_sleep) = (self.clone(), self.clone());
        links::Clock {
            now: Rc::new(move || for_now.0.now.get()),
            sleep: Rc::new(move |milliseconds| {
                let (sender, receiver) = futures_channel::oneshot::channel();
                let deadline = for_sleep.0.now.get() + milliseconds;
                for_sleep.0.sleepers.borrow_mut().push((deadline, sender));
                for_sleep.0.requested.borrow_mut().push_back(milliseconds);
                for_sleep.0.asked.notify_one();
                Box::pin(async move {
                    let _ = receiver.await;
                })
            }),
        }
    }
    fn now(&self) -> u64 {
        self.0.now.get()
    }
    fn advance(&self, milliseconds: u64) {
        self.0.now.set(self.0.now.get() + milliseconds);
        let now = self.0.now.get();
        let due: Vec<_> = {
            let mut sleepers = self.0.sleepers.borrow_mut();
            let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut *sleepers).into_iter().partition(|(deadline, _)| *deadline <= now);
            *sleepers = later;
            due
        };
        for (_, sender) in due {
            let _ = sender.send(());
        }
    }
    /// The next wait asked for that `wanted` accepts, once it is asked for; the others stay for whoever wants them.
    async fn wait_asked(&self, wanted: impl Fn(u64) -> bool) -> u64 {
        loop {
            let found = {
                let mut requested = self.0.requested.borrow_mut();
                requested.iter().position(|milliseconds| wanted(*milliseconds)).and_then(|index| requested.remove(index))
            };
            if let Some(milliseconds) = found {
                return milliseconds;
            }
            self.0.asked.notified().await;
        }
    }
    /// The supervisor's wait before an attempt, not the heartbeat's five seconds.
    async fn next_retry(&self) -> u64 {
        self.wait_asked(|milliseconds| milliseconds != 5_000).await
    }
    async fn next_heartbeat(&self) -> u64 {
        self.wait_asked(|milliseconds| milliseconds == 5_000).await
    }
}

/// An address nothing listens on.
fn dead_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    address
}

fn bare() -> (VirtualDom, App, Store<UiModel>) {
    let props = App { state: Rc::new(RefCell::new(None)), transport: TransportHandle::default(), address: String::new(), credential: String::new() };
    fn app(props: App) -> Element {
        let state = use_store(|| UiModel { view: View::Connections, ..UiModel::default() });
        crate::themes::use_themes(state);
        *props.state.borrow_mut() = Some(state);
        rsx! { ParkedConsumer { state } crate::Application { state, transport: props.transport } }
    }
    let mut dom = VirtualDom::new_with_props(app, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    (dom, props, state)
}

#[tokio::test(flavor = "current_thread")]
async fn an_unreachable_server_is_tried_again_on_the_originals_ladder_and_an_hour_of_it_takes_no_time() {
    let manual = Manual::default();
    let (mut dom, props, state) = bare();
    props.transport.set_clock(manual.clock());
    TEST_RANDOM.with(|random| random.set(Some(0.0)));
    let (transport, address) = (props.transport.clone(), dead_address());
    dom.in_scope(ScopeId::APP, || {
        spawn(async move { connect_in_background(transport, state, address, "a-token".into()).await });
    });
    // The wait doubles from one second (the quickest of the first rung) to half of five minutes, and stays there.
    let mut waits = vec![];
    for _ in 0..30 {
        let wait = drive_future(&mut dom, "the next wait", manual.next_retry()).await;
        let link = props.transport.all().pop().unwrap();
        assert_eq!(link.borrow().retry_at, Some(manual.now() + wait), "the link says when it will try again");
        waits.push(wait);
        manual.advance(wait);
    }
    assert_eq!(&waits[..10], [1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 128_000, 150_000, 150_000]);
    assert!(waits[8..].iter().all(|wait| *wait == 150_000));
    assert!(manual.now() > 3_500_000, "about an hour passed: {}", manual.now());

    // Asking to try now does not wait, and starts the ladder over.
    let link = props.transport.all().pop().unwrap();
    let _ = drive_future(&mut dom, "the wait before the request", manual.next_retry()).await;
    retry_link_now(&link);
    let again = drive_future(&mut dom, "the wait after trying now", manual.next_retry()).await;
    assert_eq!(again, 1_000, "the failures before an explicit retry are forgotten");
    // Nothing of this was seen as an environment: it never answered.
    assert!(state.peek().live.is_empty());
    drop(dom);
}

#[tokio::test(flavor = "current_thread")]
async fn a_lost_connection_tries_the_other_routes_before_it_waits_and_comes_back_with_the_window_as_it_was() {
    let manual = Manual::default();
    let a = Remote::start("env-a", "Alpha host").await;
    let (mut dom, props, state) = bare();
    props.transport.set_clock(manual.clock());
    TEST_RANDOM.with(|random| random.set(Some(0.0)));
    // The preferred route does not answer; the other does.
    let routes = vec![Route { address: dead_address(), token: "unused".into() }, Route { address: a.address.clone(), token: a.token.clone() }];
    let transport = props.transport.clone();
    dom.in_scope(ScopeId::APP, || {
        spawn(async move { connect_routes_in_background(transport, state, routes, None).await });
    });
    until(&mut dom, "the connection over the second route", |_| state.peek().parked.get(&a.id).is_some_and(|slice| slice.status == ConnectionStatus::Connected && slice.shell.synchronized)).await;
    assert!(manual.0.requested.borrow().iter().all(|wait| *wait == 5_000), "the second route was tried at once, with no wait: {:?}", manual.0.requested.borrow());
    // Something for the window to keep while the connection is gone.
    let link = props.transport.link_of(&a.id).unwrap();
    dom.in_scope(ScopeId::APP, || request(&link, state, "projects.mutate", a.project("project-a", "Alpha project"), RequestKind::Unary)).unwrap();
    until(&mut dom, "the project", |_| parked_titles(state, &a.id) == ["Alpha project"]).await;

    // The socket dies without a word: the heartbeat finds out after two beats.
    link.borrow_mut().sender.as_mut().unwrap().close();
    for _ in 0..2 {
        let beat = drive_future(&mut dom, "a heartbeat", manual.next_heartbeat()).await;
        manual.advance(beat);
    }
    // The preferred route is tried at once and fails; then a pass has failed, and the wait is the first rung.
    let wait = drive_future(&mut dom, "the wait after both routes failed", manual.next_retry()).await;
    assert_eq!(wait, 1_000);
    assert!(matches!(state.peek().live.get(&a.id), Some(ConnectionStatus::Interrupted(_))), "{:?}", state.peek().live.get(&a.id));
    assert_eq!(parked_titles(state, &a.id), ["Alpha project"], "what the window had stays while the connection is down");
    manual.advance(wait);
    until(&mut dom, "the connection again", |_| state.peek().parked.get(&a.id).is_some_and(|slice| slice.status == ConnectionStatus::Connected && slice.shell.synchronized)).await;
    assert_eq!(parked_titles(state, &a.id), ["Alpha project"]);
    drop(dom);
    a.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_route_that_answers_as_another_environment_is_refused_before_its_token_is_sent() {
    let manual = Manual::default();
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, props, state) = bare();
    props.transport.set_clock(manual.clock());
    TEST_RANDOM.with(|random| random.set(Some(0.0)));
    // Two routes of "env-a"; the second address now belongs to a different server, and the token saved for it was never issued by
    // that server, so a request carrying it would be refused for its credential, not for who it is.
    let routes = vec![Route { address: a.address.clone(), token: a.token.clone() }, Route { address: b.address.clone(), token: "a-token-b-never-issued".into() }];
    let transport = props.transport.clone();
    dom.in_scope(ScopeId::APP, || {
        spawn(async move { connect_routes_in_background(transport, state, routes, None).await });
    });
    until(&mut dom, "the first route", |_| state.peek().parked.get(&a.id).is_some_and(|slice| slice.status == ConnectionStatus::Connected && slice.shell.synchronized)).await;
    let a_id = a.id.clone();
    // The first route goes away.
    a.server.stop().await;
    let link = props.transport.link_of(&a_id).unwrap();
    link.borrow_mut().sender.as_mut().unwrap().close();
    for _ in 0..2 {
        let beat = drive_future(&mut dom, "a heartbeat", manual.next_heartbeat()).await;
        manual.advance(beat);
    }
    until(&mut dom, "the refusal", |_| matches!(state.peek().live.get(&a_id), Some(ConnectionStatus::Blocked(_)))).await;
    let Some(ConnectionStatus::Blocked(reason)) = state.peek().live.get(&a_id).cloned() else { unreachable!() };
    assert!(reason.contains("a different environment"), "{reason}");
    assert!(!reason.contains("a-token-b-never-issued"));
    drop(dom);
    b.server.stop().await;
}
