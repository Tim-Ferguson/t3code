//! Owned ACP sessions over the shared subprocess peer. No JavaScript runtime.
use crate::{acp_peer::ProcessPeer, provider_process::ProcessOptions};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_acp::{AcpError, Client, ClientOptions, RequestContext, types::*};
use t3_contracts::{AcpRegistrySettings, AcpRegistrySettingsSource};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone, Debug)]
pub struct AcpInstance {
    pub instance_id: String,
    pub display_name: String,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub config: AcpRegistrySettings,
    pub environment: HashMap<String, String>,
    pub catalog: Option<crate::acp_registry_support::Catalog>,
    pub coordinator: crate::acp_coordinator::Coordinator,
}
pub enum SessionEvent {
    Update(SessionNotification),
    Permission {
        context: RequestContext,
        request: RequestPermissionRequest,
        response: oneshot::Sender<Result<t3_acp::v2::RequestPermissionResponse, AcpError>>,
        written: oneshot::Receiver<Result<(), AcpError>>,
    },
    Terminated(AcpError),
}
pub struct AcpSession {
    pub client: Client,
    pub initialize: InitializeResponse,
    /// Latest setup state, including buffered root metadata and acknowledged mutations.
    pub setup: NewSessionResponse,
    /// Immutable session/new or session/load reply, matching source start().sessionSetupResult.
    pub setup_response: NewSessionResponse,
    pub events: mpsc::UnboundedReceiver<SessionEvent>,
    peer: ProcessPeer,
    coordinator: crate::acp_coordinator::Coordinator,
    instance_id: String,
    live_setup: Arc<Mutex<Value>>,
    commands: watch::Receiver<Option<crate::acp_coordinator::AvailableCommands>>,
    pub(crate) services: crate::acp_client_callbacks::Services,
}
pub(crate) type AuthElicitation = Arc<
    dyn Fn(
            t3_acp::v2::CreateElicitationRequest,
            RequestContext,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<t3_acp::v2::CreateElicitationResponse, AcpError>,
        > + Send
        + Sync,
>;
pub(crate) struct InitializedPeer {
    mcp_servers: Vec<t3_acp::types::McpServer>,
    pub(crate) client: Client,
    pub(crate) initialize: InitializeResponse,
    receiver: mpsc::UnboundedReceiver<SessionEvent>,
    peer: Arc<ProcessPeer>,
    services: crate::acp_client_callbacks::Services,
    live_setup: Arc<Mutex<Value>>,
    startup: Arc<Mutex<StartupMetadata>>,
    command_sender: watch::Sender<Option<crate::acp_coordinator::AvailableCommands>>,
    command_receiver: watch::Receiver<Option<crate::acp_coordinator::AvailableCommands>>,
    startup_events: mpsc::UnboundedSender<SessionEvent>,
}
impl InitializedPeer {
    pub(crate) async fn shutdown(&self) {
        self.services.shutdown().await;
        self.client.shutdown();
        self.peer.shutdown().await;
    }
}
#[derive(Default)]
struct StartupMetadata {
    root: Option<String>,
    pending: Vec<SessionNotification>,
}
fn apply_live_notification(
    notification: &SessionNotification,
    setup: &Arc<Mutex<Value>>,
    commands: &watch::Sender<Option<crate::acp_coordinator::AvailableCommands>>,
    coordinator: &crate::acp_coordinator::Coordinator,
    instance: &str,
    discovery: bool,
) {
    let raw = serde_json::to_value(notification).unwrap();
    let update = &raw["update"];
    match update["sessionUpdate"].as_str() {
        Some("available_commands_update") => {
            let advertised = crate::acp_model::available_commands(&update["availableCommands"]);
            commands.send_replace(Some(advertised.clone()));
            if !discovery {
                coordinator.publish_commands(instance, advertised);
            }
        }
        Some("config_option_update") => {
            let mut setup = setup.lock().unwrap();
            setup["configOptions"] = update["configOptions"].clone();
            // Config mode options are authoritative; legacy modes otherwise remain.
            if let Some(modes) = crate::acp_model::session_mode_state(
                &json!({"configOptions":update["configOptions"]}),
            ) {
                setup["modes"] = modes;
            }
            if !discovery {
                coordinator
                    .publish_configuration(instance, crate::acp_model::live_configuration(&setup));
            }
        }
        Some("current_mode_update") => {
            let mut setup = setup.lock().unwrap();
            let mode =
                t3_contracts::trim_wire_string(update["currentModeId"].as_str().unwrap_or(""));
            if !mode.is_empty() && setup["modes"].is_object() {
                setup["modes"]["currentModeId"] = json!(mode);
                if !discovery {
                    coordinator.publish_configuration(
                        instance,
                        crate::acp_model::live_configuration(&setup),
                    );
                }
            }
        }
        _ => {}
    }
}
impl Drop for AcpSession {
    fn drop(&mut self) {
        self.client.shutdown();
    }
}
impl AcpSession {
    pub async fn shutdown(&self) {
        self.client.shutdown();
        self.services.shutdown().await;
        self.peer.shutdown().await;
    }
    pub async fn set_model(&mut self, model: &str) -> Result<(), AcpError> {
        let setup = self.live_setup.lock().unwrap().clone();
        if let Some(option) = setup["configOptions"].as_array().and_then(|options| {
            options
                .iter()
                .find(|option| option["category"] == "model" && option["type"] == "select")
        }) {
            if option["currentValue"] != model {
                let result = self
                    .client
                    .set_session_config_option_typed(SetSessionConfigOptionRequest {
                        session_id: self.setup.session_id.clone(),
                        config_id: option["id"].as_str().unwrap().into(),
                        value: SessionConfigValue::Id(model.into()),
                        meta: Optional::Missing,
                    })
                    .await?;
                self.live_setup.lock().unwrap()["configOptions"] =
                    serde_json::to_value(result.config_options).unwrap();
            }
        } else if setup["models"].is_object() {
            if setup["models"]["currentModelId"] != model {
                self.client
                    .set_session_model_typed(SetSessionModelRequest {
                        session_id: self.setup.session_id.clone(),
                        model_id: model.into(),
                        meta: Optional::Missing,
                    })
                    .await?;
                self.live_setup.lock().unwrap()["models"]["currentModelId"] = json!(model);
            }
        } else if !model.is_empty() && model != "default" {
            return Err(AcpError::Transport(
                "This ACP agent exposes no model selection API.".into(),
            ));
        }
        self.refresh_live_setup()?;

        Ok(())
    }
    pub async fn set_options(
        &mut self,
        options: &serde_json::Map<String, Value>,
    ) -> Result<(), AcpError> {
        for (id, value) in options {
            if id == "_t3/session-mode" {
                let mode = value.as_str().ok_or_else(|| {
                    AcpError::Transport("ACP session mode must be a string.".into())
                })?;
                let request = serde_json::from_value(
                    json!({"sessionId":self.setup.session_id,"modeId":mode}),
                )
                .map_err(|error| AcpError::Transport(error.to_string()))?;
                self.client.set_session_mode_typed(request).await?;
                let mut setup = self.live_setup.lock().unwrap();
                if !setup["modes"].is_object() {
                    setup["modes"] = json!({});
                }
                setup["modes"]["currentModeId"] = json!(mode);
                continue;
            }
            let setup = self.live_setup.lock().unwrap().clone();
            let option = setup["configOptions"]
                .as_array()
                .and_then(|options| options.iter().find(|option| option["id"] == *id))
                .ok_or_else(|| {
                    AcpError::Transport(format!("ACP config option '{id}' is unavailable."))
                })?;
            let value = match (option["type"].as_str(), value) {
                (Some("boolean"), Value::Bool(value)) => SessionConfigValue::Boolean(*value),
                (Some("select"), Value::String(value)) => SessionConfigValue::Id(value.clone()),
                _ => {
                    return Err(AcpError::Transport(format!(
                        "Invalid value for ACP config option '{id}'."
                    )));
                }
            };
            let result = self
                .client
                .set_session_config_option_typed(SetSessionConfigOptionRequest {
                    session_id: self.setup.session_id.clone(),
                    config_id: id.clone(),
                    value,
                    meta: Optional::Missing,
                })
                .await?;
            self.live_setup.lock().unwrap()["configOptions"] =
                serde_json::to_value(result.config_options).unwrap();
        }
        self.refresh_live_setup()?;
        Ok(())
    }
    fn refresh_live_setup(&mut self) -> Result<(), AcpError> {
        let setup = self.live_setup.lock().unwrap().clone();
        self.setup = serde_json::from_value(setup.clone())
            .map_err(|error| AcpError::Transport(error.to_string()))?;
        self.coordinator.publish_configuration(
            &self.instance_id,
            crate::acp_model::live_configuration(&setup),
        );
        Ok(())
    }
}
impl AcpInstance {
    pub fn startup_key(&self) -> String {
        if self.config.source == AcpRegistrySettingsSource::Local {
            format!("local:{}", self.instance_id)
        } else {
            self.config.agent_id.to_string()
        }
    }
    pub fn process_options(&self, cwd: &Path) -> Result<ProcessOptions, AcpError> {
        if self.config.source != AcpRegistrySettingsSource::Local {
            return Err(AcpError::Transport(
                "ACP registry distribution resolution is not yet available in this native build."
                    .into(),
            ));
        }
        let command = self.config.command_path.as_str();
        if command.is_empty() {
            return Err(AcpError::Transport(
                "The local ACP command path is empty.".into(),
            ));
        }
        let binary = if command == "~" || command.starts_with("~/") || command.starts_with("~\\") {
            let home = self
                .environment
                .get("HOME")
                .cloned()
                .or_else(|| std::env::var("HOME").ok())
                .or_else(|| std::env::var("USERPROFILE").ok())
                .ok_or_else(|| AcpError::Transport("The home directory is unavailable.".into()))?;
            std::path::PathBuf::from(home).join(command.get(2..).unwrap_or(""))
        } else {
            command.into()
        };
        Ok(ProcessOptions {
            binary,
            args: self.config.command_args.clone(),
            cwd: cwd.into(),
            environment: self.environment.clone(),
        })
    }
    pub async fn resolve_process(&self, cwd: &Path) -> Result<ProcessOptions, AcpError> {
        if let Some(catalog) = &self.catalog {
            catalog
                .resolve(&self.config, cwd, &self.environment)
                .await
                .map_err(|error| AcpError::Transport(error.to_string()))
        } else {
            self.process_options(cwd)
        }
    }
    pub async fn connect(
        &self,
        cwd: &Path,
        saved_session: Option<&str>,
        discovery: bool,
    ) -> Result<AcpSession, AcpError> {
        if !self.enabled {
            return Err(AcpError::Transport(
                "The ACP provider instance is disabled.".into(),
            ));
        }
        let _startup =
            (!discovery).then(|| self.coordinator.foreground_startup(&self.startup_key()));
        let peer = ProcessPeer::spawn(self.resolve_process(cwd).await?)?;
        self.start_peer(peer, cwd, saved_session, discovery).await
    }
    pub(crate) async fn start_peer(
        &self,
        peer: ProcessPeer,
        cwd: &Path,
        saved_session: Option<&str>,
        discovery: bool,
    ) -> Result<AcpSession, AcpError> {
        self.start_peer_with_policy(
            peer,
            cwd,
            saved_session,
            discovery,
            crate::acp_client_callbacks::policy(&json!("approval-required"), cwd),
        )
        .await
    }
    pub(crate) async fn start_peer_with_policy(
        &self,
        peer: ProcessPeer,
        cwd: &Path,
        saved_session: Option<&str>,
        discovery: bool,
        policy: Value,
    ) -> Result<AcpSession, AcpError> {
        self.start_peer_with_services(
            peer,
            cwd,
            saved_session,
            discovery,
            self.services_for(cwd, policy),
        )
        .await
    }
    pub(crate) fn services_for(
        &self,
        cwd: &Path,
        policy: Value,
    ) -> crate::acp_client_callbacks::Services {
        // Local agentId never selects registry-only exceptions.
        let agent_id = if self.config.source == AcpRegistrySettingsSource::Registry {
            self.config.agent_id.as_str()
        } else {
            ""
        };
        crate::acp_client_callbacks::Services::new(
            agent_id,
            cwd,
            self.environment
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            policy,
        )
    }
    pub(crate) async fn start_peer_with_services(
        &self,
        peer: ProcessPeer,
        cwd: &Path,
        saved_session: Option<&str>,
        discovery: bool,
        services: crate::acp_client_callbacks::Services,
    ) -> Result<AcpSession, AcpError> {
        let retained = services.clone();
        let result = self
            .initialize_peer(peer, cwd, saved_session, discovery, services)
            .await;
        if result.is_err() {
            retained.shutdown().await;
        }
        result
    }
    pub(crate) async fn start_peer_with_mcp(
        &self,
        peer: ProcessPeer,
        cwd: &Path,
        saved_session: Option<&str>,
        services: crate::acp_client_callbacks::Services,
        mcp: &crate::provider_mcp::CredentialLease,
    ) -> Result<AcpSession, AcpError> {
        let retained = services.clone();
        let result = async {
            let mut initialized = self
                .initialize_only(peer, false, services, None, None)
                .await?;
            initialized.mcp_servers = vec![mcp.stdio_server()];
            self.finish_initialized(initialized, cwd, saved_session, false, true)
                .await
        }
        .await;
        if result.is_err() {
            retained.shutdown().await;
        }
        result
    }
    pub(crate) async fn start_resolved_peer(
        &self,
        peer: ProcessPeer,
        cwd: &Path,
        saved_session: Option<&str>,
        discovery: bool,
        registry_agent_id: &str,
        policy: Value,
    ) -> Result<AcpSession, AcpError> {
        let services = crate::acp_client_callbacks::Services::new(
            registry_agent_id,
            cwd,
            self.environment
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            policy,
        );
        self.start_peer_with_services(peer, cwd, saved_session, discovery, services)
            .await
    }
    async fn initialize_peer(
        &self,
        peer: ProcessPeer,
        cwd: &Path,
        saved_session: Option<&str>,
        discovery: bool,
        services: crate::acp_client_callbacks::Services,
    ) -> Result<AcpSession, AcpError> {
        let initialized = self
            .initialize_only(peer, discovery, services, None, None)
            .await?;
        self.finish_initialized(initialized, cwd, saved_session, discovery, !discovery)
            .await
    }
    pub(crate) async fn initialize_only(
        &self,
        peer: ProcessPeer,
        discovery: bool,
        services: crate::acp_client_callbacks::Services,
        elicitation: Option<AuthElicitation>,
        initialize_request: Option<InitializeRequest>,
    ) -> Result<InitializedPeer, AcpError> {
        let peer = Arc::new(peer);
        // Source AcpSessionRuntime uses an unbounded event queue, including load
        // replay received before setup finishes. Do not deadlock setup on a cap.
        let (events, receiver) = mpsc::unbounded_channel();
        let terminated = events.clone();
        type Acknowledgements = Arc<Mutex<HashMap<String, oneshot::Sender<Result<(), AcpError>>>>>;
        let acknowledgements: Acknowledgements = Arc::new(Mutex::new(HashMap::new()));
        let terminal_acks = acknowledgements.clone();
        let successful_acks = acknowledgements.clone();
        let failed_acks = acknowledgements.clone();
        let client = Client::with_options(
            peer.clone(),
            Duration::from_secs(90),
            ClientOptions {
                on_termination: Some(Arc::new(move |error| {
                    let events = terminated.clone();
                    let acknowledgements = terminal_acks.clone();
                    Box::pin(async move {
                        for (_, ack) in acknowledgements.lock().unwrap().drain() {
                            let _ = ack.send(Err(error.clone()));
                        }
                        let _ = events.send(SessionEvent::Terminated(error));
                    })
                })),
                on_outgoing_response: Some(Arc::new(move |id| {
                    let acknowledgements = successful_acks.clone();
                    Box::pin(async move {
                        if let Some(ack) = acknowledgements.lock().unwrap().remove(&id) {
                            let _ = ack.send(Ok(()));
                        }
                    })
                })),
                on_outgoing_response_failure: Some(Arc::new(move |id, error| {
                    let acknowledgements = failed_acks.clone();
                    Box::pin(async move {
                        if let Some(ack) = acknowledgements.lock().unwrap().remove(&id) {
                            let _ = ack.send(Err(error));
                        }
                    })
                })),
                ..Default::default()
            },
        );
        services.register(&client);
        let update_services = services.clone();
        let updates = events.clone();
        let startup_events = events.clone();
        let coordinator = self.coordinator.clone();
        let instance_id = self.instance_id.clone();
        let live_setup = Arc::new(Mutex::new(json!({})));
        let update_setup = live_setup.clone();
        let startup = Arc::new(Mutex::new(StartupMetadata::default()));
        let update_startup = startup.clone();
        let (command_sender, command_receiver) = watch::channel(None);
        let update_commands = command_sender.clone();
        client
            .handle_session_update(Arc::new(move |update| {
                let updates = updates.clone();
                let services = update_services.clone();
                let coordinator = coordinator.clone();
                let instance_id = instance_id.clone();
                let setup = update_setup.clone();
                let command_sender = update_commands.clone();
                let startup=update_startup.clone();
                Box::pin(async move {
                    let update:SessionNotification = serde_json::from_value(
                        services.resolve_update(serde_json::to_value(update).unwrap()),
                    )
                    .map_err(|error| {
                        AcpError::Transport(format!(
                            "Invalid embedded ACP terminal update: {error}"
                        ))
                    })?;
                    let mut startup=startup.lock().unwrap();
                    let Some(root)=startup.root.as_deref() else {
                        let raw=serde_json::to_value(&update).unwrap();
                        let tag=raw["update"]["sessionUpdate"].as_str().unwrap_or("");
                        if matches!(tag,"config_option_update"|"current_mode_update"|"available_commands_update"){
                            startup.pending.retain(|previous|{
                                previous.session_id!=update.session_id ||
                                serde_json::to_value(previous).unwrap()["update"]["sessionUpdate"]!=tag
                            });
                            startup.pending.push(update);
                            if startup.pending.len()>32 {startup.pending.remove(0);}
                        }
                        return Ok(());
                    };
                    if update.session_id!=root{return Ok(());}
                    apply_live_notification(&update,&setup,&command_sender,&coordinator,&instance_id,discovery);
                    updates
                        .send(SessionEvent::Update(update))
                        .map_err(|_| AcpError::Closed)
                })
            }))
            .await;
        client.handle_request_permission(Arc::new(move |request, context| {
            let events = events.clone();
            let acknowledgements = acknowledgements.clone();
            Box::pin(async move {
                if discovery {
                    return permission_response(&request, "cancel");
                }
                let (response, result) = oneshot::channel();
                let (ack, written) = oneshot::channel();
                acknowledgements
                    .lock()
                    .unwrap()
                    .insert(context.request_id.clone(), ack);
                events
                    .send(SessionEvent::Permission {
                        context,
                        request,
                        response,
                        written,
                    })
                    .map_err(|_| AcpError::Closed)?;
                result.await.map_err(|_| AcpError::Closed)?
            })
        }));
        let coordinator = self.coordinator.clone();
        let instance_id = self.instance_id.clone();
        client.handle_elicitation(Arc::new(move |request, _context| {
            let coordinator = coordinator.clone();
            let instance_id = instance_id.clone();
            Box::pin(async move {
                let request = request.as_value();
                let action = (|| {
                    if request["mode"] != "url" {
                        return None;
                    }
                    let url = request["url"].as_str()?;
                    let id = request["elicitationId"].as_str()?;
                    if url.encode_utf16().count() > 2048
                        || id.is_empty()
                        || id != t3_contracts::trim_wire_string(id)
                        || id.encode_utf16().count() > 128
                    {
                        return None;
                    }
                    let url = url::Url::parse(url).ok()?;
                    if !matches!(url.scheme(), "http" | "https") {
                        return None;
                    }
                    let message =
                        t3_contracts::trim_wire_string(request["message"].as_str().unwrap_or(""));
                    let message = String::from_utf16_lossy(
                        &message.encode_utf16().take(1024).collect::<Vec<_>>(),
                    );
                    serde_json::from_value(
                        json!({"elicitationId":id,"url":url.as_str(),"message":message}),
                    )
                    .ok()
                })();
                let accepted = match action {
                    Some(action) => {
                        coordinator
                            .request_url_authentication(&instance_id, action)
                            .await
                    }
                    None => false,
                };
                serde_json::from_value(json!({"action":if accepted{"accept"}else{"decline"}}))
                    .map_err(|error| AcpError::Transport(error.to_string()))
            })
        }));
        if let Some(elicitation) = elicitation {
            client.handle_elicitation(elicitation);
        }
        // Capabilities describe only handlers implemented by this bridge. File,
        // terminal, elicitation and MCP callback support are added with services.
        let initialize = client
            .initialize_typed(initialize_request.unwrap_or_else(|| InitializeRequest {
                client_info: Optional::Value(Implementation {
                    name: "t3-code".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    title: Optional::Value("T3 Code".into()),
                    meta: Optional::Missing,
                }),
                client_capabilities: Some(ClientCapabilities {
                    terminal: services.has_terminals().then_some(true),
                    elicitation: Optional::Value(
                        serde_json::from_value(json!({"url":{}})).unwrap(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await?;
        Ok(InitializedPeer {
            client,
            initialize,
            receiver,
            peer,
            services,
            live_setup,
            startup,
            command_sender,
            command_receiver,
            startup_events,
            mcp_servers: vec![],
        })
    }
    pub(crate) async fn finish_initialized(
        &self,
        initialized: InitializedPeer,
        cwd: &Path,
        saved_session: Option<&str>,
        discovery: bool,
        authenticate_configured: bool,
    ) -> Result<AcpSession, AcpError> {
        let InitializedPeer {
            client,
            initialize,
            receiver,
            peer,
            services,
            live_setup,
            startup,
            command_sender,
            command_receiver,
            startup_events,
            mcp_servers,
        } = initialized;
        let setup_session = || async {
            let setup = if let Some(session_id) = saved_session {
                let initialized = serde_json::to_value(&initialize).unwrap();
                if initialized["agentCapabilities"]["loadSession"] != true {
                    return Err(AcpError::Transport(
                        "This ACP agent cannot load the saved session.".into(),
                    ));
                }
                let result = client
                    .load_session_typed(LoadSessionRequest {
                        session_id: session_id.into(),
                        cwd: cwd.to_string_lossy().into(),
                        additional_directories: None,
                        mcp_servers: Some(mcp_servers.clone()),
                        meta: Optional::Missing,
                    })
                    .await?;
                NewSessionResponse {
                    session_id: session_id.into(),
                    models: result.models,
                    modes: result.modes,
                    config_options: result.config_options,
                    meta: result.meta,
                }
            } else {
                client
                    .create_session_typed(NewSessionRequest {
                        cwd: cwd.to_string_lossy().into(),
                        additional_directories: None,
                        mcp_servers: mcp_servers.clone(),
                        meta: Optional::Missing,
                    })
                    .await?
            };
            Ok::<_, AcpError>(setup)
        };
        let setup = match setup_session().await {
            Err(required) if authenticate_configured && authentication_required(&required) => {
                let configured =
                    t3_contracts::trim_wire_string(self.config.auth_method_id.as_str());
                let methods = initialize.auth_methods.as_ref();
                let selected = methods.and_then(|methods| {
                    methods.iter().find(|method| {
                        let value =
                            serde_json::to_value(method).expect("typed authentication method");
                        if !configured.is_empty() {
                            value["id"] == configured
                        } else {
                            value.get("type").is_none() || value["type"] == "agent"
                        }
                    })
                });
                if !configured.is_empty() && methods.is_some() && selected.is_none() {
                    return Err(AcpError::Transport(format!(
                        "ACP agent did not advertise configured authentication method \"{configured}\""
                    )));
                }
                let selected = selected.map(|method| serde_json::to_value(method).unwrap());
                if let Some(method) = &selected {
                    if let Some(kind) = method.get("type").and_then(Value::as_str) {
                        if kind != "agent" {
                            return Err(AcpError::Transport(format!(
                                "ACP authentication method \"{}\" requires {kind} authentication, which cannot run inside a headless provider session",
                                method["id"].as_str().unwrap()
                            )));
                        }
                    }
                }
                let method = selected
                    .as_ref()
                    .and_then(|method| method["id"].as_str())
                    .unwrap_or(configured);
                if method.is_empty() {
                    return Err(required);
                }
                client
                    .authenticate_typed(
                        serde_json::from_value(json!({"methodId":method}))
                            .expect("selected ACP method"),
                    )
                    .await?;
                // Retry exactly once. A second AuthRequired is returned to the caller.
                setup_session().await?
            }
            result => result?,
        };
        if setup.session_id.is_empty() {
            return Err(AcpError::Transport(
                "ACP returned an empty session ID.".into(),
            ));
        }
        {
            let mut startup = startup.lock().unwrap();
            let mut seeded = serde_json::to_value(&setup).unwrap();
            if let Some(modes) = crate::acp_model::session_mode_state(&seeded) {
                seeded["modes"] = modes;
            }
            *live_setup.lock().unwrap() = seeded;
            startup.root = Some(setup.session_id.clone());
            // Source runtime seeds the response, then replays the latest startup
            // metadata per root-session/tag under notification admission.
            for update in std::mem::take(&mut startup.pending) {
                if update.session_id == setup.session_id {
                    apply_live_notification(
                        &update,
                        &live_setup,
                        &command_sender,
                        &self.coordinator,
                        &self.instance_id,
                        discovery,
                    );
                    let _ = startup_events.send(SessionEvent::Update(update));
                }
            }
        }
        let setup_response = setup;
        let setup = serde_json::from_value(live_setup.lock().unwrap().clone())
            .map_err(|error| AcpError::Transport(error.to_string()))?;
        if !discovery {
            self.coordinator.publish_configuration(
                &self.instance_id,
                crate::acp_model::live_configuration(&live_setup.lock().unwrap()),
            );
        }
        Ok(AcpSession {
            coordinator: self.coordinator.clone(),
            instance_id: self.instance_id.clone(),
            live_setup,
            commands: command_receiver,
            client,
            initialize,
            setup,
            setup_response,
            events: receiver,
            peer: peer.as_ref().clone(),
            services,
        })
    }
    pub(crate) fn health_input(&self, inspection: Value) -> Value {
        let mut settings = serde_json::to_value(&self.config).expect("typed ACP settings");
        settings["enabled"] = json!(self.enabled);
        json!({"instanceId":self.instance_id,"displayName":self.display_name,"accentColor":self.accent_color,"continuationKey":format!("acpRegistry:instance:{}",self.instance_id),"settings":settings,"inspection":inspection,"checkedAt":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true)})
    }
    async fn inspect_health(&self) -> Value {
        if let Some(catalog) = &self.catalog {
            return catalog.inspection(&self.config,&self.environment).await.unwrap_or_else(|error|json!({"status":"failed","message":format!("Could not inspect ACP Registry agent: {}",error.detail)}));
        }
        if self.config.source == AcpRegistrySettingsSource::Local {
            json!({"status":if self.config.command_path.as_str().is_empty(){"unconfigured"}else if crate::acp_registry_support::executable(self.config.command_path.as_str(),&self.environment).is_some(){"ready"}else{"missing_runner"},"distribution":"local","version":null})
        } else {
            json!({"status":"failed","message":"Could not inspect ACP Registry agent: ACP registry catalog is unavailable."})
        }
    }
    pub(crate) async fn readiness(&self) -> Result<Value, AcpError> {
        let inspection = if self.enabled {
            self.inspect_health().await
        } else {
            json!({"status":"unconfigured"})
        };
        let input = self.health_input(inspection);
        let mut snapshot = crate::acp_health::checked(&input);
        if !self.enabled {
            snapshot["message"] = json!("ACP Registry is disabled in T3 Code settings.");
        } else if input["inspection"]["status"] == "ready" {
            snapshot["message"] =
                json!("Checking ACP authentication, models, and commands in the background...");
        }
        checked_snapshot(snapshot)
    }
    pub async fn discover(&self, cwd: &Path) -> Result<Value, AcpError> {
        struct Cancel(Option<oneshot::Sender<()>>);
        impl Drop for Cancel {
            fn drop(&mut self) {
                if let Some(cancel) = self.0.take() {
                    let _ = cancel.send(());
                }
            }
        }
        let (cancel, cancellation) = oneshot::channel();
        let _cancel = Cancel(Some(cancel));
        let (sender, result) = oneshot::channel();
        let instance = self.clone();
        let cwd = cwd.to_owned();
        tokio::spawn(async move {
            let result = instance.discover_owned(&cwd, cancellation).await;
            let _ = sender.send(result);
        });
        result.await.map_err(|_| AcpError::Closed)?
    }
    pub(crate) async fn discover_owned(
        &self,
        cwd: &Path,
        cancellation: oneshot::Receiver<()>,
    ) -> Result<Value, AcpError> {
        self.discover_outcome(cwd, cancellation)
            .await
            .map(|(snapshot, _)| snapshot)
    }
    pub(crate) async fn discover_enrichment(
        &self,
        cwd: &Path,
        cancellation: oneshot::Receiver<()>,
    ) -> Result<Option<Value>, AcpError> {
        self.discover_outcome(cwd, cancellation)
            .await
            .map(|(snapshot, completed)| completed.then_some(snapshot))
    }
    async fn discover_outcome(
        &self,
        cwd: &Path,
        mut cancellation: oneshot::Receiver<()>,
    ) -> Result<(Value, bool), AcpError> {
        let mut interrupted = false;
        let inspection = if self.enabled {
            self.inspect_health().await
        } else {
            json!({"status":"unconfigured"})
        };
        let mut health_input = self.health_input(inspection);
        let mut snapshot = crate::acp_health::checked(&health_input);
        if !self.enabled {
            snapshot["message"] = json!("ACP Registry is disabled in T3 Code settings.");
        } else {
            if health_input["inspection"]["status"] == "ready" {
                let Some(mut admission) = self.coordinator.background_probe(&self.startup_key())
                else {
                    snapshot["message"] = json!(
                        "ACP discovery deferred while the provider starts a foreground session."
                    );
                    return checked_snapshot(snapshot).map(|snapshot| (snapshot, false));
                };
                let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
                let resolved = tokio::select! {
                    biased;
                    _=&mut cancellation=>return checked_snapshot(snapshot).map(|snapshot|(snapshot,false)),
                    _=admission.interrupted()=>return checked_snapshot(snapshot).map(|snapshot|(snapshot,false)),
                    result=tokio::time::timeout_at(deadline,self.resolve_process(cwd))=>result.unwrap_or_else(|_|Err(AcpError::Transport("Timed out while resolving ACP probe process.".into()))),
                };
                let command = resolved
                    .as_ref()
                    .map(|options| options.binary.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let args = resolved
                    .as_ref()
                    .map(|options| options.args.clone())
                    .unwrap_or_default();
                let mut probe_initialize = Value::Null;
                match resolved.and_then(ProcessPeer::spawn) {
                    Ok(peer) => {
                        let services = self.services_for(
                            cwd,
                            crate::acp_client_callbacks::policy(&json!("approval-required"), cwd),
                        );
                        let retained = services.clone();
                        let result = tokio::select! {
                            biased;
                            _=&mut cancellation=>None,
                            _=admission.interrupted()=>None,
                            result=tokio::time::timeout_at(deadline,async {
                                let initialized=self.initialize_only(peer.clone(),true,services,None,None).await?;
                                probe_initialize=serde_json::to_value(&initialized.initialize).expect("typed initialize");
                                self.finish_initialized(initialized,cwd,None,true,false).await
                            })=>Some(result),
                        };
                        match result {
                            Some(Ok(Ok(mut session))) => {
                                // The source allows a brief advertisement grace after
                                // setup; foreground interruption still disposes the probe.
                                let advertised = tokio::select! {
                                    biased;
                                    _=&mut cancellation=>false,
                                    _=admission.interrupted()=>false,
                                    _=tokio::time::timeout(Duration::from_millis(500),session.commands.wait_for(|commands|commands.is_some()))=>true,
                                };
                                if !advertised {
                                    interrupted = true;
                                    snapshot["message"] = json!(
                                        "ACP discovery interrupted by foreground work or cancellation."
                                    );
                                }
                                let configuration = crate::acp_model::live_configuration(
                                    &serde_json::to_value(&session.setup_response).unwrap(),
                                );
                                let mut probe = serde_json::to_value(configuration).unwrap();
                                probe["sessionManagement"] =
                                    crate::acp_health::management(&probe_initialize);
                                probe["authMethods"] = json!(crate::acp_health::auth_methods(
                                    &probe_initialize,
                                    &command,
                                    &args
                                ));
                                probe["icon"] = Value::Null;
                                let commands =
                                    session.commands.borrow().clone().unwrap_or_default();
                                health_input["probe"] = json!({"probe":probe,"slashCommands":commands.slash_commands,"skills":commands.skills});
                                snapshot = crate::acp_health::checked(&health_input);
                                session.shutdown().await;
                            }
                            Some(Ok(Err(error))) => {
                                health_input["probeError"] = crate::acp_health::probe_failure(
                                    &error,
                                    crate::acp_health::auth_methods(
                                        &probe_initialize,
                                        &command,
                                        &args,
                                    ),
                                    None,
                                );
                                snapshot = crate::acp_health::checked(&health_input);
                            }
                            Some(Err(_)) => {
                                health_input["probeError"] = json!({"reason":"probe_failed","message":"The ACP agent did not resolve and create a test session within 60 seconds. Package installation or agent startup may be slow; this check retries on the next provider refresh."});
                                snapshot = crate::acp_health::checked(&health_input);
                            }
                            None => {
                                interrupted = true;
                                snapshot["status"] = json!("warning");
                                snapshot["message"] = json!(
                                    "ACP discovery interrupted by foreground work or cancellation."
                                );
                            }
                        }
                        // Retain callbacks and process outside the cancellable handshake. A
                        // timeout must finish terminal disposal and reap before publishing failure.
                        retained.shutdown().await;
                        peer.shutdown().await;
                    }
                    Err(error) => {
                        health_input["probeError"] =
                            crate::acp_health::probe_failure(&error, vec![], None);
                        snapshot = crate::acp_health::checked(&health_input);
                    }
                }
            }
        }
        checked_snapshot(snapshot).map(|snapshot| (snapshot, !interrupted))
    }
}
fn checked_snapshot(snapshot: Value) -> Result<Value, AcpError> {
    let typed: t3_contracts::ServerProvider =
        serde_json::from_value(snapshot).map_err(|error| AcpError::Transport(error.to_string()))?;
    serde_json::to_value(typed).map_err(|error| AcpError::Transport(error.to_string()))
}
pub fn permission_response(
    request: &RequestPermissionRequest,
    decision: &str,
) -> Result<t3_acp::v2::RequestPermissionResponse, AcpError> {
    let kind = match decision {
        "accept" => Some("allow_once"),
        "acceptAlways" | "acceptForSession" => Some("allow_always"),
        "decline" => Some("reject_once"),
        _ => None,
    };
    let option = kind
        .and_then(|kind| {
            request.options.iter().find_map(|option| {
                let value = option.as_value();
                (value["kind"] == kind).then(|| value["optionId"].as_str().unwrap_or("").to_owned())
            })
        })
        .map(|value| t3_contracts::trim_wire_string(&value).to_owned())
        .filter(|value| !value.is_empty());
    serde_json::from_value(match option {
        Some(option) => json!({"outcome":{"outcome":"selected","optionId":option}}),
        None => json!({"outcome":{"outcome":"cancelled"}}),
    })
    .map_err(|error| AcpError::Transport(format!("Invalid permission response: {error}")))
}
pub fn models_from_setup(setup: &Value, custom: &[String]) -> Vec<Value> {
    crate::acp_model::discovered_models(setup, custom)
}

fn authentication_required(error: &AcpError) -> bool {
    matches!(error, AcpError::Failure(failure) if matches!(failure.as_ref(), t3_acp::errors::Failure::Request(error) if error.code == -32000))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn instance(generation: u8) -> AcpInstance {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-provider.py");
        AcpInstance{instance_id:"local-agent".into(),display_name:"Local agent".into(),accent_color:None,enabled:true,config:serde_json::from_value(json!({"source":"local","commandPath":"python3","commandArgs":[fixture,generation.to_string()]})).unwrap(),environment:HashMap::new(),catalog:None,coordinator:Default::default()}
    }
    #[tokio::test]
    async fn normal_session_authenticates_only_after_required_and_retries_exactly_once() {
        tokio::time::timeout(Duration::from_secs(20), async {
            for generation in [1, 2] {
                for (scenario, configured, expected_auth, expected_setups, successful) in [
                    ("auth-ready", "agent", 0, 1, true),
                    ("auth-required", "", 1, 2, true),
                    ("auth-required", "\u{feff}agent\u{feff}", 1, 2, true),
                    ("auth-required", "\u{0085}agent", 0, 1, false),
                    ("auth-required", "missing", 0, 1, false),
                    ("auth-terminal", "agent", 0, 1, false),
                    ("auth-none", "", 0, 1, false),
                    ("auth-other-error", "agent", 0, 1, false),
                    ("auth-twice", "agent", 1, 2, false),
                ] {
                    let root = tempfile::tempdir().unwrap();
                    let log = root.path().join("requests");
                    let mut provider = instance(generation);
                    provider
                        .config
                        .command_args
                        .extend([scenario.into(), log.to_string_lossy().into_owned()]);
                    // Admission uses the same JavaScript whitespace definition as
                    // the source contract and runtime method selector.
                    provider.config.auth_method_id = configured.parse().unwrap();
                    let peer =
                        ProcessPeer::spawn(provider.resolve_process(root.path()).await.unwrap())
                            .unwrap();
                    let retained = peer.clone();
                    let result = provider.start_peer(peer, root.path(), None, false).await;
                    assert_eq!(
                        result.is_ok(),
                        successful,
                        "generation {generation}, {scenario}, {configured:?}: {}",
                        result
                            .as_ref()
                            .err()
                            .map(ToString::to_string)
                            .unwrap_or_default()
                    );
                    if let Ok(session) = result {
                        session.shutdown().await;
                    }
                    retained.shutdown().await;
                    let requests = std::fs::read_to_string(log).unwrap();
                    assert_eq!(
                        requests
                            .lines()
                            .filter(|method| matches!(*method, "authenticate" | "auth/login"))
                            .count(),
                        expected_auth,
                        "{scenario} {requests}"
                    );
                    assert_eq!(
                        requests
                            .lines()
                            .filter(|method| *method == "session/new")
                            .count(),
                        expected_setups,
                        "{scenario} {requests}"
                    );
                }
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn discovery_surfaces_auth_required_without_attempting_headless_authentication() {
        tokio::time::timeout(Duration::from_secs(10), async {
            for generation in [1, 2] {
                for configured in ["", "agent"] {
                    let root = tempfile::tempdir().unwrap();
                    let log = root.path().join("requests");
                    let mut provider = instance(generation);
                    provider.config.auth_method_id = configured.parse().unwrap();
                    provider
                        .config
                        .command_args
                        .extend(["auth-required".into(), log.to_string_lossy().into_owned()]);
                    let peer =
                        ProcessPeer::spawn(provider.resolve_process(root.path()).await.unwrap())
                            .unwrap();
                    let retained = peer.clone();
                    let result = provider.start_peer(peer, root.path(), None, true).await;
                    match result {
                        Err(error) => assert!(authentication_required(&error), "{error}"),
                        Ok(session) => {
                            session.shutdown().await;
                            panic!("discovery must surface authentication requirement");
                        }
                    }
                    retained.shutdown().await;
                    assert_eq!(
                        std::fs::read_to_string(log)
                            .unwrap()
                            .lines()
                            .collect::<Vec<_>>(),
                        ["initialize", "session/new"],
                        "no login or retry during discovery"
                    );
                }
            }
        })
        .await
        .unwrap();
    }
    fn coordinator_instance(root: &Path, scenario: &str) -> AcpInstance {
        let mut provider = instance(2);
        provider.config.command_args = vec![
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/acp-coordinator-provider.py")
                .to_string_lossy()
                .into_owned(),
            scenario.into(),
            root.join("starts").to_string_lossy().into_owned(),
        ];
        provider
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_connect_interrupts_matching_real_discovery_and_waits_owned_probe_reap() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let root = tempfile::tempdir().unwrap();
            let signal_path = root.path().join("signal.sock");
            let signal = tokio::net::UnixDatagram::bind(&signal_path).unwrap();
            let mut provider = coordinator_instance(root.path(), "priority");
            provider.environment.insert(
                "T3_SIGNAL_SOCKET".into(),
                signal_path.to_string_lossy().into_owned(),
            );
            let probe_instance = provider.clone();
            let cwd = root.path().to_owned();
            let probe = tokio::spawn(async move { probe_instance.discover(&cwd).await });
            let mut message = [0; 512];
            let length = signal.recv(&mut message).await.unwrap();
            let started: Value = serde_json::from_slice(&message[..length]).unwrap();
            let pid = started["pid"].as_i64().unwrap() as libc::pid_t;
            // Foreground starts immediately after publishing priority; it does
            // not await the disposable probe, matching the source coordinator.
            let session = provider.connect(root.path(), None, false).await.unwrap();
            let snapshot = probe.await.unwrap().unwrap();
            assert_eq!(snapshot["status"], "warning");
            assert!(
                snapshot["message"]
                    .as_str()
                    .unwrap()
                    .contains("interrupted")
            );
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "probe completion includes owned child reap"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            assert!(
                provider
                    .coordinator
                    .background_probe(&provider.startup_key())
                    .is_some(),
                "startup guard releases after foreground setup"
            );
            session.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn real_url_consent_and_live_configuration_survive_registry_replacement_and_interleaved_mutations()
     {
        tokio::time::timeout(Duration::from_secs(20),async {
            let root=tempfile::tempdir().unwrap();
            let configured=coordinator_instance(root.path(),"state");
            let settings:t3_contracts::ServerSettings=serde_json::from_value(json!({
                "providerInstances":{"codex":{"driver":"codex","enabled":false},
                    "local-agent":{"driver":"acpRegistry","enabled":true,"config":configured.config}}
            })).unwrap();
            let registry=crate::provider_registry::ProviderRegistry::discover(&settings,root.path()).await.unwrap();
            let active=registry.acp("local-agent").unwrap();
            let coordinator=registry.coordinator();
            let mut url_updates=coordinator.subscribe_url_action("local-agent");
            let mut commands=coordinator.subscribe_commands("local-agent");
            if coordinator.commands("local-agent").is_some(){commands.recv().await.unwrap();}
            assert!(coordinator.configuration("local-agent").is_none(),"disposable probe must not publish foreground live state");
            let mut provider_changes=registry.subscribe_changes();
            let root_path=root.path().to_owned();
            let mut connect=tokio::task::JoinSet::new();
            connect.spawn(async move{active.connect(&root_path,None,false).await});
            let action=url_updates.recv().await.unwrap().unwrap();
            assert_eq!(action.url.0,"https://accounts.example.com/login");
            assert_eq!(action.message.0,"Sign in");
            let changes=provider_changes.recv().await.unwrap();
            assert_eq!(changes.iter().find(|row|row["instanceId"]=="local-agent").unwrap()["auth"]["action"]["elicitationId"],"login-0");
            let mut next=serde_json::to_value(&settings).unwrap();
            next["providerInstances"]["local-agent"]["displayName"]=json!("Renamed");
            let next=serde_json::from_value(next).unwrap();
            let replaced=registry.clone();
            replaced.reconfigure(&next,root.path()).await.unwrap();
            let current=replaced.snapshots().into_iter().find(|row|row["instanceId"]=="local-agent").unwrap();
            assert_eq!(current["auth"]["action"]["elicitationId"],"login-0");
            assert_eq!(current["displayName"],"Renamed");
            assert_eq!(std::fs::read_to_string(root.path().join("starts.log")).unwrap().lines().count(),2,
                "registry refresh shares startup admission and does not launch a competing probe");
            assert!(!replaced.coordinator().accept_url_authentication(
                &serde_json::from_value(json!({"instanceId":"local-agent","elicitationId":"wrong"})).unwrap()));
            assert!(replaced.coordinator().accept_url_authentication(
                &serde_json::from_value(json!({"instanceId":"local-agent","elicitationId":"login-0"})).unwrap()));
            assert!(!coordinator.accept_url_authentication(
                &serde_json::from_value(json!({"instanceId":"local-agent","elicitationId":"login-0"})).unwrap()));
            let mut session=connect.join_next().await.unwrap().unwrap().unwrap();
            assert!(url_updates.recv().await.unwrap().is_none());
            let oracle:Value=serde_json::from_str(include_str!("../tests/fixtures/acp-startup.json")).unwrap();
            assert_eq!(serde_json::to_value(&session.setup_response).unwrap(),oracle["setup"]);
            assert_eq!(serde_json::to_value(&session.setup).unwrap()["configOptions"],oracle["configOptions"]);
            assert_eq!(serde_json::to_value(&session.setup).unwrap()["modes"],oracle["modeState"]);
            assert_eq!(serde_json::to_value(&session.setup_response).unwrap()["configOptions"][0]["currentValue"],"a",
                "unchanged source start result retains original setup response");
            assert_eq!(serde_json::to_value(&session.setup_response).unwrap()["modes"]["currentModeId"],"normal");
            assert_eq!(serde_json::to_value(&session.setup).unwrap()["configOptions"][0]["currentValue"],"early",
                "pre-response config update survives setup seed");
            assert_eq!(serde_json::to_value(&session.setup).unwrap()["modes"]["currentModeId"],"alt");
            let advertised=commands.recv().await.unwrap();
            assert_eq!(advertised.slash_commands.len(),1);
            assert_eq!(advertised.skills[0].path.as_str(),"acp://skill/space%2Fskill");
            session.set_model("b").await.unwrap();
            session.client.raw_request("x/mode",json!({"mode":"normal"})).await.unwrap();
            let current=coordinator.configuration("local-agent").unwrap();
            assert_eq!(current.current_model_id.as_deref(),Some("b"),"mode notification retains acknowledged model change");
            session.set_options(&serde_json::from_value(json!({"enabled":true})).unwrap()).await.unwrap();
            session.client.raw_request("x/mode",json!({"mode":"alt"})).await.unwrap();
            let current=serde_json::to_value(coordinator.configuration("local-agent").unwrap()).unwrap();
            assert_eq!(current["configOptions"].as_array().unwrap().iter().find(|option|option["id"]=="enabled").unwrap()["currentValue"],true,
                "mode notification retains acknowledged config option change");
            let visible=replaced.snapshots().into_iter().find(|row|row["instanceId"]=="local-agent").unwrap();
            assert!(visible["models"].as_array().unwrap().iter().any(|model|model["slug"]=="b" && model["isDefault"]==true));
            assert_eq!(visible["slashCommands"].as_array().unwrap().len(),1);
            assert!(visible["auth"].get("action").is_none());
            session.shutdown().await;
        }).await.unwrap();
    }
    #[tokio::test]
    async fn config_only_initial_mode_catalog_seeds_latest_mode_state_before_legacy_notification() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let directory = tempfile::tempdir().unwrap();
            let provider = coordinator_instance(directory.path(), "config-only");
            let mut session = provider
                .connect(directory.path(), None, false)
                .await
                .unwrap();
            let oracle: Value =
                serde_json::from_str(include_str!("../tests/fixtures/acp-startup.json")).unwrap();
            assert_eq!(
                serde_json::to_value(&session.setup_response).unwrap(),
                oracle["configOnlyMode"]["setup"]
            );
            assert_eq!(
                serde_json::to_value(&session.setup).unwrap()["modes"],
                oracle["configOnlyMode"]["initialModeState"]
            );
            session
                .client
                .raw_request("x/mode", json!({"mode":"alt"}))
                .await
                .unwrap();
            session.refresh_live_setup().unwrap();
            assert_eq!(
                serde_json::to_value(&session.setup).unwrap()["modes"],
                oracle["configOnlyMode"]["latestModeState"]
            );
            session.shutdown().await;
        })
        .await
        .unwrap();
    }
    fn prompt(session: &AcpSession, text: &str) -> PromptRequest {
        PromptRequest {
            session_id: session.setup.session_id.clone(),
            prompt: vec![serde_json::from_value(json!({"type":"text","text":text})).unwrap()],
            meta: Optional::Missing,
        }
    }
    #[tokio::test]
    async fn owned_session_negotiates_both_generations_and_preserves_permission_id_until_response()
    {
        for generation in [1, 2] {
            let cwd = tempfile::tempdir().unwrap();
            let mut session = instance(generation)
                .connect(cwd.path(), None, false)
                .await
                .unwrap();
            assert_eq!(session.initialize.protocol_version, u16::from(generation));
            let input = prompt(&session, "work");
            let client = session.client.clone();
            let task = tokio::spawn(async move { client.prompt_typed(input).await });
            let update = match session.events.recv().await.unwrap() {
                SessionEvent::Update(value) => value,
                _ => panic!("expected text"),
            };
            assert_eq!(
                serde_json::to_value(update).unwrap()["update"]["content"]["text"],
                "Hello "
            );
            match session.events.recv().await.unwrap() {
                SessionEvent::Permission {
                    context,
                    request,
                    response,
                    written,
                } => {
                    assert_eq!(serde_json::to_value(context.wire_id).unwrap(), json!(0));
                    assert_eq!(request.tool_call.tool_call_id, "command");
                    response
                        .send(permission_response(&request, "accept"))
                        .unwrap();
                    written.await.unwrap().unwrap();
                }
                _ => panic!("expected permission"),
            }
            let response = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(response.stop_reason, "end_turn");
            let mut text_seen = false;
            while let Ok(event) = session.events.try_recv() {
                if let SessionEvent::Update(value) = event {
                    if serde_json::to_value(value).unwrap()["update"]["content"]["text"]
                        == "approved"
                    {
                        text_seen = true;
                    }
                }
            }
            assert!(
                text_seen,
                "prompt completion must follow the last notification handler"
            );
        }
    }
    #[tokio::test]
    async fn saved_session_load_quarantines_history_and_cancel_settles_owned_prompt() {
        let cwd = tempfile::tempdir().unwrap();
        let mut session = instance(2)
            .connect(cwd.path(), Some("native-session"), false)
            .await
            .unwrap();
        assert!(
            session.events.try_recv().is_err(),
            "load history is quarantined from live root updates"
        );
        let input = prompt(&session, "hold");
        let client = session.client.clone();
        let task = tokio::spawn(async move { client.prompt_typed(input).await });
        let waiting = match session.events.recv().await.unwrap() {
            SessionEvent::Update(value) => value,
            _ => panic!("expected waiting milestone"),
        };
        assert_eq!(
            serde_json::to_value(waiting).unwrap()["update"]["content"]["text"],
            "waiting"
        );
        session
            .client
            .cancel_typed(serde_json::from_value(json!({"sessionId":"native-session"})).unwrap())
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .stop_reason,
            "cancelled"
        );
    }
    #[tokio::test]
    async fn discovery_uses_actual_local_catalog_and_disabled_instances_never_spawn() {
        let cwd = tempfile::tempdir().unwrap();
        let mut instance = instance(2);
        let snapshot = instance.discover(cwd.path()).await.unwrap();
        assert_eq!(snapshot["status"], "ready");
        assert_eq!(snapshot["models"][0]["slug"], "fixture-model");
        instance.enabled = false;
        instance.config.command_path = "/must-not-launch".parse().unwrap();
        assert_eq!(
            instance.discover(cwd.path()).await.unwrap()["status"],
            "disabled"
        );
    }
    async fn reaped(
        events: &mut tokio::sync::broadcast::Receiver<crate::provider_process::ProcessEvent>,
    ) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    events.recv().await.unwrap(),
                    crate::provider_process::ProcessEvent::Closed(_)
                ) {
                    break;
                }
            }
        })
        .await
        .expect("the owned child must be reaped");
    }
    #[tokio::test]
    async fn successful_failed_and_cancelled_session_setup_all_reap_the_owned_child() {
        for scenario in ["normal", "fail-initialize", "fail-load", "hold-initialize"] {
            let cwd = tempfile::tempdir().unwrap();
            let mut instance = instance(2);
            instance.config.command_args.push(scenario.into());
            let peer = ProcessPeer::spawn(instance.process_options(cwd.path()).unwrap()).unwrap();
            let mut events = peer.process_events();
            if scenario == "hold-initialize" {
                let root = cwd.path().to_owned();
                let task =
                    tokio::spawn(
                        async move { instance.start_peer(peer, &root, None, false).await },
                    );
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if let crate::provider_process::ProcessEvent::Notification {
                            method, ..
                        } = events.recv().await.unwrap()
                        {
                            if method == "x/initialize-held" {
                                break;
                            }
                        }
                    }
                })
                .await
                .unwrap();
                task.abort();
                assert!(matches!(task.await,Err(error) if error.is_cancelled()));
            } else {
                let result = instance
                    .start_peer(
                        peer,
                        cwd.path(),
                        (scenario == "fail-load").then_some("native-session"),
                        scenario == "normal",
                    )
                    .await;
                if scenario == "normal" {
                    drop(result.unwrap());
                } else {
                    assert!(result.is_err());
                }
            }
            reaped(&mut events).await;
        }
    }
    #[tokio::test]
    async fn resolved_devin_process_callbacks_require_grant_and_render_owned_terminal_output() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let cwd = tempfile::tempdir().unwrap();
            let mut instance = instance(2);
            instance
                .environment
                .insert("T3_CALLBACK_ENV".into(), "session".into());
            let mut options = instance.process_options(cwd.path()).unwrap();
            options.args = vec![
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/acp-client-terminals-provider.py")
                    .to_string_lossy()
                    .into_owned(),
            ];
            let peer = ProcessPeer::spawn(options).unwrap();
            let mut session = instance
                .start_resolved_peer(
                    peer,
                    cwd.path(),
                    None,
                    false,
                    "devin",
                    crate::acp_client_callbacks::policy(&json!("approval-required"), cwd.path()),
                )
                .await
                .unwrap();
            session.services.set_turn(
                crate::acp_client_callbacks::policy(&json!("approval-required"), cwd.path()),
                "turn-one".into(),
            );
            let client = session.client.clone();
            let input = prompt(&session, "execute");
            let prompt_task = tokio::spawn(async move { client.prompt_typed(input).await });
            let mut rendered = false;
            while !rendered {
                match session.events.recv().await.unwrap() {
                    SessionEvent::Permission {
                        request,
                        response,
                        written,
                        context,
                    } => {
                        assert_eq!(serde_json::to_value(context.wire_id).unwrap(), json!(0));
                        session
                            .services
                            .record_approval(&serde_json::to_value(&request).unwrap(), "accept");
                        response
                            .send(permission_response(&request, "accept"))
                            .unwrap();
                        written.await.unwrap().unwrap();
                    }
                    SessionEvent::Update(update) => {
                        let value = serde_json::to_value(update).unwrap();
                        if value["update"]["toolCallId"] == "terminal-tool" {
                            assert_eq!(
                                value["update"]["content"][0],
                                json!({"type":"content","content":{"type":"text","text":"SESSION"}})
                            );
                            rendered = true;
                        }
                    }
                    SessionEvent::Terminated(error) => panic!("provider terminated: {error}"),
                }
            }
            assert_eq!(prompt_task.await.unwrap().unwrap().stop_reason, "end_turn");
            assert_eq!(
                session
                    .services
                    .embedded_commands("devin-session", "terminal-tool"),
                vec!["printf '%s' \"$T3_CALLBACK_ENV\" | tr a-z A-Z"]
            );
            session.services.settle();
            session.shutdown().await;
        })
        .await
        .expect("bidirectional callback milestones");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn setup_failure_and_cancellation_await_client_terminal_cleanup_before_owner_release() {
        tokio::time::timeout(Duration::from_secs(15), async {
            for scenario in ["fail-initialize", "hold-initialize"] {
                let cwd = tempfile::tempdir().unwrap();
                let instance = instance(2);
                let services = crate::acp_client_callbacks::Services::new(
                    "devin",
                    cwd.path(),
                    Default::default(),
                    crate::acp_client_callbacks::policy(&json!("full-access"), cwd.path()),
                );
                let mut options = instance.process_options(cwd.path()).unwrap();
                options.args = vec![
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/acp-client-terminals-provider.py")
                        .to_string_lossy()
                        .into(),
                    scenario.into(),
                ];
                let peer = ProcessPeer::spawn(options).unwrap();
                let mut milestones = peer.process_events();
                let retained_peer = peer.clone();
                let retained_services = services.clone();
                let root = cwd.path().to_owned();
                let task = tokio::spawn(async move {
                    instance
                        .start_peer_with_services(peer, &root, None, false, services)
                        .await
                });
                let id = loop {
                    match milestones.recv().await.unwrap() {
                        crate::provider_process::ProcessEvent::Notification { method, params }
                            if method == "x/startup-terminal" =>
                        {
                            break params["terminalId"].as_str().unwrap().to_owned();
                        }
                        crate::provider_process::ProcessEvent::Closed(error) => {
                            panic!("startup ended before milestone: {error}")
                        }
                        _ => {}
                    }
                };
                let (pid, disposed) = retained_services.owned_terminal(&id).unwrap();
                if scenario == "fail-initialize" {
                    assert!(task.await.unwrap().is_err());
                    assert!(*disposed.borrow());
                } else {
                    task.abort();
                    assert!(matches!(task.await,Err(error) if error.is_cancelled()));
                    retained_services.shutdown().await;
                    assert!(*disposed.borrow());
                }
                retained_peer.shutdown().await;
                assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
            }
        })
        .await
        .expect("startup terminal ownership milestones");
    }
}
