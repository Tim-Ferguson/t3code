//! SSH environments of the desktop app, without a window toolkit: what `apps/desktop` wires in `main.ts`
//! (`DesktopSshEnvironment`, `resolveDesktopSshCliRunner`), `ipc/methods/sshEnvironment.ts` and
//! `ssh/DesktopSshPasswordPrompts.ts` around `SshEnvironmentManager` (`t3-remote`). The window shows the password
//! prompts this service queues and answers them with [`PasswordPrompts::resolve`].
//!
//! All ssh work runs on a runtime of its own, so that the window's executor (and the quit path, which waits for the
//! tunnels to close) never has to be the one that drives a tunnel.
//!
//! Test seams, as the original's (a layer per service): [`Ssh::with_runtime`] takes the processes, ports and HTTP probe;
//! [`Config`] names the `ssh` executable, the home directory whose `.ssh` is read for host discovery and the release
//! mirror the remote installs from, so a real window can be driven against a fake `ssh` under an isolated `HOME`.
use async_trait::async_trait;
pub use t3_remote::ssh::{DiscoveredSshHost, HostSource, SshTarget};
use std::{
    collections::BTreeMap,
    future::Future,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use t3_remote::{
    ssh::{discover_ssh_hosts, ssh_command_for_platform},
    ssh_auth::{SshAuthHost, SshPasswordPrompt, SshPasswordPromptError, SshPasswordRequest},
    ssh_scripts::RemoteT3RunnerOptions,
    ssh_tunnel::{
        ProcessSshSpawner, ReqwestHttpProbe, SshEnvironmentBootstrap, SshEnvironmentError, SshEnvironmentManager,
        SshEnvironmentManagerOptions, SshRuntime, SystemLocalPorts, resolve_ssh_target,
    },
};
use t3_client::ssh::PromptRequest;
use tokio::sync::{oneshot, watch};

/// `DEFAULT_SSH_PASSWORD_PROMPT_TIMEOUT_MS`.
pub const DEFAULT_PROMPT_TIMEOUT: Duration = Duration::from_secs(3 * 60);

/// What the desktop reads from its environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// `ssh` (`ssh.exe` on Windows), or the absolute path of another program (`T3_SSH_BIN`).
    pub ssh_executable: String,
    /// Whose `.ssh/config` and `.ssh/known_hosts` host discovery reads (`HOME`, as `NodeOS.homedir()`).
    pub home_dir: Option<String>,
    pub prompt_timeout: Duration,
    /// Which `t3` the remote runs (`resolveDesktopSshCliRunner`).
    pub runner: RemoteT3RunnerOptions,
}

impl Config {
    /// `T3_SSH_BIN` names the `ssh` program. `T3CODE_DEV_REMOTE_T3_SERVER_ENTRY_PATH` (the original's development switch) makes the
    /// remote run `node <that script>`; otherwise the remote runs the release archive of this app's version, from the mirror
    /// `T3_SSH_RELEASE_BASE_URL` names (the original has none: its default is the GitHub releases).
    pub fn from_env(env: impl Fn(&str) -> Option<String>, app_version: &str) -> Self {
        let text = |name: &str| env(name).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty());
        let runner = match text("T3CODE_DEV_REMOTE_T3_SERVER_ENTRY_PATH") {
            Some(path) => RemoteT3RunnerOptions { node_script_path: Some(path), node_engine_range: Some("^22.16 || ^23.11 || >=24.10".into()), ..Default::default() },
            None => RemoteT3RunnerOptions { archive_version: Some(app_version.to_owned()), release_base_url: text("T3_SSH_RELEASE_BASE_URL"), ..Default::default() },
        };
        Self {
            ssh_executable: text("T3_SSH_BIN").unwrap_or_else(|| ssh_command_for_platform(cfg!(windows)).to_owned()),
            home_dir: text("HOME").or_else(|| text("USERPROFILE")),
            prompt_timeout: DEFAULT_PROMPT_TIMEOUT,
            runner,
        }
    }
}

// ---- password prompts -------------------------------------------------------------------------------------------

/// `DesktopSshPasswordPromptResolveError`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("Invalid SSH password prompt id.")]
    InvalidId,
    #[error("SSH password prompt expired. Try connecting again.")]
    Expired,
}

enum Outcome {
    Password(String),
    Cancelled,
    WindowClosed,
    Stopped,
}

struct Pending {
    request: PromptRequest,
    answer: oneshot::Sender<Outcome>,
}

struct PromptState {
    pending: Vec<Pending>,
    updates: watch::Sender<Vec<PromptRequest>>,
    timeout: Duration,
}

/// `DesktopSshPasswordPrompts`: the queue of password questions ssh has asked, in the order asked.
#[derive(Clone)]
pub struct PasswordPrompts {
    state: Arc<Mutex<PromptState>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl PasswordPrompts {
    pub fn new(timeout: Duration) -> Self {
        Self { state: Arc::new(Mutex::new(PromptState { pending: Vec::new(), updates: watch::channel(Vec::new()).0, timeout })) }
    }

    /// The open questions, oldest first, announced again whenever they change.
    pub fn watch(&self) -> watch::Receiver<Vec<PromptRequest>> {
        self.state.lock().unwrap().updates.subscribe()
    }

    fn publish(state: &PromptState) {
        state.updates.send_replace(state.pending.iter().map(|pending| pending.request.clone()).collect());
    }

    fn take(&self, request_id: &str) -> Option<Pending> {
        let mut state = self.state.lock().unwrap();
        let index = state.pending.iter().position(|pending| pending.request.request_id == request_id)?;
        let pending = state.pending.remove(index);
        Self::publish(&state);
        Some(pending)
    }

    /// `resolve`: the person's answer, or `None` for cancelling. A question that timed out, was answered or was dropped is expired.
    pub fn resolve(&self, request_id: &str, password: Option<String>) -> Result<(), ResolveError> {
        let request_id = request_id.trim();
        if request_id.is_empty() {
            return Err(ResolveError::InvalidId);
        }
        let pending = self.take(request_id).ok_or(ResolveError::Expired)?;
        let _ = pending.answer.send(match password {
            Some(password) => Outcome::Password(password),
            None => Outcome::Cancelled,
        });
        Ok(())
    }

    fn end_all(&self, outcome: impl Fn() -> Outcome) {
        let mut state = self.state.lock().unwrap();
        for pending in std::mem::take(&mut state.pending) {
            let _ = pending.answer.send(outcome());
        }
        Self::publish(&state);
    }

    /// The window the questions were shown in closed.
    pub fn window_closed(&self) {
        self.end_all(|| Outcome::WindowClosed);
    }

    /// The service ends (the finalizer of the original's layer): every open question is cancelled.
    pub fn stop(&self) {
        self.end_all(|| Outcome::Stopped);
    }
}

fn prompt_error(message: String) -> SshPasswordPromptError {
    SshPasswordPromptError { cause: Some(message.clone()), message }
}

#[async_trait]
impl SshPasswordPrompt for PasswordPrompts {
    fn is_available(&self) -> bool {
        true
    }

    async fn request(&self, request: SshPasswordRequest) -> Result<Option<String>, SshPasswordPromptError> {
        let (answer, answered) = oneshot::channel();
        let (request_id, timeout) = {
            let mut state = self.state.lock().unwrap();
            let request_id = format!("{:032x}", rand::random::<u128>());
            let timeout = state.timeout;
            let announced = PromptRequest {
                request_id: request_id.clone(),
                destination: request.destination.clone(),
                username: request.username.clone(),
                prompt: request.prompt.clone(),
                expires_at_ms: now_ms() + timeout.as_millis() as u64,
            };
            state.pending.push(Pending { request: announced, answer });
            Self::publish(&state);
            (request_id, timeout)
        };
        let destination = request.destination;
        let mut answered = answered;
        let outcome = match tokio::time::timeout(timeout, &mut answered).await {
            Ok(outcome) => outcome,
            // An answer that arrived as the time ran out is the answer; otherwise the question is gone.
            Err(_) => match self.take(&request_id) {
                Some(_) => return Err(prompt_error(format!("SSH authentication timed out for {destination}."))),
                None => answered.await,
            },
        };
        match outcome {
            Ok(Outcome::Password(password)) => Ok(Some(password)),
            Ok(Outcome::Cancelled) => Ok(None),
            Ok(Outcome::WindowClosed) => Err(prompt_error("SSH authentication was cancelled because the app window closed.".into())),
            Ok(Outcome::Stopped) | Err(_) => Err(prompt_error("SSH password prompt service stopped.".into())),
        }
    }
}

// ---- the service ------------------------------------------------------------------------------------------------

/// What `ensureEnvironment` of the IPC method answers: the tunnel, or that the person cancelled the password prompt
/// (`DesktopSshPasswordPromptCancelledType`).
#[derive(Clone, Debug, PartialEq)]
pub enum Ensured {
    Ready(SshEnvironmentBootstrap),
    Cancelled(String),
}

struct Inner {
    /// Taken at the end so that dropping inside an async context does not panic.
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    handle: tokio::runtime::Handle,
    manager: SshEnvironmentManager,
    runtime_parts: SshRuntime,
    prompts: PasswordPrompts,
    config: Config,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.lock().unwrap().take() {
            runtime.shutdown_background();
        }
    }
}

/// `DesktopSshEnvironment`. Cheap to clone.
#[derive(Clone)]
pub struct Ssh {
    inner: Arc<Inner>,
}

impl Ssh {
    /// Real processes, sockets and HTTP: the `ssh` the configuration names.
    pub fn new(config: Config) -> std::io::Result<Self> {
        let executable = config.ssh_executable.clone();
        Self::with_runtime(config, move |prompt| SshRuntime {
            spawner: Arc::new(ProcessSshSpawner::new(executable.clone())),
            prompt,
            ports: Arc::new(SystemLocalPorts),
            probe: Arc::new(ReqwestHttpProbe),
            auth_host: SshAuthHost::from_process(),
            ssh_command: ssh_command_for_platform(cfg!(windows)).to_owned(),
        })
    }

    /// As `new`, with the processes, ports and probe `parts` makes around the password prompts.
    pub fn with_runtime(config: Config, parts: impl FnOnce(Arc<dyn SshPasswordPrompt>) -> SshRuntime) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("t3-ssh").enable_all().build()?;
        let prompts = PasswordPrompts::new(config.prompt_timeout);
        let runtime_parts = parts(Arc::new(prompts.clone()));
        let runner = config.runner.clone();
        let manager = SshEnvironmentManager::new(runtime_parts.clone(), SshEnvironmentManagerOptions { resolve_cli_runner: Some(Box::new(move || runner.clone())) });
        Ok(Self { inner: Arc::new(Inner { handle: runtime.handle().clone(), runtime: Mutex::new(Some(runtime)), manager, runtime_parts, prompts, config }) })
    }

    pub fn prompts(&self) -> &PasswordPrompts {
        &self.inner.prompts
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Runs `work` on the ssh runtime, for whichever executor awaits the result.
    async fn run<T: Send + 'static>(&self, work: impl Future<Output = T> + Send + 'static) -> Result<T, String> {
        self.inner.handle.spawn(work).await.map_err(|error| format!("The SSH task ended unexpectedly: {error}"))
    }

    /// `discoverHosts`: the hosts of the home directory's ssh configuration and known hosts.
    pub async fn discover_hosts(&self) -> Vec<DiscoveredSshHost> {
        let Some(home) = self.inner.config.home_dir.clone() else { return Vec::new() };
        self.run(async move { tokio::task::spawn_blocking(move || discover_ssh_hosts(&home)).await.unwrap_or_default() }).await.unwrap_or_default()
    }

    /// `resolveHost`: `ssh -G <alias>` into a target.
    pub async fn resolve_host(&self, alias: &str) -> Result<SshTarget, String> {
        let (parts, alias) = (self.inner.runtime_parts.clone(), alias.trim().to_owned());
        self.run(async move { resolve_ssh_target(&parts, &alias).await.map_err(|error| error.message) }).await?
    }

    /// `ensureEnvironment` with the pairing credential issued: the password prompt cancelled is not a failure.
    pub async fn ensure(&self, target: SshTarget, issue_pairing_token: bool) -> Result<Ensured, String> {
        let manager = self.inner.manager.clone();
        let result = self.run(async move { manager.ensure_environment(&target, issue_pairing_token).await }).await?;
        settle(result.map(Ensured::Ready))
    }

    /// `disconnectEnvironment`: closes the tunnel and stops the server this app started on the host.
    pub async fn disconnect(&self, target: SshTarget) -> Result<(), String> {
        let manager = self.inner.manager.clone();
        self.run(async move { manager.disconnect_environment(&target).await }).await?.map_err(|error| error.to_string())
    }

    /// The finalizer of the manager's scope: open questions are cancelled and every tunnel is closed, stopping the servers this app
    /// started. Waits at most `timeout`; true if it finished. For the quit path, which is not an async context.
    pub fn shutdown_blocking(&self, timeout: Duration) -> bool {
        self.inner.prompts.stop();
        let manager = self.inner.manager.clone();
        let (done, finished) = std::sync::mpsc::channel();
        self.inner.handle.spawn(async move {
            manager.shutdown().await;
            let _ = done.send(());
        });
        finished.recv_timeout(timeout).is_ok()
    }

    /// As `shutdown_blocking`, for an async caller.
    pub async fn shutdown(&self) {
        self.inner.prompts.stop();
        let manager = self.inner.manager.clone();
        let _ = self.run(async move { manager.shutdown().await }).await;
    }
}

/// `isDesktopSshPasswordPromptCancellation` / the IPC method's `catchIf`: a refused password prompt is an answer, not a failure.
fn settle(result: Result<Ensured, SshEnvironmentError>) -> Result<Ensured, String> {
    match result {
        Ok(ensured) => Ok(ensured),
        Err(error) if error.is_password_prompt_error() => Ok(Ensured::Cancelled(error.to_string())),
        Err(error) => Err(error.to_string()),
    }
}

// ---- remembered hosts ---------------------------------------------------------------------------------------------

/// The SSH environments the person added, kept in a file of the app's data directory so they are offered again.
pub fn load_saved(path: &Path) -> Vec<SshTarget> {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

pub fn store_saved(path: &Path, targets: &[SshTarget]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(targets).map_err(std::io::Error::other)?)?;
    std::fs::rename(temporary, path)
}

/// Environment variables as a map, for tests of [`Config::from_env`].
pub fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let map: BTreeMap<String, String> = pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
    move |name| map.get(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(destination: &str) -> SshPasswordRequest {
        SshPasswordRequest { destination: destination.into(), username: Some("julius".into()), prompt: "Enter the SSH password for julius@devbox.".into(), attempt: 1 }
    }

    /// Waits until the queue holds `count` questions and returns them.
    async fn queued(feed: &mut watch::Receiver<Vec<PromptRequest>>, count: usize) -> Vec<PromptRequest> {
        feed.wait_for(|open| open.len() == count).await.unwrap().clone()
    }

    #[tokio::test]
    async fn a_question_waits_for_its_answer_and_leaves_the_queue_with_it() {
        let prompts = PasswordPrompts::new(Duration::from_secs(60));
        let mut feed = prompts.watch();
        let asking = tokio::spawn({
            let prompts = prompts.clone();
            async move { prompts.request(request("devbox")).await }
        });
        let open = queued(&mut feed, 1).await;
        assert_eq!((open[0].destination.as_str(), open[0].username.as_deref()), ("devbox", Some("julius")));
        assert!(open[0].expires_at_ms > now_ms());
        prompts.resolve(&open[0].request_id, Some("hunter2".into())).unwrap();
        assert_eq!(asking.await.unwrap().unwrap(), Some("hunter2".to_owned()));
        assert!(feed.borrow().is_empty());
        // It is answered once: a second answer finds nothing to resolve.
        assert_eq!(prompts.resolve(&open[0].request_id, Some("again".into())), Err(ResolveError::Expired));
        assert_eq!(prompts.resolve("  ", None), Err(ResolveError::InvalidId));
    }

    #[tokio::test]
    async fn cancelling_answers_none_and_questions_are_asked_in_order() {
        let prompts = PasswordPrompts::new(Duration::from_secs(60));
        let mut feed = prompts.watch();
        let first = tokio::spawn({
            let prompts = prompts.clone();
            async move { prompts.request(request("one")).await }
        });
        queued(&mut feed, 1).await;
        let second = tokio::spawn({
            let prompts = prompts.clone();
            async move { prompts.request(request("two")).await }
        });
        let open = queued(&mut feed, 2).await;
        assert_eq!((open[0].destination.as_str(), open[1].destination.as_str()), ("one", "two"));
        prompts.resolve(&open[0].request_id, None).unwrap();
        assert_eq!(first.await.unwrap().unwrap(), None);
        prompts.resolve(&open[1].request_id, Some("x".into())).unwrap();
        assert_eq!(second.await.unwrap().unwrap(), Some("x".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_question_times_out_and_cannot_be_answered_late() {
        let prompts = PasswordPrompts::new(Duration::from_secs(180));
        let mut feed = prompts.watch();
        let asking = tokio::spawn({
            let prompts = prompts.clone();
            async move { prompts.request(request("devbox")).await }
        });
        let open = queued(&mut feed, 1).await;
        let error = asking.await.unwrap().unwrap_err();
        assert_eq!(error.message, "SSH authentication timed out for devbox.");
        assert!(feed.borrow().is_empty());
        assert_eq!(prompts.resolve(&open[0].request_id, Some("late".into())), Err(ResolveError::Expired));
    }

    #[tokio::test]
    async fn stopping_and_closing_the_window_cancel_every_open_question() {
        let prompts = PasswordPrompts::new(Duration::from_secs(60));
        let mut feed = prompts.watch();
        let first = tokio::spawn({
            let prompts = prompts.clone();
            async move { prompts.request(request("one")).await }
        });
        queued(&mut feed, 1).await;
        prompts.window_closed();
        assert_eq!(first.await.unwrap().unwrap_err().message, "SSH authentication was cancelled because the app window closed.");
        let second = tokio::spawn({
            let prompts = prompts.clone();
            async move { prompts.request(request("two")).await }
        });
        queued(&mut feed, 1).await;
        prompts.stop();
        assert_eq!(second.await.unwrap().unwrap_err().message, "SSH password prompt service stopped.");
    }

    #[test]
    fn the_configuration_reads_the_ssh_program_the_home_and_which_t3_the_remote_runs() {
        let config = Config::from_env(env_of(&[("T3_SSH_BIN", " /tmp/fake/ssh "), ("HOME", "/tmp/home"), ("T3_SSH_RELEASE_BASE_URL", "file:///mirror")]), "1.2.3");
        assert_eq!(config.ssh_executable, "/tmp/fake/ssh");
        assert_eq!(config.home_dir.as_deref(), Some("/tmp/home"));
        assert_eq!((config.runner.archive_version.as_deref(), config.runner.release_base_url.as_deref()), (Some("1.2.3"), Some("file:///mirror")));
        assert!(config.runner.node_script_path.is_none());
        let development = Config::from_env(env_of(&[("T3CODE_DEV_REMOTE_T3_SERVER_ENTRY_PATH", " /src/server.mjs ")]), "1.2.3");
        assert_eq!(development.runner.node_script_path.as_deref(), Some("/src/server.mjs"));
        assert!(development.runner.archive_version.is_none());
        assert_eq!(Config::from_env(env_of(&[]), "1.2.3").ssh_executable, ssh_command_for_platform(cfg!(windows)));
    }

    #[test]
    fn remembered_hosts_round_trip_and_a_missing_or_broken_file_is_none() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("ssh-environments.json");
        assert!(load_saved(&path).is_empty());
        let targets = vec![SshTarget { alias: "devbox".into(), hostname: "devbox.example.com".into(), username: Some("julius".into()), port: Some(2222) }];
        store_saved(&path, &targets).unwrap();
        assert_eq!(load_saved(&path), targets);
        std::fs::write(&path, "not json").unwrap();
        assert!(load_saved(&path).is_empty());
    }
}
