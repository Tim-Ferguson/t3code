use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use t3_client::environments::EnvironmentCatalog;
use t3_client::{
    connection::{
        ConnectionStatus, EnvironmentEndpoint, Heartbeat, HeartbeatAction,
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
    Diagnostics,
    Storage,
    Integrations,
    Archived,
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
    /// The draft the thread starts from: it is the form's key and goes when the thread exists.
    pub draft_id: String,
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
    /// The first-run wizard: whether it is open, its step, and what the gate has decided.
    pub welcome: crate::welcome::Model,
    /// What the server said about the project and thread it made at startup.
    pub server_welcome: Option<t3_client::onboarding::Welcome>,
    /// The draft of a new thread the start form shows (`/draft/<id>`); its project is `selected_project`.
    pub active_draft: Option<String>,
    /// Moves when a draft is started, sent, discarded or restored, so the sidebar's list reads again.
    pub draft_revision: u64,
    /// What the last discard of a draft left, until it can no longer be taken back.
    pub discarded_draft: Option<crate::draft_sessions::Discarded>,
    /// Whether the saved drafts have been read (or failed to be): an address that names a draft waits for it.
    pub drafts_hydrated: bool,
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
    /// The state of every connected environment but the open one, whose is the flat fields above (`links`).
    pub parked: std::collections::BTreeMap<EnvironmentId, EnvironmentState>,
    /// Every connected or connecting environment's status, the open one's included.
    pub live: std::collections::BTreeMap<EnvironmentId, ConnectionStatus>,
    /// The saved connections (routes and tokens) kept between runs, and how often they changed.
    pub saved: crate::connection_store::Saved,
    pub saved_revision: u64,
    /// The new-thread form the person gave "Auto balance", and how that choice stands (`load_balancing.rs`).
    pub auto_balance: crate::load_balancing::Auto,
    pub error: Option<String>,
    pub draft: String,
    /// Set when a send was refused for the prompt's length; the composer shows it for as long as the prompt is still too long.
    pub prompt_length_error: Option<String>,
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
    /// The tabbed panel beside the conversation: its tabs per thread, the history of closed ones, and what shows of it.
    pub right_panel: crate::right_panel::Model,
    /// The browser sessions of each thread the panel has followed, and the device state of each environment it has read.
    pub previews: crate::preview_panel::Model,
    pub devices: crate::device_panel::Model,
    /// The composer's resting layout, which the conversation's scroll rests and the composer's use lifts.
    pub composer_rest: crate::composer_rest::Model,
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
    /// The pull request the pull requests page's panel shows, and the one the open thread's branch has to offer.
    pub pr_panel: Option<crate::pull_request_panel::Target>,
    pub pr_available: Option<crate::pull_request_panel::Target>,
    /// The reviews being written, by pull request, until each is sent: the summary and the line comments. They outlive the
    /// panel (closing it does not lose a review), and not the app.
    pub pr_reviews: std::collections::BTreeMap<String, t3_client::pull_request_line_comments::ReviewDraft>,
    /// The dialog that starts a thread from a pull request, while it is open.
    pub pr_checkout: Option<crate::pull_request_thread::Open>,
    /// The lifecycle state and draft flag of each pull request the panel has read (`useSharedPullRequestSummary`), by
    /// `right_panel::status_key`: what a pull request's tab wears when no thread's link carries a newer snapshot.
    pub pr_states: std::collections::BTreeMap<String, (String, bool)>,
    pub lifecycle: crate::thread_lifecycle::Model,
    pub server_update: crate::server_update::Model,
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
    /// The file tree beside the file panel: what it has read and which folders are open.
    pub file_tree: crate::file_tree::Model,
    /// The project group the sidebar is filtered to (the key of its row), none for every project.
    pub project_scope: Option<String>,
    /// The prompts put aside with the stash shortcut, and whether its list is open.
    pub stash: crate::prompt_stash::Model,
    /// The plan being started in a new thread, while its two commands are in flight.
    pub plan_launch: Option<String>,
    pub import: crate::import_projects::Model,
    /// The `/usage-limits` result shown over the open thread's composer.
    pub usage_panel: Option<crate::limits_view::Panel>,
    /// The threads chosen in the sidebar with Ctrl/Cmd+click and Shift+click, and where their menu is open.
    /// The thread whose link dialog is open.
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
    /// The environment this link learned it goes to (from its descriptor), before and after its socket is up: whose state it keeps.
    environment: Option<EnvironmentId>,
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
    /// The task following this link's lifecycle stream, so one link never has two.
    lifecycle: Option<dioxus::dioxus_core::Task>,
    /// The link's status while it has no environment to keep it for (a first attempt made in the background).
    status: ConnectionStatus,
    /// When the socket opened (the link's clock, milliseconds), for the supervisor to know how long a connection held.
    opened_at: Option<u64>,
    /// When the supervisor will try again, while it waits to.
    retry_at: Option<u64>,
    /// Ends the wait early.
    retry_now: Option<futures_channel::oneshot::Sender<()>>,
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
#[path = "links.rs"]
mod links;
pub use links::{EnvironmentState, TransportHandle};
use links::{Slot, set_status, slot, status_of};
pub(crate) use links::environment_of;

/// The environment a connection goes to.
pub(crate) fn environment_of_handle(handle: &TransportHandle, state: Store<UiModel>) -> Option<EnvironmentId> {
    environment_of(handle, state)
}

/// Whether the endpoint in use is on this computer's loopback interface.
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
    // The link the request is made on is settled here, and what answers it is that link's.
    let link = handle.pinned();
    let required = match required_scopes(method, &payload) {
        Ok(scopes) => scopes,
        Err(error) => {
            links::fail_on(&link, state, error);
            return None;
        }
    };
    let destination = environment_of(&link, state);
    let allowed = {
        let model = state.peek();
        destination.as_ref().is_some_and(|destination| {
            required
                .iter()
                .all(|scope| model.environments.allows(destination, *scope))
        })
    };
    if !allowed {
        links::fail_on(
            &link,
            state,
            "This connection does not have the permission required for this action.",
        );
        return None;
    }
    let connected = status_of(&link, state) == ConnectionStatus::Connected;
    let mut transport = link.borrow_mut();
    if transport.sender.is_none() || !connected || transport.destination != destination {
        drop(transport);
        links::fail_on(&link, state, "Connect to your T3 Code server first.");
        return None;
    }
    let (id, frame) = transport.rpc.request(method, payload, kind);
    if let Some(sender) = transport.sender.as_mut() {
        sender.send(ewebsock::WsMessage::Text(frame.to_string()));
    }
    drop(transport);
    clear_error(&link, state);
    Some(id)
}

/// Clearing an error that is not there would still repaint everything that shows it.
fn clear_error(link: &TransportHandle, state: Store<UiModel>) {
    match slot(link) {
        Slot::Flat => {
            if state.peek().error.is_some() {
                state.error().set(None);
            }
        }
        Slot::Parked(environment) => {
            if let Some(slice) = state.parked().write().get_mut(&environment) {
                slice.error = None;
            }
        }
        Slot::Nowhere => {}
    }
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
    // The answer is waited for on the link the request went out on, whichever environment the window is on by then.
    let handle = handle.pinned();
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
    let handle = handle.pinned();
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
    /// Follows its link whichever environment the window is on (a link's own streams), instead of ending when the window leaves it.
    background: bool,
    _guard: UnaryRequestGuard,
    /// The background-activity scope this stream keeps wanted while it runs (`background_activity`).
    _scope: Option<crate::background_activity::ScopeLease>,
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
            && (self.background || self.state.peek().destination.as_ref() == Some(&self.destination))
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
    open_stream(handle, state, method, payload, false, false)
}
/// A stream of the link itself (its shell, its configuration): it goes on while another environment is open, and ends with the link.
pub(crate) fn request_stream_background(
    handle: &TransportHandle,
    state: Store<UiModel>,
    method: &str,
    payload: Value,
) -> Result<StreamRequest, String> {
    open_stream(handle, state, method, payload, false, true)
}
/// A stream whose failure the caller shows itself (`StreamRequest::next` ends with it), so the
/// app-wide banner does not repeat it.
pub fn request_stream_reported(
    handle: &TransportHandle,
    state: Store<UiModel>,
    method: &str,
    payload: Value,
) -> Result<StreamRequest, String> {
    open_stream(handle, state, method, payload, true, false)
}
/// A stream of the link it is made on, whichever environment the window is on, whose failure the caller shows itself: what one
/// environment's sign-in runs for another (`chatgpt_handoff`).
pub(crate) fn request_stream_link_reported(
    handle: &TransportHandle,
    state: Store<UiModel>,
    method: &str,
    payload: Value,
) -> Result<StreamRequest, String> {
    open_stream(handle, state, method, payload, true, true)
}
fn open_stream(
    handle: &TransportHandle,
    state: Store<UiModel>,
    method: &str,
    payload: Value,
    reported: bool,
    background: bool,
) -> Result<StreamRequest, String> {
    let handle = &handle.pinned();
    let scope_input = payload.clone();
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
    let scope = crate::background_activity::retain_for_stream(&destination.to_string(), method, &scope_input);
    Ok(StreamRequest {
        receiver: receiver.peekable(),
        failure,
        cause,
        state,
        destination,
        ended: false,
        background,
        _scope: scope,
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

/// The connection a link is on, whichever environment the window is on (`connection_owner` is that, and only while the link is the
/// window's): what a link's own tasks check to know their socket is still the one they started on.
pub(crate) fn link_owner(link: &TransportHandle) -> Option<ConnectionOwner> {
    let transport = link.borrow();
    let destination = transport.destination.clone()?;
    Some(ConnectionOwner { generation: transport.generation, socket_generation: transport.socket_generation, destination })
}

pub fn live_config_active(handle: &TransportHandle, state: Store<UiModel>) -> bool {
    let owner = connection_owner(handle, state);
    let transport = handle.borrow();
    owner.is_some()
        && transport.config_subscription == owner
        && transport.live_config_owner == owner
}

/// `live_config_active` for a link's own tasks.
fn link_live_config(link: &TransportHandle) -> bool {
    let owner = link_owner(link);
    let transport = link.borrow();
    owner.is_some() && transport.config_subscription == owner && transport.live_config_owner == owner
}

fn typed_config_of(link: &TransportHandle, state: Store<UiModel>) -> Option<t3_contracts::ServerConfig> {
    match slot(link) {
        Slot::Flat => state.typed_config().peek().clone(),
        Slot::Parked(environment) => state.peek().parked.get(&environment).and_then(|slice| slice.typed_config.clone()),
        Slot::Nowhere => None,
    }
}

fn set_config(link: &TransportHandle, state: Store<UiModel>, config: t3_contracts::ServerConfig, value: Value) {
    match slot(link) {
        Slot::Flat => {
            state.typed_config().set(Some(config));
            state.config().set(value);
        }
        Slot::Parked(environment) => {
            if let Some(slice) = state.parked().write().get_mut(&environment) {
                slice.typed_config = Some(config);
                slice.config = value;
            }
        }
        Slot::Nowhere => {}
    }
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
    let Some(owner) = link_owner(handle) else {
        return;
    };
    if handle.borrow().config_subscription.as_ref() == Some(&owner) {
        return;
    }
    let Ok(mut wire) = request_stream_background(
        handle,
        state,
        "subscribeServerConfig",
        // `usageLimitsCommand`: this client answers `/usage-limits` itself (limits_view), so the server may advertise it.
        json!({"environmentThemes":true,"usageLimitSources":true,"usageLimitsCommand":true}),
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
            if link_owner(&guard.handle).as_ref() != Some(&guard.owner) {
                return;
            }
            let values = match values {
                Ok(values) => values,
                Err(error) => {
                    if status_of(&guard.handle, state) == ConnectionStatus::Connected {
                        links::fail_on(&guard.handle, state, error);
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
                    links::fail_on(
                        &guard.handle,
                        state,
                        format!("Invalid server configuration update: {error}"),
                    );
                    return;
                }
            };
            let mut candidate = typed_config_of(&guard.handle, state);
            for event in events {
                if let Err(error) = project_config(&mut candidate, event, &guard.owner.destination)
                {
                    set_status(&guard.handle, state, ConnectionStatus::Blocked(error.clone()));
                    links::fail_on(&guard.handle, state, error);
                    let sender = guard.handle.borrow_mut().sender.take();
                    if let Some(mut sender) = sender {
                        sender.close();
                    }
                    return;
                }
            }
            if let Some(config) = candidate {
                let value = serde_json::to_value(&config).expect("validated configuration");
                guard.handle.borrow_mut().live_config_owner = Some(guard.owner.clone());
                set_config(&guard.handle, state, config, value);
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
    // A connection that is not the window's (a request about a row of another environment) is owned by its own environment, and no
    // thread of the window is open on it.
    if let Slot::Parked(environment) = slot(handle) {
        return (transport.destination.as_ref() == Some(&environment)).then(|| ResponseOwner {
            generation: transport.generation,
            socket_generation: transport.socket_generation,
            thread_generation: transport.thread_generation,
            destination: environment,
            active_thread: None,
        });
    }
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

/// Ends an environment's connection, whichever environment the window is on, and what the connection kept parked goes with it. The
/// environment stays known (and saved): it can be connected again. When it was the open one, the window is left on nothing.
pub fn disconnect_environment(handle: &TransportHandle, mut state: Store<UiModel>, destination: &EnvironmentId) {
    if let Some(link) = handle.link_of(destination) {
        link.close();
    }
    state.parked().write().remove(destination);
    state.live().write().remove(destination);
    if state.peek().destination.as_ref() == Some(destination) {
        handle.set_active(None);
        let mut model = state.write();
        save_current_environment(&mut model);
        model.take_slice();
        model.destination = None;
        model.status = ConnectionStatus::Disconnected;
        model.view = View::Connections;
    }
}

pub fn forget_environment(
    handle: &TransportHandle,
    state: Store<UiModel>,
    destination: &EnvironmentId,
) {
    disconnect_environment(handle, state, destination);
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
    if state.peek().destination.as_ref() == Some(destination) {
        state.active_draft().set(None);
    }
    state
        .draft_attachments()
        .write()
        .retain(|target, _| target.environment != destination.as_str());
    state.attachment_error().set(None);
}

/// Whether there is a live connection to `environment` (the open one or another that stays connected), so its RPCs and signed URLs apply.
pub fn connected_to(handle: &TransportHandle, state: Store<UiModel>, environment: &str) -> bool {
    let Some(link) = EnvironmentId::new(environment).ok().and_then(|environment| handle.link_of(&environment)) else { return false };
    status_of(&link, state) == ConnectionStatus::Connected && link.borrow().sender.is_some()
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
    let link = handle.link_of(&EnvironmentId::new(environment).ok()?)?;
    let endpoint = link.borrow().endpoint.clone()?;
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

/// Leaves the open thread for the start form.
fn show_form(handle: &TransportHandle, mut state: Store<UiModel>, project: Option<String>, draft: Option<&str>) {
    cancel_thread_subscription(handle);
    let mut model = state.write();
    save_current_environment(&mut model);
    if let Some(project) = project {
        model.selected_project = Some(project);
    }
    // A new thread keeps the draft the form shows while it holds nothing and is the project's; one with work in it stays where it
    // is (in the sidebar) and the form takes another, as the original's new-thread request does.
    let previous = model.active_draft.clone();
    model.active_draft = match draft {
        Some(draft) => Some(draft.to_owned()),
        None => model.active_draft.clone().filter(|id| {
            let Some(destination) = model.destination.as_ref() else { return false };
            let target = t3_client::draft_storage::DraftTarget::draft(destination.to_string(), id.as_str());
            let document = model.draft_storage.document.borrow();
            document.draft(&target).is_some_and(|session| Some(&session.project_id) == model.selected_project.as_ref()) && !document.has_content(&target)
        }),
    };
    model.active_thread = None;
    model.thread = ThreadState::default();
    model.draft.clear();
    model.view = View::Chat;
    model.sidebar_open = false;
    let project = model.selected_project.clone().or_else(|| model.shell.snapshot.as_ref().and_then(|snapshot| snapshot.projects.first()).map(|project| project.id.to_string()));
    drop(model);
    if let Some(project) = project {
        crate::draft_sessions::ensure_active(state, &project);
    }
    // What the form left behind with nothing in it goes, unless the form took it again.
    let now = state.active_draft().peek().clone();
    if let Some(previous) = previous.filter(|previous| Some(previous) != now.as_ref()) {
        crate::draft_sessions::retire_if_empty(state, &previous);
    }
}
pub fn new_thread(handle: &TransportHandle, state: Store<UiModel>, project: Option<String>) {
    show_form(handle, state, project, None);
}
/// Opens one of the drafts, from the sidebar or its address. A draft that is not there (sent, discarded, another environment's)
/// opens nothing and says so by returning false.
pub fn open_draft(handle: &TransportHandle, state: Store<UiModel>, draft_id: &str) -> bool {
    let Some(project) = crate::draft_sessions::project_of(state, draft_id) else { return false };
    show_form(handle, state, Some(project), Some(draft_id));
    true
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
    draft_id: &str,
    selection: &t3_contracts::ModelSelection,
    mode: t3_contracts::RuntimeMode,
    workspace: t3_contracts::ThreadLaunchWorkspaceStrategy,
    initial_text: Option<&str>,
) -> Option<String> {
    if let Some(message) = initial_text.and_then(t3_client::composer_submission::prompt_length_message) {
        state.prompt_length_error().set(Some(message));
        return None;
    }
    let owned = state.peek();
    let destination = owned.destination.clone()?;
    let active_thread = owned.active_thread.clone();
    let choices = owned
        .new_thread_choices
        .get(&(destination.clone(), draft_id.into()))
        .cloned()
        .unwrap_or_default();
    if owned
        .pending_launches
        .values()
        .any(|pending| pending.destination == destination && pending.draft_id == draft_id)
    {
        return None;
    }
    if crate::draft_storage::has_unrendered(
        state,
        &t3_client::draft_storage::DraftTarget::draft(destination.to_string(), draft_id),
    ) {
        drop(owned);
        fail(
            state,
            "This saved draft contains context that the Rust composer cannot send yet. The original draft was preserved; open it in the original app to send all content.",
        );
        return None;
    }
    let target =
        t3_client::draft_storage::DraftTarget::draft(destination.to_string(), draft_id);
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
    let (composed, context, context_ids) = crate::composer_chips::for_send(state, &t3_client::draft_storage::DraftTarget::draft(destination.to_string(), draft_id), text.unwrap_or_default());
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
            draft_id: draft_id.into(),
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
    // A prompt the provider would refuse never leaves the composer (`submitComposerDraft`).
    if let Some(message) = t3_client::composer_submission::prompt_length_message(&text) {
        drop(model);
        state.prompt_length_error().set(Some(message));
        return;
    }
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
    // `/feedback` in a Codex thread is not a message to the agent: the conversation goes to OpenAI.
    if attachments.is_empty() && context_ids.is_empty() && options.edit_run.is_none() {
        if let Some(reason) = t3_client::thread_feedback::parse_command(&prompt).filter(|_| crate::thread_feedback::is_codex(state, &selection)) {
            crate::thread_feedback::submit(handle, state, &thread_id, reason);
            return;
        }
    }
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

/// A request to the connected environment's own HTTP API (a file a page downloaded, a screenshot, files for an open picker) with the
/// connection's credential, answered in bytes. The answer is dropped, as an error, when the connection it was asked on is no longer
/// the one the caller is on.
pub(crate) async fn environment_bytes(handle: &TransportHandle, state: Store<UiModel>, method: reqwest::Method, path: &str, body: Option<(String, Vec<u8>)>) -> Result<Vec<u8>, String> {
    let Some(owner) = response_owner(handle, state) else { return Err("Reconnect to read this.".into()) };
    let (endpoint, token) = {
        let transport = handle.borrow();
        let Some(endpoint) = transport.endpoint.clone() else { return Err("Connect to a server to read this.".into()) };
        (endpoint, transport.bearer_token.clone())
    };
    let mut request = reqwest::Client::new().request(method, endpoint.http(path)).header("x-t3-orchestration-protocol", "2").timeout(std::time::Duration::from_secs(120));
    if !token.is_empty() {
        request = request.bearer_auth(&token);
    }
    if let Some((content_type, bytes)) = body {
        request = request.header("content-type", content_type).body(bytes);
    }
    #[cfg(target_arch = "wasm32")]
    {
        request = match cookie_policy(!token.is_empty()) {
            CookiePolicy::Omit => request.fetch_credentials_omit(),
            CookiePolicy::SameOrigin => request.fetch_credentials_same_origin(),
        };
    }
    let answer = async {
        let response = request.send().await.map_err(|error| format!("Could not reach your T3 Code server: {error}"))?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|error| error.to_string())?;
        if status.is_success() { Ok(bytes.to_vec()) } else { Err(match String::from_utf8_lossy(&bytes).trim() { "" => format!("Server returned {status}"), text => text.to_owned() }) }
    }
    .await;
    if response_owner(handle, state) != Some(owner) {
        return Err("The connection changed while this was being read.".into());
    }
    answer
}

/// What a socket to one of the environment's media routes (`/api/preview-stream/ws`, the device hub) is given instead of a header,
/// which a socket cannot carry: nothing for a cookie session, else a ticket, which covers everything a panel opens at once.
pub(crate) async fn media_ticket(handle: &TransportHandle, state: Store<UiModel>) -> Result<Option<String>, String> {
    if handle.borrow().bearer_token.is_empty() {
        return Ok(None);
    }
    let answer = environment_json(handle, state, Some(&json!({})), "api/auth/websocket-ticket").await?;
    answer["ticket"].as_str().map(|ticket| Some(ticket.to_owned())).ok_or_else(|| "Server did not issue a WebSocket ticket".to_owned())
}

/// The address of a route of the connected environment for an element that loads it itself (an `<img>`), which cannot carry a header
/// either (`path` carries its query).
pub(crate) fn environment_http_url(handle: &TransportHandle, path: &str) -> Option<String> {
    handle.borrow().endpoint.as_ref().map(|endpoint| endpoint.http(path).to_string())
}

/// The address of a route of the connected environment for a socket (`path` carries its query).
pub(crate) fn environment_socket_url(handle: &TransportHandle, path: &str) -> Option<String> {
    let endpoint = handle.borrow().endpoint.clone()?;
    let mut url = endpoint.http(path);
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme).ok()?;
    Some(url.to_string())
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
    let link = handle.pinned();
    {
        let mut transport = link.borrow_mut();
        if let Some(mut sender) = transport.sender.take() {
            sender.close();
        }
        transport.rpc.disconnect();
        transport.fail_waiters();
    }
    set_status(&link, state, ConnectionStatus::Interrupted(reason.to_owned()));
    clear_pending(&link, state);
}

/// A request in flight when its connection drops is not replayed, so nothing waits on it any more.
fn clear_pending(link: &TransportHandle, state: Store<UiModel>) {
    match slot(link) {
        Slot::Flat => {
            state.pending_messages().write().clear();
            state.pending_launches().write().clear();
        }
        Slot::Parked(environment) => {
            if let Some(slice) = state.parked().write().get_mut(&environment) {
                slice.pending_messages.clear();
                slice.pending_launches.clear();
            }
        }
        Slot::Nowhere => {}
    }
}

/// A bootstrap credential is exchanged once; it never becomes an ordinary
/// Authorization header or a WebSocket query parameter.
/// Leaves the window at "Connecting…" before a switch to another environment begins, so that whoever then waits for the new
/// connection does not mistake the old one, still shown as connected, for it. The old one stays connected, parked.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn begin_connection(handle: &TransportHandle, state: Store<UiModel>) {
    links::park_active(handle, state);
    state.status().set(ConnectionStatus::Connecting);
    state.error().set(None);
}

/// A link for a connection to `address` that the window is on: the one that already goes to that address (its tasks are superseded),
/// or a new one. The environment that was open stays connected, parked.
fn open_activated_link(handle: &TransportHandle, state: Store<UiModel>, address: &str) -> TransportHandle {
    let link = handle.open_link(EnvironmentEndpoint::new(address).ok().as_ref());
    link.borrow_mut().generation += 1;
    links::activate(&link, state);
    link
}

pub async fn pair_and_connect(handle: TransportHandle, state: Store<UiModel>, address: String, credential: String) {
    pair_and_connect_with(handle, state, address, credential, false).await;
}

/// `pair_and_connect` for a server the person added: the token the pairing credential was exchanged for is saved with the
/// environment (`connection_store`), so that the connection comes back without pairing again.
pub async fn pair_and_remember(handle: TransportHandle, state: Store<UiModel>, address: String, credential: String) {
    pair_and_connect_with(handle, state, address, credential, true).await;
}

async fn pair_and_connect_with(
    handle: TransportHandle,
    state: Store<UiModel>,
    address: String,
    credential: String,
    remember: bool,
) {
    let handle = open_activated_link(&handle, state, &address);
    let generation = handle.borrow().generation;
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
        Ok(token) => connect_on(handle, state, vec![Route { address, token }], remember).await,
        Err(error) => {
            set_status(&handle, state, ConnectionStatus::Blocked(error.clone()));
            links::fail_on(&handle, state, error);
        }
    }
}

/// The credential a pairing link brought to this page (`/pair#token=…`, which the Authorized clients list makes), taken out of the
/// address so it is not kept in the history or the address bar (`stripPairingTokenFromUrl`). Only the web build has an address.
#[cfg(target_arch = "wasm32")]
pub(crate) fn take_pairing_link() -> Option<String> {
    let href = web_sys::window()?.location().href().ok()?;
    let credential = t3_client::access::pairing_token_from_url(&href)?;
    let clean = serde_json::to_string(&t3_client::access::address_without_pairing_token(&href)?).ok()?;
    let _ = js_sys::eval(&format!("history.replaceState(null,'',{clean})"));
    Some(credential)
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn take_pairing_link() -> Option<String> {
    None
}

/// Opens a pairing link on this server's own address. A browser that is signed in already leaves the link unused, as the original's
/// `/pair` route does (the credential is good once, and a signed-in person has no need of it); any other pairs with it and connects.
pub async fn pair_from_link(handle: TransportHandle, state: Store<UiModel>, address: String, credential: String) {
    let signed_in = async {
        let endpoint = EnvironmentEndpoint::new(&address).ok()?;
        let session = authenticated_json(&reqwest::Client::new(), &endpoint, "api/auth/session", "", false).await.ok()?;
        serde_json::from_value::<t3_contracts::AuthSessionState>(session).ok().map(|session| session.authenticated)
    }
    .await
    .unwrap_or(false);
    if signed_in {
        connect(handle, state, address, String::new()).await;
    } else {
        pair_and_connect(handle, state, address, credential).await;
    }
}

pub async fn connect(
    handle: TransportHandle,
    state: Store<UiModel>,
    address: String,
    token: String,
) {
    connect_with(handle, state, address, token, false).await;
}

/// `connect` with a bearer token the person typed in: the connection is saved with its token once it is made.
pub async fn connect_remembering(handle: TransportHandle, state: Store<UiModel>, address: String, token: String) {
    connect_with(handle, state, address, token, true).await;
}

async fn connect_with(
    handle: TransportHandle,
    state: Store<UiModel>,
    address: String,
    token: String,
    remember: bool,
) {
    let link = open_activated_link(&handle, state, &address);
    connect_on(link, state, vec![Route { address, token }], remember).await;
}

/// Connects to a server and keeps it connected without making it the window's: the environment that is open stays open. The
/// connection is tried again for as long as it is lost, as any other.
pub async fn connect_in_background(handle: TransportHandle, state: Store<UiModel>, address: String, token: String) {
    connect_routes_in_background(handle, state, vec![Route { address, token }], None).await;
}

/// `connect_in_background` for an environment with several routes, preferred first: a lost connection tries the others at once, and
/// waits (with the original's backoff) only when none answers. When the environment is known (a saved one), the connection is its
/// from the start: it is listed as connecting and as lost while it cannot be reached, and a route that answers as another
/// environment is refused before its token is sent.
pub async fn connect_routes_in_background(handle: TransportHandle, state: Store<UiModel>, routes: Vec<Route>, environment: Option<EnvironmentId>) {
    let Some(first) = routes.first() else { return };
    if environment.as_ref().is_some_and(|environment| handle.link_of(environment).is_some()) {
        return;
    }
    let link = handle.open_link(EnvironmentEndpoint::new(&first.address).ok().as_ref());
    link.borrow_mut().generation += 1;
    if let Some(environment) = environment {
        link.borrow_mut().environment = Some(environment.clone());
        if !matches!(slot(&link), Slot::Flat) {
            state.parked().write().entry(environment).or_default();
        }
        set_status(&link, state, ConnectionStatus::Connecting);
    }
    connect_on(link, state, routes, false).await;
}

/// One way to reach an environment: an address and the token it takes. The token is a secret.
#[derive(Clone)]
pub struct Route {
    pub address: String,
    pub token: String,
}

/// The link's connection and its reconnecting, until the link is superseded or ended (`EnvironmentSupervisor`): a lost connection
/// tries the next route at once, and when a whole pass of the routes has failed it waits `retry_delay_ms` of the failures in a row
/// (`t3_client::supervisor`), which a connection that held for thirty seconds starts over. An explicit retry skips the wait.
async fn connect_on(
    link: TransportHandle,
    state: Store<UiModel>,
    routes: Vec<Route>,
    mut remember: bool,
) {
    let generation = {
        let mut transport = link.borrow_mut();
        transport.generation += 1;
        transport.generation
    };
    let clock = link.clock();
    let mut backoff = t3_client::supervisor::Backoff::default();
    let (mut route, mut failed_in_a_row) = (0usize, 0usize);
    loop {
        let Route { address, token } = routes[route].clone();
        connect_once(
            link.clone(),
            state,
            address,
            token,
            generation,
            remember,
        )
        .await;
        // Saved once, by the attempt that got through.
        if link.borrow().opened {
            remember = false;
        }
        if link.borrow().generation != generation
            || !matches!(status_of(&link, state), ConnectionStatus::Interrupted(_))
        {
            return;
        }
        if link.borrow().opened {
            // It was a connection: the routes start again from the preferred one, and the failures it ended are counted against its
            // length (`Backoff::failed`).
            failed_in_a_row = 0;
            let held_from = link.borrow().opened_at;
            if let Some(held_from) = held_from {
                backoff.established(held_from);
            }
        }
        failed_in_a_row += 1;
        let (next, wait) = t3_client::supervisor::next_route(routes.len(), route, failed_in_a_row);
        route = next;
        if wait {
            let delay = backoff.failed((clock.now)(), random());
            link.borrow_mut().retry_at = Some((clock.now)() + delay);
            let retry_now = wait_for_retry(&link, &clock, delay).await;
            link.borrow_mut().retry_at = None;
            if retry_now {
                backoff.reset();
                failed_in_a_row = 0;
                route = 0;
            }
        }
        if link.borrow().generation != generation {
            return;
        }
    }
}

/// Waits for `delay` milliseconds of the link's clock, or for an explicit retry; whether it was the retry.
async fn wait_for_retry(link: &TransportHandle, clock: &links::Clock, delay: u64) -> bool {
    let (sender, receiver) = futures_channel::oneshot::channel();
    link.borrow_mut().retry_now = Some(sender);
    let sleep = (clock.sleep)(delay);
    match futures_util::future::select(sleep, receiver).await {
        futures_util::future::Either::Left(_) => false,
        futures_util::future::Either::Right((retry, _)) => retry.is_ok(),
    }
}

/// Asks a connection that is waiting to try again to try now, and to start its waits over (the "Retry" of the Connections page).
pub fn retry_now(handle: &TransportHandle, environment: &EnvironmentId) {
    if let Some(link) = handle.link_of(environment) {
        retry_link_now(&link);
    }
}

fn retry_link_now(link: &TransportHandle) {
    let sender = link.borrow_mut().retry_now.take();
    if let Some(sender) = sender {
        let _ = sender.send(());
    }
}

/// A number in [0, 1) for the spread of retries.
fn random() -> f64 {
    #[cfg(test)]
    if let Some(value) = TEST_RANDOM.with(|value| value.get()) {
        return value;
    }
    (uuid::Uuid::new_v4().as_u128() >> 75) as f64 / (1u64 << 53) as f64
}
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_RANDOM: std::cell::Cell<Option<f64>> = const { std::cell::Cell::new(None) };
}

/// Whether this attempt retries the connection that was lost: the same server, after it was
/// interrupted. Then what the person was looking at stays on screen, labelled as disconnected,
/// rather than the app emptying itself (and saying "Add a project") for as long as the server is down.
fn retries_lost_connection(link: &TransportHandle, state: Store<UiModel>, _address: &str) -> bool {
    matches!(status_of(link, state), ConnectionStatus::Interrupted(_)) && environment_of(link, state).is_some()
}

/// What an environment's view shows before the environment has answered: nothing.
fn clear_visible(model: &mut UiModel) {
    model.destination = None;
    model.shell = ShellState::default();
    model.thread = ThreadState::default();
    model.active_thread = None;
    model.selected_project = None;
    model.active_draft = None;
    model.config = Value::Null;
    model.typed_config = None;
    model.draft.clear();
}

fn reset_for_connect(link: &TransportHandle, mut state: Store<UiModel>, keep_visible: bool) {
    {
        let mut transport = link.borrow_mut();
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
    let environment = environment_of(link, state);
    if let Some(environment) = &environment {
        state.environments().write().revoke(environment);
    }
    match slot(link) {
        Slot::Flat => {
            let mut model = state.write();
            save_current_environment(&mut model);
            if !keep_visible {
                clear_visible(&mut model);
            }
            model.status = ConnectionStatus::Connecting;
            model.error = None;
            model.pending_messages.clear();
            model.pending_launches.clear();
            model.grants = SessionGrantInput::default();
        }
        Slot::Parked(environment) => {
            if let Some(slice) = state.parked().write().get_mut(&environment) {
                if !keep_visible {
                    *slice = EnvironmentState::default();
                }
                slice.status = ConnectionStatus::Connecting;
                slice.error = None;
                slice.pending_messages.clear();
                slice.pending_launches.clear();
                slice.grants = SessionGrantInput::default();
            }
        }
        Slot::Nowhere => {}
    }
    set_status(link, state, ConnectionStatus::Connecting);
}

async fn connect_once(
    handle: TransportHandle,
    mut state: Store<UiModel>,
    address: String,
    token: String,
    generation: u64,
    remember: bool,
) {
    let handle = handle.pinned();
    let keep_visible = retries_lost_connection(&handle, state, &address);
    reset_for_connect(&handle, state, keep_visible);
    let endpoint = match EnvironmentEndpoint::new(&address) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            links::fail_on(&handle, state, error.to_string());
            set_status(&handle, state, ConnectionStatus::Disconnected);
            if matches!(slot(&handle), Slot::Flat) {
                state.view().set(View::Connections);
            }
            return;
        }
    };
    let client = reqwest::Client::new();
    let known_environment = handle.borrow().environment.clone();
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
        // The routes of a link are one environment's: an address that answers as another environment is not another way to it, and
        // the token of the route is not sent to it.
        if let Some(known) = &known_environment {
            if *known != destination {
                return Err(format!("That address reaches {label}, a different environment. Add it as its own environment instead."));
            }
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
                clear_slice(&handle, state);
            }
            set_status(
                &handle,
                state,
                if unreachable {
                    ConnectionStatus::Interrupted(error.clone())
                } else {
                    ConnectionStatus::Blocked(error.clone())
                },
            );
            if matches!(status_of(&handle, state), ConnectionStatus::Blocked(_)) && matches!(slot(&handle), Slot::Flat) {
                state.view().set(View::Connections);
            }
            links::fail_on(&handle, state, error);
            return;
        }
    };
    let saved_label = label.clone();
    let registration =
        state
            .environments()
            .write()
            .register(&endpoint, destination.clone(), label, protocol);
    if let Err(error) = registration {
        let error = error.to_string();
        set_status(&handle, state, ConnectionStatus::Blocked(error.clone()));
        links::fail_on(&handle, state, error);
        return;
    }
    if remember {
        crate::connection_store::remember(state, endpoint.http("").as_str(), &destination, &saved_label, &token);
    }
    // One connection per environment: an address that goes to an environment already connected another way adds nothing, and one
    // that goes to an environment whose connection is lost takes it over, with what that connection kept.
    if let Some(other) = handle.link_of(&destination).filter(|other| other.id() != handle.id()) {
        let was_open = handle.id().is_some() && handle.id() == handle.active_id();
        if status_of(&other, state) == ConnectionStatus::Connected {
            handle.close();
            if was_open {
                switch_to_link(&other, state, true);
            }
            return;
        }
        links::take_over(&handle, &other, state);
    }
    handle.borrow_mut().environment = Some(destination.clone());
    let draft_of = |model: &UiModel| {
        model
            .active_thread
            .as_ref()
            .and_then(|thread| model.environments.records.get(&destination).and_then(|record| record.cache.drafts.get(thread)))
            .cloned()
            .unwrap_or_default()
    };
    match slot(&handle) {
        Slot::Flat => {
            let mut model = state.write();
            if model.destination.as_ref() != Some(&destination) {
                model.config = Value::Null;
                model.typed_config = None;
            }
            model.active_draft = None;
            model.draft = draft_of(&model);
            model.destination = Some(destination.clone());
            model.grants = grants.clone();
            model
                .environments
                .set_session(&destination, grants.clone())
                .expect("registered environment");
        }
        Slot::Parked(_) | Slot::Nowhere => {
            let mut model = state.write();
            let slice = model.parked.entry(destination.clone()).or_default();
            slice.status = ConnectionStatus::Connecting;
            slice.grants = grants.clone();
            model
                .environments
                .set_session(&destination, grants.clone())
                .expect("registered environment");
        }
    }
    set_status(&handle, state, ConnectionStatus::Connecting);
    if matches!(slot(&handle), Slot::Flat) {
        crate::draft_storage::restore_destination(state);
    }
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
            links::fail_on(&handle, state, error.to_string());
            set_status(&handle, state, ConnectionStatus::Interrupted(error.to_string()));
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
    // This timer never writes render state while the connection is healthy.
    // Socket traffic cannot postpone the protocol heartbeat.
    let heartbeat_clock = handle.clock();
    spawn(async move {
        loop {
            (heartbeat_clock.sleep)(5000).await;
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
                    {
                        let now = (handle.clock().now)();
                        let mut transport = handle.borrow_mut();
                        transport.opened = true;
                        transport.opened_at = Some(now);
                    }
                    set_status(&handle, state, ConnectionStatus::Connected);
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
                    // Only the open environment has a thread open.
                    if matches!(slot(&handle), Slot::Flat) {
                        let active = state.peek().active_thread.clone();
                        if let Some(active) = active {
                            select_thread(&handle, state, active);
                        }
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
                        Err(error) => links::fail_on(&handle, state, error.to_string()),
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
            Some(SocketWake::Heartbeat) if status_of(&handle, state) == ConnectionStatus::Connected => {
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

/// What an environment's view shows before the environment has answered, for the link's own slice.
fn clear_slice(link: &TransportHandle, mut state: Store<UiModel>) {
    match slot(link) {
        Slot::Flat => clear_visible(&mut state.write()),
        Slot::Parked(environment) => {
            if let Some(slice) = state.parked().write().get_mut(&environment) {
                *slice = EnvironmentState::default();
            }
        }
        Slot::Nowhere => {}
    }
}

/// Makes a connected environment the one the window is on. Nothing is asked of the others, and nothing of this one is lost: its
/// shell has kept arriving while it was away. Only the open environment has a thread stream and a lifecycle stream, which this one
/// takes up again. Returns whether there is such an environment.
pub fn switch_to_environment(handle: &TransportHandle, state: Store<UiModel>, environment: &EnvironmentId) -> bool {
    switch_to(handle, state, environment, true)
}

/// `resume` asks again for the thread the environment had open; a caller that is about to open another leaves it.
fn switch_to(handle: &TransportHandle, state: Store<UiModel>, environment: &EnvironmentId, resume: bool) -> bool {
    match handle.link_of(environment) {
        Some(link) => {
            switch_to_link(&link, state, resume);
            true
        }
        None => false,
    }
}

fn switch_to_link(link: &TransportHandle, state: Store<UiModel>, resume: bool) {
    links::activate(link, state);
    if status_of(link, state) == ConnectionStatus::Connected {
        if resume {
            resubscribe_thread(link, state);
        }
        crate::lifecycle::start(link, state);
    }
}

/// Opens a thread of a connected environment, whichever one the window is on: the window moves to the environment first (parked
/// state and all, `links`), then the thread is opened as `select_thread` opens it. False, with nothing changed, when the
/// environment is not connected. Every row that can belong to another environment opens its thread through this.
pub fn select_thread_in(handle: &TransportHandle, state: Store<UiModel>, environment: &EnvironmentId, thread_id: String) -> bool {
    if state.peek().destination.as_ref() != Some(environment) && !switch_to(handle, state, environment, false) {
        return false;
    }
    select_thread(handle, state, thread_id);
    true
}

/// The start form for a project of a connected environment, moving the window to the environment first.
pub fn new_thread_in(handle: &TransportHandle, state: Store<UiModel>, environment: &EnvironmentId, project: Option<String>) -> bool {
    if state.peek().destination.as_ref() != Some(environment) && !switch_to(handle, state, environment, false) {
        return false;
    }
    new_thread(handle, state, project);
    true
}

/// Asks again for the open thread, without clearing what is drawn of it: the server's snapshot replaces it.
fn resubscribe_thread(link: &TransportHandle, state: Store<UiModel>) {
    let Some(thread_id) = state.peek().active_thread.clone() else { return };
    cancel_thread_subscription(link);
    let id = request(
        link,
        state,
        "orchestration.subscribeThread",
        json!({"threadId":thread_id,"requestCompletionMarker":true,"acceptBoundedSnapshot":true}),
        RequestKind::Stream,
    );
    link.borrow_mut().thread_subscription = id;
}

/// Keeps the one task following a link's lifecycle stream: a task started before it is ended.
pub(crate) fn keep_lifecycle(link: &TransportHandle, task: dioxus::dioxus_core::Task) {
    let previous = link.borrow_mut().lifecycle.replace(task);
    if let Some(previous) = previous {
        previous.cancel();
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
                    Ok(items) => apply_shell(handle, state, items),
                    Err(error) => links::fail_on(handle, state, format!("Could not decode shell update: {error}")),
                }
            } else if method == "orchestration.subscribeThread" {
                // The open thread is the open environment's: a link the window has left has no thread stream (it was cancelled on leaving).
                if !matches!(slot(handle), Slot::Flat) {
                    return;
                }
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
                        links::fail_on(handle, state, format!("Invalid server configuration: {error}"));
                        return;
                    }
                };
                let expected = environment_of(handle, state);
                if expected.as_ref().is_none_or(|expected| {
                    value["environment"]["environmentId"].as_str() != Some(expected.as_str())
                }) {
                    set_status(handle, state, ConnectionStatus::Blocked(
                        "Server identity changed during connection".into(),
                    ));
                    links::fail_on(handle, state, "Server identity changed during connection");
                    let sender = handle.borrow_mut().sender.take();
                    if let Some(mut sender) = sender {
                        sender.close();
                    }
                    return;
                }
                // A unary refresh can finish after a newer streamed registry
                // update. Once live projection owns this socket, only that
                // ordered subscription may replace the config.
                if link_live_config(handle) {
                    return;
                }
                let typed =
                    serde_json::from_value(value.clone()).expect("configuration validated above");
                set_config(handle, state, typed, value);
            } else if method == "orchestration.launchThread" {
                match serde_json::from_value::<t3_contracts::ThreadLaunchResult>(value) {
                    Ok(receipt) => {
                        finish_pending_launch(handle, state, &id, Some(receipt.thread_id.as_str()))
                    }
                    Err(error) => {
                        take_pending_launch(handle, state, &id);
                        links::fail_on(handle, state, format!("Invalid thread launch receipt: {error}"));
                    }
                }
            }
            // A reply to a request that shows its own answer is not a message's receipt, and
            // clearing the map for it would repaint the composer for every status refresh.
            if !reported {
                finish_pending_message_on(handle, state, &id, true);
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
            finish_pending_message_on(handle, state, &id, false);
            take_pending_launch(handle, state, &id);
            if !missing_attachment && !diff_request && !vcs_request && !reported {
                links::fail_on(handle, state, rpc_error_message(&cause));
            }
        }
        RpcEvent::Defect(error) => links::fail_on(handle, state, rpc_error_message(&error)),
        _ => {}
    }
}

fn finish_pending_launch(
    handle: &TransportHandle,
    state: Store<UiModel>,
    id: &str,
    thread_id: Option<&str>,
) {
    let pending = take_pending_launch(handle, state, id);
    let Some(pending) = pending else {
        return;
    };
    let Some(thread_id) = thread_id else {
        links::fail_on(
            handle,
            state,
            "The server returned an invalid thread launch receipt.",
        );
        return;
    };
    let key = (pending.destination.clone(), pending.draft_id.clone());
    let draft_target = t3_client::draft_storage::DraftTarget::draft(pending.destination.to_string(), pending.draft_id.clone());
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
    // The draft's thread exists, so the draft is gone, unless more went into it while the server worked.
    let current = {
        let model = state.peek();
        model.destination.as_ref() == Some(&pending.destination)
            && model.active_thread == pending.active_thread
            && model.active_draft.as_deref() == Some(pending.draft_id.as_str())
    };
    if unchanged_text && unchanged_choices {
        crate::draft_sessions::promote(state, &pending.destination, &pending.draft_id);
    }
    // The launch claimed these uploads, so they leave the draft whatever else changed.
    // After the acknowledgement above, which resets the saved attachment list.
    crate::attachments::sent(state, &draft_target, &pending.attachments);
    crate::composer_chips::sent(state, &draft_target, &pending.contexts);
    if current && !pending.background {
        select_thread(handle, state, thread_id.to_owned());
    }
}

/// Applies a shell chunk to the link's own shell: the open environment's, or a parked one's, which the window does not draw.
fn apply_shell(link: &TransportHandle, state: Store<UiModel>, items: Vec<ShellStreamItem>) {
    match slot(link) {
        Slot::Flat => {
            let mut shell_store = state.shell();
            let mut shell = shell_store.write();
            for item in items {
                shell.apply(item);
            }
        }
        Slot::Parked(environment) => {
            if let Some(slice) = state.parked().write().get_mut(&environment) {
                for item in items {
                    slice.shell.apply(item);
                }
            }
        }
        Slot::Nowhere => {}
    }
}

/// The launch a reply is the receipt of, taken from the slice its link keeps it in.
fn take_pending_launch(link: &TransportHandle, state: Store<UiModel>, id: &str) -> Option<PendingLaunch> {
    match slot(link) {
        Slot::Flat => {
            if state.pending_launches().peek().contains_key(id) {
                state.pending_launches().write().remove(id)
            } else {
                None
            }
        }
        Slot::Parked(environment) => state.parked().write().get_mut(&environment).and_then(|slice| slice.pending_launches.remove(id)),
        Slot::Nowhere => None,
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

/// A reply on a link that is the window's, or on one that is parked.
fn finish_pending_message_on(link: &TransportHandle, state: Store<UiModel>, id: &str, success: bool) {
    let parked = match slot(link) {
        Slot::Parked(environment) => Some(environment),
        Slot::Flat => None,
        Slot::Nowhere => return,
    };
    finish_pending_message_in(state, parked.as_ref(), id, success);
}

fn finish_pending_message(state: Store<UiModel>, id: &str, success: bool) {
    finish_pending_message_in(state, None, id, success);
}

/// The message a reply is the receipt of, taken from the slice its link keeps it in (`parked` names a parked environment).
fn finish_pending_message_in(state: Store<UiModel>, parked: Option<&EnvironmentId>, id: &str, success: bool) {
    // Every reply passes here, and a write to the map repaints the composer even when the reply
    // is not a message's (a refresh, a git action), so only a pending message is written.
    let pending = match parked {
        None => {
            if !state.pending_messages().peek().contains_key(id) {
                return;
            }
            state.pending_messages().write().remove(id)
        }
        Some(environment) => {
            if !state.peek().parked.get(environment).is_some_and(|slice| slice.pending_messages.contains_key(id)) {
                return;
            }
            state.parked().write().get_mut(environment).and_then(|slice| slice.pending_messages.remove(id))
        }
    };
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
        // `ServerSelfUpdateError.message` is "Server update failed: <reason>".
        if error["_tag"] == "ServerSelfUpdateError" {
            if let Some(reason) = error["reason"].as_str() {
                return Some(format!("Server update failed: {reason}"));
            }
        }
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
                    draft_id: "same-project".into(),
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
                    draft_id: "same-project".into(),
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
                draft_id: key.1.clone(),
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
                draft_id: key.1.clone(),
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
                draft_id: key.1.clone(),
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
#[path = "fake_issuer.rs"]
mod fake_issuer;

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

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "pair_link_tests.rs"]
mod pair_link_tests;

#[cfg(all(test, unix))]
#[path = "diagnostics_tests.rs"]
mod diagnostics_tests;

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

#[cfg(all(test, unix))]
#[path = "pull_request_routing_tests.rs"]
mod pull_request_routing_tests;

#[cfg(all(test, unix))]
#[path = "folder_browser_tests.rs"]
mod folder_browser_tests;

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
#[path = "usage_hub_tests.rs"]
mod usage_hub_tests;

#[cfg(all(test, unix))]
#[path = "import_clone_tests.rs"]
mod import_clone_tests;

#[cfg(all(test, unix))]
#[path = "chips_tests.rs"]
mod chips_tests;
#[cfg(all(test, unix))]
#[path = "draft_sessions_tests.rs"]
mod draft_sessions_tests;

#[cfg(all(test, unix))]
#[path = "welcome_tests.rs"]
mod welcome_tests;

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
#[path = "right_panel_tests.rs"]
mod right_panel_tests;
#[cfg(all(test, unix))]
#[path = "preview_tests.rs"]
mod preview_tests;
#[cfg(all(test, unix))]
#[path = "device_tests.rs"]
mod device_tests;

#[cfg(all(test, unix))]
#[path = "settings_search_tests.rs"]
mod settings_search_tests;

#[cfg(all(test, unix))]
#[path = "sidebar_groups_tests.rs"]
mod sidebar_groups_tests;

#[cfg(all(test, unix))]
#[path = "file_tree_tests.rs"]
mod file_tree_tests;

#[cfg(all(test, unix))]
#[path = "background_activity_tests.rs"]
mod background_activity_tests;

#[cfg(all(test, unix))]
#[path = "storage_tests.rs"]
mod storage_tests;

#[cfg(all(test, unix))]
#[path = "composer_validation_tests.rs"]
mod composer_validation_tests;

#[cfg(all(test, unix))]
#[path = "server_update_tests.rs"]
mod server_update_tests;

#[cfg(all(test, unix))]
#[path = "acp_registry_tests.rs"]
mod acp_registry_tests;

#[cfg(all(test, unix))]
#[path = "connection_store_tests.rs"]
mod connection_store_tests;

#[cfg(all(test, unix))]
#[path = "environments_tests.rs"]
mod environments_tests;

#[cfg(all(test, unix))]
#[path = "fleet_tests.rs"]
mod fleet_tests;
