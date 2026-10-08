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
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Debug)]
pub struct AcpInstance {
    pub instance_id: String,
    pub display_name: String,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub config: AcpRegistrySettings,
    pub environment: HashMap<String, String>,
    pub catalog: Option<crate::acp_registry_support::Catalog>,
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
    pub setup: NewSessionResponse,
    pub events: mpsc::UnboundedReceiver<SessionEvent>,
    peer: ProcessPeer,
    pub(crate) services: crate::acp_client_callbacks::Services,
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
        let setup = serde_json::to_value(&self.setup).unwrap();
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
                self.setup.config_options = Optional::Value(result.config_options);
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
            }
        } else if !model.is_empty() && model != "default" {
            return Err(AcpError::Transport(
                "This ACP agent exposes no model selection API.".into(),
            ));
        }
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
                continue;
            }
            let setup = serde_json::to_value(&self.setup).unwrap();
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
            self.setup.config_options = Optional::Value(result.config_options);
        }
        Ok(())
    }
}
impl AcpInstance {
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
        client
            .handle_session_update(Arc::new(move |update| {
                let updates = updates.clone();
                let services = update_services.clone();
                Box::pin(async move {
                    let update = serde_json::from_value(
                        services.resolve_update(serde_json::to_value(update).unwrap()),
                    )
                    .map_err(|error| {
                        AcpError::Transport(format!(
                            "Invalid embedded ACP terminal update: {error}"
                        ))
                    })?;
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
        // Capabilities describe only handlers implemented by this bridge. File,
        // terminal, elicitation and MCP callback support are added with services.
        let initialize = client
            .initialize_typed(InitializeRequest {
                client_info: Optional::Value(Implementation {
                    name: "t3-code".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    title: Optional::Value("T3 Code".into()),
                    meta: Optional::Missing,
                }),
                client_capabilities: Some(ClientCapabilities {
                    terminal: services.has_terminals().then_some(true),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await?;
        if !self.config.auth_method_id.as_str().is_empty() {
            let request = serde_json::from_value(json!({"methodId":self.config.auth_method_id}))
                .map_err(|error| {
                    AcpError::Transport(format!("Invalid authentication request: {error}"))
                })?;
            client.authenticate_typed(request).await?;
        }
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
                    mcp_servers: Some(vec![]),
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
                    mcp_servers: vec![],
                    meta: Optional::Missing,
                })
                .await?
        };
        if setup.session_id.is_empty() {
            return Err(AcpError::Transport(
                "ACP returned an empty session ID.".into(),
            ));
        }
        Ok(AcpSession {
            client,
            initialize,
            setup,
            events: receiver,
            peer: peer.as_ref().clone(),
            services,
        })
    }
    pub async fn discover(&self, cwd: &Path) -> Result<Value, AcpError> {
        let mut snapshot = json!({"instanceId":self.instance_id,"driver":"acpRegistry","displayName":self.display_name,"enabled":self.enabled,"installed":false,"version":null,"status":if self.enabled{"warning"}else{"disabled"},"auth":{"status":"unknown"},"checkedAt":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),"availability":"available","models":[],"slashCommands":[],"skills":[]});
        if let Some(accent) = &self.accent_color {
            snapshot["accentColor"] = json!(accent);
        }
        if !self.enabled {
            snapshot["message"] = json!("The ACP provider instance is disabled.");
        } else {
            let inspected = if self.config.source == AcpRegistrySettingsSource::Registry {
                match &self.catalog {
                    Some(catalog) => catalog
                        .inspection(&self.config, &self.environment)
                        .await
                        .map_err(|error| error.to_string()),
                    None => Err("ACP registry catalog is unavailable.".into()),
                }
            } else {
                Ok(json!({"status":"ready"}))
            };
            let probe = match inspected {
                Ok(info) if info["status"] == "ready" => true,
                Ok(info) => {
                    snapshot["message"] = json!(format!(
                        "ACP Registry provider is {}. Prepare the selected agent before discovery.",
                        info["status"].as_str().unwrap_or("unavailable")
                    ));
                    false
                }
                Err(error) => {
                    snapshot["message"] = json!(error);
                    false
                }
            };
            if probe {
                match self.resolve_process(cwd).await.and_then(ProcessPeer::spawn) {
                    Ok(peer) => {
                        let services = self.services_for(
                            cwd,
                            crate::acp_client_callbacks::policy(&json!("approval-required"), cwd),
                        );
                        let retained = services.clone();
                        let result = tokio::time::timeout(
                            Duration::from_secs(30),
                            self.start_peer_with_services(peer.clone(), cwd, None, true, services),
                        )
                        .await;
                        match result {
                            Ok(Ok(session)) => {
                                snapshot["installed"] = json!(true);
                                snapshot["status"] = json!("ready");
                                snapshot["auth"] = json!({"status":"authenticated"});
                                let initialized =
                                    serde_json::to_value(&session.initialize).unwrap();
                                snapshot["version"] = initialized["agentInfo"]["version"].clone();
                                snapshot["models"] = json!(models_from_setup(
                                    &serde_json::to_value(&session.setup).unwrap(),
                                    &self.config.custom_models
                                ));
                                session.shutdown().await;
                            }
                            Ok(Err(error)) => {
                                snapshot["status"] = json!("error");
                                snapshot["message"] =
                                    json!(format!("ACP provider probe failed: {error}"));
                            }
                            Err(_) => {
                                snapshot["status"] = json!("error");
                                snapshot["message"] =
                                    json!("Timed out while checking the ACP provider.");
                            }
                        }
                        // Retain callbacks and process outside the cancellable handshake. A
                        // timeout must finish terminal disposal and reap before publishing failure.
                        retained.shutdown().await;
                        peer.shutdown().await;
                    }
                    Err(error) => {
                        snapshot["status"] = json!("error");
                        snapshot["message"] = json!(format!("ACP provider probe failed: {error}"));
                    }
                }
            }
        }
        let typed: t3_contracts::ServerProvider = serde_json::from_value(snapshot)
            .map_err(|error| AcpError::Transport(error.to_string()))?;
        serde_json::to_value(typed).map_err(|error| AcpError::Transport(error.to_string()))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    fn instance(generation: u8) -> AcpInstance {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-provider.py");
        AcpInstance{instance_id:"local-agent".into(),display_name:"Local agent".into(),accent_color:None,enabled:true,config:serde_json::from_value(json!({"source":"local","commandPath":"python3","commandArgs":[fixture,generation.to_string()]})).unwrap(),environment:HashMap::new(),catalog:None}
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
    async fn saved_session_load_replay_precedes_prompt_and_cancel_settles_owned_prompt() {
        let cwd = tempfile::tempdir().unwrap();
        let mut session = instance(2)
            .connect(cwd.path(), Some("native-session"), false)
            .await
            .unwrap();
        let replay = match session.events.recv().await.unwrap() {
            SessionEvent::Update(value) => value,
            _ => panic!("expected replay"),
        };
        assert_eq!(
            serde_json::to_value(replay).unwrap()["update"]["content"]["text"],
            "replayed"
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
