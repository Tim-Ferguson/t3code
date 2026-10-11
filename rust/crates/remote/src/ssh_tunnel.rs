//! Remote T3 servers over SSH (`packages/ssh/src/tunnel.ts`): launching or reusing a `t3` server on a
//! remote host, forwarding its port to a loopback port here, minting a pairing token over the same
//! ssh connection, and stopping it again. The desktop shell's "SSH environments" are this module;
//! the scripts it sends are [`crate::ssh_scripts`] and the password prompt it calls back into is
//! [`crate::ssh_auth::SshPasswordPrompt`].
//!
//! # How one connection works
//!
//! [`SshEnvironmentManager::ensure_environment`] is the whole story for a target:
//!
//! 1. `ssh -G <alias>` resolves the alias (the `ssh` binary, not this crate, knows `~/.ssh/config`),
//!    then the caller's username and port override it. The result's connection key
//!    ([`crate::ssh::target_connection_key`]) names the tunnel in this process; the first 16 hex
//!    characters of its SHA-256 ([`crate::ssh::remote_state_key`]) name the state directory on the
//!    remote (`~/.t3/ssh-launch/<key>`), so that key must never change.
//! 2. Under a per-key lock, an existing tunnel is reused when its forwarded port still answers
//!    `GET /` within 2 s; otherwise it is closed and a new one is made.
//! 3. A new tunnel runs the launch script on the remote (`ssh host sh -l -s -- <key>`, script on
//!    stdin; 90 s for the dev node-script runner, 15 min for the release-archive runner because the
//!    first launch downloads about 70 MB). The script prints `{"remotePort":N,"serverKind":...}`
//!    after starting a managed server, or adopting the one already running in `~/.t3` ("external").
//! 4. A free loopback port is reserved and `ssh -N -L <local>:127.0.0.1:<remote>` is spawned
//!    (`ExitOnForwardFailure`, no control master, keepalives); the tunnel is ready when the
//!    forwarded port answers `GET /` within 20 s, and fails at once if ssh exits first.
//! 5. With `issue_pairing_token`, the pairing script runs the remote CLI
//!    (`auth pairing create --json`) and the credential comes back as the bootstrap token.
//!
//! Closing a tunnel (stale tunnel, [`SshEnvironmentManager::shutdown`], a failed reconnect) kills the
//! ssh child and stops a *managed* remote server (never an "external" one, which belongs to
//! someone else); an explicit [`SshEnvironmentManager::disconnect_environment`] does the same but
//! lets the stop failure reach the caller.
//!
//! # Authentication
//!
//! Every ssh operation runs through [`SshEnvironmentManager`]'s auth loop: with a prompt service
//! available ssh runs `BatchMode=yes` and, when it fails with a permission-denied/authentication
//! message ([`crate::ssh_auth::is_ssh_auth_failure`]), the user is asked for a password (up to two
//! attempts) and the operation is retried with `SSH_ASKPASS` serving the cached password. The password is
//! remembered per tunnel key until it is rejected. Without a prompt service ssh uses its own
//! interactive prompts.
//!
//! # Testing seams
//!
//! Processes go through [`SshSpawner`] ([`ProcessSshSpawner`] runs a real executable, tests point it
//! at a fake `ssh` script at an absolute path), HTTP readiness through [`HttpProbe`], and loopback
//! ports through [`LocalPorts`]. All waits use `tokio::time`, so tests run under paused time.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::ssh::{
    SshError, SshInvalidTarget, SshTarget, base_ssh_args, build_ssh_host_spec,
    get_last_non_empty_output_line, redact_ssh_error_output, remote_state_key,
    target_connection_key,
};
use crate::ssh_auth::{
    BatchMode, SshAuthHost, SshAuthOptions, SshChildEnvironmentOptions, SshPasswordPrompt,
    SshPasswordPromptError, SshPasswordRequest, build_ssh_child_environment, is_ssh_auth_failure,
};
use crate::ssh_scripts::*;
use crate::util::js_trim;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

const DEFAULT_SSH_COMMAND_TIMEOUT: Duration = Duration::from_millis(60_000);
/// A tunnel that was ready before must answer again within this to be reused.
const REUSE_READY_TIMEOUT: Duration = Duration::from_millis(2_000);

// ---- errors ------------------------------------------------------------------------------------

/// The errors of `SshEnvironmentEffectError`, tagged as the original tags them.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SshEnvironmentError {
    /// `SshCommandError` / `SshInvalidTargetError`.
    #[error(transparent)]
    Ssh(#[from] SshError),
    /// `SshLaunchError`
    #[error("{message}")]
    Launch {
        message: String,
        stdout: String,
        cause: Option<String>,
    },
    /// `SshPairingError`
    #[error("{message}")]
    Pairing {
        message: String,
        stdout: String,
        cause: Option<String>,
    },
    /// `SshReadinessError`
    #[error("{message}")]
    Readiness { message: String },
    /// `SshPasswordPromptError`
    #[error("{message}")]
    PasswordPrompt {
        message: String,
        cause: Option<String>,
    },
    /// `NetService.NetError`
    #[error("{message}")]
    Net { message: String },
    /// `SshMissingRunnerError` / `SshInvalidArchiveVersionError`, which the original throws as defects.
    #[error(transparent)]
    Runner(#[from] RunnerScriptError),
}

impl From<SshInvalidTarget> for SshEnvironmentError {
    fn from(error: SshInvalidTarget) -> Self {
        Self::Ssh(error.into())
    }
}

impl From<SshPasswordPromptError> for SshEnvironmentError {
    fn from(error: SshPasswordPromptError) -> Self {
        Self::PasswordPrompt {
            message: error.message,
            cause: error.cause,
        }
    }
}

impl SshEnvironmentError {
    /// The `_tag` of the original error class.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Ssh(SshError::Command { .. }) => "SshCommandError",
            Self::Ssh(SshError::InvalidTarget(_)) => "SshInvalidTargetError",
            Self::Launch { .. } => "SshLaunchError",
            Self::Pairing { .. } => "SshPairingError",
            Self::Readiness { .. } => "SshReadinessError",
            Self::PasswordPrompt { .. } => "SshPasswordPromptError",
            Self::Net { .. } => "NetError",
            Self::Runner(RunnerScriptError::MissingRunner) => "SshMissingRunnerError",
            Self::Runner(RunnerScriptError::InvalidArchiveVersion { .. }) => {
                "SshInvalidArchiveVersionError"
            }
        }
    }

    /// The desktop treats this one as the user dismissing the prompt, not a failure.
    pub fn is_password_prompt_error(&self) -> bool {
        matches!(self, Self::PasswordPrompt { .. })
    }
}

// ---- processes -----------------------------------------------------------------------------------

/// What an ssh process produced, once it has exited and its pipes are drained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub stdout: String,
    pub stderr: String,
    /// The exit code; `128 + signal` for a process a signal ended.
    pub exit_code: i32,
}

pub struct SshSpawnRequest {
    pub args: Vec<String>,
    /// Added to the inherited environment.
    pub env: BTreeMap<String, String>,
    /// Written to the process's stdin, which is then closed (`None` closes it at once).
    pub stdin: Option<String>,
}

/// A spawned ssh process.
#[async_trait]
pub trait SshProcess: Send + Sync {
    fn pid(&self) -> Option<u32>;
    /// Resolves when the process has exited and its output is drained; callable any number of times.
    async fn wait(&self) -> io::Result<ProcessOutput>;
    fn is_running(&self) -> bool;
    /// SIGTERM, then SIGKILL if it is still alive after `grace`. Returns when it has exited.
    async fn kill(&self, grace: Duration);
}

#[async_trait]
pub trait SshSpawner: Send + Sync {
    async fn spawn(&self, request: SshSpawnRequest) -> io::Result<Arc<dyn SshProcess>>;
}

/// Spawns a real executable (`ssh`, or a fake at an absolute path in tests).
#[derive(Debug, Clone)]
pub struct ProcessSshSpawner {
    pub executable: String,
}

impl ProcessSshSpawner {
    pub fn new(executable: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
}

type ExitSlot = Option<Result<ProcessOutput, String>>;

struct RealProcess {
    pid: Option<u32>,
    exit: tokio::sync::watch::Receiver<ExitSlot>,
}

fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: i32) {
    // SAFETY: `pid` is the id this process captured when it spawned the child, and it signals only
    // that child; a pid that has been reaped makes `kill` fail with ESRCH, which is ignored.
    unsafe {
        libc::kill(pid as i32, signal);
    }
}

#[async_trait]
impl SshSpawner for ProcessSshSpawner {
    async fn spawn(&self, request: SshSpawnRequest) -> io::Result<Arc<dyn SshProcess>> {
        let mut child = tokio::process::Command::new(&self.executable)
            .args(&request.args)
            .envs(&request.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id();
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let input = request.stdin;
        let (tx, rx) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            let feed = async move {
                if let Some(input) = input {
                    let _ = stdin.write_all(input.as_bytes()).await;
                }
                drop(stdin);
            };
            let (_, out, err, status) = tokio::join!(
                feed,
                async {
                    let mut buf = Vec::new();
                    stdout.read_to_end(&mut buf).await.map(|_| buf)
                },
                async {
                    let mut buf = Vec::new();
                    stderr.read_to_end(&mut buf).await.map(|_| buf)
                },
                child.wait()
            );
            let result = match (out, err, status) {
                (Ok(out), Ok(err), Ok(status)) => Ok(ProcessOutput {
                    stdout: String::from_utf8_lossy(&out).into_owned(),
                    stderr: String::from_utf8_lossy(&err).into_owned(),
                    exit_code: exit_code_of(status),
                }),
                (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                    Err(error.to_string())
                }
            };
            let _ = tx.send(Some(result));
        });
        Ok(Arc::new(RealProcess { pid, exit: rx }))
    }
}

#[async_trait]
impl SshProcess for RealProcess {
    fn pid(&self) -> Option<u32> {
        self.pid
    }

    async fn wait(&self) -> io::Result<ProcessOutput> {
        let mut exit = self.exit.clone();
        loop {
            if let Some(result) = exit.borrow_and_update().clone() {
                return result.map_err(io::Error::other);
            }
            if exit.changed().await.is_err() {
                // The monitor task ended without reporting (the runtime is shutting down).
                return Err(io::Error::other("the ssh process monitor stopped"));
            }
        }
    }

    fn is_running(&self) -> bool {
        self.exit.borrow().is_none()
    }

    async fn kill(&self, grace: Duration) {
        if !self.is_running() {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            send_signal(pid, libc::SIGTERM);
            if tokio::time::timeout(grace, self.wait()).await.is_err() {
                send_signal(pid, libc::SIGKILL);
            }
            let _ = self.wait().await;
        }
        #[cfg(not(unix))]
        {
            let _ = grace;
            let _ = self.wait().await;
        }
    }
}

// ---- services --------------------------------------------------------------------------------------

/// `NetService`: the two checks the tunnel needs.
#[async_trait]
pub trait LocalPorts: Send + Sync {
    /// `reserveLoopbackPort`: an ephemeral loopback port, released again immediately.
    async fn reserve_loopback_port(&self) -> Result<u16, String>;
    /// `canListenOnHost`: whether a TCP server could bind there right now.
    async fn can_listen_on_host(&self, port: u16, host: &str) -> bool;
}

/// Binds real sockets on the loopback interface.
pub struct SystemLocalPorts;

#[async_trait]
impl LocalPorts for SystemLocalPorts {
    async fn reserve_loopback_port(&self) -> Result<u16, String> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| e.to_string())?;
        listener
            .local_addr()
            .map(|a| a.port())
            .map_err(|e| e.to_string())
    }

    async fn can_listen_on_host(&self, port: u16, host: &str) -> bool {
        match tokio::net::TcpListener::bind((host, port)).await {
            Ok(_) => true,
            // IPv6-absent hosts must not fail availability checks.
            Err(error) => error.kind() == io::ErrorKind::AddrNotAvailable,
        }
    }
}

/// One HTTP GET for readiness: the status, or why there was none.
#[async_trait]
pub trait HttpProbe: Send + Sync {
    async fn get(&self, url: &str) -> Result<u16, String>;
}

/// Probes with `reqwest`.
pub struct ReqwestHttpProbe;

#[async_trait]
impl HttpProbe for ReqwestHttpProbe {
    async fn get(&self, url: &str) -> Result<u16, String> {
        let response = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        // The original drains the body before judging the response.
        let _ = response.bytes().await;
        Ok(status)
    }
}

/// Everything the tunnel code calls out to.
#[derive(Clone)]
pub struct SshRuntime {
    pub spawner: Arc<dyn SshSpawner>,
    pub prompt: Arc<dyn SshPasswordPrompt>,
    pub ports: Arc<dyn LocalPorts>,
    pub probe: Arc<dyn HttpProbe>,
    pub auth_host: SshAuthHost,
    /// `ssh` or `ssh.exe`, as diagnostics show the command.
    pub ssh_command: String,
}

impl SshRuntime {
    /// Real processes, sockets and HTTP; the prompt service is the shell's.
    pub fn system(prompt: Arc<dyn SshPasswordPrompt>) -> Self {
        let ssh_command = crate::ssh::ssh_command_for_platform(cfg!(windows));
        Self {
            spawner: Arc::new(ProcessSshSpawner::new(ssh_command)),
            prompt,
            ports: Arc::new(SystemLocalPorts),
            probe: Arc::new(ReqwestHttpProbe),
            auth_host: SshAuthHost::from_process(),
            ssh_command: ssh_command.to_owned(),
        }
    }
}

// ---- running one ssh command ---------------------------------------------------------------------------

/// `RunSshCommandOptions`
#[derive(Debug, Clone, Default)]
pub struct RunSshCommandOptions {
    pub auth: SshAuthOptions,
    pub pre_host_args: Vec<String>,
    pub remote_command_args: Vec<String>,
    pub stdin: Option<String>,
    pub timeout: Option<Duration>,
}

pub use crate::ssh::SshCommandResult;

fn normalize_ssh_error_message(stdout: &str, stderr: &str, fallback: &str) -> String {
    let stderr = js_trim(stderr);
    if !stderr.is_empty() {
        return stderr.to_owned();
    }
    let stdout = js_trim(stdout);
    if stdout.is_empty() {
        fallback.to_owned()
    } else {
        stdout.to_owned()
    }
}

fn prepare_child_environment(
    rt: &SshRuntime,
    auth: &SshAuthOptions,
) -> Result<BTreeMap<String, String>, SshError> {
    build_ssh_child_environment(
        &SshChildEnvironmentOptions {
            interactive_auth: auth.interactive_auth.unwrap_or(false),
            auth_secret: auth.auth_secret.clone(),
            ..Default::default()
        },
        &rt.auth_host,
    )
    .map_err(|_| SshError::Command {
        command: vec!["ssh".to_owned()],
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        message: "Failed to prepare SSH authentication helpers.".to_owned(),
    })
}

/// `runSshCommand`: one ssh invocation to completion.
///
/// TODO(original packages/ssh/src/command.ts `buildSshHostSpec`): the host spec is an ssh argument
/// with no `--` before it, so a target alias starting with `-` is read as an ssh option (see the
/// note in [`crate::ssh`]). Copied. Failures keep the original's messages: the
/// trimmed stderr, else redacted stdout, else `SSH command failed for <host> (exit N).`.
pub async fn run_ssh_command(
    rt: &SshRuntime,
    target: &SshTarget,
    input: RunSshCommandOptions,
) -> Result<SshCommandResult, SshError> {
    let host_spec = build_ssh_host_spec(target)?;
    let environment = prepare_child_environment(rt, &input.auth)?;
    let batch_mode = input.auth.batch_mode.map(BatchMode::as_str).unwrap_or(
        if input.auth.interactive_auth.unwrap_or(false) {
            "no"
        } else {
            "yes"
        },
    );
    let mut args = base_ssh_args(target, Some(batch_mode));
    args.extend(input.pre_host_args.iter().cloned());
    args.push(host_spec.clone());
    args.extend(input.remote_command_args.iter().cloned());
    let timeout = input.timeout.unwrap_or(DEFAULT_SSH_COMMAND_TIMEOUT);
    let ssh_args: Vec<String> = std::iter::once("ssh".to_owned())
        .chain(args.iter().cloned())
        .collect();
    tracing::debug!(
        alias = %target.alias,
        command = ?ssh_args,
        has_stdin = input.stdin.is_some(),
        timeout_ms = timeout.as_millis() as u64,
        "ssh.command.start"
    );
    let failure = |message: String, exit_code: Option<i32>, stdout: String, stderr: String| {
        SshError::Command {
            command: ssh_args.clone(),
            exit_code,
            stdout,
            stderr,
            message,
        }
    };
    let process = rt
        .spawner
        .spawn(SshSpawnRequest {
            args: args.clone(),
            env: environment,
            stdin: input.stdin.clone(),
        })
        .await
        .map_err(|error| SshError::Command {
            command: std::iter::once(rt.ssh_command.clone())
                .chain(args.iter().cloned())
                .collect(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            message: error.to_string(),
        })?;
    let output = match tokio::time::timeout(timeout, process.wait()).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => {
            return Err(failure(
                format!("Failed to run SSH command for {host_spec}."),
                None,
                String::new(),
                String::new(),
            ));
        }
        Err(_) => {
            process.kill(TUNNEL_SHUTDOWN_TIMEOUT).await;
            tracing::warn!(alias = %target.alias, timeout_ms = timeout.as_millis() as u64, "ssh.command.timedOut");
            return Err(SshError::Command {
                command: vec!["ssh".to_owned()],
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                message: format!("SSH command timed out after {}ms.", timeout.as_millis()),
            });
        }
    };
    if output.exit_code != 0 {
        let diagnostic_stdout = redact_ssh_error_output(&output.stdout);
        tracing::warn!(alias = %target.alias, exit_code = output.exit_code, "ssh.command.failed");
        let message = normalize_ssh_error_message(
            &diagnostic_stdout,
            &output.stderr,
            &format!(
                "SSH command failed for {host_spec} (exit {}).",
                output.exit_code
            ),
        );
        return Err(failure(
            message,
            Some(output.exit_code),
            diagnostic_stdout,
            output.stderr,
        ));
    }
    Ok(SshCommandResult {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// `resolveSshTarget`: `ssh -G <alias>` into a target; any failure falls back to the alias itself.
pub async fn resolve_ssh_target(
    rt: &SshRuntime,
    alias: &str,
) -> Result<SshTarget, SshInvalidTarget> {
    let alias = js_trim(alias);
    if alias.is_empty() {
        return Err(SshInvalidTarget {
            message: "SSH host alias is required.".to_owned(),
        });
    }
    let fallback = SshTarget {
        alias: alias.to_owned(),
        hostname: alias.to_owned(),
        username: None,
        port: None,
    };
    let result = run_ssh_command(
        rt,
        &fallback,
        RunSshCommandOptions {
            pre_host_args: vec!["-G".to_owned()],
            ..Default::default()
        },
    )
    .await;
    Ok(match result {
        Ok(output) => crate::ssh::parse_ssh_resolve_output(alias, &output.stdout),
        Err(_) => fallback,
    })
}

// ---- readiness -----------------------------------------------------------------------------------------

/// `waitForHttpReady`'s input; defaults are the shared module's (30 s overall, 100 ms between
/// attempts) with the SSH wrapper's 1 s per probe.
#[derive(Debug, Clone)]
pub struct WaitForHttpReadyInput {
    pub base_url: String,
    pub path: Option<String>,
    pub timeout: Option<Duration>,
    pub interval: Option<Duration>,
    pub probe_timeout: Option<Duration>,
}

impl WaitForHttpReadyInput {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            path: None,
            timeout: None,
            interval: None,
            probe_timeout: None,
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// `describeReadinessCause` for plain data: keeps `_tag`, `message`, and the nested `reason` and
/// `cause` of an object, and passes anything else through.
pub fn describe_readiness_cause(cause: &Value) -> Value {
    let Value::Object(record) = cause else {
        return cause.clone();
    };
    let mut described = serde_json::Map::new();
    if let Some(tag @ Value::String(_)) = record.get("_tag") {
        described.insert("_tag".to_owned(), tag.clone());
    }
    if let Some(message @ Value::String(_)) = record.get("message") {
        described.insert("message".to_owned(), message.clone());
    }
    for key in ["reason", "cause"] {
        if let Some(nested) = record.get(key) {
            described.insert(key.to_owned(), describe_readiness_cause(nested));
        }
    }
    Value::Object(described)
}

/// `waitForHttpReady`: polls `base_url + path` until it answers 2xx, or fails with the original's
/// `SshReadinessError` messages. Each probe is bounded by the probe timeout so one hung request
/// cannot stall the retries, and attempts are `interval` apart, at most `ceil(timeout / interval)`
/// retries after the first.
pub async fn wait_for_http_ready(
    probe: &dyn HttpProbe,
    input: WaitForHttpReadyInput,
) -> Result<(), SshEnvironmentError> {
    let timeout = input.timeout.unwrap_or(Duration::from_millis(30_000));
    let interval = input.interval.unwrap_or(Duration::from_millis(100));
    let probe_timeout = input.probe_timeout.unwrap_or(SSH_READY_PROBE_TIMEOUT);
    let retries = if interval.is_zero() {
        0
    } else {
        timeout.as_nanos().div_ceil(interval.as_nanos()) as u64
    };
    let request_url = url::Url::parse(&input.base_url)
        .and_then(|base| base.join(input.path.as_deref().unwrap_or("/")))
        .map(|u| u.to_string())
        .map_err(|_| SshEnvironmentError::Readiness {
            message: "Invalid URL".to_owned(),
        })?;
    tracing::debug!(base_url = %input.base_url, %request_url, "httpReadiness.start");
    let readiness_error = |message: String| SshEnvironmentError::Readiness { message };
    let generic = || {
        readiness_error(format!(
            "Backend readiness probe failed at {request_url}."
        ))
    };
    let last_failure: Mutex<Value> = Mutex::new(Value::Null);
    let attempts = async {
        let mut attempt: u64 = 0;
        let mut retried: u64 = 0;
        loop {
            attempt += 1;
            let failure = match tokio::time::timeout(probe_timeout, probe.get(&request_url)).await
            {
                Ok(Ok(status)) if (200..300).contains(&status) => return Ok(()),
                Ok(Ok(status)) => {
                    (generic(), json!({"_tag": "HttpClientError", "message": format!("status {status}")}))
                }
                Ok(Err(message)) => (generic(), json!({"message": message})),
                Err(_) => (
                    readiness_error(format!(
                        "Backend readiness probe exceeded {}ms at {request_url}.",
                        probe_timeout.as_millis()
                    )),
                    json!({"kind": "probe-timeout", "attempt": attempt, "probeTimeoutMs": probe_timeout.as_millis() as u64}),
                ),
            };
            *last_failure.lock().unwrap() =
                json!({"attempt": attempt, "cause": describe_readiness_cause(&failure.1)});
            if retried >= retries {
                return Err(failure.0);
            }
            retried += 1;
            tokio::time::sleep(interval).await;
        }
    };
    match tokio::time::timeout(timeout, attempts).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(base_url = %input.base_url, last_failure = %last_failure.lock().unwrap(), "httpReadiness.timedOut");
            Err(readiness_error(format!(
                "Timed out waiting {}ms for backend readiness at {}.",
                timeout.as_millis(),
                input.base_url
            )))
        }
    }
}

// ---- remote operations ---------------------------------------------------------------------------------

/// What `launchOrReuseRemoteServer` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteLaunch {
    pub remote_port: i64,
    pub remote_server_kind: Option<RemoteServerKind>,
}

fn auth_fields(command: &mut RunSshCommandOptions, auth: &SshAuthOptions) {
    command.auth = auth.clone();
}

/// `launchOrReuseRemoteServer`: runs the launch script and reads the port it printed.
pub async fn launch_or_reuse_remote_server(
    rt: &SshRuntime,
    target: &SshTarget,
    auth: &SshAuthOptions,
    runner: Option<&RemoteT3RunnerOptions>,
) -> Result<RemoteLaunch, SshEnvironmentError> {
    let state_key = remote_state_key(target);
    let (runner_kind, runner_detail) = ssh_runner_log_fields(runner);
    tracing::info!(alias = %target.alias, runner = runner_kind, runner_detail = ?runner_detail, %state_key, "ssh.remoteServer.launch.start");
    let mut command = RunSshCommandOptions {
        remote_command_args: ["sh", "-l", "-s", "--"]
            .into_iter()
            .map(str::to_owned)
            .chain([state_key.clone()])
            .collect(),
        stdin: Some(build_remote_launch_script(runner)?),
        timeout: Some(if is_node_script_runner(runner) {
            REMOTE_LAUNCH_TIMEOUT
        } else {
            REMOTE_ARCHIVE_LAUNCH_TIMEOUT
        }),
        ..Default::default()
    };
    auth_fields(&mut command, auth);
    let result = run_ssh_command(rt, target, command).await?;
    if get_last_non_empty_output_line(&result.stdout).is_none() {
        return Err(SshEnvironmentError::Launch {
            message: "SSH launch did not return a remote port.".to_owned(),
            stdout: result.stdout,
            cause: None,
        });
    }
    let Some(parsed) = decode_remote_launch_output(&result.stdout) else {
        return Err(SshEnvironmentError::Launch {
            message: "SSH launch returned unparseable output.".to_owned(),
            stdout: result.stdout,
            cause: Some("the output is not the expected JSON".to_owned()),
        });
    };
    let Some(remote_port) = json_integer(parsed.remote_port) else {
        return Err(SshEnvironmentError::Launch {
            message: format!(
                "SSH launch returned an invalid remote port: {}.",
                describe_remote_port(parsed.remote_port)
            ),
            stdout: result.stdout,
            cause: None,
        });
    };
    tracing::info!(alias = %target.alias, remote_port, server_kind = ?parsed.server_kind, %state_key, "ssh.remoteServer.launch.ready");
    Ok(RemoteLaunch {
        remote_port,
        remote_server_kind: parsed.server_kind,
    })
}

/// `issueRemotePairingToken`: the credential of a pairing link made by the remote CLI.
pub async fn issue_remote_pairing_token(
    rt: &SshRuntime,
    target: &SshTarget,
    auth: &SshAuthOptions,
    runner: Option<&RemoteT3RunnerOptions>,
) -> Result<String, SshEnvironmentError> {
    let state_key = remote_state_key(target);
    tracing::debug!(alias = %target.alias, %state_key, "ssh.remoteServer.pairingToken.start");
    let mut command = RunSshCommandOptions {
        remote_command_args: vec!["sh".to_owned(), "-s".to_owned()],
        stdin: Some(build_remote_pairing_script(&state_key, runner)?),
        // Pairing may be the first command on a cold remote, so it can install the archive.
        timeout: (!is_node_script_runner(runner)).then_some(REMOTE_ARCHIVE_LAUNCH_TIMEOUT),
        ..Default::default()
    };
    auth_fields(&mut command, auth);
    let result = run_ssh_command(rt, target, command).await?;
    if get_last_non_empty_output_line(&result.stdout).is_none() {
        return Err(SshEnvironmentError::Pairing {
            message: "SSH pairing did not return a credential.".to_owned(),
            stdout: result.stdout,
            cause: None,
        });
    }
    let Some(credential) = decode_remote_pairing_output(&result.stdout) else {
        return Err(SshEnvironmentError::Pairing {
            message: "SSH pairing returned unparseable output.".to_owned(),
            stdout: result.stdout,
            cause: Some("the output is not the expected JSON".to_owned()),
        });
    };
    if js_trim(&credential).is_empty() {
        return Err(SshEnvironmentError::Pairing {
            message: "SSH pairing command returned an invalid credential.".to_owned(),
            stdout: result.stdout,
            cause: None,
        });
    }
    tracing::debug!(alias = %target.alias, %state_key, "ssh.remoteServer.pairingToken.created");
    Ok(credential)
}

/// `stopRemoteServer`: stops the managed server (an "external" one is left alone) and clears its
/// ownership files; fails, keeping them, when the server does not stop within 2 seconds.
pub async fn stop_remote_server(
    rt: &SshRuntime,
    target: &SshTarget,
    auth: &SshAuthOptions,
) -> Result<(), SshError> {
    let state_key = remote_state_key(target);
    tracing::info!(alias = %target.alias, %state_key, "ssh.remoteServer.stop.start");
    let mut command = RunSshCommandOptions {
        remote_command_args: vec!["sh".to_owned(), "-s".to_owned()],
        stdin: Some(build_remote_stop_script(&state_key)),
        ..Default::default()
    };
    auth_fields(&mut command, auth);
    run_ssh_command(rt, target, command).await?;
    tracing::info!(alias = %target.alias, %state_key, "ssh.remoteServer.stop.succeeded");
    Ok(())
}

/// `readRemoteServerLogTail`: the last 80 lines of the remote server's log.
pub async fn read_remote_server_log_tail(
    rt: &SshRuntime,
    target: &SshTarget,
    auth: &SshAuthOptions,
) -> Result<String, SshError> {
    let mut command = RunSshCommandOptions {
        remote_command_args: vec!["sh".to_owned(), "-s".to_owned()],
        stdin: Some(build_remote_log_tail_script(&remote_state_key(target))),
        timeout: Some(REMOTE_LOG_TAIL_TIMEOUT),
        ..Default::default()
    };
    auth_fields(&mut command, auth);
    let result = run_ssh_command(rt, target, command).await?;
    Ok(js_trim(&result.stdout).to_owned())
}

// ---- tunnels ---------------------------------------------------------------------------------------------

/// The result of `ensureEnvironment` (`DesktopSshEnvironmentBootstrap`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshEnvironmentBootstrap {
    pub target: SshTarget,
    pub http_base_url: String,
    pub ws_base_url: String,
    pub pairing_token: Option<String>,
    pub remote_port: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_server_kind: Option<RemoteServerKind>,
}

/// A running `ssh -N -L` and what it forwards.
pub struct SshTunnelEntry {
    pub key: String,
    pub target: SshTarget,
    pub remote_port: i64,
    pub remote_server_kind: Option<RemoteServerKind>,
    pub local_port: u16,
    pub http_base_url: String,
    pub ws_base_url: String,
    process: Arc<dyn SshProcess>,
    closed: AtomicBool,
}

struct StartTunnel<'a> {
    key: &'a str,
    resolved_target: &'a SshTarget,
    remote_port: i64,
    local_port: u16,
    http_base_url: &'a str,
    ws_base_url: &'a str,
    auth: &'a SshAuthOptions,
    remote_server_kind: Option<RemoteServerKind>,
}

fn tunnel_args(input: &StartTunnel<'_>, host_spec: &str) -> Vec<String> {
    let mut args = base_ssh_args(
        input.resolved_target,
        Some(input.auth.batch_mode.map(BatchMode::as_str).unwrap_or("no")),
    );
    for option in [
        "ExitOnForwardFailure=yes",
        "ControlMaster=no",
        "ControlPath=none",
        "ControlPersist=no",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
    ] {
        args.push("-o".to_owned());
        args.push(option.to_owned());
    }
    args.extend(["-n".to_owned(), "-N".to_owned(), "-L".to_owned()]);
    args.push(format!(
        "{}:127.0.0.1:{}",
        input.local_port, input.remote_port
    ));
    args.push(host_spec.to_owned());
    args
}

/// The arguments `ssh` is started with for a tunnel (`startSshTunnel`).
pub fn build_tunnel_args(
    target: &SshTarget,
    local_port: u16,
    remote_port: i64,
    batch_mode: Option<BatchMode>,
) -> Result<Vec<String>, SshInvalidTarget> {
    let host_spec = build_ssh_host_spec(target)?;
    let auth = SshAuthOptions {
        batch_mode,
        ..Default::default()
    };
    Ok(tunnel_args(
        &StartTunnel {
            key: "",
            resolved_target: target,
            remote_port,
            local_port,
            http_base_url: "",
            ws_base_url: "",
            auth: &auth,
            remote_server_kind: None,
        },
        &host_spec,
    ))
}

/// `startSshTunnel`: spawns the forward and waits until it answers (or ssh exits first).
async fn start_ssh_tunnel(
    rt: &SshRuntime,
    input: StartTunnel<'_>,
) -> Result<Arc<SshTunnelEntry>, SshEnvironmentError> {
    let target = input.resolved_target;
    let host_spec = build_ssh_host_spec(target)?;
    let environment = prepare_child_environment(rt, input.auth)?;
    let args = tunnel_args(&input, &host_spec);
    let tunnel_command: Vec<String> = std::iter::once(rt.ssh_command.clone())
        .chain(args.iter().cloned())
        .collect();
    tracing::debug!(alias = %target.alias, command = ?tunnel_command, local_port = input.local_port, remote_port = input.remote_port, "ssh.tunnel.spawn.start");
    let process = rt
        .spawner
        .spawn(SshSpawnRequest {
            args: args.clone(),
            env: environment,
            stdin: None,
        })
        .await
        .map_err(|error| SshError::Command {
            command: tunnel_command.clone(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            message: error.to_string(),
        })?;
    tracing::debug!(alias = %target.alias, pid = ?process.pid(), "ssh.tunnel.spawn.succeeded");
    let entry = Arc::new(SshTunnelEntry {
        key: input.key.to_owned(),
        target: target.clone(),
        remote_port: input.remote_port,
        remote_server_kind: input.remote_server_kind,
        local_port: input.local_port,
        http_base_url: input.http_base_url.to_owned(),
        ws_base_url: input.ws_base_url.to_owned(),
        process: process.clone(),
        closed: AtomicBool::new(false),
    });
    let ready = wait_for_http_ready(
        rt.probe.as_ref(),
        WaitForHttpReadyInput::new(input.http_base_url).timeout(SSH_READY_TIMEOUT),
    );
    let exit_failure = async {
        match process.wait().await {
            Ok(output) => {
                tracing::warn!(alias = %target.alias, exit_code = output.exit_code, "ssh.tunnel.process.exited");
                Err::<(), _>(SshEnvironmentError::Ssh(SshError::Command {
                    command: tunnel_command.clone(),
                    exit_code: Some(output.exit_code),
                    message: normalize_ssh_error_message(
                        "",
                        &output.stderr,
                        &format!(
                            "SSH tunnel exited unexpectedly for {} (exit {}).",
                            target.alias, output.exit_code
                        ),
                    ),
                    stdout: String::new(),
                    stderr: output.stderr,
                }))
            }
            Err(error) => Err(SshEnvironmentError::Ssh(SshError::Command {
                command: tunnel_command.clone(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                message: error.to_string(),
            })),
        }
    };
    let raced = tokio::select! {
        result = ready => result,
        result = exit_failure => result,
    };
    match raced {
        Ok(()) => {
            tracing::info!(alias = %target.alias, pid = ?process.pid(), "ssh.tunnel.ready");
            Ok(entry)
        }
        Err(cause) => {
            let running = process.is_running();
            let listening = !rt.ports.can_listen_on_host(input.local_port, "127.0.0.1").await;
            let log_tail = read_remote_server_log_tail(rt, target, input.auth).await.ok();
            tracing::warn!(alias = %target.alias, pid = ?process.pid(), process_running = running, local_port_listening = listening, remote_log_tail = ?log_tail.filter(|t| !t.is_empty()), cause = %cause, "ssh.tunnel.ready.failed");
            process.kill(TUNNEL_SHUTDOWN_TIMEOUT).await;
            Err(cause)
        }
    }
}

// ---- the manager -------------------------------------------------------------------------------------------

/// Mutual exclusion per key (`KeyedLock`): holders of one key run one at a time in arrival order,
/// other keys are independent, and a key's lock exists only while someone holds or waits on it. Not
/// reentrant.
#[derive(Default)]
pub struct KeyedLock {
    locks: Mutex<HashMap<String, (Arc<tokio::sync::Mutex<()>>, usize)>>,
}

impl KeyedLock {
    pub async fn with_lock<F: Future>(&self, key: &str, work: F) -> F::Output {
        struct Release<'a> {
            lock: &'a KeyedLock,
            key: &'a str,
        }
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                let mut locks = self.lock.locks.lock().unwrap();
                if let Some(entry) = locks.get_mut(self.key) {
                    entry.1 -= 1;
                    if entry.1 == 0 {
                        locks.remove(self.key);
                    }
                }
            }
        }
        let mutex = {
            let mut locks = self.locks.lock().unwrap();
            let entry = locks
                .entry(key.to_owned())
                .or_insert_with(|| (Arc::new(tokio::sync::Mutex::new(())), 0));
            entry.1 += 1;
            entry.0.clone()
        };
        let _release = Release { lock: self, key };
        let _permit = mutex.lock().await;
        work.await
    }

    /// The keys someone holds or waits on right now.
    pub fn active_keys(&self) -> Vec<String> {
        self.locks.lock().unwrap().keys().cloned().collect()
    }
}

/// `SshEnvironmentManagerOptions`: `resolve_cli_runner` says which `t3` the remote runs, asked on
/// every `ensure_environment`.
#[derive(Default)]
pub struct SshEnvironmentManagerOptions {
    pub resolve_cli_runner: Option<Box<dyn Fn() -> RemoteT3RunnerOptions + Send + Sync>>,
}

struct ManagerInner {
    rt: SshRuntime,
    options: SshEnvironmentManagerOptions,
    tunnels: Mutex<HashMap<String, Arc<SshTunnelEntry>>>,
    target_locks: KeyedLock,
    auth_secrets: Mutex<HashMap<String, String>>,
}

/// `SshEnvironmentManager`. Cheap to clone; clones share the same tunnels.
#[derive(Clone)]
pub struct SshEnvironmentManager {
    inner: Arc<ManagerInner>,
}

fn merge_target(base: SshTarget, requested: &SshTarget) -> SshTarget {
    SshTarget {
        username: requested.username.clone().or(base.username),
        port: requested.port.or(base.port),
        ..base
    }
}

impl SshEnvironmentManager {
    pub fn new(rt: SshRuntime, options: SshEnvironmentManagerOptions) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                rt,
                options,
                tunnels: Mutex::new(HashMap::new()),
                target_locks: KeyedLock::default(),
                auth_secrets: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The keys of the tunnels currently registered.
    pub fn tunnel_keys(&self) -> Vec<String> {
        self.inner.tunnels.lock().unwrap().keys().cloned().collect()
    }

    /// `ensureEnvironment`: a ready tunnel to the target's t3 server, launching the server and the
    /// tunnel as needed, plus a pairing credential when asked.
    pub async fn ensure_environment(
        &self,
        target: &SshTarget,
        issue_pairing_token: bool,
    ) -> Result<SshEnvironmentBootstrap, SshEnvironmentError> {
        let inner = &self.inner;
        tracing::info!(alias = %target.alias, issue_pairing_token, "ssh.environment.ensure.start");
        let resolved = self.resolve(target).await?;
        let key = target_connection_key(&resolved);
        let runner = inner.options.resolve_cli_runner.as_ref().map(|f| f());
        inner
            .target_locks
            .with_lock(&key, async {
                let entry = self.ensure_tunnel_entry(&key, &resolved, runner.as_ref()).await?;
                let pairing_token = if issue_pairing_token {
                    let target = entry.target.clone();
                    let runner = runner.clone();
                    let rt = inner.rt.clone();
                    Some(
                        self.run_with_ssh_auth(&key, &entry.target, move |auth| {
                            let (rt, target, runner) = (rt.clone(), target.clone(), runner.clone());
                            Box::pin(async move {
                                issue_remote_pairing_token(&rt, &target, &auth, runner.as_ref())
                                    .await
                            })
                        })
                        .await?,
                    )
                } else {
                    None
                };
                tracing::info!(alias = %entry.target.alias, local_port = entry.local_port, remote_port = entry.remote_port, issued_pairing_token = pairing_token.is_some(), "ssh.environment.ensure.succeeded");
                Ok(SshEnvironmentBootstrap {
                    target: entry.target.clone(),
                    http_base_url: entry.http_base_url.clone(),
                    ws_base_url: entry.ws_base_url.clone(),
                    pairing_token,
                    remote_port: entry.remote_port,
                    remote_server_kind: entry.remote_server_kind,
                })
            })
            .await
    }

    /// `disconnectEnvironment`: closes the tunnel and stops the remote server; unlike the
    /// close-time stop, a failure to stop it is returned.
    pub async fn disconnect_environment(
        &self,
        target: &SshTarget,
    ) -> Result<(), SshEnvironmentError> {
        let inner = &self.inner;
        tracing::info!(alias = %target.alias, "ssh.environment.disconnect.start");
        let resolved = self.resolve(target).await?;
        let key = target_connection_key(&resolved);
        inner
            .target_locks
            .with_lock(&key, async {
                let entry = inner.tunnels.lock().unwrap().remove(&key);
                if let Some(entry) = entry {
                    // Explicit disconnect owns the remote stop (below), so the entry is no longer
                    // registered when its close runs; the close finishes even if the caller goes away.
                    let manager = self.clone();
                    let _ = tokio::spawn(async move { manager.close_tunnel_entry(&entry).await })
                        .await;
                }
                let rt = inner.rt.clone();
                let resolved_for_stop = resolved.clone();
                self.run_with_ssh_auth(&key, &resolved, move |auth| {
                    let (rt, target) = (rt.clone(), resolved_for_stop.clone());
                    Box::pin(async move {
                        stop_remote_server(&rt, &target, &auth)
                            .await
                            .map_err(SshEnvironmentError::from)
                    })
                })
                .await?;
                tracing::info!(alias = %resolved.alias, "ssh.environment.disconnect.succeeded");
                Ok(())
            })
            .await
    }

    /// The manager scope's finalizer: closes every tunnel concurrently, stopping their managed
    /// remote servers on a best-effort basis. Call it when the app quits.
    pub async fn shutdown(&self) {
        let entries: Vec<Arc<SshTunnelEntry>> =
            self.inner.tunnels.lock().unwrap().values().cloned().collect();
        let mut closing = tokio::task::JoinSet::new();
        for entry in entries {
            let manager = self.clone();
            closing.spawn(async move { manager.close_tunnel_entry(&entry).await });
        }
        while closing.join_next().await.is_some() {}
    }

    async fn resolve(&self, target: &SshTarget) -> Result<SshTarget, SshEnvironmentError> {
        let alias = if target.alias.is_empty() {
            &target.hostname
        } else {
            &target.alias
        };
        let base = resolve_ssh_target(&self.inner.rt, alias).await?;
        Ok(merge_target(base, target))
    }

    /// `closeTunnelEntry` plus the entry scope's finalizer: runs once per entry.
    async fn close_tunnel_entry(&self, entry: &Arc<SshTunnelEntry>) {
        tracing::debug!(alias = %entry.target.alias, key = %entry.key, "ssh.tunnel.close.start");
        if entry.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let stop_remote = {
            let mut tunnels = self.inner.tunnels.lock().unwrap();
            let registered = tunnels
                .get(&entry.key)
                .is_some_and(|current| Arc::ptr_eq(current, entry));
            if registered {
                tunnels.remove(&entry.key);
            }
            registered
        };
        entry.process.kill(TUNNEL_SHUTDOWN_TIMEOUT).await;
        if stop_remote {
            let secret = self
                .inner
                .auth_secrets
                .lock()
                .unwrap()
                .get(&entry.key)
                .cloned();
            let auth = match secret {
                None => SshAuthOptions {
                    batch_mode: Some(BatchMode::Yes),
                    interactive_auth: Some(false),
                    ..Default::default()
                },
                Some(secret) => SshAuthOptions {
                    auth_secret: Some(Some(secret)),
                    batch_mode: Some(BatchMode::No),
                    interactive_auth: Some(true),
                },
            };
            if let Err(error) = stop_remote_server(&self.inner.rt, &entry.target, &auth).await {
                tracing::debug!(alias = %entry.target.alias, %error, "ssh.environment.tunnel.finalizer.failed");
            }
        }
        tracing::info!(alias = %entry.target.alias, key = %entry.key, "ssh.tunnel.close.succeeded");
    }

    async fn ensure_tunnel_entry(
        &self,
        key: &str,
        resolved: &SshTarget,
        runner: Option<&RemoteT3RunnerOptions>,
    ) -> Result<Arc<SshTunnelEntry>, SshEnvironmentError> {
        let existing = self.inner.tunnels.lock().unwrap().get(key).cloned();
        if let Some(entry) = existing {
            tracing::debug!(alias = %resolved.alias, key, "ssh.environment.tunnel.existing.check");
            let readiness = wait_for_http_ready(
                self.inner.rt.probe.as_ref(),
                WaitForHttpReadyInput::new(entry.http_base_url.clone()).timeout(REUSE_READY_TIMEOUT),
            )
            .await;
            match readiness {
                Ok(()) => {
                    tracing::debug!(alias = %resolved.alias, key, "ssh.environment.tunnel.reused");
                    return Ok(entry);
                }
                Err(cause) => {
                    tracing::warn!(alias = %resolved.alias, key, %cause, "ssh.environment.tunnel.existing.stale");
                    self.close_tunnel_entry(&entry).await;
                }
            }
        }
        self.create_tunnel_entry(key, resolved, runner)
            .await
            .inspect_err(|cause| {
                tracing::warn!(alias = %resolved.alias, key, %cause, "ssh.environment.tunnel.create.failed");
            })
    }

    async fn create_tunnel_entry(
        &self,
        key: &str,
        resolved: &SshTarget,
        runner: Option<&RemoteT3RunnerOptions>,
    ) -> Result<Arc<SshTunnelEntry>, SshEnvironmentError> {
        let rt = &self.inner.rt;
        tracing::debug!(alias = %resolved.alias, key, "ssh.environment.tunnel.create.start");
        let launch = {
            let (rt, target, runner) = (rt.clone(), resolved.clone(), runner.cloned());
            self.run_with_ssh_auth(key, resolved, move |auth| {
                let (rt, target, runner) = (rt.clone(), target.clone(), runner.clone());
                Box::pin(async move {
                    launch_or_reuse_remote_server(&rt, &target, &auth, runner.as_ref()).await
                })
            })
            .await?
        };
        tracing::debug!(alias = %resolved.alias, key, remote_port = launch.remote_port, kind = ?launch.remote_server_kind, "ssh.environment.remotePort.ready");
        let local_port = rt
            .ports
            .reserve_loopback_port()
            .await
            .map_err(|message| SshEnvironmentError::Net { message })?;
        let http_base_url = format!("http://127.0.0.1:{local_port}/");
        let ws_base_url = format!("ws://127.0.0.1:{local_port}/");
        tracing::debug!(alias = %resolved.alias, key, local_port, remote_port = launch.remote_port, "ssh.environment.localPort.reserved");
        let entry = {
            let (rt, target, key_owned) = (rt.clone(), resolved.clone(), key.to_owned());
            let (http, ws) = (http_base_url.clone(), ws_base_url.clone());
            self.run_with_ssh_auth(key, resolved, move |auth| {
                let (rt, target, key, http, ws) = (
                    rt.clone(),
                    target.clone(),
                    key_owned.clone(),
                    http.clone(),
                    ws.clone(),
                );
                Box::pin(async move {
                    start_ssh_tunnel(
                        &rt,
                        StartTunnel {
                            key: &key,
                            resolved_target: &target,
                            remote_port: launch.remote_port,
                            local_port,
                            http_base_url: &http,
                            ws_base_url: &ws,
                            auth: &auth,
                            remote_server_kind: launch.remote_server_kind,
                        },
                    )
                    .await
                })
            })
            .await?
        };
        self.inner
            .tunnels
            .lock()
            .unwrap()
            .insert(key.to_owned(), entry.clone());
        tracing::debug!(alias = %resolved.alias, key, local_port, remote_port = launch.remote_port, "ssh.environment.tunnel.create.succeeded");
        Ok(entry)
    }

    /// `promptForPassword`
    async fn prompt_for_password(
        &self,
        target: &SshTarget,
        attempt: u32,
    ) -> Result<String, SshEnvironmentError> {
        let prompt = &self.inner.rt.prompt;
        let host_spec = build_ssh_host_spec(target)?;
        if !prompt.is_available() {
            tracing::warn!(alias = %target.alias, attempt, "ssh.auth.passwordPrompt.unavailable");
            return Err(SshEnvironmentError::PasswordPrompt {
                message: format!("SSH authentication failed for {host_spec}."),
                cause: None,
            });
        }
        tracing::info!(alias = %target.alias, attempt, "ssh.auth.passwordPrompt.request");
        let destination = {
            let alias = js_trim(&target.alias);
            if alias.is_empty() {
                js_trim(&target.hostname)
            } else {
                alias
            }
        };
        let password = prompt
            .request(SshPasswordRequest {
                attempt,
                destination: destination.to_owned(),
                username: target.username.clone(),
                prompt: format!("Enter the SSH password for {host_spec}."),
            })
            .await?;
        match password {
            None => {
                tracing::warn!(alias = %target.alias, attempt, "ssh.auth.passwordPrompt.cancelled");
                Err(SshEnvironmentError::PasswordPrompt {
                    message: format!("SSH authentication cancelled for {host_spec}."),
                    cause: None,
                })
            }
            Some(password) => {
                tracing::info!(alias = %target.alias, attempt, "ssh.auth.passwordPrompt.received");
                Ok(password)
            }
        }
    }

    /// `runWithSshAuth`: runs `operation`; on an authentication failure with a prompt service, asks
    /// for a password (at most twice) and runs it again with the password served through
    /// `SSH_ASKPASS`. The password is remembered per `key` until a run rejects it.
    async fn run_with_ssh_auth<T>(
        &self,
        key: &str,
        target: &SshTarget,
        operation: impl Fn(SshAuthOptions) -> BoxFuture<'static, Result<T, SshEnvironmentError>>,
    ) -> Result<T, SshEnvironmentError> {
        let prompt = &self.inner.rt.prompt;
        let mut prompt_count: u32 = 0;
        let mut auth_secret: Option<String> = self.inner.auth_secrets.lock().unwrap().get(key).cloned();
        loop {
            let auth = match &auth_secret {
                None => SshAuthOptions {
                    auth_secret: None,
                    batch_mode: Some(if prompt.is_available() {
                        BatchMode::Yes
                    } else {
                        BatchMode::No
                    }),
                    interactive_auth: Some(!prompt.is_available()),
                },
                Some(secret) => SshAuthOptions {
                    auth_secret: Some(Some(secret.clone())),
                    batch_mode: Some(BatchMode::No),
                    interactive_auth: Some(true),
                },
            };
            let error = match operation(auth).await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            if !is_ssh_auth_failure(&error.to_string()) {
                return Err(error);
            }
            tracing::warn!(alias = %target.alias, key, prompt_count, %error, "ssh.auth.failed");
            if !prompt.is_available() {
                return Err(error);
            }
            if auth_secret.is_some() {
                self.inner.auth_secrets.lock().unwrap().remove(key);
            }
            if prompt_count >= 2 {
                return Err(error);
            }
            prompt_count += 1;
            let secret = self.prompt_for_password(target, prompt_count).await?;
            self.inner
                .auth_secrets
                .lock()
                .unwrap()
                .insert(key.to_owned(), secret.clone());
            auth_secret = Some(secret);
        }
    }
}
