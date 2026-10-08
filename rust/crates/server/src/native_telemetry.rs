//! Owned NDJSON resource sidecar with supervised recovery and correlated requests.
use crate::{
    resource_binary::{ResourceMonitorBinary, TelemetryError},
    resource_policy,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use t3_contracts::*;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
};
#[derive(Clone)]
pub struct NativeTelemetryOptions {
    pub binary: ResourceMonitorBinary,
    pub cwd: std::path::PathBuf,
    pub handshake_timeout: Duration,
    pub request_timeout: Duration,
    pub history_timeout: Duration,
}
impl NativeTelemetryOptions {
    pub fn host(cwd: std::path::PathBuf, configured: Option<std::path::PathBuf>) -> Self {
        Self {
            binary: ResourceMonitorBinary::host(configured),
            cwd,
            handshake_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            history_timeout: Duration::from_secs(15),
        }
    }
}
#[derive(Clone, Debug)]
pub struct NativeTelemetryHealth {
    pub status: ResourceTelemetrySourceStatus,
    pub hello: Option<ResourceMonitorHelloEvent>,
    pub last_sample_at: Option<UtcDateTime>,
    pub last_error: Option<String>,
    pub restart_count: u64,
    pub sample_interval_ms: u64,
}
#[derive(Clone, Debug)]
pub struct NativeSnapshot {
    pub generation: u64,
    pub snapshot: ResourceMonitorSnapshotEvent,
}
#[derive(Clone)]
struct Control {
    power: HostPowerSnapshot,
    live_subscribers: usize,
    interval: u64,
}
struct Collection {
    desired: Control,
    applied: Control,
}
enum Reply {
    Snapshot(NativeSnapshot),
    ProcessTable(Vec<ResourceMonitorProcessTableEntry>),
    History(Vec<ResourceMonitorSnapshotEvent>),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplyKind {
    Snapshot,
    ProcessTable,
    History,
}
struct Pending {
    kind: ReplyKind,
    send: oneshot::Sender<Result<Reply, TelemetryError>>,
    history: Vec<ResourceMonitorSnapshotEvent>,
}
struct Outgoing {
    message: ResourceMonitorCommand,
    complete: oneshot::Sender<Result<(), TelemetryError>>,
}
struct Shared {
    options: NativeTelemetryOptions,
    health: watch::Sender<NativeTelemetryHealth>,
    snapshots: broadcast::Sender<NativeSnapshot>,
    writer: Mutex<Option<mpsc::Sender<Outgoing>>>,
    pending: Mutex<HashMap<String, Pending>>,
    collection: tokio::sync::Mutex<Collection>,
    external: Mutex<Vec<ResourceMonitorExternalProcess>>,
    stop: watch::Sender<bool>,
    retry: tokio::sync::Notify,
    closed: AtomicBool,
    releases: mpsc::UnboundedSender<()>,
}
struct Inner {
    shared: Arc<Shared>,
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    shutdown: tokio::sync::Mutex<()>,
    controls: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.stop.send_replace(true);
    }
}
#[derive(Clone)]
pub struct NativeTelemetryClient(Arc<Inner>);
struct PendingGuard {
    shared: Arc<Shared>,
    id: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.shared.pending.lock().unwrap().remove(&self.id);
    }
}
pub struct NativeSnapshotSubscription {
    _demand: DemandGuard,
    receiver: broadcast::Receiver<NativeSnapshot>,
    stopped: watch::Receiver<bool>,
}
impl NativeSnapshotSubscription {
    pub async fn recv(&mut self) -> Result<Option<NativeSnapshot>, TelemetryError> {
        loop {
            tokio::select! {biased;_=self.stopped.wait_for(|stopped|*stopped)=>return Ok(None),event=self.receiver.recv()=>match event{Ok(event)=>return Ok(Some(event)),Err(broadcast::error::RecvError::Lagged(_))=>continue,Err(_)=>return Ok(None)}}
        }
    }
}
// Arm after demand mutation, before any writer await. Apply errors retain
// desired state as in the source; interrupted acquisitions release ownership.
struct DemandGuard {
    shared: Arc<Shared>,
    armed: bool,
}
impl Drop for DemandGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.shared.releases.send(());
        }
    }
}
fn cause(message: impl std::fmt::Display) -> Value {
    json!({"name":"Error","message":message.to_string()})
}
fn io_cause(error: io::Error) -> Value {
    crate::terminal_store::io_cause(error)
}
fn command_error(operation: impl Into<String>, error: impl std::fmt::Display) -> TelemetryError {
    TelemetryError::CommandFailed {
        operation: operation.into(),
        cause: cause(error),
    }
}
impl Shared {
    fn update_health(&self, update: impl FnOnce(&mut NativeTelemetryHealth)) {
        self.health.send_modify(update);
    }
    fn unavailable(&self) -> TelemetryError {
        TelemetryError::Unavailable {
            reason: self
                .health
                .borrow()
                .last_error
                .clone()
                .unwrap_or_else(|| "sidecar is not running".into()),
        }
    }
    fn fail_pending(&self, error: TelemetryError) {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, pending) in pending {
            let _ = pending.send.send(Err(error.clone()));
        }
    }
    async fn write(&self, message: ResourceMonitorCommand) -> Result<(), TelemetryError> {
        let tag =
            serde_json::to_value(&message).map_err(|error| command_error("encode", error))?["type"]
                .as_str()
                .unwrap()
                .to_owned();
        let writer = self
            .writer
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| self.unavailable())?;
        let (complete, receive) = oneshot::channel();
        writer
            .send(Outgoing { message, complete })
            .await
            .map_err(|error| command_error(&tag, error))?;
        receive.await.map_err(|error| command_error(tag, error))?
    }
    async fn update_control(
        &self,
        update: impl FnOnce(&mut Control),
    ) -> Result<(), TelemetryError> {
        let mut collection = self.collection.lock().await;
        update(&mut collection.desired);
        self.apply_control(&mut collection).await
    }
    async fn apply_control(&self, collection: &mut Collection) -> Result<(), TelemetryError> {
        collection.desired.interval = resource_policy::sample_interval_ms(
            &collection.desired.power,
            collection.desired.live_subscribers,
        );
        self.update_health(|health| health.sample_interval_ms = collection.desired.interval);
        let health = self.health.borrow().clone();
        let has_writer = self.writer.lock().unwrap().is_some();
        if resource_policy::can_command(health.status, has_writer) {
            if collection.applied.interval != collection.desired.interval {
                self.write(decode_command(json!({"version":3,"type":"setSampleInterval","sampleIntervalMs":collection.desired.interval}))).await?;
            }
            if (collection.applied.live_subscribers > 0)
                != (collection.desired.live_subscribers > 0)
            {
                self.write(decode_command(json!({"version":3,"type":"setStreaming","enabled":collection.desired.live_subscribers>0}))).await?;
            }
        }
        collection.applied = collection.desired.clone();
        Ok(())
    }
    async fn change_demand(&self, delta: i32) -> Result<(), TelemetryError> {
        self.update_control(|control| {
            control.live_subscribers = if delta > 0 {
                control.live_subscribers + 1
            } else {
                control.live_subscribers.saturating_sub(1)
            }
        })
        .await
    }
    fn take_pending(&self, id: &str, kind: ReplyKind) -> Option<Pending> {
        let mut pending = self.pending.lock().unwrap();
        if pending.get(id).is_some_and(|entry| entry.kind == kind) {
            pending.remove(id)
        } else {
            None
        }
    }
    fn event(
        &self,
        event: ResourceMonitorEvent,
        generation: u64,
        hello: &watch::Sender<Option<ResourceMonitorHelloEvent>>,
    ) -> Result<(), TelemetryError> {
        match event {
            ResourceMonitorEvent::Hello(event) => {
                self.update_health(|health| {
                    health.status = ResourceTelemetrySourceStatus::Starting;
                    health.hello = Some(event.clone());
                    health.last_error = None;
                });
                hello.send_replace(Some(event));
            }
            ResourceMonitorEvent::Snapshot(event) => {
                let snapshot = NativeSnapshot {
                    generation,
                    snapshot: event,
                };
                self.update_health(|health| {
                    health.status = ResourceTelemetrySourceStatus::Healthy;
                    health.last_error = None;
                    health.last_sample_at = chrono::DateTime::from_timestamp_millis(
                        snapshot.snapshot.sampled_at_unix_ms.0 as i64,
                    )
                    .map(Into::into);
                });
                let _ = self.snapshots.send(snapshot.clone());
                if let Some(id) = &snapshot.snapshot.request_id {
                    if let Some(pending) = self.take_pending(id.as_str(), ReplyKind::Snapshot) {
                        let _ = pending.send.send(Ok(Reply::Snapshot(snapshot)));
                    }
                }
            }
            ResourceMonitorEvent::ProcessTable(event) => {
                if let Some(pending) =
                    self.take_pending(event.request_id.as_str(), ReplyKind::ProcessTable)
                {
                    let _ = pending.send.send(Ok(Reply::ProcessTable(event.processes)));
                }
            }
            ResourceMonitorEvent::HistoryChunk(event) => {
                self.update_health(|health| {
                    health.status = ResourceTelemetrySourceStatus::Healthy;
                    health.last_error = None;
                    if let Some(snapshot) = event.snapshots.last() {
                        health.last_sample_at = chrono::DateTime::from_timestamp_millis(
                            snapshot.sampled_at_unix_ms.0 as i64,
                        )
                        .map(Into::into);
                    }
                });
                let mut pending = self.pending.lock().unwrap();
                if let Some(request) = pending
                    .get_mut(event.request_id.as_str())
                    .filter(|pending| pending.kind == ReplyKind::History)
                {
                    request.history.extend(event.snapshots);
                    if event.done {
                        let request = pending.remove(event.request_id.as_str()).unwrap();
                        let _ = request.send.send(Ok(Reply::History(request.history)));
                    }
                }
            }
            ResourceMonitorEvent::Error(event) => {
                self.update_health(|health| {
                    health.status = ResourceTelemetrySourceStatus::Degraded;
                    health.last_error = Some(event.message.to_string());
                });
                if !event.recoverable {
                    return Err(TelemetryError::CommandFailed {
                        operation: event.code.to_string(),
                        cause: Value::String(event.message.to_string()),
                    });
                }
            }
        }
        Ok(())
    }
}
fn decode_command(value: Value) -> ResourceMonitorCommand {
    serde_json::from_value(value).expect("typed internal resource command")
}
impl NativeTelemetryClient {
    pub fn new(options: NativeTelemetryOptions) -> Self {
        let power:HostPowerSnapshot=serde_json::from_value(json!({"source":"unknown","idle":"unknown","idleSeconds":null,"locked":"unknown","suspended":false,"onBattery":"unknown","lowPowerMode":"unknown","thermalState":"unknown","stale":true,"updatedAt":chrono::Utc::now().to_rfc3339()})).unwrap();
        let initial = Control {
            power,
            live_subscribers: 0,
            interval: 5000,
        };
        let (health, _) = watch::channel(NativeTelemetryHealth {
            status: ResourceTelemetrySourceStatus::Starting,
            hello: None,
            last_sample_at: None,
            last_error: None,
            restart_count: 0,
            sample_interval_ms: 5000,
        });
        let (snapshots, _) = broadcast::channel(8);
        let (stop, stopped) = watch::channel(false);
        let (releases, mut released) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            options,
            health,
            snapshots,
            writer: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            collection: tokio::sync::Mutex::new(Collection {
                desired: initial.clone(),
                applied: initial,
            }),
            external: Mutex::new(vec![]),
            stop,
            retry: tokio::sync::Notify::new(),
            closed: AtomicBool::new(false),
            releases,
        });
        let control = shared.clone();
        let mut controls_stopped = stopped.clone();
        let controls = tokio::spawn(async move {
            loop {
                let release = tokio::select! {biased;
                    _=async{let _=controls_stopped.wait_for(|value|*value).await;}=>None,
                    release=released.recv()=>release,
                };
                if release.is_none() {
                    break;
                }
                let _ = control.change_demand(-1).await;
            }
            let mut collection = control.collection.lock().await;
            while released.try_recv().is_ok() {
                collection.desired.live_subscribers =
                    collection.desired.live_subscribers.saturating_sub(1);
            }
        });
        let supervisor = shared.clone();
        let task = tokio::spawn(async move {
            supervise(supervisor, stopped).await;
        });
        Self(Arc::new(Inner {
            shared,
            task: tokio::sync::Mutex::new(Some(task)),
            shutdown: tokio::sync::Mutex::new(()),
            controls: tokio::sync::Mutex::new(Some(controls)),
        }))
    }
    pub fn health(&self) -> NativeTelemetryHealth {
        self.0.shared.health.borrow().clone()
    }
    pub fn subscribe_health(&self) -> watch::Receiver<NativeTelemetryHealth> {
        self.0.shared.health.subscribe()
    }
    pub fn capabilities(&self) -> Result<ResourceMonitorCapabilities, TelemetryError> {
        self.health()
            .hello
            .map(|hello| hello.capabilities)
            .ok_or_else(|| TelemetryError::Unavailable {
                reason: self
                    .health()
                    .last_error
                    .unwrap_or_else(|| "handshake is incomplete".into()),
            })
    }
    pub fn retry(&self) -> bool {
        let health = self.health();
        let allowed = !*self.0.shared.stop.borrow()
            && resource_policy::can_retry(
                health.status,
                self.0.shared.writer.lock().unwrap().is_some(),
            );
        if allowed {
            self.0.shared.retry.notify_one();
        }
        allowed
    }
    pub async fn set_host_power_state(
        &self,
        power: HostPowerSnapshot,
    ) -> Result<(), TelemetryError> {
        self.0
            .shared
            .update_control(|control| control.power = power)
            .await
    }
    pub async fn set_external_processes(
        &self,
        processes: Vec<ResourceMonitorExternalProcess>,
    ) -> Result<(), TelemetryError> {
        *self.0.shared.external.lock().unwrap() = processes.clone();
        let health = self.health();
        if resource_policy::can_command(
            health.status,
            self.0.shared.writer.lock().unwrap().is_some(),
        ) {
            self.0
                .shared
                .write(decode_command(
                    json!({"version":3,"type":"setExternalProcesses","processes":processes}),
                ))
                .await?;
        }
        Ok(())
    }
    pub async fn subscribe(&self) -> Result<NativeSnapshotSubscription, TelemetryError> {
        let shared = self.0.shared.clone();
        let receiver = shared.snapshots.subscribe();
        let mut demand = DemandGuard {
            shared: shared.clone(),
            armed: false,
        };
        let result = {
            let mut collection = shared.collection.lock().await;
            if *shared.stop.borrow() {
                return Err(shared.unavailable());
            }
            collection.desired.live_subscribers += 1;
            demand.armed = true;
            shared.apply_control(&mut collection).await
        };
        if let Err(error) = result {
            demand.armed = false;
            return Err(error);
        }
        Ok(NativeSnapshotSubscription {
            _demand: demand,
            receiver,
            stopped: shared.stop.subscribe(),
        })
    }
    async fn request(
        &self,
        operation: &str,
        extra: Value,
        timeout: Duration,
    ) -> Result<Reply, TelemetryError> {
        let shared = self.0.shared.clone();
        let health = self.health();
        if !resource_policy::can_command(health.status, shared.writer.lock().unwrap().is_some()) {
            return Err(shared.unavailable());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (send, receive) = oneshot::channel();
        shared.pending.lock().unwrap().insert(
            id.clone(),
            Pending {
                kind: match operation {
                    "sampleNow" => ReplyKind::Snapshot,
                    "processTable" => ReplyKind::ProcessTable,
                    "readHistory" => ReplyKind::History,
                    _ => unreachable!(),
                },
                send,
                history: vec![],
            },
        );
        let _guard = PendingGuard {
            shared: shared.clone(),
            id: id.clone(),
        };
        let mut command = json!({"version":3,"type":operation,"requestId":id});
        command
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        shared.write(decode_command(command)).await?;
        tokio::time::timeout(timeout, receive)
            .await
            .map_err(|_| TelemetryError::RequestTimedOut {
                operation: operation.into(),
                timeout_ms: timeout.as_millis() as u64,
            })?
            .map_err(|error| command_error(operation, error))?
    }
    pub async fn process_table(
        &self,
    ) -> Result<Vec<ResourceMonitorProcessTableEntry>, TelemetryError> {
        match self
            .request(
                "processTable",
                json!({}),
                self.0.shared.options.request_timeout,
            )
            .await?
        {
            Reply::ProcessTable(table) => Ok(table),
            _ => Err(command_error("processTable", "unexpected response kind")),
        }
    }
    pub async fn sample_now(&self) -> Result<NativeSnapshot, TelemetryError> {
        match self
            .request(
                "sampleNow",
                json!({}),
                self.0.shared.options.request_timeout,
            )
            .await?
        {
            Reply::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(command_error("sampleNow", "unexpected response kind")),
        }
    }
    pub async fn read_history(
        &self,
        window_ms: u64,
    ) -> Result<Vec<ResourceMonitorSnapshotEvent>, TelemetryError> {
        match self
            .request(
                "readHistory",
                json!({"windowMs":window_ms}),
                self.0.shared.options.history_timeout,
            )
            .await?
        {
            Reply::History(history) => Ok(history),
            _ => Err(command_error("readHistory", "unexpected response kind")),
        }
    }
    pub async fn shutdown(&self) {
        let _serial = self.0.shutdown.lock().await;
        self.0.shared.stop.send_replace(true);
        let mut supervisor = self.0.task.lock().await;
        if let Some(task) = supervisor.as_mut() {
            if let Err(error) = task.await {
                tracing::error!(%error,"native telemetry supervisor failed");
            }
        }
        supervisor.take();
        let mut controls = self.0.controls.lock().await;
        if let Some(task) = controls.as_mut() {
            if let Err(error) = task.await {
                tracing::error!(%error,"native telemetry control worker failed");
            }
        }
        controls.take();
    }
}

async fn supervise(shared: Arc<Shared>, mut stopped: watch::Receiver<bool>) {
    let mut failures = vec![];
    let mut attempt = 0;
    loop {
        if *stopped.borrow() {
            break;
        }
        // The attempt owns child cleanup; do not cancel this future from an
        // outer select while reader/writer workers and a child are live.
        let error = match run_attempt(shared.clone(), stopped.clone()).await {
            Ok(()) => break,
            Err(error) => error,
        };
        let now = chrono::Utc::now().timestamp_millis();
        failures = resource_policy::recent_failures(&failures, now);
        if failures.is_empty() {
            attempt = 0;
        }
        failures.push(now);
        let exhausted = failures.len() >= 5;
        shared.update_health(|health| {
            health.status = if exhausted {
                ResourceTelemetrySourceStatus::Unavailable
            } else {
                ResourceTelemetrySourceStatus::Degraded
            };
            health.hello = None;
            health.last_error = Some(error.to_string());
            health.restart_count += 1;
        });
        shared.fail_pending(error);
        if exhausted {
            tokio::select! {biased;_=stopped.wait_for(|value|*value)=>break,_=shared.retry.notified()=>{}}
            failures.clear();
            attempt = 0;
            shared.update_health(|health| {
                health.status = ResourceTelemetrySourceStatus::Starting;
                health.hello = None;
                health.last_error = None;
            });
        } else {
            let manual = tokio::select! {biased;_=stopped.wait_for(|value|*value)=>break,_=shared.retry.notified()=>true,_=tokio::time::sleep(Duration::from_millis(resource_policy::restart_delay_ms(attempt)))=>false};
            attempt = if manual { 0 } else { attempt + 1 };
        }
    }
    shared.closed.store(true, Ordering::SeqCst);
    shared.fail_pending(TelemetryError::Unavailable {
        reason: "native telemetry stopped".into(),
    });
    shared.update_health(|health| {
        health.status = ResourceTelemetrySourceStatus::Stopped;
        health.hello = None;
    });
}
async fn run_attempt(
    shared: Arc<Shared>,
    mut stopped: watch::Receiver<bool>,
) -> Result<(), TelemetryError> {
    let path = shared.options.binary.resolve()?;
    let mut child = tokio::process::Command::new(&path)
        .current_dir(&shared.options.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| TelemetryError::SpawnFailed {
            path: path.to_string_lossy().into_owned(),
            cause: io_cause(error),
        })?;
    let generation = shared.health.borrow().restart_count;
    let (writer, mut outgoing) = mpsc::channel::<Outgoing>(64);
    *shared.writer.lock().unwrap() = Some(writer);
    shared.update_health(|health| {
        health.status = ResourceTelemetrySourceStatus::Starting;
        health.hello = None;
    });
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (failure, mut failed) = watch::channel::<Option<TelemetryError>>(None);
    let (hello, mut greeted) = watch::channel(None);
    let read_shared = shared.clone();
    let read_failure = failure.clone();
    let reader = tokio::spawn(async move {
        let mut stdout = NdjsonLines::new(BufReader::new(stdout));
        let error = loop {
            let text = match stdout.next().await {
                Ok(Some(line)) => line,
                Ok(None) => break TelemetryError::StreamClosed,
                Err(error) => {
                    break TelemetryError::DecodeFailed {
                        cause: io_cause(error),
                    };
                }
            };
            if text.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(error) => {
                    break TelemetryError::DecodeFailed {
                        cause: cause(error),
                    };
                }
            };
            if let Some(version) = value.get("version").and_then(Value::as_number) {
                if version.as_f64() != Some(3.0) {
                    break TelemetryError::ProtocolMismatch {
                        expected_version: 3,
                        received_version: version.clone(),
                    };
                }
            }
            let event = match serde_json::from_value(value) {
                Ok(event) => event,
                Err(error) => {
                    break TelemetryError::DecodeFailed {
                        cause: cause(error),
                    };
                }
            };
            if let Err(error) = read_shared.event(event, generation, &hello) {
                break error;
            }
        };
        read_failure.send_if_modified(|failure| {
            if failure.is_none() {
                *failure = Some(error);
                true
            } else {
                false
            }
        });
    });
    let write_failure = failure.clone();
    let writer = tokio::spawn(async move {
        while let Some(outgoing) = outgoing.recv().await {
            let message = serde_json::to_value(&outgoing.message).unwrap();
            let operation = message["type"].as_str().unwrap();
            let mut bytes = serde_json::to_vec(&message).unwrap();
            bytes.push(b'\n');
            let result = async {
                stdin.write_all(&bytes).await?;
                stdin.flush().await
            }
            .await
            .map_err(|error| TelemetryError::CommandFailed {
                operation: operation.into(),
                cause: io_cause(error),
            });
            let failure = result.clone().err();
            let _ = outgoing.complete.send(result);
            if let Some(error) = failure {
                write_failure.send_if_modified(|failure| {
                    if failure.is_none() {
                        *failure = Some(error);
                        true
                    } else {
                        false
                    }
                });
                break;
            }
        }
    });
    let stderr = tokio::spawn(async move {
        let mut buffer = [0_u8; 8192];
        while let Ok(count) = stderr.read(&mut buffer).await {
            if count == 0 {
                break;
            }
        }
    });
    let active = async {
        let hello = tokio::time::timeout(
            shared.options.handshake_timeout,
            greeted.wait_for(|hello| hello.is_some()),
        )
        .await
        .map_err(|_| TelemetryError::HandshakeTimedOut {
            timeout_ms: shared.options.handshake_timeout.as_millis() as u64,
        })?
        .map_err(|error| command_error("hello", error))?
        .as_ref()
        .unwrap()
        .clone();
        {
            let mut collection = shared.collection.lock().await;
            let external = shared.external.lock().unwrap().clone();
            shared.write(decode_command(json!({"version":3,"type":"configure","rootPid":std::process::id(),"sampleIntervalMs":collection.desired.interval,"externalProcesses":external}))).await?;
            if collection.desired.live_subscribers > 0 {
                shared
                    .write(decode_command(
                        json!({"version":3,"type":"setStreaming","enabled":true}),
                    ))
                    .await?;
            }
            collection.applied = collection.desired.clone();
            shared.update_health(|health| {
                health.status = ResourceTelemetrySourceStatus::Healthy;
                health.hello = Some(hello);
            });
        }
        let external = shared.external.lock().unwrap().clone();
        shared
            .write(decode_command(
                json!({"version":3,"type":"setExternalProcesses","processes":external}),
            ))
            .await?;
        std::future::pending::<Result<(), TelemetryError>>().await
    };
    let result = tokio::select! {biased;_=stopped.wait_for(|value|*value)=>Ok(()),error=async{failed.wait_for(|error|error.is_some()).await.map(|value|value.as_ref().unwrap().clone())}=>Err(error.unwrap_or(TelemetryError::StreamClosed)),status=child.wait()=>Err(match status{Ok(status)=>TelemetryError::Exited{exit_code:status.code().unwrap_or(-1)},Err(error)=>command_error("waitForExit",error)}),result=active=>result};
    *shared.writer.lock().unwrap() = None;
    reader.abort();
    writer.abort();
    stderr.abort();
    let _ = reader.await;
    let _ = writer.await;
    let _ = stderr.await;
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    #[cfg(windows)]
    let _ = child.start_kill();
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(_)) => {}
        other => {
            if let Ok(Err(error)) = other {
                tracing::warn!(%error,"native telemetry child wait failed");
            }
            if let Err(error) = child.kill().await {
                tracing::warn!(%error,"native telemetry child kill failed");
            }
            if let Err(error) = child.wait().await {
                tracing::error!(%error,"native telemetry child reap failed");
            }
        }
    }
    result
}

// Effect decodeText replaces invalid UTF-8, strips initial BOM and does not
// flush incomplete UTF-8 at EOF. splitLines emits CR/LF and final partial lines.
// Source NDJSON imposes no size limit on trusted native history frames.
struct NdjsonLines<R> {
    reader: R,
    pending: Vec<u8>,
    skip_lf: bool,
    first: bool,
}
impl<R: AsyncBufRead + Unpin> NdjsonLines<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            pending: vec![],
            skip_lf: false,
            first: true,
        }
    }
    fn decode(&mut self, eof: bool) -> String {
        if eof {
            let mut offset = 0;
            while let Err(error) = std::str::from_utf8(&self.pending[offset..]) {
                offset += error.valid_up_to();
                match error.error_len() {
                    Some(length) => offset += length,
                    None => {
                        self.pending.truncate(offset);
                        break;
                    }
                }
            }
        }
        let bytes = std::mem::take(&mut self.pending);
        let text = String::from_utf8_lossy(&bytes);
        let text = if self.first {
            self.first = false;
            text.strip_prefix('\u{feff}').unwrap_or(&text)
        } else {
            &text
        };
        text.to_owned()
    }
    async fn next(&mut self) -> io::Result<Option<String>> {
        loop {
            let buffer = self.reader.fill_buf().await?;
            if buffer.is_empty() {
                let text = self.decode(true);
                return Ok((!text.is_empty()).then_some(text));
            }
            if self.skip_lf {
                self.skip_lf = false;
                if buffer[0] == b'\n' {
                    self.reader.consume(1);
                    continue;
                }
            }
            if let Some(end) = buffer
                .iter()
                .position(|byte| *byte == b'\r' || *byte == b'\n')
            {
                self.pending.extend_from_slice(&buffer[..end]);
                self.skip_lf = buffer[end] == b'\r';
                self.reader.consume(end + 1);
                return Ok(Some(self.decode(false)));
            }
            self.pending.extend_from_slice(buffer);
            let count = buffer.len();
            self.reader.consume(count);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn fixture(mode: &str) -> (tempfile::TempDir, NativeTelemetryClient) {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("monitor.py");
        std::fs::write(
            &script,
            include_str!("../tests/fixtures/resource-monitor.py")
                .replace("__MODE__", mode)
                .replace(
                    "__LOG__",
                    &directory.path().join("commands.jsonl").to_string_lossy(),
                ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut options = NativeTelemetryOptions::host(directory.path().to_owned(), None);
        options.binary.overrides = vec![script];
        options.binary.directories.clear();
        if mode == "timeout" {
            options.request_timeout = Duration::ZERO;
        }
        if mode == "nohello" {
            options.handshake_timeout = Duration::ZERO;
        }
        (directory, NativeTelemetryClient::new(options))
    }
    async fn healthy(client: &NativeTelemetryClient) {
        let mut health = client.subscribe_health();
        tokio::time::timeout(
            Duration::from_secs(5),
            health.wait_for(|health| health.status == ResourceTelemetrySourceStatus::Healthy),
        )
        .await
        .unwrap()
        .unwrap();
    }
    fn dead(pid: u32) {
        assert_eq!(
            unsafe { libc::kill(pid as i32, 0) },
            -1,
            "owned sidecar remains alive"
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    #[tokio::test]
    async fn sidecar_handshake_correlated_requests_chunked_history_and_owned_shutdown() {
        let (_directory, client) = fixture("normal");
        healthy(&client).await;
        let pid = client.health().hello.unwrap().sidecar_pid.0 as u32;
        let snapshot = client.sample_now().await.unwrap();
        assert_eq!(snapshot.snapshot.sequence.0, 1);
        let table = client.process_table().await.unwrap();
        assert_eq!(table[0].pid.0, pid as u64);
        let history = client.read_history(60_000).await.unwrap();
        assert_eq!(history.len(), 2);
        let mut stream = client.subscribe().await.unwrap();
        let snapshot = tokio::time::timeout(Duration::from_secs(5), stream.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.generation, 0);
        drop(stream);
        let ((), ()) = tokio::join!(client.shutdown(), client.shutdown());
        assert_eq!(
            client.health().status,
            ResourceTelemetrySourceStatus::Stopped
        );
        dead(pid);
    }
    #[tokio::test]
    async fn cancelled_subscription_acquisition_releases_demand_but_apply_error_retains_desired() {
        let (_directory, client) = fixture("normal");
        healthy(&client).await;
        let (writer, mut outgoing) = mpsc::channel(4);
        *client.0.shared.writer.lock().unwrap() = Some(writer);
        let task = tokio::spawn({
            let client = client.clone();
            async move { client.subscribe().await }
        });
        let held = outgoing.recv().await.unwrap();
        assert!(matches!(
            held.message,
            ResourceMonitorCommand::SetSampleInterval(_)
        ));
        assert_eq!(client.health().sample_interval_ms, 1000);
        task.abort();
        assert!(matches!(task.await,Err(error) if error.is_cancelled()));
        let mut health = client.subscribe_health();
        tokio::time::timeout(
            Duration::from_secs(5),
            health.wait_for(|health| health.sample_interval_ms == 5000),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            client
                .0
                .shared
                .collection
                .lock()
                .await
                .desired
                .live_subscribers,
            0
        );
        drop(held);
        let task = tokio::spawn({
            let client = client.clone();
            async move { client.subscribe().await }
        });
        let held = outgoing.recv().await.unwrap();
        held.complete
            .send(Err(command_error("setSampleInterval", "fixture failure")))
            .unwrap();
        assert!(task.await.unwrap().is_err());
        assert_eq!(
            client
                .0
                .shared
                .collection
                .lock()
                .await
                .desired
                .live_subscribers,
            1
        );
        client.shutdown().await;
    }
    #[tokio::test]
    async fn cancelled_request_removes_waiter_and_other_operations_remain_usable() {
        let (_directory, client) = fixture("hold");
        healthy(&client).await;
        let (writer, mut outgoing) = mpsc::channel(4);
        *client.0.shared.writer.lock().unwrap() = Some(writer);
        let task = tokio::spawn({
            let client = client.clone();
            async move { client.process_table().await }
        });
        let held = outgoing.recv().await.unwrap();
        assert_eq!(client.0.shared.pending.lock().unwrap().len(), 1);
        held.complete.send(Ok(())).unwrap();
        task.abort();
        assert!(matches!(task.await,Err(error) if error.is_cancelled()));
        assert!(client.0.shared.pending.lock().unwrap().is_empty());
        client.shutdown().await;
    }
    #[tokio::test]
    async fn restarted_sidecar_reestablishes_handshake_and_request_generation() {
        let (_directory, client) = fixture("restart");
        healthy(&client).await;
        let _ = client.sample_now().await;
        let mut health = client.subscribe_health();
        tokio::time::timeout(
            Duration::from_secs(5),
            health.wait_for(|health| {
                health.restart_count == 1 && health.status == ResourceTelemetrySourceStatus::Healthy
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(client.sample_now().await.unwrap().generation, 1);
        assert_eq!(client.process_table().await.unwrap().len(), 1);
        client.shutdown().await;
    }
    #[tokio::test]
    async fn cancelled_first_shutdown_preserves_owned_join_for_next_caller() {
        let (_directory, client) = fixture("hold-term");
        healthy(&client).await;
        let pid = client.health().hello.unwrap().sidecar_pid.0 as u32;
        let mut stopped = client.0.shared.stop.subscribe();
        let first = tokio::spawn({
            let client = client.clone();
            async move {
                client.shutdown().await;
            }
        });
        stopped.wait_for(|value| *value).await.unwrap();
        first.abort();
        assert!(matches!(first.await,Err(error) if error.is_cancelled()));
        client.shutdown().await;
        dead(pid);
        assert!(!client.retry());
    }
    #[tokio::test]
    async fn final_client_drop_reaps_child_without_explicit_shutdown() {
        let (_directory, client) = fixture("normal");
        healthy(&client).await;
        let pid = client.health().hello.unwrap().sidecar_pid.0 as u32;
        let mut health = client.subscribe_health();
        drop(client);
        tokio::time::timeout(
            Duration::from_secs(5),
            health.wait_for(|health| health.status == ResourceTelemetrySourceStatus::Stopped),
        )
        .await
        .unwrap()
        .unwrap();
        dead(pid);
    }
    #[test]
    fn subscription_drop_after_runtime_shutdown_does_not_spawn_or_panic() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let subscription = runtime.block_on(async {
            let (_directory, client) = fixture("normal");
            healthy(&client).await;
            let subscription = client.subscribe().await.unwrap();
            client.shutdown().await;
            subscription
        });
        drop(runtime);
        drop(subscription);
    }
    #[tokio::test]
    async fn wrong_response_kind_does_not_consume_request_identity() {
        let (_directory, client) = fixture("normal");
        healthy(&client).await;
        let (writer, mut outgoing) = mpsc::channel(4);
        *client.0.shared.writer.lock().unwrap() = Some(writer);
        let task = tokio::spawn({
            let client = client.clone();
            async move { client.process_table().await }
        });
        let held = outgoing.recv().await.unwrap();
        let id = serde_json::to_value(&held.message).unwrap()["requestId"]
            .as_str()
            .unwrap()
            .to_owned();
        held.complete.send(Ok(())).unwrap();
        let (hello, _) = watch::channel(None);
        let wrong = serde_json::from_value(
            json!({"version":3,"type":"historyChunk","requestId":id,"done":true,"snapshots":[]}),
        )
        .unwrap();
        client.0.shared.event(wrong, 0, &hello).unwrap();
        assert_eq!(client.0.shared.pending.lock().unwrap().len(), 1);
        let right = serde_json::from_value(
            json!({"version":3,"type":"processTable","requestId":id,"processes":[]}),
        )
        .unwrap();
        client.0.shared.event(right, 0, &hello).unwrap();
        assert!(task.await.unwrap().unwrap().is_empty());
        client.shutdown().await;
    }
    #[tokio::test]
    async fn protocol_mismatch_whitespace_line_and_handshake_timeout_fail_attempt() {
        for (mode, message) in [
            ("bad-version", "protocol 2"),
            ("whitespace", "decode"),
            ("nohello", "handshake timed out"),
        ] {
            let (_directory, client) = fixture(mode);
            let mut health = client.subscribe_health();
            tokio::time::timeout(
                Duration::from_secs(5),
                health.wait_for(|health| health.status == ResourceTelemetrySourceStatus::Degraded),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                client.health().last_error.unwrap().contains(message),
                "mode {mode}"
            );
            assert!(client.capabilities().is_err());
            client.shutdown().await;
        }
    }
    #[tokio::test]
    async fn timed_out_request_releases_waiter_and_does_not_stop_sidecar() {
        let (_directory, client) = fixture("timeout");
        healthy(&client).await;
        assert!(
            matches!(client.process_table().await,Err(TelemetryError::RequestTimedOut{operation,timeout_ms:0}) if operation=="processTable")
        );
        assert!(client.0.shared.pending.lock().unwrap().is_empty());
        assert_eq!(
            client.health().status,
            ResourceTelemetrySourceStatus::Healthy
        );
        client.shutdown().await;
    }
    #[tokio::test]
    async fn ndjson_framing_matches_original_effect_source_oracle() {
        for (index, line) in include_str!("../tests/fixtures/resource-ndjson.jsonl")
            .lines()
            .enumerate()
        {
            let case: Value = serde_json::from_str(line).unwrap();
            let chunks: Vec<Vec<u8>> = serde_json::from_value(case["chunks"].clone()).unwrap();
            // Capacity equals fixture chunk size; the last EOF chunk is short.
            let capacity = chunks.first().map(Vec::len).unwrap_or(1).max(1);
            let bytes = chunks.concat();
            let mut reader = NdjsonLines::new(BufReader::with_capacity(capacity, bytes.as_slice()));
            let mut lines = vec![];
            while let Some(line) = reader.next().await.unwrap() {
                lines.push(line);
            }
            assert_eq!(json!(lines), case["lines"], "Effect framing fixture{index}");
        }
    }
    #[tokio::test]
    async fn ndjson_matches_text_decoder_and_split_lines_boundaries() {
        for capacity in 1..=8 {
            let bytes =
                b"\xef\xbb\xbf{\"text\":\"\xf0\x9f\x98\x80\"}\r\n\n \r{\"invalid\":\"\xff\"}\n{}";
            let mut lines = NdjsonLines::new(BufReader::with_capacity(capacity, &bytes[..]));
            let mut actual = vec![];
            while let Some(line) = lines.next().await.unwrap() {
                actual.push(line);
            }
            assert_eq!(
                actual,
                vec!["{\"text\":\"😀\"}", "", " ", "{\"invalid\":\"�\"}", "{}"]
            );
            assert!(serde_json::from_str::<Value>(&actual[2]).is_err());
        }
        let mut lines = NdjsonLines::new(BufReader::new(&b"\xffx\xe2\x82"[..]));
        assert_eq!(lines.next().await.unwrap().as_deref(), Some("�x"));
    }
    // Builds independently from the main MSRV1.89 workspace. Run after
    // cargo build --manifest-path native/resource-monitor/Cargo.toml
    // --target-dir target/resource-monitor --offline --locked -j2.
    #[tokio::test]
    #[ignore = "requires separately built preserved native monitor (Rust1.95)"]
    async fn preserved_native_monitor_real_process_table_sampling_and_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/resource-monitor/debug/t3-resource-monitor");
        let mut options = NativeTelemetryOptions::host(directory.path().to_owned(), None);
        options.binary.overrides = vec![path];
        options.binary.directories.clear();
        let client = NativeTelemetryClient::new(options);
        healthy(&client).await;
        let pid = client.health().hello.unwrap().sidecar_pid.0 as u32;
        let table = client.process_table().await.unwrap();
        assert!(table.iter().any(|entry| entry.pid.0 == pid as u64));
        let sample = client.sample_now().await.unwrap();
        assert!(sample.snapshot.scanned_process_count.0 > 0);
        assert!(!client.read_history(60_000).await.unwrap().is_empty());
        client.shutdown().await;
        dead(pid);
    }
}
