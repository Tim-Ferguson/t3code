//! Resource telemetry aggregation and scoped live demand from ResourceTelemetry.ts.
//! Keep Number-domain model values until the public schema is encoded; inferring
//! Electron CPU can legitimately produce values rejected by that wire schema.
use crate::{
    desktop_telemetry::{Clock, DesktopHealth, DesktopTelemetryReceiver},
    native_telemetry::{NativeSnapshot, NativeTelemetryClient, NativeTelemetryHealth},
    resource_attribution::ResourceAttribution,
    resource_history::{self, History, HistoryInput},
    resource_model::{self, MergeInput, ProcessState, TelemetryCounters},
};
use indexmap::{IndexMap, IndexSet};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use t3_contracts::*;
use tokio::{
    sync::{Notify, broadcast, watch},
    task::{JoinHandle, JoinSet},
};

#[derive(Debug, thiserror::Error)]
#[error("Resource telemetry operation '{operation}' failed.")]
pub struct ResourceTelemetryRefreshFailed {
    pub operation: &'static str,
    pub cause: crate::resource_binary::TelemetryError,
}
struct State {
    native: Option<ResourceMonitorSnapshotEvent>,
    desktop: Option<DesktopHostTelemetrySnapshot>,
    previous: IndexMap<String, ProcessState>,
    counters: TelemetryCounters,
    latest: Value,
    last_sequence: u64,
    last_generation: u64,
}
struct Shared {
    native: NativeTelemetryClient,
    desktop: DesktopTelemetryReceiver,
    attribution: ResourceAttribution,
    state: Mutex<State>,
    changes: broadcast::Sender<Value>,
    stop: watch::Sender<bool>,
    abandoned: watch::Sender<bool>,
    #[cfg(test)]
    dropped: Arc<Notify>,
    demand: AtomicUsize,
    demand_changed: Notify,
    ready: watch::Sender<bool>,
    server_pid: u64,
    clock: Clock,
}
#[cfg(test)]
impl Drop for Shared {
    fn drop(&mut self) {
        self.dropped.notify_waiters();
    }
}
struct Inner {
    shared: Arc<Shared>,
    tasks: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.stop.send_replace(true);
        self.shared.abandoned.send_replace(true);
    }
}
#[derive(Clone)]
pub struct ResourceTelemetry(Arc<Inner>);
pub struct ResourceTelemetrySubscription {
    pub latest: Value,
    receiver: broadcast::Receiver<Value>,
    stop: watch::Receiver<bool>,
    _lease: LiveLease,
}
struct LiveLease(Arc<Inner>);
impl Drop for LiveLease {
    fn drop(&mut self) {
        self.0.shared.demand.fetch_sub(1, Ordering::SeqCst);
        self.0.shared.demand_changed.notify_one();
    }
}
impl ResourceTelemetrySubscription {
    pub async fn recv(&mut self) -> Option<Value> {
        loop {
            tokio::select! {biased;_=self.stop.wait_for(|v|*v)=>return None,result=self.receiver.recv()=>match result{Ok(value)=>return Some(value),Err(broadcast::error::RecvError::Lagged(_))=>continue,Err(_)=>return None}}
        }
    }
}
fn option<T: serde::Serialize>(value: Option<T>) -> Value {
    match value {
        Some(value) => json!({"_tag":"Some","value":value}),
        None => json!({"_tag":"None"}),
    }
}
fn health(
    native: &NativeTelemetryHealth,
    desktop: &DesktopHealth,
    snapshot: Option<&ResourceMonitorSnapshotEvent>,
) -> ResourceTelemetryHealth {
    serde_json::from_value(json!({"native":{"status":native.status,"lastSampleAt":option(native.last_sample_at),"lastError":option(native.last_error.as_deref())},"desktop":{"status":desktop.status,"lastSampleAt":option(desktop.last_sample_at),"lastError":option(desktop.last_error.as_deref())},"sidecarVersion":option(native.hello.as_ref().map(|h|&h.sidecar_version)),"sidecarPid":option(native.hello.as_ref().map(|h|h.sidecar_pid)),"restartCount":native.restart_count,"collectionDurationMicros":snapshot.map_or(0,|v|v.collection_duration_micros.0),"scannedProcessCount":snapshot.map_or(0,|v|v.scanned_process_count.0),"retainedProcessCount":snapshot.map_or(0,|v|v.retained_process_count.0),"inaccessibleProcessCount":snapshot.map_or(0,|v|v.inaccessible_process_count.0)})).expect("owner health satisfies resource schema")
}
fn unknown_power(read_at: &str) -> Value {
    json!({"source":"unknown","idle":"unknown","idleSeconds":null,"locked":"unknown","suspended":false,"onBattery":"unknown","lowPowerMode":"unknown","thermalState":"unknown","stale":true,"updatedAt":read_at})
}
fn snapshot(
    shared: &Shared,
    current: &State,
    merged: &resource_model::MergeResult,
    native_health: &NativeTelemetryHealth,
) -> Value {
    let read_at = resource_model::timestamp(merged.sampled_at_ms);
    json!({"readAt":read_at,"sampleIntervalMs":native_health.sample_interval_ms,"processes":merged.processes,"groups":merged.groups,"power":current.desktop.as_ref().map(|d|serde_json::to_value(&d.power).unwrap()).unwrap_or_else(||unknown_power(&read_at)),"speedLimitPercent":current.desktop.as_ref().map(|d|option(d.speed_limit_percent.as_ref())).unwrap_or_else(||option::<u8>(None)),"attribution":shared.attribution.snapshot((shared.clock)()),"health":health(native_health,&shared.desktop.health(),current.native.as_ref())})
}
impl Shared {
    fn rebuild(
        &self,
        native: Option<NativeSnapshot>,
        desktop: Option<DesktopHostTelemetrySnapshot>,
        update_previous: bool,
        publish_when_live: bool,
    ) -> Value {
        let mut state = self.state.lock().unwrap();
        let native_health = self.native.health();
        if let Some(incoming) = native.as_ref() {
            if incoming.generation < native_health.restart_count
                || incoming.generation < state.last_generation
                || (incoming.generation == state.last_generation
                    && incoming.snapshot.sequence.0 <= state.last_sequence)
            {
                return state.latest.clone();
            }
        }
        if let Some(incoming) = native {
            state.last_generation = incoming.generation;
            state.last_sequence = incoming.snapshot.sequence.0;
            state.native = Some(incoming.snapshot);
        }
        if let Some(desktop) = desktop {
            state.desktop = Some(desktop);
        }
        let mut roots = IndexSet::new();
        let mut starts = HashMap::new();
        if let Some(native) = state.native.as_ref() {
            if let Some(external) = native.external_processes.as_ref() {
                for root in external {
                    roots.insert(root.pid.0);
                    if let Some(start) = root.start_time_ms {
                        starts.insert(root.pid.0, start.0);
                    }
                }
            }
        }
        if let Some(desktop) = state.desktop.as_ref() {
            roots.insert(desktop.electron_pid.0);
        }
        let fallback = state
            .latest
            .get("readAt")
            .and_then(Value::as_str)
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map_or((self.clock)() as f64, |v| v.timestamp_millis() as f64);
        let merged = resource_model::merge(MergeInput {
            server_pid: self.server_pid,
            sidecar_pid: native_health.hello.as_ref().map(|v| v.sidecar_pid.0),
            fallback_sampled_at_ms: fallback,
            native_snapshot: state.native.as_ref(),
            desktop_snapshot: state.desktop.as_ref(),
            electron_root_pids: &roots,
            electron_root_start_times: &starts,
            previous: &state.previous,
            counters: &state.counters,
            update_previous,
        });
        state.latest = snapshot(self, &state, &merged, &native_health);
        state.previous = merged.previous;
        state.counters = merged.counters;
        if !publish_when_live || self.demand.load(Ordering::SeqCst) > 0 {
            let _ = self.changes.send(state.latest.clone());
        }
        state.latest.clone()
    }
    fn refresh_health(&self) {
        let mut state = self.state.lock().unwrap();
        state.latest["health"] = serde_json::to_value(health(
            &self.native.health(),
            &self.desktop.health(),
            state.native.as_ref(),
        ))
        .unwrap();
        if self.demand.load(Ordering::SeqCst) > 0 {
            let _ = self.changes.send(state.latest.clone());
        }
    }
    async fn desktop_controls(&self, snapshot: &DesktopHostTelemetrySnapshot) {
        let root = snapshot
            .electron_processes
            .iter()
            .find(|p| p.pid == snapshot.electron_pid);
        let _ = self
            .native
            .set_external_processes(vec![ResourceMonitorExternalProcess {
                pid: snapshot.electron_pid,
                start_time_ms: root.map(|p| p.creation_time_ms),
            }])
            .await;
        let _ = self
            .native
            .set_host_power_state(snapshot.power.clone())
            .await;
    }
}
impl ResourceTelemetry {
    pub async fn new(
        native: NativeTelemetryClient,
        desktop: DesktopTelemetryReceiver,
        attribution: ResourceAttribution,
        server_pid: u64,
        clock: Clock,
    ) -> Self {
        let native_health_events = native.subscribe_health_events();
        let native_health = native_health_events.latest.clone();
        let desktop_health = desktop.subscribe_health();
        let desktop_events = desktop.subscribe();
        let read_at = clock();
        let (stop, _) = watch::channel(false);
        let (abandoned, _) = watch::channel(false);
        let (changes, _) = broadcast::channel(8);
        let (ready, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            native,
            desktop,
            attribution,
            state: Mutex::new(State {
                native: None,
                desktop: desktop_events.latest.clone(),
                previous: IndexMap::new(),
                counters: TelemetryCounters::default(),
                latest: json!({"readAt":resource_model::timestamp(read_at as f64)}),
                last_sequence: 0,
                last_generation: native_health.restart_count,
            }),
            changes,
            stop,
            abandoned,
            #[cfg(test)]
            dropped: Arc::new(Notify::new()),
            demand: AtomicUsize::new(0),
            demand_changed: Notify::new(),
            ready,
            server_pid,
            clock,
        });
        // Establish all subscriptions before construction work that can yield.
        let owner = Self(Arc::new(Inner {
            shared: shared.clone(),
            tasks: tokio::sync::Mutex::new(Vec::new()),
        }));
        if let Some(initial) = desktop_events.latest.as_ref() {
            shared.desktop_controls(initial).await;
        }
        shared.rebuild(None, None, false, true);
        // Source initial snapshot records construction time, even when the
        // desktop sample used by its model predates initialization.
        shared.state.lock().unwrap().latest["readAt"] =
            json!(resource_model::timestamp(read_at as f64));
        let tasks = vec![
            tokio::spawn(desktop_loop(shared.clone(), desktop_events)),
            tokio::spawn(native_health_loop(shared.clone(), native_health_events)),
            tokio::spawn(desktop_health_loop(shared.clone(), desktop_health)),
            tokio::spawn(live_loop(shared)),
        ];
        *owner.0.tasks.lock().await = tasks;
        owner
    }
    pub fn latest(&self) -> Value {
        self.0.shared.state.lock().unwrap().latest.clone()
    }
    pub fn latest_wire(&self) -> Result<ResourceTelemetrySnapshot, serde_json::Error> {
        serde_json::from_value(self.latest())
    }
    pub async fn subscribe(&self) -> Option<ResourceTelemetrySubscription> {
        // Reserve demand and construct its cancellation guard before the first await.
        self.0.shared.demand.fetch_add(1, Ordering::SeqCst);
        let lease = LiveLease(self.0.clone());
        self.0.shared.demand_changed.notify_one();
        let mut ready = self.0.shared.ready.subscribe();
        let mut stop = self.0.shared.stop.subscribe();
        tokio::select! {biased;_=stop.wait_for(|v|*v)=>return None,result=ready.wait_for(|v|*v)=>{if result.is_err(){return None;}}}
        // Seed and receiver are paired under the same publication lock. Events
        // during acquisition are already represented by the returned snapshot.
        let (latest, receiver) = {
            let state = self.0.shared.state.lock().unwrap();
            (state.latest.clone(), self.0.shared.changes.subscribe())
        };
        Some(ResourceTelemetrySubscription {
            latest,
            receiver,
            stop,
            _lease: lease,
        })
    }
    pub async fn refresh(&self) -> Result<Value, ResourceTelemetryRefreshFailed> {
        let incoming = self.0.shared.native.sample_now().await.map_err(|cause| {
            ResourceTelemetryRefreshFailed {
                operation: "refresh",
                cause,
            }
        })?;
        Ok(self.0.shared.rebuild(Some(incoming), None, true, false))
    }
    pub async fn validate_process_identity(
        &self,
        identity: &ResourceTelemetryProcessIdentity,
    ) -> Result<bool, ResourceTelemetryRefreshFailed> {
        let snapshot = self.0.shared.native.sample_now().await.map_err(|cause| {
            ResourceTelemetryRefreshFailed {
                operation: "validateProcessIdentity",
                cause,
            }
        })?;
        Ok(snapshot
            .snapshot
            .processes
            .iter()
            .any(|p| p.pid == identity.pid && p.start_time_ms == identity.start_time_ms))
    }
    pub fn retry(&self) -> Value {
        json!({"accepted":self.0.shared.native.retry(),"snapshot":self.latest()})
    }
    pub async fn read_history(&self, input: &ResourceTelemetryHistoryInput) -> History {
        let shared = &self.0.shared;
        let read_at = (shared.clock)();
        let (window, bucket) =
            resource_history::normalize(input.window_ms.0 as f64, input.bucket_ms.0 as f64);
        let snapshots = shared
            .native
            .read_history(window as u64)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error,"Failed to read native resource telemetry history");
                Vec::new()
            });
        let native = shared.native.health();
        let state = shared.state.lock().unwrap();
        let health = health(&native, &shared.desktop.health(), state.native.as_ref());
        resource_history::build(HistoryInput {
            read_at_ms: read_at,
            window_ms: window,
            bucket_ms: bucket,
            sample_interval_ms: native.sample_interval_ms,
            server_pid: shared.server_pid,
            sidecar_pid: native.hello.as_ref().map(|v| v.sidecar_pid.0),
            desktop_snapshot: state.desktop.as_ref(),
            snapshots: &snapshots,
            health: &health,
        })
    }
    pub async fn shutdown(&self) {
        self.0.shared.stop.send_replace(true);
        let mut tasks = self.0.tasks.lock().await;
        while let Some(task) = tasks.last_mut() {
            let _ = task.await;
            tasks.pop();
        }
    }
    pub async fn closed(&self) {
        let _ = self.0.shared.stop.subscribe().wait_for(|v| *v).await;
    }
}
async fn desktop_loop(
    shared: Arc<Shared>,
    mut events: crate::desktop_telemetry::DesktopSubscription,
) {
    let mut stop = shared.stop.subscribe();
    loop {
        let event = tokio::select! {biased;_=stop.wait_for(|v|*v)=>break,event=events.recv()=>match event{Some(v)=>v,None=>break}};
        tokio::select! {biased;_=stop.wait_for(|v|*v)=>break,_=shared.desktop_controls(&event)=>{}}
        shared.rebuild(None, Some(event), false, true);
    }
}
async fn native_health_loop(
    shared: Arc<Shared>,
    mut events: crate::native_telemetry::NativeHealthSubscription,
) {
    let mut stop = shared.stop.subscribe();
    loop {
        tokio::select! {biased;_=stop.wait_for(|v|*v)=>break,event=events.recv()=>if event.is_none(){break;}}
        shared.refresh_health();
    }
}
async fn desktop_health_loop(
    shared: Arc<Shared>,
    mut events: crate::desktop_telemetry::DesktopHealthSubscription,
) {
    let mut stop = shared.stop.subscribe();
    loop {
        tokio::select! {biased;_=stop.wait_for(|v|*v)=>break,event=events.recv()=>if event.is_none(){break;}}
        shared.refresh_health();
    }
}
async fn live_loop(shared: Arc<Shared>) {
    let mut stop = shared.stop.subscribe();
    let mut live = JoinSet::new();
    let mut active = false;
    loop {
        let notified = shared.demand_changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if *stop.borrow() {
            break;
        }
        let wanted = shared.demand.load(Ordering::SeqCst) > 0;
        if wanted && !active {
            tokio::select! {biased;_=stop.wait_for(|v|*v)=>break,_=shared.desktop.set_diagnostics_demand(true)=>{}}
            let subscription = tokio::select! {biased;_=stop.wait_for(|v|*v)=>break,result=shared.native.subscribe()=>result};
            if let Ok(mut subscription) = subscription {
                let shared = shared.clone();
                live.spawn(async move {
                    while let Ok(Some(snapshot)) = subscription.recv().await {
                        shared.rebuild(Some(snapshot), None, true, false);
                    }
                });
            }
            let sample = shared.clone();
            live.spawn(async move {
                if let Ok(snapshot) = sample.native.sample_now().await {
                    sample.rebuild(Some(snapshot), None, true, false);
                }
            });
            active = true;
            shared.ready.send_replace(true);
            continue;
        }
        if !wanted && active {
            shared.ready.send_replace(false);
            live.abort_all();
            while live.join_next().await.is_some() {}
            tokio::select! {biased;_=stop.wait_for(|v|*v)=>break,_=shared.desktop.set_diagnostics_demand(false)=>{}}
            active = false;
            continue;
        }
        tokio::select! {biased;_=stop.wait_for(|v|*v)=>break,_=&mut notified=>{},result=live.join_next(),if !live.is_empty()=>{let _=result;}}
    }
    shared.ready.send_replace(false);
    live.abort_all();
    while live.join_next().await.is_some() {}
    // The service owns this final control task too. Cancellation of shutdown does
    // not lose its join. A parent stops the desktop receiver if its pipe is stuck.
    let mut abandoned = shared.abandoned.subscribe();
    tokio::select! {biased;_=abandoned.wait_for(|v|*v)=>{},_=shared.desktop.set_diagnostics_demand(false)=>{}}
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    fn native() -> (tempfile::TempDir, NativeTelemetryClient) {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("monitor.py");
        let source = include_str!("../tests/fixtures/resource-monitor.py")
            .replace("__MODE__", "normal")
            .replace(
                "__LOG__",
                &root.path().join("commands.jsonl").to_string_lossy(),
            );
        std::fs::write(&script, source).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut options =
            crate::native_telemetry::NativeTelemetryOptions::host(root.path().to_owned(), None);
        options.binary.overrides = vec![script];
        options.binary.directories.clear();
        (root, NativeTelemetryClient::new(options))
    }
    async fn healthy(native: &NativeTelemetryClient) {
        let mut health = native.subscribe_health();
        tokio::time::timeout(
            Duration::from_secs(5),
            health.wait_for(|v| v.status == ResourceTelemetrySourceStatus::Healthy),
        )
        .await
        .unwrap()
        .unwrap();
    }
    fn sample(sequence: u64, generation: u64, cpu: u64, write: u64) -> NativeSnapshot {
        NativeSnapshot{generation,snapshot:serde_json::from_value(json!({"version":3,"type":"snapshot","sequence":sequence,"sampledAtUnixMs":sequence*1000,"collectionDurationMicros":300,"scannedProcessCount":80,"retainedProcessCount":2,"inaccessibleProcessCount":1,"processes":[{"pid":100,"ppid":1,"startTimeMs":100,"runTimeMs":1000,"name":"server","command":"server","status":"Running","cpuPercent":0,"cpuTimeMs":sequence*10,"residentBytes":1024,"virtualBytes":2048,"ioReadBytes":0,"ioWriteBytes":0,"ioSemantics":"storage"},{"pid":4242,"ppid":100,"startTimeMs":200,"runTimeMs":1000,"name":"codex","command":"codex app-server","status":"Running","cpuPercent":0,"cpuTimeMs":cpu,"residentBytes":1024,"virtualBytes":2048,"ioReadBytes":0,"ioWriteBytes":write,"ioSemantics":"storage"}]})).unwrap()}
    }
    async fn service(
        native: NativeTelemetryClient,
        desktop: DesktopTelemetryReceiver,
    ) -> ResourceTelemetry {
        ResourceTelemetry::new(
            native,
            desktop,
            ResourceAttribution::default(),
            100,
            Arc::new(|| 5000),
        )
        .await
    }
    #[tokio::test]
    async fn initial_desktop_identity_power_and_updates_do_not_advance_native_counters() {
        let (directory, native) = native();
        healthy(&native).await;
        let (input, mut writer) = tokio::io::duplex(4096);
        let mut options = crate::desktop_telemetry::DesktopTelemetryOptions::unavailable("desktop");
        options.input = Some((3, Box::pin(input)));
        let desktop = DesktopTelemetryReceiver::new(options).await;
        let mut events = desktop.subscribe();
        let mut payload = json!({"version":1,"type":"desktopTelemetry","sequence":1,"sampledAtUnixMs":1000,"electronPid":5000,"power":{"source":"electron-main","idle":"false","idleSeconds":2,"locked":"false","suspended":false,"onBattery":"true","lowPowerMode":"unknown","thermalState":"fair","stale":false,"updatedAt":"1970-01-01T00:00:01.000Z"},"speedLimitPercent":90,"electronProcesses":[{"pid":5000,"creationTimeMs":300,"type":"Browser","name":"electron","cpuPercent":2,"cumulativeCpuSeconds":0.02,"idleWakeupsPerSecond":3,"workingSetBytes":4096,"peakWorkingSetBytes":8192}]});
        writer
            .write_all(format!("{payload}\n").as_bytes())
            .await
            .unwrap();
        events.recv().await.unwrap();
        let telemetry = service(native.clone(), desktop.clone()).await;
        assert_eq!(telemetry.latest()["readAt"], "1970-01-01T00:00:05.000Z");
        assert_eq!(
            telemetry.latest()["speedLimitPercent"],
            json!({"_tag":"Some","value":90})
        );
        native.process_table().await.unwrap();
        let commands = std::fs::read_to_string(directory.path().join("commands.jsonl")).unwrap();
        assert!(
            commands
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|v| v["type"] == "setExternalProcesses"
                    && v["processes"] == json!([{"pid":5000,"startTimeMs":300}]))
        );
        let mut snapshot = sample(1, 1, 100, 1000);
        let mut native_value = serde_json::to_value(&snapshot.snapshot).unwrap();
        native_value["externalProcesses"] = json!([{"pid":5000,"startTimeMs":300}]);
        let mut root = native_value["processes"][0].clone();
        root["pid"] = json!(5000);
        root["startTimeMs"] = json!(300);
        root["name"] = json!("electron");
        native_value["processes"].as_array_mut().unwrap().push(root);
        snapshot.snapshot = serde_json::from_value(native_value).unwrap();
        let value = telemetry
            .0
            .shared
            .rebuild(Some(snapshot), None, true, false);
        assert!(
            value["processes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["identity"]["pid"] == 5000 && p["category"] == "electron-main")
        );
        let before =
            serde_json::to_value(&telemetry.0.shared.state.lock().unwrap().counters).unwrap();
        let mut subscription = telemetry.subscribe().await.unwrap();
        payload["sequence"] = json!(2);
        payload["sampledAtUnixMs"] = json!(2000);
        payload["power"]["onBattery"] = json!("false");
        payload["electronProcesses"][0]["cumulativeCpuSeconds"] = json!(0.04);
        writer
            .write_all(format!("{payload}\n").as_bytes())
            .await
            .unwrap();
        let updated = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let next = subscription.recv().await.unwrap();
                if next["power"]["onBattery"] == "false" {
                    return next;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(updated["groups"]["backend"], value["groups"]["backend"]);
        assert_eq!(
            serde_json::to_value(&telemetry.0.shared.state.lock().unwrap().counters).unwrap(),
            before
        );
        drop(subscription);
        telemetry.shutdown().await;
        desktop.shutdown().await;
        native.shutdown().await;
    }
    #[tokio::test]
    async fn source_generation_identity_counters_attribution_and_history_are_preserved() {
        let (_directory, native) = native();
        healthy(&native).await;
        let desktop = DesktopTelemetryReceiver::new(
            crate::desktop_telemetry::DesktopTelemetryOptions::unavailable("web"),
        )
        .await;
        let telemetry = service(native.clone(), desktop.clone()).await;
        telemetry
            .0
            .shared
            .attribution
            .record(crate::resource_attribution::AttributionRecord {
                component: "sqlite".into(),
                operation: "write".into(),
                logical_write_bytes: Some(99.6),
                ..Default::default()
            });
        let first = telemetry
            .0
            .shared
            .rebuild(Some(sample(1, 1, 100, 1000)), None, true, false);
        let rejected =
            telemetry
                .0
                .shared
                .rebuild(Some(sample(100, 0, 500, 9000)), None, true, false);
        assert_eq!(first, rejected);
        let next = telemetry
            .0
            .shared
            .rebuild(Some(sample(2, 1, 200, 2000)), None, true, false);
        assert_eq!(next["groups"]["backend"]["ioWriteBytes"], json!(1000.));
        assert_eq!(next["groups"]["backend"]["cpuTimeMs"], json!(110.));
        assert_eq!(
            next["attribution"]["entries"][0]["logicalWriteBytes"],
            json!(100.)
        );
        let duplicate =
            telemetry
                .0
                .shared
                .rebuild(Some(sample(2, 1, 900, 9000)), None, true, false);
        assert_eq!(next, duplicate);
        telemetry.latest_wire().unwrap();
        let history = telemetry
            .read_history(
                &serde_json::from_value(json!({"windowMs":5000,"bucketMs":1000})).unwrap(),
            )
            .await;
        history.wire().unwrap();
        assert_eq!(history.buckets.len(), 5);
        assert!(
            !telemetry
                .validate_process_identity(
                    &serde_json::from_value(json!({"pid":100,"startTimeMs":100})).unwrap()
                )
                .await
                .unwrap()
        );
        telemetry.shutdown().await;
        desktop.shutdown().await;
        native.shutdown().await;
    }
    #[tokio::test]
    async fn retained_subscriptions_pair_snapshot_and_changes_and_release_last_demand() {
        let (_directory, native) = native();
        healthy(&native).await;
        let (control, reader) = tokio::io::duplex(4096);
        let mut frames = BufReader::new(reader).lines();
        let mut options = crate::desktop_telemetry::DesktopTelemetryOptions::unavailable("desktop");
        options.control = Some((4, Box::pin(control)));
        let desktop = DesktopTelemetryReceiver::new(options).await;
        assert_eq!(
            serde_json::from_str::<Value>(&frames.next_line().await.unwrap().unwrap()).unwrap()["type"],
            "setHostPowerIntervals"
        );
        let telemetry = service(native.clone(), desktop.clone()).await;
        let mut first = telemetry.subscribe().await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&frames.next_line().await.unwrap().unwrap()).unwrap()["enabled"],
            true
        );
        let second = telemetry.subscribe().await.unwrap();
        assert_eq!(telemetry.0.shared.demand.load(Ordering::SeqCst), 2);
        drop(second);
        assert_eq!(telemetry.0.shared.demand.load(Ordering::SeqCst), 1);
        let event = telemetry
            .0
            .shared
            .rebuild(Some(sample(7, 2, 400, 4000)), None, true, false);
        loop {
            if first.recv().await.unwrap() == event {
                break;
            }
        }
        drop(first);
        assert_eq!(
            serde_json::from_str::<Value>(
                &tokio::time::timeout(Duration::from_secs(5), frames.next_line())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
            )
            .unwrap()["enabled"],
            false
        );
        telemetry.shutdown().await;
        desktop.shutdown().await;
        native.shutdown().await;
    }
    struct HeldControl {
        writes: usize,
        entered: Arc<Notify>,
    }
    impl tokio::io::AsyncWrite for HeldControl {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.writes > 0 {
                self.entered.notify_one();
                return std::task::Poll::Pending;
            }
            self.writes += 1;
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    #[tokio::test]
    async fn dropping_last_service_owner_cancels_stalled_final_control_and_all_workers() {
        let (_directory, native) = native();
        healthy(&native).await;
        let entered = Arc::new(Notify::new());
        let mut options = crate::desktop_telemetry::DesktopTelemetryOptions::unavailable("desktop");
        options.control = Some((
            4,
            Box::pin(HeldControl {
                writes: 0,
                entered: entered.clone(),
            }),
        ));
        let desktop = DesktopTelemetryReceiver::new(options).await;
        let telemetry = service(native.clone(), desktop.clone()).await;
        let task = tokio::spawn({
            let telemetry = telemetry.clone();
            async move { telemetry.subscribe().await }
        });
        entered.notified().await;
        task.abort();
        assert!(matches!(task.await,Err(error)if error.is_cancelled()));
        let dropped = telemetry.0.shared.dropped.clone();
        let finished = dropped.notified();
        tokio::pin!(finished);
        finished.as_mut().enable();
        drop(telemetry);
        tokio::time::timeout(Duration::from_secs(5), finished)
            .await
            .unwrap();
        desktop.shutdown().await;
        native.shutdown().await;
    }
    #[tokio::test]
    async fn cancelled_acquisition_releases_reservation_and_stalled_control_shutdown_is_owned() {
        let (_directory, native) = native();
        healthy(&native).await;
        let entered = Arc::new(Notify::new());
        let mut options = crate::desktop_telemetry::DesktopTelemetryOptions::unavailable("desktop");
        options.control = Some((
            4,
            Box::pin(HeldControl {
                writes: 0,
                entered: entered.clone(),
            }),
        ));
        let desktop = DesktopTelemetryReceiver::new(options).await;
        let telemetry = service(native.clone(), desktop.clone()).await;
        let subscription = tokio::spawn({
            let telemetry = telemetry.clone();
            async move { telemetry.subscribe().await }
        });
        entered.notified().await;
        assert_eq!(telemetry.0.shared.demand.load(Ordering::SeqCst), 1);
        subscription.abort();
        assert!(matches!(subscription.await,Err(error)if error.is_cancelled()));
        assert_eq!(telemetry.0.shared.demand.load(Ordering::SeqCst), 0);
        // A parent must close the receiver alongside aggregation; it owns the pipe
        // and unblocks the final demand-disable frame even when the host won't read.
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(telemetry.shutdown(), desktop.shutdown());
            native.shutdown().await;
        })
        .await
        .unwrap();
    }
}
