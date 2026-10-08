//! Source background activity leases, host power state, and live work policy.
use crate::{
    background_settings,
    desktop_telemetry::{Clock, DesktopTelemetryReceiver},
    server_settings::{SettingsError, SettingsService},
};
use indexmap::IndexMap;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::sync::{Arc, Mutex};
use t3_contracts::*;
use tokio::{
    sync::{broadcast, watch},
    task::JoinHandle,
};

pub fn scope_key(scope: &BackgroundScope) -> String {
    match scope {
        BackgroundScope::ServerConfig => "server-config".into(),
        BackgroundScope::ProviderStatus { instance_id } => instance_id.as_ref().map_or_else(
            || "provider-status".into(),
            |id| format!("provider-status:{id}"),
        ),
        BackgroundScope::VcsStatus { cwd } => format!("vcs-status:{cwd}"),
        BackgroundScope::GitRefs { cwd } => format!("git-refs:{cwd}"),
        BackgroundScope::Diagnostics => "diagnostics".into(),
        BackgroundScope::Thread { thread_id } => format!("thread:{thread_id}"),
    }
}
pub fn active(lease: &ClientActivityLease, now: UtcDateTime) -> bool {
    lease.expires_at > now
}
fn foreground(lease: &ClientActivityLease, now: UtcDateTime) -> bool {
    active(lease, now) && lease.visible && (lease.focused || lease.recently_interacted)
}
fn client_constrained(lease: &ClientActivityLease, settings: &Value) -> bool {
    (settings["pauseWhenClientLowPower"] == true
        && lease.low_power_mode == Some(BackgroundBooleanState::True))
        || (settings["pauseWhenOnBattery"] == true
            && lease.battery_state == Some(ClientBatteryState::Unplugged))
}
pub fn host_constrained(host: &HostPowerSnapshot, settings: &Value) -> bool {
    !host.stale
        && (host.suspended
            || (settings["pauseWhenHostLocked"] == true
                && host.locked == BackgroundBooleanState::True)
            || matches!(
                host.thermal_state,
                HostPowerThermalState::Serious | HostPowerThermalState::Critical
            )
            || (settings["pauseWhenHostLowPower"] == true
                && host.low_power_mode == BackgroundBooleanState::True)
            || (settings["pauseWhenOnBattery"] == true
                && host.on_battery == BackgroundBooleanState::True))
}
pub fn lease_may_run(
    lease: &ClientActivityLease,
    scope: &BackgroundScope,
    now: UtcDateTime,
    settings: &Value,
) -> bool {
    active(lease, now)
        && lease
            .scopes
            .iter()
            .any(|candidate| scope_key(candidate) == scope_key(scope))
        && !client_constrained(lease, settings)
        && (settings["profile"] == "performance" || foreground(lease, now))
}
pub fn compute_snapshot(
    host: &HostPowerSnapshot,
    leases: &IndexMap<String, ClientActivityLease>,
    now: UtcDateTime,
    settings: &Value,
) -> BackgroundPolicySnapshot {
    let leases: Vec<_> = leases
        .values()
        .filter(|lease| active(lease, now))
        .cloned()
        .collect();
    let foreground: Vec<_> = leases
        .iter()
        .filter(|lease| foreground(lease, now))
        .collect();
    let mut keys: Vec<_> = leases
        .iter()
        .flat_map(|lease| lease.scopes.iter().map(scope_key))
        .collect();
    // Source Array.toSorted uses UTF-16 code-unit order.
    keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    keys.dedup();
    BackgroundPolicySnapshot {
        host_power: host.clone(),
        active_foreground_lease_count: foreground.len().into(),
        should_run_opportunistic_work: foreground
            .iter()
            .any(|lease| !client_constrained(lease, settings))
            && !host_constrained(host, settings),
        leases,
        active_scope_keys: keys,
        updated_at: now,
    }
}
pub fn upsert(
    leases: &mut IndexMap<String, ClientActivityLease>,
    lease: ClientActivityLease,
    now: UtcDateTime,
) {
    leases.retain(|_, lease| active(lease, now));
    let key = serde_json::to_string(&(
        lease.session_id.as_str(),
        lease.rpc_client_id.0,
        lease.client_id.0.as_str(),
    ))
    .unwrap();
    if !leases.contains_key(&key) {
        let mut count = 0;
        let mut oldest: Option<(String, UtcDateTime)> = None;
        for (key, current) in leases.iter() {
            if current.session_id == lease.session_id
                && current.rpc_client_id == lease.rpc_client_id
            {
                count += 1;
                if oldest
                    .as_ref()
                    .is_none_or(|(_, time)| current.updated_at < *time)
                {
                    oldest = Some((key.clone(), current.updated_at));
                }
            }
        }
        if count >= 16 {
            if let Some((key, _)) = oldest {
                leases.shift_remove(&key);
            }
        }
    }
    leases.insert(key, lease);
}
fn same_power(left: &HostPowerSnapshot, right: &HostPowerSnapshot) -> bool {
    left.source == right.source
        && left.idle == right.idle
        && left.locked == right.locked
        && left.suspended == right.suspended
        && left.on_battery == right.on_battery
        && left.low_power_mode == right.low_power_mode
        && left.thermal_state == right.thermal_state
        && left.stale == right.stale
}
fn unknown_power(now: UtcDateTime) -> HostPowerSnapshot {
    HostPowerSnapshot {
        source: HostPowerSource::Unknown,
        idle: BackgroundBooleanState::Unknown,
        idle_seconds: None,
        locked: BackgroundBooleanState::Unknown,
        suspended: false,
        on_battery: BackgroundBooleanState::Unknown,
        low_power_mode: BackgroundBooleanState::Unknown,
        thermal_state: HostPowerThermalState::Unknown,
        stale: true,
        updated_at: now,
    }
}
fn datetime(ms: i64) -> UtcDateTime {
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("clock returned an invalid date")
        .into()
}
struct State {
    host: HostPowerSnapshot,
    leases: IndexMap<String, ClientActivityLease>,
}
struct Shared {
    state: Mutex<State>,
    settings: SettingsService,
    clock: Clock,
    changes: broadcast::Sender<BackgroundPolicySnapshot>,
    power_changes: broadcast::Sender<()>,
    stop: watch::Sender<bool>,
    publish: tokio::sync::Mutex<()>,
}
impl Shared {
    fn report_power(&self, power: HostPowerSnapshot) {
        let mut state = self.state.lock().unwrap();
        if power.updated_at < state.host.updated_at {
            return;
        }
        let changed = !same_power(&state.host, &power);
        state.host = power;
        if changed {
            let _ = self.power_changes.send(());
        }
    }
    async fn resolved(&self) -> Value {
        match self.settings.snapshot().await {
            Ok(settings) => {
                background_settings::resolve_server(&serde_json::to_value(settings).unwrap())
            }
            Err(_) => background_settings::preset("balanced"),
        }
    }
    async fn snapshot(&self) -> BackgroundPolicySnapshot {
        let settings = self.resolved().await;
        let now = datetime((self.clock)());
        let state = self.state.lock().unwrap();
        compute_snapshot(&state.host, &state.leases, now, &settings)
    }
    async fn publish(&self, expire: bool) {
        let _permit = self.publish.lock().await;
        let settings = self.resolved().await;
        let now = datetime((self.clock)());
        let mut state = self.state.lock().unwrap();
        if expire {
            state.leases.retain(|_, lease| active(lease, now));
        }
        let _ = self
            .changes
            .send(compute_snapshot(&state.host, &state.leases, now, &settings));
    }
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
pub struct BackgroundPolicy(Arc<Inner>);
pub struct BackgroundSubscription {
    pub latest: BackgroundPolicySnapshot,
    changes: broadcast::Receiver<BackgroundPolicySnapshot>,
    stop: watch::Receiver<bool>,
}
impl BackgroundSubscription {
    pub async fn recv(&mut self) -> Option<BackgroundPolicySnapshot> {
        loop {
            tokio::select! {biased;_=self.stop.wait_for(|stop|*stop)=>return None,next=self.changes.recv()=>match next{Ok(next)=>return Some(next),Err(broadcast::error::RecvError::Lagged(_))=>continue,Err(_)=>return None}}
        }
    }
}
impl BackgroundPolicy {
    pub async fn start(
        settings: SettingsService,
        desktop: DesktopTelemetryReceiver,
        clock: Clock,
    ) -> Result<Self, SettingsError> {
        let mut settings_changes = settings.subscribe().await?;
        let mut desktop_changes = desktop.subscribe();
        let initial = desktop_changes
            .latest
            .as_ref()
            .map(|snapshot| snapshot.power.clone())
            .unwrap_or_else(|| unknown_power(datetime(clock())));
        let (changes, _) = broadcast::channel(1);
        let (power_changes, mut power) = broadcast::channel(1);
        let (stop, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                host: initial,
                leases: IndexMap::new(),
            }),
            settings,
            clock,
            changes,
            power_changes,
            stop,
            publish: tokio::sync::Mutex::new(()),
        });
        let feed = shared.clone();
        let mut stopped = shared.stop.subscribe();
        let desktop_task = tokio::spawn(async move {
            loop {
                tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>break,next=desktop_changes.recv()=>match next {Some(snapshot)=>feed.report_power(snapshot.power),None=>break}}
            }
        });
        let feed = shared.clone();
        let mut stopped = shared.stop.subscribe();
        let policy_task = tokio::spawn(async move {
            let mut settings_active = true;
            let mut expire = tokio::time::interval_at(
                tokio::time::Instant::now() + std::time::Duration::from_secs(15),
                std::time::Duration::from_secs(15),
            );
            expire.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let prune = tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>break,next=settings_changes.recv(),if settings_active=>{if next.is_none(){settings_active=false;continue;}false},next=power.recv()=>{if matches!(next,Err(broadcast::error::RecvError::Closed)){break;}false},_=expire.tick()=>true};
                tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>break,_=feed.publish(prune)=>{}}
            }
        });
        Ok(Self(Arc::new(Inner {
            shared,
            tasks: tokio::sync::Mutex::new(vec![desktop_task, policy_task]),
        })))
    }
    pub async fn snapshot(&self) -> BackgroundPolicySnapshot {
        self.0.shared.snapshot().await
    }
    pub async fn subscribe(&self) -> BackgroundSubscription {
        let _permit = self.0.shared.publish.lock().await;
        let changes = self.0.shared.changes.subscribe();
        let latest = self.snapshot().await;
        BackgroundSubscription {
            latest,
            changes,
            stop: self.0.shared.stop.subscribe(),
        }
    }
    pub fn report_host_power_state(&self, power: HostPowerSnapshot) {
        self.0.shared.report_power(power);
    }
    pub async fn report_client_activity(
        &self,
        session_id: AuthSessionId,
        rpc_client_id: RpcClientId,
        input: ClientActivityReportInput,
    ) {
        let shared = &self.0.shared;
        let _permit = shared.publish.lock().await;
        let settings = shared.resolved().await;
        let now = datetime((shared.clock)());
        let ttl = input
            .ttl_ms
            .as_ref()
            .map_or(45000., |number| number.as_f64().unwrap())
            .clamp(1000., 120000.);
        let lease = ClientActivityLease {
            session_id,
            rpc_client_id,
            client_id: input.client_id,
            client_kind: input.client_kind,
            visible: input.visible,
            focused: input.focused,
            recently_interacted: input.recently_interacted,
            app_state: input.app_state,
            low_power_mode: input.low_power_mode,
            battery_state: input.battery_state,
            network_type: input.network_type,
            scopes: input.scopes,
            updated_at: now,
            expires_at: datetime(((now.timestamp_millis() as f64) + ttl) as i64),
        };
        let mut state = shared.state.lock().unwrap();
        upsert(&mut state.leases, lease, now);
        let _ = shared
            .changes
            .send(compute_snapshot(&state.host, &state.leases, now, &settings));
    }
    pub async fn remove_rpc_client(&self, session: &AuthSessionId, rpc: &RpcClientId) {
        let shared = &self.0.shared;
        let _permit = shared.publish.lock().await;
        let settings = shared.resolved().await;
        let now = datetime((shared.clock)());
        let mut state = shared.state.lock().unwrap();
        state
            .leases
            .retain(|_, lease| lease.session_id != *session || lease.rpc_client_id != *rpc);
        let _ = shared
            .changes
            .send(compute_snapshot(&state.host, &state.leases, now, &settings));
    }
    pub async fn has_demand(&self, scope: &BackgroundScope) -> bool {
        self.snapshot()
            .await
            .active_scope_keys
            .contains(&scope_key(scope))
    }
    pub async fn should_run_scope_work(&self, scope: &BackgroundScope) -> bool {
        let snapshot = self.snapshot().await;
        let settings = self.0.shared.resolved().await;
        !host_constrained(&snapshot.host_power, &settings)
            && snapshot
                .leases
                .iter()
                .any(|lease| lease_may_run(lease, scope, snapshot.updated_at, &settings))
    }
    pub async fn should_run_opportunistic_work(&self) -> bool {
        self.snapshot().await.should_run_opportunistic_work
    }
    pub async fn closed(&self) {
        let _ = self.0.shared.stop.subscribe().wait_for(|stop| *stop).await;
    }
    pub async fn shutdown(&self) {
        self.0.shared.stop.send_replace(true);
        let mut tasks = self.0.tasks.lock().await;
        while !tasks.is_empty() {
            let _ = (&mut tasks[0]).await;
            tasks.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        desktop_telemetry::DesktopTelemetryOptions, server_secret_store::ServerSecretStore,
        server_settings::SettingsOptions,
    };
    use std::sync::atomic::{AtomicI64, Ordering};
    const NOW: i64 = 1791417600000;
    fn power(at: i64) -> HostPowerSnapshot {
        serde_json::from_value(json!({"source":"electron-main","idle":"false","idleSeconds":0,"locked":"false","suspended":false,"onBattery":"false","lowPowerMode":"false","thermalState":"nominal","stale":false,"updatedAt":datetime(at)})).unwrap()
    }
    fn report() -> ClientActivityReportInput {
        serde_json::from_value(json!({"clientId":"client","clientKind":"web","visible":true,"focused":true,"recentlyInteracted":false,"scopes":[{"type":"diagnostics"}],"observedAt":datetime(NOW-100000)})).unwrap()
    }
    async fn services() -> (
        tempfile::TempDir,
        SettingsService,
        DesktopTelemetryReceiver,
        BackgroundPolicy,
        Arc<AtomicI64>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let secrets = ServerSecretStore::open(temp.path().join("secrets")).unwrap();
        let mut options = SettingsOptions::file(temp.path().join("settings.json"), secrets);
        options.watch = false;
        let settings = SettingsService::start(options).await.unwrap();
        let time = Arc::new(AtomicI64::new(NOW));
        let clock: Clock = {
            let time = time.clone();
            Arc::new(move || time.load(Ordering::Acquire))
        };
        let mut options = DesktopTelemetryOptions::unavailable("web");
        options.clock = clock.clone();
        let desktop = DesktopTelemetryReceiver::new(options).await;
        let policy = BackgroundPolicy::start(settings.clone(), desktop.clone(), clock)
            .await
            .unwrap();
        (temp, settings, desktop, policy, time)
    }
    #[test]
    fn original_policy_decisions_and_lease_eviction() {
        for (index, line) in include_str!("../tests/fixtures/background-policy.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let rows: Vec<ClientActivityLease> =
                serde_json::from_value(row["leases"].clone()).unwrap();
            let mut leases: IndexMap<_, _> = rows
                .into_iter()
                .enumerate()
                .map(|(index, lease)| (index.to_string(), lease))
                .collect();
            let now: UtcDateTime = serde_json::from_value(row["now"].clone()).unwrap();
            match row["op"].as_str().unwrap() {
                "compute" => {
                    let host: HostPowerSnapshot =
                        serde_json::from_value(row["power"].clone()).unwrap();
                    let output = compute_snapshot(&host, &leases, now, &row["settings"]);
                    assert_eq!(
                        serde_json::to_value(output).unwrap(),
                        row["output"],
                        "policy witness {index}"
                    );
                    let scope: BackgroundScope =
                        serde_json::from_value(row["scope"].clone()).unwrap();
                    assert_eq!(
                        !host_constrained(&host, &row["settings"])
                            && leases.values().any(|lease| lease_may_run(
                                lease,
                                &scope,
                                now,
                                &row["settings"]
                            )),
                        row["run"].as_bool().unwrap(),
                        "work witness {index}"
                    );
                }
                "upsert" => {
                    // Source uses the full trusted session/connection/client tuple.
                    leases = leases
                        .into_values()
                        .map(|lease| {
                            (
                                serde_json::to_string(&(
                                    lease.session_id.as_str(),
                                    lease.rpc_client_id.0,
                                    lease.client_id.0.as_str(),
                                ))
                                .unwrap(),
                                lease,
                            )
                        })
                        .collect();
                    upsert(
                        &mut leases,
                        serde_json::from_value(row["incoming"].clone()).unwrap(),
                        now,
                    );
                    assert_eq!(
                        serde_json::to_value(leases.into_values().collect::<Vec<_>>()).unwrap(),
                        row["output"],
                        "eviction witness {index}"
                    );
                }
                _ => unreachable!(),
            }
        }
    }
    #[tokio::test]
    async fn power_latest_ignores_old_reports_and_idle_seconds_only_does_not_publish() {
        let (_temp, settings, desktop, policy, _) = services().await;
        let mut observed = policy.subscribe().await;
        let mut initial = power(NOW);
        initial.idle_seconds = Some(4.into());
        policy.report_host_power_state(initial.clone());
        let actual = observed.recv().await.unwrap();
        assert_eq!(actual.host_power, initial);
        let mut newer = initial.clone();
        newer.updated_at = datetime(NOW + 10);
        newer.idle_seconds = Some(8.into());
        policy.report_host_power_state(newer.clone());
        assert_eq!(policy.snapshot().await.host_power, newer);
        assert!(matches!(
            observed.changes.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        let mut older = initial;
        older.thermal_state = HostPowerThermalState::Critical;
        policy.report_host_power_state(older);
        assert_eq!(policy.snapshot().await.host_power, newer);
        newer.thermal_state = HostPowerThermalState::Critical;
        policy.report_host_power_state(newer.clone());
        assert_eq!(observed.recv().await.unwrap().host_power, newer);
        policy.shutdown().await;
        assert!(observed.recv().await.is_none());
        desktop.shutdown().await;
        settings.shutdown().await;
    }
    #[tokio::test(start_paused = true)]
    async fn live_settings_expiry_and_connection_removal_preserve_sibling_leases() {
        let (_temp, settings, desktop, policy, time) = services().await;
        let session = AuthSessionId::new("session").unwrap();
        let mut input = report();
        input.focused = false;
        input.visible = false;
        input.ttl_ms = Some(500.into());
        policy
            .report_client_activity(session.clone(), NonNegativeInt(0), input)
            .await;
        policy
            .report_client_activity(session.clone(), NonNegativeInt(1), report())
            .await;
        let mut observed = policy.subscribe().await;
        assert_eq!(observed.latest.leases.len(), 2);
        assert_eq!(
            observed.latest.leases[0].expires_at.timestamp_millis(),
            NOW + 1000
        );
        assert_eq!(observed.latest.leases[0].updated_at.timestamp_millis(), NOW);
        policy.remove_rpc_client(&session, &NonNegativeInt(1)).await;
        let changed = observed.recv().await.unwrap();
        assert_eq!(changed.leases.len(), 1);
        assert!(
            !policy
                .should_run_scope_work(&BackgroundScope::Diagnostics)
                .await
        );
        settings
            .update(
                serde_json::from_value(json!({"backgroundActivity":{"profile":"performance"}}))
                    .unwrap(),
            )
            .await
            .unwrap();
        observed.recv().await.unwrap();
        assert!(
            policy
                .should_run_scope_work(&BackgroundScope::Diagnostics)
                .await
        );
        time.store(NOW + 1000, Ordering::Release);
        assert!(!policy.has_demand(&BackgroundScope::Diagnostics).await);
        tokio::time::advance(std::time::Duration::from_secs(15)).await;
        assert!(observed.recv().await.unwrap().leases.is_empty());
        assert!(policy.0.shared.state.lock().unwrap().leases.is_empty());
        policy.shutdown().await;
        desktop.shutdown().await;
        settings.shutdown().await;
    }
    #[tokio::test]
    async fn desktop_power_seed_changes_and_owned_feed_teardown_use_actual_pipe() {
        use tokio::io::AsyncWriteExt;
        let temp = tempfile::tempdir().unwrap();
        let secrets = ServerSecretStore::open(temp.path().join("secrets")).unwrap();
        let mut options = SettingsOptions::file(temp.path().join("settings.json"), secrets);
        options.watch = false;
        let settings = SettingsService::start(options).await.unwrap();
        let (input, mut sender) = tokio::io::duplex(4096);
        let mut options = DesktopTelemetryOptions::unavailable("desktop");
        options.input = Some((3, Box::pin(input)));
        options.clock = Arc::new(|| NOW);
        let desktop = DesktopTelemetryReceiver::new(options).await;
        let mut received = desktop.subscribe();
        async fn emit(
            sender: &mut tokio::io::DuplexStream,
            sequence: u64,
            power: &HostPowerSnapshot,
        ) {
            let frame = json!({"version":1,"type":"desktopTelemetry","sequence":sequence,"sampledAtUnixMs":power.updated_at.timestamp_millis(),"electronPid":100,"power":power,"speedLimitPercent":null,"electronProcesses":[]});
            sender
                .write_all((frame.to_string() + "\n").as_bytes())
                .await
                .unwrap();
        }
        let initial = power(NOW);
        emit(&mut sender, 1, &initial).await;
        received.recv().await.unwrap();
        let policy = BackgroundPolicy::start(settings.clone(), desktop.clone(), Arc::new(|| NOW))
            .await
            .unwrap();
        let mut observed = policy.subscribe().await;
        assert_eq!(observed.latest.host_power, initial);
        let mut changed = power(NOW + 1);
        changed.on_battery = BackgroundBooleanState::True;
        emit(&mut sender, 2, &changed).await;
        assert_eq!(observed.recv().await.unwrap().host_power, changed);
        received.recv().await.unwrap();
        policy.shutdown().await;
        let mut stopped = changed.clone();
        stopped.updated_at = datetime(NOW + 2);
        stopped.locked = BackgroundBooleanState::True;
        emit(&mut sender, 3, &stopped).await;
        received.recv().await.unwrap();
        assert_eq!(
            policy.snapshot().await.host_power,
            changed,
            "policy shutdown joined its owned desktop feed"
        );
        desktop.shutdown().await;
        settings.shutdown().await;
    }
    #[tokio::test]
    async fn cancelled_shutdown_retry_does_not_repoll_completed_handles() {
        let (_temp, settings, desktop, policy, _) = services().await;
        policy.shutdown().await;
        let (complete, completed) = tokio::sync::oneshot::channel();
        let first = tokio::spawn(async move {
            let _ = complete.send(());
        });
        completed.await.unwrap();
        assert!(first.is_finished());
        let (release, held) = tokio::sync::oneshot::channel();
        let second = tokio::spawn(async move {
            let _ = held.await;
        });
        *policy.0.tasks.lock().await = vec![first, second];
        let mut stopping = Box::pin(policy.shutdown());
        assert!(futures_util::poll!(&mut stopping).is_pending());
        drop(stopping);
        assert_eq!(policy.0.tasks.lock().await.len(), 1);
        release.send(()).unwrap();
        policy.shutdown().await;
        assert!(policy.0.tasks.lock().await.is_empty());
        desktop.shutdown().await;
        settings.shutdown().await;
    }
}
