//! Quota from places this environment cannot run turns on, today a CLIProxyAPI hub pooling several subscription accounts
//! (`UsageLimitSources.ts`).
//!
//! Each enabled `settings.usageLimitSources` entry is polled on the provider health-check interval and on every change of the
//! entries, then published as one snapshot per source over `subscribeServerConfig`. A source that fails keeps its row with
//! `error` set, so the user can see it is configured but unreachable. Nothing is persisted: like provider status, this is live
//! state that re-derives at start.
//!
//! The service is attached to the store (`install`/`current`), as the other services the transport reaches are.
use crate::{
    background_policy::BackgroundPolicy,
    background_settings,
    cliproxy_api::{Clock, CliproxyApi, Hub, HubConfig, SourceError},
    persistence::Store,
    provider_registry::ProviderChanges,
    server_settings::SettingsService,
};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use t3_contracts::BackgroundScope;
use tokio::{sync::watch, task::JoinHandle};

/// `DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL`, and what a zero or negative interval waits instead.
const FALLBACK_INTERVAL: Duration = Duration::from_secs(60);
/// `Effect.forEach(entries, ..., { concurrency: 4 })`
const CONCURRENCY: usize = 4;

/// What a subscriber sees: the snapshots now, shared between the readers of one publish.
pub type Snapshots = Arc<Vec<Value>>;

/// How the poll waits out its interval: the runtime's timer in production, a tick the test gives in a test.
pub type Sleeper = Arc<dyn Fn(Duration) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;

struct Inner {
    hub: Arc<dyn Hub>,
    settings: Option<SettingsService>,
    background: Option<BackgroundPolicy>,
    clock: Clock,
    state: watch::Sender<Snapshots>,
    /// One refresh at a time: a slow hub read started before a settings change must not publish after the change's own
    /// refresh and resurrect a removed source. Callers queue behind the one in flight and see the current settings. A
    /// redemption shares it, so a stale read in flight cannot overwrite one.
    refresh: tokio::sync::Mutex<()>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner()).drain(..) {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct UsageLimitSources(Arc<Inner>);

/// The current snapshots followed by every change.
pub struct SourceChanges {
    changes: watch::Receiver<Snapshots>,
    first: bool,
}
impl SourceChanges {
    /// The set at subscription first, then each distinct set published since; `None` once the service is gone.
    pub async fn next(&mut self) -> Option<Snapshots> {
        if std::mem::take(&mut self.first) {
            return Some(self.changes.borrow_and_update().clone());
        }
        self.changes.changed().await.ok()?;
        Some(self.changes.borrow_and_update().clone())
    }
}

/// `sourceLabel`: the entry's own label, else the hub's host, else the entry's id.
fn source_label(id: &str, config: &Value) -> String {
    if let Some(label) = config["label"].as_str().filter(|label| !label.is_empty()) {
        return label.to_owned();
    }
    url::Url::parse(config["url"].as_str().unwrap_or_default())
        .ok()
        .and_then(|url| url.host_str().map(|host| url.port().map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"))))
        .unwrap_or_else(|| id.to_owned())
}

fn hub_config(config: &Value) -> HubConfig {
    HubConfig { url: config["url"].as_str().unwrap_or_default().to_owned(), management_key: config["managementKey"].as_str().unwrap_or_default().to_owned() }
}

impl UsageLimitSources {
    /// Reads the sources once and keeps them current: on the provider health interval while the background policy allows
    /// provider-status work, and on each change of the entries.
    pub async fn start(settings: Option<SettingsService>, background: Option<BackgroundPolicy>, hub: Arc<dyn Hub>, clock: Clock) -> Self {
        Self::start_with(settings, background, hub, clock, Arc::new(|wait| Box::pin(tokio::time::sleep(wait)))).await
    }

    /// As `start`, waiting out the interval by `sleep`.
    pub async fn start_with(settings: Option<SettingsService>, background: Option<BackgroundPolicy>, hub: Arc<dyn Hub>, clock: Clock, sleep: Sleeper) -> Self {
        let (state, _) = watch::channel(Snapshots::default());
        let service = Self(Arc::new(Inner { hub, settings, background, clock, state, refresh: Default::default(), tasks: Default::default() }));
        // Settings edits re-read straight away, so a new hub shows up without waiting for the interval and a removed one leaves the list.
        let subscription = match &service.0.settings {
            Some(settings) => settings.subscribe().await.ok(),
            None => None,
        };
        let mut tasks = vec![];
        if let Some(mut subscription) = subscription {
            let weak = Arc::downgrade(&service.0);
            let mut sources = subscription_sources(&serde_json::to_value(&subscription.snapshot).unwrap_or(Value::Null));
            tasks.push(tokio::spawn(async move {
                while let Some(next) = subscription.recv().await {
                    let next = subscription_sources(&serde_json::to_value(&next).unwrap_or(Value::Null));
                    if next == sources {
                        continue;
                    }
                    sources = next;
                    let Some(inner) = weak.upgrade() else { return };
                    Self(inner).refresh().await;
                }
            }));
        }
        let weak = Arc::downgrade(&service.0);
        let inner_sleep = sleep;
        tasks.push(tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else { return };
                let wait = Self(inner.clone()).interval().await;
                drop(inner);
                (inner_sleep)(wait).await;
                let Some(inner) = weak.upgrade() else { return };
                let service = Self(inner);
                if service.provider_status_allowed().await {
                    service.refresh().await;
                }
            }
        }));
        let weak = Arc::downgrade(&service.0);
        tasks.push(tokio::spawn(async move {
            if let Some(inner) = weak.upgrade() {
                Self(inner).refresh().await;
            }
        }));
        service.0.tasks.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).extend(tasks);
        service
    }

    /// `current`
    pub fn current(&self) -> Snapshots {
        self.0.state.borrow().clone()
    }

    /// `streamChanges`: the current set followed by every change, repeats dropped.
    pub fn subscribe(&self) -> SourceChanges {
        SourceChanges { changes: self.0.state.subscribe(), first: true }
    }

    /// How long to wait before the next poll: the provider health-check interval.
    async fn interval(&self) -> Duration {
        let settings = match &self.0.settings {
            Some(settings) => settings.snapshot().await.ok().and_then(|settings| serde_json::to_value(settings).ok()),
            None => None,
        };
        let milliseconds = settings.map_or(Some(60_000.0), |settings| background_settings::resolve_server(&settings)["providerHealthRefreshInterval"].as_f64());
        match milliseconds {
            Some(milliseconds) if milliseconds > 0.0 => Duration::from_millis(milliseconds as u64),
            _ => FALLBACK_INTERVAL,
        }
    }

    async fn provider_status_allowed(&self) -> bool {
        match &self.0.background {
            Some(policy) => policy.should_run_scope_work(&BackgroundScope::ProviderStatus { instance_id: None }).await,
            None => true,
        }
    }

    async fn settings(&self) -> Option<Value> {
        let settings = self.0.settings.as_ref()?;
        settings.snapshot().await.ok().and_then(|settings| serde_json::to_value(settings).ok())
    }

    /// `readSource`
    async fn read_source(&self, id: &str, config: &Value) -> Value {
        let checked_at = crate::execution::at(chrono::DateTime::from_timestamp_millis((self.0.clock)()).unwrap_or_default());
        let mut snapshot = json!({"id":id,"kind":config["kind"],"label":source_label(id, config),"checkedAt":checked_at});
        if config["managementKey"].as_str().is_none_or(str::is_empty) {
            snapshot["accounts"] = json!([]);
            snapshot["error"] = json!("No management key configured.");
            return snapshot;
        }
        match self.0.hub.read_accounts(&hub_config(config), &self.0.clock).await {
            Ok(accounts) => snapshot["accounts"] = json!(accounts),
            Err(error) => {
                tracing::debug!(source = id, detail = %error.0, "usage limit source read failed");
                snapshot["accounts"] = json!([]);
                snapshot["error"] = json!(error.0);
            }
        }
        snapshot
    }

    fn publish(&self, next: Vec<Value>) {
        self.0.state.send_if_modified(|current| {
            if **current == next {
                return false;
            }
            *current = Arc::new(next);
            true
        });
    }

    /// `refresh`: re-reads every enabled source now. Never fails; a failure lands on the snapshot.
    pub async fn refresh(&self) {
        let _one_at_a_time = self.0.refresh.lock().await;
        let settings = self.settings().await;
        let entries: Vec<(String, Value)> = settings
            .as_ref()
            .and_then(|settings| settings["usageLimitSources"].as_object())
            .into_iter()
            .flatten()
            .filter(|(_, config)| config["enabled"] == true)
            .map(|(id, config)| (id.clone(), config.clone()))
            .collect();
        let snapshots = stream::iter(entries).map(|(id, config)| async move { self.read_source(&id, &config).await }).buffered(CONCURRENCY).collect::<Vec<_>>().await;
        self.publish(snapshots);
    }

    /// `consumeResetCredit`: spends the credit through the hub, then re-reads that source.
    pub async fn consume_reset_credit(&self, source_id: &str, account_id: &str, credit_id: &str) -> Result<Value, SourceError> {
        let _one_at_a_time = self.0.refresh.lock().await;
        let Some(settings) = self.settings().await else {
            return Err(SourceError("Could not read hub settings.".into()));
        };
        let config = &settings["usageLimitSources"][source_id];
        if config["enabled"] != true || config["managementKey"].as_str().is_none_or(str::is_empty) {
            return Err(SourceError("The usage limit source is missing or disabled.".into()));
        }
        let result = self.0.hub.consume(&hub_config(config), account_id, credit_id, &self.0.clock).await?;
        let snapshot = self.read_source(source_id, config).await;
        let previous = self.current();
        self.publish(previous.iter().map(|source| if source["id"] == source_id { snapshot.clone() } else { source.clone() }).collect());
        Ok(result)
    }
}

/// The entries a change of settings is compared by.
fn subscription_sources(settings: &Value) -> Value {
    settings["usageLimitSources"].clone()
}

/// The production service: the hub over HTTP, the wall clock.
pub async fn start_production(settings: Option<SettingsService>, background: Option<BackgroundPolicy>) -> UsageLimitSources {
    UsageLimitSources::start(settings, background, Arc::new(CliproxyApi::default()), Arc::new(|| chrono::Utc::now().timestamp_millis())).await
}

#[derive(Default)]
struct Slot(std::sync::OnceLock<UsageLimitSources>);

/// Installs the server's service; a server that never installed one has no sources.
pub fn install(store: &Store, sources: UsageLimitSources) {
    let _ = store.extension(Slot::default).0.set(sources);
}

pub fn current(store: &Store) -> Option<UsageLimitSources> {
    store.extension(Slot::default).0.get().cloned()
}

/// The `usageLimitSourcesUpdated` event of `subscribeServerConfig`.
pub fn event(sources: &[Value]) -> Value {
    json!({"version":1,"type":"usageLimitSourcesUpdated","payload":{"sources":sources}})
}

// ---- the command `/usage-limits` -------------------------------------------------------------

const COMMAND_NAME: &str = "usage-limits";

/// `hasProviderUsageLimits` for the slash command: a provider whose driver a source reports on is offered it, and a source that
/// failed to read (no accounts, an error) counts for every driver, so its error is not hidden until the next success.
fn offers_command(driver: &str, providers: &[Value], sources: &[Value]) -> bool {
    let reports = |provider: &Value| {
        provider["enabled"] == true && provider["installed"] == true && provider["driver"] == driver && provider.get("usageLimits").is_some_and(|limits| !limits.is_null()) && provider["availability"] != "unavailable"
    };
    providers.iter().any(reports)
        || sources.iter().any(|source| {
            source["accounts"].as_array().is_some_and(|accounts| accounts.iter().any(|account| account["driver"] == driver)) || (!source["error"].is_null() && source["accounts"].as_array().is_none_or(|accounts| accounts.is_empty()))
        })
}

/// `withUsageLimitsCommands`: `/usage-limits` on the providers that have limits to show, on the workspace catalogs too, which
/// replace the global command list.
pub fn with_usage_limits_commands(providers: &[Value], sources: &[Value]) -> Vec<Value> {
    let command = json!({"name":COMMAND_NAME,"description":"Show this provider's usage limits"});
    let with_command = |commands: &Value| {
        let mut commands: Vec<Value> = commands.as_array().into_iter().flatten().filter(|existing| existing["name"] != COMMAND_NAME).cloned().collect();
        commands.push(command.clone());
        json!(commands)
    };
    providers
        .iter()
        .map(|provider| {
            if !provider["driver"].as_str().is_some_and(|driver| offers_command(driver, providers, sources)) {
                return provider.clone();
            }
            let mut provider = provider.clone();
            provider["slashCommands"] = with_command(&provider["slashCommands"]);
            if let Some(snapshots) = provider.get_mut("workspaceSnapshots").and_then(Value::as_array_mut) {
                for snapshot in snapshots {
                    snapshot["slashCommands"] = with_command(&snapshot["slashCommands"]);
                }
            }
            provider
        })
        .collect()
}

/// `sameUsageLimitCommandCoverage`: two sets with the same drivers need no catalog republish, however much their quotas moved.
pub fn same_command_coverage(previous: &[Value], next: &[Value]) -> bool {
    let coverage = |sources: &[Value]| -> std::collections::BTreeSet<String> {
        sources
            .iter()
            .flat_map(|source| {
                let accounts = source["accounts"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                if !source["error"].is_null() && accounts.is_empty() {
                    vec!["*".to_owned()]
                } else {
                    accounts.iter().map(|account| account["driver"].as_str().unwrap_or_default().to_owned()).collect()
                }
            })
            .collect()
    };
    coverage(previous) == coverage(next)
}

/// The provider catalogs a subscriber that asked for the command sees: the registry's newest list paired with the sources' newest
/// set (`zipLatestWith`), the sources counting only when the drivers they cover change, so a quota that moved republishes nothing.
pub fn command_catalogs(changes: ProviderChanges, latest: Vec<Value>, sources: &UsageLimitSources) -> impl futures_util::Stream<Item = Arc<Vec<Value>>> + Send + 'static + use<> {
    struct Pair {
        changes: ProviderChanges,
        latest: Vec<Value>,
        usage: SourceChanges,
        known: Snapshots,
    }
    let pair = Pair { changes, latest, usage: sources.subscribe(), known: sources.current() };
    stream::unfold(pair, |mut pair| async move {
        loop {
            tokio::select! {
                next = pair.changes.recv() => {
                    pair.latest = next?.as_ref().clone();
                    return Some((Arc::new(with_usage_limits_commands(&pair.latest, pair.known.as_slice())), pair));
                }
                next = pair.usage.next() => {
                    let next = next?;
                    if same_command_coverage(pair.known.as_slice(), next.as_slice()) {
                        continue;
                    }
                    pair.known = next;
                    return Some((Arc::new(with_usage_limits_commands(&pair.latest, pair.known.as_slice())), pair));
                }
            }
        }
    })
}
