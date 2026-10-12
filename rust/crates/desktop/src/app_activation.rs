//! `t3 app` reaching this window: the broker that holds a request until the page is ready for it, the page's answer to it, and the
//! control socket service that ties them to the address `t3 app` asks at (`app/DesktopAppActivationBroker.ts`,
//! `app/DesktopAppActivation.ts`, `apps/web/src/desktopAppActivation.ts`). The socket itself, with the original's ownership checks,
//! is `t3_remote::desktop_app_control`; this is what the shell does with a request that arrives on it.
//!
//! A request waits here until the page says it is ready (`Broker::register_renderer`, the original's `appActivation.setReady(true)`),
//! is handed to the page one at a time (`appActivation.onRequest`), and ends when the page answers it (`Broker::complete`,
//! `appActivation.complete`), the window goes away, the command that asked is closed, or 15 seconds pass.
use std::{
    future::Future,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::{DesktopAppActivationErrorCode, DesktopAppActivationRequest, DesktopAppActivationResponse, ExecutionEnvironmentPlatformOs};
use tokio::sync::oneshot;

pub use async_trait::async_trait;
pub use t3_remote::desktop_app_control::DESKTOP_REQUEST_TIMEOUT as REQUEST_TIMEOUT;

fn failure(request_id: &str, code: DesktopAppActivationErrorCode, message: &str) -> DesktopAppActivationResponse {
    DesktopAppActivationResponse::failure(request_id, code, message)
}

/// Where the page receives a request. Err is a page that is gone (the original's `webContents.send` throwing); the request is then
/// held again until a page registers.
pub type Renderer = Arc<dyn Fn(&DesktopAppActivationRequest) -> Result<(), ()> + Send + Sync>;

struct Pending {
    request: DesktopAppActivationRequest,
    resolve: Option<oneshot::Sender<DesktopAppActivationResponse>>,
    timer: tokio::task::JoinHandle<()>,
    dispatched: bool,
}

#[derive(Default)]
struct State {
    /// In the order the requests arrived.
    pending: Vec<Pending>,
    renderer: Option<Renderer>,
    closed: bool,
}

/// `DesktopAppActivationBroker`: holds CLI requests until the real window is ready to handle them. The renderer is called with the
/// broker locked, and must not call back into it (the shell's sends on a channel).
pub struct Broker {
    state: Mutex<State>,
    timeout: Duration,
    /// Brings the window forward. Called with no lock held, for every request that is accepted.
    activate: Box<dyn Fn() + Send + Sync>,
    /// Runs the deadlines.
    runtime: tokio::runtime::Handle,
}

impl Broker {
    pub fn new(runtime: tokio::runtime::Handle, timeout: Duration, activate: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::default()), timeout, activate: Box::new(activate), runtime })
    }

    /// `request`: the answer to a request, which comes when the page completes it or one of the ways it ends happens. Everything up to
    /// the first await runs at once, so a request is registered when this is first polled.
    pub fn request(self: &Arc<Self>, request: DesktopAppActivationRequest) -> impl Future<Output = DesktopAppActivationResponse> + Send + use<> {
        let id = request.request_id.as_str().to_owned();
        let answer = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                Err(failure(&id, DesktopAppActivationErrorCode::RendererUnavailable, "T3 Code is shutting down."))
            } else if state.pending.iter().any(|pending| pending.request.request_id.as_str() == id) {
                Err(failure(&id, DesktopAppActivationErrorCode::InvalidRequest, "The request id is already in use."))
            } else {
                let (resolve, receiver) = oneshot::channel();
                let timer = {
                    let (broker, id, timeout) = (self.clone(), id.clone(), self.timeout);
                    self.runtime.spawn(async move {
                        tokio::time::sleep(timeout).await;
                        broker.settle(failure(&id, DesktopAppActivationErrorCode::RequestTimeout, "The desktop app did not finish opening the project in time."));
                    })
                };
                state.pending.push(Pending { request, resolve: Some(resolve), timer, dispatched: false });
                Ok(receiver)
            }
        };
        if answer.is_ok() {
            (self.activate)();
            Self::flush(&mut self.state.lock().unwrap());
        }
        async move {
            match answer {
                Ok(receiver) => receiver.await.unwrap_or_else(|_| failure(&id, DesktopAppActivationErrorCode::RendererUnavailable, "T3 Code is shutting down.")),
                Err(response) => response,
            }
        }
    }

    /// `registerRenderer` (`appActivation.setReady(true)`): the page can take requests, and the ones that waited are sent.
    pub fn register_renderer(&self, renderer: Renderer) {
        let mut state = self.state.lock().unwrap();
        state.renderer = Some(renderer);
        Self::flush(&mut state);
    }

    /// `clearRenderer` (`setReady(false)`, the window closing, the page navigating): what was sent to the page and not answered ends.
    pub fn clear_renderer(&self) {
        let mut state = self.state.lock().unwrap();
        state.renderer = None;
        let dispatched: Vec<String> = state.pending.iter().filter(|pending| pending.dispatched).map(|pending| pending.request.request_id.as_str().to_owned()).collect();
        for id in dispatched {
            Self::settle_in(&mut state, failure(&id, DesktopAppActivationErrorCode::RendererUnavailable, "The T3 Code window closed before it opened the project."));
        }
    }

    /// `complete` (`appActivation.complete`): the page's answer.
    pub fn complete(&self, response: DesktopAppActivationResponse) {
        self.settle(response);
    }

    /// `cancel`: the command that asked went away before it was answered.
    pub fn cancel(&self, request_id: &str) {
        self.settle(failure(request_id, DesktopAppActivationErrorCode::RendererUnavailable, "The command closed before T3 Code was ready."));
    }

    /// `close`: the app is quitting; everything held ends, and nothing new is taken.
    pub fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.renderer = None;
        let ids: Vec<String> = state.pending.iter().map(|pending| pending.request.request_id.as_str().to_owned()).collect();
        for id in ids {
            Self::settle_in(&mut state, failure(&id, DesktopAppActivationErrorCode::RendererUnavailable, "T3 Code is shutting down."));
        }
    }

    /// `#flush`: the first request not yet sent goes to the page, when none is out. One at a time, so that the page opens one project
    /// before it opens the next.
    fn flush(state: &mut State) {
        let Some(renderer) = state.renderer.clone() else { return };
        if state.pending.iter().any(|pending| pending.dispatched) {
            return;
        }
        let Some(pending) = state.pending.iter_mut().find(|pending| !pending.dispatched) else { return };
        pending.dispatched = true;
        if renderer(&pending.request).is_err() {
            pending.dispatched = false;
            state.renderer = None;
        }
    }

    fn settle(&self, response: DesktopAppActivationResponse) {
        Self::settle_in(&mut self.state.lock().unwrap(), response);
    }

    /// `#settle`: ends the request the response is for, if it is still held, and sends the next.
    fn settle_in(state: &mut State, response: DesktopAppActivationResponse) {
        let Some(index) = state.pending.iter().position(|pending| pending.request.request_id.as_str() == response.request_id()) else { return };
        let mut pending = state.pending.remove(index);
        pending.timer.abort();
        if let Some(resolve) = pending.resolve.take() {
            let _ = resolve.send(response);
        }
        Self::flush(state);
    }
}

// ---- the page's side -------------------------------------------------------------------------------------------------

/// The environment the request is opened in: the app's own server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub environment_id: String,
    pub platform: ExecutionEnvironmentPlatformOs,
}

/// What the page does for a request (`DesktopAppActivationDependencies`). Errors are messages a person reads.
#[async_trait::async_trait(?Send)]
pub trait Dependencies {
    /// None while the app's own environment is not connected.
    fn target(&self) -> Option<Target>;
    /// The id of the project of the environment whose folder this is, if there is one.
    fn find_project(&self, environment_id: &str, workspace_root: &str) -> Option<String>;
    async fn create_project(&self, environment_id: &str, workspace_root: &str) -> Result<String, String>;
    /// Returns when the project has arrived in the environment's shell.
    async fn wait_for_project(&self, environment_id: &str, project_id: &str) -> Result<(), String>;
    /// Opens a new thread of the project; its id, or none when the page could not.
    async fn open_thread(&self, environment_id: &str, project_id: &str) -> Result<Option<String>, String>;
}

fn os_of(platform: t3_contracts::DesktopAppActivationPlatform) -> ExecutionEnvironmentPlatformOs {
    match platform {
        t3_contracts::DesktopAppActivationPlatform::Darwin => ExecutionEnvironmentPlatformOs::Darwin,
        t3_contracts::DesktopAppActivationPlatform::Linux => ExecutionEnvironmentPlatformOs::Linux,
        t3_contracts::DesktopAppActivationPlatform::Win32 => ExecutionEnvironmentPlatformOs::Windows,
    }
}

fn os_name(os: ExecutionEnvironmentPlatformOs) -> &'static str {
    match os {
        ExecutionEnvironmentPlatformOs::Darwin => "darwin",
        ExecutionEnvironmentPlatformOs::Linux => "linux",
        ExecutionEnvironmentPlatformOs::Windows => "windows",
        ExecutionEnvironmentPlatformOs::Unknown => "unknown",
    }
}

/// `errorMessage`: the error's own text when it has any.
fn message_or(error: String, fallback: &str) -> String {
    if error.trim().is_empty() { fallback.to_owned() } else { error }
}

/// `handleDesktopAppActivationRequest`: opens the project of the request, adding it first when the app does not have it, and a new
/// thread in it; the answer is what `t3 app` prints or fails with.
pub async fn handle_request(request: &DesktopAppActivationRequest, deps: &dyn Dependencies) -> DesktopAppActivationResponse {
    use DesktopAppActivationErrorCode::{EnvironmentUnavailable, PlatformMismatch, ProjectCreateFailed, ThreadOpenFailed};
    let id = request.request_id.as_str();
    let Some(target) = deps.target() else {
        return failure(id, EnvironmentUnavailable, "The desktop app's primary local environment is not connected.");
    };
    let requested = os_of(request.platform);
    if requested != target.platform {
        return failure(
            id,
            PlatformMismatch,
            &format!(
                "The command path is for {}, but the desktop app's primary environment uses {}. Cross-platform path mapping is not supported.",
                os_name(requested),
                os_name(target.platform)
            ),
        );
    }
    let root = request.workspace_root.as_str();
    let project = match deps.find_project(&target.environment_id, root) {
        Some(project) => project,
        None => {
            let created = async {
                let project = deps.create_project(&target.environment_id, root).await?;
                deps.wait_for_project(&target.environment_id, &project).await?;
                Ok::<_, String>(project)
            };
            match created.await {
                Ok(project) => project,
                Err(error) => return failure(id, ProjectCreateFailed, &message_or(error, "T3 Code could not add the project.")),
            }
        }
    };
    const NO_THREAD: &str = "T3 Code could not open a new thread for the project.";
    match deps.open_thread(&target.environment_id, &project).await {
        Ok(Some(thread)) => DesktopAppActivationResponse::success(id, &project, &thread).unwrap_or_else(|_| failure(id, ThreadOpenFailed, NO_THREAD)),
        Ok(None) => failure(id, ThreadOpenFailed, NO_THREAD),
        Err(error) => failure(id, ThreadOpenFailed, &message_or(error, NO_THREAD)),
    }
}

// ---- the control socket ----------------------------------------------------------------------------------------------

/// `path.resolve`: joined onto `cwd` unless absolute, with `.` and `..` taken out without touching the disk. Symlinks are not
/// followed: `t3 app` hashes the string it builds the same way, and `/var` against `/private/var` is a different address.
pub fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    let joined = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// What the address of the control socket is made of: the same four things `t3 app` reads (`cli_app.rs`).
#[derive(Debug, Clone)]
pub struct Config {
    /// The state directory the address is named for, as `t3 app` joins it: `<T3 home>/userdata`.
    pub state_dir: PathBuf,
    /// `process.platform`.
    pub platform: &'static str,
    pub temp_dir: PathBuf,
    pub user_id: Option<u32>,
}

impl Config {
    /// This process: its working directory makes a relative state directory absolute, and `env` names the temporary directory.
    pub fn for_process(state_dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        Self {
            state_dir: absolute(state_dir, &cwd),
            platform: t3_contracts::node_platform_name(),
            temp_dir: t3_remote::desktop_app_control::node_tmpdir(env),
            user_id: t3_remote::desktop_app_control::current_user_id(),
        }
    }

    pub fn address(&self) -> t3_remote::desktop_app_control::ControlAddress {
        t3_remote::desktop_app_control::resolve_address(&self.state_dir, self.platform, &self.temp_dir, self.user_id)
    }
}

/// How often the socket is looked for again. The original watches the directory; a check is one `lstat`.
pub const RECLAIM_INTERVAL: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests;

#[cfg(unix)]
pub use service::Service;

#[cfg(unix)]
mod service {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use t3_remote::desktop_app_control::{ControlHandler, ControlServer, start_control_server};
    use tokio::sync::{mpsc, watch};

    /// How long `close` waits for the socket to be closed and its file removed.
    const CLOSE_WAIT: Duration = Duration::from_secs(2);

    /// A message every `interval`, the first one after it, until nobody is listening.
    pub(super) async fn ticker(interval: Duration, ticks: mpsc::UnboundedSender<()>) {
        let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            timer.tick().await;
            if ticks.send(()).is_err() {
                return;
            }
        }
    }

    struct Handler(Arc<Broker>);

    #[async_trait::async_trait]
    impl ControlHandler for Handler {
        async fn handle(&self, request: DesktopAppActivationRequest) -> DesktopAppActivationResponse {
            self.0.request(request).await
        }
        fn cancel(&self, request_id: &str) {
            self.0.cancel(request_id);
        }
    }

    /// `DesktopAppActivation`: serves `t3 app` on the control address until `close`, on a runtime of its own so that the window's
    /// executor and the quit path never wait on it.
    pub struct Service {
        runtime: Mutex<Option<tokio::runtime::Runtime>>,
        broker: Arc<Broker>,
        server: Arc<ControlServer>,
        address: String,
        passes: watch::Receiver<u64>,
        closed: AtomicBool,
    }

    impl Service {
        /// Binds the address (taking it over from any other app that has it) and starts serving. `activate` brings the window
        /// forward for every request.
        pub fn start(config: &Config, activate: impl Fn() + Send + Sync + 'static) -> std::io::Result<Self> {
            let (ticks_in, ticks) = mpsc::unbounded_channel();
            let service = Self::start_with(config, REQUEST_TIMEOUT, activate, ticks)?;
            // The timer that asks for a reclaim, on the service's own runtime.
            if let Some(runtime) = service.runtime.lock().unwrap().as_ref() {
                runtime.spawn(ticker(RECLAIM_INTERVAL, ticks_in));
            }
            Ok(service)
        }

        /// `start` with the moments to look for the socket again given as messages, which is how a test moves them.
        pub fn start_with(config: &Config, timeout: Duration, activate: impl Fn() + Send + Sync + 'static, mut reclaim: mpsc::UnboundedReceiver<()>) -> std::io::Result<Self> {
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("t3-app-control").enable_all().build()?;
            let broker = Broker::new(runtime.handle().clone(), timeout, activate);
            let address = config.address();
            // Bound on the service's runtime and waited for here, so that this works from any thread, async or not.
            let (bound, answer) = std::sync::mpsc::channel();
            runtime.spawn({
                let (address, user_id, handler) = (address.clone(), config.user_id, Arc::new(Handler(broker.clone())));
                async move {
                    let _ = bound.send(start_control_server(&address, user_id, handler).await);
                }
            });
            let server = Arc::new(answer.recv().map_err(|_| std::io::Error::other("The control socket service stopped while starting."))??);
            let (passed, passes) = watch::channel(0u64);
            runtime.spawn({
                let server = server.clone();
                async move {
                    while reclaim.recv().await.is_some() {
                        // `reclaim` binds the address again only if no socket is there.
                        if let Err(error) = server.reclaim().await {
                            eprintln!("Could not restore the desktop app control socket: {error}");
                        }
                        passed.send_modify(|count| *count += 1);
                    }
                }
            });
            Ok(Self { runtime: Mutex::new(Some(runtime)), broker, address: address.address, server, passes, closed: AtomicBool::new(false) })
        }

        pub fn broker(&self) -> Arc<Broker> {
            self.broker.clone()
        }

        pub fn address(&self) -> &str {
            &self.address
        }

        /// The number of times the socket has been looked for again, as it changes.
        pub fn reclaim_passes(&self) -> watch::Receiver<u64> {
            self.passes.clone()
        }

        /// Stops listening, removes the socket file if it is still this app's, and ends what is held. A close that does not finish is
        /// not waited for: the app is quitting.
        pub fn close(&self) {
            if self.closed.swap(true, Ordering::SeqCst) {
                return;
            }
            let Some(runtime) = self.runtime.lock().unwrap().take() else { return };
            let (done, finished) = std::sync::mpsc::channel();
            let server = self.server.clone();
            runtime.spawn(async move {
                let _ = done.send(server.close().await);
            });
            match finished.recv_timeout(CLOSE_WAIT) {
                Ok(Err(error)) => eprintln!("Could not close the desktop app control socket: {error}"),
                Ok(Ok(())) => {}
                Err(_) => eprintln!("The desktop app control socket did not close in time."),
            }
            self.broker.close();
            runtime.shutdown_background();
        }
    }
}
