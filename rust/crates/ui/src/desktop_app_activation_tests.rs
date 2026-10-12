//! `t3 app` through the window: a request the shell's broker holds is opened by the mounted page against a real in-process server, and
//! the end to end test sends it with the command itself (`t3_server::cli_app`) over the control socket of a temporary state directory.
//! What the broker and the socket do alone is tested in `t3_desktop::app_activation`.
use super::*;
use crate::runtime::transport_tests::{api_fixture, drive_future, start_fixture};
use serde_json::json;
use std::{sync::atomic::{AtomicUsize, Ordering}, time::Duration};
use t3_contracts::{AuthEnvironmentScope, DesktopAppActivationErrorCode, DesktopAppActivationResponse};
use t3_desktop::app_activation::REQUEST_TIMEOUT;

fn request(id: &str, root: &str, platform: &str) -> DesktopAppActivationRequest {
    DesktopAppActivationRequest::parse(&json!({"version":1,"requestId":id,"type":"open-workspace","workspaceRoot":root,"platform":platform}).to_string()).unwrap()
}

fn folder() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("t3a").tempdir_in("/tmp").unwrap()
}

#[derive(Clone)]
struct Props {
    state: Rc<RefCell<Option<Store<UiModel>>>>,
    transport: TransportHandle,
    address: String,
    credential: String,
    broker: Arc<Broker>,
}
impl PartialEq for Props {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.state, &other.state) && Arc::ptr_eq(&self.broker, &other.broker) && self.address == other.address
    }
}
fn harness(props: Props) -> Element {
    let state = use_store(UiModel::default);
    *props.state.borrow_mut() = Some(state);
    use_context_provider({
        let (broker, address) = (props.broker.clone(), props.address.clone());
        move || Handle { broker, server_address: Rc::new(move || Some(address.clone())) }
    });
    use_future({
        let props = props.clone();
        move || {
            let (transport, address, credential) = (props.transport.clone(), props.address.clone(), props.credential.clone());
            async move { runtime::pair_and_connect(transport, state, address, credential).await }
        }
    });
    rsx! { Coordinator { state, transport: props.transport.clone() } }
}

/// The window with its page mounted, paired with a real server, and the broker the page serves.
struct Rig {
    dom: VirtualDom,
    state: Store<UiModel>,
    broker: Arc<Broker>,
    raised: Arc<AtomicUsize>,
    server: crate::runtime::fixture_server::Server,
    _store: t3_server::persistence::Store,
    _directory: tempfile::TempDir,
}
impl Rig {
    async fn start() -> Self {
        let raised = Arc::new(AtomicUsize::new(0));
        let broker = Broker::new(tokio::runtime::Handle::current(), REQUEST_TIMEOUT, {
            let raised = raised.clone();
            move || {
                raised.fetch_add(1, Ordering::SeqCst);
            }
        });
        Self::with_broker(broker, raised, "linux").await
    }
    async fn with_broker(broker: Arc<Broker>, raised: Arc<AtomicUsize>, os: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut api = api_fixture(&directory, "activation-env");
        // The platform the environment says it runs on (`serverConfig.environment.platform.os`).
        api.environment["platform"]["os"] = json!(os);
        api.config.as_mut().unwrap()["environment"]["platform"]["os"] = json!(os);
        let store = api.store.clone();
        let scopes = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];
        let credential = api.auth.create_pairing_credential(&scopes, chrono::Utc::now(), chrono::Duration::minutes(5)).unwrap();
        let (address, server) = start_fixture(api).await;
        let props = Props { state: Rc::new(RefCell::new(None)), transport: TransportHandle::default(), address, credential, broker: broker.clone() };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        Self { dom, state, broker, raised, server, _store: store, _directory: directory }
    }
    /// Asks as the control socket does, and drives the page until the answer.
    async fn ask(&mut self, request: DesktopAppActivationRequest) -> DesktopAppActivationResponse {
        let asked = tokio::spawn(self.broker.request(request));
        drive_future(&mut self.dom, "answer", asked).await.unwrap()
    }
    fn projects(&self) -> Vec<(String, String)> {
        let state = self.state;
        let shell = state.shell();
        let shell = shell.read();
        shell.snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| (project.id.to_string(), project.workspace_root.to_string())).collect()).unwrap_or_default()
    }
    async fn stop(self) {
        drop(self.dom);
        self.server.stop().await;
    }
}

fn opened(response: &DesktopAppActivationResponse) -> (String, String) {
    match response {
        DesktopAppActivationResponse::Success(success) => (success.project_id.as_str().to_owned(), success.thread_id.as_str().to_owned()),
        other => panic!("expected the project to open, got {other:?}"),
    }
}
fn refused(response: &DesktopAppActivationResponse) -> (DesktopAppActivationErrorCode, String) {
    match response {
        DesktopAppActivationResponse::Failure(failure) => (failure.code, failure.message.as_str().to_owned()),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_folder_the_app_does_not_have_is_added_and_a_new_thread_is_opened_in_it() {
    let folder = folder();
    let root = folder.path().to_string_lossy().into_owned();
    let mut rig = Rig::start().await;
    // The request comes before the page is connected: it waits for the environment to be ready and is then opened.
    let response = rig.ask(request("a", &root, "linux")).await;
    let (project, thread) = opened(&response);
    assert_eq!(rig.raised.load(Ordering::SeqCst), 1, "the window is brought forward for the request");

    let projects = rig.projects();
    assert_eq!(projects.len(), 1, "{projects:?}");
    assert_eq!(projects[0].0, project);
    assert_eq!(t3_client::project_import::comparable(&projects[0].1).trim_start_matches("/private"), t3_client::project_import::comparable(&root).trim_start_matches("/private"));
    let state = rig.state;
    assert_eq!(state.selected_project().peek().as_deref(), Some(project.as_str()), "the window is on the project");
    assert_eq!(state.active_draft().peek().as_deref(), Some(thread.as_str()), "and on the new thread's form");
    assert_eq!(*state.view().peek(), runtime::View::Chat);
    rig.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_folder_the_app_already_has_is_not_added_again() {
    let folder = folder();
    let root = folder.path().to_string_lossy().into_owned();
    let mut rig = Rig::start().await;
    let (project, thread) = opened(&rig.ask(request("a", &root, "linux")).await);
    let (again, same_thread) = opened(&rig.ask(request("b", &root, "linux")).await);
    assert_eq!(again, project);
    assert_eq!(same_thread, thread, "the project's empty draft is the new thread");
    assert_eq!(rig.projects().len(), 1);
    assert_eq!(rig.raised.load(Ordering::SeqCst), 2);
    rig.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn two_requests_are_opened_one_after_the_other_in_the_order_they_came() {
    let (first, second) = (folder(), folder());
    let mut rig = Rig::start().await;
    let one = tokio::spawn(rig.broker.request(request("a", &first.path().to_string_lossy(), "linux")));
    let two = tokio::spawn(rig.broker.request(request("b", &second.path().to_string_lossy(), "linux")));
    let (one, two) = drive_future(&mut rig.dom, "both", async { (one.await.unwrap(), two.await.unwrap()) }).await;
    let (first_project, _) = opened(&one);
    let (second_project, second_thread) = opened(&two);
    assert_ne!(first_project, second_project);
    let order: Vec<String> = rig.projects().into_iter().map(|(id, _)| id).collect();
    assert_eq!(order.len(), 2);
    // The window ends on the last project asked for.
    assert_eq!(rig.state.selected_project().peek().as_deref(), Some(second_project.as_str()));
    assert_eq!(rig.state.active_draft().peek().as_deref(), Some(second_thread.as_str()));
    rig.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_command_from_another_platform_is_refused_and_adds_nothing() {
    let folder = folder();
    let mut rig = Rig::start().await;
    let response = rig.ask(request("a", &folder.path().to_string_lossy(), "darwin")).await;
    assert_eq!(
        refused(&response),
        (DesktopAppActivationErrorCode::PlatformMismatch, "The command path is for darwin, but the desktop app's primary environment uses linux. Cross-platform path mapping is not supported.".to_owned())
    );
    assert!(rig.projects().is_empty());
    rig.stop().await;
}

// ---- the wait for a project that was added

/// A page with a model and nothing else: the waits are tested on what the shell has and has not been told.
fn bare(props: (Rc<RefCell<Option<(Store<UiModel>, Signal<u64>)>>>, u8)) -> Element {
    let state = use_store(UiModel::default);
    let changes = use_signal(|| 0u64);
    *props.0.borrow_mut() = Some((state, changes));
    rsx! {}
}

fn snapshot_with(project: &str) -> t3_contracts::ShellSnapshot {
    serde_json::from_value(json!({
        "schemaVersion": 1, "snapshotSequence": 1, "threads": [], "archivedThreads": [],
        "projects": [{"id": project, "title": "p", "workspaceRoot": "/work/p", "defaultModelSelection": null, "scripts": [], "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z"}],
    }))
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_project_is_waited_for_until_the_shell_has_it_and_not_longer_than_ten_seconds() {
    let seen = Rc::new(RefCell::new(None));
    let mut dom = VirtualDom::new_with_props(bare, (seen.clone(), 0));
    dom.rebuild_in_place();
    let (state, changes) = seen.borrow().unwrap();
    let environment = EnvironmentId::new("env-1").unwrap();
    state.destination().set(Some(environment.clone()));
    let waiting = Waiting { list: Rc::default(), changes };
    // Time is what the test says it is: each wait gets a clock it ends by sending on.
    let clocks: Rc<RefCell<Vec<(u64, futures_channel::oneshot::Sender<()>)>>> = Rc::default();
    let pause: Rc<dyn Fn(u64) -> futures_util::future::LocalBoxFuture<'static, ()>> = Rc::new({
        let clocks = clocks.clone();
        move |milliseconds| {
            let (sender, receiver) = futures_channel::oneshot::channel();
            clocks.borrow_mut().push((milliseconds, sender));
            Box::pin(async move {
                let _ = receiver.await;
            })
        }
    });
    let page = Page { state, transport: TransportHandle::default(), address: Rc::new(|| None), waiting: waiting.clone(), pause };

    // Never told of: given up on when ten seconds have passed, and the wait is forgotten.
    let give_up = page.wait_for_project("env-1", "p-1");
    let mut give_up = std::pin::pin!(give_up);
    assert!(futures_util::poll!(give_up.as_mut()).is_pending());
    assert_eq!(clocks.borrow().iter().map(|(milliseconds, _)| *milliseconds).collect::<Vec<_>>(), [10_000]);
    let (_, ten_seconds) = clocks.borrow_mut().remove(0);
    ten_seconds.send(()).unwrap();
    assert_eq!(futures_util::poll!(give_up.as_mut()), std::task::Poll::Ready(Err("The project did not appear in the desktop app.".to_owned())));
    assert!(waiting.list.borrow().is_empty());

    // Told of while waiting: it goes on at once.
    let wait = page.wait_for_project("env-1", "p-2");
    let mut wait = std::pin::pin!(wait);
    assert!(futures_util::poll!(wait.as_mut()).is_pending());
    release(&waiting.list, state, &environment);
    assert!(futures_util::poll!(wait.as_mut()).is_pending(), "the shell has not got it yet");
    state.shell().write().snapshot = Some(snapshot_with("p-2"));
    release(&waiting.list, state, &environment);
    assert_eq!(futures_util::poll!(wait.as_mut()), std::task::Poll::Ready(Ok(())));
    assert!(waiting.list.borrow().is_empty());

    // A project already there needs no wait at all.
    assert_eq!(page.wait_for_project("env-1", "p-2").await, Ok(()));
    assert_eq!(page.find_project("env-1", "/work/p/").as_deref(), Some("p-2"), "found by its folder, as the original compares paths");
    assert_eq!(page.find_project("env-1", "/work/other"), None);
}

// ---- the command itself

#[cfg(unix)]
mod command {
    use super::*;
    use t3_desktop::app_activation::{Config, Service};

    /// The shell's socket service in a temporary state directory, and the page of a window on a server that says it runs on `os`.
    struct Window {
        /// `<home>/userdata` is the state directory the address is named for; `t3 app --base-dir <home>` asks there.
        home: std::path::PathBuf,
        socket_dir: tempfile::TempDir,
        service: Service,
        rig: Rig,
    }
    impl Window {
        async fn open(os: &str) -> Self {
            let socket_dir = folder();
            let home = socket_dir.path().join("home");
            let config = Config { state_dir: home.join("userdata"), platform: "linux", temp_dir: socket_dir.path().to_path_buf(), user_id: t3_remote::desktop_app_control::current_user_id() };
            let (_ticks, reclaim) = tokio::sync::mpsc::unbounded_channel();
            let raised = Arc::new(AtomicUsize::new(0));
            let service = Service::start_with(&config, REQUEST_TIMEOUT, { let raised = raised.clone(); move || { raised.fetch_add(1, Ordering::SeqCst); } }, reclaim).unwrap();
            let rig = Rig::with_broker(service.broker(), raised, os).await;
            Self { home, socket_dir, service, rig }
        }
        /// `t3 app` run for `root` on a thread of its own, and the page driven until it has ended.
        async fn t3_app(&mut self, root: &str) -> t3_server::cli_commands::Outcome {
            let (sent, received) = futures_channel::oneshot::channel();
            let args = vec!["--base-dir".to_owned(), self.home.to_string_lossy().into_owned(), root.to_owned()];
            let temp = self.socket_dir.path().to_path_buf();
            std::thread::spawn(move || {
                let env = |_: &str| None;
                let host = t3_server::cli_app::Host { env: &env, platform: "linux", cwd: temp.clone(), temp_dir: temp, user_id: t3_remote::desktop_app_control::current_user_id(), request_id: "t3-app-request".into() };
                let _ = sent.send(t3_server::cli_app::app_command_on(&args, host));
            });
            drive_future(&mut self.rig.dom, "t3 app", received).await.unwrap()
        }
        async fn close(self) {
            let socket = std::path::PathBuf::from(self.service.address());
            assert!(socket.exists());
            self.service.close();
            assert!(!socket.exists(), "quitting leaves no socket file behind");
            self.rig.stop().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn t3_app_opens_a_folder_in_the_window_over_the_control_socket_and_says_so() {
        let mut window = Window::open("linux").await;
        let project = folder();
        let root = project.path().to_string_lossy().into_owned();
        let outcome = window.t3_app(&root).await;
        assert_eq!((outcome.code, outcome.stdout.as_str(), outcome.stderr.as_str()), (0, format!("Opened {root} in T3 Code.\n").as_str(), ""));
        assert_eq!(window.rig.raised.load(Ordering::SeqCst), 1, "the window was brought forward");
        assert_eq!(window.rig.projects().len(), 1);
        assert!(window.rig.state.active_draft().peek().is_some());

        // The same command again opens the project that is there.
        let outcome = window.t3_app(&root).await;
        assert_eq!(outcome.code, 0);
        assert_eq!(window.rig.projects().len(), 1);
        window.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn t3_app_fails_with_the_code_when_the_window_is_on_another_platform() {
        let mut window = Window::open("windows").await;
        let project = folder();
        let root = project.path().to_string_lossy().into_owned();
        let outcome = window.t3_app(&root).await;
        assert_eq!((outcome.code, outcome.stdout.as_str()), (1, ""));
        assert_eq!(outcome.stderr, format!("T3 Code could not open {root} (platform-mismatch).\n"));
        assert!(window.rig.projects().is_empty());
        window.close().await;
    }
}

