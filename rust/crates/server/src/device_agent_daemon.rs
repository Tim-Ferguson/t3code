//! Owns agent-device's self-starting HTTP daemon and its CLI control processes.
use crate::{device_commands::DeviceCommands, terminal_environment::Environment};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};

#[derive(Clone, PartialEq, Eq)]
pub struct AgentDeviceEndpoint {
    pub base_url: String,
    pub token: String,
    pub entry_path: PathBuf,
    pub pid: Option<i64>,
    pub version: Option<String>,
}
impl std::fmt::Debug for AgentDeviceEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentDeviceEndpoint")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .field("entry_path", &self.entry_path)
            .field("pid", &self.pid)
            .field("version", &self.version)
            .finish()
    }
}
#[derive(Clone)]
pub struct AgentDaemonOptions {
    pub state_dir: PathBuf,
    pub environment: Environment,
    pub ready_timeout: Duration,
    pub poll_interval: Duration,
}
impl AgentDaemonOptions {
    pub fn host(state_dir: PathBuf, environment: Environment) -> Self {
        Self {
            state_dir: state_dir.join("device/agent-device"),
            environment,
            ready_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(100),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDaemonError {
    Stopped,
    Timeout,
}
struct Job {
    stop: watch::Sender<bool>,
    handle: JoinHandle<()>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
struct Cancel(Option<watch::Sender<bool>>);
impl Drop for Cancel {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            stop.send_replace(true);
        }
    }
}
struct Inner {
    options: AgentDaemonOptions,
    current: Arc<Mutex<Option<AgentDeviceEndpoint>>>,
    start: Arc<tokio::sync::Mutex<()>>,
    jobs: Mutex<Vec<Job>>,
    cleanup: tokio::sync::Mutex<Vec<Job>>,
    stop_owner: Arc<tokio::sync::Mutex<()>>,
    stopped: AtomicBool,
    closed: AtomicBool,
}
impl Drop for Inner {
    fn drop(&mut self) {
        for job in self.jobs.get_mut().unwrap().drain(..) {
            job.stop.send_replace(true);
        }
    }
}
#[derive(Clone)]
pub struct AgentDeviceDaemon(Arc<Inner>);
impl AgentDeviceDaemon {
    pub fn new(options: AgentDaemonOptions) -> Self {
        Self(Arc::new(Inner {
            options,
            current: Arc::new(Mutex::new(None)),
            start: Arc::new(tokio::sync::Mutex::new(())),
            jobs: Mutex::new(Vec::new()),
            cleanup: tokio::sync::Mutex::new(Vec::new()),
            stop_owner: Arc::new(tokio::sync::Mutex::new(())),
            stopped: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }))
    }
    pub fn current(&self) -> Option<AgentDeviceEndpoint> {
        self.0.current.lock().unwrap().clone()
    }
    pub async fn ensure(
        &self,
        node_path: PathBuf,
        entry_path: PathBuf,
    ) -> Result<AgentDeviceEndpoint, AgentDaemonError> {
        let admission = self.0.start.clone().lock_owned().await;
        if self.0.closed.load(Ordering::Acquire) || self.0.stopped.load(Ordering::Acquire) {
            return Err(AgentDaemonError::Stopped);
        }
        if let Some(current) = self.current() {
            return Ok(current);
        }
        let (stop, stopped) = watch::channel(false);
        let mut cancel = Cancel(Some(stop.clone()));
        let (reply, receive) = oneshot::channel();
        {
            let mut jobs = self.0.jobs.lock().unwrap();
            if self.0.closed.load(Ordering::Acquire) || self.0.stopped.load(Ordering::Acquire) {
                return Err(AgentDaemonError::Stopped);
            }
            jobs.retain(|job| !job.handle.is_finished());
            let options = self.0.options.clone();
            let current = self.0.current.clone();
            let start = self.0.start.clone();
            jobs.push(Job { stop, handle: tokio::spawn(async move {
                let mut admission=Some(admission);
                let commands=DeviceCommands::new();
                let startup=start_daemon(&options,&node_path,&entry_path,&commands);
                let mut stopped=stopped;
                let result=tokio::select! { biased; _=wait_stopped(&mut stopped)=>Err(AgentDaemonError::Stopped), result=startup=>result };
                // A canceled bootstrap may own a still-running CLI. Reap it before stop control.
                commands.shutdown().await;
                match result {
                    Ok(endpoint) if !*stopped.borrow() => {
                        *current.lock().unwrap()=Some(endpoint.clone());
                        let accepted=reply.send(Ok(endpoint)).is_ok();
                        if accepted {
                            drop(admission.take());
                            wait_stopped(&mut stopped).await;
                        }
                    }
                    result => { let _=reply.send(if result.is_ok() { Err(AgentDaemonError::Stopped) } else { result }); }
                }
                // Replacement startup cannot overlap the old daemon stop command.
                let _cleanup_admission=match admission.take() { Some(guard)=>guard, None=>start.lock_owned().await };
                *current.lock().unwrap()=None;
                stop_daemon(&options,&node_path,&entry_path).await;
            }) });
        }
        let result = receive.await.unwrap_or(Err(AgentDaemonError::Stopped));
        if result.is_ok() {
            cancel.0 = None;
        }
        result
    }
    pub async fn stop(&self) {
        let admission = self.0.stop_owner.clone().lock_owned().await;
        let owner = self.clone();
        let (reply, receive) = oneshot::channel();
        // Accepted cleanup retains admission and resets reusable state even when its caller drops.
        tokio::spawn(async move {
            owner.stop_owned().await;
            drop(admission);
            let _ = reply.send(());
        });
        let _ = receive.await;
    }
    async fn stop_owned(&self) {
        let mut cleanup = self.0.cleanup.lock().await;
        {
            let mut jobs = self.0.jobs.lock().unwrap();
            self.0.stopped.store(true, Ordering::Release);
            cleanup.extend(jobs.drain(..));
        }
        for job in cleanup.iter() {
            job.stop.send_replace(true);
        }
        while let Some(job) = cleanup.last_mut() {
            let _ = (&mut job.handle).await;
            cleanup.pop();
        }
        *self.0.current.lock().unwrap() = None;
        self.0.stopped.store(false, Ordering::Release);
    }
    pub async fn shutdown(&self) {
        self.0.closed.store(true, Ordering::Release);
        self.stop().await;
    }
}
async fn wait_stopped(stopped: &mut watch::Receiver<bool>) {
    if !*stopped.borrow() {
        let _ = stopped.wait_for(|value| *value).await;
    }
}
fn integer(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    (number.is_finite() && number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0)
        .then_some(number as i64)
}
async fn read_file(options: &AgentDaemonOptions, entry: &PathBuf) -> Option<AgentDeviceEndpoint> {
    let file: Value = serde_json::from_slice(
        &tokio::fs::read(options.state_dir.join("daemon.json"))
            .await
            .ok()?,
    )
    .ok()?;
    let port = integer(file.get("httpPort")?)?;
    let token = file.get("token")?.as_str()?.to_owned();
    let pid = match file.get("pid") {
        None => None,
        Some(value) => Some(integer(value)?),
    };
    let version = match file.get("version") {
        None => None,
        Some(value) => Some(value.as_str()?.to_owned()),
    };
    Some(AgentDeviceEndpoint {
        base_url: format!("http://127.0.0.1:{port}"),
        token,
        entry_path: entry.clone(),
        pid,
        version,
    })
}
async fn start_daemon(
    options: &AgentDaemonOptions,
    node: &PathBuf,
    entry: &PathBuf,
    commands: &DeviceCommands,
) -> Result<AgentDeviceEndpoint, AgentDaemonError> {
    let _ = tokio::fs::create_dir_all(&options.state_dir).await;
    if let Some(existing) = read_file(options, entry).await {
        let healthy = tokio::time::timeout(Duration::from_secs(2), async {
            let response = reqwest::get(format!("{}/health", existing.base_url))
                .await
                .ok()?;
            let good = response.status().as_u16() == 200;
            response.bytes().await.ok()?;
            Some(good)
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
        if healthy {
            return Ok(existing);
        }
        let _ = tokio::fs::remove_file(options.state_dir.join("daemon.json")).await;
    }
    let mut environment = options.environment.clone();
    for (key, value) in [
        (
            "AGENT_DEVICE_STATE_DIR",
            options.state_dir.to_string_lossy().as_ref(),
        ),
        ("AGENT_DEVICE_DAEMON_SERVER_MODE", "http"),
        ("AGENT_DEVICE_DAEMON_IDLE_TIMEOUT_MS", "0"),
        ("AGENT_DEVICE_NO_UPDATE_NOTIFIER", "1"),
        ("FORCE_COLOR", "0"),
        ("NO_COLOR", "1"),
    ] {
        environment.insert(key.into(), value.into());
    }
    let _ = commands
        .run(
            node.clone(),
            vec![
                entry.to_string_lossy().into_owned(),
                "devices".into(),
                "--json".into(),
            ],
            environment,
            options.ready_timeout,
        )
        .await;
    let deadline = tokio::time::Instant::now() + options.ready_timeout;
    loop {
        if let Some(file) = read_file(options, entry).await {
            return Ok(file);
        }
        if tokio::time::Instant::now() > deadline {
            return Err(AgentDaemonError::Timeout);
        }
        tokio::time::sleep(options.poll_interval).await;
    }
}
async fn stop_daemon(options: &AgentDaemonOptions, node: &PathBuf, entry: &PathBuf) {
    let commands = DeviceCommands::new();
    let mut environment = options.environment.clone();
    environment.insert("AGENT_DEVICE_NO_UPDATE_NOTIFIER".into(), "1".into());
    let _ = commands
        .run(
            node.clone(),
            vec![
                entry.to_string_lossy().into_owned(),
                "daemon".into(),
                "stop".into(),
                "--state-dir".into(),
                options.state_dir.to_string_lossy().into_owned(),
            ],
            environment,
            Duration::from_secs(10),
        )
        .await;
    commands.shutdown().await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    struct Fixture {
        root: tempfile::TempDir,
        socket: tokio::net::UnixDatagram,
        daemon: AgentDeviceDaemon,
        entry: PathBuf,
    }
    impl Fixture {
        fn new(held_stop: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("milestones.sock");
            let socket = tokio::net::UnixDatagram::bind(&path).unwrap();
            let entry = root.path().join("agent.py");
            std::fs::write(&entry, include_str!("../tests/fixtures/device-agent.py")).unwrap();
            let mut environment = Environment::new();
            for (key, path) in [
                ("FIXTURE_MILESTONES", path),
                ("FIXTURE_LOG", root.path().join("calls.jsonl")),
                ("FIXTURE_HOLD_START", root.path().join("hold")),
            ] {
                environment.insert(key.into(), path.to_string_lossy().into_owned());
            }
            if held_stop {
                environment.insert(
                    "FIXTURE_STOP_GATE".into(),
                    root.path().join("stop.sock").to_string_lossy().into_owned(),
                );
            }
            let daemon =
                AgentDeviceDaemon::new(AgentDaemonOptions::host(root.path().into(), environment));
            Self {
                root,
                socket,
                daemon,
                entry,
            }
        }
        async fn milestone(&self, event: &str) -> i32 {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let mut bytes = [0; 1024];
                    let size = self.socket.recv(&mut bytes).await.unwrap();
                    let value: Value = serde_json::from_slice(&bytes[..size]).unwrap();
                    if value["event"] == event {
                        return value["pid"].as_i64().unwrap() as i32;
                    }
                }
            })
            .await
            .unwrap()
        }
        async fn release_stop(&self) {
            drop(
                tokio::net::UnixStream::connect(self.root.path().join("stop.sock"))
                    .await
                    .unwrap(),
            );
        }
        async fn ensure(&self) -> Result<AgentDeviceEndpoint, AgentDaemonError> {
            self.daemon
                .ensure("/usr/bin/python3".into(), self.entry.clone())
                .await
        }
    }
    #[tokio::test]
    async fn daemon_file_schema_distinguishes_absent_null_and_integer_numbers() {
        let fixture = Fixture::new(false);
        let options = &fixture.daemon.0.options;
        tokio::fs::create_dir_all(&options.state_dir).await.unwrap();
        for row in include_str!("../tests/fixtures/device-daemon.jsonl").lines() {
            let row: Value = serde_json::from_str(row).unwrap();
            tokio::fs::write(
                options.state_dir.join("daemon.json"),
                row["input"].to_string(),
            )
            .await
            .unwrap();
            let endpoint = read_file(options, &fixture.entry).await;
            assert_eq!(
                endpoint.is_some(),
                row["accepted"].as_bool().unwrap(),
                "{row}"
            );
            if let Some(endpoint) = endpoint {
                assert_eq!(endpoint.token, row["output"]["token"].as_str().unwrap());
                assert_eq!(
                    endpoint.pid,
                    row["output"]
                        .get("pid")
                        .map(|value| integer(value).unwrap())
                );
                assert_eq!(
                    endpoint.version,
                    row["output"]
                        .get("version")
                        .map(|value| value.as_str().unwrap().to_owned())
                );
            }
        }
        fixture.daemon.shutdown().await;
    }
    #[tokio::test]
    async fn bootstrap_environment_and_stop_argv_preserve_source_protocol() {
        let fixture = Fixture::new(false);
        let endpoint = fixture.ensure().await.unwrap();
        assert_eq!(endpoint.base_url, "http://127.0.0.1:12345");
        assert_eq!(endpoint.token, "isolated token");
        assert_eq!(fixture.ensure().await.unwrap().base_url, endpoint.base_url);
        fixture.daemon.stop().await;
        let rows: Vec<Value> = std::fs::read_to_string(fixture.root.path().join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|row| serde_json::from_str(row).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["args"], json!(["devices", "--json"]));
        for (key, value) in [
            ("AGENT_DEVICE_DAEMON_SERVER_MODE", "http"),
            ("AGENT_DEVICE_DAEMON_IDLE_TIMEOUT_MS", "0"),
            ("AGENT_DEVICE_NO_UPDATE_NOTIFIER", "1"),
            ("FORCE_COLOR", "0"),
            ("NO_COLOR", "1"),
        ] {
            assert_eq!(rows[0]["env"][key], value);
        }
        assert_eq!(
            rows[1]["args"],
            json!([
                "daemon",
                "stop",
                "--state-dir",
                fixture.daemon.0.options.state_dir
            ])
        );
        assert!(fixture.daemon.current().is_none());
        fixture.daemon.shutdown().await;
    }
    #[tokio::test]
    async fn canceled_bootstrap_is_reaped_and_replacement_waits_for_old_stop() {
        let fixture = Fixture::new(true);
        std::fs::write(fixture.root.path().join("hold"), "").unwrap();
        let first = tokio::spawn({
            let daemon = fixture.daemon.clone();
            let entry = fixture.entry.clone();
            async move { daemon.ensure("/usr/bin/python3".into(), entry).await }
        });
        let first_pid = fixture.milestone("bootstrap").await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        fixture.milestone("stop").await;
        assert_eq!(unsafe { libc::kill(first_pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert!(
            fixture.daemon.0.start.try_lock().is_err(),
            "cleanup must retain startup admission"
        );
        std::fs::remove_file(fixture.root.path().join("hold")).unwrap();
        let (entered, started) = oneshot::channel();
        let second = tokio::spawn({
            let daemon = fixture.daemon.clone();
            let entry = fixture.entry.clone();
            async move {
                let _ = entered.send(());
                daemon.ensure("/usr/bin/python3".into(), entry).await
            }
        });
        started.await.unwrap();
        fixture.release_stop().await;
        let endpoint = tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            fixture.daemon.current().unwrap().base_url,
            endpoint.base_url
        );
        let shutdown = tokio::spawn({
            let daemon = fixture.daemon.clone();
            async move { daemon.shutdown().await }
        });
        fixture.milestone("stop").await;
        fixture.release_stop().await;
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert!(fixture.daemon.current().is_none());
    }
    #[tokio::test]
    async fn healthy_existing_daemon_is_reused_and_owned_stop_terminates_it() {
        let fixture = Fixture::new(false);
        let options = &fixture.daemon.0.options;
        tokio::fs::create_dir_all(&options.state_dir).await.unwrap();
        let mut child = tokio::process::Command::new("/usr/bin/python3")
            .arg(&fixture.entry)
            .arg("serve")
            .arg(options.state_dir.join("daemon.json"))
            .env(
                "FIXTURE_MILESTONES",
                fixture.root.path().join("milestones.sock"),
            )
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = fixture.milestone("server").await;
        assert_eq!(child.id().unwrap() as i32, pid);
        let endpoint = fixture.ensure().await.unwrap();
        assert_eq!(endpoint.pid, Some(pid as i64));
        assert_eq!(endpoint.version.as_deref(), Some("0.21.12"));
        fixture.daemon.shutdown().await;
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        let calls = std::fs::read_to_string(fixture.root.path().join("calls.jsonl")).unwrap();
        assert!(!calls.contains("devices"));
        assert!(calls.contains("daemon"));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    #[tokio::test]
    async fn canceled_stop_finishes_cleanup_and_restores_reusable_startup() {
        let fixture = Fixture::new(true);
        fixture.ensure().await.unwrap();
        let stop = tokio::spawn({
            let daemon = fixture.daemon.clone();
            async move { daemon.stop().await }
        });
        fixture.milestone("stop").await;
        stop.abort();
        assert!(stop.await.unwrap_err().is_cancelled());
        // The accepted stop owner keeps admission, even after the requesting future is gone.
        assert!(fixture.daemon.0.stop_owner.try_lock().is_err());
        fixture.release_stop().await;
        // Await the owner barrier, not a sleep or an optimistic current() observation.
        let complete =
            tokio::time::timeout(Duration::from_secs(5), fixture.daemon.0.stop_owner.lock())
                .await
                .unwrap();
        assert!(!fixture.daemon.0.stopped.load(Ordering::Acquire));
        drop(complete);
        assert_eq!(
            fixture.ensure().await.unwrap().base_url,
            "http://127.0.0.1:12345"
        );
        let shutdown = tokio::spawn({
            let daemon = fixture.daemon.clone();
            async move { daemon.shutdown().await }
        });
        fixture.milestone("stop").await;
        fixture.release_stop().await;
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            fixture.ensure().await,
            Err(AgentDaemonError::Stopped)
        ));
    }
}
