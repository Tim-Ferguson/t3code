//! Owned desktop telemetry ingress, control writes and source stale policy.
//! The transport accepts owned async pipes; it never opens a live descriptor
//! implicitly. A supervising desktop supplies these pipes when spawning server.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::*;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
};
pub type Reader = Pin<Box<dyn AsyncRead + Send>>;
pub type Writer = Pin<Box<dyn AsyncWrite + Send>>;
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
pub struct DesktopTelemetryOptions {
    pub mode: String,
    pub input: Option<(i32, Reader)>,
    pub control: Option<(i32, Writer)>,
    pub active_interval_ms: u64,
    pub idle_interval_ms: u64,
    pub clock: Clock,
}
impl DesktopTelemetryOptions {
    pub fn unavailable(mode: impl Into<String>) -> Self {
        Self {
            mode: mode.into(),
            input: None,
            control: None,
            active_interval_ms: 30_000,
            idle_interval_ms: 120_000,
            clock: Arc::new(|| chrono::Utc::now().timestamp_millis()),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum DesktopTelemetryError {
    #[error("Desktop telemetry descriptor is unavailable in '{mode}' mode.")]
    DesktopTelemetryDescriptorUnavailable { mode: String },
    #[error(
        "Desktop telemetry protocol {received_version} is incompatible with expected protocol {expected_version}."
    )]
    DesktopTelemetryProtocolMismatch {
        expected_version: u32,
        received_version: serde_json::Number,
    },
    #[error("Failed to decode desktop telemetry.")]
    DesktopTelemetryDecodeFailed { cause: Value },
    #[error("Desktop telemetry stream on fd {fd} failed.")]
    DesktopTelemetryStreamFailed { fd: i32, cause: Value },
    #[error("Desktop telemetry stream on fd {fd} closed.")]
    DesktopTelemetryStreamClosed { fd: i32 },
    #[error("Desktop telemetry on fd {fd} has not updated for {stale_after_ms}ms.")]
    DesktopTelemetryStale { fd: i32, stale_after_ms: u64 },
    #[error("Desktop telemetry control '{operation}' failed on fd {fd}.")]
    DesktopTelemetryControlFailed {
        fd: i32,
        operation: String,
        cause: Value,
    },
    #[error("Desktop telemetry control stalled on fd {fd} with {remaining_bytes} bytes remaining.")]
    DesktopTelemetryControlStalled { fd: i32, remaining_bytes: usize },
}
#[derive(Clone, Debug)]
pub struct DesktopHealth {
    pub status: ResourceTelemetrySourceStatus,
    pub last_sample_at: Option<UtcDateTime>,
    pub last_error: Option<String>,
}
struct State {
    latest: Option<DesktopHostTelemetrySnapshot>,
    update: Option<DesktopUpdateStatusReport>,
    health: DesktopHealth,
    last_contact: Option<i64>,
    stale_after: u64,
}
struct Control {
    bytes: Vec<u8>,
    reply: oneshot::Sender<Result<(), DesktopTelemetryError>>,
}
struct Shared {
    state: Mutex<State>,
    snapshots: broadcast::Sender<DesktopHostTelemetrySnapshot>,
    updates: broadcast::Sender<DesktopUpdateStatusReport>,
    health: broadcast::Sender<DesktopHealth>,
    stop: watch::Sender<bool>,
    clock: Clock,
    fd: Option<i32>,
    control_fd: Option<i32>,
    controls: Option<mpsc::Sender<Control>>,
}
struct Inner {
    shared: Arc<Shared>,
    tasks: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.stop.send_replace(true);
    }
}
#[derive(Clone)]
pub struct DesktopTelemetryReceiver(Arc<Inner>);
pub struct DesktopSubscription {
    pub latest: Option<DesktopHostTelemetrySnapshot>,
    receiver: broadcast::Receiver<DesktopHostTelemetrySnapshot>,
    stop: watch::Receiver<bool>,
}
impl DesktopSubscription {
    pub async fn recv(&mut self) -> Option<DesktopHostTelemetrySnapshot> {
        loop {
            tokio::select! {biased;_=self.stop.wait_for(|stop|*stop)=>return None,result=self.receiver.recv()=>match result{Ok(snapshot)=>return Some(snapshot),Err(broadcast::error::RecvError::Lagged(_))=>continue,Err(_)=>return None}}
        }
    }
}
pub struct DesktopHealthSubscription {
    pub latest: DesktopHealth,
    receiver: broadcast::Receiver<DesktopHealth>,
    stop: watch::Receiver<bool>,
}
impl DesktopHealthSubscription {
    pub async fn recv(&mut self) -> Option<DesktopHealth> {
        loop {
            tokio::select! {biased;_=self.stop.wait_for(|stop|*stop)=>return None,result=self.receiver.recv()=>match result{Ok(health)=>return Some(health),Err(broadcast::error::RecvError::Lagged(_))=>continue,Err(_)=>return None}}
        }
    }
}
pub struct DesktopUpdateSubscription {
    pub latest: Option<DesktopUpdateStatusReport>,
    receiver: broadcast::Receiver<DesktopUpdateStatusReport>,
    stop: watch::Receiver<bool>,
}
impl DesktopUpdateSubscription {
    pub async fn recv(&mut self) -> Option<DesktopUpdateStatusReport> {
        loop {
            tokio::select! {biased;_=self.stop.wait_for(|stop|*stop)=>return None,result=self.receiver.recv()=>match result{Ok(update)=>return Some(update),Err(broadcast::error::RecvError::Lagged(_))=>continue,Err(_)=>return None}}
        }
    }
}
pub fn stale_after(active: u64, idle: u64) -> u64 {
    90_000.max(active.max(idle).saturating_add(30_000))
}
pub fn contact_stale(last: Option<i64>, now: i64) -> bool {
    last.is_some_and(|last| now as i128 - last as i128 >= 90_000)
}
impl Shared {
    fn health(&self, change: impl FnOnce(&mut DesktopHealth)) {
        let mut state = self.state.lock().unwrap();
        change(&mut state.health);
        let _ = self.health.send(state.health.clone());
    }
    fn failed(&self, error: DesktopTelemetryError) {
        self.health(|health| {
            health.status = ResourceTelemetrySourceStatus::Degraded;
            health.last_error = Some(error.to_string());
        });
    }
    fn ingest(&self, message: DesktopHostTelemetryMessage) {
        let mut state = self.state.lock().unwrap();
        state.last_contact = Some((self.clock)());
        match message {
            DesktopHostTelemetryMessage::Hello(_) => {
                state.health.status = ResourceTelemetrySourceStatus::Healthy;
                state.health.last_error = None;
                let _ = self.health.send(state.health.clone());
            }
            DesktopHostTelemetryMessage::Update(report) => {
                state.update = Some(report.clone());
                let _ = self.updates.send(report);
            }
            DesktopHostTelemetryMessage::Snapshot(snapshot) => {
                let sampled =
                    chrono::DateTime::from_timestamp_millis(snapshot.sampled_at_unix_ms.0 as i64)
                        .expect("source DateTime range")
                        .into();
                state.health = DesktopHealth {
                    status: ResourceTelemetrySourceStatus::Healthy,
                    last_sample_at: Some(sampled),
                    last_error: None,
                };
                state.latest = Some(snapshot.clone());
                let _ = self.health.send(state.health.clone());
                let _ = self.snapshots.send(snapshot);
            }
        }
    }
    fn stale_tick(&self) {
        let Some(fd) = self.fd else {
            return;
        };
        let now = (self.clock)();
        let mut state = self.state.lock().unwrap();
        let stale_after = state.stale_after;
        let stale = state.latest.as_ref().is_some_and(|snapshot| {
            !snapshot.power.stale
                && now as i128 - snapshot.sampled_at_unix_ms.0 as i128 >= stale_after as i128
        });
        if stale {
            let snapshot = state.latest.as_mut().unwrap();
            snapshot.power.stale = true;
            let snapshot = snapshot.clone();
            if state.health.status != ResourceTelemetrySourceStatus::Stopped {
                state.health.status = ResourceTelemetrySourceStatus::Degraded;
                state.health.last_error = Some(
                    DesktopTelemetryError::DesktopTelemetryStale {
                        fd,
                        stale_after_ms: stale_after,
                    }
                    .to_string(),
                );
            }
            let _ = self.health.send(state.health.clone());
            let _ = self.snapshots.send(snapshot);
            return;
        }
        if !contact_stale(state.last_contact, now)
            || state.health.status == ResourceTelemetrySourceStatus::Stopped
            || state.health.last_sample_at.is_some()
        {
            return;
        }
        let message = DesktopTelemetryError::DesktopTelemetryStale {
            fd,
            stale_after_ms: 90_000,
        }
        .to_string();
        if state.health.status == ResourceTelemetrySourceStatus::Degraded
            && state.health.last_error.as_ref() == Some(&message)
        {
            return;
        }
        state.health.status = ResourceTelemetrySourceStatus::Degraded;
        state.health.last_error = Some(message);
        let _ = self.health.send(state.health.clone());
    }
}
impl DesktopTelemetryReceiver {
    pub async fn new(mut options: DesktopTelemetryOptions) -> Self {
        let fd = options.input.as_ref().map(|(fd, _)| *fd);
        let control_fd = options.control.as_ref().map(|(fd, _)| *fd);
        let initial = DesktopHealth {
            status: if fd.is_some() {
                ResourceTelemetrySourceStatus::Starting
            } else {
                ResourceTelemetrySourceStatus::Unavailable
            },
            last_sample_at: None,
            last_error: if fd.is_none() {
                Some(
                    DesktopTelemetryError::DesktopTelemetryDescriptorUnavailable {
                        mode: options.mode,
                    }
                    .to_string(),
                )
            } else {
                None
            },
        };
        let (health, _) = broadcast::channel(4);
        let (stop, _) = watch::channel(false);
        let (snapshots, _) = broadcast::channel(8);
        let (updates, _) = broadcast::channel(16);
        let (controls, mut queued) = mpsc::channel::<Control>(32);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                latest: None,
                update: None,
                health: initial,
                last_contact: fd.map(|_| (options.clock)()),
                stale_after: stale_after(30_000, 120_000),
            }),
            snapshots,
            updates,
            health,
            stop,
            clock: options.clock.clone(),
            fd,
            control_fd,
            controls: control_fd.map(|_| controls),
        });
        let receiver = Self(Arc::new(Inner {
            shared: shared.clone(),
            tasks: tokio::sync::Mutex::new(Vec::new()),
        }));
        if let Some((fd, mut writer)) = options.control.take() {
            let owner = shared.clone();
            let mut stop = owner.stop.subscribe();
            receiver.0.tasks.try_lock().unwrap().push(tokio::spawn(async move {
                loop {let command=tokio::select!{biased;_=stop.wait_for(|stop|*stop)=>break,command=queued.recv()=>command};let Some(command)=command else{break;};
                    let result=tokio::select!{biased;_=stop.wait_for(|stop|*stop)=>Err(control_error(fd,"write","receiver stopped")),result=write_frame(fd,&mut writer,&command.bytes)=>result};
                    if let Err(error)=&result {owner.failed(error.clone());}let _=command.reply.send(result);
                }
            }));
        }
        if control_fd.is_some() {
            // Source initializes control intervals before reading ingress. The
            // local owner exists already, so cancelled construction stops/reaps
            // its writer and drops the as-yet-unstarted input pipe.
            let _ = receiver
                .set_host_power_intervals(options.active_interval_ms, options.idle_interval_ms)
                .await;
        }
        if let Some((fd, reader)) = options.input.take() {
            let owner = shared.clone();
            let mut stop = owner.stop.subscribe();
            receiver.0.tasks.try_lock().unwrap().push(tokio::spawn(async move {
                let mut lines=crate::native_telemetry::NdjsonLines::new(BufReader::new(reader));
                loop {
                    let line=tokio::select!{biased;_=stop.wait_for(|stop|*stop)=>return,line=lines.next()=>line};
                    let line=match line{Ok(Some(line))=>line,Ok(None)=>{owner.health(|health|{health.status=ResourceTelemetrySourceStatus::Stopped;health.last_error=Some(DesktopTelemetryError::DesktopTelemetryStreamClosed{fd}.to_string());});return;},Err(error)=>{owner.failed(DesktopTelemetryError::DesktopTelemetryStreamFailed{fd,cause:json!({"message":error.to_string(),"code":error.raw_os_error()})});return;}};
                    if line.is_empty(){continue;}
                    match decode(&line){Ok(message)=>owner.ingest(message),Err(error)=>{owner.failed(error);return;}}
                }
            }));
            let owner = shared.clone();
            let mut stop = owner.stop.subscribe();
            receiver.0.tasks.try_lock().unwrap().push(tokio::spawn(async move {loop{tokio::select!{biased;_=stop.wait_for(|stop|*stop)=>return,_=tokio::time::sleep(Duration::from_secs(30))=>owner.stale_tick()}}}));
        }
        receiver
    }
    pub fn latest(&self) -> Option<DesktopHostTelemetrySnapshot> {
        self.0.shared.state.lock().unwrap().latest.clone()
    }
    pub fn health(&self) -> DesktopHealth {
        self.0.shared.state.lock().unwrap().health.clone()
    }
    pub fn subscribe_health(&self) -> DesktopHealthSubscription {
        let state = self.0.shared.state.lock().unwrap();
        DesktopHealthSubscription {
            latest: state.health.clone(),
            receiver: self.0.shared.health.subscribe(),
            stop: self.0.shared.stop.subscribe(),
        }
    }
    pub fn subscribe(&self) -> DesktopSubscription {
        let state = self.0.shared.state.lock().unwrap();
        DesktopSubscription {
            latest: state.latest.clone(),
            receiver: self.0.shared.snapshots.subscribe(),
            stop: self.0.shared.stop.subscribe(),
        }
    }
    pub fn updates(&self) -> DesktopUpdateSubscription {
        let state = self.0.shared.state.lock().unwrap();
        DesktopUpdateSubscription {
            latest: state.update.clone(),
            receiver: self.0.shared.updates.subscribe(),
            stop: self.0.shared.stop.subscribe(),
        }
    }
    async fn control(&self, value: Value) -> Result<(), DesktopTelemetryError> {
        Self::control_shared(&self.0.shared, value).await
    }
    async fn control_shared(
        shared: &Arc<Shared>,
        value: Value,
    ) -> Result<(), DesktopTelemetryError> {
        let Some(fd) = shared.control_fd else {
            return Ok(());
        };
        let typed: DesktopTelemetryControlMessage =
            serde_json::from_value(value).map_err(|error| control_error(fd, "encode", error))?;
        let mut bytes =
            serde_json::to_vec(&typed).map_err(|error| control_error(fd, "encode", error))?;
        bytes.push(b'\n');
        let (reply, wait) = oneshot::channel();
        let mut stopped = shared.stop.subscribe();
        tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>return Err(control_error(fd,"write","receiver stopped")),result=shared.controls.as_ref().unwrap().send(Control{bytes,reply})=>result.map_err(|error|control_error(fd,"write",error))?};
        tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>Err(control_error(fd,"write","receiver stopped")),result=wait=>result.map_err(|error|control_error(fd,"write",error))?}
    }
    pub async fn set_diagnostics_demand(&self, enabled: bool) -> Result<(), DesktopTelemetryError> {
        self.control(json!({"version":1,"type":"setDiagnosticsDemand","enabled":enabled}))
            .await
    }
    pub async fn set_host_power_intervals(
        &self,
        active: u64,
        idle: u64,
    ) -> Result<(), DesktopTelemetryError> {
        self.control(json!({"version":1,"type":"setHostPowerIntervals","activeIntervalMs":active,"idleIntervalMs":idle})).await?;
        self.0.shared.state.lock().unwrap().stale_after = stale_after(active, idle);
        Ok(())
    }
    pub async fn request_update(&self, id: &str) -> Result<(), DesktopTelemetryError> {
        self.control(json!({"version":1,"type":"requestDesktopUpdate","requestId":id}))
            .await
    }
    pub async fn commit_update(&self, id: &str) -> Result<(), DesktopTelemetryError> {
        self.control(json!({"version":1,"type":"commitDesktopUpdate","requestId":id}))
            .await
    }
    pub async fn cancel_update(&self, id: &str) -> Result<(), DesktopTelemetryError> {
        self.control(json!({"version":1,"type":"cancelDesktopUpdate","requestId":id}))
            .await
    }
    pub async fn shutdown(&self) {
        self.0.shared.stop.send_replace(true);
        let mut tasks = self.0.tasks.lock().await;
        while let Some(task) = tasks.last_mut() {
            let _ = task.await;
            tasks.pop();
        }
    }
}
fn control_error(fd: i32, operation: &str, cause: impl std::fmt::Display) -> DesktopTelemetryError {
    DesktopTelemetryError::DesktopTelemetryControlFailed {
        fd,
        operation: operation.into(),
        cause: json!({"message":cause.to_string()}),
    }
}
async fn write_frame(
    fd: i32,
    writer: &mut Writer,
    bytes: &[u8],
) -> Result<(), DesktopTelemetryError> {
    let mut offset = 0;
    while offset < bytes.len() {
        let written = writer
            .write(&bytes[offset..])
            .await
            .map_err(|error| control_error(fd, "write", error))?;
        if written == 0 {
            return Err(DesktopTelemetryError::DesktopTelemetryControlStalled {
                fd,
                remaining_bytes: bytes.len() - offset,
            });
        }
        offset += written;
    }
    Ok(())
}
fn decode(line: &str) -> Result<DesktopHostTelemetryMessage, DesktopTelemetryError> {
    let value: Value = serde_json::from_str(line).map_err(|error| {
        DesktopTelemetryError::DesktopTelemetryDecodeFailed {
            cause: json!({"message":error.to_string()}),
        }
    })?;
    if let Some(version) = value.get("version").and_then(Value::as_number) {
        if version.as_f64() != Some(1.) {
            return Err(DesktopTelemetryError::DesktopTelemetryProtocolMismatch {
                expected_version: 1,
                received_version: version.clone(),
            });
        }
    }
    serde_json::from_value(value).map_err(|error| {
        DesktopTelemetryError::DesktopTelemetryDecodeFailed {
            cause: json!({"message":error.to_string()}),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::poll;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    const BASE: u64 = 1_700_000_000_000;
    fn sample(sequence: u64, time: u64) -> Value {
        json!({"version":1,"type":"desktopTelemetry","sequence":sequence,"sampledAtUnixMs":time,"electronPid":100,"power":{"source":"electron-main","idle":"false","idleSeconds":0,"locked":"false","suspended":false,"onBattery":"false","lowPowerMode":"unknown","thermalState":"nominal","stale":false,"updatedAt":chrono::DateTime::from_timestamp_millis(time as i64).unwrap().to_rfc3339()},"speedLimitPercent":null,"electronProcesses":[]})
    }
    fn update() -> Value {
        json!({"version":1,"type":"desktopUpdateStatus","requestId":"update-request","outcome":"ready-to-install","state":{"enabled":true,"status":"downloaded","channel":"latest","currentVersion":"1.0","hostArch":"arm64","appArch":"arm64","runningUnderArm64Translation":false,"availableVersion":"1.1","downloadedVersion":"1.1","releaseNotes":[],"omittedReleaseCount":0,"downloadPercent":100,"checkedAt":null,"message":null,"errorContext":null,"canRetry":false}})
    }
    async fn receive<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn owned_ingress_controls_update_reports_and_eof_preserve_source_order() {
        let (input, mut desktop) = tokio::io::duplex(4096);
        let (control, output) = tokio::io::duplex(4096);
        let mut options = DesktopTelemetryOptions::unavailable("desktop");
        options.input = Some((3, Box::pin(input)));
        options.control = Some((4, Box::pin(control)));
        let receiver = DesktopTelemetryReceiver::new(options).await;
        let mut lines = BufReader::new(output);
        let mut line = String::new();
        receive(lines.read_line(&mut line)).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"version":1,"type":"setHostPowerIntervals","activeIntervalMs":30000,"idleIntervalMs":120000})
        );
        receiver.request_update("request-one").await.unwrap();
        line.clear();
        receive(lines.read_line(&mut line)).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["requestId"],
            "request-one"
        );
        let mut health = receiver.subscribe_health();
        let mut snapshots = receiver.subscribe();
        let mut updates = receiver.updates();
        assert!(snapshots.latest.is_none());
        desktop.write_all(b"\xef\xbb\xbf{\"version\":1,\"type\":\"desktopTelemetryHello\",\"electronPid\":100}\r\n").await.unwrap();
        assert_eq!(
            receive(health.recv()).await.unwrap().status,
            ResourceTelemetrySourceStatus::Healthy
        );
        assert!(receiver.health().last_sample_at.is_none());
        desktop
            .write_all(format!("{}\n", sample(1, BASE)).as_bytes())
            .await
            .unwrap();
        let snapshot = receive(snapshots.recv()).await.unwrap();
        assert_eq!(snapshot.sequence.0, 1);
        let sampled = receive(health.recv())
            .await
            .unwrap()
            .last_sample_at
            .unwrap();
        desktop
            .write_all(format!("{}\n", update()).as_bytes())
            .await
            .unwrap();
        let report = receive(updates.recv()).await.unwrap();
        assert_eq!(report.request_id.unwrap().as_str(), "update-request");
        assert_eq!(receiver.latest().unwrap().sequence.0, 1);
        assert_eq!(receiver.health().last_sample_at.unwrap(), sampled);
        drop(desktop);
        assert_eq!(
            receive(health.recv()).await.unwrap().status,
            ResourceTelemetrySourceStatus::Stopped
        );
        receiver.shutdown().await;
        assert!(snapshots.recv().await.is_none());
        assert!(updates.recv().await.is_none());
    }
    #[tokio::test]
    async fn cancelled_initial_control_write_closes_all_owned_pipes() {
        let (input, mut desktop) = tokio::io::duplex(8);
        let (control, mut output) = tokio::io::duplex(1);
        let mut options = DesktopTelemetryOptions::unavailable("desktop");
        options.input = Some((3, Box::pin(input)));
        options.control = Some((4, Box::pin(control)));
        let construction = tokio::spawn(DesktopTelemetryReceiver::new(options));
        let mut first = [0];
        receive(output.read_exact(&mut first)).await.unwrap();
        assert_eq!(first[0], b'{');
        construction.abort();
        assert!(matches!(construction.await,Err(error)if error.is_cancelled()));
        let mut tail = Vec::new();
        receive(output.read_to_end(&mut tail)).await.unwrap();
        let mut byte = [0];
        assert_eq!(receive(desktop.read(&mut byte)).await.unwrap(), 0);
    }
    #[tokio::test]
    async fn sliding_health_four_and_atomic_subscribe_seed_preserve_bursts_and_races() {
        let receiver =
            DesktopTelemetryReceiver::new(DesktopTelemetryOptions::unavailable("web")).await;
        let mut subscription = receiver.subscribe_health();
        assert_eq!(
            subscription.latest.status,
            ResourceTelemetrySourceStatus::Unavailable
        );
        for sequence in 0..7 {
            receiver
                .0
                .shared
                .ingest(serde_json::from_value(sample(sequence, BASE + sequence)).unwrap());
        }
        for sequence in 3..7 {
            let health = receive(subscription.recv()).await.unwrap();
            assert_eq!(
                health.last_sample_at.unwrap().timestamp_millis(),
                (BASE + sequence) as i64
            );
        }
        for sequence in 7..107 {
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let subscribing = receiver.clone();
            let writing = receiver.0.shared.clone();
            let a = barrier.clone();
            let subscriber = std::thread::spawn(move || {
                a.wait();
                subscribing.subscribe_health()
            });
            let b = barrier.clone();
            let writer = std::thread::spawn(move || {
                b.wait();
                writing.ingest(serde_json::from_value(sample(sequence, BASE + sequence)).unwrap());
            });
            barrier.wait();
            let mut subscription = subscriber.join().unwrap();
            writer.join().unwrap();
            let latest = subscription
                .latest
                .last_sample_at
                .unwrap()
                .timestamp_millis();
            if latest == (BASE + sequence) as i64 {
                assert!(matches!(
                    subscription.receiver.try_recv(),
                    Err(broadcast::error::TryRecvError::Empty)
                ));
            } else {
                assert_eq!(latest, (BASE + sequence - 1) as i64);
                assert_eq!(
                    subscription
                        .receiver
                        .try_recv()
                        .unwrap()
                        .last_sample_at
                        .unwrap()
                        .timestamp_millis(),
                    (BASE + sequence) as i64
                );
            }
        }
        receiver.shutdown().await;
    }
    #[tokio::test]
    async fn source_contact_and_snapshot_staleness_keep_stopped_health_and_sample_identity() {
        let now = Arc::new(std::sync::atomic::AtomicI64::new(BASE as i64));
        let (input, _desktop) = tokio::io::duplex(64);
        let mut options = DesktopTelemetryOptions::unavailable("desktop");
        options.input = Some((3, Box::pin(input)));
        options.clock = Arc::new({
            let now = now.clone();
            move || now.load(std::sync::atomic::Ordering::SeqCst)
        });
        let receiver = DesktopTelemetryReceiver::new(options).await;
        now.store(BASE as i64 + 89999, std::sync::atomic::Ordering::SeqCst);
        receiver.0.shared.stale_tick();
        assert_eq!(
            receiver.health().status,
            ResourceTelemetrySourceStatus::Starting
        );
        now.store(BASE as i64 + 90000, std::sync::atomic::Ordering::SeqCst);
        receiver.0.shared.stale_tick();
        assert_eq!(
            receiver.health().status,
            ResourceTelemetrySourceStatus::Degraded
        );
        receiver
            .0
            .shared
            .ingest(serde_json::from_value(sample(1, BASE + 90000)).unwrap());
        now.store(BASE as i64 + 239999, std::sync::atomic::Ordering::SeqCst);
        receiver.0.shared.stale_tick();
        assert!(!receiver.latest().unwrap().power.stale);
        now.store(BASE as i64 + 240000, std::sync::atomic::Ordering::SeqCst);
        receiver.0.shared.stale_tick();
        assert!(receiver.latest().unwrap().power.stale);
        assert_eq!(
            receiver.health().status,
            ResourceTelemetrySourceStatus::Degraded
        );
        receiver.0.shared.health(|health| {
            health.status = ResourceTelemetrySourceStatus::Stopped;
            health.last_error = Some("closed".into());
        });
        receiver
            .0
            .shared
            .ingest(serde_json::from_value(sample(2, BASE + 240000)).unwrap());
        receiver.0.shared.health(|health| {
            health.status = ResourceTelemetrySourceStatus::Stopped;
            health.last_error = Some("closed".into());
        });
        now.store(BASE as i64 + 390000, std::sync::atomic::Ordering::SeqCst);
        receiver.0.shared.stale_tick();
        assert!(receiver.latest().unwrap().power.stale);
        assert_eq!(
            receiver.health().status,
            ResourceTelemetrySourceStatus::Stopped
        );
        assert_eq!(receiver.health().last_error.as_deref(), Some("closed"));
        receiver.shutdown().await;
    }
    #[tokio::test]
    async fn cancelled_shutdown_retains_owned_joins_for_next_caller() {
        let (input, mut desktop) = tokio::io::duplex(8);
        let mut options = DesktopTelemetryOptions::unavailable("desktop");
        options.input = Some((3, Box::pin(input)));
        let receiver = DesktopTelemetryReceiver::new(options).await;
        let (release, held) = oneshot::channel();
        receiver.0.tasks.lock().await.push(tokio::spawn(async {
            let _ = held.await;
        }));
        let mut first = Box::pin(receiver.shutdown());
        assert!(poll!(first.as_mut()).is_pending());
        drop(first);
        release.send(()).unwrap();
        receiver.shutdown().await;
        let mut byte = [0];
        assert_eq!(receive(desktop.read(&mut byte)).await.unwrap(), 0);
    }
    #[tokio::test]
    async fn malformed_or_incompatible_input_has_source_error_category() {
        for (line, expected) in [
            (
                "{\"version\":2,\"type\":\"desktopTelemetryHello\",\"electronPid\":100}\n",
                "incompatible",
            ),
            (" \n", "decode"),
            (
                "{\"version\":1,\"type\":\"desktopUpdateStatus\"}\n",
                "decode",
            ),
        ] {
            let (input, mut desktop) = tokio::io::duplex(4096);
            let mut options = DesktopTelemetryOptions::unavailable("desktop");
            options.input = Some((3, Box::pin(input)));
            let receiver = DesktopTelemetryReceiver::new(options).await;
            let mut health = receiver.subscribe_health();
            desktop.write_all(line.as_bytes()).await.unwrap();
            let failed = receive(health.recv()).await.unwrap();
            assert_eq!(failed.status, ResourceTelemetrySourceStatus::Degraded);
            assert!(failed.last_error.unwrap().contains(expected));
            receiver.shutdown().await;
        }
    }
    struct ZeroWriter;
    impl AsyncWrite for ZeroWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(0))
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    #[tokio::test]
    async fn stalled_control_is_typed_and_hello_recovers_health() {
        let mut options = DesktopTelemetryOptions::unavailable("desktop");
        options.control = Some((4, Box::pin(ZeroWriter)));
        options.active_interval_ms = 300_000;
        options.idle_interval_ms = 1_000_000;
        let receiver = DesktopTelemetryReceiver::new(options).await;
        assert_eq!(receiver.0.shared.state.lock().unwrap().stale_after, 150_000);
        assert_eq!(
            receiver.health().status,
            ResourceTelemetrySourceStatus::Degraded
        );
        assert!(
            matches!(receiver.set_diagnostics_demand(true).await,Err(DesktopTelemetryError::DesktopTelemetryControlStalled{fd:4,remaining_bytes})if remaining_bytes>0)
        );
        receiver.0.shared.ingest(
            serde_json::from_value(
                json!({"version":1,"type":"desktopTelemetryHello","electronPid":100}),
            )
            .unwrap(),
        );
        assert_eq!(
            receiver.health().status,
            ResourceTelemetrySourceStatus::Healthy
        );
        assert!(receiver.health().last_sample_at.is_none());
        receiver.shutdown().await;
    }
}
