use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use t3_client::environments::EnvironmentCatalog;
use t3_client::{
    connection::{
        ConnectionStatus, EnvironmentEndpoint, Heartbeat, HeartbeatAction, reconnect_delay_ms,
    },
    rpc::{RequestKind, RpcEvent, RpcSession},
    shell::ShellState,
    thread::ThreadState,
};
use t3_contracts::{
    AuthEnvironmentScope, EnvironmentId, SessionGrantInput, ShellStreamItem,
    client_rpc_required_scopes, rpc_required_scope,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Chat,
    Connections,
    Providers,
    Appearance,
}
#[derive(Debug, Clone)]
pub struct PendingMessage {
    pub destination: EnvironmentId,
    pub thread_id: String,
    pub text: String,
}
#[derive(Debug, Clone)]
pub struct PendingLaunch {
    pub destination: EnvironmentId,
    pub project_id: String,
    pub text: String,
    pub active_thread: Option<String>,
    pub choices: NewThreadChoices,
}
pub type NewThreadChoices = t3_client::draft_storage::DraftChoices;

#[derive(Debug, Clone, Default, Store)]
pub struct UiModel {
    pub status: ConnectionStatus,
    pub shell: ShellState,
    pub thread: ThreadState,
    pub active_thread: Option<String>,
    pub selected_project: Option<String>,
    pub config: Value,
    pub typed_config: Option<t3_contracts::ServerConfig>,
    pub client_settings: t3_contracts::ClientSettings,
    pub client_settings_error: Option<String>,
    pub draft_storage: crate::draft_storage::DraftHandle,
    pub draft_storage_error: Option<String>,
    pub sticky_models: std::collections::BTreeMap<EnvironmentId, t3_contracts::ModelSelection>,
    pub grants: SessionGrantInput,
    pub destination: Option<EnvironmentId>,
    pub environments: EnvironmentCatalog,
    pub error: Option<String>,
    pub draft: String,
    pub pending_messages: std::collections::BTreeMap<String, PendingMessage>,
    pub new_thread_drafts: std::collections::BTreeMap<(EnvironmentId, String), String>,
    pub new_thread_choices: std::collections::BTreeMap<(EnvironmentId, String), NewThreadChoices>,
    pub pending_launches: std::collections::BTreeMap<String, PendingLaunch>,
    pub view: View,
    pub dark: bool,
    pub sidebar_open: bool,
}
impl UiModel {
    #[cfg(test)]
    pub fn model_options(&self) -> Vec<(String, String, String)> {
        self.typed_config
            .as_ref()
            .map(|config| {
                t3_client::new_thread::available_models(
                    config,
                    &self.client_settings,
                    ui_surface(),
                    None,
                )
                .into_iter()
                .map(|row| {
                    (
                        row.instance_id.to_string(),
                        row.model.slug,
                        format!("{} · {}", row.provider_label, row.model.name),
                    )
                })
                .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Default)]
pub struct Transport {
    sender: Option<ewebsock::WsSender>,
    rpc: RpcSession,
    thread_subscription: Option<String>,
    generation: u64,
    opened: bool,
    thread_generation: u64,
    destination: Option<EnvironmentId>,
    endpoint: Option<EnvironmentEndpoint>,
    bearer_token: String,
    unary_waiters:
        std::collections::BTreeMap<String, futures_channel::oneshot::Sender<Result<Value, String>>>,
}
impl Transport {
    fn fail_waiters(&mut self) {
        for (_, waiter) in std::mem::take(&mut self.unary_waiters) {
            let _ = waiter.send(Err("Connection closed before the server replied.".into()));
        }
    }
}
#[derive(Clone, Default)]
pub struct TransportHandle(Rc<RefCell<Transport>>);
impl std::ops::Deref for TransportHandle {
    type Target = Rc<RefCell<Transport>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl PartialEq for TransportHandle {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

pub fn default_address() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|window| window.location().origin().ok())
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::var("T3_SERVER_URL").unwrap_or_else(|_| {
            if cfg!(feature = "mobile") {
                String::new()
            } else {
                "http://127.0.0.1:3774".into()
            }
        })
    }
}

fn browser_pairing(endpoint: &EnvironmentEndpoint) -> bool {
    cfg!(target_arch = "wasm32") && endpoint.is_same_origin(&default_address())
}

pub fn default_credential_kind() -> String {
    if cfg!(target_arch = "wasm32") {
        "session"
    } else {
        "pairing"
    }
    .into()
}

fn fail(state: Store<UiModel>, message: impl Into<String>) {
    state.error().set(Some(message.into()));
}

pub fn request(
    handle: &TransportHandle,
    state: Store<UiModel>,
    method: &str,
    payload: Value,
    kind: RequestKind,
) -> Option<String> {
    let required = match required_scopes(method, &payload) {
        Ok(scopes) => scopes,
        Err(error) => {
            fail(state, error);
            return None;
        }
    };
    let allowed = {
        let model = state.peek();
        model.destination.as_ref().is_some_and(|destination| {
            required
                .iter()
                .all(|scope| model.environments.allows(destination, *scope))
        })
    };
    if !allowed {
        fail(
            state,
            "This connection does not have the permission required for this action.",
        );
        return None;
    }
    let mut transport = handle.borrow_mut();
    if transport.sender.is_none()
        || state.peek().status != ConnectionStatus::Connected
        || transport.destination != state.peek().destination
    {
        fail(state, "Connect to your T3 Code server first.");
        return None;
    }
    let (id, frame) = transport.rpc.request(method, payload, kind);
    if let Some(sender) = transport.sender.as_mut() {
        sender.send(ewebsock::WsMessage::Text(frame.to_string()));
    }
    state.error().set(None);
    Some(id)
}

fn required_scopes(method: &str, payload: &Value) -> Result<Vec<AuthEnvironmentScope>, String> {
    let mut scopes =
        client_rpc_required_scopes(method, Some(payload)).map_err(|error| error.to_string())?;
    let server_scope = rpc_required_scope(method).map_err(|error| error.to_string())?;
    if !scopes.contains(&server_scope) {
        scopes.push(server_scope);
    }
    Ok(scopes)
}

pub async fn request_value(
    handle: TransportHandle,
    state: Store<UiModel>,
    method: &str,
    payload: Value,
) -> Result<Value, String> {
    let id = request(&handle, state, method, payload, RequestKind::Unary).ok_or_else(|| {
        state
            .peek()
            .error
            .clone()
            .unwrap_or_else(|| "Request unavailable.".into())
    })?;
    let (sender, receiver) = futures_channel::oneshot::channel();
    let generation = handle.borrow().generation;
    handle.borrow_mut().unary_waiters.insert(id.clone(), sender);
    let _guard = UnaryRequestGuard {
        handle: handle.clone(),
        id,
        generation,
    };
    receiver
        .await
        .map_err(|_| "Request was interrupted.".to_owned())?
}

struct UnaryRequestGuard {
    handle: TransportHandle,
    id: String,
    generation: u64,
}
impl Drop for UnaryRequestGuard {
    fn drop(&mut self) {
        let mut transport = self.handle.borrow_mut();
        if transport.generation != self.generation {
            return;
        }
        transport.unary_waiters.remove(&self.id);
        if let Some(frame) = transport.rpc.cancel(&self.id) {
            if let Some(sender) = transport.sender.as_mut() {
                sender.send(ewebsock::WsMessage::Text(frame.to_string()));
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResponseOwner {
    generation: u64,
    thread_generation: u64,
    destination: EnvironmentId,
    active_thread: Option<String>,
}
pub fn response_owner(handle: &TransportHandle, state: Store<UiModel>) -> Option<ResponseOwner> {
    let transport = handle.borrow();
    let model = state.peek();
    let destination = model.destination.clone()?;
    (transport.destination.as_ref() == Some(&destination)).then(|| ResponseOwner {
        generation: transport.generation,
        thread_generation: transport.thread_generation,
        destination,
        active_thread: model.active_thread.clone(),
    })
}

pub fn forget_environment(
    handle: &TransportHandle,
    mut state: Store<UiModel>,
    destination: &EnvironmentId,
) {
    if state.peek().destination.as_ref() == Some(destination) {
        let mut transport = handle.borrow_mut();
        transport.generation += 1;
        if let Some(mut sender) = transport.sender.take() {
            sender.close();
        }
        transport.rpc.disconnect();
        transport.fail_waiters();
        transport.destination = None;
        transport.endpoint = None;
        transport.bearer_token.clear();
        transport.thread_subscription = None;
        let mut model = state.write();
        model.destination = None;
        model.shell = ShellState::default();
        model.thread = ThreadState::default();
        model.active_thread = None;
        model.selected_project = None;
        model.config = Value::Null;
        model.typed_config = None;
        model.draft.clear();
        model.pending_messages.clear();
        model.pending_launches.clear();
        model.grants = SessionGrantInput::default();
        model.error = None;
        model.status = ConnectionStatus::Disconnected;
        model.view = View::Connections;
    }
    crate::draft_storage::forget(state, destination.as_str());
    state.environments().write().forget(destination);
    state.sticky_models().write().remove(destination);
    state
        .new_thread_drafts()
        .write()
        .retain(|(owner, _), _| owner != destination);
    state
        .new_thread_choices()
        .write()
        .retain(|(owner, _), _| owner != destination);
}

fn cancel_thread_subscription(handle: &TransportHandle) {
    {
        let mut transport = handle.borrow_mut();
        transport.thread_generation += 1;
        if let Some(id) = transport.thread_subscription.take() {
            if let Some(frame) = transport.rpc.cancel(&id) {
                if let Some(sender) = transport.sender.as_mut() {
                    sender.send(ewebsock::WsMessage::Text(frame.to_string()));
                }
            }
        }
    }
}

pub fn new_thread(handle: &TransportHandle, mut state: Store<UiModel>, project: Option<String>) {
    cancel_thread_subscription(handle);
    let mut model = state.write();
    save_current_environment(&mut model);
    if let Some(project) = project {
        model.selected_project = Some(project);
    }
    model.active_thread = None;
    model.thread = ThreadState::default();
    model.draft.clear();
    model.view = View::Chat;
    model.sidebar_open = false;
}

pub fn select_thread(handle: &TransportHandle, mut state: Store<UiModel>, thread_id: String) {
    cancel_thread_subscription(handle);
    {
        let mut model = state.write();
        save_current_environment(&mut model);
        model.draft = model
            .destination
            .as_ref()
            .and_then(|destination| model.environments.records.get(destination))
            .and_then(|record| record.cache.drafts.get(&thread_id))
            .cloned()
            .unwrap_or_default();
        model.active_thread = Some(thread_id.clone());
        model.thread = ThreadState::default();
        model.view = View::Chat;
        model.sidebar_open = false;
    }
    crate::draft_storage::restore_destination(state);
    let id = request(
        handle,
        state,
        "orchestration.subscribeThread",
        json!({"threadId":thread_id,"requestCompletionMarker":true,"acceptBoundedSnapshot":true}),
        RequestKind::Stream,
    );
    handle.borrow_mut().thread_subscription = id;
}

pub fn command(
    handle: &TransportHandle,
    state: Store<UiModel>,
    kind: &str,
    mut payload: Value,
) -> Option<String> {
    payload["type"] = json!(kind);
    payload["commandId"] = json!(uuid::Uuid::new_v4().to_string());
    request(
        handle,
        state,
        "orchestration.dispatchCommand",
        payload,
        RequestKind::Unary,
    )
}

pub fn ui_surface() -> t3_client::new_thread::Surface {
    if cfg!(feature = "mobile") {
        t3_client::new_thread::Surface::Mobile
    } else {
        t3_client::new_thread::Surface::Web
    }
}

pub fn launch_thread(
    handle: &TransportHandle,
    state: Store<UiModel>,
    project: &str,
    selection: &t3_contracts::ModelSelection,
    mode: t3_contracts::RuntimeMode,
    workspace: t3_contracts::ThreadLaunchWorkspaceStrategy,
    initial_text: Option<&str>,
) -> Option<String> {
    let owned = state.peek();
    let destination = owned.destination.clone()?;
    let active_thread = owned.active_thread.clone();
    let choices = owned
        .new_thread_choices
        .get(&(destination.clone(), project.into()))
        .cloned()
        .unwrap_or_default();
    if owned
        .pending_launches
        .values()
        .any(|pending| pending.destination == destination && pending.project_id == project)
    {
        return None;
    }
    if crate::draft_storage::has_unrendered(
        state,
        &t3_client::draft_storage::DraftTarget::project(destination.to_string(), project),
    ) {
        drop(owned);
        fail(
            state,
            "This saved draft contains attachments or context that the Rust composer cannot send yet. The original draft was preserved; open it in the original app to send all content.",
        );
        return None;
    }
    let config = owned.typed_config.as_ref()?;
    let normalized =
        t3_client::new_thread::launch_selection(config, &owned.client_settings, selection, mode);
    let Some((selection, mode)) = normalized else {
        drop(owned);
        fail(
            state,
            "Selected provider is unavailable on this environment.",
        );
        return None;
    };
    drop(owned);
    let mut input = json!({"commandId":uuid::Uuid::new_v4().to_string(),"projectId":project,"title":"New thread","generateTitle":false,"modelSelection":selection,"runtimeMode":mode,"interactionMode":"default","creationSource":creation_source(),"workspaceStrategy":workspace});
    let text = initial_text.filter(|text| !text.trim().is_empty());
    if let Some(text) = text {
        input["initialMessage"] =
            json!({"messageId":uuid::Uuid::new_v4().to_string(),"text":text,"attachments":[]});
    }
    // Validate the complete wire shape before creating a correlated mutation.
    let input = match serde_json::from_value::<t3_contracts::ThreadLaunchInput>(input)
        .and_then(serde_json::to_value)
    {
        Ok(input) => input,
        Err(error) => {
            fail(state, error.to_string());
            return None;
        }
    };
    let id = request(
        handle,
        state,
        "orchestration.launchThread",
        input,
        RequestKind::Unary,
    )?;
    state.pending_launches().write().insert(
        id.clone(),
        PendingLaunch {
            destination,
            project_id: project.into(),
            text: text.unwrap_or_default().into(),
            active_thread,
            choices,
        },
    );
    Some(id)
}

pub fn stop_thread(handle: &TransportHandle, state: Store<UiModel>, thread_id: &str) {
    let run = state
        .peek()
        .thread
        .projection
        .as_ref()
        .and_then(|projection| projection["runs"].as_array())
        .and_then(|runs| {
            runs.iter().rev().find(|run| {
                matches!(
                    run["status"].as_str(),
                    Some("preparing" | "starting" | "running" | "waiting")
                )
            })
        })
        .and_then(|run| run["id"].as_str())
        .map(str::to_owned);
    if let Some(run_id) = run {
        command(
            handle,
            state,
            "run.interrupt",
            json!({"threadId":thread_id,"runId":run_id,"holdQueue":true}),
        );
    }
}

pub fn send_message(handle: &TransportHandle, state: Store<UiModel>) {
    let model = state.peek();
    let Some(thread_id) = model.active_thread.clone() else {
        return;
    };
    let text = model.draft.clone();
    let Some(destination) = model.destination.clone() else {
        return;
    };
    if crate::draft_storage::has_unrendered(
        state,
        &t3_client::draft_storage::DraftTarget::thread(destination.to_string(), thread_id.clone()),
    ) {
        drop(model);
        fail(
            state,
            "This saved draft contains attachments or context that the Rust composer cannot send yet. The original draft was preserved; open it in the original app to send all content.",
        );
        return;
    }
    if text.trim().is_empty()
        || model
            .pending_messages
            .values()
            .any(|pending| pending.destination == destination && pending.thread_id == thread_id)
    {
        return;
    }
    drop(model);
    if let Some(id) = command(
        handle,
        state,
        "message.dispatch",
        json!({"threadId":thread_id,"messageId":uuid::Uuid::new_v4().to_string(),"text":text,"attachments":[],"createdBy":"user","creationSource":creation_source(),"dispatchMode":{"type":"start_immediately"},"deliveryIntent":"auto"}),
    ) {
        state.pending_messages().write().insert(
            id,
            PendingMessage {
                destination,
                thread_id,
                text,
            },
        );
    }
}

pub fn client_surface() -> &'static str {
    if cfg!(feature = "mobile") {
        "mobile"
    } else if cfg!(feature = "desktop") {
        "desktop"
    } else {
        "web"
    }
}
// The original CreationSource schema has no desktop variant. The desktop
// application's component surface records the same source as its web client.
pub fn creation_source() -> &'static str {
    creation_source_for(client_surface())
}
fn creation_source_for(surface: &str) -> &'static str {
    if surface == "mobile" { "mobile" } else { "web" }
}

fn save_current_environment(model: &mut UiModel) {
    if let Some(destination) = &model.destination {
        if let Some(record) = model.environments.records.get_mut(destination) {
            if let Some(thread) = &model.active_thread {
                record
                    .cache
                    .drafts
                    .insert(thread.clone(), model.draft.clone());
            }
            record.cache.shell = model.shell.clone();
            record.cache.active_thread = model.active_thread.clone();
            record.cache.selected_project = model.selected_project.clone();
            record.cache.thread = model.thread.clone();
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CookiePolicy {
    Omit,
    SameOrigin,
}
#[cfg(any(target_arch = "wasm32", test))]
fn cookie_policy(bearer: bool) -> CookiePolicy {
    if bearer {
        CookiePolicy::Omit
    } else {
        CookiePolicy::SameOrigin
    }
}

async fn authenticated_json(
    client: &reqwest::Client,
    endpoint: &EnvironmentEndpoint,
    path: &str,
    token: &str,
    post: bool,
) -> Result<Value, String> {
    let request = if post {
        client.post(endpoint.http(path))
    } else {
        client.get(endpoint.http(path))
    };
    read_json(request, token, path).await
}

async fn read_json(
    mut request: reqwest::RequestBuilder,
    token: &str,
    path: &str,
) -> Result<Value, String> {
    request = request.timeout(std::time::Duration::from_secs(15));
    if !token.is_empty() {
        request = request.bearer_auth(token);
    }
    #[cfg(target_arch = "wasm32")]
    {
        request = match cookie_policy(!token.is_empty()) {
            CookiePolicy::Omit => request.fetch_credentials_omit(),
            CookiePolicy::SameOrigin => request.fetch_credentials_same_origin(),
        };
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Could not reach your T3 Code server: {error}"))?;
    if !response.status().is_success() {
        return Err(if response.status().as_u16() == 401 {
            "Pair with this server or enter a valid access token to connect.".into()
        } else {
            format!("Server returned {} for {path}", response.status())
        });
    }
    response
        .json()
        .await
        .map_err(|error| format!("Invalid server response: {error}"))
}

pub async fn load_earlier(handle: TransportHandle, state: Store<UiModel>) -> Result<(), String> {
    let (generation, thread_generation, destination, thread_id, cursor, endpoint, token) = {
        let transport = handle.borrow();
        let model = state.peek();
        let Some(destination) = model.destination.clone() else {
            return Err("Connect to a server to load history.".into());
        };
        if transport.destination.as_ref() != Some(&destination)
            || model.status != ConnectionStatus::Connected
        {
            return Err("Reconnect to load history.".into());
        }
        if !model
            .environments
            .allows(&destination, AuthEnvironmentScope::OrchestrationRead)
        {
            return Err("This connection cannot read thread history.".into());
        }
        let Some(thread_id) = model.active_thread.clone() else {
            return Ok(());
        };
        let Some(cursor) = model
            .thread
            .history_cursor
            .clone()
            .filter(|_| model.thread.has_more_history)
        else {
            return Ok(());
        };
        let Some(endpoint) = transport.endpoint.clone() else {
            return Err("Connect to a server to load history.".into());
        };
        (
            transport.generation,
            transport.thread_generation,
            destination,
            thread_id,
            cursor,
            endpoint,
            transport.bearer_token.clone(),
        )
    };
    let request = reqwest::Client::new()
        .get(endpoint.thread_history(&thread_id, &cursor))
        .header("x-t3-orchestration-protocol", "2");
    let result = read_json(request, &token, "thread history").await;
    // An old destination or thread must never receive a page after navigation.
    let model = state.peek();
    if handle.borrow().generation != generation
        || handle.borrow().thread_generation != thread_generation
        || model.destination.as_ref() != Some(&destination)
        || model.active_thread.as_deref() != Some(&thread_id)
        || model.thread.history_cursor.as_deref() != Some(&cursor)
        || !model
            .environments
            .allows(&destination, AuthEnvironmentScope::OrchestrationRead)
    {
        return Ok(());
    }
    drop(model);
    let page = serde_json::from_value::<t3_contracts::ThreadHistoryPage>(result?)
        .and_then(serde_json::to_value)
        .map_err(|error| format!("Invalid thread history: {error}"))?;
    state
        .thread()
        .write()
        .merge_history_for_cursor(&cursor, &page)
        .map_err(str::to_owned)?;
    Ok(())
}

async fn delay(milliseconds: u64) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(milliseconds.min(u32::MAX as u64) as u32).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_millis(milliseconds)).await;
}

#[derive(Clone, Copy)]
enum SocketWake {
    Socket,
    Heartbeat,
}

fn interrupt_connection(handle: &TransportHandle, state: Store<UiModel>, reason: &str) {
    let mut transport = handle.borrow_mut();
    if let Some(mut sender) = transport.sender.take() {
        sender.close();
    }
    transport.rpc.disconnect();
    transport.fail_waiters();
    state
        .status()
        .set(ConnectionStatus::Interrupted(reason.to_owned()));
    state.pending_messages().write().clear();
    state.pending_launches().write().clear();
}

/// A bootstrap credential is exchanged once; it never becomes an ordinary
/// Authorization header or a WebSocket query parameter.
pub async fn pair_and_connect(
    handle: TransportHandle,
    state: Store<UiModel>,
    address: String,
    credential: String,
) {
    let generation = {
        let mut transport = handle.borrow_mut();
        transport.generation += 1;
        transport.generation
    };
    reset_for_connect(&handle, state);
    let result=async {
        if credential.trim().is_empty(){return Err("Enter the pairing credential generated by this server.".into());}
        let endpoint=EnvironmentEndpoint::new(&address).map_err(|error|error.to_string())?;
        let client=reqwest::Client::new();
        let descriptor=authenticated_json(&client,&endpoint,".well-known/t3/environment","",false).await?;
        let _:t3_contracts::ExecutionEnvironmentDescriptor=serde_json::from_value(descriptor.clone()).map_err(|error|format!("Invalid environment descriptor: {error}"))?;
        let destination:EnvironmentId=serde_json::from_value(descriptor["environmentId"].clone()).map_err(|error|format!("Invalid environment identity: {error}"))?;
        let protocol=descriptor["orchestrationProtocolVersion"].as_u64().unwrap_or(1);
        if handle.borrow().generation!=generation{return Err("Connection superseded.".into());}
        state.peek().environments.validate_identity(&endpoint,&destination,protocol).map_err(|error|error.to_string())?;
        if browser_pairing(&endpoint) {
            let response=read_json(client.post(endpoint.http("api/auth/browser-session")).json(&json!({"credential":credential.trim()})),"","browser pairing").await?;
            let _:t3_contracts::AuthBrowserSessionResult=serde_json::from_value(response).map_err(|error|format!("Invalid browser pairing response: {error}"))?;
            return Ok(String::new());
        }
        let response=read_json(client.post(endpoint.http("oauth/token")).form(&[
            ("grant_type","urn:ietf:params:oauth:grant-type:token-exchange"),
            ("subject_token",credential.trim()),
            ("subject_token_type","urn:t3:params:oauth:token-type:environment-bootstrap"),
            ("requested_token_type","urn:ietf:params:oauth:token-type:access_token"),
        ]),"","oauth/token").await?;
        let result:t3_contracts::AuthAccessTokenResult=serde_json::from_value(response).map_err(|error|format!("Invalid token exchange response: {error}"))?;
        if serde_json::to_value(result.token_type).map_err(|error|error.to_string())?!=json!("Bearer"){return Err("This server requires a proof-bound client session, which this Rust client does not yet support.".into());}
        Ok::<_,String>(result.access_token.to_string())
    }.await;
    if handle.borrow().generation != generation {
        return;
    }
    match result {
        Ok(token) => connect(handle, state, address, token).await,
        Err(error) => {
            state.status().set(ConnectionStatus::Blocked(error.clone()));
            fail(state, error);
        }
    }
}

pub async fn connect(
    handle: TransportHandle,
    state: Store<UiModel>,
    address: String,
    token: String,
) {
    let generation = {
        let mut transport = handle.borrow_mut();
        transport.generation += 1;
        transport.generation
    };
    let mut failures = 0;
    loop {
        connect_once(
            handle.clone(),
            state,
            address.clone(),
            token.clone(),
            generation,
        )
        .await;
        if handle.borrow().generation != generation
            || !matches!(&state.peek().status, ConnectionStatus::Interrupted(_))
        {
            return;
        }
        if handle.borrow().opened {
            failures = 0;
        }
        delay(reconnect_delay_ms(failures)).await;
        failures = failures.saturating_add(1);
        if handle.borrow().generation != generation {
            return;
        }
    }
}

fn reset_for_connect(handle: &TransportHandle, mut state: Store<UiModel>) {
    {
        let mut transport = handle.borrow_mut();
        if let Some(mut sender) = transport.sender.take() {
            sender.close();
        }
        transport.rpc.disconnect();
        transport.fail_waiters();
        transport.opened = false;
        transport.rpc = RpcSession::default();
        transport.thread_subscription = None;
        transport.destination = None;
    }
    {
        let mut model = state.write();
        save_current_environment(&mut model);
        if let Some(destination) = model.destination.clone() {
            model.environments.revoke(&destination);
        }
        model.destination = None;
        model.shell = ShellState::default();
        model.thread = ThreadState::default();
        model.active_thread = None;
        model.selected_project = None;
        model.config = Value::Null;
        model.typed_config = None;
        model.draft.clear();
        model.status = ConnectionStatus::Connecting;
        model.error = None;
        model.pending_messages.clear();
        model.pending_launches.clear();
        model.grants = SessionGrantInput::default();
    }
}

async fn connect_once(
    handle: TransportHandle,
    mut state: Store<UiModel>,
    address: String,
    token: String,
    generation: u64,
) {
    reset_for_connect(&handle, state);
    let endpoint = match EnvironmentEndpoint::new(&address) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            fail(state, error.to_string());
            state.status().set(ConnectionStatus::Disconnected);
            state.view().set(View::Connections);
            return;
        }
    };
    let client = reqwest::Client::new();
    let authorization = async {
        let descriptor =
            authenticated_json(&client, &endpoint, ".well-known/t3/environment", "", false).await?;
        let _: t3_contracts::ExecutionEnvironmentDescriptor =
            serde_json::from_value(descriptor.clone())
                .map_err(|error| format!("Invalid environment descriptor: {error}"))?;
        let destination: EnvironmentId =
            serde_json::from_value(descriptor["environmentId"].clone())
                .map_err(|error| format!("Invalid environment identity: {error}"))?;
        let protocol = descriptor["orchestrationProtocolVersion"]
            .as_u64()
            .unwrap_or(1);
        let label = descriptor["label"].as_str().unwrap_or(&address).to_owned();
        if handle.borrow().generation != generation {
            return Err("Connection superseded.".to_owned());
        }
        state
            .peek()
            .environments
            .validate_identity(&endpoint, &destination, protocol)
            .map_err(|error| error.to_string())?;
        let session =
            authenticated_json(&client, &endpoint, "api/auth/session", &token, false).await?;
        let session: t3_contracts::AuthSessionState = serde_json::from_value(session)
            .map_err(|error| format!("Invalid session response: {error}"))?;
        let grants = SessionGrantInput::from(&session);
        let ticket = authenticated_json(
            &client,
            &endpoint,
            "api/auth/websocket-ticket",
            &token,
            true,
        )
        .await?;
        let ticket = ticket["ticket"]
            .as_str()
            .ok_or("Server did not issue a WebSocket ticket")?
            .to_owned();
        Ok::<_, String>((destination, label, protocol, grants, ticket))
    }
    .await;
    if handle.borrow().generation != generation {
        return;
    }
    let (destination, label, protocol, grants, ticket) = match authorization {
        Ok(authorization) => authorization,
        Err(error) => {
            state.status().set(
                if error.starts_with("Could not reach your T3 Code server:") {
                    ConnectionStatus::Interrupted(error.clone())
                } else {
                    ConnectionStatus::Blocked(error.clone())
                },
            );
            if matches!(*state.status().peek(), ConnectionStatus::Blocked(_)) {
                state.view().set(View::Connections);
            }
            fail(state, error);
            return;
        }
    };
    let registration =
        state
            .environments()
            .write()
            .register(&endpoint, destination.clone(), label, protocol);
    if let Err(error) = registration {
        let error = error.to_string();
        state.status().set(ConnectionStatus::Blocked(error.clone()));
        fail(state, error);
        return;
    }
    let cache = state.peek().environments.records[&destination]
        .cache
        .clone();
    {
        let mut model = state.write();
        model.shell = cache.shell;
        model.thread = cache.thread;
        model.active_thread = cache.active_thread;
        model.selected_project = cache.selected_project;
        model.draft = model
            .active_thread
            .as_ref()
            .and_then(|thread| cache.drafts.get(thread))
            .cloned()
            .unwrap_or_default();
        model.destination = Some(destination.clone());
        model
            .environments
            .set_session(&destination, grants.clone())
            .expect("registered environment");
    }
    crate::draft_storage::restore_destination(state);
    let (wake_sender, mut wake_receiver) = futures_channel::mpsc::unbounded::<SocketWake>();
    let heartbeat_sender = wake_sender.clone();
    let socket_url = endpoint.socket(Some(&ticket), client_surface());
    let (sender, receiver) = match ewebsock::connect_with_wakeup(
        socket_url.to_string(),
        ewebsock::Options::default(),
        move || {
            let _ = wake_sender.unbounded_send(SocketWake::Socket);
        },
    ) {
        Ok(socket) => socket,
        Err(error) => {
            fail(state, error.to_string());
            state
                .status()
                .set(ConnectionStatus::Interrupted(error.to_string()));
            return;
        }
    };
    {
        let mut transport = handle.borrow_mut();
        transport.sender = Some(sender);
        transport.destination = Some(destination);
        transport.endpoint = Some(endpoint);
        transport.bearer_token = token;
    }
    state.grants().set(grants);
    // This timer never writes render state while the connection is healthy.
    // Socket traffic cannot postpone the protocol heartbeat.
    spawn(async move {
        loop {
            delay(5000).await;
            if heartbeat_sender
                .unbounded_send(SocketWake::Heartbeat)
                .is_err()
            {
                return;
            }
        }
    });
    let mut heartbeat = Heartbeat::default();
    loop {
        if handle.borrow().generation != generation {
            return;
        }
        while let Some(event) = receiver.try_recv() {
            match event {
                ewebsock::WsEvent::Opened => {
                    heartbeat.reset();
                    handle.borrow_mut().opened = true;
                    state.status().set(ConnectionStatus::Connected);
                    request(
                        &handle,
                        state,
                        "server.getConfig",
                        json!({}),
                        RequestKind::Unary,
                    );
                    // Old server identities may be cached in the screen during
                    // reconnect; until persistent cache validation is ported,
                    // request an authoritative shell rather than resuming it.
                    request(
                        &handle,
                        state,
                        "orchestration.subscribeShell",
                        json!({"requestCompletionMarker":true}),
                        RequestKind::Stream,
                    );
                    let active = state.peek().active_thread.clone();
                    if let Some(active) = active {
                        select_thread(&handle, state, active);
                    }
                }
                ewebsock::WsEvent::Message(ewebsock::WsMessage::Text(text)) => {
                    let result = handle.borrow_mut().rpc.receive(&text);
                    match result {
                        Ok((events, frames)) => {
                            if let Some(sender) = handle.borrow_mut().sender.as_mut() {
                                for frame in frames {
                                    sender.send(ewebsock::WsMessage::Text(frame.to_string()));
                                }
                            }
                            for event in events {
                                if matches!(&event, RpcEvent::Pong) {
                                    heartbeat.pong();
                                }
                                apply_rpc_event(&handle, state, event);
                            }
                        }
                        Err(error) => fail(state, error.to_string()),
                    }
                }
                ewebsock::WsEvent::Closed | ewebsock::WsEvent::Error(_) => {
                    interrupt_connection(&handle, state, "Connection interrupted. Reconnecting…");
                    return;
                }
                _ => {}
            }
        }
        let wake = wake_receiver.next().await;
        if handle.borrow().generation != generation {
            return;
        }
        match wake {
            Some(SocketWake::Heartbeat) if state.peek().status == ConnectionStatus::Connected => {
                match heartbeat.tick() {
                    HeartbeatAction::Ping => {
                        if let Some(sender) = handle.borrow_mut().sender.as_mut() {
                            sender.send(ewebsock::WsMessage::Text(
                                json!({"_tag":"Ping"}).to_string(),
                            ));
                        }
                    }
                    HeartbeatAction::Timeout => {
                        interrupt_connection(
                            &handle,
                            state,
                            "Server heartbeat timed out. Reconnecting…",
                        );
                        return;
                    }
                }
            }
            Some(_) => {}
            None => return,
        }
    }
}

fn apply_rpc_event(handle: &TransportHandle, state: Store<UiModel>, event: RpcEvent) {
    match event {
        RpcEvent::Values { method, values, .. } => {
            // The source RPC codec decodes the complete chunk before exposing it
            // to subscribers. Avoid committing a prefix of a malformed chunk.
            if method == "orchestration.subscribeShell" {
                let decoded: Result<Vec<ShellStreamItem>, _> =
                    values.into_iter().map(serde_json::from_value).collect();
                match decoded {
                    Ok(items) => {
                        let mut shell_store = state.shell();
                        let mut shell = shell_store.write();
                        for item in items {
                            shell.apply(item);
                        }
                    }
                    Err(error) => fail(state, format!("Could not decode shell update: {error}")),
                }
            } else if method == "orchestration.subscribeThread" {
                let decoded: Result<Vec<Value>, _> =
                    values.into_iter().map(decode_thread_stream).collect();
                match decoded {
                    Ok(items) => {
                        let mut thread_store = state.thread();
                        let mut thread = thread_store.write();
                        for item in items {
                            if let Err(error) = thread.apply(&item) {
                                fail(state, error);
                                break;
                            }
                        }
                    }
                    Err(error) => fail(state, error),
                }
            }
        }
        RpcEvent::Complete { id, method, value } => {
            if let Some(waiter) = handle.borrow_mut().unary_waiters.remove(&id) {
                let _ = waiter.send(Ok(value.clone()));
            }
            if method == "server.getConfig" {
                let value = match serde_json::from_value::<t3_contracts::ServerConfig>(value)
                    .and_then(serde_json::to_value)
                {
                    Ok(value) => value,
                    Err(error) => {
                        fail(state, format!("Invalid server configuration: {error}"));
                        return;
                    }
                };
                let expected = state.peek().destination.clone();
                if expected.as_ref().is_none_or(|expected| {
                    value["environment"]["environmentId"].as_str() != Some(expected.as_str())
                }) {
                    state.status().set(ConnectionStatus::Blocked(
                        "Server identity changed during connection".into(),
                    ));
                    fail(state, "Server identity changed during connection");
                    if let Some(mut sender) = handle.borrow_mut().sender.take() {
                        sender.close();
                    }
                    return;
                }
                let typed =
                    serde_json::from_value(value.clone()).expect("configuration validated above");
                state.typed_config().set(Some(typed));
                state.config().set(value);
            } else if method == "orchestration.launchThread" {
                match serde_json::from_value::<t3_contracts::ThreadLaunchResult>(value) {
                    Ok(receipt) => {
                        finish_pending_launch(handle, state, &id, Some(receipt.thread_id.as_str()))
                    }
                    Err(error) => {
                        state.pending_launches().write().remove(&id);
                        fail(state, format!("Invalid thread launch receipt: {error}"));
                    }
                }
            }
            finish_pending_message(state, &id, true);
        }
        RpcEvent::Failed { id, cause, .. } => {
            if let Some(waiter) = handle.borrow_mut().unary_waiters.remove(&id) {
                let _ = waiter.send(Err(rpc_error_message(&cause)));
            }
            finish_pending_message(state, &id, false);
            state.pending_launches().write().remove(&id);
            fail(state, rpc_error_message(&cause));
        }
        RpcEvent::Defect(error) => fail(state, rpc_error_message(&error)),
        _ => {}
    }
}

fn finish_pending_launch(
    handle: &TransportHandle,
    state: Store<UiModel>,
    id: &str,
    thread_id: Option<&str>,
) {
    let pending = state.pending_launches().write().remove(id);
    let Some(pending) = pending else {
        return;
    };
    let Some(thread_id) = thread_id else {
        fail(
            state,
            "The server returned an invalid thread launch receipt.",
        );
        return;
    };
    let key = (pending.destination.clone(), pending.project_id.clone());
    let unchanged_text = state
        .new_thread_drafts()
        .peek()
        .get(&key)
        .map(String::as_str)
        .unwrap_or_default()
        == pending.text;
    let unchanged_choices = state
        .new_thread_choices()
        .peek()
        .get(&key)
        .cloned()
        .unwrap_or_default()
        == pending.choices;
    if unchanged_text && unchanged_choices {
        state.new_thread_drafts().write().remove(&key);
        state.new_thread_choices().write().remove(&key);
        crate::draft_storage::acknowledge(
            state,
            t3_client::draft_storage::DraftTarget::project(
                pending.destination.to_string(),
                pending.project_id.clone(),
            ),
        );
    }
    let current = {
        let model = state.peek();
        model.destination.as_ref() == Some(&pending.destination)
            && model.active_thread == pending.active_thread
            && model
                .selected_project
                .as_deref()
                .is_none_or(|project| project == pending.project_id)
    };
    if current {
        select_thread(handle, state, thread_id.to_owned());
    }
}

/// Decode the complete available contract before acquiring a mutable state
/// guard. Unknown future events advance the cursor without applying payloads.
fn decode_thread_stream(value: Value) -> Result<Value, String> {
    let decoded = serde_json::from_value::<t3_contracts::ThreadStreamItem>(value.clone())
        .map_err(|error| format!("Invalid thread update: {error}"))?;
    match decoded {
        t3_contracts::ThreadStreamItem::UnknownEvent {
            sequence,
            event_type,
        } => Ok(json!({"kind":"unknown-event","sequence":sequence,"eventType":event_type})),
        t3_contracts::ThreadStreamItem::Event {
            sequence,
            mut event,
        } => {
            event.payload = t3_contracts::normalize_event_payload(&event.event_type, event.payload)
                .map_err(|error| format!("Invalid thread update: {error}"))?;
            Ok(json!({"kind":"event","sequence":sequence,"event":event}))
        }
        decoded => {
            serde_json::to_value(decoded).map_err(|error| format!("Invalid thread update: {error}"))
        }
    }
}

fn finish_pending_message(state: Store<UiModel>, id: &str, success: bool) {
    let pending = state.pending_messages().write().remove(id);
    let Some(pending) = pending.filter(|_| success) else {
        return;
    };
    // A receipt belongs to its original destination and conversation even when
    // the user has navigated elsewhere while the request was in flight.
    let current = {
        let model = state.peek();
        model.destination.as_ref() == Some(&pending.destination)
            && model.active_thread.as_deref() == Some(pending.thread_id.as_str())
    };
    let target = t3_client::draft_storage::DraftTarget::thread(
        pending.destination.to_string(),
        pending.thread_id.clone(),
    );
    let unchanged_saved =
        state.peek().draft_storage.document.borrow().prompt(&target) == Some(pending.text.as_str());
    if unchanged_saved {
        crate::draft_storage::acknowledge(state, target);
    }
    if current && *state.draft().peek() == pending.text {
        state.draft().set(String::new());
        crate::draft_storage::acknowledge(
            state,
            t3_client::draft_storage::DraftTarget::thread(
                pending.destination.to_string(),
                pending.thread_id.clone(),
            ),
        );
    }
    if let Some(record) = state
        .environments()
        .write()
        .records
        .get_mut(&pending.destination)
    {
        if record.cache.drafts.get(&pending.thread_id) == Some(&pending.text) {
            record.cache.drafts.remove(&pending.thread_id);
        }
    }
}

fn rpc_error_message(value: &Value) -> String {
    if let Some(message) = value["message"].as_str() {
        return message.to_owned();
    }
    if let Some(causes) = value.as_array() {
        for cause in causes {
            if let Some(message) = cause["error"]["message"]
                .as_str()
                .or_else(|| cause["defect"].as_str())
            {
                return message.to_owned();
            }
        }
    }
    "The server could not complete this request.".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone)]
    struct ReceiptHarness(std::rc::Rc<RefCell<Option<Store<UiModel>>>>);
    fn receipt_harness(props: ReceiptHarness) -> Element {
        let state = use_store(UiModel::default);
        *props.0.borrow_mut() = Some(state);
        rsx! {}
    }
    #[test]
    fn malformed_thread_chunk_does_not_commit_a_valid_prefix() {
        let props = ReceiptHarness(Rc::new(RefCell::new(None)));
        let mut dom = VirtualDom::new_with_props(receipt_harness, props.clone());
        dom.rebuild_in_place();
        let state = props.0.borrow().unwrap();
        apply_rpc_event(
            &TransportHandle::default(),
            state,
            RpcEvent::Values {
                id: "subscription".into(),
                method: "orchestration.subscribeThread".into(),
                values: vec![
                    json!({"kind":"event","sequence":5,"event":{"type":"future.event"}}),
                    json!({"kind":"snapshot","snapshotSequence":6,"projection":{"thread":{"id":"thread"}}}),
                ],
            },
        );
        assert_eq!(state.thread().peek().sequence, 0);
        assert!(state.thread().peek().projection.is_none());
        assert!(
            state
                .error()
                .peek()
                .as_ref()
                .is_some_and(|error| error.starts_with("Invalid thread update:"))
        );
    }

    #[test]
    fn delayed_message_receipts_only_clear_owned_unchanged_drafts() {
        let props = ReceiptHarness(Rc::new(RefCell::new(None)));
        let mut dom = VirtualDom::new_with_props(receipt_harness, props.clone());
        dom.rebuild_in_place();
        let mut state = props.0.borrow().unwrap();
        let environment_a: EnvironmentId = serde_json::from_value(json!("environment-a")).unwrap();
        let environment_b: EnvironmentId = serde_json::from_value(json!("environment-b")).unwrap();
        let endpoint_a = EnvironmentEndpoint::new("http://environment-a").unwrap();
        let endpoint_b = EnvironmentEndpoint::new("http://environment-b").unwrap();
        {
            let mut model = state.write();
            model
                .environments
                .register(&endpoint_a, environment_a.clone(), "A".into(), 2)
                .unwrap();
            model
                .environments
                .register(&endpoint_b, environment_b.clone(), "B".into(), 2)
                .unwrap();
            model.destination = Some(environment_b.clone());
            model.active_thread = Some("same-thread".into());
            model.draft = "identical text".into();
            model
                .environments
                .records
                .get_mut(&environment_a)
                .unwrap()
                .cache
                .drafts
                .insert("same-thread".into(), "identical text".into());
            model
                .environments
                .records
                .get_mut(&environment_b)
                .unwrap()
                .cache
                .drafts
                .insert("other-thread".into(), "edited while pending".into());
            model.pending_messages.insert(
                "old-environment".into(),
                PendingMessage {
                    destination: environment_a.clone(),
                    thread_id: "same-thread".into(),
                    text: "identical text".into(),
                },
            );
            model.pending_messages.insert(
                "other-thread".into(),
                PendingMessage {
                    destination: environment_b.clone(),
                    thread_id: "other-thread".into(),
                    text: "original text".into(),
                },
            );
            model.pending_messages.insert(
                "failed-current".into(),
                PendingMessage {
                    destination: environment_b.clone(),
                    thread_id: "same-thread".into(),
                    text: "identical text".into(),
                },
            );
        }
        finish_pending_message(state, "old-environment", true);
        assert_eq!(*state.draft().peek(), "identical text");
        assert!(
            !state.peek().environments.records[&environment_a]
                .cache
                .drafts
                .contains_key("same-thread")
        );
        finish_pending_message(state, "other-thread", true);
        assert_eq!(*state.draft().peek(), "identical text");
        assert_eq!(
            state.peek().environments.records[&environment_b]
                .cache
                .drafts["other-thread"],
            "edited while pending"
        );
        finish_pending_message(state, "failed-current", false);
        assert_eq!(*state.draft().peek(), "identical text");
        assert!(state.pending_messages().peek().is_empty());
        state.pending_messages().write().insert(
            "confirmed-current".into(),
            PendingMessage {
                destination: environment_b,
                thread_id: "same-thread".into(),
                text: "identical text".into(),
            },
        );
        finish_pending_message(state, "confirmed-current", true);
        assert!(state.draft().peek().is_empty());
    }

    #[test]
    fn launch_receipts_keep_edited_drafts_and_do_not_navigate_another_destination() {
        let props = ReceiptHarness(Rc::new(RefCell::new(None)));
        let mut dom = VirtualDom::new_with_props(receipt_harness, props.clone());
        dom.rebuild_in_place();
        let mut state = props.0.borrow().unwrap();
        let a = EnvironmentId::new("environment-a").unwrap();
        let b = EnvironmentId::new("environment-b").unwrap();
        let key = (a.clone(), "same-project".into());
        {
            let mut model = state.write();
            model.destination = Some(b);
            model.active_thread = Some("current-thread".into());
            model
                .new_thread_drafts
                .insert(key.clone(), "edited while pending".into());
            model.pending_launches.insert(
                "old-launch".into(),
                PendingLaunch {
                    destination: a.clone(),
                    project_id: "same-project".into(),
                    text: "original".into(),
                    active_thread: None,
                    choices: NewThreadChoices::default(),
                },
            );
        }
        finish_pending_launch(
            &TransportHandle::default(),
            state,
            "old-launch",
            Some("launched-thread"),
        );
        assert_eq!(
            state
                .new_thread_drafts()
                .peek()
                .get(&key)
                .map(String::as_str),
            Some("edited while pending")
        );
        assert_eq!(
            state.active_thread().peek().as_deref(),
            Some("current-thread")
        );
        state.pending_launches().write().insert(
            "confirmed".into(),
            PendingLaunch {
                destination: a,
                project_id: "same-project".into(),
                text: "edited while pending".into(),
                active_thread: None,
                choices: NewThreadChoices::default(),
            },
        );
        finish_pending_launch(
            &TransportHandle::default(),
            state,
            "confirmed",
            Some("launched-thread"),
        );
        assert!(!state.new_thread_drafts().peek().contains_key(&key));
        assert_eq!(
            state.active_thread().peek().as_deref(),
            Some("current-thread")
        );
        // Opening another thread on the same environment is also navigation,
        // even when project and thread identifiers are otherwise identical.
        state.destination().set(Some(key.0.clone()));
        state.pending_launches().write().insert(
            "navigated".into(),
            PendingLaunch {
                destination: key.0.clone(),
                project_id: key.1.clone(),
                text: String::new(),
                active_thread: None,
                choices: NewThreadChoices::default(),
            },
        );
        finish_pending_launch(
            &TransportHandle::default(),
            state,
            "navigated",
            Some("launched-thread"),
        );
        assert_eq!(
            state.active_thread().peek().as_deref(),
            Some("current-thread")
        );
        state
            .new_thread_drafts()
            .write()
            .insert(key.clone(), "retain after malformed receipt".into());
        state.pending_launches().write().insert(
            "malformed".into(),
            PendingLaunch {
                destination: key.0.clone(),
                project_id: key.1.clone(),
                text: "retain after malformed receipt".into(),
                active_thread: None,
                choices: NewThreadChoices::default(),
            },
        );
        apply_rpc_event(
            &TransportHandle::default(),
            state,
            RpcEvent::Complete {
                id: "malformed".into(),
                method: "orchestration.launchThread".into(),
                value: json!({"threadId":"thread","projection":{},"resumed":false}),
            },
        );
        assert_eq!(
            state
                .new_thread_drafts()
                .peek()
                .get(&key)
                .map(String::as_str),
            Some("retain after malformed receipt")
        );
        assert!(!state.pending_launches().peek().contains_key("malformed"));
        assert!(
            state
                .error()
                .peek()
                .as_ref()
                .unwrap()
                .starts_with("Invalid thread launch receipt")
        );
    }

    #[test]
    fn launch_receipt_does_not_clear_edited_permissions_with_unchanged_prompt() {
        let props = ReceiptHarness(Rc::new(RefCell::new(None)));
        let mut dom = VirtualDom::new_with_props(receipt_harness, props.clone());
        dom.rebuild_in_place();
        let state = props.0.borrow().unwrap();
        let key = (
            EnvironmentId::new("environment").unwrap(),
            "project".to_owned(),
        );
        state
            .new_thread_drafts()
            .write()
            .insert(key.clone(), "same prompt".into());
        state.new_thread_choices().write().insert(
            key.clone(),
            NewThreadChoices {
                runtime_mode: Some(t3_contracts::RuntimeMode::ApprovalRequired),
                ..Default::default()
            },
        );
        state.pending_launches().write().insert(
            "launch".into(),
            PendingLaunch {
                destination: key.0.clone(),
                project_id: key.1.clone(),
                text: "same prompt".into(),
                active_thread: None,
                choices: NewThreadChoices {
                    runtime_mode: Some(t3_contracts::RuntimeMode::FullAccess),
                    ..Default::default()
                },
            },
        );
        finish_pending_launch(&TransportHandle::default(), state, "launch", Some("thread"));
        assert_eq!(
            state
                .new_thread_drafts()
                .peek()
                .get(&key)
                .map(String::as_str),
            Some("same prompt")
        );
        assert_eq!(
            state
                .new_thread_choices()
                .peek()
                .get(&key)
                .unwrap()
                .runtime_mode,
            Some(t3_contracts::RuntimeMode::ApprovalRequired)
        );
    }

    #[test]
    fn malformed_known_thread_event_is_rejected_but_future_event_advances_cursor() {
        assert!(decode_thread_stream(json!({"kind":"event","sequence":5,"event":{"id":"event","threadId":"thread","type":"thread.metadata-updated","occurredAt":"2026-10-07T00:00:00Z","payload":{"id":"thread"}}})).is_err());
        let value=decode_thread_stream(json!({"kind":"event","sequence":6,"event":{"type":"future.event","payload":{"bad":"unknown"}}})).unwrap();
        let mut thread = ThreadState::default();
        assert!(!thread.apply(&value).unwrap());
        assert_eq!(thread.sequence, 6);
    }
    #[test]
    fn known_stream_event_uses_source_normalized_payload() {
        let value=decode_thread_stream(json!({"kind":"event","sequence":7,"event":{"id":"  event  ","threadId":"  thread  ","type":"provider-session.detached","occurredAt":"2026-10-07T00:00:00Z","payload":{"providerSessionId":"  session  ","detachedAt":"2026-10-07T00:00:00Z","unrecognized":"discarded by source codec"}}})).unwrap();
        assert_eq!(value["event"]["id"], "event");
        assert_eq!(value["event"]["threadId"], "thread");
        assert_eq!(value["event"]["payload"]["providerSessionId"], "session");
        assert!(value["event"]["payload"].get("unrecognized").is_none());
    }
    #[test]
    fn cancelled_detail_read_releases_waiter_and_suppresses_late_values() {
        let handle = TransportHandle::default();
        let (id, _) = handle.borrow_mut().rpc.request(
            "orchestration.getTurnItem",
            json!({}),
            RequestKind::Unary,
        );
        let (sender, _receiver) = futures_channel::oneshot::channel();
        handle.borrow_mut().unary_waiters.insert(id.clone(), sender);
        drop(UnaryRequestGuard {
            handle: handle.clone(),
            id: id.clone(),
            generation: 0,
        });
        assert!(handle.borrow().unary_waiters.is_empty());
        let (events, acks) = handle
            .borrow_mut()
            .rpc
            .receive(
                &json!({"_tag":"Chunk","requestId":id,"values":[{"item":{"output":"late"}}]})
                    .to_string(),
            )
            .unwrap();
        assert!(events.is_empty());
        assert_eq!(acks.len(), 1);
    }
    #[test]
    fn old_cancelled_read_cannot_remove_reused_request_id_on_new_connection() {
        let handle = TransportHandle::default();
        let (old_id, _) = handle.borrow_mut().rpc.request(
            "orchestration.getTurnItem",
            json!({}),
            RequestKind::Unary,
        );
        let guard = UnaryRequestGuard {
            handle: handle.clone(),
            id: old_id.clone(),
            generation: 0,
        };
        {
            let mut transport = handle.borrow_mut();
            transport.generation = 1;
            transport.rpc = RpcSession::default();
        }
        let (id, _) = handle.borrow_mut().rpc.request(
            "orchestration.getTurnItem",
            json!({}),
            RequestKind::Unary,
        );
        assert_eq!(id, old_id);
        let (sender, _receiver) = futures_channel::oneshot::channel();
        handle.borrow_mut().unary_waiters.insert(id.clone(), sender);
        drop(guard);
        assert!(handle.borrow().unary_waiters.contains_key(&id));
        let (events,_)=handle.borrow_mut().rpc.receive(&json!({"_tag":"Exit","requestId":id,"exit":{"_tag":"Success","value":{"item":null}}}).to_string()).unwrap();
        assert!(matches!(&events[0], RpcEvent::Complete { .. }));
    }
    #[test]
    fn bearer_requests_omit_cookies_for_hosted_remote_browser_compatibility() {
        assert_eq!(cookie_policy(true), CookiePolicy::Omit);
        assert_eq!(cookie_policy(false), CookiePolicy::SameOrigin);
    }
    #[test]
    fn desktop_metadata_and_creation_source_follow_distinct_wire_schemas() {
        assert_eq!(creation_source_for("desktop"), "web");
        assert_eq!(creation_source_for("web"), "web");
        assert_eq!(creation_source_for("mobile"), "mobile");
        for surface in ["web", "desktop", "mobile"] {
            let source = creation_source_for(surface);
            let _: t3_contracts::CreationSource = serde_json::from_value(json!(source)).unwrap();
        }
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod transport_tests;
