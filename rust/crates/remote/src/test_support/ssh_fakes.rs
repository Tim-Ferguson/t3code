//! In-memory fakes of the SSH layer: an `SshSpawner` whose processes finish, run until killed, or
//! finish after a (virtual) delay, a scriptable `HttpProbe`, and fixed loopback ports. Nothing here
//! starts a process or opens a socket.
#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use crate::ssh_auth::{DisabledSshPasswordPrompt, SshAuthHost};
use crate::ssh_tunnel::*;

#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub stdin: Option<String>,
}

impl RecordedCall {
    pub fn has(&self, arg: &str) -> bool {
        self.args.iter().any(|a| a == arg)
    }
}

/// A scripted process.
pub struct FakeProcess {
    exit: tokio::sync::watch::Sender<Option<ProcessOutput>>,
    pub kills: AtomicUsize,
    /// Awaited by `kill` before the process is reported dead (a slow shutdown).
    kill_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    kill_started: Arc<tokio::sync::Notify>,
}

impl FakeProcess {
    fn new(output: Option<ProcessOutput>) -> Arc<Self> {
        Arc::new(Self {
            exit: tokio::sync::watch::channel(output).0,
            kills: AtomicUsize::new(0),
            kill_gate: Mutex::new(None),
            kill_started: Arc::new(tokio::sync::Notify::new()),
        })
    }

    pub fn finished(stdout: &str, stderr: &str, exit_code: i32) -> Arc<Self> {
        Self::new(Some(ProcessOutput {
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            exit_code,
        }))
    }

    /// Runs until killed (then exits 143) or until `finish` is called.
    pub fn running() -> Arc<Self> {
        Self::new(None)
    }

    pub fn finish(&self, stdout: &str, stderr: &str, exit_code: i32) {
        self.exit.send_replace(Some(ProcessOutput {
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            exit_code,
        }));
    }

    /// Makes the next `kill` wait until `release` is notified.
    pub fn stall_kill(&self) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.kill_gate.lock().unwrap() = Some(gate.clone());
        (gate, self.kill_started.clone())
    }
}

#[async_trait]
impl SshProcess for FakeProcess {
    fn pid(&self) -> Option<u32> {
        Some(123)
    }

    async fn wait(&self) -> io::Result<ProcessOutput> {
        let mut rx = self.exit.subscribe();
        loop {
            if let Some(output) = rx.borrow_and_update().clone() {
                return Ok(output);
            }
            rx.changed().await.map_err(io::Error::other)?;
        }
    }

    fn is_running(&self) -> bool {
        self.exit.borrow().is_none()
    }

    async fn kill(&self, _grace: Duration) {
        self.kills.fetch_add(1, Ordering::SeqCst);
        self.kill_started.notify_one();
        let gate = self.kill_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        if self.is_running() {
            self.finish("", "", 143);
        }
    }
}

type Responder = Box<dyn Fn(&RecordedCall) -> Arc<dyn SshProcess> + Send + Sync>;

/// Records every spawn and answers with whatever the responder returns.
pub struct FakeSpawner {
    pub calls: Mutex<Vec<RecordedCall>>,
    responder: Responder,
    spawned: tokio::sync::Notify,
}

impl FakeSpawner {
    pub fn new(
        responder: impl Fn(&RecordedCall) -> Arc<dyn SshProcess> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            responder: Box::new(responder),
            spawned: tokio::sync::Notify::new(),
        })
    }

    /// Every call answers with this stdout and exit 0.
    pub fn always(stdout: &'static str) -> Arc<Self> {
        Self::new(move |_| FakeProcess::finished(stdout, "", 0))
    }

    pub fn recorded(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn count(&self, predicate: impl Fn(&RecordedCall) -> bool) -> usize {
        self.calls.lock().unwrap().iter().filter(|c| predicate(c)).count()
    }
}

#[async_trait]
impl SshSpawner for FakeSpawner {
    async fn spawn(&self, request: SshSpawnRequest) -> io::Result<Arc<dyn SshProcess>> {
        let call = RecordedCall {
            args: request.args,
            env: request.env,
            stdin: request.stdin,
        };
        let process = (self.responder)(&call);
        self.calls.lock().unwrap().push(call);
        self.spawned.notify_waiters();
        Ok(process)
    }
}

/// An `HttpProbe` that answers from a queue (then repeats the last answer), or never answers.
pub struct FakeProbe {
    answers: Mutex<VecDeque<Result<u16, String>>>,
    last: Mutex<Result<u16, String>>,
    pub urls: Mutex<Vec<String>>,
    hang: bool,
}

impl FakeProbe {
    pub fn ok() -> Arc<Self> {
        Self::answering(vec![Ok(200)])
    }

    pub fn answering(answers: Vec<Result<u16, String>>) -> Arc<Self> {
        let last = answers.last().cloned().unwrap_or(Ok(200));
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            last: Mutex::new(last),
            urls: Mutex::new(Vec::new()),
            hang: false,
        })
    }

    /// Never answers: a probe stuck on one request.
    pub fn hanging() -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(VecDeque::new()),
            last: Mutex::new(Ok(200)),
            urls: Mutex::new(Vec::new()),
            hang: true,
        })
    }
}

#[async_trait]
impl HttpProbe for FakeProbe {
    async fn get(&self, url: &str) -> Result<u16, String> {
        self.urls.lock().unwrap().push(url.to_owned());
        if self.hang {
            std::future::pending::<()>().await;
        }
        let next = self.answers.lock().unwrap().pop_front();
        next.unwrap_or_else(|| self.last.lock().unwrap().clone())
    }
}

pub struct FixedPorts(pub u16);

#[async_trait]
impl LocalPorts for FixedPorts {
    async fn reserve_loopback_port(&self) -> Result<u16, String> {
        Ok(self.0)
    }

    async fn can_listen_on_host(&self, _port: u16, _host: &str) -> bool {
        true
    }
}

/// A runtime wired to fakes, with a temp directory for askpass files.
pub fn runtime(
    spawner: Arc<dyn SshSpawner>,
    probe: Arc<dyn HttpProbe>,
    temp_root: &std::path::Path,
) -> SshRuntime {
    SshRuntime {
        spawner,
        prompt: Arc::new(DisabledSshPasswordPrompt),
        ports: Arc::new(FixedPorts(41_773)),
        probe,
        auth_host: SshAuthHost {
            windows: false,
            process_display: None,
            temp_root: temp_root.to_path_buf(),
        },
        ssh_command: "ssh".to_owned(),
    }
}
