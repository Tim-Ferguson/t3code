//! Probing a device host over SSH (`device/SshDeviceHost.ts`: `probe`, `bootstrap`, `ownerFor`; `device/sshDeviceScript.ts`).
//!
//! The probe pipes the unchanged remote script (`ssh_device_script.txt`, made by `tools/generate_ssh_device_script.mjs`) into `node` on the
//! host and reads its JSON answer: which platforms the host can run devices for, and which versions of the device tools it has. It
//! starts nothing and installs nothing. The other modes (start, agent-start, stop-agent, stop) belong to a connected host: [`SshDeviceHost`]
//! (`SshDeviceHost.make`) runs them, forwards the host's hub and agent-device ports to loopback over `ssh -N -L`, and reconnects when the
//! forward drops or a helper dies.
use crate::{
    device_commands::HostCommandOutput,
    device_host::DeviceHost,
    local_device_host::{DeviceHostAgentReady, DeviceHostReady, DevicePhase},
};
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::{DeviceHostId, DeviceHostStatus, DeviceHostSummary, DeviceHostUnavailableError, DeviceHostUnavailableErrorTag, DevicePlatform, DevicePlatformAvailability, DeviceToolVersion, DeviceToolVersions, SshDeviceHostConfig};
use t3_remote::ssh::{RunSshOptions, Ssh, SshError, SshTarget, base_ssh_args};
use tokio::{io::AsyncReadExt, sync::oneshot, task::JoinHandle};

const SCRIPT: &str = include_str!("ssh_device_script.txt");
const ENVIRONMENT: &str = include_str!("ssh_device_environment.txt");

/// What a script run does on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Probe,
    Start,
    AgentStart,
    StopAgent,
    Stop,
}
impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Probe => "probe",
            Mode::Start => "start",
            Mode::AgentStart => "agent-start",
            Mode::StopAgent => "stop-agent",
            Mode::Stop => "stop",
        }
    }
    /// An install can take a long while; a probe or a stop must not.
    fn timeout(self) -> Duration {
        Duration::from_millis(if matches!(self, Mode::Start | Mode::AgentStart) { 1_300_000 } else { 45_000 })
    }
}

/// `DeviceHostError`: a host failed at one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub host_id: String,
    pub step: String,
    pub cause: String,
}
impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Device host {} failed while {}.", self.host_id, self.step)
    }
}
impl std::error::Error for HostError {}

/// `quoteRemoteArg`: single-quoted for the remote shell.
pub fn quote_remote_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// `remoteDeviceScript`: the script text the host's `node` runs, for this owner and mode.
pub fn remote_script(owner: &str, mode: Mode) -> String {
    SCRIPT.replace("\"@@OWNER@@\"", &Value::String(owner.to_owned()).to_string()).replace("\"@@MODE@@\"", &Value::String(mode.name().to_owned()).to_string())
}

/// `ownerFor`: a stable name for this environment's claim on the host, so two servers sharing a host do not stop each other's hub.
pub fn owner_for(state_dir: &Path, host_id: &str) -> String {
    let environment_id = std::fs::read_to_string(state_dir.join("rust-environment-id")).unwrap_or_else(|_| state_dir.to_string_lossy().into_owned());
    let digest = Sha256::digest(format!("{environment_id}\0{}\0{host_id}", state_dir.to_string_lossy()).as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()[..24].to_owned()
}

fn target_for(config: &SshDeviceHostConfig) -> SshTarget {
    SshTarget { alias: config.target.as_str().to_owned(), hostname: config.target.as_str().to_owned(), username: None, port: config.port.flatten().map(|port| port.0) }
}

fn identity_args(config: &SshDeviceHostConfig) -> Vec<String> {
    match config.identity_file.as_ref().and_then(|file| file.as_ref()) {
        Some(file) => vec!["-i".into(), file.as_str().to_owned()],
        None => vec![],
    }
}

/// `bootstrap`: runs the script in one mode over SSH and returns what it printed.
pub async fn bootstrap(ssh: &Ssh, config: &SshDeviceHostConfig, owner: &str, mode: Mode) -> Result<String, HostError> {
    let command = format!("{ENVIRONMENT}command -v node >/dev/null 2>&1 || {{ echo \"Node is missing from the non-interactive SSH PATH\" >&2; exit 1; }}; exec node");
    let options = RunSshOptions {
        pre_host_args: identity_args(config),
        remote_command_args: vec!["sh".into(), "-c".into(), quote_remote_arg(&command)],
        stdin: Some(remote_script(owner, mode)),
        timeout: Some(mode.timeout()),
        batch_mode: None,
    };
    ssh.run(&target_for(config), options).await.map(|result| result.stdout).map_err(|error| HostError { host_id: config.id.as_str().to_owned(), step: mode.name().to_owned(), cause: error.to_string() })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Probe {
    #[allow(dead_code)]
    node_path: String,
    #[serde(default)]
    tools: Option<DeviceToolVersions>,
    platforms: Vec<DevicePlatformAvailability>,
}

/// `probe`: what the host can run and which device tools it has.
pub async fn probe(ssh: &Ssh, config: &SshDeviceHostConfig, owner: &str) -> Result<DeviceHostSummary, HostError> {
    let stdout = bootstrap(ssh, config, owner, Mode::Probe).await?;
    let failed = |cause: String| HostError { host_id: config.id.as_str().to_owned(), step: "reading probe result".into(), cause };
    let value: Probe = serde_json::from_str(stdout.trim()).map_err(|error| failed(error.to_string()))?;
    let installed = |tool: &t3_contracts::DeviceToolVersion| tool.installed_versions.contains(&tool.required_version);
    let (hub_installed, agent_installed) = value.tools.as_ref().map_or((false, false), |tools| (installed(&tools.hub), installed(&tools.agent)));
    let mut summary = json!({"id":config.id,"label":config.label,"kind":"ssh","hubInstalled":hub_installed,"agentDeviceInstalled":agent_installed,"platforms":value.platforms});
    if let Some(tools) = &value.tools {
        summary["tools"] = serde_json::to_value(tools).map_err(|error| failed(error.to_string()))?;
    }
    serde_json::from_value(summary).map_err(|error| failed(error.to_string()))
}

/// `deviceToolInstallMessage`: what the host is doing while its tool installs.
pub fn device_tool_install_message(name: &str, tool: Option<&DeviceToolVersion>) -> String {
    let Some(tool) = tool else { return format!("Installing {name}…") };
    let previous = tool.running_version.clone().or_else(|| {
        let mut versions = tool.installed_versions.clone();
        versions.sort_by(|a, b| numeric_order(a, b));
        versions.pop()
    });
    match previous {
        Some(previous) if !tool.installed_versions.contains(&tool.required_version) => format!("Updating {name} from {previous} to {}…", tool.required_version),
        _ => format!("Installing {name} {}…", tool.required_version),
    }
}
/// `a.localeCompare(b, "en", { numeric: true })` for dotted versions: digit runs compare as numbers.
fn numeric_order(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(text: &str) -> Vec<(bool, String)> {
        let mut parts: Vec<(bool, String)> = Vec::new();
        for character in text.chars() {
            let digit = character.is_ascii_digit();
            match parts.last_mut() {
                Some((last, run)) if *last == digit => run.push(character),
                _ => parts.push((digit, character.to_string())),
            }
        }
        parts
    }
    let (left, right) = (parts(a), parts(b));
    for (x, y) in left.iter().zip(&right) {
        let order = match (x.0, y.0) {
            (true, true) => x.1.trim_start_matches('0').len().cmp(&y.1.trim_start_matches('0').len()).then_with(|| x.1.trim_start_matches('0').cmp(y.1.trim_start_matches('0'))),
            _ => x.1.to_lowercase().cmp(&y.1.to_lowercase()),
        };
        if order.is_ne() {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

/// Runs a host command where the devices are, over the same SSH target (`run` in `SshDeviceHost.make`).
#[derive(Debug, Clone)]
pub struct SshRunner {
    ssh: Ssh,
    config: SshDeviceHostConfig,
}
impl SshRunner {
    pub fn new(ssh: Ssh, config: SshDeviceHostConfig) -> Self {
        Self { ssh, config }
    }
    /// A command that cannot run, or exits non-zero, is an output with that code (127 when there is none), never an error.
    pub async fn run(&self, command: &str, args: &[String], timeout: Duration, stdin: Option<Vec<u8>>) -> HostCommandOutput {
        let words = std::iter::once(command).chain(args.iter().map(String::as_str)).map(quote_remote_arg).collect::<Vec<_>>().join(" ");
        let script = format!("{ENVIRONMENT}exec {words}");
        let options = RunSshOptions {
            pre_host_args: identity_args(&self.config),
            remote_command_args: vec!["sh".into(), "-c".into(), quote_remote_arg(&script)],
            stdin: stdin.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
            timeout: Some(timeout),
            batch_mode: None,
        };
        match self.ssh.run(&target_for(&self.config), options).await {
            Ok(result) => HostCommandOutput { stdout: result.stdout, stderr: result.stderr, code: 0 },
            Err(SshError::Command { exit_code, stdout, message, .. }) => HostCommandOutput { stdout, stderr: message, code: exit_code.unwrap_or(127) },
            Err(error) => HostCommandOutput { stdout: String::new(), stderr: error.to_string(), code: 127 },
        }
    }
}

/// Rewrites the agent-device config after every (re)connect that has the agent, because the forwarded local port changes (`onReady`).
pub type OnAgentReady = Arc<dyn Fn(DeviceHostAgentReady) -> BoxFuture<'static, Result<(), HostError>> + Send + Sync>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Started {
    node_path: String,
    #[serde(default)]
    tools: Option<DeviceToolVersions>,
    platforms: Vec<DevicePlatformAvailability>,
    hub_port: u16,
    #[serde(default)]
    daemon_port: Option<u16>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    entry_path: Option<String>,
    helpers: StartedHelpers,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartedHelpers {
    serve_sim_ax_settings: Option<String>,
    serve_sim_cli: Option<String>,
}

/// What `connect` leaves running for one connection: the `ssh -N -L` forward. Closing it ends the process and waits for it.
struct Resources {
    kill: oneshot::Sender<()>,
    task: JoinHandle<()>,
}
impl Resources {
    async fn close(self) {
        let _ = self.kill.send(());
        let _ = self.task.await;
    }
}
/// The connection scope of the original: an identity (a monitor compares it to know whether it still belongs to the host) and what runs.
struct Connection {
    id: u64,
    resources: Option<Resources>,
}
#[derive(Clone)]
struct Connected {
    ready: DeviceHostReady,
    agent: Option<crate::device_agent_daemon::AgentDeviceEndpoint>,
}
struct SshState {
    stopped: bool,
    activated: bool,
    wants_agent: bool,
    ready: Option<Connected>,
    connection: Option<Connection>,
    next_connection: u64,
    generation: u64,
    summary: DeviceHostSummary,
}
struct SshInner {
    config: SshDeviceHostConfig,
    id: DeviceHostId,
    ssh: Ssh,
    owner: String,
    on_ready: Option<OnAgentReady>,
    on_status: Option<DevicePhase>,
    http: reqwest::Client,
    /// The original's one-permit semaphore: connects, stops and agent changes take turns.
    lock: tokio::sync::Mutex<()>,
    state: Mutex<SshState>,
    monitors: Mutex<Vec<JoinHandle<()>>>,
    /// Tests learn each wait the host is about to make (a health interval, a reconnect delay) here, and shorten the forward's deadline.
    #[cfg(test)]
    tap: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Duration>>>,
    #[cfg(test)]
    forward_deadline: Mutex<Duration>,
}
impl Drop for SshInner {
    fn drop(&mut self) {
        for monitor in self.monitors.get_mut().unwrap().drain(..) {
            monitor.abort();
        }
    }
}

/// `SshDeviceHost.make`: a device host reached over SSH. Nothing runs on the host or here until `ensure_ready`.
#[derive(Clone)]
pub struct SshDeviceHost(Arc<SshInner>);

/// How often a connected host's helpers are asked if they still answer, and the longest wait between reconnect attempts.
const HEALTH_INTERVAL: Duration = Duration::from_secs(10);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const LAST_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const FORWARD_READY_TIMEOUT: Duration = Duration::from_secs(15);

impl SshDeviceHost {
    pub fn new(config: SshDeviceHostConfig, ssh: Ssh, state_dir: &Path, on_ready: Option<OnAgentReady>, on_status: Option<DevicePhase>) -> Self {
        let id = DeviceHostId::new(config.id.as_str()).expect("a device host id is a valid host id");
        let owner = owner_for(state_dir, config.id.as_str());
        let summary = serde_json::from_value(json!({"id":config.id,"label":config.label,"kind":"ssh","hubInstalled":false,"agentDeviceInstalled":false,"platforms":[]})).expect("an SSH host summary");
        Self(Arc::new(SshInner {
            id,
            ssh,
            owner,
            on_ready,
            on_status,
            http: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap(),
            lock: tokio::sync::Mutex::new(()),
            state: Mutex::new(SshState { stopped: false, activated: false, wants_agent: false, ready: None, connection: None, next_connection: 0, generation: 0, summary }),
            monitors: Mutex::new(Vec::new()),
            #[cfg(test)]
            tap: Mutex::new(None),
            #[cfg(test)]
            forward_deadline: Mutex::new(FORWARD_READY_TIMEOUT),
            config,
        }))
    }
    /// Tells a test about a wait before the host makes it, so the test can move a paused clock past exactly that wait.
    fn waiting(&self, _delay: Duration) {
        #[cfg(test)]
        if let Some(tap) = self.0.tap.lock().unwrap().as_ref() {
            let _ = tap.send(_delay);
        }
    }
    #[cfg(test)]
    pub(crate) fn set_forward_deadline(&self, deadline: Duration) {
        *self.0.forward_deadline.lock().unwrap() = deadline;
    }
    /// Every wait the host is about to make (a health interval, a reconnect delay), as it is made.
    #[cfg(test)]
    pub(crate) fn waits(&self) -> tokio::sync::mpsc::UnboundedReceiver<Duration> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        *self.0.tap.lock().unwrap() = Some(sender);
        receiver
    }
    fn forward_deadline(&self) -> Duration {
        #[cfg(test)]
        return *self.0.forward_deadline.lock().unwrap();
        #[cfg(not(test))]
        FORWARD_READY_TIMEOUT
    }
    /// The configuration this host was made from, which `reconcile` compares to decide whether the host is still the configured one.
    pub fn config(&self) -> &SshDeviceHostConfig {
        &self.0.config
    }
    fn unavailable(&self, error: &HostError) -> DeviceHostUnavailableError {
        DeviceHostUnavailableError {
            tag: DeviceHostUnavailableErrorTag::DeviceHostUnavailableError,
            host_id: self.0.id.clone(),
            reason: if error.step == "probe" { "Could not connect to this host over SSH.".into() } else { format!("Device support failed during {}.", error.step) },
            cause: Some(Some(json!({"_tag":"DeviceHostError","hostId":error.host_id,"step":error.step,"message":error.to_string(),"cause":{"message":error.cause}}))),
        }
    }
    async fn status(&self, status: DeviceHostStatus, detail: Option<String>) {
        if let Some(on_status) = &self.0.on_status {
            on_status(status, detail).await;
        }
    }
    fn failure(&self, step: &str, cause: impl ToString) -> HostError {
        HostError { host_id: self.0.id.as_str().to_owned(), step: step.to_owned(), cause: cause.to_string() }
    }
    async fn bootstrap(&self, mode: Mode) -> Result<String, HostError> {
        bootstrap(&self.0.ssh, &self.0.config, &self.0.owner, mode).await
    }
    async fn probe(&self) -> Result<DeviceHostSummary, HostError> {
        probe(&self.0.ssh, &self.0.config, &self.0.owner).await
    }
    fn take_connection(&self) -> Option<Resources> {
        self.0.state.lock().unwrap().connection.take().and_then(|connection| connection.resources)
    }
    async fn close_connection(&self) {
        if let Some(resources) = self.take_connection() {
            resources.close().await;
        }
    }
    async fn ready_http(&self, base: &str, route: &str, stderr: &Arc<Mutex<String>>) -> Result<(), HostError> {
        let url = format!("{base}{route}");
        let poll = async {
            // The original retries every 100 ms, at most ceil(15000 / 100) times after the first.
            for attempt in 0..=150 {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                let answer = self.0.http.get(&url).timeout(Duration::from_secs(1)).send().await;
                if answer.is_ok_and(|response| response.status().is_success()) {
                    return true;
                }
            }
            false
        };
        if matches!(tokio::time::timeout(self.forward_deadline(), poll).await, Ok(true)) {
            return Ok(());
        }
        let stderr = stderr.lock().unwrap().clone();
        Err(self.failure("waiting for SSH forward", if stderr.is_empty() { "Forwarded endpoint did not answer." } else { &stderr }))
    }
    async fn alive(&self, base: &str, route: &str) -> bool {
        matches!(tokio::time::timeout(HEALTH_TIMEOUT, self.0.http.get(format!("{base}{route}")).send()).await, Ok(Ok(response)) if response.status().as_u16() == 200)
    }
    async fn reserve_port(&self, step: &str) -> Result<u16, HostError> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.map_err(|error| self.failure(step, error))?;
        listener.local_addr().map(|address| address.port()).map_err(|error| self.failure(step, error))
    }
    /// `connectOnce`: starts the host's hub (and agent-device when wanted), forwards its ports here and waits for them to answer.
    async fn connect_once(&self) -> Result<Connected, HostError> {
        let agent = {
            let mut state = self.0.state.lock().unwrap();
            state.activated = true;
            state.wants_agent
        };
        let output = self.bootstrap(if agent { Mode::AgentStart } else { Mode::Start }).await?;
        self.status(DeviceHostStatus::Starting, None).await;
        let remote: Started = serde_json::from_str(output.trim()).map_err(|error| self.failure("reading host endpoints", error))?;
        {
            let mut state = self.0.state.lock().unwrap();
            let agent_installed = agent || state.summary.agent_device_installed;
            state.summary.platforms = remote.platforms.clone();
            state.summary.tools = remote.tools.clone().map(Some);
            state.summary.hub_installed = true;
            state.summary.agent_device_installed = agent_installed;
        }
        let hub_port = self.reserve_port("reserving hub port").await?;
        let daemon_port = self.reserve_port("reserving daemon port").await?;
        let (id, generation) = {
            let mut state = self.0.state.lock().unwrap();
            state.next_connection += 1;
            state.generation += 1;
            let id = state.next_connection;
            state.connection = Some(Connection { id, resources: None });
            (id, state.generation)
        };
        let target = target_for(&self.0.config);
        let mut args = base_ssh_args(&target, Some("yes"));
        args.extend(identity_args(&self.0.config));
        args.extend(["-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=3", "-N", "-L"].map(String::from));
        args.push(format!("127.0.0.1:{hub_port}:127.0.0.1:{}", remote.hub_port));
        if let Some(remote_daemon) = remote.daemon_port {
            args.extend(["-L".to_owned(), format!("127.0.0.1:{daemon_port}:127.0.0.1:{remote_daemon}")]);
        }
        args.push(self.0.config.target.as_str().to_owned());
        let mut child = tokio::process::Command::new(&self.0.ssh.executable)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| self.failure("forwarding ports", error))?;
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut pipe = child.stderr.take().expect("piped stderr");
        let (kill, killed) = oneshot::channel();
        let (exit, exited) = tokio::sync::watch::channel(false);
        let kept = stderr.clone();
        let task = tokio::spawn(async move {
            // The last 2000 characters of what ssh said, for the error when the forward does not come up.
            let read = async {
                let mut chunk = [0u8; 4096];
                let mut text = String::new();
                loop {
                    match pipe.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            text.push_str(&String::from_utf8_lossy(&chunk[..count]));
                            let characters: Vec<char> = text.chars().collect();
                            if characters.len() > 2000 {
                                text = characters[characters.len() - 2000..].iter().collect();
                            }
                            *kept.lock().unwrap() = text.clone();
                        }
                    }
                }
            };
            tokio::select! {
                _ = killed => { let _ = child.kill().await; }
                _ = async { let _ = tokio::join!(read, child.wait()); } => {}
            }
            let _ = child.wait().await;
            exit.send_replace(true);
        });
        {
            let mut state = self.0.state.lock().unwrap();
            if let Some(connection) = state.connection.as_mut().filter(|connection| connection.id == id) {
                connection.resources = Some(Resources { kill, task });
            } else {
                task.abort();
            }
        }
        let origin = format!("http://127.0.0.1:{hub_port}");
        let helper = |path: Option<String>| path.map(PathBuf::from);
        let ready = DeviceHostReady::remote(
            self.0.id.clone(),
            generation,
            SshRunner::new(self.0.ssh.clone(), self.0.config.clone()),
            PathBuf::from(&remote.node_path),
            origin.clone(),
            helper(remote.helpers.serve_sim_ax_settings),
            helper(remote.helpers.serve_sim_cli),
        );
        let agent_device = match (remote.daemon_port, remote.token, remote.entry_path) {
            (Some(_), Some(token), Some(entry)) => Some(crate::device_agent_daemon::AgentDeviceEndpoint { base_url: format!("http://127.0.0.1:{daemon_port}"), token, entry_path: PathBuf::from(entry), pid: None, version: None }),
            _ => None,
        };
        self.ready_http(&origin, "/readyz", &stderr).await?;
        if let Some(endpoint) = &agent_device {
            self.ready_http(&endpoint.base_url, "/health", &stderr).await?;
        }
        let next = Connected { ready, agent: agent_device };
        if let (Some(endpoint), Some(on_ready)) = (&next.agent, &self.0.on_ready) {
            on_ready(DeviceHostAgentReady { host: next.ready.clone(), agent_device: endpoint.clone() }).await?;
        }
        self.0.state.lock().unwrap().ready = Some(next.clone());
        self.status(DeviceHostStatus::Ready, None).await;
        self.monitor(id, next.clone(), exited);
        Ok(next)
    }
    /// Reconnect also repairs helpers that died while SSH itself stayed connected.
    fn monitor(&self, id: u64, connected: Connected, mut exited: tokio::sync::watch::Receiver<bool>) {
        let weak = Arc::downgrade(&self.0);
        let task = tokio::spawn(async move {
            let unhealthy = async {
                loop {
                    let Some(inner) = weak.upgrade() else { return };
                    SshDeviceHost(inner).waiting(HEALTH_INTERVAL);
                    tokio::time::sleep(HEALTH_INTERVAL).await;
                    let Some(inner) = weak.upgrade() else { return };
                    let host = SshDeviceHost(inner);
                    let hub_alive = host.alive(&connected.ready.origin, "/readyz").await;
                    let daemon_alive = match &connected.agent {
                        Some(agent) => host.alive(&agent.base_url, "/health").await,
                        None => true,
                    };
                    if !hub_alive || !daemon_alive {
                        return;
                    }
                }
            };
            tokio::select! {
                _ = exited.wait_for(|exited| *exited) => {}
                _ = unhealthy => {}
            }
            let Some(inner) = weak.upgrade() else { return };
            SshDeviceHost(inner).reconnect(id).await;
        });
        let mut monitors = self.0.monitors.lock().unwrap();
        monitors.retain(|monitor| !monitor.is_finished());
        monitors.push(task);
    }
    fn is_current(&self, id: u64) -> bool {
        let state = self.0.state.lock().unwrap();
        !state.stopped && state.connection.as_ref().is_some_and(|connection| connection.id == id)
    }
    /// What the monitor of connection `id` does once the forward or a helper is gone: retry with a doubling delay until a connect works or
    /// the host is stopped or has moved on to another connection.
    async fn reconnect(&self, id: u64) {
        if !self.is_current(id) {
            return;
        }
        self.0.state.lock().unwrap().ready = None;
        self.status(DeviceHostStatus::Starting, Some("Reconnecting to device host…".into())).await;
        let resources = self.0.state.lock().unwrap().connection.as_mut().and_then(|connection| connection.resources.take());
        if let Some(resources) = resources {
            resources.close().await;
        }
        let mut delay = FIRST_RECONNECT_DELAY;
        loop {
            if !self.is_current(id) {
                return;
            }
            self.waiting(delay);
            tokio::time::sleep(delay).await;
            let result = {
                let _lock = self.0.lock.lock().await;
                let skip = {
                    let state = self.0.state.lock().unwrap();
                    state.stopped || state.ready.is_some()
                };
                if skip { Ok(()) } else { self.connect().await.map(|_| ()) }
            };
            let error = match result {
                Ok(()) => return,
                Err(error) => error,
            };
            self.status(DeviceHostStatus::Failed, Some(error.to_string())).await;
            let other = {
                let mut state = self.0.state.lock().unwrap();
                let other = state.connection.take().filter(|connection| connection.id != id);
                state.connection = Some(Connection { id, resources: None });
                other
            };
            if let Some(resources) = other.and_then(|connection| connection.resources) {
                resources.close().await;
            }
            delay = (delay * 2).min(LAST_RECONNECT_DELAY);
        }
    }
    /// `connect`: SSH binds after the reservation is released, so a competing bind gets fresh ports, twice.
    async fn connect(&self) -> Result<Connected, HostError> {
        for attempt in 0.. {
            match self.connect_once().await {
                Ok(connected) => return Ok(connected),
                Err(error) => {
                    self.close_connection().await;
                    if attempt >= 2 || !["forwarding ports", "waiting for SSH forward"].contains(&error.step.as_str()) {
                        return Err(error);
                    }
                }
            }
        }
        unreachable!("the loop returns")
    }
    async fn ready_locked(&self, on_phase: &DevicePhase) -> Result<Connected, HostError> {
        {
            let mut state = self.0.state.lock().unwrap();
            state.stopped = false;
            if let Some(ready) = &state.ready {
                return Ok(ready.clone());
            }
        }
        let summary = self.probe().await?;
        let (status, detail) = if summary.hub_installed {
            (DeviceHostStatus::Starting, None)
        } else {
            (DeviceHostStatus::Installing, Some(device_tool_install_message("device hub", summary.tools.as_ref().and_then(|tools| tools.as_ref()).map(|tools| &tools.hub))))
        };
        self.0.state.lock().unwrap().summary = summary;
        on_phase(status, detail).await;
        match self.connect().await {
            Ok(connected) => Ok(connected),
            Err(error) => {
                self.close_connection().await;
                Err(error)
            }
        }
    }
    async fn ensure_connected(&self, on_phase: &DevicePhase) -> Result<Connected, HostError> {
        let _lock = self.0.lock.lock().await;
        self.ready_locked(on_phase).await
    }
    /// `changeAgent`: reconnects with agent-device on or off. `None` when turning it off for a host that has none running.
    async fn change_agent(&self, enabled: bool) -> Result<Option<Connected>, HostError> {
        let _lock = self.0.lock.lock().await;
        let previous = {
            let mut state = self.0.state.lock().unwrap();
            state.wants_agent = enabled;
            match &state.ready {
                Some(ready) if enabled && ready.agent.is_some() => return Ok(Some(ready.clone())),
                Some(ready) if !enabled && ready.agent.is_none() => return Ok(None),
                None if !enabled => return Ok(None),
                _ => {}
            }
            state.ready = None;
            state.connection.take()
        };
        if let Some(resources) = previous.and_then(|connection| connection.resources) {
            resources.close().await;
        }
        if !enabled {
            self.bootstrap(Mode::StopAgent).await?;
        }
        match self.connect().await {
            Ok(connected) => Ok(Some(connected)),
            Err(error) => {
                self.close_connection().await;
                if enabled {
                    let _ = self.bootstrap(Mode::StopAgent).await;
                }
                Err(error)
            }
        }
    }
}

#[async_trait]
impl DeviceHost for SshDeviceHost {
    fn id(&self) -> DeviceHostId {
        self.0.id.clone()
    }
    async fn summary(&self) -> DeviceHostSummary {
        self.0.state.lock().unwrap().summary.clone()
    }
    async fn inspect(&self) -> Result<DeviceHostSummary, HostError> {
        let value = self.probe().await?;
        self.0.state.lock().unwrap().summary = value.clone();
        Ok(value)
    }
    async fn platform(&self, platform: DevicePlatform) -> DevicePlatformAvailability {
        let unreachable = || DevicePlatformAvailability { platform, available: false, reason: Some(Some("Cannot reach device host. Test its SSH connection in Settings.".into())) };
        match self.probe().await {
            Ok(value) => {
                self.0.state.lock().unwrap().summary.platforms = value.platforms.clone();
                value.platforms.into_iter().find(|candidate| candidate.platform == platform).unwrap_or_else(unreachable)
            }
            Err(_) => unreachable(),
        }
    }
    async fn ensure_ready(&self, on_phase: DevicePhase) -> Result<DeviceHostReady, DeviceHostUnavailableError> {
        self.ensure_connected(&on_phase).await.map(|connected| connected.ready).map_err(|error| self.unavailable(&error))
    }
    async fn ensure_agent_ready(&self, on_phase: DevicePhase) -> Result<DeviceHostAgentReady, DeviceHostUnavailableError> {
        let run = async {
            self.ensure_connected(&on_phase).await?;
            let summary = self.summary().await;
            let (status, detail) = if summary.agent_device_installed {
                (DeviceHostStatus::Starting, None)
            } else {
                (DeviceHostStatus::Installing, Some(device_tool_install_message("agent tools", summary.tools.as_ref().and_then(|tools| tools.as_ref()).map(|tools| &tools.agent))))
            };
            on_phase(status, detail).await;
            match self.change_agent(true).await? {
                Some(Connected { ready, agent: Some(agent_device) }) => Ok(DeviceHostAgentReady { host: ready, agent_device }),
                _ => Err(self.failure("starting agent tools", "Daemon endpoint missing")),
            }
        };
        run.await.map_err(|error| self.unavailable(&error))
    }
    fn current(&self) -> Option<DeviceHostReady> {
        self.0.state.lock().unwrap().ready.as_ref().map(|connected| connected.ready.clone())
    }
    fn current_agent(&self) -> Option<crate::device_agent_daemon::AgentDeviceEndpoint> {
        self.0.state.lock().unwrap().ready.as_ref().and_then(|connected| connected.agent.clone())
    }
    fn generation(&self) -> u64 {
        self.0.state.lock().unwrap().generation
    }
    async fn stop_agent(&self) {
        let _ = self.change_agent(false).await;
    }
    async fn stop(&self) {
        let _lock = self.0.lock.lock().await;
        let (resources, activated) = {
            let mut state = self.0.state.lock().unwrap();
            state.stopped = true;
            state.ready = None;
            let resources = state.connection.take().and_then(|connection| connection.resources);
            let activated = std::mem::replace(&mut state.activated, false);
            state.wants_agent = false;
            (resources, activated)
        };
        if let Some(resources) = resources {
            resources.close().await;
        }
        if activated {
            let _ = self.bootstrap(Mode::Stop).await;
        }
    }
    async fn shutdown(&self) {
        let monitors: Vec<_> = self.0.monitors.lock().unwrap().drain(..).collect();
        for monitor in &monitors {
            monitor.abort();
        }
        for monitor in monitors {
            let _ = monitor.await;
        }
        self.stop().await;
    }
}
