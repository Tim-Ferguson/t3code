//! Instance routing and source-compatible legacy configuration hydration.
use crate::{
    acp_runtime::AcpInstance,
    codex::{CodexConfig, CodexInstance},
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
    Codex(CodexInstance),
    Acp(AcpInstance),
}
struct RegistryState {
    codex: HashMap<String, CodexInstance>,
    acp: HashMap<String, AcpInstance>,
    snapshots: Vec<Value>,
    settings: ServerSettings,
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
}
impl ProviderRegistry {
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
        let mut registry = Self::discover_with_context(
            settings,
            cwd,
            catalog,
            crate::acp_coordinator::Coordinator::default(),
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
    ) -> Result<Self, ProcessError> {
        let entries = derive_instance_configs(settings);
        let mut results =
            futures_util::stream::iter(entries)
                .map(|(id, entry)| {
                    let catalog = catalog.clone();
                    let coordinator = coordinator.clone();
                    async move {
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
                        let snapshot = instance.discover(cwd).await?;
                        Ok::<_, ProcessError>((
                            id,
                            Some(RegisteredInstance::Codex(instance)),
                            snapshot,
                        ))
                    }
                })
                .buffer_unordered(4)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;
        // Keep deterministic instance ordering even when probes complete out of order.
        results.sort_by(|left, right| left.0.cmp(&right.0));
        let mut codex = HashMap::new();
        let mut acp = HashMap::new();
        let mut snapshots = vec![];
        for (id, instance, snapshot) in results {
            if let Some(instance) = instance {
                match instance {
                    RegisteredInstance::Codex(instance) => {
                        codex.insert(id, instance);
                    }
                    RegisteredInstance::Acp(instance) => {
                        acp.insert(id, instance);
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
                codex,
                acp,
                snapshots,
                settings: settings.clone(),
            })),
            catalog,
            refresh: Arc::new(tokio::sync::Mutex::new(())),
            changes: Arc::new(Mutex::new(ProviderListeners::default())),
            coordinator,
            observer: None,
            auth: Arc::new(RwLock::new(None)),
            health: Default::default(),
            mcp: Default::default(),
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
        }
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
    if snapshot["enabled"] != true || snapshot["status"] != "ready" {
        return Err(ProcessError::Protocol(
            snapshot["message"]
                .as_str()
                .unwrap_or("Provider instance is not ready.")
                .into(),
        ));
    }
    match snapshot["driver"].as_str() {
        Some("codex") => Ok("codex"),
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
        let settings:ServerSettings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false,"config":{"binaryPath":"/must-not-execute"}},"unknown":{"driver":"fork","config":{}}}})).unwrap();
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
        let settings:ServerSettings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":true,"config":{"enabled":false,"binaryPath":script},"environment":[{"name":"MARKER","value":marker}]},"codex_work":{"driver":"codex","enabled":false,"config":{"enabled":true,"binaryPath":script},"environment":[{"name":"MARKER","value":marker}]}}})).unwrap();
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
