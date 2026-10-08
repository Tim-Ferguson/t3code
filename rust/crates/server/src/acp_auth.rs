//! Explicit ACP sign-in over the owned process/runtime bridge.
use crate::{
    acp_authentication_state::AuthenticationState,
    acp_client_callbacks::Services,
    acp_coordinator::ForegroundStartup,
    acp_peer::ProcessPeer,
    acp_runtime::{AcpInstance, AuthElicitation},
    provider_auth_flow::{AuthBackend, AuthContext, AuthFlow, AuthResult, Respond},
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::*;
use tokio::sync::{oneshot, watch};

struct Pending {
    peer: ProcessPeer,
    services: Services,
}
impl Pending {
    async fn close(self) {
        self.services.shutdown().await;
        self.peer.shutdown().await;
    }
}
#[derive(Clone)]
struct MethodsJob {
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}
pub(crate) type Changed = Arc<dyn Fn(bool) -> BoxFuture<'static, ()> + Send + Sync>;
#[derive(Clone)]
pub struct AcpAuth {
    instance: AcpInstance,
    cwd: PathBuf,
    confirmation: AuthenticationState,
    changed: Option<Changed>,
    pending: Arc<Mutex<Option<Pending>>>,
    startup: Arc<Mutex<Option<ForegroundStartup>>>,
    terminal: Arc<Mutex<Option<MethodsJob>>>,
    methods_jobs: Arc<Mutex<Vec<MethodsJob>>>,
    known_methods: Arc<Mutex<Option<(String, Vec<ProviderAuthMethod>)>>>,
}
fn failure(instance: &str, operation: &str, detail: &str) -> ProviderSetupError {
    ProviderSetupError {
        tag: ProviderSetupErrorTag::ProviderSetupError,
        instance_id: instance.parse().expect("instance validated"),
        operation: operation.into(),
        detail: detail.into(),
        cause: None,
    }
}
fn text(value: &str, max: usize) -> String {
    String::from_utf16_lossy(
        &trim_wire_string(value)
            .encode_utf16()
            .take(max)
            .collect::<Vec<_>>(),
    )
}
fn auth_initialize() -> t3_acp::types::InitializeRequest {
    use t3_acp::types::*;
    InitializeRequest {
        client_info: Optional::Value(Implementation {
            name: "t3-code-provider-auth".into(),
            version: "0.0.0".into(),
            title: Optional::Missing,
            meta: Optional::Missing,
        }),
        client_capabilities: Some(ClientCapabilities {
            auth: Some(ClientAuthCapabilities {
                terminal: Some(true),
                ..Default::default()
            }),
            fs: Some(FsClientCapabilities {
                read_text_file: Some(false),
                write_text_file: Some(false),
                ..Default::default()
            }),
            terminal: Some(false),
            elicitation: Optional::Value(serde_json::from_value(json!({"url":{}})).unwrap()),
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn advertised_methods(initialized: &Value) -> Vec<ProviderAuthMethod> {
    initialized["authMethods"].as_array().into_iter().flatten().filter_map(|method|{
        let id=method["id"].as_str()?;
        if id.is_empty()||id!=trim_wire_string(id)||id.encode_utf16().count()>128{return None}
        let kind=match method["type"].as_str().unwrap_or("agent"){"agent"=>"agent","terminal"=>"terminal","env_var"=>"credentials",_=>return None};
        let name=text(method["name"].as_str().unwrap_or(""),160);let description=method["description"].as_str().map(|value|text(value,1024)).filter(|value|!value.is_empty());
        serde_json::from_value(json!({"id":id,"name":if name.is_empty(){id}else{&name},"description":description,"type":kind})).ok()
    }).take(32).collect()
}
impl AcpAuth {
    pub fn new(instance: AcpInstance, cwd: PathBuf, confirmation: AuthenticationState) -> Self {
        Self {
            instance,
            cwd,
            confirmation,
            changed: None,
            pending: Arc::new(Mutex::new(None)),
            startup: Arc::new(Mutex::new(None)),
            terminal: Arc::new(Mutex::new(None)),
            methods_jobs: Arc::new(Mutex::new(vec![])),
            known_methods: Arc::new(Mutex::new(None)),
        }
    }
    pub(crate) fn with_changed(mut self, changed: Changed) -> Self {
        self.changed = Some(changed);
        self
    }
    async fn authentication_changed(&self, authenticated: bool) {
        if let Some(changed) = &self.changed {
            changed(authenticated).await;
        } else {
            self.confirmation.set(authenticated).await;
        }
    }
    #[cfg(test)]
    pub(crate) fn confirmation(&self) -> AuthenticationState {
        self.confirmation.clone()
    }
    pub(crate) async fn invalidate_confirmation(&self) {
        self.authentication_changed(false).await;
    }
    pub fn controller(&self) -> AuthFlow {
        AuthFlow::new(
            self.instance.instance_id.clone(),
            Arc::new(self.clone()),
            (!self.instance.config.auth_method_id.as_str().is_empty())
                .then(|| self.instance.config.auth_method_id.to_string()),
            Duration::from_secs(300),
            ProviderCredentialOwner::Provider,
            false,
        )
    }
    async fn close_peer(&self) {
        let pending = self.pending.lock().unwrap().take();
        if let Some(pending) = pending {
            pending.close().await;
        }
    }
    async fn cleanup_pending(&self) {
        let terminal = self.terminal.lock().unwrap().take();
        if let Some(terminal) = terminal {
            terminal.cancel.send_replace(true);
            let mut done = terminal.done;
            if !*done.borrow() {
                let _ = done.wait_for(|done| *done).await;
            }
        }
        self.close_peer().await;
        self.startup.lock().unwrap().take();
    }
    async fn initialize(
        &self,
        elicitation: Option<AuthElicitation>,
    ) -> AuthResult<crate::acp_runtime::InitializedPeer> {
        {
            let mut startup = self.startup.lock().unwrap();
            if startup.is_none() {
                *startup = Some(
                    self.instance
                        .coordinator
                        .foreground_startup(&self.instance.startup_key()),
                );
            }
        }
        let process = self
            .instance
            .resolve_process(&self.cwd)
            .await
            .map_err(|_| {
                failure(
                    &self.instance.instance_id,
                    "start",
                    "Could not prepare the selected ACP agent.",
                )
            })?;
        let services = self
            .instance
            .services_for(&self.cwd, json!({"mode":"readonly"}));
        let peer = ProcessPeer::spawn(process).map_err(|_| {
            failure(
                &self.instance.instance_id,
                "start",
                "Could not start the selected ACP agent.",
            )
        })?;
        *self.pending.lock().unwrap() = Some(Pending {
            peer: peer.clone(),
            services: services.clone(),
        });
        self.instance
            .initialize_only(peer, true, services, elicitation, Some(auth_initialize()))
            .await
            .map_err(|_| {
                failure(
                    &self.instance.instance_id,
                    "start",
                    "Could not initialize the selected ACP agent.",
                )
            })
    }
    async fn discover_methods(
        &self,
        mut cancel: watch::Receiver<bool>,
    ) -> AuthResult<Vec<ProviderAuthMethod>> {
        let Some(mut admission) = self
            .instance
            .coordinator
            .background_probe(&self.instance.startup_key())
        else {
            return Err(failure(
                &self.instance.instance_id,
                "methods",
                "Sign-in discovery was interrupted by an active provider session. Try again.",
            ));
        };
        let pending = Arc::new(Mutex::new(None::<Pending>));
        let resources = pending.clone();
        let discover = async {
            let version = if let Some(catalog) = &self.instance.catalog {
                let inspected = catalog
                    .inspection(&self.instance.config, &self.instance.environment)
                    .await
                    .map_err(|_| {
                        failure(
                            &self.instance.instance_id,
                            "methods",
                            "Could not inspect the selected ACP agent.",
                        )
                    })?;
                if inspected["status"] != "ready" {
                    return Err(failure(
                        &self.instance.instance_id,
                        "methods",
                        "Prepare this ACP agent before signing in.",
                    ));
                }
                inspected["version"].as_str().map(str::to_owned)
            } else {
                None
            };
            if let Some(version) = &version {
                if let Some((known, methods)) = &*self.known_methods.lock().unwrap() {
                    if known == version {
                        return Ok(methods.clone());
                    }
                }
            }
            let process = self
                .instance
                .resolve_process(&self.cwd)
                .await
                .map_err(|_| {
                    failure(
                        &self.instance.instance_id,
                        "methods",
                        "Could not prepare the selected ACP agent.",
                    )
                })?;
            let services = self
                .instance
                .services_for(&self.cwd, json!({"mode":"readonly"}));
            let peer = ProcessPeer::spawn(process).map_err(|_| {
                failure(
                    &self.instance.instance_id,
                    "methods",
                    "Could not start the selected ACP agent.",
                )
            })?;
            *resources.lock().unwrap() = Some(Pending {
                peer: peer.clone(),
                services: services.clone(),
            });
            let decline: AuthElicitation = Arc::new(|_, _| {
                Box::pin(async {
                    serde_json::from_value(json!({"action":"decline"}))
                        .map_err(|error| t3_acp::AcpError::Transport(error.to_string()))
                })
            });
            let initialized = self
                .instance
                .initialize_only(peer, true, services, Some(decline), Some(auth_initialize()))
                .await
                .map_err(|_| {
                    failure(
                        &self.instance.instance_id,
                        "methods",
                        "Could not discover this agent's sign-in methods.",
                    )
                })?;
            let methods =
                advertised_methods(&serde_json::to_value(&initialized.initialize).unwrap());
            initialized.shutdown().await;
            if let Some(version) = version {
                *self.known_methods.lock().unwrap() = Some((version, methods.clone()));
            }
            Ok(methods)
        };
        let result = tokio::select! {biased;result=discover=>result,_=cancel.wait_for(|cancel|*cancel)=>Err(failure(&self.instance.instance_id,"methods","Sign-in discovery was interrupted by an active provider session. Try again.")),_=admission.interrupted()=>Err(failure(&self.instance.instance_id,"methods","Sign-in discovery was interrupted by an active provider session. Try again.")),_=tokio::time::sleep(Duration::from_secs(30))=>Err(failure(&self.instance.instance_id,"methods","The ACP agent did not advertise sign-in methods in time."))};
        let retained = pending.lock().unwrap().take();
        if let Some(retained) = retained {
            retained.close().await;
        }
        drop(admission);
        result
    }
    async fn run_terminal(&self, method: Value, context: AuthContext) -> AuthResult<()> {
        let process = self
            .instance
            .resolve_process(&self.cwd)
            .await
            .map_err(|_| {
                failure(
                    &self.instance.instance_id,
                    "start",
                    "Could not prepare the selected ACP agent.",
                )
            })?;
        let mut args = process.args;
        if let Some(extra) = method["args"].as_array() {
            args.extend(extra.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        let mut environment: indexmap::IndexMap<String, String> = std::env::vars().collect();
        environment.extend(process.environment);
        if let Some(extra) = method["env"].as_object() {
            environment.extend(extra.iter().filter_map(|(key, value)| {
                value.as_str().map(|value| (key.clone(), value.into()))
            }));
        }
        let input = crate::terminal_process::PtySpawn {
            shell: process.binary,
            args,
            cwd: self.cwd.clone(),
            environment,
            cols: 80,
            rows: 24,
        };
        let (cancel, mut cancellation) = watch::channel(false);
        let (done, completed) = watch::channel(false);
        let (sender, result) = oneshot::channel();
        *self.terminal.lock().unwrap() = Some(MethodsJob {
            cancel,
            done: completed,
        });
        let instance = self.instance.instance_id.clone();
        tokio::spawn(async move {
            // Spawning is admitted to this owner before the blocking OS call.
            // Cancellation waits adoption, then closes/reaps that exact child.
            let result = match crate::terminal_io::TerminalProcess::spawn(
                input,
                Duration::from_millis(500),
            )
            .await
            {
                Err(_) => Err(failure(
                    &instance,
                    "start",
                    "Could not open the provider sign-in terminal.",
                )),
                Ok(mut process) => {
                    let mut transcript = String::new();
                    let mut offset = 0u64;
                    let publish = |transcript: &str, offset: u64| {
                        let io = process.io.clone();
                        let instance = instance.clone();
                        context.set_interaction(serde_json::from_value(json!({"type":"terminal","id":"terminal","output":transcript,"outputOffset":offset})).unwrap(),Some(Respond::new(Arc::new(move|response|{
                            let io=io.clone();let instance=instance.clone();Box::pin(async move{
                                if let ProviderAuthResponse::Terminal{data,size}=response{
                                    if let Some(size)=size{io.resize(size.cols.0 as u16,size.rows.0 as u16).await.map_err(|_|failure(&instance,"respond","The provider sign-in terminal is no longer available."))?;}
                                    if !data.0.is_empty(){io.write(data.0).await.map_err(|_|failure(&instance,"respond","The provider sign-in terminal is no longer available."))?;}
                                }Ok(())
                            })
                        }))),None);
                    };
                    publish(&transcript, offset);
                    let result = loop {
                        let event = tokio::select! {biased;_=cancellation.wait_for(|cancel|*cancel)=>break Err(failure(&instance,"start","Sign-in cancelled.")),event=process.events.recv()=>event};
                        match event {
                            Some(crate::terminal_process::PtyEvent::Output(data)) => {
                                let units: Vec<_> = data.encode_utf16().collect();
                                let data = String::from_utf16_lossy(
                                    &units[units.len().saturating_sub(16384)..],
                                );
                                offset += data.encode_utf16().count() as u64;
                                transcript.push_str(&data);
                                let units: Vec<_> = transcript.encode_utf16().collect();
                                transcript = String::from_utf16_lossy(
                                    &units[units.len().saturating_sub(16384)..],
                                );
                                publish(&transcript, offset);
                            }
                            Some(crate::terminal_process::PtyEvent::Exited(exit)) => {
                                break if exit.code == 0 {
                                    Ok(())
                                } else {
                                    Err(failure(
                                        &instance,
                                        "start",
                                        "The provider login command did not finish successfully.",
                                    ))
                                };
                            }
                            _ => {
                                break Err(failure(
                                    &instance,
                                    "start",
                                    "The provider login command did not finish successfully.",
                                ));
                            }
                        }
                    };
                    let _ = process.close().await;
                    result
                }
            };
            done.send_replace(true);
            let _ = sender.send(result);
        });
        result.await.map_err(|_| {
            failure(
                &self.instance.instance_id,
                "start",
                "The provider login command did not finish successfully.",
            )
        })?
    }
    async fn sign_in(&self, method_id: String, context: AuthContext) -> AuthResult<()> {
        if !self.instance.enabled {
            return Err(failure(
                &self.instance.instance_id,
                "start",
                "Enable this provider before signing in.",
            ));
        }
        let interactions = context.clone();
        let interaction: AuthElicitation = Arc::new(move |request, _| {
            let context = interactions.clone();
            Box::pin(async move {
                let request = request.as_value();
                let action = match (
                    request["mode"].as_str(),
                    request["url"].as_str(),
                    request["elicitationId"].as_str(),
                ) {
                    (Some("url"), Some(url), Some(id)) if url.encode_utf16().count() <= 2048 => {
                        let id = trim_wire_string(id);
                        let url = url::Url::parse(url)
                            .ok()
                            .filter(|url| matches!(url.scheme(), "http" | "https"));
                        if id.is_empty() || id.encode_utf16().count() > 128 || url.is_none() {
                            false
                        } else {
                            let (consent, result) = oneshot::channel();
                            let consent = Arc::new(Mutex::new(Some(consent)));
                            let browser=serde_json::from_value(json!({"type":"browser","id":id,"url":url.unwrap().as_str(),"requiresConsent":true})).unwrap();
                            context.set_interaction(
                                browser,
                                Some(Respond::new(Arc::new(move |response| {
                                    let consent = consent.clone();
                                    Box::pin(async move {
                                        if let ProviderAuthResponse::Browser { action } = response {
                                            if let Some(consent) = consent.lock().unwrap().take() {
                                                let _ = consent.send(
                                                    action == ProviderAuthBrowserAction::Accept,
                                                );
                                            }
                                        }
                                        Ok(())
                                    })
                                }))),
                                None,
                            );
                            result.await.unwrap_or(false)
                        }
                    }
                    _ => false,
                };
                serde_json::from_value(json!({"action":if action{"accept"}else{"decline"}}))
                    .map_err(|error| t3_acp::AcpError::Transport(error.to_string()))
            })
        });
        let mut initialized = self.initialize(Some(interaction.clone())).await?;
        let raw = serde_json::to_value(&initialized.initialize).unwrap();
        let method = raw["authMethods"]
            .as_array()
            .and_then(|methods| methods.iter().find(|method| method["id"] == method_id))
            .ok_or_else(|| {
                failure(
                    &self.instance.instance_id,
                    "start",
                    "The agent no longer advertises this sign-in method.",
                )
            })?;
        let terminal_method = method["type"] == "terminal";
        match method["type"].as_str().unwrap_or("agent") {
            "agent" => {
                initialized
                    .client
                    .authenticate_typed(
                        serde_json::from_value(json!({"methodId":method_id})).unwrap(),
                    )
                    .await
                    .map_err(|_| {
                        failure(
                            &self.instance.instance_id,
                            "start",
                            "The ACP agent could not complete sign-in.",
                        )
                    })?;
            }
            "env_var" => {}
            "terminal" => {
                let method = method.clone();
                initialized.shutdown().await;
                self.close_peer().await;
                self.run_terminal(method, context.clone()).await?;
                context.verifying();
                initialized = self.initialize(Some(interaction)).await.map_err(|_| {
                    failure(
                        &self.instance.instance_id,
                        "verify",
                        "The provider could not create a session after terminal sign-in.",
                    )
                })?;
            }
            _ => {
                return Err(failure(
                    &self.instance.instance_id,
                    "start",
                    "The agent no longer advertises this sign-in method.",
                ));
            }
        }
        // The same initialized process verifies agent/env_var sign-in by opening
        // a session. env_var IDs are deliberately never sent to authenticate.
        // The session is disposable and does not publish foreground chat state.
        // Context is retained by the elicitation callback, but verification is
        // also observable through this clone independently of provider callbacks.
        context.verifying();
        self.instance
            .finish_initialized(initialized, &self.cwd, None, true, false)
            .await
            .map_err(|_| {
                failure(
                    &self.instance.instance_id,
                    "verify",
                    if terminal_method {
                        "The provider could not create a session after terminal sign-in."
                    } else {
                        "The provider could not create a session after sign-in."
                    },
                )
            })?
            .shutdown()
            .await;
        self.cleanup_pending().await;
        self.authentication_changed(true).await;
        Ok(())
    }
}
impl AuthBackend for AcpAuth {
    fn methods(&self) -> BoxFuture<'static, AuthResult<Vec<ProviderAuthMethod>>> {
        let backend = self.clone();
        Box::pin(async move {
            let (cancel, cancellation) = watch::channel(false);
            let (done, completed) = watch::channel(false);
            {
                let mut jobs = backend.methods_jobs.lock().unwrap();
                jobs.retain(|job| !*job.done.borrow());
                jobs.push(MethodsJob {
                    cancel: cancel.clone(),
                    done: completed,
                });
            }
            struct Cancel(watch::Sender<bool>);
            impl Drop for Cancel {
                fn drop(&mut self) {
                    self.0.send_replace(true);
                }
            }
            let _cancel = Cancel(cancel);
            let (sender, result) = oneshot::channel();
            tokio::spawn(async move {
                let result = backend.discover_methods(cancellation).await;
                done.send_replace(true);
                let _ = sender.send(result);
            });
            result.await.map_err(|_| {
                failure(
                    "acpRegistry",
                    "methods",
                    "Could not discover this agent's sign-in methods.",
                )
            })?
        })
    }
    fn authenticate(
        &self,
        method: String,
        context: AuthContext,
    ) -> BoxFuture<'static, AuthResult<()>> {
        let backend = self.clone();
        Box::pin(async move { backend.sign_in(method, context).await })
    }
    fn cleanup(&self) -> BoxFuture<'static, ()> {
        let backend = self.clone();
        Box::pin(async move { backend.cleanup_pending().await })
    }
    fn cleanup_methods(&self) -> BoxFuture<'static, ()> {
        // A cancelled caller signals its own discovery job via its guard.
        // Await those jobs without cancelling another concurrent refresh/start.
        let jobs = {
            let mut admitted = self.methods_jobs.lock().unwrap();
            let all = std::mem::take(&mut *admitted);
            let (cancelled, active): (Vec<_>, Vec<_>) = all
                .into_iter()
                .partition(|job| *job.cancel.borrow() || *job.done.borrow());
            *admitted = active;
            cancelled
        };
        Box::pin(async move {
            for job in jobs {
                job.cancel.send_replace(true);
                let mut done = job.done;
                if !*done.borrow() {
                    let _ = done.wait_for(|done| *done).await;
                }
            }
        })
    }
    fn logout(&self) -> BoxFuture<'static, AuthResult<Option<String>>> {
        let backend = self.clone();
        Box::pin(async move {
            let result=tokio::time::timeout(Duration::from_secs(60),async{
            let initialized=backend.initialize(None).await?;let raw=serde_json::to_value(&initialized.initialize).unwrap();
            if raw["agentCapabilities"]["auth"]["logout"].is_null(){return Err(failure(&backend.instance.instance_id,"logout","This ACP agent could not sign out. It may not advertise logout support."));}
            initialized.client.logout_typed(serde_json::from_value(json!({})).unwrap()).await.map_err(|_|failure(&backend.instance.instance_id,"logout","This ACP agent could not sign out. It may not advertise logout support."))?;
            initialized.shutdown().await;Ok(None)
        }).await.unwrap_or_else(|_|Err(failure(&backend.instance.instance_id,"logout","The ACP agent did not finish signing out in time.")));
            backend.cleanup_pending().await;
            if result.is_ok() {
                backend.authentication_changed(false).await;
            }
            result
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    async fn fixture(
        version: u8,
        environment: HashMap<String, String>,
    ) -> (tempfile::TempDir, AcpAuth) {
        let initialize = serde_json::to_value(auth_initialize()).unwrap();
        t3_acp::schema::decode("v1.InitializeRequest", initialize.clone()).unwrap();
        assert_eq!(initialize["protocolVersion"], 2);
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("agent.log");
        let config:AcpRegistrySettings=serde_json::from_value(json!({"source":"local","enabled":true,"commandPath":"python3","commandArgs":[format!("{}/tests/fixtures/acp-auth-provider.py",env!("CARGO_MANIFEST_DIR")),log,version.to_string()]})).unwrap();
        let instance = AcpInstance {
            instance_id: "auth-fixture".into(),
            display_name: "Auth fixture".into(),
            accent_color: None,
            enabled: true,
            config: config.clone(),
            environment: environment.clone(),
            catalog: None,
            coordinator: Default::default(),
        };
        let confirmation = AuthenticationState::open(
            root.path().join("caches"),
            &instance.instance_id,
            &config,
            &[],
            &environment,
        )
        .await;
        let backend = AcpAuth::new(instance, root.path().into(), confirmation);
        (root, backend)
    }
    async fn next_phase(
        states: &mut crate::provider_auth_flow::AuthSubscription,
        phase: ProviderAuthPhase,
    ) -> ProviderAuthState {
        loop {
            let state = states.recv().await.unwrap();
            if state.phase == phase {
                return state;
            }
            assert_ne!(
                state.phase,
                ProviderAuthPhase::Failed,
                "unexpected failure awaiting {phase:?}: {:?}",
                state.message
            );
        }
    }
    async fn initial_methods(flow: &AuthFlow) {
        let mut states = flow.subscribe("owner".into());
        loop {
            let state = states.recv().await.unwrap();
            if let Some(methods) = state.methods {
                assert_eq!(methods.0.len(), 3, "{:?}", state.message);
                return;
            }
        }
    }
    fn noop() -> crate::provider_auth_flow::StopSession {
        Arc::new(|| Box::pin(async {}))
    }
    fn logs(root: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(root.join("agent.log"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn reaped(root: &std::path::Path) {
        for pid in logs(root).iter().filter_map(|row| row["pid"].as_i64()) {
            assert_eq!(
                unsafe { libc::kill(pid as i32, 0) },
                -1,
                "owned child {pid} still present"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
    #[tokio::test]
    async fn real_v1_and_v2_agent_login_requires_owner_consent_verifies_persists_and_logs_out() {
        tokio::time::timeout(Duration::from_secs(15),async{
        for version in [1,2]{
            let(root,backend)=fixture(version,HashMap::new()).await;let flow=backend.controller();initial_methods(&flow).await;
            assert!(!backend.confirmation.get());assert!(logs(root.path()).iter().all(|row|row["method"]=="initialize"),"method discovery creates no session or login");reaped(root.path());
            let mut states=flow.subscribe("owner".into());let start=flow.start("owner".into(),Some("agent".into()),None,None,noop()).await.unwrap();
            let waiting=next_phase(&mut states,ProviderAuthPhase::Waiting).await;assert_eq!(waiting.authorization_url.as_deref(),Some("https://example.test/login"));
            let input:ProviderAuthRespondInput=serde_json::from_value(json!({"instanceId":"auth-fixture","flowId":start.flow_id.unwrap(),"interactionId":"fixture-consent","response":{"type":"browser","action":"accept"}})).unwrap();
            assert!(flow.respond("other",input.clone()).await.is_err());flow.respond("owner",input).await.unwrap();
            next_phase(&mut states,ProviderAuthPhase::Succeeded).await;assert!(backend.confirmation.get());reaped(root.path());
            assert_eq!(flow.logout(noop()).await.unwrap().phase,ProviderAuthPhase::Idle);assert!(!backend.confirmation.get());reaped(root.path());flow.shutdown().await;
        }
    }).await.unwrap();
    }
    #[tokio::test]
    async fn real_configured_environment_verifies_without_sending_env_var_id_to_authenticate() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (root, backend) = fixture(
                2,
                HashMap::from([("FIXTURE_TOKEN".into(), "configured-only".into())]),
            )
            .await;
            let flow = backend.controller();
            initial_methods(&flow).await;
            let mut states = flow.subscribe("owner".into());
            flow.start(
                "owner".into(),
                Some("credentials".into()),
                None,
                None,
                noop(),
            )
            .await
            .unwrap();
            next_phase(&mut states, ProviderAuthPhase::Succeeded).await;
            assert!(backend.confirmation.get());
            assert!(
                !logs(root.path())
                    .iter()
                    .any(|row| row["method"] == "auth/login" || row["method"] == "authenticate")
            );
            reaped(root.path());
            flow.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn real_terminal_login_receives_input_then_reconnects_before_verifying() {
        tokio::time::timeout(Duration::from_secs(15),async{
        let(root,backend)=fixture(2,HashMap::new()).await;let flow=backend.controller();initial_methods(&flow).await;let mut states=flow.subscribe("owner".into());
        let start=flow.start("owner".into(),Some("terminal".into()),None,None,noop()).await.unwrap();
        loop{let state=states.recv().await.unwrap();if matches!(&state.interaction,Some(Some(Some(ProviderAuthInteraction::Terminal{output,..}))) if output.0.contains("Enter fixture code:")){break;}assert_ne!(state.phase,ProviderAuthPhase::Failed,"{:?}",state.message);}
        flow.respond("owner",serde_json::from_value(json!({"instanceId":"auth-fixture","flowId":start.flow_id.unwrap(),"interactionId":"terminal","response":{"type":"terminal","data":"fixture-code\n","size":{"cols":100,"rows":30}}})).unwrap()).await.unwrap();
        next_phase(&mut states,ProviderAuthPhase::Succeeded).await;assert!(backend.confirmation.get());let log=logs(root.path());let login=log.iter().position(|row|row["method"]=="terminal-login").unwrap();assert!(log[login+1..].iter().any(|row|row["method"]=="initialize"));assert!(log[login+1..].iter().any(|row|row["method"]=="session/new"));reaped(root.path());flow.shutdown().await;
    }).await.unwrap();
    }
    #[tokio::test]
    async fn real_held_initialization_and_login_are_reaped_before_controller_shutdown_or_cancel() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let socket_root = tempfile::tempdir().unwrap();
            let path = socket_root.path().join("milestones.sock");
            let socket = tokio::net::UnixDatagram::bind(&path).unwrap();
            let mut bytes = [0; 4096];
            let (root, backend) = fixture(
                2,
                HashMap::from([
                    ("AUTH_SCENARIO".into(), "held-initialize".into()),
                    ("AUTH_SOCKET".into(), path.to_string_lossy().into()),
                ]),
            )
            .await;
            let flow = backend.controller();
            socket.recv(&mut bytes).await.unwrap();
            flow.shutdown().await;
            reaped(root.path());
            assert!(!backend.confirmation.get());
            let (root, backend) = fixture(
                2,
                HashMap::from([
                    ("AUTH_SCENARIO".into(), "held-login".into()),
                    ("AUTH_SOCKET".into(), path.to_string_lossy().into()),
                ]),
            )
            .await;
            let flow = backend.controller();
            initial_methods(&flow).await;
            let start = flow
                .start("owner".into(), Some("agent".into()), None, None, noop())
                .await
                .unwrap();
            loop {
                let n = socket.recv(&mut bytes).await.unwrap();
                let signal: Value = serde_json::from_slice(&bytes[..n]).unwrap();
                if signal["method"] == "auth/login" {
                    break;
                }
            }
            let cancelled = flow
                .cancel("owner", start.flow_id.unwrap().0.as_str())
                .await
                .unwrap();
            assert_eq!(cancelled.phase, ProviderAuthPhase::Cancelled);
            reaped(root.path());
            assert!(!backend.confirmation.get());
            flow.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn real_pending_url_consent_cancels_and_declined_consent_never_confirms_sign_in() {
        tokio::time::timeout(Duration::from_secs(15),async{
        for decline in [false,true]{
            let(root,backend)=fixture(2,HashMap::new()).await;let flow=backend.controller();initial_methods(&flow).await;let mut states=flow.subscribe("owner".into());
            let start=flow.start("owner".into(),Some("agent".into()),None,None,noop()).await.unwrap();next_phase(&mut states,ProviderAuthPhase::Waiting).await;
            if decline {
                flow.respond("owner",serde_json::from_value(json!({"instanceId":"auth-fixture","flowId":start.flow_id.unwrap(),"interactionId":"fixture-consent","response":{"type":"browser","action":"decline"}})).unwrap()).await.unwrap();
                let failed=next_phase(&mut states,ProviderAuthPhase::Failed).await;assert_eq!(failed.message.as_deref(),Some("The ACP agent could not complete sign-in."));
            }else{
                assert_eq!(flow.cancel("owner",start.flow_id.unwrap().0.as_str()).await.unwrap().phase,ProviderAuthPhase::Cancelled);
            }
            assert!(!backend.confirmation.get());assert!(!root.path().join("agent.log.credentials").exists());reaped(root.path());assert!(!flow.is_changing_credentials());flow.shutdown().await;
        }
    }).await.unwrap();
    }
}
