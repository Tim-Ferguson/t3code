//! What a server that a service launcher started needs to update itself (`cloud/serviceLauncherClient.ts`'s default
//! `ServiceLauncherHostProcess`, `processRunner.ts` and the HTTP client `pinnedRuntime.ts` downloads with), supplied to
//! `self_update_rpc::build`.
//!
//! * [`IpcLauncherHost`] is the channel Node gives a child spawned with an `ipc` stdio entry: the descriptor named by
//!   `NODE_CHANNEL_FD`, carrying one JSON message per line (`NODE_CHANNEL_SERIALIZATION_MODE` `json`, the default; the
//!   `advanced` V8 serialization is not read, and such a channel is treated as absent).
//! * [`HostProcessRunner`] runs a command to completion with a timeout (60 s when none is given).
//! * [`HttpReleaseFeed`] opens a release asset with a plain GET; a non-2xx answer is a failure.
use async_trait::async_trait;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use t3_remote::{
    pinned_runtime::{ProcessRunOutput, ProcessRunner, ReleaseAssetStream, ReleaseFetcher},
    service_launcher_client::{LauncherEvent, ServiceLauncherHost},
    service_protocol::ServiceLauncherChildMessage,
};
use tokio::sync::broadcast;

/// The channel to a launcher over any pair of byte streams. Messages from the launcher are published as they arrive; the
/// end of the stream is a disconnect.
pub struct IpcLauncherHost {
    writer: Mutex<Box<dyn Write + Send>>,
    events: broadcast::Sender<LauncherEvent>,
    connected: Arc<AtomicBool>,
}

impl IpcLauncherHost {
    pub fn new(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Arc<Self> {
        let (events, _) = broadcast::channel(64);
        let connected = Arc::new(AtomicBool::new(true));
        let host = Arc::new(Self { writer: Mutex::new(Box::new(writer)), events: events.clone(), connected: connected.clone() });
        std::thread::Builder::new()
            .name("launcher-ipc".into())
            .spawn(move || {
                for line in BufReader::new(reader).lines() {
                    let Ok(line) = line else { break };
                    // A line that is not JSON is not a message (Node's own channel ignores it the same way).
                    if let Ok(message) = serde_json::from_str::<Value>(&line) {
                        let _ = events.send(LauncherEvent::Message(message));
                    }
                }
                connected.store(false, Ordering::SeqCst);
                let _ = events.send(LauncherEvent::Disconnect);
            })
            .expect("the launcher channel reader starts");
        host
    }

    /// The channel this process was started with, if any (`process.connected`): Unix only, and only when `env` names a
    /// descriptor that is open and the serialization is JSON.
    #[cfg(unix)]
    pub fn from_environment(env: &std::collections::BTreeMap<String, String>) -> Option<Arc<Self>> {
        use std::os::fd::FromRawFd;
        let descriptor: i32 = env.get("NODE_CHANNEL_FD")?.trim().parse().ok()?;
        if env.get("NODE_CHANNEL_SERIALIZATION_MODE").is_some_and(|mode| mode != "json") || descriptor < 3 {
            return None;
        }
        // SAFETY: only used to ask whether the descriptor is open; the process owns what the launcher handed it.
        if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } < 0 {
            return None;
        }
        // SAFETY: the launcher passed this descriptor to this process for its exclusive use (`NODE_CHANNEL_FD`), and it is
        // taken once, here, at startup.
        let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
        let writer = file.try_clone().ok()?;
        Some(Self::new(file, writer))
    }

    #[cfg(not(unix))]
    pub fn from_environment(_env: &std::collections::BTreeMap<String, String>) -> Option<Arc<Self>> {
        None
    }
}

impl ServiceLauncherHost for IpcLauncherHost {
    fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }
    fn send(&self, message: &ServiceLauncherChildMessage) -> Result<(), String> {
        if !self.connected() {
            return Err("The launcher channel is closed.".into());
        }
        let mut line = serde_json::to_string(message).map_err(|error| error.to_string())?;
        line.push('\n');
        let mut writer = self.writer.lock().map_err(|_| "The launcher channel is poisoned.".to_owned())?;
        writer.write_all(line.as_bytes()).and_then(|()| writer.flush()).map_err(|error| error.to_string())
    }
    fn subscribe(&self) -> broadcast::Receiver<LauncherEvent> {
        self.events.subscribe()
    }
}

/// `ProcessRunner.layer`: the command runs to completion under a timeout, and a command that cannot start or does not finish in
/// time is an error. The child is killed when the run is dropped.
pub struct HostProcessRunner;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[async_trait]
impl ProcessRunner for HostProcessRunner {
    async fn run(&self, command: &str, args: &[String], timeout: Option<Duration>) -> Result<ProcessRunOutput, String> {
        let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);
        let mut child = tokio::process::Command::new(command);
        child.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
        let output = tokio::time::timeout(timeout, child.output())
            .await
            .map_err(|_| format!("'{command}' timed out after {} ms.", timeout.as_millis()))?
            .map_err(|error| format!("Failed to spawn process '{command}': {error}"))?;
        Ok(ProcessRunOutput { code: output.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&output.stdout).into_owned(), stderr: String::from_utf8_lossy(&output.stderr).into_owned() })
    }
}

/// The release feed: GitHub Releases, or the base URL `T3CODE_CLI_RELEASE_BASE_URL` names.
pub struct HttpReleaseFeed {
    client: reqwest::Client,
}

impl HttpReleaseFeed {
    pub fn new() -> Self {
        Self { client: reqwest::Client::builder().user_agent(concat!("t3-server/", env!("CARGO_PKG_VERSION"))).build().unwrap_or_default() }
    }
}

impl Default for HttpReleaseFeed {
    fn default() -> Self {
        Self::new()
    }
}

struct HttpAsset(reqwest::Response);

#[async_trait]
impl ReleaseAssetStream for HttpAsset {
    fn content_length(&self) -> Option<u64> {
        self.0.content_length()
    }
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.0.chunk().await.map(|chunk| chunk.map(|bytes| bytes.to_vec())).map_err(|error| error.to_string())
    }
}

#[async_trait]
impl ReleaseFetcher for HttpReleaseFeed {
    async fn open(&self, url: &str) -> Result<Box<dyn ReleaseAssetStream>, String> {
        let response = self.client.get(url).send().await.map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("{url} answered {}.", response.status()));
        }
        Ok(Box::new(HttpAsset(response)))
    }
}
