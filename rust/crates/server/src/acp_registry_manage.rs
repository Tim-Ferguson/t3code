//! ACP agent session and provider management: `server.listAcpRegistrySessions`, `importAcpRegistrySession`,
//! `deleteAcpRegistrySession`, `listAcpRegistryProviders`, `setAcpRegistryProvider`,
//! `disableAcpRegistryProvider` and `logoutAcpRegistry` (the management half of AcpRegistryProbe.ts and the
//! handlers of ws.ts). Each operation starts the agent through the registry-resolved command, asks it one
//! ACP question and stops it again; nothing here keeps a process, and no credential, header or agent output
//! reaches a log or an error.
use crate::{
    acp_peer::ProcessPeer,
    acp_runtime::{AcpInstance, InitializedPeer},
    transport::ApiState,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_acp::{AcpError, AgentMethod};
use t3_contracts::*;

/// An `AcpRegistryOperationError` as it crosses the wire: a reason and a message, never the underlying cause.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub reason: String,
    pub message: String,
    pub auth_methods: Option<Value>,
    pub auth_action: Option<Value>,
}
impl Failure {
    pub fn new(reason: &str, message: impl Into<String>) -> Self {
        Self { reason: reason.into(), message: message.into(), auth_methods: None, auth_action: None }
    }
    pub fn wire(&self) -> Value {
        let mut value = json!({"_tag":"AcpRegistryOperationError","reason":self.reason,"message":self.message});
        if let Some(methods) = &self.auth_methods {
            value["authMethods"] = methods.clone();
        }
        if let Some(action) = &self.auth_action {
            value["authAction"] = action.clone();
        }
        value
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Operation {
    List,
    Delete,
    ProvidersList,
    ProvidersSet,
    ProvidersDisable,
}
fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}
fn bounded_text(value: &str, max: usize) -> String {
    String::from_utf16_lossy(&trim_wire_string(value).encode_utf16().take(max).collect::<Vec<_>>())
}
/// Non-empty, already trimmed and within `max` UTF-16 units, else dropped.
fn bounded_opaque(value: &str, max: usize) -> Option<String> {
    (!value.is_empty() && value == trim_wire_string(value) && utf16_len(value) <= max).then(|| value.to_owned())
}
/// `normalizeAcpRegistryWebUrl`: http(s) only, at most 2048 UTF-16 units, in its normalized form.
pub fn web_url(value: &str) -> Option<String> {
    if utf16_len(value) > 2048 {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.as_str().to_owned())
}
fn error_code(error: &AcpError) -> Option<i64> {
    match error {
        AcpError::Request(request) => Some(request.code),
        AcpError::ResponseError { error, .. } => Some(error.code),
        AcpError::Failure(failure) => match failure.as_ref() {
            t3_acp::errors::Failure::Request(request) => Some(request.code),
            _ => None,
        },
        _ => None,
    }
}
fn method_not_found() -> AcpError {
    AcpError::Request(t3_acp::RpcError { code: -32601, message: "ACP agent does not advertise this method".into(), data: None })
}
/// `managementFailure`: a missing method means the agent lacks the feature; an authentication problem is
/// reported as such; anything else gets the operation's own generic message.
pub fn management_failure(operation: Operation, error: &AcpError) -> Failure {
    if error_code(error) == Some(-32601) {
        return match operation {
            Operation::List => Failure::new("session_list_unsupported", "The ACP agent does not advertise session listing."),
            Operation::Delete => Failure::new("session_delete_unsupported", "The ACP agent does not advertise session deletion."),
            _ => Failure::new("providers_unsupported", "The ACP agent does not advertise provider configuration."),
        };
    }
    let classified = crate::acp_health::probe_failure(error, vec![], None);
    let (reason, message) = (classified["reason"].as_str().unwrap_or("probe_failed"), classified["message"].as_str().unwrap_or(""));
    if reason == "authentication_failed" {
        return Failure::new(reason, message);
    }
    match operation {
        Operation::Delete => Failure::new("session_delete_failed", "Could not delete the ACP session."),
        Operation::ProvidersList => Failure::new("providers_list_failed", "Could not list ACP providers."),
        Operation::ProvidersSet | Operation::ProvidersDisable => Failure::new("provider_configuration_failed", "Could not update the ACP provider configuration."),
        Operation::List => Failure::new(reason, message),
    }
}
fn timeout_failure(operation: Operation) -> Failure {
    match operation {
        Operation::List => Failure::new("probe_failed", "The ACP session list request timed out."),
        Operation::Delete => Failure::new("session_delete_failed", "The ACP session delete request timed out."),
        Operation::ProvidersList => Failure::new("providers_list_failed", "The ACP provider list request timed out."),
        Operation::ProvidersSet => Failure::new("provider_configuration_failed", "The ACP provider configuration request timed out."),
        Operation::ProvidersDisable => Failure::new("provider_configuration_failed", "The ACP provider disable request timed out."),
    }
}

fn management_initialize() -> t3_acp::types::InitializeRequest {
    use t3_acp::types::*;
    InitializeRequest {
        client_info: Optional::Value(Implementation { name: "t3-code-session-manager".into(), version: "0.0.0".into(), title: Optional::Missing, meta: Optional::Missing }),
        client_capabilities: Some(ClientCapabilities {
            auth: Some(ClientAuthCapabilities { terminal: Some(false), ..Default::default() }),
            fs: Some(FsClientCapabilities { read_text_file: Some(false), write_text_file: Some(false), ..Default::default() }),
            terminal: Some(false),
            elicitation: Optional::Value(serde_json::from_value(json!({"url":{}})).unwrap()),
            ..Default::default()
        }),
        ..Default::default()
    }
}
struct Pending {
    peer: ProcessPeer,
    services: crate::acp_client_callbacks::Services,
    initialized: Option<Arc<InitializedPeer>>,
}
/// Run one ACP question against a freshly started agent, within `budget`, and stop the agent however it ends.
async fn ask<T: Send>(
    instance: &AcpInstance,
    cwd: &Path,
    operation: Operation,
    budget: Duration,
    question: impl for<'a> FnOnce(&'a InitializedPeer, &'a Value) -> futures_util::future::BoxFuture<'a, Result<T, Failure>> + Send,
) -> Result<T, Failure> {
    let held: Arc<Mutex<Option<Pending>>> = Arc::new(Mutex::new(None));
    let run = async {
        let process = match &instance.catalog {
            Some(catalog) => catalog.resolve(&instance.config, cwd, &instance.environment).await.map_err(|error| Failure::new(error.reason, error.detail))?,
            None => instance.process_options(cwd).map_err(|error| management_failure(operation, &error))?,
        };
        let services = instance.services_for(cwd, json!({"mode":"readonly"}));
        let peer = ProcessPeer::spawn(process).map_err(|error| management_failure(operation, &AcpError::Transport(error.to_string())))?;
        *held.lock().unwrap() = Some(Pending { peer: peer.clone(), services: services.clone(), initialized: None });
        let initialized = Arc::new(instance.initialize_only(peer, true, services, None, Some(management_initialize())).await.map_err(|error| management_failure(operation, &error))?);
        if let Some(pending) = held.lock().unwrap().as_mut() {
            pending.initialized = Some(initialized.clone());
        }
        let capabilities = serde_json::to_value(&initialized.initialize).map_err(|_| Failure::new("probe_failed", "The ACP agent could not create a test session."))?;
        question(&initialized, &capabilities).await
    };
    let result = tokio::time::timeout(budget, run).await.unwrap_or_else(|_| Err(timeout_failure(operation)));
    let pending = held.lock().unwrap().take();
    if let Some(pending) = pending {
        if let Some(initialized) = &pending.initialized {
            initialized.shutdown().await;
        }
        pending.services.shutdown().await;
        pending.peer.shutdown().await;
    }
    result
}
fn present(value: &Value) -> bool {
    !value.is_null()
}

/// `session/list`, bounded and normalized: at most 256 sessions with usable ids and directories.
pub fn normalize_sessions(listed: &Value, capabilities: &Value) -> Value {
    let sessions: Vec<Value> = listed["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .take(256)
        .filter_map(|session| {
            let id = bounded_opaque(session["sessionId"].as_str()?, 1024)?;
            let cwd = bounded_opaque(session["cwd"].as_str()?, 4096)?;
            let directories: Vec<String> = session["additionalDirectories"].as_array().into_iter().flatten().filter_map(|directory| bounded_opaque(directory.as_str()?, 4096)).take(32).collect();
            let text = |key: &str, max: usize| session[key].as_str().map(|text| bounded_text(text, max)).filter(|text| !text.is_empty());
            Some(json!({"sessionId":id,"cwd":cwd,"additionalDirectories":directories,"title":text("title",1024),"updatedAt":text("updatedAt",128),"importedThreadId":null}))
        })
        .collect();
    let caps = &capabilities["agentCapabilities"];
    json!({
        "sessions": sessions,
        "nextCursor": listed["nextCursor"].as_str().map(|cursor| bounded_text(cursor, 2048)).filter(|cursor| !cursor.is_empty()),
        "canLoad": caps["loadSession"] == true,
        "canResume": present(&caps["sessionCapabilities"]["resume"]),
        "canDelete": present(&caps["sessionCapabilities"]["delete"]),
    })
}
/// `providers/list`, bounded and normalized: at most 64 providers, 16 protocols each.
pub fn normalize_providers(listed: &Value) -> Value {
    let providers: Vec<Value> = listed["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .take(64)
        .filter_map(|provider| {
            let id = bounded_opaque(provider["providerId"].as_str()?, 256)?;
            let supported: Vec<String> = provider["supported"].as_array().into_iter().flatten().filter_map(|protocol| bounded_opaque(protocol.as_str()?, 64)).take(16).collect();
            let current = provider["current"].as_object().and_then(|current| {
                let api_type = bounded_opaque(current.get("apiType")?.as_str()?, 64)?;
                let base_url = web_url(current.get("baseUrl")?.as_str()?)?;
                Some(json!({"apiType":api_type,"baseUrl":base_url}))
            });
            Some(json!({"providerId":id,"supported":supported,"required":provider["required"] == true,"current":current}))
        })
        .collect();
    json!({"providers": providers})
}

pub async fn list_sessions(instance: &AcpInstance, cwd: &Path, cursor: Option<&str>, budget: Duration) -> Result<Value, Failure> {
    let cursor = cursor.map(str::to_owned);
    let cwd_text = cwd.to_string_lossy().into_owned();
    ask(instance, cwd, Operation::List, budget, move |peer, capabilities| {
        Box::pin(async move {
            if !present(&capabilities["agentCapabilities"]["sessionCapabilities"]["list"]) {
                return Err(management_failure(Operation::List, &method_not_found()));
            }
            let mut params = json!({"cwd":cwd_text});
            if let Some(cursor) = cursor {
                params["cursor"] = json!(cursor);
            }
            let listed = peer.client.call(AgentMethod::ListSessions, params).await.map_err(|error| management_failure(Operation::List, &error))?;
            Ok(normalize_sessions(&listed, capabilities))
        })
    })
    .await
}
pub async fn delete_session(instance: &AcpInstance, cwd: &Path, session_id: &str, budget: Duration) -> Result<(), Failure> {
    let session_id = session_id.to_owned();
    ask(instance, cwd, Operation::Delete, budget, move |peer, capabilities| {
        Box::pin(async move {
            if !present(&capabilities["agentCapabilities"]["sessionCapabilities"]["delete"]) {
                return Err(management_failure(Operation::Delete, &method_not_found()));
            }
            peer.client.call(AgentMethod::DeleteSession, json!({"sessionId":session_id})).await.map_err(|error| management_failure(Operation::Delete, &error))?;
            Ok(())
        })
    })
    .await
}
pub async fn list_providers(instance: &AcpInstance, cwd: &Path, budget: Duration) -> Result<Value, Failure> {
    ask(instance, cwd, Operation::ProvidersList, budget, move |peer, capabilities| {
        Box::pin(async move {
            if !present(&capabilities["agentCapabilities"]["providers"]) {
                return Err(management_failure(Operation::ProvidersList, &method_not_found()));
            }
            let listed = peer.client.call(AgentMethod::ListProviders, json!({})).await.map_err(|error| management_failure(Operation::ProvidersList, &error))?;
            Ok(normalize_providers(&listed))
        })
    })
    .await
}
pub async fn set_provider(instance: &AcpInstance, cwd: &Path, request: Value, budget: Duration) -> Result<(), Failure> {
    ask(instance, cwd, Operation::ProvidersSet, budget, move |peer, capabilities| {
        Box::pin(async move {
            if !present(&capabilities["agentCapabilities"]["providers"]) {
                return Err(management_failure(Operation::ProvidersSet, &method_not_found()));
            }
            peer.client.call(AgentMethod::SetProvider, request).await.map_err(|error| management_failure(Operation::ProvidersSet, &error))?;
            Ok(())
        })
    })
    .await
}
pub async fn disable_provider(instance: &AcpInstance, cwd: &Path, provider_id: &str, budget: Duration) -> Result<(), Failure> {
    let provider_id = provider_id.to_owned();
    ask(instance, cwd, Operation::ProvidersDisable, budget, move |peer, capabilities| {
        Box::pin(async move {
            if !present(&capabilities["agentCapabilities"]["providers"]) {
                return Err(management_failure(Operation::ProvidersDisable, &method_not_found()));
            }
            peer.client.call(AgentMethod::DisableProvider, json!({"providerId":provider_id})).await.map_err(|error| management_failure(Operation::ProvidersDisable, &error))?;
            Ok(())
        })
    })
    .await
}

// ---- the socket handlers (ws.ts) ----

fn wire(failure: Failure) -> Value {
    failure.wire()
}
fn typed<T: serde::de::DeserializeOwned + serde::Serialize>(value: Value) -> Result<Value, Value> {
    let typed: T = serde_json::from_value(value).map_err(|error| json!({"_tag":"NativeServiceError","message":format!("ACP management result did not fit the contract: {error}")}))?;
    serde_json::to_value(typed).map_err(|_| json!({"_tag":"NativeServiceError","message":"ACP management result could not be encoded."}))
}
/// The workspace root of an existing project.
fn project_root(state: &ApiState, project_id: &ProjectId) -> Result<String, Failure> {
    let missing = |message: String| Failure::new("project_not_found", message);
    let project = state.store.projection("project", project_id.as_str()).map_err(|_| missing(format!("Project {project_id} is unavailable.")))?;
    project
        .filter(|project| project["deletedAt"].is_null())
        .and_then(|project| project["workspaceRoot"].as_str().map(str::to_owned))
        .ok_or_else(|| missing(format!("Project {project_id} was not found.")))
}
fn acp_instance(state: &ApiState, instance_id: &ProviderInstanceId) -> Result<AcpInstance, Failure> {
    let registry = state.providers.as_ref().ok_or_else(|| Failure::new("instance_not_found", format!("Provider instance {instance_id} was not found.")))?;
    registry.acp_instance(instance_id.as_str()).ok_or_else(|| Failure::new("instance_not_found", format!("Provider instance {instance_id} was not found.")))
}
fn snapshot(state: &ApiState, instance_id: &ProviderInstanceId) -> Value {
    state.providers.as_ref().and_then(|registry| registry.snapshots().into_iter().find(|snapshot| snapshot["instanceId"] == instance_id.as_str())).unwrap_or(Value::Null)
}
/// `deriveThreadFromProviderThread`: the thread an imported native session maps to.
pub fn imported_thread_id(driver: &str, instance_id: &str, session_id: &str) -> String {
    let encode = crate::provider_maintenance::encode_uri_component;
    ["provider", driver, "provider-instance", instance_id, "native-thread", session_id].iter().fold("thread".to_owned(), |id, part| format!("{id}:{}", encode(part)))
}
fn thread_exists(state: &ApiState, thread_id: &str) -> Result<bool, Failure> {
    state.store.projection("thread", thread_id).map(|thread| thread.is_some()).map_err(|_| Failure::new("session_import_failed", "Could not inspect existing imported ACP sessions."))
}
fn budget(state: &ApiState) -> Duration {
    state.providers.as_ref().map(|registry| registry.management().options().acp_timeout).unwrap_or(Duration::from_secs(60))
}
/// Close live runtimes of this instance before its configuration changes under them.
async fn close_instance(state: &ApiState, instance_id: &ProviderInstanceId) -> Result<(), ()> {
    match &state.execution {
        Some(execution) => (execution.authentication_stop())(vec![instance_id.to_string()]).await.map_err(|_| ()),
        None => Ok(()),
    }
}

pub async fn handle_list_sessions(state: &ApiState, input: AcpRegistryListSessionsInput) -> Result<Value, Value> {
    let root = project_root(state, &input.project_id).map_err(wire)?;
    let instance = acp_instance(state, &input.instance_id).map_err(wire)?;
    let cursor = input.cursor.as_ref().map(|cursor| cursor.0.clone());
    let mut listed = Box::pin(list_sessions(&instance, Path::new(&root), cursor.as_deref(), budget(state))).await.map_err(wire)?;
    let driver = "acpRegistry";
    for session in listed["sessions"].as_array_mut().into_iter().flatten() {
        let thread_id = imported_thread_id(driver, input.instance_id.as_str(), session["sessionId"].as_str().unwrap_or(""));
        if thread_exists(state, &thread_id).map_err(wire)? {
            session["importedThreadId"] = json!(thread_id);
        }
    }
    typed::<AcpRegistryListSessionsResult>(listed)
}
pub async fn handle_import_session(state: &ApiState, input: AcpRegistryImportSessionInput) -> Result<Value, Value> {
    let failed = |message: &str| wire(Failure::new("session_import_failed", message));
    let root = project_root(state, &input.project_id).map_err(wire)?;
    let _ = root;
    let instance = acp_instance(state, &input.instance_id).map_err(wire)?;
    let snapshot = snapshot(state, &input.instance_id);
    if snapshot["nativeSessions"]["canLoad"] != true && snapshot["nativeSessions"]["canResume"] != true {
        return Err(wire(Failure::new("session_resume_unsupported", "The ACP agent cannot load or resume native sessions.")));
    }
    let thread_id = imported_thread_id("acpRegistry", input.instance_id.as_str(), input.session_id.0.as_str());
    if thread_exists(state, &thread_id).map_err(|_| failed("Could not inspect the imported ACP session mapping."))? {
        return typed::<AcpRegistryImportSessionResult>(json!({"threadId":thread_id,"imported":false}));
    }
    let model = snapshot["models"].as_array().and_then(|models| models.iter().find(|model| model["isDefault"] == true).or(models.first())).and_then(|model| model["slug"].as_str()).unwrap_or("default").to_owned();
    let mut metadata = json!({"itemIdentityVersion":2});
    if let Some(Some(title)) = &input.title {
        metadata["title"] = json!(title.0);
    }
    if let Some(Some(updated)) = &input.updated_at {
        metadata["updatedAt"] = json!(updated.0);
    }
    let title = input.title.as_ref().and_then(|title| title.as_ref()).map(|title| title.0.to_string()).unwrap_or_else(|| "Imported ACP session".into());
    let command = json!({
        "type":"thread.create","commandId":uuid::Uuid::new_v4().to_string(),"threadId":thread_id,"projectId":input.project_id,"title":title,
        "modelSelection":{"instanceId":input.instance_id,"model":model},"runtimeMode":"approval-required","interactionMode":"default",
        "importedNativeThread":{"ref":{"driver":"acpRegistry","nativeId":input.session_id,"strength":"strong"},"metadata":metadata},
        "branch":null,"worktreePath":null,"createdBy":"user","creationSource":"web"
    });
    let _ = instance;
    // The thread is created by the orchestrator, which owns the native-thread reference and replays the
    // session on its first run. A refusal there (including a concurrent import of the same session) is
    // answered as the source answers a failed launch.
    let launched = crate::thread::ThreadService::new(state.store.clone()).dispatch(&command, chrono::Utc::now());
    match launched {
        Ok(receipt) if receipt.status != "rejected" => typed::<AcpRegistryImportSessionResult>(json!({"threadId":thread_id,"imported":true})),
        _ => {
            if thread_exists(state, &thread_id).map_err(|_| failed("Could not inspect the imported ACP session after launch failed."))? {
                return typed::<AcpRegistryImportSessionResult>(json!({"threadId":thread_id,"imported":false}));
            }
            Err(failed("Could not create a T3 thread for the ACP session."))
        }
    }
}
pub async fn handle_delete_session(state: &ApiState, input: AcpRegistryDeleteSessionInput) -> Result<Value, Value> {
    let root = project_root(state, &input.project_id).map_err(wire)?;
    let instance = acp_instance(state, &input.instance_id).map_err(wire)?;
    if snapshot(state, &input.instance_id)["nativeSessions"]["canDelete"] != true {
        return Err(wire(Failure::new("session_delete_unsupported", "The ACP agent does not advertise session deletion.")));
    }
    let thread_id = imported_thread_id("acpRegistry", input.instance_id.as_str(), input.session_id.0.as_str());
    let imported = thread_exists(state, &thread_id).map_err(|_| wire(Failure::new("session_delete_failed", "Could not inspect the imported ACP session mapping.")))?;
    if imported {
        return Err(wire(Failure::new("session_delete_failed", "Delete the imported T3 thread before deleting its native ACP session.")));
    }
    Box::pin(delete_session(&instance, Path::new(&root), input.session_id.0.as_str(), budget(state))).await.map_err(wire)?;
    typed::<AcpRegistryDeleteSessionResult>(json!({"deleted":true}))
}
pub async fn handle_list_providers(state: &ApiState, input: AcpRegistryListProvidersInput) -> Result<Value, Value> {
    let root = project_root(state, &input.project_id).map_err(wire)?;
    let instance = acp_instance(state, &input.instance_id).map_err(wire)?;
    if snapshot(state, &input.instance_id)["configurableProviders"] != true {
        return Err(wire(Failure::new("providers_unsupported", "The ACP agent does not advertise provider configuration.")));
    }
    typed::<AcpRegistryListProvidersResult>(Box::pin(list_providers(&instance, Path::new(&root), budget(state))).await.map_err(wire)?)
}
pub async fn handle_set_provider(state: &ApiState, input: AcpRegistrySetProviderInput) -> Result<Value, Value> {
    let configuration = |message: &str| wire(Failure::new("provider_configuration_failed", message));
    let root = project_root(state, &input.project_id).map_err(wire)?;
    let instance = acp_instance(state, &input.instance_id).map_err(wire)?;
    if input.headers.as_ref().is_some_and(|headers| headers.0.len() > 32) {
        return Err(configuration("ACP provider configuration accepts at most 32 headers."));
    }
    let listed = Box::pin(list_providers(&instance, Path::new(&root), budget(state))).await.map_err(wire)?;
    let provider = listed["providers"].as_array().and_then(|providers| providers.iter().find(|provider| provider["providerId"] == input.provider_id.0.as_str()));
    if !provider.is_some_and(|provider| provider["supported"].as_array().is_some_and(|supported| supported.iter().any(|protocol| *protocol == input.api_type.0.as_str()))) {
        return Err(configuration(&format!("Provider {} does not support {}.", input.provider_id.0, input.api_type.0)));
    }
    close_instance(state, &input.instance_id).await.map_err(|_| configuration("Could not stop live sessions before updating the ACP provider."))?;
    let mut request = json!({"providerId":input.provider_id,"apiType":input.api_type,"baseUrl":input.base_url});
    if let Some(headers) = &input.headers {
        request["headers"] = Value::Object(headers.0.clone());
    }
    Box::pin(set_provider(&instance, Path::new(&root), request, budget(state))).await.map_err(wire)?;
    refresh(state, &input.instance_id).await;
    typed::<AcpRegistrySetProviderResult>(json!({"configured":true}))
}
pub async fn handle_disable_provider(state: &ApiState, input: AcpRegistryDisableProviderInput) -> Result<Value, Value> {
    let configuration = |message: &str| wire(Failure::new("provider_configuration_failed", message));
    let root = project_root(state, &input.project_id).map_err(wire)?;
    let instance = acp_instance(state, &input.instance_id).map_err(wire)?;
    let listed = Box::pin(list_providers(&instance, Path::new(&root), budget(state))).await.map_err(wire)?;
    let provider = listed["providers"].as_array().and_then(|providers| providers.iter().find(|provider| provider["providerId"] == input.provider_id.0.as_str()));
    match provider {
        None => return Err(configuration(&format!("Provider {} was not advertised by the ACP agent.", input.provider_id.0))),
        Some(provider) if provider["required"] == true => return Err(configuration(&format!("Provider {} is required and cannot be disabled.", input.provider_id.0))),
        Some(_) => {}
    }
    close_instance(state, &input.instance_id).await.map_err(|_| configuration("Could not stop live sessions before disabling the ACP provider."))?;
    Box::pin(disable_provider(&instance, Path::new(&root), input.provider_id.0.as_str(), budget(state))).await.map_err(wire)?;
    refresh(state, &input.instance_id).await;
    typed::<AcpRegistryDisableProviderResult>(json!({"disabled":true}))
}
/// `server.logoutAcpRegistry`: sign out through the instance's authentication controller, which also stops its
/// live sessions and forgets what it knew of the account, then re-read the instance's health.
pub async fn handle_logout(state: &ApiState, input: AcpRegistryLogoutInput) -> Result<Value, Value> {
    acp_instance(state, &input.instance_id).map_err(wire)?;
    if snapshot(state, &input.instance_id)["auth"]["canLogout"] != true {
        return Err(wire(Failure::new("logout_unsupported", "The ACP agent does not advertise logout.")));
    }
    let service = state.provider_auth.as_ref().ok_or_else(|| wire(Failure::new("logout_failed", "Could not sign out of the ACP agent.")))?;
    service.logout(input.instance_id.as_str()).await.map_err(|_| wire(Failure::new("logout_failed", "Could not sign out of the ACP agent.")))?;
    refresh(state, &input.instance_id).await;
    typed::<AcpRegistryLogoutResult>(json!({"loggedOut":true}))
}
async fn refresh(state: &ApiState, instance_id: &ProviderInstanceId) {
    if let Some(registry) = &state.providers {
        let _ = registry.refresh_instance(instance_id.as_str()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalization_url_rule_and_thread_ids_match_the_original_source() {
        let mut checked = 0;
        for line in include_str!("../tests/fixtures/acp-registry-manage.jsonl").lines() {
            let fixture: Value = serde_json::from_str(line).unwrap();
            match fixture["kind"].as_str().unwrap() {
                "sessions" => assert_eq!(normalize_sessions(&fixture["listed"], &fixture["initialized"]), fixture["expected"], "{fixture}"),
                "providers" => assert_eq!(normalize_providers(&fixture["listed"]), fixture["expected"], "{fixture}"),
                "url" => assert_eq!(web_url(fixture["url"].as_str().unwrap()), fixture["expected"].as_str().map(str::to_owned), "{fixture}"),
                "thread" => assert_eq!(imported_thread_id("acpRegistry", fixture["instance"].as_str().unwrap(), fixture["session"].as_str().unwrap()), fixture["expected"].as_str().unwrap(), "{fixture}"),
                other => panic!("{other}"),
            }
            checked += 1;
        }
        assert_eq!(checked, 40);
    }
    #[test]
    fn a_missing_method_means_the_feature_is_absent_and_other_failures_carry_no_agent_text() {
        let absent = method_not_found();
        assert_eq!(management_failure(Operation::List, &absent).reason, "session_list_unsupported");
        assert_eq!(management_failure(Operation::Delete, &absent).reason, "session_delete_unsupported");
        for operation in [Operation::ProvidersList, Operation::ProvidersSet, Operation::ProvidersDisable] {
            assert_eq!(management_failure(operation, &absent).reason, "providers_unsupported");
        }
        let broken = AcpError::Transport("failed reading /home/me/.config/agent/token sk-123".into());
        for (operation, reason) in [(Operation::Delete, "session_delete_failed"), (Operation::ProvidersList, "providers_list_failed"), (Operation::ProvidersSet, "provider_configuration_failed"), (Operation::ProvidersDisable, "provider_configuration_failed"), (Operation::List, "probe_failed")] {
            let failure = management_failure(operation, &broken);
            assert_eq!(failure.reason, reason);
            assert!(!failure.message.contains("sk-123") && !failure.message.contains("/home/me"));
        }
        let wants_login = AcpError::Transport("Please log in first".into());
        assert_eq!(management_failure(Operation::Delete, &wants_login).reason, "authentication_failed");
    }
}
