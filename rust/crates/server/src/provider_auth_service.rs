//! Environment-local credential routing, independent of the WebSocket owner.
use crate::{
    acp_auth::AcpAuth,
    acp_authentication_state::AuthenticationState,
    provider_auth_flow::{AuthFlow, AuthResult, RoutedStop, SessionAccess, StopSession},
    provider_registry::{ProviderRegistry, derive_instance_configs},
};
use futures_util::future::BoxFuture;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use t3_contracts::*;
use tokio::sync::{mpsc, watch};

/// The execution owner resolves these instance IDs against persisted session
/// bindings, rather than a thread's currently selected model.
pub type StopInstances =
    Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, AuthResult<()>> + Send + Sync>;

#[derive(Clone)]
struct Controller {
    fingerprint: String,
    binding: String,
    flow: AuthFlow,
}
struct Inner {
    registry: ProviderRegistry,
    cwd: PathBuf,
    cache_dir: PathBuf,
    controllers: Arc<tokio::sync::Mutex<HashMap<String, Controller>>>,
    credentials: tokio::sync::Mutex<()>,
    reconfiguration: Arc<tokio::sync::Mutex<()>>,
    #[cfg(test)]
    closing: std::sync::Mutex<Option<mpsc::UnboundedSender<()>>>,
    stop: StopInstances,
    stopped: watch::Sender<bool>,
}
struct Owner(Arc<Inner>);
impl std::ops::Deref for Owner {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stopped.send_replace(true);
        let inner = self.0.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                close_controllers(&inner).await;
            });
        }
    }
}
#[derive(Clone)]
pub struct ProviderAuthService(Arc<Owner>);
#[derive(Clone)]
pub(crate) struct WeakProviderAuthService(std::sync::Weak<Owner>);
impl WeakProviderAuthService {
    pub(crate) fn upgrade(&self) -> Option<ProviderAuthService> {
        self.0.upgrade().map(ProviderAuthService)
    }
}
pub(crate) struct ReconfigurationHold {
    _admission: tokio::sync::OwnedMutexGuard<()>,
}

pub struct ProviderAuthChanges {
    receiver: mpsc::UnboundedReceiver<AuthResult<ProviderAuthState>>,
    stop: watch::Sender<bool>,
}
impl ProviderAuthChanges {
    pub async fn recv(&mut self) -> Option<AuthResult<ProviderAuthState>> {
        self.receiver.recv().await
    }
}
impl Drop for ProviderAuthChanges {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
async fn close_controllers(inner: &Inner) {
    let _admission = inner.reconfiguration.lock().await;
    let controllers = std::mem::take(&mut *inner.controllers.lock().await);
    for controller in controllers.into_values() {
        controller.flow.shutdown().await;
    }
}

fn failure(instance: &str, operation: &str, detail: &str) -> ProviderSetupError {
    ProviderSetupError {
        tag: ProviderSetupErrorTag::ProviderSetupError,
        instance_id: instance.parse().expect("validated instance"),
        operation: operation.into(),
        detail: detail.into(),
        cause: None,
    }
}
fn binding(instance: &str, config: &AcpRegistrySettings) -> String {
    if config.source == AcpRegistrySettingsSource::Local {
        format!("acp:local:{instance}")
    } else {
        format!("acp:{}", config.agent_id)
    }
}
// Effect Equal compares records independent of insertion order, while preserving
// array order. Normalize JSON numbers to their JavaScript numeric identity too.
fn structural_fingerprint(value: serde_json::Value) -> String {
    fn canonical(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(object) => {
                let sorted: std::collections::BTreeMap<_, _> = object.into_iter().collect();
                serde_json::Value::Object(
                    sorted
                        .into_iter()
                        .map(|(key, value)| (key, canonical(value)))
                        .collect(),
                )
            }
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(canonical).collect())
            }
            serde_json::Value::Number(number) => serde_json::from_str(&t3_acp::js_number(&number))
                .expect("JavaScript finite JSON number"),
            value => value,
        }
    }
    serde_json::to_string(&canonical(value)).expect("JSON fingerprint")
}
impl ProviderAuthService {
    pub fn new(
        registry: ProviderRegistry,
        cwd: PathBuf,
        cache_dir: PathBuf,
        stop: StopInstances,
    ) -> Self {
        let service = Self(Arc::new(Owner(Arc::new(Inner {
            registry: registry.clone(),
            cwd,
            cache_dir,
            stop,
            controllers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            credentials: tokio::sync::Mutex::new(()),
            reconfiguration: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(test)]
            closing: std::sync::Mutex::new(None),
            stopped: watch::channel(false).0,
        }))));
        registry.attach_auth(WeakProviderAuthService(Arc::downgrade(&service.0)));
        service
    }
    pub(crate) async fn hold_reconfiguration(
        &self,
        settings: &ServerSettings,
    ) -> ReconfigurationHold {
        let admission = self.0.reconfiguration.clone().lock_owned().await;
        let entries = derive_instance_configs(settings);
        let mut controllers = self.0.controllers.clone().lock_owned().await;
        let removed = controllers
            .iter()
            .filter(|(id, controller)| {
                let entry = entries
                    .iter()
                    .find(|(entry, _)| entry.as_str() == id.as_str())
                    .map(|(_, value)| value);
                entry.is_none_or(|entry| {
                    structural_fingerprint(serde_json::to_value(entry).unwrap())
                        != controller.fingerprint
                })
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let closing = removed
            .into_iter()
            .filter_map(|id| controllers.remove(&id))
            .collect::<Vec<_>>();
        drop(controllers);
        #[cfg(test)]
        if let Some(closing) = self.0.closing.lock().unwrap().as_ref() {
            let _ = closing.send(());
        }
        // Credential stop routing can read the controller map during cleanup.
        // Only factory admission stays held through registry publication.
        for controller in closing {
            controller.flow.shutdown().await;
        }
        ReconfigurationHold {
            _admission: admission,
        }
    }
    async fn controller(&self, instance: &str, operation: &str) -> AuthResult<Controller> {
        if *self.0.stopped.borrow() {
            return Err(failure(
                instance,
                operation,
                "This provider instance is no longer available.",
            ));
        }
        let _admission = self.0.reconfiguration.lock().await;
        if *self.0.stopped.borrow() {
            return Err(failure(
                instance,
                operation,
                "This provider instance is no longer available.",
            ));
        }
        let mut controllers = self.0.controllers.lock().await;
        let (entry, captured) = self.0.registry.acp_auth_entry(instance).ok_or_else(|| {
            failure(
                instance,
                operation,
                "This provider instance is no longer available.",
            )
        })?;
        let fingerprint =
            structural_fingerprint(serde_json::to_value(&entry).expect("typed provider settings"));
        if let Some(controller) = controllers
            .get(instance)
            .filter(|controller| controller.fingerprint == fingerprint)
        {
            return Ok(controller.clone());
        }
        if let Some(previous) = controllers.remove(instance) {
            // Successful access scopes remain owned by their actor parent.
            drop(controllers);
            previous.flow.shutdown().await;
            controllers = self.0.controllers.lock().await;
        }
        if *self.0.stopped.borrow() {
            return Err(failure(
                instance,
                operation,
                "This provider instance is no longer available.",
            ));
        }
        let environment = entry.environment.clone().unwrap_or_default();
        let mut process_environment = std::env::vars().collect::<HashMap<String, String>>();
        process_environment.extend(captured.environment.clone());
        let confirmation = AuthenticationState::open(
            self.0.cache_dir.clone(),
            instance,
            &captured.config,
            &environment,
            &process_environment,
        )
        .await;
        let backend = AcpAuth::new(captured.clone(), self.0.cwd.clone(), confirmation);
        let controller = Controller {
            fingerprint,
            binding: binding(instance, &captured.config),
            flow: backend.controller(),
        };
        controllers.insert(instance.into(), controller.clone());
        Ok(controller)
    }
    async fn check_shared_binding(
        &self,
        instance: &str,
        operation: &str,
        target: &Controller,
    ) -> AuthResult<()> {
        if self
            .0
            .controllers
            .lock()
            .await
            .iter()
            .any(|(id, controller)| {
                id != instance
                    && controller.binding == target.binding
                    && controller.flow.is_changing_credentials()
            })
        {
            return Err(failure(
                instance,
                operation,
                "Another provider instance is changing this shared sign-in. Finish or cancel it first.",
            ));
        }
        Ok(())
    }
    fn stop_routed(&self, instance: String, credential_binding: String) -> RoutedStop {
        let service = self.clone();
        RoutedStop::new(Arc::new(move || {
            let service = service.clone();
            let instance = instance.clone();
            let credential_binding = credential_binding.clone();
            Box::pin(async move {
                let mut affected = vec![instance.clone()];
                for (id, entry) in derive_instance_configs(&service.0.registry.settings()) {
                    if entry.driver.as_str() != "acpRegistry" {
                        continue;
                    }
                    let Ok(config) = serde_json::from_value::<AcpRegistrySettings>(
                        entry.config.unwrap_or_else(|| serde_json::json!({})),
                    ) else {
                        continue;
                    };
                    if id.as_str() != instance
                        && binding(id.as_str(), &config) == credential_binding
                    {
                        affected.push(id.to_string());
                    }
                }
                (service.0.stop)(affected.clone()).await?;
                let other = service
                    .0
                    .controllers
                    .lock()
                    .await
                    .iter()
                    .filter(|(id, controller)| {
                        *id != &instance
                            && affected.contains(id)
                            && controller.binding == credential_binding
                    })
                    .map(|(_, controller)| controller.flow.clone())
                    .collect::<Vec<_>>();
                for controller in other {
                    controller.invalidate().await;
                }
                Ok(())
            })
        }))
    }
    pub async fn start(
        &self,
        input: ProviderAuthStartInput,
        owner: String,
    ) -> AuthResult<ProviderAuthState> {
        let _credentials = self.0.credentials.lock().await;
        let id = input.instance_id.to_string();
        let controller = self.controller(&id, "start").await?;
        self.check_shared_binding(&id, "start", &controller).await?;
        controller
            .flow
            .start(
                owner,
                input.method_id.map(|id| id.0.to_string()),
                input.return_url,
                input.callback_mode,
                self.stop_routed(id, controller.binding),
            )
            .await
    }
    pub async fn respond(
        &self,
        input: ProviderAuthRespondInput,
        owner: &str,
    ) -> AuthResult<ProviderAuthState> {
        self.controller(input.instance_id.as_str(), "respond")
            .await?
            .flow
            .respond(owner, input)
            .await
    }
    pub async fn complete(
        &self,
        input: ProviderAuthCompleteInput,
        owner: &str,
    ) -> AuthResult<ProviderAuthState> {
        self.controller(input.instance_id.as_str(), "complete")
            .await?
            .flow
            .complete(owner, input)
            .await
    }
    pub async fn cancel(
        &self,
        input: ProviderAuthCancelInput,
        owner: &str,
    ) -> AuthResult<ProviderAuthState> {
        self.controller(input.instance_id.as_str(), "cancel")
            .await?
            .flow
            .cancel(owner, input.flow_id.0.as_str())
            .await
    }
    pub async fn logout(&self, instance: &str) -> AuthResult<ProviderAuthState> {
        let _credentials = self.0.credentials.lock().await;
        let controller = self.controller(instance, "logout").await?;
        self.check_shared_binding(instance, "logout", &controller)
            .await?;
        controller
            .flow
            .logout(self.stop_routed(instance.into(), controller.binding))
            .await
    }
    pub async fn subscribe(
        &self,
        instance: &str,
        owner: String,
    ) -> AuthResult<ProviderAuthChanges> {
        // Register registry changes before resolving the initial controller.
        // Each rebuild switches to its seeded, owner-redacted state stream.
        let mut registry = self.0.registry.subscribe_changes();
        let mut controller = self.controller(instance, "subscribe").await?;
        let mut states = controller.flow.subscribe(owner.clone());
        let (sender, receiver) = mpsc::unbounded_channel();
        let (stop, mut cancelled) = watch::channel(false);
        let mut stopped = self.0.stopped.subscribe();
        let service = Arc::downgrade(&self.0);
        let instance = instance.to_owned();
        tokio::spawn(async move {
            let mut closed_states = false;
            loop {
                if *cancelled.borrow() || *stopped.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    _=cancelled.changed()=>break,
                    _=stopped.changed()=>break,
                    state=states.recv(), if !closed_states =>match state {
                        Some(state)=>if sender.send(Ok(state)).is_err(){break},
                        None=>closed_states=true,
                    },
                    changed=registry.recv()=>{
                        if changed.is_none(){break;}
                        let Some(service)=service.upgrade() else {break};
                        match ProviderAuthService(service).controller(&instance,"subscribe").await {
                            Ok(next)=>if next.fingerprint!=controller.fingerprint {
                                states=next.flow.subscribe(owner.clone());
                                closed_states=false;
                                controller=next;
                            },
                            Err(error)=>{let _=sender.send(Err(error));break},
                        }
                    }
                }
            }
        });
        Ok(ProviderAuthChanges { receiver, stop })
    }
    pub async fn admit_session(
        &self,
        instance: &str,
        stop: StopSession,
    ) -> AuthResult<SessionAccess> {
        self.controller(instance, "session")
            .await?
            .flow
            .admit_session(stop)
            .await
    }
    pub async fn shutdown(&self) {
        self.0.stopped.send_replace(true);
        close_controllers(&self.0).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::time::Duration;

    async fn methods(states: &mut ProviderAuthChanges) -> ProviderAuthState {
        loop {
            let state = states.recv().await.unwrap().unwrap();
            if state.methods.is_some() {
                return state;
            }
        }
    }
    async fn phase(
        states: &mut ProviderAuthChanges,
        phase: ProviderAuthPhase,
    ) -> ProviderAuthState {
        loop {
            let state = states.recv().await.unwrap().unwrap();
            if state.phase == phase {
                return state;
            }
            assert_ne!(
                state.phase,
                ProviderAuthPhase::Failed,
                "{:?}",
                state.message
            );
        }
    }
    fn reaped(log: &std::path::Path) {
        for line in std::fs::read_to_string(log).unwrap().lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            assert_eq!(
                unsafe { libc::kill(row["pid"].as_i64().unwrap() as i32, 0) },
                -1,
                "owned process still present: {row}"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
    #[tokio::test]
    async fn shutdown_rejects_controller_lookup_queued_before_factory_admission_without_spawning() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let root = tempfile::tempdir().unwrap();
            let log = root.path().join("agent.log");
            let settings: ServerSettings = serde_json::from_value(json!({"providerInstances":{
                "codex":{"driver":"codex","enabled":false},
                "agent":{"driver":"acpRegistry","enabled":false,"config":{"source":"local","commandPath":"python3","commandArgs":[format!("{}/tests/fixtures/acp-auth-provider.py",env!("CARGO_MANIFEST_DIR")),log,"2"]}}
            }})).unwrap();
            let registry = ProviderRegistry::discover(&settings,root.path()).await.unwrap();
            let service = ProviderAuthService::new(registry,root.path().into(),root.path().join("caches"),Arc::new(|_|Box::pin(async{Ok(())})));
            let admission = service.0.reconfiguration.lock().await;
            let mut lookup = Box::pin(service.subscribe("agent","owner".into()));
            // One deliberate poll admits the request before factory acquisition.
            assert!(futures_util::poll!(lookup.as_mut()).is_pending());
            let mut shutdown = Box::pin(service.shutdown());
            assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
            assert!(*service.0.stopped.borrow());
            drop(admission);
            assert!(lookup.await.is_err(),"queued lookup must recheck shutdown after admission");
            shutdown.await;
            assert!(!log.exists(),"no new controller method process can escape shutdown");
            assert!(service.0.controllers.lock().await.is_empty());
        }).await.unwrap();
    }
    #[tokio::test]
    async fn reconfiguration_waits_admitted_logout_cleanup_without_holding_controller_map() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let root = tempfile::tempdir().unwrap();
            let log = root.path().join("agent.log");
            let settings: ServerSettings = serde_json::from_value(json!({"providerInstances":{
                "codex":{"driver":"codex","enabled":false},
                "agent":{"driver":"acpRegistry","enabled":false,"config":{"source":"local","commandPath":"python3","commandArgs":[format!("{}/tests/fixtures/acp-auth-provider.py",env!("CARGO_MANIFEST_DIR")),log,"2"]}}
            }})).unwrap();
            let registry = ProviderRegistry::discover(&settings,root.path()).await.unwrap();
            let (entered, mut stop_entered) = mpsc::unbounded_channel();
            let service = ProviderAuthService::new(registry.clone(),root.path().into(),root.path().join("caches"),Arc::new(move |_| {
                let entered = entered.clone();
                Box::pin(async move {
                    let (release,released) = tokio::sync::oneshot::channel();
                    entered.send(release).unwrap();
                    released.await.unwrap();
                    Ok(())
                })
            }));
            let mut states = service.subscribe("agent","owner".into()).await.unwrap();
            methods(&mut states).await;
            let logout_service = service.clone();
            let logout = tokio::spawn(async move { logout_service.logout("agent").await });
            let release = stop_entered.recv().await.unwrap();
            let hold_service = service.clone();
            let mut changed = serde_json::to_value(&settings).unwrap();
            changed["providerInstances"]["agent"]["displayName"] = json!("Changed");
            let changed: ServerSettings = serde_json::from_value(changed).unwrap();
            let (closing, mut closed) = mpsc::unbounded_channel();
            *service.0.closing.lock().unwrap() = Some(closing);
            let hold = tokio::spawn(async move { hold_service.hold_reconfiguration(&changed).await });
            closed.recv().await.unwrap();
            assert!(service.0.controllers.lock().await.is_empty());
            assert!(!hold.is_finished(), "owned logout cleanup remains held");
            release.send(()).unwrap();
            logout.await.unwrap().unwrap();
            drop(hold.await.unwrap());
            service.shutdown().await;
            reaped(&log);
        }).await.unwrap();
    }
    #[tokio::test]
    async fn unready_provider_real_login_is_owner_private_and_rebuild_cancels_owned_callback_scope()
    {
        tokio::time::timeout(Duration::from_secs(15), async {
            let root = tempfile::tempdir().unwrap();
            let log = root.path().join("agent.log");
            let settings: ServerSettings = serde_json::from_value(json!({"providerInstances":{
                "codex":{"driver":"codex","enabled":false},
                "agent":{"driver":"acpRegistry","enabled":true,"config":{"source":"local","commandPath":"python3","commandArgs":[format!("{}/tests/fixtures/acp-auth-provider.py",env!("CARGO_MANIFEST_DIR")),log,"2"]}}
            }})).unwrap();
            let registry = ProviderRegistry::discover(&settings,root.path()).await.unwrap();
            assert!(registry.acp("agent").is_err(), "turn dispatch remains unavailable before readiness");
            let service = ProviderAuthService::new(registry.clone(),root.path().into(),root.path().join("caches"),Arc::new(|_|Box::pin(async{Ok(())})));
            let mut owner = service.subscribe("agent","owner".into()).await.unwrap();
            assert_eq!(methods(&mut owner).await.methods.unwrap().0.len(),3);
            reaped(&log);
            let mut other = service.subscribe("agent","other".into()).await.unwrap();
            let input: ProviderAuthStartInput = serde_json::from_value(json!({"instanceId":"agent","methodId":"agent"})).unwrap();
            let start = service.start(input.clone(),"owner".into()).await.unwrap();
            let waiting = phase(&mut owner,ProviderAuthPhase::Waiting).await;
            assert_eq!(waiting.flow_id, start.flow_id);
            assert_eq!(service.start(input.clone(),"owner".into()).await.unwrap().flow_id,start.flow_id);
            assert!(service.start(input,"other".into()).await.is_err());
            let hidden = phase(&mut other,ProviderAuthPhase::Waiting).await;
            assert!(hidden.flow_id.is_none() && hidden.authorization_url.is_none() && hidden.expires_at.is_none());
            assert_eq!(hidden.interaction,Some(Some(None)));
            let mut changed = serde_json::to_value(&settings).unwrap();
            changed["providerInstances"]["agent"]["displayName"] = json!("Renamed");
            registry.reconfigure(&serde_json::from_value(changed).unwrap(),root.path()).await.unwrap();
            reaped(&log);
            let rebuilt = methods(&mut owner).await;
            assert_eq!(rebuilt.phase,ProviderAuthPhase::Idle);
            assert!(rebuilt.flow_id.is_none());
            assert!(service.cancel(serde_json::from_value(json!({"instanceId":"agent","flowId":start.flow_id.unwrap()})).unwrap(),"owner").await.is_err());
            service.shutdown().await;
            reaped(&log);
            let weak = Arc::downgrade(&service.0);
            drop(owner);drop(other);drop(service);
            assert!(weak.upgrade().is_none(), "registry auth handle must not retain its owner");
        }).await.unwrap();
    }
}
