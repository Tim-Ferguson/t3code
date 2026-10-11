//! Instance routing and source-compatible legacy configuration hydration.
use crate::{
    acp_runtime::AcpInstance,
    claude_instance::ClaudeInstance,
    grok_instance::GrokInstance,
    antigravity_instance::AntigravityInstance,
    opencode_instance::OpenCodeInstance,
    codex::{CodexConfig, CodexInstance},
    pi_instance::PiInstance,
    provider_process::ProcessError,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, RwLock, Weak},
};
use t3_contracts::{
    ProviderInstanceConfig, ProviderInstanceConfigMap, ProviderInstanceId, ServerSettings,
};

pub fn derive_instance_configs(settings: &ServerSettings) -> ProviderInstanceConfigMap {
    let mut instances = settings.provider_instances.clone();
    let legacy =
        serde_json::to_value(&settings.providers).expect("typed legacy settings serialize");
    for (driver, config) in legacy.as_object().unwrap() {
        let id: ProviderInstanceId = driver.parse().unwrap();
        instances.entry(id).or_insert_with(|| {
            serde_json::from_value(json!({"driver":driver,"config":config}))
                .expect("legacy settings envelope")
        });
    }
    instances
}
enum RegisteredInstance {
    Claude(ClaudeInstance),
    Grok(GrokInstance),
    Antigravity(AntigravityInstance),
    OpenCode(OpenCodeInstance),
    Codex(CodexInstance),
    Acp(AcpInstance),
    Pi(PiInstance),
}
struct RegistryState {
    claude: HashMap<String, ClaudeInstance>,
    grok: HashMap<String, GrokInstance>,
    antigravity: HashMap<String, AntigravityInstance>,
    opencode: HashMap<String, OpenCodeInstance>,
    codex: HashMap<String, CodexInstance>,
    acp: HashMap<String, AcpInstance>,
    pi: HashMap<String, PiInstance>,
    snapshots: Vec<Value>,
    settings: ServerSettings,
    /// Where the instances were last probed; a refresh probes from here again.
    cwd: std::path::PathBuf,
}
#[derive(Default)]
struct ProviderListeners {
    next: u64,
    senders: HashMap<u64, tokio::sync::mpsc::UnboundedSender<Arc<Vec<Value>>>>,
}
/// Changes only, as in the source registry's PubSub.unbounded stream. Dropping
/// the subscription removes its queue rather than retaining a stale client.
pub struct ProviderChanges {
    receiver: tokio::sync::mpsc::UnboundedReceiver<Arc<Vec<Value>>>,
    listeners: Weak<Mutex<ProviderListeners>>,
    id: u64,
}
impl ProviderChanges {
    pub async fn recv(&mut self) -> Option<Arc<Vec<Value>>> {
        self.receiver.recv().await
    }
}
impl Drop for ProviderChanges {
    fn drop(&mut self) {
        if let Some(listeners) = self.listeners.upgrade() {
            listeners.lock().unwrap().senders.remove(&self.id);
        }
    }
}
/// Whether a live process of this driver may hold work a settled turn started, which Stop must end before it
/// settles what the thread shows: Codex's terminals, Claude's background shells, Grok's process group.
pub(crate) fn holds_background_work(driver: &str) -> bool {
    matches!(driver, "codex" | "claudeAgent" | "grok")
}
#[derive(Clone)]
pub struct ProviderRegistry {
    state: Arc<RwLock<RegistryState>>,
    catalog: Option<crate::acp_registry_support::Catalog>,
    refresh: Arc<tokio::sync::Mutex<()>>,
    changes: Arc<Mutex<ProviderListeners>>,
    coordinator: crate::acp_coordinator::Coordinator,
    observer: Option<Arc<crate::acp_coordinator::LiveObserver>>,
    auth: Arc<RwLock<Option<crate::provider_auth_service::WeakProviderAuthService>>>,
    health: crate::acp_health_jobs::Refreshes,
    mcp: Arc<RwLock<Option<crate::provider_mcp::ProviderMcpSessions>>>,
    text_generation: Arc<RwLock<crate::text_generation_run::Options>>,
    management: crate::provider_manage::Management,
    idle: crate::provider_idle::IdleRelease,
    finalize: crate::deferred_finalize::FinishDebounce,
    attach: crate::deferred_finalize::AttachGate,
    /// The refresh of every instance now running; callers arriving meanwhile share its result.
    inflight: Arc<Mutex<Option<futures_util::future::Shared<futures_util::future::BoxFuture<'static, Result<Vec<Value>, String>>>>>>,
}
impl ProviderRegistry {
    /// The clock and windows on which an idle provider process is released.
    pub fn idle_release(&self) -> &crate::provider_idle::IdleRelease {
        &self.idle
    }
    /// The windows a held turn waits out before it settles, on the idle release's clock.
    pub fn finish_debounce(&self) -> &crate::deferred_finalize::FinishDebounce {
        &self.finalize
    }
    /// When a continuation run attaches to the traffic buffered for it; open unless a test closes it.
    pub fn attach_gate(&self) -> &crate::deferred_finalize::AttachGate {
        &self.attach
    }
    pub fn set_mcp_sessions(&self, sessions: crate::provider_mcp::ProviderMcpSessions) {
        *self.mcp.write().unwrap() = Some(sessions);
    }
    pub(crate) fn reserve_mcp(
        &self,
        store: &crate::persistence::Store,
        thread: &str,
        instance: &str,
    ) -> Result<Option<crate::provider_mcp::CredentialLease>, String> {
        let Some(sessions) = self.mcp.read().unwrap().clone() else {
            return Ok(None);
        };
        let view = store
            .projection("thread", thread)
            .map_err(|error| error.to_string())?
            .ok_or("Thread not found.")?;
        let project_id: t3_contracts::ProjectId = view["thread"]["projectId"]
            .as_str()
            .ok_or("Thread project missing.")?
            .parse()
            .map_err(|error: t3_contracts::ValidationError| error.to_string())?;
        let project_exists = store
            .projection("project", project_id.as_str())
            .map_err(|error| error.to_string())?
            .is_some();
        let settings = self.settings();
        let overrides = settings.project_settings_overrides.get(&project_id);
        let browser = if !project_exists
            && settings
                .project_settings_overrides
                .values()
                .any(|entry| entry.enable_agent_browser_access.is_some())
        {
            false
        } else {
            overrides
                .and_then(|entry| entry.enable_agent_browser_access)
                .unwrap_or(settings.enable_agent_browser_access)
        };
        let device = if !project_exists
            && settings
                .project_settings_overrides
                .values()
                .any(|entry| entry.enable_agent_device_access.is_some())
        {
            false
        } else {
            overrides
                .and_then(|entry| entry.enable_agent_device_access)
                .unwrap_or(settings.enable_agent_device_access)
        };
        sessions
            .reserve(
                thread
                    .parse()
                    .map_err(|error: t3_contracts::ValidationError| error.to_string())?,
                instance
                    .parse()
                    .map_err(|error: t3_contracts::ValidationError| error.to_string())?,
                browser,
                device,
            )
            .map(Some)
    }
    pub async fn discover(settings: &ServerSettings, cwd: &Path) -> Result<Self, ProcessError> {
        Self::discover_with_catalog(settings, cwd, None).await
    }
    pub async fn discover_with_catalog(
        settings: &ServerSettings,
        cwd: &Path,
        catalog: Option<crate::acp_registry_support::Catalog>,
    ) -> Result<Self, ProcessError> {
        let management = crate::provider_manage::Management::new(catalog.as_ref().map(|catalog| catalog.tools_dir().to_path_buf()));
        Self::discover_with_management(settings, cwd, catalog, management).await
    }
    /// With provider management already configured, so its settings apply to the first probe too.
    pub async fn discover_with_management(
        settings: &ServerSettings,
        cwd: &Path,
        catalog: Option<crate::acp_registry_support::Catalog>,
        management: crate::provider_manage::Management,
    ) -> Result<Self, ProcessError> {
        let mut registry = Self::discover_with_context(
            settings,
            cwd,
            catalog,
            crate::acp_coordinator::Coordinator::default(),
            management,
        )
        .await?;
        let state = Arc::downgrade(&registry.state);
        let changes = Arc::downgrade(&registry.changes);
        registry.observer = Some(Arc::new(registry.coordinator.observe_live_state(Arc::new(
            move |instance, update| {
                let (Some(state), Some(changes)) = (state.upgrade(), changes.upgrade()) else {
                    return;
                };
                let mut state = state.write().unwrap();
                let custom = state
                    .acp
                    .get(instance)
                    .map(|instance| instance.config.custom_models.clone())
                    .unwrap_or_default();
                if let Some(snapshot) = state
                    .snapshots
                    .iter_mut()
                    .find(|snapshot| snapshot["instanceId"] == instance)
                {
                    apply_live_update(snapshot, update, &custom);
                    let published = Arc::new(state.snapshots.clone());
                    changes
                        .lock()
                        .unwrap()
                        .senders
                        .retain(|_, sender| sender.send(published.clone()).is_ok());
                }
            },
        ))));
        Ok(registry)
    }
    async fn discover_with_context(
        settings: &ServerSettings,
        cwd: &Path,
        catalog: Option<crate::acp_registry_support::Catalog>,
        coordinator: crate::acp_coordinator::Coordinator,
        management: crate::provider_manage::Management,
    ) -> Result<Self, ProcessError> {
        let entries = derive_instance_configs(settings);
        let mut results =
            futures_util::stream::iter(entries)
                .map(|(id, entry)| discover_entry(id, entry, catalog.clone(), coordinator.clone(), management.clone(), cwd, false))
                .buffer_unordered(4)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;
        // Keep deterministic instance ordering even when probes complete out of order.
        // Presentation order is the source `orderProviderSnapshots`: built-in driver rank
        // (Codex, then Claude), driver, display name, instance id.
        let rank = |driver: &str| ["codex", "claudeAgent", "cursor", "grok", "opencode", "antigravity"].iter().position(|name| *name == driver).unwrap_or(6);
        let key = |snapshot: &Value| (rank(snapshot["driver"].as_str().unwrap_or("")), snapshot["driver"].as_str().unwrap_or("").to_owned(), snapshot["displayName"].as_str().unwrap_or("").to_owned(), snapshot["instanceId"].as_str().unwrap_or("").to_owned());
        results.sort_by_key(|result| key(&result.2));
        let mut codex = HashMap::new();
        let mut claude = HashMap::new();
        let mut grok = HashMap::new();
        let mut antigravity = HashMap::new();
        let mut opencode = HashMap::new();
        let mut acp = HashMap::new();
        let mut pi = HashMap::new();
        let mut snapshots = vec![];
        for (id, instance, snapshot) in results {
            if let Some(instance) = instance {
                match instance {
                    RegisteredInstance::Claude(instance) => {
                        claude.insert(id, instance);
                    }
                    RegisteredInstance::Grok(instance) => {
                        grok.insert(id, instance);
                    }
                    RegisteredInstance::Antigravity(instance) => {
                        antigravity.insert(id, instance);
                    }
                    RegisteredInstance::OpenCode(instance) => {
                        opencode.insert(id, instance);
                    }
                    RegisteredInstance::Codex(instance) => {
                        codex.insert(id, instance);
                    }
                    RegisteredInstance::Acp(instance) => {
                        acp.insert(id, instance);
                    }
                    RegisteredInstance::Pi(instance) => {
                        pi.insert(id, instance);
                    }
                }
            }
            let mut snapshot = snapshot;
            let id = snapshot["instanceId"].as_str().unwrap_or("").to_owned();
            if let Some(instance) = acp.get(&id) {
                apply_coordinator_state(
                    &mut snapshot,
                    &coordinator,
                    &id,
                    &instance.config.custom_models,
                );
            }
            snapshots.push(snapshot);
        }
        Ok(Self {
            state: Arc::new(RwLock::new(RegistryState {
                claude,
                grok,
                antigravity,
                opencode,
                codex,
                acp,
                pi,
                snapshots,
                settings: settings.clone(),
                cwd: cwd.to_path_buf(),
            })),
            catalog,
            refresh: Arc::new(tokio::sync::Mutex::new(())),
            changes: Arc::new(Mutex::new(ProviderListeners::default())),
            coordinator,
            observer: None,
            auth: Arc::new(RwLock::new(None)),
            health: Default::default(),
            mcp: Default::default(),
            text_generation: Default::default(),
            management,
            idle: Default::default(),
            finalize: Default::default(),
            attach: Default::default(),
            inflight: Default::default(),
        })
    }
    /// Existing registry clones observe a complete replacement after discovery.
    /// Actors already executing keep their captured provider and runtime policy.
    pub async fn reconfigure(
        &self,
        settings: &ServerSettings,
        cwd: &Path,
    ) -> Result<Vec<Value>, ProcessError> {
        let _permit = self.refresh.lock().await;
        let auth = self.authentication_service();
        let _auth_hold = match auth {
            Some(auth) => Some(auth.hold_reconfiguration(settings).await),
            None => None,
        };
        self.health
            .stop_all_retaining(
                false,
                _auth_hold.as_ref().map(|hold| hold._admission.clone()),
            )
            .await;
        let replacement = Self::discover_with_context(
            settings,
            cwd,
            self.catalog.clone(),
            self.coordinator.clone(),
            self.management.clone(),
        )
        .await?;
        let mut state = Arc::try_unwrap(replacement.state)
            .map_err(|_| {
                ProcessError::Protocol("Provider discovery state unexpectedly shared.".into())
            })?
            .into_inner()
            .unwrap();
        let mut current = self.state.write().unwrap();
        for snapshot in &mut state.snapshots {
            let id = snapshot["instanceId"].as_str().unwrap_or("").to_owned();
            if let Some(instance) = state.acp.get(&id) {
                apply_coordinator_state(
                    snapshot,
                    &self.coordinator,
                    &id,
                    &instance.config.custom_models,
                );
            }
            self.management.apply_update_state(snapshot);
        }
        self.management.retain_update_states(&state.snapshots.iter().filter_map(|snapshot| snapshot["instanceId"].as_str().map(str::to_owned)).collect());
        let snapshots = state.snapshots.clone();
        *current = state;
        let published = Arc::new(snapshots.clone());
        self.changes
            .lock()
            .unwrap()
            .senders
            .retain(|_, sender| sender.send(published.clone()).is_ok());
        Ok(snapshots)
    }
    /// Explicit sign-in invalidates only this instance's disposable health
    /// enrichment. It never enters registry/controller reconfiguration.
    pub(crate) async fn authentication_changed(
        &self,
        instance: &str,
        expected: &ProviderInstanceConfig,
        cwd: &Path,
        confirmation: crate::acp_authentication_state::AuthenticationState,
        authenticated: bool,
    ) {
        let Some((entry, _captured)) = self.acp_auth_entry(instance) else {
            return;
        };
        if serde_json::to_value(entry).unwrap() != serde_json::to_value(expected).unwrap() {
            return;
        }
        let Some(mut lease) = self.health.admit(instance).await else {
            return;
        };
        let state = Arc::downgrade(&self.state);
        let changes = Arc::downgrade(&self.changes);
        let expected = serde_json::to_value(expected).unwrap();
        let instance = instance.to_owned();
        let cwd = cwd.to_owned();
        let (published, readiness) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            lease.await_previous().await;
            if lease.stopped() {
                return;
            }
            let captured = {
                let Some(state) = state.upgrade() else { return };
                let current = state.read().unwrap();
                let entries = derive_instance_configs(&current.settings);
                if !entries
                    .get(&instance.parse().expect("validated instance"))
                    .is_some_and(|entry| serde_json::to_value(entry).unwrap() == expected)
                {
                    return;
                }
                let Some(captured) = current.acp.get(&instance).cloned() else {
                    return;
                };
                captured
            };
            // A superseded probe is fully disposed before the confirmation and
            // refreshed readiness are applied to the next generation.
            confirmation.set(authenticated).await;
            if !authenticated {
                captured.coordinator.clear_configuration(&instance);
                captured.coordinator.clear_commands(&instance);
            }
            let mut cancelled = lease.cancellation();
            let snapshot = tokio::select! {biased;_ = cancelled.wait_for(|stop|*stop)=>return,result=captured.readiness()=>result};
            let Ok(snapshot) = snapshot else { return };
            let installed = snapshot["installed"] == true;
            let publish = |mut snapshot: Value| {
                lease.publish(|| {
                    let (Some(state), Some(changes)) = (state.upgrade(), changes.upgrade()) else {
                        return;
                    };
                    let mut current = state.write().unwrap();
                    let matches = derive_instance_configs(&current.settings)
                        .get(&instance.parse().expect("validated provider instance"))
                        .is_some_and(|entry| serde_json::to_value(entry).unwrap() == expected);
                    if !matches {
                        return;
                    }
                    if snapshot["auth"]["status"] == "unknown"
                        && snapshot["enabled"] == true
                        && snapshot["installed"] == true
                        && confirmation.get()
                    {
                        snapshot["auth"]["status"] = json!("authenticated");
                    }
                    apply_coordinator_state(
                        &mut snapshot,
                        &captured.coordinator,
                        &instance,
                        &captured.config.custom_models,
                    );
                    if let Some(existing) = current
                        .snapshots
                        .iter_mut()
                        .find(|snapshot| snapshot["instanceId"] == instance)
                    {
                        *existing = snapshot;
                    }
                    let published = Arc::new(current.snapshots.clone());
                    changes
                        .lock()
                        .unwrap()
                        .senders
                        .retain(|_, sender| sender.send(published.clone()).is_ok());
                });
            };
            publish(snapshot);
            let _ = published.send(());
            if !installed || lease.stopped() {
                return;
            }
            let (cancel, cancellation) = tokio::sync::oneshot::channel();
            let cancellation_bridge = tokio::spawn(async move {
                let _ = cancelled.wait_for(|stop| *stop).await;
                let _ = cancel.send(());
            });
            let enriched = captured.discover_enrichment(&cwd, cancellation).await;
            cancellation_bridge.abort();
            if let Ok(Some(snapshot)) = enriched {
                if !lease.stopped() {
                    if snapshot["auth"]["status"] == "unauthenticated" {
                        confirmation.set(false).await;
                    }
                    publish(snapshot);
                }
            }
        });
        // Source onChanged awaits readiness refresh, while enrichment belongs
        // to the provider's scope and continues independently afterward.
        let _ = readiness.await;
    }
    #[cfg(test)]
    pub(crate) fn observe_health_refreshes(&self) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        self.health.observe_admissions()
    }
    #[cfg(test)]
    pub(crate) async fn wait_health_refresh(&self, instance: &str) {
        self.health.wait(instance).await;
    }
    pub(crate) async fn shutdown_health(&self) {
        self.health.stop_all(true).await;
    }
    pub fn subscribe_changes(&self) -> ProviderChanges {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut listeners = self.changes.lock().unwrap();
        let id = listeners.next;
        listeners.next += 1;
        listeners.senders.insert(id, sender);
        ProviderChanges {
            receiver,
            listeners: Arc::downgrade(&self.changes),
            id,
        }
    }
    /// Registration and initial snapshot share the state lock with publication,
    /// closing the gap between a config snapshot and its provider-change stream.
    pub fn snapshot_and_subscribe(&self) -> (Vec<Value>, ProviderChanges) {
        let state = self.state.read().unwrap();
        let changes = self.subscribe_changes();
        (state.snapshots.clone(), changes)
    }
    pub fn coordinator(&self) -> crate::acp_coordinator::Coordinator {
        self.coordinator.clone()
    }
    pub fn catalog(&self) -> Option<crate::acp_registry_support::Catalog> {
        self.catalog.clone()
    }
    pub fn snapshots(&self) -> Vec<Value> {
        self.state.read().unwrap().snapshots.clone()
    }
    pub(crate) fn settings(&self) -> ServerSettings {
        self.state.read().unwrap().settings.clone()
    }
    pub fn driver(&self, instance_id: &str) -> Result<&'static str, ProcessError> {
        state_driver(&self.state.read().unwrap(), instance_id)
    }
    /// `ProviderContinuationIdentity.continuationKey`: instances of one driver with the same key keep one
    /// native conversation, as two accounts that share a Claude home do. Without a key of its own an
    /// instance's is its identity, so it continues with nobody else.
    pub fn continuation_key(&self, instance_id: &str) -> Option<String> {
        let state = self.state.read().unwrap();
        let driver = state_driver(&state, instance_id).ok()?;
        let snapshot = state.snapshots.iter().find(|snapshot| snapshot["instanceId"] == instance_id)?;
        Some(snapshot["continuation"]["groupKey"].as_str().map_or_else(|| format!("{driver}:instance:{instance_id}"), str::to_owned))
    }
    /// Whether a thread whose native conversation belongs to `from` continues it under `to`
    /// (`ProviderSessionTransitionPolicy`: `restart_and_resume` for a changed instance): the same
    /// driver, the same continuation key, and a target that is enabled.
    pub fn continues_across(&self, from: &str, to: &str) -> bool {
        let state = self.state.read().unwrap();
        let (Ok(a), Ok(b)) = (state_driver(&state, from), state_driver(&state, to)) else { return false };
        drop(state);
        a == b && self.continuation_key(from).is_some_and(|key| Some(key) == self.continuation_key(to)) && self.snapshots().iter().any(|snapshot| snapshot["instanceId"] == to && snapshot["enabled"] == true)
    }
    /// Credential setup is available before readiness and while disabled. Capture
    /// configuration and its factory instance under the same registry read lock.
    pub(crate) fn acp_auth_entry(
        &self,
        instance_id: &str,
    ) -> Option<(ProviderInstanceConfig, AcpInstance)> {
        let state = self.state.read().unwrap();
        let entry = derive_instance_configs(&state.settings)
            .into_iter()
            .find(|(id, _)| id.as_str() == instance_id)?
            .1;
        let instance = state.acp.get(instance_id)?.clone();
        Some((entry, instance))
    }
    pub(crate) fn attach_auth(
        &self,
        service: crate::provider_auth_service::WeakProviderAuthService,
    ) {
        *self.auth.write().unwrap() = Some(service);
    }
    pub(crate) fn authentication_service(
        &self,
    ) -> Option<crate::provider_auth_service::ProviderAuthService> {
        self.auth
            .read()
            .unwrap()
            .as_ref()
            .and_then(|service| service.upgrade())
    }
    pub fn acp(&self, instance_id: &str) -> Result<AcpInstance, ProcessError> {
        let state = self.state.read().unwrap();
        // A Grok session starts through the generic ACP instance of its executable.
        if state_driver(&state, instance_id)? == "grok" {
            return state.grok.get(instance_id).map(GrokInstance::acp_instance).ok_or_else(|| ProcessError::Protocol("Grok provider instance is unavailable.".into()));
        }
        // So does an Antigravity session, through the generic ACP instance of its resolved executable.
        if state_driver(&state, instance_id)? == "antigravity" {
            return state.antigravity.get(instance_id).map(AntigravityInstance::acp_instance).ok_or_else(|| ProcessError::Protocol("Antigravity provider instance is unavailable.".into()));
        }
        if state_driver(&state, instance_id)? != "acpRegistry" {
            return Err(ProcessError::Protocol(
                "Provider instance is not ACP Registry.".into(),
            ));
        }
        state
            .acp
            .get(instance_id)
            .cloned()
            .ok_or_else(|| ProcessError::Protocol("ACP provider instance is unavailable.".into()))
    }
    pub fn grok(&self, instance_id: &str) -> Result<GrokInstance, ProcessError> {
        let state = self.state.read().unwrap();
        if state_driver(&state, instance_id)? != "grok" {
            return Err(ProcessError::Protocol("Provider instance is not Grok.".into()));
        }
        state.grok.get(instance_id).cloned().ok_or_else(|| ProcessError::Protocol("Grok provider instance is unavailable.".into()))
    }
    pub fn antigravity(&self, instance_id: &str) -> Result<AntigravityInstance, ProcessError> {
        let state = self.state.read().unwrap();
        if state_driver(&state, instance_id)? != "antigravity" {
            return Err(ProcessError::Protocol("Provider instance is not Antigravity.".into()));
        }
        state.antigravity.get(instance_id).cloned().ok_or_else(|| ProcessError::Protocol("Antigravity provider instance is unavailable.".into()))
    }
    pub fn opencode(&self, instance_id: &str) -> Result<OpenCodeInstance, ProcessError> {
        let state = self.state.read().unwrap();
        if state_driver(&state, instance_id)? != "opencode" {
            return Err(ProcessError::Protocol("Provider instance is not OpenCode.".into()));
        }
        state.opencode.get(instance_id).cloned().ok_or_else(|| ProcessError::Protocol("OpenCode provider instance is unavailable.".into()))
    }
    /// The Antigravity instance behind a sign-in, ready or not: credential setup is available before readiness.
    pub(crate) fn antigravity_auth_entry(&self, instance_id: &str) -> Option<(ProviderInstanceConfig, AntigravityInstance)> {
        let state = self.state.read().unwrap();
        let entry = derive_instance_configs(&state.settings).into_iter().find(|(id, _)| id.as_str() == instance_id)?.1;
        Some((entry, state.antigravity.get(instance_id)?.clone()))
    }
    /// What a session learned (`onSessionStarted`, `onAvailableCommands`, `onSignedOut`) is folded into the instance's
    /// snapshot and published; an instance that is gone, or that is not Antigravity, is left alone.
    pub fn antigravity_update(&self, instance_id: &str, change: impl FnOnce(&AntigravityInstance, &mut Value, &str)) {
        let mut state = self.state.write().unwrap();
        let Some(instance) = state.antigravity.get(instance_id).cloned() else { return };
        let Some(snapshot) = state.snapshots.iter_mut().find(|snapshot| snapshot["instanceId"] == instance_id) else { return };
        let before = snapshot.clone();
        change(&instance, snapshot, &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        if *snapshot == before {
            return;
        }
        let snapshots = state.snapshots.clone();
        drop(state);
        self.publish(snapshots);
    }
    pub fn claude(&self, instance_id: &str) -> Result<ClaudeInstance, ProcessError> {
        let state = self.state.read().unwrap();
        if state_driver(&state, instance_id)? != "claudeAgent" {
            return Err(ProcessError::Protocol("Provider instance is not Claude.".into()));
        }
        state.claude.get(instance_id).cloned().ok_or_else(|| ProcessError::Protocol("Claude provider instance is unavailable.".into()))
    }
    pub fn pi(&self, instance_id: &str) -> Result<PiInstance, ProcessError> {
        let state = self.state.read().unwrap();
        if state_driver(&state, instance_id)? != "pi" {
            return Err(ProcessError::Protocol("Provider instance is not Pi.".into()));
        }
        state.pi.get(instance_id).cloned().ok_or_else(|| ProcessError::Protocol("Pi provider instance is unavailable.".into()))
    }
    pub fn codex(&self, instance_id: &str) -> Result<CodexInstance, ProcessError> {
        let state = self.state.read().unwrap();
        if state_driver(&state, instance_id)? != "codex" {
            return Err(ProcessError::Protocol(
                "Provider instance is not Codex.".into(),
            ));
        }
        state
            .codex
            .get(instance_id)
            .cloned()
            .ok_or_else(|| ProcessError::Protocol("Codex provider instance is unavailable.".into()))
    }
    /// What a one-shot text generation needs of an instance: its configuration, not its readiness
    /// (the original asks the instance's CLI whatever its last health check said).
    pub(crate) fn text_instance(&self, instance_id: &str) -> Option<TextInstance> {
        let state = self.state.read().unwrap();
        let snapshot = state.snapshots.iter().find(|snapshot| snapshot["instanceId"] == instance_id)?;
        if let Some(instance) = state.codex.get(instance_id) {
            return Some(TextInstance::Codex(instance.clone(), snapshot["models"].as_array().cloned().unwrap_or_default()));
        }
        if let Some(instance) = state.claude.get(instance_id) {
            return Some(TextInstance::Claude(instance.clone()));
        }
        if let Some(instance) = state.pi.get(instance_id) {
            return Some(TextInstance::Pi(instance.clone()));
        }
        if let Some(instance) = state.opencode.get(instance_id) {
            return Some(TextInstance::OpenCode(instance.clone(), self.idle_release().clock()));
        }
        if let Some(instance) = state.antigravity.get(instance_id) {
            return Some(TextInstance::Antigravity(instance.clone()));
        }
        Some(TextInstance::Other(snapshot["driver"].as_str().unwrap_or("").to_string()))
    }
    /// How one-shot text generations are bounded; tests tighten it.
    pub fn text_generation_options(&self) -> crate::text_generation_run::Options {
        self.text_generation.read().unwrap().clone()
    }
    pub fn set_text_generation_options(&self, options: crate::text_generation_run::Options) {
        *self.text_generation.write().unwrap() = options;
    }
}
/// An instance as one-shot text generation sees it.
pub enum TextInstance {
    /// With the models its last snapshot listed.
    Codex(CodexInstance, Vec<Value>),
    Claude(ClaudeInstance),
    Pi(crate::pi_instance::PiInstance),
    /// With the clock its server's idle window runs on.
    OpenCode(OpenCodeInstance, Arc<dyn crate::scheduled_tasks::SchedulerClock>),
    Antigravity(AntigravityInstance),
    /// A driver with no adapter here.
    Other(String),
}
/// On-demand re-probing, update state and maintenance lookups (see provider_manage.rs).
impl ProviderRegistry {
    pub fn management(&self) -> &crate::provider_manage::Management {
        &self.management
    }
    fn publish(&self, snapshots: Vec<Value>) {
        let published = Arc::new(snapshots);
        self.changes.lock().unwrap().senders.retain(|_, sender| sender.send(published.clone()).is_ok());
    }
    /// The update target of a live Codex, Claude or Grok instance; other drivers cannot be updated here.
    pub(crate) fn maintenance_target(&self, instance_id: &str) -> Option<crate::provider_manage::Target> {
        let state = self.state.read().unwrap();
        state.codex.get(instance_id).map(crate::provider_manage::codex_target).or_else(|| state.claude.get(instance_id).map(crate::provider_manage::claude_target)).or_else(|| state.grok.get(instance_id).map(crate::provider_manage::grok_target)).or_else(|| state.antigravity.get(instance_id).map(crate::provider_manage::antigravity_target)).or_else(|| state.pi.get(instance_id).map(crate::provider_manage::pi_target)).or_else(|| state.opencode.get(instance_id).and_then(crate::opencode_instance::update_target))
    }
    pub(crate) fn codex_instance(&self, instance_id: &str) -> Option<CodexInstance> {
        self.state.read().unwrap().codex.get(instance_id).cloned()
    }
    pub(crate) fn acp_instance(&self, instance_id: &str) -> Option<AcpInstance> {
        self.state.read().unwrap().acp.get(instance_id).cloned()
    }
    pub(crate) fn antigravity_instance_ids(&self) -> Vec<String> {
        self.state.read().unwrap().antigravity.keys().cloned().collect()
    }
    pub(crate) fn codex_instance_ids(&self) -> Vec<String> {
        self.state.read().unwrap().codex.keys().cloned().collect()
    }
    /// Every executable the configured instances run, with the environment each would run in.
    pub(crate) fn settings_binaries(&self) -> Vec<(String, HashMap<String, String>)> {
        derive_instance_configs(&self.settings())
            .values()
            .filter_map(|entry| {
                let binary = entry.config.as_ref()?.get("binaryPath")?.as_str()?.trim().to_owned();
                let instance: HashMap<String, String> = entry.environment.as_ref().map(|variables| variables.iter().map(|variable| (variable.name.to_string(), variable.value.clone())).collect()).unwrap_or_default();
                (!binary.is_empty()).then(|| (binary, crate::provider_manage::merged_environment(&instance)))
            })
            .collect()
    }
    /// Every live instance that has an npm package, as (instance, package).
    pub(crate) fn maintenance_packages(&self) -> Vec<(String, String)> {
        let state = self.state.read().unwrap();
        state.codex.keys().map(|id| (id.clone(), "@openai/codex".to_owned())).chain(state.claude.keys().map(|id| (id.clone(), "@anthropic-ai/claude-code".to_owned()))).chain(state.pi.keys().map(|id| (id.clone(), "@earendil-works/pi-coding-agent".to_owned()))).collect()
    }
    /// Set (or clear) an instance's volatile update state and republish it. Returns the new list.
    pub async fn set_update_state(&self, instance: &str, update: Option<Value>) -> Vec<Value> {
        self.management.set_update_state(instance, update);
        let mut state = self.state.write().unwrap();
        let Some(snapshot) = state.snapshots.iter_mut().find(|snapshot| snapshot["instanceId"] == instance) else {
            return state.snapshots.clone();
        };
        let before = snapshot.clone();
        self.management.apply_update_state(snapshot);
        let changed = *snapshot != before;
        let snapshots = state.snapshots.clone();
        drop(state);
        if changed {
            self.publish(snapshots.clone());
        }
        snapshots
    }
    /// `snapshot.applyUsageLimits`: fold a sparse runtime update into the usage windows an instance
    /// publishes and republish when they changed.
    pub fn apply_usage_limits(&self, instance: &str, update: &Value, checked_at: &str) {
        let mut state = self.state.write().unwrap();
        let Some(snapshot) = state.snapshots.iter_mut().find(|snapshot| snapshot["instanceId"] == instance) else { return };
        let Some(next) = crate::claude_usage_limits::apply_usage_limits_update(snapshot.get("usageLimits").filter(|limits| !limits.is_null()), update, checked_at) else { return };
        if snapshot.get("usageLimits") == Some(&next) {
            return;
        }
        snapshot["usageLimits"] = next;
        let snapshots = state.snapshots.clone();
        drop(state);
        self.publish(snapshots);
    }
    /// Re-probe the named instances, replacing their snapshots (and runtime objects) and publishing the
    /// new list. An instance that no longer has a configuration, or whose probe cannot be built, keeps
    /// its last snapshot. Unknown ids answer with the current list, as the original's do.
    async fn refresh_ids(&self, ids: Option<Vec<String>>) -> Result<Vec<Value>, ProcessError> {
        let _permit = self.refresh.lock().await;
        let (settings, cwd, known) = {
            let state = self.state.read().unwrap();
            (state.settings.clone(), state.cwd.clone(), state.snapshots.iter().filter_map(|snapshot| snapshot["instanceId"].as_str().map(str::to_owned)).collect::<Vec<_>>())
        };
        let entries = derive_instance_configs(&settings);
        let wanted = ids.unwrap_or(known);
        let probes = futures_util::stream::iter(wanted.into_iter().filter_map(|id| {
            let key: ProviderInstanceId = id.parse().ok()?;
            entries.get(&key).cloned().map(|entry| (key, entry))
        }))
        .map(|(id, entry)| Box::pin(discover_entry(id, entry, self.catalog.clone(), self.coordinator.clone(), self.management.clone(), &cwd, false)))
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
        let checks = settings.enable_provider_update_checks;
        let mut finished = vec![];
        for probe in probes {
            let (id, instance, mut snapshot) = match probe {
                Ok(probe) => probe,
                Err(error) => {
                    tracing::warn!(error = %error, "provider refresh could not probe an instance");
                    continue;
                }
            };
            if let Some(RegisteredInstance::Acp(acp)) = &instance {
                apply_coordinator_state(&mut snapshot, &self.coordinator, &id, &acp.config.custom_models);
            }
            let target = match &instance {
                Some(RegisteredInstance::Codex(codex)) => Some(crate::provider_manage::codex_target(codex)),
                Some(RegisteredInstance::Claude(claude)) => Some(crate::provider_manage::claude_target(claude)),
                Some(RegisteredInstance::Grok(grok)) => Some(crate::provider_manage::grok_target(grok)),
                Some(RegisteredInstance::Antigravity(antigravity)) => Some(crate::provider_manage::antigravity_target(antigravity)),
                Some(RegisteredInstance::Pi(pi)) => Some(crate::provider_manage::pi_target(pi)),
                Some(RegisteredInstance::OpenCode(opencode)) => crate::opencode_instance::update_target(opencode),
                _ => None,
            };
            self.management.finish_snapshot(&mut snapshot, target.as_ref(), checks).await;
            finished.push((id, instance, snapshot));
        }
        let mut state = self.state.write().unwrap();
        let before = state.snapshots.clone();
        for (id, instance, snapshot) in finished {
            state.claude.remove(&id);
            state.grok.remove(&id);
            state.antigravity.remove(&id);
            state.opencode.remove(&id);
            state.codex.remove(&id);
            state.acp.remove(&id);
            state.pi.remove(&id);
            match instance {
                Some(RegisteredInstance::Claude(instance)) => {
                    state.claude.insert(id.clone(), instance);
                }
                Some(RegisteredInstance::Grok(instance)) => {
                    state.grok.insert(id.clone(), instance);
                }
                Some(RegisteredInstance::Antigravity(instance)) => {
                    state.antigravity.insert(id.clone(), instance);
                }
                Some(RegisteredInstance::OpenCode(instance)) => {
                    state.opencode.insert(id.clone(), instance);
                }
                Some(RegisteredInstance::Codex(instance)) => {
                    state.codex.insert(id.clone(), instance);
                }
                Some(RegisteredInstance::Acp(instance)) => {
                    state.acp.insert(id.clone(), instance);
                }
                Some(RegisteredInstance::Pi(instance)) => {
                    state.pi.insert(id.clone(), instance);
                }
                None => {}
            }
            match state.snapshots.iter_mut().find(|existing| existing["instanceId"] == id.as_str()) {
                Some(existing) => {
                    // Per-workspace snapshots survive a machine re-probe, as in the source's merge.
                    let mut snapshot = snapshot;
                    // A probe that failed this time must not wipe bars an earlier probe or a turn drew.
                    if snapshot["driver"] == "claudeAgent" {
                        match crate::claude_usage_limits::resolve_usage_limits_after_probe(existing.get("usageLimits"), snapshot.get("usageLimits")) {
                            Some(limits) => snapshot["usageLimits"] = limits,
                            None => {
                                snapshot.as_object_mut().map(|snapshot| snapshot.remove("usageLimits"));
                            }
                        }
                    }
                    // The health check only initializes the agent, so what a session proved about the account is carried.
                    if snapshot["driver"] == "antigravity" {
                        crate::antigravity_instance::carry_saved_account(existing, &mut snapshot);
                        crate::antigravity_instance::keep_signed_out(existing, &mut snapshot);
                    }
                    if snapshot.get("workspaceSnapshots").is_none() {
                        if let Some(kept) = existing.get("workspaceSnapshots").cloned() {
                            snapshot["workspaceSnapshots"] = kept;
                        }
                    }
                    *existing = snapshot;
                }
                None => state.snapshots.push(snapshot),
            }
        }
        let snapshots = state.snapshots.clone();
        drop(state);
        if snapshots != before {
            self.publish(snapshots.clone());
        }
        Ok(snapshots)
    }
    /// `refreshInstance`.
    pub async fn refresh_instance(&self, instance: &str) -> Result<Vec<Value>, ProcessError> {
        self.refresh_ids(Some(vec![instance.to_owned()])).await
    }
    /// `refresh()`: untargeted refreshes probe every instance, so callers arriving while one runs
    /// share it instead of each starting their own set of provider processes.
    pub async fn refresh_all(&self) -> Result<Vec<Value>, ProcessError> {
        use futures_util::FutureExt;
        let pass = {
            let mut slot = self.inflight.lock().unwrap();
            match &*slot {
                Some(pass) => pass.clone(),
                None => {
                    let registry = self.clone();
                    // Detached, so an abandoned first caller cannot cancel the pass others await.
                    let task = tokio::spawn(async move {
                        let result = Box::pin(registry.refresh_ids(None)).await.map_err(|error| error.to_string());
                        registry.inflight.lock().unwrap().take();
                        result
                    });
                    let pass = async move { task.await.unwrap_or_else(|error| Err(error.to_string())) }.boxed().shared();
                    *slot = Some(pass.clone());
                    pass
                }
            }
        };
        pass.await.map_err(ProcessError::Protocol)
    }
    /// `refreshWorkspaceSnapshot`: the skills and slash commands one cwd sees, kept per provider for a few
    /// minutes. A cwd with a current snapshot is left alone unless `fresh`; a fresh scan also re-reads the
    /// instance's machine snapshot and drops the other instances' snapshots for the cwd.
    pub async fn refresh_workspace(&self, instance: &str, cwd: &str, fresh: bool) -> Result<Vec<Value>, ProcessError> {
        const MAX_SNAPSHOTS: usize = 16;
        let workspace_of = |provider: &Value| provider["workspaceSnapshots"].as_array().and_then(|all| all.iter().find(|snapshot| snapshot["cwd"] == cwd)).cloned();
        if fresh {
            let mut state = self.state.write().unwrap();
            let mut changed = false;
            for provider in state.snapshots.iter_mut().filter(|provider| provider["instanceId"] != instance) {
                if let Some(all) = provider.get_mut("workspaceSnapshots").and_then(Value::as_array_mut) {
                    let before = all.len();
                    all.retain(|snapshot| snapshot["cwd"] != cwd);
                    changed |= all.len() != before;
                }
            }
            let snapshots = state.snapshots.clone();
            drop(state);
            if changed {
                self.publish(snapshots);
            }
        }
        let provider = self.snapshots().into_iter().find(|provider| provider["instanceId"] == instance);
        let Some(provider) = provider.filter(|provider| provider["enabled"] == true) else {
            return Ok(self.snapshots());
        };
        let scanned_from = workspace_of(&provider);
        let current = scanned_from.as_ref().is_some_and(|snapshot| {
            snapshot["slashCommandsPending"] != true
                && snapshot["checkedAt"].as_str().is_some_and(|at| chrono::DateTime::parse_from_rfc3339(at).is_ok_and(|at| chrono::Utc::now().timestamp_millis() - at.timestamp_millis() < t3_contracts::PROVIDER_WORKSPACE_SNAPSHOT_TTL_MS))
        });
        if !fresh && current {
            return Ok(self.snapshots());
        }
        let scanned_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if fresh {
            self.refresh_instance(instance).await?;
        }
        let (settings, id) = (self.settings(), instance.parse::<ProviderInstanceId>().map_err(|error| ProcessError::Protocol(error.to_string()))?);
        let Some(entry) = derive_instance_configs(&settings).get(&id).cloned() else {
            return Ok(self.snapshots());
        };
        let Ok((_, _, scoped)) = Box::pin(discover_entry(id, entry, self.catalog.clone(), self.coordinator.clone(), self.management.clone(), Path::new(cwd), true)).await else {
            return Ok(self.snapshots());
        };
        if scoped["status"] == "error" && scoped.get("slashCommandsPending").is_none() {
            return Ok(self.snapshots());
        }
        let mut state = self.state.write().unwrap();
        let before = state.snapshots.clone();
        // Written only if the cwd's snapshot did not change during the scan: a newer one landed first.
        if let Some(provider) = state.snapshots.iter_mut().find(|provider| provider["instanceId"] == instance && workspace_of(provider) == scanned_from) {
            let mut all = provider["workspaceSnapshots"].as_array().cloned().unwrap_or_default();
            all.retain(|snapshot| snapshot["cwd"] != cwd);
            all.push(json!({"cwd":cwd,"checkedAt":scanned_at,"slashCommands":scoped["slashCommands"],"skills":scoped["skills"]}));
            let keep = all.len().saturating_sub(MAX_SNAPSHOTS);
            provider["workspaceSnapshots"] = json!(all.split_off(keep));
        }
        let snapshots = state.snapshots.clone();
        drop(state);
        if snapshots != before {
            self.publish(snapshots.clone());
        }
        Ok(snapshots)
    }
}
/// Probe one configured instance: its runtime object (when the driver has an adapter here) and its snapshot.
async fn discover_entry(
    id: ProviderInstanceId,
    entry: ProviderInstanceConfig,
    catalog: Option<crate::acp_registry_support::Catalog>,
    coordinator: crate::acp_coordinator::Coordinator,
    management: crate::provider_manage::Management,
    cwd: &Path,
    // A scan for one workspace skips what only the machine's snapshot carries (usage windows).
    workspace: bool,
) -> Result<(String, Option<RegisteredInstance>, Value), ProcessError> {
        let id = id.to_string();
        if entry.driver.as_str() == "acpRegistry" {
            let config =
                match serde_json::from_value::<t3_contracts::AcpRegistrySettings>(
                    entry.config.clone().unwrap_or(json!({})),
                ) {
                    Ok(config) => config,
                    Err(error) => {
                        return Ok((
                            id.clone(),
                            None,
                            unavailable(
                                &id,
                                &entry,
                                format!("Invalid ACP config: {error}"),
                            )?,
                        ));
                    }
                };
            let instance = AcpInstance {
                instance_id: id.clone(),
                display_name: entry
                    .display_name
                    .as_ref()
                    .and_then(Option::as_ref)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "ACP Registry".into()),
                accent_color: entry
                    .accent_color
                    .as_ref()
                    .and_then(Option::as_ref)
                    .map(ToString::to_string),
                enabled: t3_contracts::resolve_provider_instance_enabled(&entry),
                config,
                catalog,
                coordinator,
                environment: entry
                    .environment
                    .as_ref()
                    .map(|variables| {
                        variables
                            .iter()
                            .map(|variable| {
                                (variable.name.to_string(), variable.value.clone())
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            let snapshot = instance
                .discover(cwd)
                .await
                .map_err(|error| ProcessError::Protocol(error.to_string()))?;
            return Ok((id, Some(RegisteredInstance::Acp(instance)), snapshot));
        }
        if entry.driver.as_str() == "claudeAgent" {
            let config = match serde_json::from_value::<t3_contracts::ClaudeSettings>(entry.config.clone().unwrap_or(json!({}))) {
                Ok(config) => config,
                Err(error) => return Ok((id.clone(), None, unavailable(&id, &entry, format!("Invalid config for instance '{id}': {error}"))?)),
            };
            let instance = ClaudeInstance {
                instance_id: id.clone(),
                display_name: entry.display_name.as_ref().and_then(Option::as_ref).map(ToString::to_string).unwrap_or_else(|| "Claude".into()),
                accent_color: entry.accent_color.as_ref().and_then(Option::as_ref).map(ToString::to_string),
                enabled: t3_contracts::resolve_provider_instance_enabled(&entry),
                config,
                environment: entry.environment.as_ref().map(|variables| variables.iter().map(|variable| (variable.name.to_string(), variable.value.clone())).collect()).unwrap_or_default(),
            };
            let snapshot = instance.discover_scoped(cwd, !workspace).await?;
            return Ok((id, Some(RegisteredInstance::Claude(instance)), snapshot));
        }
        if entry.driver.as_str() == "grok" {
            let config = match serde_json::from_value::<t3_contracts::GrokSettings>(entry.config.clone().unwrap_or(json!({}))) {
                Ok(config) => config,
                Err(error) => return Ok((id.clone(), None, unavailable(&id, &entry, format!("Invalid config for instance '{id}': {error}"))?)),
            };
            let instance = GrokInstance {
                instance_id: id.clone(),
                display_name: entry.display_name.as_ref().and_then(Option::as_ref).map(ToString::to_string).unwrap_or_else(|| "Grok".into()),
                accent_color: entry.accent_color.as_ref().and_then(Option::as_ref).map(ToString::to_string),
                enabled: t3_contracts::resolve_provider_instance_enabled(&entry),
                config,
                environment: entry.environment.as_ref().map(|variables| variables.iter().map(|variable| (variable.name.to_string(), variable.value.clone())).collect()).unwrap_or_default(),
            };
            let snapshot = instance.discover_scoped(cwd, !workspace).await?;
            return Ok((id, Some(RegisteredInstance::Grok(instance)), snapshot));
        }
        if entry.driver.as_str() == "pi" {
            let config = match serde_json::from_value::<t3_contracts::PiSettings>(entry.config.clone().unwrap_or(json!({}))) {
                Ok(config) => config,
                Err(error) => return Ok((id.clone(), None, unavailable(&id, &entry, format!("Invalid config for instance '{id}': {error}"))?)),
            };
            let instance = PiInstance {
                instance_id: id.clone(),
                display_name: entry.display_name.as_ref().and_then(Option::as_ref).map(ToString::to_string).unwrap_or_else(|| "Pi".into()),
                accent_color: entry.accent_color.as_ref().and_then(Option::as_ref).map(ToString::to_string),
                enabled: t3_contracts::resolve_provider_instance_enabled(&entry),
                config,
                environment: entry.environment.as_ref().map(|variables| variables.iter().map(|variable| (variable.name.to_string(), variable.value.clone())).collect()).unwrap_or_default(),
            };
            let snapshot = instance.discover_scoped(cwd, !workspace).await?;
            return Ok((id, Some(RegisteredInstance::Pi(instance)), snapshot));
        }
        if entry.driver.as_str() == "antigravity" {
            let config = match serde_json::from_value::<t3_contracts::AntigravitySettings>(entry.config.clone().unwrap_or(json!({}))) {
                Ok(config) => config,
                Err(error) => return Ok((id.clone(), None, unavailable(&id, &entry, format!("Invalid config for instance '{id}': {error}"))?)),
            };
            let instance = AntigravityInstance {
                instance_id: id.clone(),
                display_name: entry.display_name.as_ref().and_then(Option::as_ref).map(ToString::to_string).unwrap_or_else(|| "Antigravity".into()),
                accent_color: entry.accent_color.as_ref().and_then(Option::as_ref).map(ToString::to_string),
                enabled: t3_contracts::resolve_provider_instance_enabled(&entry),
                config,
                environment: entry.environment.as_ref().map(|variables| variables.iter().map(|variable| (variable.name.to_string(), variable.value.clone())).collect()).unwrap_or_default(),
                management: management.clone(),
            };
            let snapshot = instance.discover(cwd).await?;
            return Ok((id, Some(RegisteredInstance::Antigravity(instance)), snapshot));
        }
        if entry.driver.as_str() == "opencode" {
            let config = match serde_json::from_value::<t3_contracts::OpenCodeSettings>(entry.config.clone().unwrap_or(json!({}))) {
                Ok(config) => config,
                Err(error) => return Ok((id.clone(), None, unavailable(&id, &entry, format!("Invalid config for instance '{id}': {error}"))?)),
            };
            let instance = OpenCodeInstance::new(
                id.clone(),
                entry.display_name.as_ref().and_then(Option::as_ref).map(ToString::to_string).unwrap_or_else(|| "OpenCode".into()),
                entry.accent_color.as_ref().and_then(Option::as_ref).map(ToString::to_string),
                t3_contracts::resolve_provider_instance_enabled(&entry),
                config,
                entry.environment.as_ref().map(|variables| variables.iter().map(|variable| (variable.name.to_string(), variable.value.clone())).collect()).unwrap_or_default(),
            );
            let snapshot = instance.discover_scoped(cwd, !workspace).await?;
            return Ok((id, Some(RegisteredInstance::OpenCode(instance)), snapshot));
        }
        if entry.driver.as_str() != "codex" {
            return Ok((
                id.clone(),
                None,
                unavailable(
                    &id,
                    &entry,
                    format!(
                        "Driver '{}' is not registered in this native build.",
                        entry.driver
                    ),
                )?,
            ));
        }
        let config = match serde_json::from_value::<CodexConfig>(
            entry.config.clone().unwrap_or(json!({})),
        ) {
            Ok(config) => config,
            Err(error) => {
                return Ok((
                    id.clone(),
                    None,
                    unavailable(
                        &id,
                        &entry,
                        format!("Invalid config for instance '{id}': {error}"),
                    )?,
                ));
            }
        };
        let environment = entry
            .environment
            .as_ref()
            .map(|variables| {
                variables
                    .iter()
                    .map(|variable| {
                        (variable.name.to_string(), variable.value.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let instance = CodexInstance {
            instance_id: id.clone(),
            display_name: entry
                .display_name
                .as_ref()
                .and_then(Option::as_ref)
                .map(ToString::to_string)
                .unwrap_or_else(|| "Codex".into()),
            accent_color: entry
                .accent_color
                .as_ref()
                .and_then(Option::as_ref)
                .map(ToString::to_string),
            enabled: t3_contracts::resolve_provider_instance_enabled(&entry),
            config,
            environment,
        };
        // A managed-setup instance reports its installation; there is no CLI to probe until one is installed.
        let snapshot = if instance.config.setup_mode == Some(t3_contracts::CodexSetupMode::Managed) {
            crate::provider_install::managed_snapshot(&management, &instance).await?
        } else {
            instance.discover(cwd).await?
        };
        Ok::<_, ProcessError>((
            id,
            Some(RegisteredInstance::Codex(instance)),
            snapshot,
        ))
}
fn apply_live_update(
    snapshot: &mut Value,
    update: &crate::acp_coordinator::LiveUpdate,
    custom: &[String],
) {
    use crate::acp_coordinator::LiveUpdate;
    match update {
        LiveUpdate::Commands(commands) => {
            snapshot["slashCommands"] = json!(commands.slash_commands);
            snapshot["skills"] = json!(commands.skills);
        }
        LiveUpdate::Configuration(configuration) => {
            if snapshot["enabled"] == true {
                snapshot["status"] = json!("ready");
            }
            snapshot.as_object_mut().unwrap().remove("message");
            snapshot["models"] = json!(crate::acp_model::models_from_live_configuration(
                configuration,
                custom
            ));
        }
        LiveUpdate::UrlAction(action) => {
            if let Some(action) = action {
                snapshot["auth"]["action"] = json!(action);
            } else {
                snapshot["auth"].as_object_mut().unwrap().remove("action");
            }
        }
    }
}
fn apply_coordinator_state(
    snapshot: &mut Value,
    coordinator: &crate::acp_coordinator::Coordinator,
    instance: &str,
    custom: &[String],
) {
    if let Some(commands) = coordinator.commands(instance) {
        apply_live_update(
            snapshot,
            &crate::acp_coordinator::LiveUpdate::Commands(commands),
            custom,
        );
    }
    if let Some(configuration) = coordinator.configuration(instance) {
        apply_live_update(
            snapshot,
            &crate::acp_coordinator::LiveUpdate::Configuration(configuration),
            custom,
        );
    }
    apply_live_update(
        snapshot,
        &crate::acp_coordinator::LiveUpdate::UrlAction(coordinator.url_action(instance)),
        custom,
    );
}
fn state_driver(state: &RegistryState, instance_id: &str) -> Result<&'static str, ProcessError> {
    let snapshot = state
        .snapshots
        .iter()
        .find(|snapshot| snapshot["instanceId"] == instance_id)
        .ok_or_else(|| {
            ProcessError::Protocol(format!("Provider instance '{instance_id}' is unavailable."))
        })?;
    // Antigravity's health check does not sign in, so it is never ready before a session; the session itself reports
    // a missing installation or a signed-out account.
    if snapshot["enabled"] != true || (snapshot["status"] != "ready" && snapshot["driver"] != "antigravity") {
        return Err(ProcessError::Protocol(
            snapshot["message"]
                .as_str()
                .unwrap_or("Provider instance is not ready.")
                .into(),
        ));
    }
    match snapshot["driver"].as_str() {
        Some("codex") => Ok("codex"),
        Some("claudeAgent") => Ok("claudeAgent"),
        Some("grok") => Ok("grok"),
        Some("pi") => Ok("pi"),
        Some("antigravity") => Ok("antigravity"),
        Some("opencode") => Ok("opencode"),
        Some("acpRegistry") => Ok("acpRegistry"),
        _ => Err(ProcessError::Protocol(
            "Provider driver is not registered.".into(),
        )),
    }
}
fn unavailable(
    id: &str,
    entry: &ProviderInstanceConfig,
    reason: String,
) -> Result<Value, ProcessError> {
    let mut value = json!({"instanceId":id,"driver":entry.driver,"displayName":entry.display_name.as_ref().and_then(Option::as_ref).map(ToString::to_string).unwrap_or_else(||entry.driver.to_string()),"enabled":false,"installed":false,"version":null,"status":"disabled","auth":{"status":"unknown"},"checkedAt":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),"models":[],"slashCommands":[],"skills":[],"availability":"unavailable","unavailableReason":reason,"message":reason});
    if let Some(accent) = entry.accent_color.as_ref().and_then(Option::as_ref) {
        value["accentColor"] = json!(accent);
    }
    let typed: t3_contracts::ServerProvider =
        serde_json::from_value(value).map_err(|error| ProcessError::Protocol(error.to_string()))?;
    Ok(serde_json::to_value(typed).unwrap())
}
/// Apply the original client settings redaction without changing runtime secrets.
pub fn redact_settings(settings: &ServerSettings) -> Value {
    let mut value = serde_json::to_value(settings).unwrap();
    for instance in value["providerInstances"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        if let Some(environment) = instance
            .get_mut("environment")
            .and_then(Value::as_array_mut)
        {
            for variable in environment {
                if variable["sensitive"] == true {
                    let redacted = variable["value"]
                        .as_str()
                        .is_some_and(|value| !value.is_empty())
                        || variable["valueRedacted"] == true;
                    variable["value"] = json!("");
                    if redacted {
                        variable["valueRedacted"] = json!(true);
                    }
                } else {
                    variable.as_object_mut().unwrap().remove("valueRedacted");
                }
            }
        }
    }
    let redact = |value: &mut Value| {
        if value.as_str().is_some_and(|value| !value.is_empty()) {
            *value = json!("••••••");
        }
    };
    if let Some(sources) = value["usageLimitSources"].as_object_mut() {
        for source in sources.values_mut() {
            if let Some(key) = source.get_mut("managementKey") {
                redact(key);
            }
        }
    }
    for field in ["accessToken", "apiToken"] {
        redact(&mut value["bitbucket"][field]);
    }
    if let Some(tokens) = value["github"]["tokens"].as_object_mut() {
        for token in tokens.values_mut() {
            redact(token);
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_instances_win_legacy_slots_and_unknown_drivers_survive() {
        let settings:ServerSettings=serde_json::from_value(json!({"providers":{"codex":{"enabled":true}},"providerInstances":{"codex":{"driver":"fork","config":{"opaque":true}},"codex_work":{"driver":"codex","enabled":false}}})).unwrap();
        let instances = derive_instance_configs(&settings);
        assert_eq!(instances[&"codex".parse().unwrap()].driver.as_str(), "fork");
        assert_eq!(
            instances[&"codex".parse().unwrap()]
                .config
                .as_ref()
                .unwrap()["opaque"],
            true
        );
        assert!(instances.contains_key(&"pi".parse().unwrap()));
        assert_eq!(
            instances[&"codex_work".parse().unwrap()].enabled,
            Some(false)
        );
    }
    #[tokio::test]
    async fn unavailable_and_disabled_instances_never_launch_executables() {
        let settings:ServerSettings=serde_json::from_value(json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","enabled":false},"codex":{"driver":"codex","enabled":false,"config":{"binaryPath":"/must-not-execute"}},"unknown":{"driver":"fork","config":{}}}})).unwrap();
        let registry = ProviderRegistry::discover(&settings, std::env::temp_dir().as_path())
            .await
            .unwrap();
        let snapshots = registry.snapshots();
        assert_eq!(
            snapshots
                .iter()
                .find(|value| value["instanceId"] == "codex")
                .unwrap()["status"],
            "disabled"
        );
        assert_eq!(
            snapshots
                .iter()
                .find(|value| value["instanceId"] == "unknown")
                .unwrap()["availability"],
            "unavailable"
        );
        assert!(registry.codex("codex").is_err());
    }
    #[tokio::test]
    async fn either_explicit_disable_flag_prevents_executable_launch() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("provider");
        let marker = directory.path().join("launched");
        std::fs::write(
            &script,
            "#!/usr/bin/env python3\nimport os\nopen(os.environ['MARKER'],'w').write('started')\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let settings:ServerSettings=serde_json::from_value(json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","enabled":false},"codex":{"driver":"codex","enabled":true,"config":{"enabled":false,"binaryPath":script,"homePath":directory.path().join("codex-home")},"environment":[{"name":"MARKER","value":marker}]},"codex_work":{"driver":"codex","enabled":false,"config":{"enabled":true,"binaryPath":script,"homePath":directory.path().join("codex-home")},"environment":[{"name":"MARKER","value":marker}]}}})).unwrap();
        let registry = ProviderRegistry::discover(&settings, directory.path())
            .await
            .unwrap();
        for id in ["codex", "codex_work"] {
            assert_eq!(
                registry
                    .snapshots()
                    .iter()
                    .find(|value| value["instanceId"] == id)
                    .unwrap()["status"],
                "disabled"
            );
            assert!(registry.codex(id).is_err());
        }
        assert!(!marker.exists());
    }
    #[tokio::test]
    async fn provider_changes_are_changes_only_buffered_and_follow_complete_shared_state_swap() {
        let directory = tempfile::tempdir().unwrap();
        let mut input = serde_json::to_value(ServerSettings::default()).unwrap();
        for config in input["providers"].as_object_mut().unwrap().values_mut() {
            config["enabled"] = json!(false);
        }
        input["providerInstances"] =
            json!({"codex":{"driver":"codex","enabled":false,"displayName":"Old"}});
        let old: ServerSettings = serde_json::from_value(input.clone()).unwrap();
        let registry = ProviderRegistry::discover(&old, directory.path())
            .await
            .unwrap();
        let shared = registry.clone();
        let (initial, mut changes) = registry.snapshot_and_subscribe();
        assert_eq!(
            initial
                .iter()
                .find(|row| row["instanceId"] == "codex")
                .unwrap()["displayName"],
            "Old"
        );
        assert!(
            matches!(
                changes.receiver.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "source changes stream does not replay a seed"
        );
        input["providerInstances"]["codex"]["displayName"] = json!("New");
        let new = serde_json::from_value(input).unwrap();
        registry.reconfigure(&new, directory.path()).await.unwrap();
        let event = changes.recv().await.unwrap();
        assert_eq!(
            event
                .iter()
                .find(|row| row["instanceId"] == "codex")
                .unwrap()["displayName"],
            "New"
        );
        assert_eq!(shared.snapshots(), *event);
        assert_eq!(shared.settings(), new);
        assert!(shared.codex("codex").is_err());
        drop(changes);
        assert!(
            registry.changes.lock().unwrap().senders.is_empty(),
            "dropping a client releases its source queue"
        );
    }
    #[test]
    fn settings_redact_sensitive_environment_without_mutating_runtime_values() {
        let settings:ServerSettings=serde_json::from_value(json!({"providerInstances":{"work":{"driver":"codex","environment":[{"name":"SECRET","value":"private","sensitive":true},{"name":"PUBLIC","value":"public","valueRedacted":true}]}},"github":{"tokens":{"github.com":"private"}},"bitbucket":{"apiToken":"private"}})).unwrap();
        let redacted = redact_settings(&settings);
        assert_eq!(
            redacted["providerInstances"]["work"]["environment"][0]["value"],
            ""
        );
        assert_eq!(
            redacted["providerInstances"]["work"]["environment"][0]["valueRedacted"],
            true
        );
        assert!(
            redacted["providerInstances"]["work"]["environment"][1]
                .get("valueRedacted")
                .is_none()
        );
        assert_ne!(redacted["github"]["tokens"]["github.com"], "private");
        assert_eq!(
            settings.provider_instances[&"work".parse().unwrap()]
                .environment
                .as_ref()
                .unwrap()[0]
                .value,
            "private"
        );
    }
}
