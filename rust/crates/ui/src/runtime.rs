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
    General,
    Connections,
    Providers,
    Appearance,
    Keybindings,
    ScheduledTasks,
    SourceControl,
    TextGeneration,
    Projects,
    PullRequests,
    Usage,
}
#[derive(Debug, Clone)]
pub struct PendingMessage {
    pub destination: EnvironmentId,
    pub thread_id: String,
    pub text: String,
    /// Local ids of the composer attachments this send claims.
    pub attachments: Vec<String>,
    /// Ids of the context chips this send claims.
    pub contexts: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct PendingLaunch {
    pub destination: EnvironmentId,
    pub project_id: String,
    pub text: String,
    pub active_thread: Option<String>,
    pub choices: NewThreadChoices,
    pub attachments: Vec<String>,
    pub contexts: Vec<String>,
    /// Started with `composer.sendBackground`: the person stays on the form.
    pub background: bool,
}
pub type NewThreadChoices = t3_client::draft_storage::DraftChoices;

#[derive(Debug, Clone, Default, Store)]
pub struct UiModel {
    pub status: ConnectionStatus,
    pub shell: ShellState,
    pub thread: ThreadState,
    pub active_thread: Option<String>,
    pub selected_project: Option<String>,
    /// The thread a first run's welcome opened, so a reconnect does not open it again.
    pub bootstrap_opened: Option<String>,
    /// The notice that threads of the previous version are being restored, while they are.
    pub migration_notice: Option<u64>,
    pub config: Value,
    pub typed_config: Option<t3_contracts::ServerConfig>,
    pub client_settings: t3_contracts::ClientSettings,
    pub client_settings_error: Option<String>,
    pub draft_storage: crate::draft_storage::DraftHandle,
    pub draft_storage_error: Option<String>,
    pub thread_model_options: std::collections::BTreeMap<
        (EnvironmentId, t3_contracts::ProviderInstanceId, String),
        Vec<t3_contracts::ProviderOptionSelection>,
    >,
    pub sticky_models: std::collections::BTreeMap<EnvironmentId, t3_contracts::ModelSelection>,
    pub grants: SessionGrantInput,
    pub destination: Option<EnvironmentId>,
    pub environments: EnvironmentCatalog,
    pub error: Option<String>,
    pub draft: String,
    /// Composer attachments by draft; only the composer that owns a draft reads its entry.
    pub draft_attachments:
        std::collections::BTreeMap<t3_client::draft_storage::DraftTarget, Vec<t3_client::attachments::Attachment>>,
    pub attachment_error: Option<(t3_client::draft_storage::DraftTarget, String)>,
    /// Context chips by draft; only the composer that owns a draft reads its entry.
    pub draft_contexts: std::collections::BTreeMap<t3_client::draft_storage::DraftTarget, Vec<Value>>,
    pub timeline_send: Option<(String, String)>,
    pub pending_messages: std::collections::BTreeMap<String, PendingMessage>,
    pub new_thread_drafts: std::collections::BTreeMap<(EnvironmentId, String), String>,
    pub new_thread_choices: std::collections::BTreeMap<(EnvironmentId, String), NewThreadChoices>,
    pub thread_choices: std::collections::BTreeMap<(EnvironmentId, String), NewThreadChoices>,
    pub pending_launches: std::collections::BTreeMap<String, PendingLaunch>,
    pub view: View,
    pub diff: crate::diff_panel::DiffModel,
    /// The file the panel beside the conversation shows (opened from the file picker or the content search).
    pub file_panel: Option<crate::file_panel::Target>,
    /// The files open in that panel, in the order they were opened; `file_panel` is the one shown.
    pub file_tabs: Vec<crate::file_panel::Target>,
    /// The files whose edits are not on disk yet (`cwd`, a NUL, the path): the dot on a tab.
    pub file_pending: std::collections::BTreeSet<String>,
    /// What was last handed to the server to write for such a file, until a write confirms it, so a panel opened again before that
    /// shows the edit and not the file as it was.
    pub file_unsaved: std::collections::BTreeMap<String, String>,
    /// Whether the custom snooze dialog is open for the chosen threads.
    pub custom_snooze: bool,
    /// The one thread the custom snooze dialog is for (its row's menu), else the chosen threads.
    pub custom_snooze_thread: Option<String>,
    pub rewind: crate::rollback::Model,
    pub palette: crate::command_palette::Model,
    /// Whether the open thread's branch control offers its picker, and how often something has
    /// asked for it to toggle (the palette, the `composer.branch` shortcut).
    pub branch_available: bool,
    pub branch_toggles: u64,
    /// How often `composer.sendBackground` has asked the open new-thread form to start its thread.
    pub background_sends: u64,
    pub notices: Vec<crate::notice::Notice>,
    /// The git action each workspace is running or last failed at, outliving the control that
    /// started it so that leaving the thread does not interrupt a push.
    pub git_runs: std::collections::BTreeMap<String, crate::git_control::Run>,
    /// A notice's call to action asking the control of a workspace to run an action again.
    pub git_follow: Option<crate::git_control::FollowRequest>,
    /// The pull request the open thread's panel shows, and the one its branch has to offer.
    pub pr_panel: Option<crate::pull_request_panel::Target>,
    pub pr_available: Option<crate::pull_request_panel::Target>,
    /// The reviews being written, by pull request, until each is sent: the summary and the line comments. They outlive the
    /// panel (closing it does not lose a review), and not the app.
    pub pr_reviews: std::collections::BTreeMap<String, t3_client::pull_request_line_comments::ReviewDraft>,
    /// The dialog that starts a thread from a pull request, while it is open.
    pub pr_checkout: Option<crate::pull_request_thread::Open>,
    pub lifecycle: crate::thread_lifecycle::Model,
    pub relations: crate::thread_relations::Model,
    /// The SSH environments added, and the one the window is switched to.
    pub ssh: crate::ssh_environments::Model,
    pub usage: crate::usage_page::Model,
    pub redeem: crate::limits_view::Redeem,
    /// The environment's tracked repository clones.
    pub clones: crate::clone_view::Model,
    /// A sidebar row dropped in its new place, until the server's state shows it there.
    pub reorder: crate::sidebar_sections::Reorder,
    /// The Snoozed and Settled shelves of the sidebar: open or folded.
    pub shelves: crate::sidebar_sections::Shelves,
    /// The prompts put aside with the stash shortcut, and whether its list is open.
    pub stash: crate::prompt_stash::Model,
    /// The plan being started in a new thread, while its two commands are in flight.
    pub plan_launch: Option<String>,
    pub import: crate::import_projects::Model,
    /// The `/usage-limits` result shown over the open thread's composer.
    pub usage_panel: Option<crate::limits_view::Panel>,
    /// The threads chosen in the sidebar with Ctrl/Cmd+click and Shift+click, and where their menu is open.
    /// The thread whose Pull requests tab is open beside the conversation, and the thread whose link dialog is open.
    pub thread_prs: Option<String>,
    /// The project the Projects settings page is showing.
    pub settings_project: Option<String>,
    pub link_dialog: Option<String>,
    pub selection: t3_client::thread_selection::Selection,
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
    // Request ids restart on each socket, including automatic reconnects within
    // the same user connection generation. Cancellation belongs to this epoch.
    socket_generation: u64,
    opened: bool,
    thread_generation: u64,
    destination: Option<EnvironmentId>,
    endpoint: Option<EnvironmentEndpoint>,
    bearer_token: String,
    unary_waiters:
        std::collections::BTreeMap<String, futures_channel::oneshot::Sender<Result<Value, String>>>,
    stream_waiters: std::collections::BTreeMap<String, StreamWaiter>,
    /// Requests whose failure the caller shows itself, so it stays out of the app-wide banner.
    reported: std::collections::BTreeSet<String>,
    config_subscription: Option<ConnectionOwner>,
    config_request_id: Option<String>,
    live_config_owner: Option<ConnectionOwner>,
}
struct StreamWaiter {
    sender: futures_channel::mpsc::Sender<Vec<Value>>,
    failure: Rc<RefCell<Option<String>>>,
    /// What the server failed the stream with, as it sent it, for a caller that reads its parts.
    cause: Rc<RefCell<Option<Value>>>,
}
impl Transport {
    fn fail_waiters(&mut self) {
        self.reported.clear();
        for (_, waiter) in std::mem::take(&mut self.unary_waiters) {
            let _ = waiter.send(Err("Connection closed before the server replied.".into()));
        }
        for (_, waiter) in std::mem::take(&mut self.stream_waiters) {
            *waiter.failure.borrow_mut() = Some("Connection interrupted. Reconnecting…".into());
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

/// Whether the endpoint in use is on this computer's loopback interface.
#[cfg(feature = "desktop")]
pub(crate) fn connected_to_loopback(handle: &TransportHandle) -> bool {
    handle.borrow().endpoint.as_ref().is_some_and(|endpoint| matches!(endpoint.http("").host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
}

/// `GET /api/rust/state-import` of a server whose import failed: that server serves nothing else. Any other answer
/// (a missing route, a page, an unreachable server) is none.
pub(crate) async fn probe_state_import(address: &str) -> Option<t3_contracts::StateImportStatus> {
    let url = format!("{}/api/rust/state-import", address.trim_end_matches('/'));
    let body = reqwest::Client::new().get(url).send().await.ok()?.json::<Value>().await.ok()?;
    serde_json::from_value(body.get("stateImport")?.clone()).ok()
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
    // Clearing an error that is not there would still repaint everything that shows it.
    if state.peek().error.is_some() {
        state.error().set(None);
    }
    Some(id)
}

fn required_scopes(method: &str, payload: &Value) -> Result<Vec<AuthEnvironmentScope>, String> {
    let mut scopes =
        client_rpc_required_scopes(method, Some(payload)).map_err(|error| error.to_string())?;
    if method == "server.updateSettings" {
        // RpcAuthorization uses the decoded patch rather than the method's
        // fallback scope. Provider-only atomic mutations need manage access;
        // a nonempty patch keeps its independently required settings grants.
        let input: t3_contracts::UpdateServerSettingsInput =
            serde_json::from_value(payload.clone()).map_err(|error| error.to_string())?;
        let mut settings_scopes = input.patch.required_scopes();
        if input.provider_instance_mutation.is_some() {
            if serde_json::to_value(&input.patch)
                .map_err(|error| error.to_string())?
                .as_object()
                .is_some_and(|patch| patch.is_empty())
            {
                settings_scopes.clear();
            }
            if !settings_scopes.contains(&AuthEnvironmentScope::ProvidersManage) {
                settings_scopes.push(AuthEnvironmentScope::ProvidersManage);
            }
        }
        for scope in settings_scopes {
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        return Ok(scopes);
    }
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
    let generation = handle.borrow().socket_generation;
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

/// `request_value` for a caller that shows the failure where it belongs (the rewind dialog, the
/// branch control): the failure comes back as the `Err` and does not also fill the app banner.
pub async fn request_value_reported(
    handle: TransportHandle,
    state: Store<UiModel>,
    method: &str,
    payload: Value,
) -> Result<Value, String> {
    let id = request(&handle, state, method, payload, RequestKind::Unary).ok_or_else(|| {
        state.peek().error.clone().unwrap_or_else(|| "Request unavailable.".into())
    })?;
    handle.borrow_mut().reported.insert(id.clone());
    let (sender, receiver) = futures_channel::oneshot::channel();
    let generation = handle.borrow().socket_generation;
    handle.borrow_mut().unary_waiters.insert(id.clone(), sender);
    let _guard = UnaryRequestGuard { handle: handle.clone(), id, generation };
    receiver.await.map_err(|_| "Request was interrupted.".to_owned())?
}

struct UnaryRequestGuard {
    handle: TransportHandle,
    id: String,
    generation: u64,
}
impl Drop for UnaryRequestGuard {
    fn drop(&mut self) {
        let mut transport = self.handle.borrow_mut();
        if transport.socket_generation != self.generation {
            return;
        }
        transport.unary_waiters.remove(&self.id);
        transport.stream_waiters.remove(&self.id);
        transport.reported.remove(&self.id);
        if let Some(frame) = transport.rpc.cancel(&self.id) {
            if let Some(sender) = transport.sender.as_mut() {
                sender.send(ewebsock::WsMessage::Text(frame.to_string()));
            }
        }
    }
}

/// A channel owns one Effect stream. Dropping the consumer interrupts the
/// server stream and releases its waiter without retaining an unmounted view.
pub struct StreamRequest {
    receiver: futures_util::stream::Peekable<futures_channel::mpsc::Receiver<Vec<Value>>>,
    failure: Rc<RefCell<Option<String>>>,
    cause: Rc<RefCell<Option<Value>>>,
    state: Store<UiModel>,
    destination: EnvironmentId,
    ended: bool,
    _guard: UnaryRequestGuard,
}
impl StreamRequest {
    /// The error the server ended the stream with, when it did and this client has not lost the
    /// connection instead.
    pub(crate) fn cause(&self) -> Option<Value> {
        self.cause.borrow().clone()
    }
    #[cfg(test)]
    pub(crate) fn request_id(&self) -> String {
        self._guard.id.clone()
    }
    #[cfg(test)]
    pub(crate) async fn wait_ready(&mut self) -> bool {
        std::pin::Pin::new(&mut self.receiver)
            .peek()
            .await
            .is_some()
    }
    fn current(&self) -> bool {
        let transport = self._guard.handle.borrow();
        transport.socket_generation == self._guard.generation
            && transport.destination.as_ref() == Some(&self.destination)
            && self.state.peek().destination.as_ref() == Some(&self.destination)
    }
    fn interruption(&mut self) -> Option<String> {
        if self.ended {
            return None;
        }
        let error = if !self.current() {
            Some("Stream destination or connection changed.".into())
        } else {
            self.failure.borrow_mut().take()
        };
        if error.is_some() {
            self.ended = true;
        }
        error
    }
    pub async fn next(&mut self) -> Option<Result<Vec<Value>, String>> {
        if self.ended {
            return None;
        }
        if let Some(error) = self.interruption() {
            return Some(Err(error));
        }
        let next = self.receiver.next().await;
        if let Some(error) = self.interruption() {
            return Some(Err(error));
        }
        match next {
            Some(values) => {
                let mut transport = self._guard.handle.borrow_mut();
                if transport.socket_generation == self._guard.generation
                    && transport.stream_waiters.contains_key(&self._guard.id)
                {
                    if let Some(sender) = transport.sender.as_mut() {
                        sender.send(ewebsock::WsMessage::Text(
                            json!({"_tag":"Ack","requestId":self._guard.id}).to_string(),
                        ));
                    }
                }
                Some(Ok(values))
            }
            None => {
                self.ended = true;
                None
            }
        }
    }
}
pub fn request_stream(
    handle: &TransportHandle,
    state: Store<UiModel>,
    method: &str,
    payload: Value,
) -> Result<StreamRequest, String> {
    open_stream(handle, state, method, payload, false)
}
/// A stream whose failure the caller shows itself (`StreamRequest::next` ends with it), so the
/// app-wide banner does not repeat it.
pub fn request_stream_reported(
    handle: &TransportHandle,
    state: Store<UiModel>,
    method: &str,
    payload: Value,
) -> Result<StreamRequest, String> {
    open_stream(handle, state, method, payload, true)
}
fn open_stream(
    handle: &TransportHandle,
    state: Store<UiModel>,
    method: &str,
    payload: Value,
    reported: bool,
) -> Result<StreamRequest, String> {
    let id = request(handle, state, method, payload, RequestKind::Stream).ok_or_else(|| {
        state
            .peek()
            .error
            .clone()
            .unwrap_or_else(|| "Stream unavailable.".into())
    })?;
    if reported {
        handle.borrow_mut().reported.insert(id.clone());
    }
    // futures mpsc reserves one slot per sender: zero extra capacity means one
    // admitted chunk. Its ACK is sent on consumption, never in the socket loop.
    let (sender, receiver) = futures_channel::mpsc::channel(0);
    let failure = Rc::new(RefCell::new(None));
    let cause = Rc::new(RefCell::new(None));
    let generation = handle.borrow().socket_generation;
    let destination = handle
        .borrow()
        .destination
        .clone()
        .expect("request validated connection destination");
    handle.borrow_mut().stream_waiters.insert(
        id.clone(),
        StreamWaiter {
            sender,
            failure: failure.clone(),
            cause: cause.clone(),
        },
    );
    Ok(StreamRequest {
        receiver: receiver.peekable(),
        failure,
        cause,
        state,
        destination,
        ended: false,
        _guard: UnaryRequestGuard {
            handle: handle.clone(),
            id,
            generation,
        },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionOwner {
    generation: u64,
    socket_generation: u64,
    destination: EnvironmentId,
}
pub fn connection_owner(
    handle: &TransportHandle,
    state: Store<UiModel>,
) -> Option<ConnectionOwner> {
    let transport = handle.borrow();
    let destination = state.destination().peek().clone()?;
    (transport.destination.as_ref() == Some(&destination)).then_some(ConnectionOwner {
        generation: transport.generation,
        socket_generation: transport.socket_generation,
        destination,
    })
}

pub fn live_config_active(handle: &TransportHandle, state: Store<UiModel>) -> bool {
    let owner = connection_owner(handle, state);
    let transport = handle.borrow();
    owner.is_some()
        && transport.config_subscription == owner
        && transport.live_config_owner == owner
}

struct ConfigStreamGuard {
    handle: TransportHandle,
    owner: ConnectionOwner,
    id: String,
}
impl Drop for ConfigStreamGuard {
    fn drop(&mut self) {
        let mut transport = self.handle.borrow_mut();
        if transport.config_subscription.as_ref() == Some(&self.owner)
            && transport.config_request_id.as_ref() == Some(&self.id)
        {
            transport.config_subscription = None;
            transport.config_request_id = None;
            if transport.live_config_owner.as_ref() == Some(&self.owner) {
                transport.live_config_owner = None;
            }
        }
    }
}

fn start_config_subscription(handle: &TransportHandle, state: Store<UiModel>) {
    let Some(owner) = connection_owner(handle, state) else {
        return;
    };
    if handle.borrow().config_subscription.as_ref() == Some(&owner) {
        return;
    }
    let Ok(mut wire) = request_stream(
        handle,
        state,
        "subscribeServerConfig",
        json!({"environmentThemes":true,"usageLimitSources":true}),
    ) else {
        return;
    };
    handle.borrow_mut().config_subscription = Some(owner.clone());
    let id = wire._guard.id.clone();
    handle.borrow_mut().config_request_id = Some(id.clone());
    let guard = ConfigStreamGuard {
        handle: handle.clone(),
        owner,
        id,
    };
    spawn(async move {
        while let Some(values) = wire.next().await {
            if connection_owner(&guard.handle, state).as_ref() != Some(&guard.owner) {
                return;
            }
            let values = match values {
                Ok(values) => values,
                Err(error) => {
                    if *state.status().peek() == ConnectionStatus::Connected {
                        fail(state, error);
                    }
                    return;
                }
            };
            let events = match values
                .into_iter()
                .map(serde_json::from_value::<t3_contracts::ServerConfigStreamEvent>)
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(events) => events,
                Err(error) => {
                    fail(
                        state,
                        format!("Invalid server configuration update: {error}"),
                    );
                    return;
                }
            };
            let mut candidate = state.typed_config().peek().clone();
            for event in events {
                if let Err(error) = project_config(&mut candidate, event, &guard.owner.destination)
                {
                    state.status().set(ConnectionStatus::Blocked(error.clone()));
                    fail(state, error);
                    if let Some(mut sender) = guard.handle.borrow_mut().sender.take() {
                        sender.close();
                    }
                    return;
                }
            }
            if let Some(config) = candidate {
                let value = serde_json::to_value(&config).expect("validated configuration");
                guard.handle.borrow_mut().live_config_owner = Some(guard.owner.clone());
                state.typed_config().set(Some(config));
                state.config().set(value);
            }
        }
    });
}

fn project_config(
    current: &mut Option<t3_contracts::ServerConfig>,
    event: t3_contracts::ServerConfigStreamEvent,
    destination: &EnvironmentId,
) -> Result<(), String> {
    use t3_contracts::ServerConfigStreamEvent as Event;
    match event {
        Event::Snapshot { mut config, .. } => {
            if &config.environment.environment_id != destination {
                return Err("Server identity changed during configuration subscription".into());
            }
            if let Some(previous) = current.as_ref() {
                if config.environment.capabilities.environment_themes == Some(true) {
                    config.environment_themes = previous.environment_themes.clone();
                }
                if config.environment.capabilities.usage_limit_sources == Some(true) {
                    config.usage_limit_sources = previous.usage_limit_sources.clone();
                }
            }
            *current = Some(config);
        }
        event => {
            if let Some(config) = current.as_mut() {
                match event {
                    Event::ProviderStatuses { payload, .. } => config.providers = payload.providers,
                    Event::SettingsUpdated { payload, .. } => config.settings = payload.settings,
                    Event::KeybindingsUpdated { payload, .. } => {
                        config.keybindings = payload.keybindings;
                        config.issues = payload.issues;
                    }
                    Event::EnvironmentThemesUpdated { payload, .. } => {
                        config.environment_themes =
                            (!payload.themes.is_empty()).then_some(Some(payload.themes))
                    }
                    Event::UsageLimitSourcesUpdated { payload, .. } => {
                        config.usage_limit_sources =
                            (!payload.sources.0.is_empty()).then_some(Some(payload.sources))
                    }
                    Event::Snapshot { .. } => unreachable!(),
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResponseOwner {
    generation: u64,
    socket_generation: u64,
    thread_generation: u64,
    destination: EnvironmentId,
    active_thread: Option<String>,
}
impl ResponseOwner {
    /// Whether both are the same connection to the same environment, whichever thread is open.
    pub fn same_connection(&self, other: &Self) -> bool {
        self.generation == other.generation && self.socket_generation == other.socket_generation && self.destination == other.destination
    }
}
pub fn response_owner(handle: &TransportHandle, state: Store<UiModel>) -> Option<ResponseOwner> {
    let transport = handle.borrow();
    let model = state.peek();
    let destination = model.destination.clone()?;
    (transport.destination.as_ref() == Some(&destination)).then(|| ResponseOwner {
        generation: transport.generation,
        socket_generation: transport.socket_generation,
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
        .thread_model_options()
        .write()
        .retain(|(owner, _, _), _| owner != destination);
    state
        .thread_choices()
        .write()
        .retain(|(owner, _), _| owner != destination);
    state
        .new_thread_drafts()
        .write()
        .retain(|(owner, _), _| owner != destination);
    state
        .new_thread_choices()
        .write()
        .retain(|(owner, _), _| owner != destination);
    state
        .draft_attachments()
        .write()
        .retain(|target, _| target.environment != destination.as_str());
    state.attachment_error().set(None);
}

/// Whether the live connection is to `environment`, so its RPCs and signed URLs apply.
pub fn connected_to(handle: &TransportHandle, state: Store<UiModel>, environment: &str) -> bool {
    let transport = handle.borrow();
    state.peek().status == ConnectionStatus::Connected
        && transport.sender.is_some()
        && transport.destination.as_ref().map(EnvironmentId::as_str) == Some(environment)
        && state.peek().destination.as_ref().map(EnvironmentId::as_str) == Some(environment)
}
/// Resolves a server-relative URL (upload and asset URLs) against the connected
/// environment's own address, so remote and tunnelled connections work. None once the
/// connection no longer belongs to `environment`.
pub fn http_url(
    handle: &TransportHandle,
    state: Store<UiModel>,
    environment: &str,
    relative: &str,
) -> Option<String> {
    if !connected_to(handle, state, environment) {
        return None;
    }
    let endpoint = handle.borrow().endpoint.clone()?;
    Some(endpoint.http(relative).to_string())
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
            "This saved draft contains context that the Rust composer cannot send yet. The original draft was preserved; open it in the original app to send all content.",
        );
        return None;
    }
    let target =
        t3_client::draft_storage::DraftTarget::project(destination.to_string(), project);
    let (attachment_ids, attachments) = match crate::attachments::for_send(state, &target) {
        Ok(sendable) => sendable,
        Err(error) => {
            drop(owned);
            fail(state, error);
            return None;
        }
    };
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
    // The thread starts with the original's temporary title; `orchestration.launchThread` has no
    // title generation for an initial message (the server refuses `generateTitle` there).
    let title = {
        let (image, file) = t3_client::thread_title::first_attachment_names(&json!(attachments));
        t3_client::thread_title::title_seed(initial_text.unwrap_or_default(), image.as_deref(), file.as_deref())
    };
    let mut input = json!({"commandId":uuid::Uuid::new_v4().to_string(),"projectId":project,"title":title,"generateTitle":false,"modelSelection":selection,"runtimeMode":mode,"interactionMode":"default","creationSource":creation_source(),"workspaceStrategy":workspace});
    let text = initial_text.filter(|text| !text.trim().is_empty());
    // The chips of the form ride on the first message, named after its prose by a reference each.
    let (composed, context, context_ids) = crate::composer_chips::for_send(state, &t3_client::draft_storage::DraftTarget::project(destination.to_string(), project), text.unwrap_or_default());
    if text.is_some() || !attachments.is_empty() || context.is_some() {
        input["initialMessage"] = json!({"messageId":uuid::Uuid::new_v4().to_string(),"text":composed,"attachments":attachments});
        if let Some(context) = context {
            input["initialMessage"]["context"] = context;
        }
        if !attachments.is_empty() {
            // Claimed uploads are filed under the thread, which must therefore be named up front.
            input["threadId"] = json!(uuid::Uuid::new_v4().to_string());
        }
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
            attachments: attachment_ids,
            contexts: context_ids,
            background: false,
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
        return;
    }
    // `interruptThreadTurn` with no run to interrupt: work left in the background is stopped through
    // the latest run, and otherwise the pull requests the server is watching are let go.
    let background = state.peek().shell.snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)).is_some_and(|thread| !thread.pending_background_tasks.is_empty());
    if background && crate::composer_panels::stop_background_work(handle, state, thread_id) {
        return;
    }
    unwatch_pull_requests(handle, state, thread_id);
}

/// Stops the server watching the pull requests filed under the thread; true when it was asked to.
pub(crate) fn unwatch_pull_requests(handle: &TransportHandle, state: Store<UiModel>, thread_id: &str) -> bool {
    let watched: Vec<_> = {
        let model = state.peek();
        let thread = model.shell.snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id));
        thread.and_then(|thread| serde_json::to_value(thread).ok()).map(|thread| t3_client::thread_pull_requests::links(&thread).into_iter().filter(|link| link.watching).collect()).unwrap_or_default()
    };
    let mut sent = false;
    for link in watched {
        let Some(host) = link.host else { continue };
        let mut input = t3_client::thread_pull_requests::watch_command(thread_id, &host, &link.repository, link.number, false);
        let kind = input["type"].as_str().unwrap_or_default().to_owned();
        if let Some(fields) = input.as_object_mut() {
            fields.remove("type");
        }
        sent |= command(handle, state, &kind, input).is_some();
    }
    sent
}

/// How one send differs from "send the draft": how it is delivered to a running thread, a plan it
/// accepts, the mode it switches the thread to, the text it sends instead of the draft, or the
/// queued message it replaces.
#[derive(Default, Clone)]
pub struct SendOptions {
    /// `auto`, `queue`, `steer` or `restart` (`resolveComposerDispatchMode`); `auto` when empty.
    pub mode: Option<&'static str>,
    pub source_plan: Option<Value>,
    pub interaction_mode: Option<&'static str>,
    pub text: Option<String>,
    pub edit_run: Option<String>,
    /// The interrupted or limited run this message continues by hand: "Resume thread". It starts
    /// at once and carries no delivery intent or title seed, as the original's `dispatchMode: "start"`.
    pub manual_continuation: Option<String>,
}

pub fn send_message(handle: &TransportHandle, state: Store<UiModel>) {
    send_with(handle, state, SendOptions::default());
}

pub fn send_with(handle: &TransportHandle, state: Store<UiModel>, options: SendOptions) {
    let model = state.peek();
    let Some(thread_id) = model.active_thread.clone() else {
        return;
    };
    let text = options.text.clone().unwrap_or_else(|| model.draft.clone());
    let Some(destination) = model.destination.clone() else {
        return;
    };
    if crate::rollback::is_rewinding(state, &(destination.to_string(), thread_id.clone())) {
        return;
    }
    if crate::draft_storage::has_unrendered(
        state,
        &t3_client::draft_storage::DraftTarget::thread(destination.to_string(), thread_id.clone()),
    ) {
        drop(model);
        fail(
            state,
            "This saved draft contains context that the Rust composer cannot send yet. The original draft was preserved; open it in the original app to send all content.",
        );
        return;
    }
    let target =
        t3_client::draft_storage::DraftTarget::thread(destination.to_string(), thread_id.clone());
    let (attachment_ids, attachments) = match crate::attachments::for_send(state, &target) {
        Ok(sendable) => sendable,
        Err(error) => {
            drop(model);
            fail(state, error);
            return;
        }
    };
    // The chips this send carries, named after the prose by a reference each.
    let prompt = text;
    let (text, context, context_ids) = crate::composer_chips::for_send(state, &target, &prompt);
    if (text.trim().is_empty() && attachments.is_empty())
        || model
            .pending_messages
            .values()
            .any(|pending| pending.destination == destination && pending.thread_id == thread_id)
    {
        return;
    }
    let thread = model
        .shell
        .snapshot
        .as_ref()
        .and_then(|snapshot| {
            snapshot
                .threads
                .iter()
                .find(|thread| thread.id.as_str() == thread_id)
        })
        .cloned();
    let Some(thread) = thread else {
        return;
    };
    let choices = model
        .thread_choices
        .get(&(destination.clone(), thread_id.clone()))
        .cloned()
        .unwrap_or_default();
    let selection = choices
        .model_selection
        .clone()
        .unwrap_or_else(|| thread.model_selection.clone());
    let (selection, mode) = model
        .typed_config
        .as_ref()
        .and_then(|config| {
            t3_client::new_thread::launch_selection(
                config,
                &model.client_settings,
                &selection,
                choices.runtime_mode.unwrap_or(thread.runtime_mode),
            )
        })
        .unwrap_or((
            selection,
            choices.runtime_mode.unwrap_or(thread.runtime_mode),
        ));
    drop(model);
    if let Some(error) = model_change_error(state, &thread, &selection) {
        fail(state, error);
        return;
    }
    crate::draft_storage::save_thread_choices(
        state,
        &(destination.clone(), thread_id.clone()),
        &choices,
    );
    // The seed always comes from the message (`ChatView`'s `titleSeed`) and rides only while the thread's
    // projection holds no messages, so a fork's first message retitles it as a new thread's does.
    let title_seed = {
        let (image, file) = t3_client::thread_title::first_attachment_names(&json!(attachments));
        let seed = t3_client::thread_title::title_seed(&prompt, image.as_deref(), file.as_deref());
        let model = state.peek();
        let held = model.thread.projection.as_ref().filter(|p| p["thread"]["id"] == thread_id.as_str()).map(|p| p["messages"].as_array().map_or(0, Vec::len));
        let resolves = model.typed_config.as_ref().is_some_and(|config| config.environment.capabilities.server_resolved_command_context == Some(true));
        t3_client::thread_title::seed_to_send(seed, held, resolves)
    };
    let pending_id = uuid::Uuid::new_v4().to_string();
    let generation = handle.borrow().generation;
    state.pending_messages().write().insert(
        pending_id.clone(),
        PendingMessage {
            destination: destination.clone(),
            thread_id: thread_id.clone(),
            text: prompt.clone(),
            attachments: attachment_ids,
            contexts: context_ids,
        },
    );
    let handle = handle.clone();
    dioxus::dioxus_core::spawn_forever(async move {
        if !owns_pending_send(&handle, state, generation, &destination, &pending_id) {
            state.pending_messages().write().remove(&pending_id);
            return;
        }
        if mode != thread.runtime_mode {
            let result = request_value(handle.clone(),state,"orchestration.dispatchCommand",json!({"type":"thread.runtime-mode.set","commandId":uuid::Uuid::new_v4().to_string(),"threadId":thread_id,"runtimeMode":mode})).await;
            if let Err(error) = result {
                if owns_pending_send(&handle, state, generation, &destination, &pending_id) {
                    fail(state, error);
                }
                state.pending_messages().write().remove(&pending_id);
                return;
            }
        }
        // A settings acknowledgement does not authorize replay into a new
        // connection/destination. Navigation within this environment is safe.
        if !owns_pending_send(&handle, state, generation, &destination, &pending_id) {
            state.pending_messages().write().remove(&pending_id);
            return;
        }
        if let Some(mode) = options.interaction_mode.filter(|mode| serde_json::to_value(thread.interaction_mode).ok().as_ref().and_then(Value::as_str) != Some(*mode)) {
            let result = request_value(handle.clone(),state,"orchestration.dispatchCommand",json!({"type":"thread.interaction-mode.set","commandId":uuid::Uuid::new_v4().to_string(),"threadId":thread_id,"interactionMode":mode})).await;
            if let Err(error) = result {
                if owns_pending_send(&handle, state, generation, &destination, &pending_id) {
                    fail(state, error);
                }
                state.pending_messages().write().remove(&pending_id);
                return;
            }
            if !owns_pending_send(&handle, state, generation, &destination, &pending_id) {
                state.pending_messages().write().remove(&pending_id);
                return;
            }
        }
        let message_id = uuid::Uuid::new_v4().to_string();
        let id = if let Some(run_id) = options.edit_run.clone() {
            command(&handle, state, "queued-run.edit", json!({"threadId":thread_id,"runId":run_id,"text":text,"attachments":attachments}))
        } else {
            let mode = options.mode.unwrap_or("auto");
            let mut payload = json!({"threadId":thread_id,"messageId":message_id,"modelSelection":selection,"text":text,"attachments":attachments,"createdBy":"user","creationSource":creation_source()});
            if let Some(seed) = title_seed {
                payload["titleSeed"] = json!(seed);
            }
            if let Some(run_id) = options.manual_continuation.clone() {
                payload.as_object_mut().expect("payload object").remove("titleSeed");
                payload["manualContinuationOfRunId"] = json!(run_id);
                payload["dispatchMode"] = json!({"type":"start_immediately"});
            } else if mode == "queue" {
                payload["dispatchMode"] = json!({"type":"queue_after_active"});
            } else {
                payload["dispatchMode"] = json!({"type":"start_immediately"});
                payload["deliveryIntent"] = json!(mode);
            }
            if let Some(plan) = options.source_plan.clone() {
                payload["sourcePlanRef"] = plan;
            }
            if let Some(context) = context.clone() {
                payload["context"] = context;
            }
            command(&handle, state, "message.dispatch", payload)
        };
        if id.is_some() {
            state.timeline_send().set(Some((
                serde_json::to_string(&(destination.as_str(), &thread_id))
                    .expect("scoped timeline identity"),
                message_id,
            )));
        }
        let pending = state.pending_messages().write().remove(&pending_id);
        if let Some((id, pending)) = id.zip(pending) {
            state.pending_messages().write().insert(id, pending);
        }
    });
}
fn owns_pending_send(
    handle: &TransportHandle,
    state: Store<UiModel>,
    generation: u64,
    destination: &EnvironmentId,
    id: &str,
) -> bool {
    let transport = handle.borrow();
    transport.generation == generation
        && transport.destination.as_ref() == Some(destination)
        && state.destination().peek().as_ref() == Some(destination)
        && state
            .pending_messages()
            .peek()
            .get(id)
            .is_some_and(|pending| &pending.destination == destination)
}
pub fn model_change_error(
    state: Store<UiModel>,
    thread: &t3_contracts::ThreadShell,
    selection: &t3_contracts::ModelSelection,
) -> Option<String> {
    let model = state.peek();
    let projection = model
        .thread
        .projection
        .as_ref()
        .filter(|p| p["thread"]["id"] == thread.id.as_str());
    let started = thread.active_provider_thread_id.is_some()
        || projection.is_some_and(|p| {
            p["runs"].as_array().is_some_and(|runs| {
                runs.iter()
                    .any(|run| !(run["status"] == "queued" && run["queueHeld"] == true))
            })
        })
        || thread
            .pending_background_tasks
            .iter()
            .any(|task| !matches!(task, t3_contracts::PendingBackgroundTask::Command { .. }));
    let supports_handoff =
        projection.is_some_and(t3_client::started_thread::supports_provider_handoff);
    let providers = model
        .typed_config
        .as_ref()
        .map(|config| config.providers.0.as_slice())
        .unwrap_or_default();
    t3_client::started_thread::model_change_block(
        providers,
        started,
        supports_handoff,
        &thread.model_selection,
        Some(&thread.provider_instance_id),
        selection,
    )
    .map(|block| format!("{}. {}", block.title, block.description))
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

async fn read_json(request: reqwest::RequestBuilder, token: &str, path: &str) -> Result<Value, String> {
    read_json_within(request, token, path, 15).await
}

async fn read_json_within(request: reqwest::RequestBuilder, token: &str, path: &str, seconds: u64) -> Result<Value, String> {
    read_json_answering(request, token, path, seconds, false).await
}

/// `read_json_within`; with `server_message` a refusal that carries the server's own `message` (the shape of the original's
/// HTTP API errors) is returned as that message.
async fn read_json_answering(
    mut request: reqwest::RequestBuilder,
    token: &str,
    path: &str,
    seconds: u64,
    server_message: bool,
) -> Result<Value, String> {
    request = request.timeout(std::time::Duration::from_secs(seconds));
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
        let status = response.status().as_u16();
        // A refused pairing credential is its own answer, not "you are not paired yet": the credential is
        // one use and expires, and the person has to mint another.
        if status == 401 && matches!(path, "browser pairing" | "oauth/token") {
            return Err("That pairing credential did not work. It may have expired or already been used; create a new one on the server.".into());
        }
        let described = format!("Server returned {} for {path}", response.status());
        if status == 401 {
            return Err("Pair with this server or enter a valid access token to connect.".into());
        }
        if server_message {
            let body: Value = response.json().await.unwrap_or(Value::Null);
            return Err(body["message"].as_str().map_or(described, str::to_owned));
        }
        return Err(described);
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

/// A pull request's patch, read over HTTP at the connected environment's own address (so remote
/// and tunnelled connections work) with the connection's credential. The answer is dropped, as an
/// error, when the connection it was asked on is no longer the one the caller is on.
pub async fn post_environment_json(handle: &TransportHandle, state: Store<UiModel>, path: &str, payload: &Value) -> Result<Value, String> {
    environment_json(handle, state, Some(payload), path).await
}

/// The same for a read (`payload` none) or a write: the pairing links and signed-in clients of the environment.
pub(crate) async fn environment_json(handle: &TransportHandle, state: Store<UiModel>, payload: Option<&Value>, path: &str) -> Result<Value, String> {
    environment_json_answering(handle, state, payload, path, false).await
}

/// `environment_json` where a refusal comes back as the server's own message, for a page that words the failure with it.
pub(crate) async fn environment_json_with_message(handle: &TransportHandle, state: Store<UiModel>, payload: Option<&Value>, path: &str) -> Result<Value, String> {
    environment_json_answering(handle, state, payload, path, true).await
}

async fn environment_json_answering(handle: &TransportHandle, state: Store<UiModel>, payload: Option<&Value>, path: &str, server_message: bool) -> Result<Value, String> {
    let Some(owner) = response_owner(handle, state) else { return Err("Reconnect to read this.".into()) };
    let (endpoint, token) = {
        let transport = handle.borrow();
        let Some(endpoint) = transport.endpoint.clone() else { return Err("Connect to a server to read this.".into()) };
        (endpoint, transport.bearer_token.clone())
    };
    let client = reqwest::Client::new();
    let request = match payload {
        Some(payload) => client.post(endpoint.http(path)).json(payload),
        None => client.get(endpoint.http(path)),
    }
    .header("x-t3-orchestration-protocol", "2");
    let answer = read_json_answering(request, &token, path, 60, server_message).await;
    if response_owner(handle, state) != Some(owner) {
        return Err("The connection changed while this was being read.".into());
    }
    answer
}

/// The address this connection reaches the server at, for a link another device can open.
pub(crate) fn connected_origin(handle: &TransportHandle) -> Option<String> {
    handle.borrow().endpoint.as_ref().map(|endpoint| endpoint.http("").origin().ascii_serialization())
}

pub(crate) async fn delay(milliseconds: u64) {
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
/// Ends the connection in use and leaves the window at "Connecting…" before a switch to another environment begins, so that
/// whoever then waits for the new connection does not mistake the old one, still shown as connected, for it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn begin_connection(handle: &TransportHandle, state: Store<UiModel>) {
    handle.borrow_mut().generation += 1;
    reset_for_connect(handle, state, false);
}

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
    reset_for_connect(&handle, state, false);
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

/// Whether this attempt retries the connection that was lost: the same server, after it was
/// interrupted. Then what the person was looking at stays on screen, labelled as disconnected,
/// rather than the app emptying itself (and saying "Add a project") for as long as the server is down.
fn retries_lost_connection(handle: &TransportHandle, state: Store<UiModel>, address: &str) -> bool {
    let model = state.peek();
    matches!(model.status, ConnectionStatus::Interrupted(_))
        && model.destination.is_some()
        && handle.borrow().endpoint.is_some()
        && handle.borrow().endpoint == EnvironmentEndpoint::new(address).ok()
}

/// What an environment's view shows before the environment has answered: nothing.
fn clear_visible(model: &mut UiModel) {
    model.destination = None;
    model.shell = ShellState::default();
    model.thread = ThreadState::default();
    model.active_thread = None;
    model.selected_project = None;
    model.config = Value::Null;
    model.typed_config = None;
    model.draft.clear();
}

fn reset_for_connect(handle: &TransportHandle, mut state: Store<UiModel>, keep_visible: bool) {
    {
        let mut transport = handle.borrow_mut();
        if let Some(mut sender) = transport.sender.take() {
            sender.close();
        }
        transport.rpc.disconnect();
        transport.fail_waiters();
        transport.socket_generation += 1;
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
        if !keep_visible {
            clear_visible(&mut model);
        }
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
    let keep_visible = retries_lost_connection(&handle, state, &address);
    reset_for_connect(&handle, state, keep_visible);
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
            let unreachable = error.starts_with("Could not reach your T3 Code server:");
            if !unreachable {
                // Refused, not lost: nothing of the last environment is the person's to see any more.
                clear_visible(&mut state.write());
            }
            state.status().set(
                if unreachable {
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
        if model.destination.as_ref() != Some(&destination) {
            model.config = Value::Null;
            model.typed_config = None;
        }
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
                    start_config_subscription(&handle, state);
                    crate::lifecycle::start(&handle, state);
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
                            let mut transport = handle.borrow_mut();
                            let frames: Vec<_> = frames
                                .into_iter()
                                .filter(|frame| {
                                    frame["_tag"] != "Ack"
                                        || frame["requestId"].as_str().is_none_or(|id| {
                                            !transport.stream_waiters.contains_key(id)
                                        })
                                })
                                .collect();
                            if let Some(sender) = transport.sender.as_mut() {
                                for frame in frames {
                                    sender.send(ewebsock::WsMessage::Text(frame.to_string()));
                                }
                            }
                            drop(transport);
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
        RpcEvent::Values { id, method, values } => {
            let mut transport = handle.borrow_mut();
            if let Some(waiter) = transport.stream_waiters.get_mut(&id) {
                if waiter.sender.try_send(values).is_err() {
                    if let Some(waiter) = transport.stream_waiters.remove(&id) {
                        *waiter.failure.borrow_mut() =
                            Some("Terminal stream exceeded its acknowledged chunk window.".into());
                    }
                    if let Some(frame) = transport.rpc.cancel(&id) {
                        if let Some(sender) = transport.sender.as_mut() {
                            sender.send(ewebsock::WsMessage::Text(frame.to_string()));
                        }
                    }
                }
                return;
            }
            drop(transport);
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
            let reported = handle.borrow().reported.contains(&id);
            handle.borrow_mut().stream_waiters.remove(&id);
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
                // A unary refresh can finish after a newer streamed registry
                // update. Once live projection owns this socket, only that
                // ordered subscription may replace the config.
                if live_config_active(handle, state) {
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
            // A reply to a request that shows its own answer is not a message's receipt, and
            // clearing the map for it would repaint the composer for every status refresh.
            if !reported {
                finish_pending_message(state, &id, true);
            }
        }
        RpcEvent::Failed { id, method, cause } => {
            let optional_config = method == "subscribeServerConfig"
                && handle.borrow().live_config_owner.is_none()
                && has_rpc_error_tag(
                    &cause,
                    &[
                        "NativeServiceUnavailableError",
                        "NativeMethodUnsupportedError",
                    ],
                );
            if let Some(waiter) = handle.borrow_mut().stream_waiters.remove(&id) {
                *waiter.cause.borrow_mut() = Some(cause.clone());
                if !optional_config {
                    *waiter.failure.borrow_mut() = Some(if method == t3_client::vcs::SUBSCRIBE_STATUS {
                        t3_client::vcs::failure_message(&cause)
                    } else {
                        rpc_error_message(&cause)
                    });
                }
            }
            // The config consumer owns its epoch's errors. Optional legacy
            // services retain the already requested unary bootstrap snapshot.
            if method == "subscribeServerConfig" {
                return;
            }
            // An expired or deleted attachment is an answer the attachment UI shows
            // itself, not a request failure. So is a diff that cannot be produced: the
            // diff panel shows the reason beside a retry, not in the app-wide banner.
            let missing_attachment = method == "assets.createUrl"
                && has_rpc_error_tag(&cause, &["AssetAttachmentNotFoundError"]);
            let diff_request = matches!(
                method.as_str(),
                t3_client::checkpoints::TURN_DIFF | t3_client::checkpoints::FULL_THREAD_DIFF
            );
            // Source control failures read from their parts, and the branch control shows them.
            let vcs_request = method.starts_with("vcs.") || method == t3_client::vcs::SUBSCRIBE_STATUS;
            let reported = handle.borrow_mut().reported.remove(&id);
            if let Some(waiter) = handle.borrow_mut().unary_waiters.remove(&id) {
                let _ = waiter.send(Err(if missing_attachment {
                    crate::attachments::ASSET_NOT_FOUND.into()
                } else if diff_request {
                    t3_client::checkpoints::failure_message(&cause)
                } else if vcs_request {
                    t3_client::vcs::failure_message(&cause)
                } else if let Some(message) = method.starts_with("pullRequests.").then(|| t3_client::pull_request::failure_message(&cause)).flatten() {
                    message
                } else if let Some(message) = t3_client::provider_manage::failure_message(&cause) {
                    message
                } else {
                    rpc_error_message(&cause)
                }));
            }
            finish_pending_message(state, &id, false);
            if state.pending_launches().peek().contains_key(&id) {
                state.pending_launches().write().remove(&id);
            }
            if !missing_attachment && !diff_request && !vcs_request && !reported {
                fail(state, rpc_error_message(&cause));
            }
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
    // The launch claimed these uploads, so they leave the draft whatever else changed.
    // After the acknowledgement above, which resets the saved attachment list.
    crate::attachments::sent(
        state,
        &t3_client::draft_storage::DraftTarget::project(
            pending.destination.to_string(),
            pending.project_id.clone(),
        ),
        &pending.attachments,
    );
    crate::composer_chips::sent(
        state,
        &t3_client::draft_storage::DraftTarget::project(pending.destination.to_string(), pending.project_id.clone()),
        &pending.contexts,
    );
    let current = {
        let model = state.peek();
        model.destination.as_ref() == Some(&pending.destination)
            && model.active_thread == pending.active_thread
            && model
                .selected_project
                .as_deref()
                .is_none_or(|project| project == pending.project_id)
    };
    if current && !pending.background {
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
    // Every reply passes here, and a write to the map repaints the composer even when the reply
    // is not a message's (a refresh, a git action), so only a pending message is written.
    if !state.pending_messages().peek().contains_key(id) {
        return;
    }
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
        crate::draft_storage::acknowledge_content(state, target.clone());
    }
    if current && *state.draft().peek() == pending.text {
        state.draft().set(String::new());
        crate::draft_storage::acknowledge_content(
            state,
            t3_client::draft_storage::DraftTarget::thread(
                pending.destination.to_string(),
                pending.thread_id.clone(),
            ),
        );
    }
    // After the acknowledgements above, which reset the saved attachment list.
    crate::attachments::sent(state, &target, &pending.attachments);
    crate::composer_chips::sent(state, &target, &pending.contexts);
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
    // Most errors say `message`; the source control repository errors say `detail`, with what
    // actually went wrong in their cause.
    let text = |error: &Value| {
        error["message"].as_str().map(str::to_owned).or_else(|| {
            let detail = error["detail"].as_str()?;
            Some(match error["cause"]["message"].as_str() {
                Some(cause) if !cause.is_empty() && cause != detail => format!("{detail} {cause}"),
                _ => detail.to_owned(),
            })
        })
    };
    if let Some(message) = text(value) {
        return message;
    }
    if let Some(causes) = value.as_array() {
        for cause in causes {
            if let Some(message) = text(&cause["error"]).or_else(|| cause["defect"].as_str().map(str::to_owned)) {
                return message;
            }
        }
    }
    "The server could not complete this request.".into()
}

fn has_rpc_error_tag(value: &Value, tags: &[&str]) -> bool {
    value["_tag"]
        .as_str()
        .is_some_and(|tag| tags.contains(&tag))
        || value
            .as_array()
            .is_some_and(|values| values.iter().any(|value| has_rpc_error_tag(value, tags)))
        || value
            .get("error")
            .is_some_and(|value| has_rpc_error_tag(value, tags))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_settings_authorization_matches_original_decoded_patch_and_atomic_mutation() {
        for (index, line) in include_str!("settings_rpc_scope_fixtures.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let result = required_scopes("server.updateSettings", &row["input"]);
            if row["rejected"] == true {
                assert!(result.is_err(), "source-rejected authorization row {index}");
            } else {
                assert_eq!(
                    serde_json::to_value(result.unwrap()).unwrap(),
                    row["expected"],
                    "authorization row {index}"
                );
            }
        }
    }
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
                    attachments: vec![], contexts: vec![],
                    text: "identical text".into(),
                },
            );
            model.pending_messages.insert(
                "other-thread".into(),
                PendingMessage {
                    destination: environment_b.clone(),
                    thread_id: "other-thread".into(),
                    attachments: vec![], contexts: vec![],
                    text: "original text".into(),
                },
            );
            model.pending_messages.insert(
                "failed-current".into(),
                PendingMessage {
                    destination: environment_b.clone(),
                    thread_id: "same-thread".into(),
                    attachments: vec![], contexts: vec![],
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
                attachments: vec![], contexts: vec![],
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
                    attachments: vec![], contexts: vec![],
                    text: "original".into(),
                    active_thread: None,
                    choices: NewThreadChoices::default(),
                    background: false,
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
                attachments: vec![], contexts: vec![],
                text: "edited while pending".into(),
                active_thread: None,
                choices: NewThreadChoices::default(),
                background: false,
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
                attachments: vec![], contexts: vec![],
                text: String::new(),
                active_thread: None,
                choices: NewThreadChoices::default(),
                background: false,
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
                attachments: vec![], contexts: vec![],
                text: "retain after malformed receipt".into(),
                active_thread: None,
                choices: NewThreadChoices::default(),
                background: false,
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
                attachments: vec![], contexts: vec![],
                text: "same prompt".into(),
                active_thread: None,
                choices: NewThreadChoices {
                    runtime_mode: Some(t3_contracts::RuntimeMode::FullAccess),
                    ..Default::default()
                },
                background: false,
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
            transport.socket_generation = 1;
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
    fn auto_reconnect_socket_epoch_protects_reused_ids_without_user_generation_change() {
        let handle = TransportHandle::default();
        let (old_id, _) =
            handle
                .borrow_mut()
                .rpc
                .request("terminal.observe", json!({}), RequestKind::Stream);
        let guard = UnaryRequestGuard {
            handle: handle.clone(),
            id: old_id.clone(),
            generation: 0,
        };
        {
            let mut transport = handle.borrow_mut();
            transport.socket_generation += 1;
            transport.rpc = RpcSession::default();
        }
        let (new_id, _) =
            handle
                .borrow_mut()
                .rpc
                .request("terminal.observe", json!({}), RequestKind::Stream);
        assert_eq!(old_id, new_id);
        let (sender, _receiver) = futures_channel::mpsc::channel(0);
        handle.borrow_mut().stream_waiters.insert(
            new_id.clone(),
            StreamWaiter {
                sender,
                failure: Rc::default(),
                cause: Rc::default(),
            },
        );
        drop(guard);
        assert!(handle.borrow().stream_waiters.contains_key(&new_id));
        assert_eq!(
            handle.borrow().generation,
            0,
            "automatic socket replacement stays in same user connection generation"
        );
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

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "fixture_server.rs"]
pub(crate) mod fixture_server;

#[cfg(test)]
#[path = "transport_tests.rs"]
pub(crate) mod transport_tests;

#[cfg(all(test, unix))]
#[path = "provider_manage_tests.rs"]
mod provider_manage_tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "attachment_tests.rs"]
mod attachment_tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "provider_auth_tests.rs"]
mod provider_auth_tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "diff_tests.rs"]
pub(crate) mod diff_tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "review_tests.rs"]
mod review_tests;

#[cfg(all(test, unix))]
#[path = "git_action_tests.rs"]
mod git_action_tests;

#[cfg(all(test, unix))]
#[path = "access_page_tests.rs"]
mod access_page_tests;

#[cfg(all(test, unix))]
#[path = "connect_page_tests.rs"]
mod connect_page_tests;

#[cfg(all(test, unix))]
#[path = "connect_agent_tests.rs"]
mod connect_agent_tests;

#[cfg(all(test, unix))]
#[path = "ssh_environments_tests.rs"]
mod ssh_environments_tests;

#[cfg(all(test, unix))]
#[path = "file_edit_tests.rs"]
mod file_edit_tests;

#[cfg(all(test, unix))]
#[path = "import_notice_tests.rs"]
mod import_notice_tests;

#[cfg(all(test, unix))]
#[path = "pull_request_tests.rs"]
mod pull_request_tests;

#[cfg(all(test, unix))]
#[path = "thread_title_tests.rs"]
mod thread_title_tests;

#[cfg(all(test, unix))]
#[path = "routing_tests.rs"]
mod routing_tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "rollback_tests.rs"]
mod rollback_tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "branch_tests.rs"]
mod branch_tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "keys_tests.rs"]
pub(crate) mod keys_tests;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "scheduled_tasks_tests.rs"]
mod scheduled_tasks_tests;

#[cfg(all(test, unix))]
#[path = "source_control_tests.rs"]
pub(crate) mod source_control_tests;

#[cfg(all(test, unix))]
#[path = "fake_github.rs"]
pub(crate) mod fake_github;

#[cfg(all(test, unix))]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(all(test, unix))]
#[path = "text_generation_tests.rs"]
mod text_generation_tests;

#[cfg(all(test, unix))]
#[path = "settings_rows_tests.rs"]
mod settings_rows_tests;

#[cfg(all(test, unix))]
#[path = "prompt_history_tests.rs"]
mod prompt_history_tests;

#[cfg(all(test, unix))]
#[path = "usage_tests.rs"]
mod usage_tests;

#[cfg(all(test, unix))]
#[path = "import_clone_tests.rs"]
mod import_clone_tests;

#[cfg(all(test, unix))]
#[path = "chips_tests.rs"]
mod chips_tests;

#[cfg(all(test, unix))]
#[path = "context_menu_tests.rs"]
mod context_menu_tests;

#[cfg(all(test, unix))]
#[path = "thread_relations_tests.rs"]
mod thread_relations_tests;

#[cfg(all(test, unix))]
#[path = "pull_request_gitlab_tests.rs"]
mod pull_request_gitlab_tests;

#[cfg(all(test, unix))]
#[path = "pull_request_hosts_tests.rs"]
mod pull_request_hosts_tests;

#[cfg(all(test, unix))]
#[path = "thread_settle_tests.rs"]
mod thread_settle_tests;

#[cfg(all(test, unix))]
#[path = "limit_recovery_tests.rs"]
mod limit_recovery_tests;

#[cfg(all(test, unix))]
#[path = "prompt_stash_tests.rs"]
mod prompt_stash_tests;

#[cfg(all(test, unix))]
#[path = "sidebar_sections_tests.rs"]
mod sidebar_sections_tests;

#[cfg(all(test, unix))]
#[path = "thread_row_tests.rs"]
mod thread_row_tests;

#[cfg(all(test, unix))]
#[path = "open_in_tests.rs"]
mod open_in_tests;

#[cfg(all(test, unix))]
#[path = "project_search_tests.rs"]
mod project_search_tests;

#[cfg(all(test, unix))]
#[path = "thread_selection_tests.rs"]
mod thread_selection_tests;

#[cfg(all(test, unix))]
#[path = "thread_pull_requests_panel_tests.rs"]
mod thread_pull_requests_panel_tests;

#[cfg(all(test, unix))]
#[path = "settings_search_tests.rs"]
mod settings_search_tests;
