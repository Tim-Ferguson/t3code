use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use t3_client::environments::EnvironmentCatalog;
use t3_client::{
    connection::{ConnectionStatus, EnvironmentEndpoint},
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
#[derive(Debug, Clone, Default, Store)]
pub struct UiModel {
    pub status: ConnectionStatus,
    pub shell: ShellState,
    pub thread: ThreadState,
    pub active_thread: Option<String>,
    pub selected_project: Option<String>,
    pub config: Value,
    pub grants: SessionGrantInput,
    pub destination: Option<EnvironmentId>,
    pub environments: EnvironmentCatalog,
    pub error: Option<String>,
    pub draft: String,
    pub pending_message: Option<(String, String)>,
    pub view: View,
    pub dark: bool,
    pub sidebar_open: bool,
}
impl UiModel {
    pub fn model_options(&self) -> Vec<(String, String, String)> {
        self.config["providers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|provider| {
                provider["enabled"].as_bool() == Some(true)
                    && provider["installed"].as_bool() == Some(true)
                    && provider["availability"] != "unavailable"
            })
            .flat_map(|provider| {
                let instance = provider["instanceId"].as_str().unwrap_or("").to_owned();
                let label = provider["displayName"]
                    .as_str()
                    .or_else(|| provider["driver"].as_str())
                    .unwrap_or(&instance)
                    .to_owned();
                provider["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(move |model| {
                        let slug = model["slug"].as_str()?;
                        Some((
                            instance.clone(),
                            slug.to_owned(),
                            format!("{} · {}", label, model["name"].as_str().unwrap_or(slug)),
                        ))
                    })
            })
            .collect()
    }
}

#[derive(Default)]
pub struct Transport {
    sender: Option<ewebsock::WsSender>,
    rpc: RpcSession,
    thread_subscription: Option<String>,
    generation: u64,
    thread_generation: u64,
    destination: Option<EnvironmentId>,
    endpoint: Option<EnvironmentEndpoint>,
    bearer_token: String,
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
        std::env::var("T3_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:3773".into())
    }
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

pub fn select_thread(handle: &TransportHandle, mut state: Store<UiModel>, thread_id: String) {
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

pub fn send_message(handle: &TransportHandle, state: Store<UiModel>) {
    let model = state.peek();
    let Some(thread_id) = model.active_thread.clone() else {
        return;
    };
    let text = model.draft.clone();
    if text.trim().is_empty() || model.pending_message.is_some() {
        return;
    }
    drop(model);
    if let Some(id) = command(
        handle,
        state,
        "message.dispatch",
        json!({"threadId":thread_id,"messageId":uuid::Uuid::new_v4().to_string(),"text":text,"attachments":[],"createdBy":"user","creationSource":creation_source(),"dispatchMode":{"type":"start_immediately"},"deliveryIntent":"auto"}),
    ) {
        state.pending_message().set(Some((id, text)));
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
    let page = result?;
    state
        .thread()
        .write()
        .merge_history_for_cursor(&cursor, &page)
        .map_err(str::to_owned)?;
    Ok(())
}

pub async fn connect(
    handle: TransportHandle,
    mut state: Store<UiModel>,
    address: String,
    token: String,
) {
    let generation = {
        let mut transport = handle.borrow_mut();
        transport.generation += 1;
        if let Some(mut sender) = transport.sender.take() {
            sender.close();
        }
        transport.rpc.disconnect();
        transport.rpc = RpcSession::default();
        transport.thread_subscription = None;
        transport.destination = None;
        transport.generation
    };
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
        model.draft.clear();
        model.status = ConnectionStatus::Connecting;
        model.error = None;
        model.pending_message = None;
        model.grants = SessionGrantInput::default();
    }
    let endpoint = match EnvironmentEndpoint::new(&address) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            fail(state, error.to_string());
            state.status().set(ConnectionStatus::Disconnected);
            return;
        }
    };
    let client = reqwest::Client::new();
    let authorization = async {
        let descriptor =
            authenticated_json(&client, &endpoint, ".well-known/t3/environment", "", false).await?;
        let destination: EnvironmentId =
            serde_json::from_value(descriptor["environmentId"].clone())
                .map_err(|error| format!("Invalid environment identity: {error}"))?;
        let protocol = descriptor["orchestrationProtocolVersion"]
            .as_u64()
            .unwrap_or(1);
        let label = descriptor["label"].as_str().unwrap_or(&address).to_owned();
        let session =
            authenticated_json(&client, &endpoint, "api/auth/session", &token, false).await?;
        let grants: SessionGrantInput = serde_json::from_value(session)
            .map_err(|error| format!("Invalid session response: {error}"))?;
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
            state.status().set(ConnectionStatus::Blocked(error.clone()));
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
    let (wake_sender, mut wake_receiver) = futures_channel::mpsc::unbounded::<()>();
    let socket_url = endpoint.socket(Some(&ticket), client_surface());
    let (sender, receiver) = match ewebsock::connect_with_wakeup(
        socket_url.to_string(),
        ewebsock::Options::default(),
        move || {
            let _ = wake_sender.unbounded_send(());
        },
    ) {
        Ok(socket) => socket,
        Err(error) => {
            fail(state, error.to_string());
            state.status().set(ConnectionStatus::Disconnected);
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
    loop {
        if handle.borrow().generation != generation {
            return;
        }
        while let Some(event) = receiver.try_recv() {
            match event {
                ewebsock::WsEvent::Opened => {
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
                                apply_rpc_event(&handle, state, event);
                            }
                        }
                        Err(error) => fail(state, error.to_string()),
                    }
                }
                ewebsock::WsEvent::Closed | ewebsock::WsEvent::Error(_) => {
                    let mut transport = handle.borrow_mut();
                    transport.sender = None;
                    transport.rpc.disconnect();
                    state.status().set(ConnectionStatus::Interrupted(
                        "Connection closed. Reconnect to continue.".into(),
                    ));
                    state.pending_message().set(None);
                    return;
                }
                _ => {}
            }
        }
        if wake_receiver.next().await.is_none() {
            return;
        }
    }
}

fn apply_rpc_event(handle: &TransportHandle, state: Store<UiModel>, event: RpcEvent) {
    match event {
        RpcEvent::Values { method, values, .. } => {
            for value in values {
                if method == "orchestration.subscribeShell" {
                    match serde_json::from_value::<ShellStreamItem>(value) {
                        Ok(item) => {
                            state.shell().write().apply(item);
                        }
                        Err(error) => {
                            fail(state, format!("Could not decode shell update: {error}"))
                        }
                    }
                } else if method == "orchestration.subscribeThread" {
                    let result = state.thread().write().apply(&value);
                    if let Err(error) = result {
                        fail(state, error);
                    }
                }
            }
        }
        RpcEvent::Complete { id, method, value } => {
            if method == "server.getConfig" {
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
                state.config().set(value);
            } else if method == "orchestration.launchThread" {
                if let Some(thread_id) = value["threadId"].as_str() {
                    select_thread(handle, state, thread_id.to_owned());
                }
            }
            let pending = state.peek().pending_message.clone();
            if let Some((pending_id, text)) = pending {
                if id == pending_id {
                    if *state.draft().peek() == text {
                        state.draft().set(String::new());
                    }
                    state.pending_message().set(None);
                }
            }
        }
        RpcEvent::Failed { id, cause, .. } => {
            if state
                .peek()
                .pending_message
                .as_ref()
                .is_some_and(|(pending_id, _)| *pending_id == id)
            {
                state.pending_message().set(None);
            }
            fail(state, rpc_error_message(&cause));
        }
        RpcEvent::Defect(error) => fail(state, rpc_error_message(&error)),
        _ => {}
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
