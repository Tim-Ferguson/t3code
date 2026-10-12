use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use t3_contracts::DesktopAppActivationPlatform;

fn request(id: &str, root: &str) -> DesktopAppActivationRequest {
    DesktopAppActivationRequest::parse(&serde_json::json!({"version":1,"requestId":id,"type":"open-workspace","workspaceRoot":root,"platform":"linux"}).to_string()).unwrap()
}
fn opened(id: &str) -> DesktopAppActivationResponse {
    DesktopAppActivationResponse::success(id, "project-1", "thread-1").unwrap()
}
fn failed(response: &DesktopAppActivationResponse) -> (DesktopAppActivationErrorCode, String) {
    match response {
        DesktopAppActivationResponse::Failure(failure) => (failure.code, failure.message.as_str().to_owned()),
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// A broker whose window raises are counted, and a page that records what it is sent.
struct Harness {
    broker: Arc<Broker>,
    raised: Arc<AtomicUsize>,
    sent: Arc<Mutex<Vec<String>>>,
}
impl Harness {
    fn new() -> Self {
        let raised = Arc::new(AtomicUsize::new(0));
        let broker = Broker::new(tokio::runtime::Handle::current(), REQUEST_TIMEOUT, {
            let raised = raised.clone();
            move || {
                raised.fetch_add(1, Ordering::SeqCst);
            }
        });
        Self { broker, raised, sent: Arc::default() }
    }
    fn page(&self) -> Renderer {
        let sent = self.sent.clone();
        Arc::new(move |request| {
            sent.lock().unwrap().push(request.request_id.as_str().to_owned());
            Ok(())
        })
    }
    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
}

#[tokio::test(start_paused = true)]
async fn after_shutdown_a_request_is_answered_at_once_and_the_window_is_left_alone() {
    let harness = Harness::new();
    harness.broker.close();
    let response = harness.broker.request(request("a", "/p")).await;
    assert_eq!(failed(&response), (DesktopAppActivationErrorCode::RendererUnavailable, "T3 Code is shutting down.".to_owned()));
    assert_eq!(harness.raised.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn a_request_id_in_use_is_refused_and_the_first_request_is_untouched() {
    let harness = Harness::new();
    let first = tokio::spawn(harness.broker.request(request("a", "/p")));
    tokio::task::yield_now().await;
    let second = harness.broker.request(request("a", "/q")).await;
    assert_eq!(failed(&second), (DesktopAppActivationErrorCode::InvalidRequest, "The request id is already in use.".to_owned()));
    assert_eq!(harness.raised.load(Ordering::SeqCst), 1, "only the request that was taken raises the window");
    harness.broker.register_renderer(harness.page());
    harness.broker.complete(opened("a"));
    assert_eq!(first.await.unwrap(), opened("a"));
}

#[tokio::test(start_paused = true)]
async fn a_request_the_page_never_answers_ends_after_fifteen_seconds() {
    let harness = Harness::new();
    let held = tokio::spawn(harness.broker.request(request("a", "/p")));
    tokio::task::yield_now().await;
    tokio::time::advance(REQUEST_TIMEOUT - Duration::from_millis(1)).await;
    assert!(!held.is_finished(), "not before the deadline");
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(failed(&held.await.unwrap()), (DesktopAppActivationErrorCode::RequestTimeout, "The desktop app did not finish opening the project in time.".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn an_answered_request_leaves_no_deadline_behind() {
    let harness = Harness::new();
    harness.broker.register_renderer(harness.page());
    let held = tokio::spawn(harness.broker.request(request("a", "/p")));
    tokio::task::yield_now().await;
    harness.broker.complete(opened("a"));
    assert_eq!(held.await.unwrap(), opened("a"));
    // The same id is free again, and the old deadline does not end the new request early.
    let again = tokio::spawn(harness.broker.request(request("a", "/p")));
    tokio::task::yield_now().await;
    tokio::time::advance(REQUEST_TIMEOUT - Duration::from_secs(1)).await;
    assert!(!again.is_finished());
    harness.broker.complete(opened("a"));
    assert_eq!(again.await.unwrap(), opened("a"));
}

#[tokio::test(start_paused = true)]
async fn requests_wait_for_the_page_and_go_to_it_one_at_a_time_in_order() {
    let harness = Harness::new();
    let first = tokio::spawn(harness.broker.request(request("a", "/p")));
    let second = tokio::spawn(harness.broker.request(request("b", "/q")));
    tokio::task::yield_now().await;
    assert_eq!(harness.sent(), Vec::<String>::new(), "nothing is sent before the page is ready");
    assert_eq!(harness.raised.load(Ordering::SeqCst), 2);

    harness.broker.register_renderer(harness.page());
    assert_eq!(harness.sent(), ["a"], "the second waits for the first");
    harness.broker.complete(opened("a"));
    assert_eq!(first.await.unwrap(), opened("a"));
    assert_eq!(harness.sent(), ["a", "b"]);
    harness.broker.complete(opened("b"));
    assert_eq!(second.await.unwrap(), opened("b"));

    // A request that arrives while the page is ready and idle is sent at once.
    let third = tokio::spawn(harness.broker.request(request("c", "/r")));
    tokio::task::yield_now().await;
    assert_eq!(harness.sent(), ["a", "b", "c"]);
    harness.broker.complete(opened("c"));
    third.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_page_that_goes_away_ends_what_it_was_sent_and_keeps_what_it_was_not() {
    let harness = Harness::new();
    harness.broker.register_renderer(harness.page());
    let sent = tokio::spawn(harness.broker.request(request("a", "/p")));
    let waiting = tokio::spawn(harness.broker.request(request("b", "/q")));
    tokio::task::yield_now().await;
    harness.broker.clear_renderer();
    assert_eq!(failed(&sent.await.unwrap()), (DesktopAppActivationErrorCode::RendererUnavailable, "The T3 Code window closed before it opened the project.".to_owned()));
    assert_eq!(harness.sent(), ["a"], "the one that waited was not sent to a page that is gone");
    assert!(!waiting.is_finished());
    // The next page gets it.
    harness.broker.register_renderer(harness.page());
    assert_eq!(harness.sent(), ["a", "b"]);
    harness.broker.complete(opened("b"));
    assert_eq!(waiting.await.unwrap(), opened("b"));
}

#[tokio::test(start_paused = true)]
async fn a_command_that_closes_cancels_its_request_and_the_next_one_is_sent() {
    let harness = Harness::new();
    harness.broker.register_renderer(harness.page());
    let first = tokio::spawn(harness.broker.request(request("a", "/p")));
    let second = tokio::spawn(harness.broker.request(request("b", "/q")));
    tokio::task::yield_now().await;
    harness.broker.cancel("a");
    assert_eq!(failed(&first.await.unwrap()), (DesktopAppActivationErrorCode::RendererUnavailable, "The command closed before T3 Code was ready.".to_owned()));
    assert_eq!(harness.sent(), ["a", "b"]);
    harness.broker.cancel("nobody");
    harness.broker.complete(opened("nobody"));
    harness.broker.complete(opened("b"));
    assert_eq!(second.await.unwrap(), opened("b"));
}

#[tokio::test(start_paused = true)]
async fn a_page_that_cannot_receive_is_dropped_and_the_request_is_held_for_the_next_one() {
    let harness = Harness::new();
    harness.broker.register_renderer(Arc::new(|_| Err(())));
    let held = tokio::spawn(harness.broker.request(request("a", "/p")));
    tokio::task::yield_now().await;
    assert!(!held.is_finished());
    harness.broker.register_renderer(harness.page());
    assert_eq!(harness.sent(), ["a"]);
    harness.broker.complete(opened("a"));
    held.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn closing_ends_everything_that_is_held() {
    let harness = Harness::new();
    harness.broker.register_renderer(harness.page());
    let sent = tokio::spawn(harness.broker.request(request("a", "/p")));
    let waiting = tokio::spawn(harness.broker.request(request("b", "/q")));
    tokio::task::yield_now().await;
    harness.broker.close();
    for held in [sent, waiting] {
        assert_eq!(failed(&held.await.unwrap()), (DesktopAppActivationErrorCode::RendererUnavailable, "T3 Code is shutting down.".to_owned()));
    }
    assert_eq!(harness.sent(), ["a"], "closing sends nothing more");
}

// ---- the page's side

struct Fake {
    target: Option<Target>,
    projects: Vec<(String, String)>,
    create: Result<String, String>,
    wait: Result<(), String>,
    thread: Result<Option<String>, String>,
    calls: std::cell::RefCell<Vec<String>>,
}
impl Fake {
    fn linux() -> Self {
        Self {
            target: Some(Target { environment_id: "env-1".into(), platform: ExecutionEnvironmentPlatformOs::Linux }),
            projects: vec![("project-1".into(), "/work/known".into())],
            create: Ok("project-new".into()),
            wait: Ok(()),
            thread: Ok(Some("thread-1".into())),
            calls: Default::default(),
        }
    }
}
#[async_trait::async_trait(?Send)]
impl Dependencies for Fake {
    fn target(&self) -> Option<Target> {
        self.target.clone()
    }
    fn find_project(&self, environment_id: &str, workspace_root: &str) -> Option<String> {
        assert_eq!(environment_id, "env-1");
        self.projects.iter().find(|(_, root)| root == workspace_root).map(|(id, _)| id.clone())
    }
    async fn create_project(&self, environment_id: &str, workspace_root: &str) -> Result<String, String> {
        self.calls.borrow_mut().push(format!("create {environment_id} {workspace_root}"));
        self.create.clone()
    }
    async fn wait_for_project(&self, _: &str, project_id: &str) -> Result<(), String> {
        self.calls.borrow_mut().push(format!("wait {project_id}"));
        self.wait.clone()
    }
    async fn open_thread(&self, _: &str, project_id: &str) -> Result<Option<String>, String> {
        self.calls.borrow_mut().push(format!("open {project_id}"));
        self.thread.clone()
    }
}

#[tokio::test]
async fn a_known_project_opens_a_thread_without_being_added_again() {
    let fake = Fake::linux();
    let response = handle_request(&request("a", "/work/known"), &fake).await;
    assert_eq!(response, DesktopAppActivationResponse::success("a", "project-1", "thread-1").unwrap());
    assert_eq!(*fake.calls.borrow(), ["open project-1"]);
}

#[tokio::test]
async fn an_unknown_folder_is_added_and_waited_for_before_its_thread_opens() {
    let fake = Fake::linux();
    let response = handle_request(&request("a", "/work/new"), &fake).await;
    assert_eq!(response, DesktopAppActivationResponse::success("a", "project-new", "thread-1").unwrap());
    assert_eq!(*fake.calls.borrow(), ["create env-1 /work/new", "wait project-new", "open project-new"]);
}

#[tokio::test]
async fn no_connected_environment_and_another_platform_are_refused_before_anything_is_added() {
    let mut fake = Fake::linux();
    fake.target = None;
    let response = handle_request(&request("a", "/work/new"), &fake).await;
    assert_eq!(failed(&response), (DesktopAppActivationErrorCode::EnvironmentUnavailable, "The desktop app's primary local environment is not connected.".to_owned()));

    let mut fake = Fake::linux();
    fake.target.as_mut().unwrap().platform = ExecutionEnvironmentPlatformOs::Windows;
    let response = handle_request(&request("a", "/work/new"), &fake).await;
    assert_eq!(
        failed(&response),
        (
            DesktopAppActivationErrorCode::PlatformMismatch,
            "The command path is for linux, but the desktop app's primary environment uses windows. Cross-platform path mapping is not supported.".to_owned()
        )
    );
    assert!(fake.calls.borrow().is_empty());
    assert_eq!(os_of(DesktopAppActivationPlatform::Win32), ExecutionEnvironmentPlatformOs::Windows, "win32 is the environment's windows");
}

#[tokio::test]
async fn a_project_that_cannot_be_added_or_a_thread_that_cannot_open_says_which_and_why() {
    let mut fake = Fake::linux();
    fake.create = Err("The folder does not exist.".into());
    let response = handle_request(&request("a", "/work/new"), &fake).await;
    assert_eq!(failed(&response), (DesktopAppActivationErrorCode::ProjectCreateFailed, "The folder does not exist.".to_owned()));

    let mut fake = Fake::linux();
    fake.wait = Err("  ".into());
    let response = handle_request(&request("a", "/work/new"), &fake).await;
    assert_eq!(failed(&response), (DesktopAppActivationErrorCode::ProjectCreateFailed, "T3 Code could not add the project.".to_owned()));

    let mut fake = Fake::linux();
    fake.thread = Ok(None);
    let response = handle_request(&request("a", "/work/known"), &fake).await;
    assert_eq!(failed(&response), (DesktopAppActivationErrorCode::ThreadOpenFailed, "T3 Code could not open a new thread for the project.".to_owned()));

    let mut fake = Fake::linux();
    fake.thread = Err("No thread today.".into());
    let response = handle_request(&request("a", "/work/known"), &fake).await;
    assert_eq!(failed(&response), (DesktopAppActivationErrorCode::ThreadOpenFailed, "No thread today.".to_owned()));
}

// ---- the address

#[test]
fn the_address_is_made_from_the_absolute_state_directory_as_t3_app_makes_it() {
    let env = |name: &str| (name == "TMPDIR").then(|| "/var/folders/xy/T/".to_owned());
    let relative = Config::for_process(Path::new("data/../data/userdata"), &env);
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(relative.state_dir, absolute(&cwd.join("data/userdata"), &cwd));
    assert_eq!(relative.temp_dir, Path::new("/var/folders/xy/T"), "Node's tmpdir drops the trailing slash");

    let config = Config { state_dir: PathBuf::from("/home/me/.t3/userdata"), platform: "linux", temp_dir: PathBuf::from("/tmp"), user_id: Some(501) };
    let expected = t3_remote::desktop_app_control::resolve_address(Path::new("/home/me/.t3/userdata"), "linux", Path::new("/tmp"), Some(501));
    assert_eq!(config.address(), expected);
    assert!(expected.address.starts_with("/tmp/t3code-501/") && expected.address.ends_with(".sock"));
    assert_eq!(absolute(Path::new("/a/./b/../c"), Path::new("/cwd")), Path::new("/a/c"));
    assert_eq!(absolute(Path::new("/private/var/x"), Path::new("/cwd")), Path::new("/private/var/x"), "links are not followed");
}

#[test]
fn a_state_directory_in_the_original_apps_live_data_is_the_address_the_original_listens_at_and_is_refused() {
    // `t3 app` names `<T3 home>/userdata`; an app whose state directory is that one would take the original app's address over.
    let home = Path::new("/Users/me");
    let original = Config { state_dir: home.join(".t3/userdata"), platform: "darwin", temp_dir: PathBuf::from("/tmp"), user_id: Some(501) }.address();
    let env = |name: &str| (name == "T3_DESKTOP_STATE_DIR").then(|| "/Users/me/.t3/userdata".to_owned()).or_else(|| (name == "T3_UI_DATA_DIR").then(|| "/Users/me/data".to_owned()));
    let dirs = crate::launch::AppDirs::resolve(env, "macos").unwrap();
    let ours = Config { state_dir: dirs.server_state.clone(), platform: "darwin", temp_dir: PathBuf::from("/tmp"), user_id: Some(501) }.address();
    assert_eq!(ours, original, "the same address");
    assert!(dirs.check_state_is_not_live_data(Some(home)).is_err(), "and the shell does not bind it");
    // The app's own default is another directory, so another address.
    let own = crate::launch::AppDirs::resolve(|name| (name == "T3_UI_DATA_DIR").then(|| "/Users/me/data".to_owned()), "macos").unwrap();
    assert!(own.check_state_is_not_live_data(Some(home)).is_ok());
    assert_ne!(Config { state_dir: own.server_state, platform: "darwin", temp_dir: PathBuf::from("/tmp"), user_id: Some(501) }.address(), original);
}

#[cfg(unix)]
mod socket {
    use super::*;
    use t3_remote::desktop_app_control::{CLI_RESPONSE_TIMEOUT, send_request};

    /// A short directory: a socket path may not be longer than the system allows, and the default temporary directory can be.
    fn short_dir() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("t3a").tempdir_in("/tmp").unwrap()
    }
    fn config(dir: &Path) -> Config {
        Config { state_dir: dir.join("home/userdata"), platform: "linux", temp_dir: dir.to_path_buf(), user_id: t3_remote::desktop_app_control::current_user_id() }
    }
    fn page(service: &Service) -> std::sync::mpsc::Receiver<String> {
        let (sent, received) = std::sync::mpsc::channel();
        service.broker().register_renderer(Arc::new(move |request| sent.send(request.workspace_root.as_str().to_owned()).map_err(|_| ())));
        received
    }

    #[tokio::test]
    async fn a_request_on_the_socket_reaches_the_page_and_its_answer_comes_back() {
        let dir = short_dir();
        let config = config(dir.path());
        let (_ticks, reclaim) = tokio::sync::mpsc::unbounded_channel();
        let raised = Arc::new(AtomicUsize::new(0));
        let service = Service::start_with(
            &config,
            REQUEST_TIMEOUT,
            {
                let raised = raised.clone();
                move || {
                    raised.fetch_add(1, Ordering::SeqCst);
                }
            },
            reclaim,
        )
        .unwrap();
        assert_eq!(service.address(), config.address().address);
        let received = page(&service);

        let asked = tokio::spawn({
            let address = service.address().to_owned();
            async move { send_request(&address, None, &request("a", "/work/new"), CLI_RESPONSE_TIMEOUT).await }
        });
        let root = tokio::task::spawn_blocking(move || received.recv().unwrap()).await.unwrap();
        assert_eq!(root, "/work/new");
        assert_eq!(raised.load(Ordering::SeqCst), 1, "the window comes forward for the request");
        service.broker().complete(opened("a"));
        assert_eq!(asked.await.unwrap().unwrap(), opened("a"));

        let socket = Path::new(service.address()).to_path_buf();
        assert!(socket.exists());
        service.close();
        assert!(!socket.exists(), "a closed service leaves no socket file behind");
        service.close();
    }

    #[tokio::test]
    async fn the_socket_is_bound_again_when_its_file_is_gone_and_not_before_a_look_for_it() {
        let dir = short_dir();
        let config = config(dir.path());
        let (ticks, reclaim) = tokio::sync::mpsc::unbounded_channel();
        let service = Service::start_with(&config, REQUEST_TIMEOUT, || {}, reclaim).unwrap();
        let mut passes = service.reclaim_passes();
        let received = page(&service);
        let socket = Path::new(service.address()).to_path_buf();

        std::fs::remove_file(&socket).unwrap();
        assert!(!socket.exists());
        ticks.send(()).unwrap();
        passes.wait_for(|count| *count >= 1).await.unwrap();
        assert!(socket.exists(), "the socket is back");

        let asked = tokio::spawn({
            let address = service.address().to_owned();
            async move { send_request(&address, None, &request("a", "/work/back"), CLI_RESPONSE_TIMEOUT).await }
        });
        assert_eq!(tokio::task::spawn_blocking(move || received.recv().unwrap()).await.unwrap(), "/work/back");
        service.broker().complete(opened("a"));
        asked.await.unwrap().unwrap();

        // A socket that is there is left alone.
        ticks.send(()).unwrap();
        passes.wait_for(|count| *count >= 2).await.unwrap();
        service.close();
        assert!(!socket.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn the_socket_is_looked_for_every_five_seconds_and_never_at_once() {
        let (ticks, mut received) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(service::ticker(RECLAIM_INTERVAL, ticks));
        tokio::task::yield_now().await;
        assert!(received.try_recv().is_err(), "nothing looks before the first interval");
        tokio::time::advance(RECLAIM_INTERVAL).await;
        received.recv().await.unwrap();
        assert!(received.try_recv().is_err());
        tokio::time::advance(RECLAIM_INTERVAL).await;
        received.recv().await.unwrap();
    }
}
