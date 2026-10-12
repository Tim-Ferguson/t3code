//! The server's own browser (apps/server/src/preview/ServerBrowser.ts): the tabs a thread's preview opens in a headless browser on the
//! environment, the agent that drives them through the automation broker, and the viewers that watch and take over (`server_browser_viewer`).
//!
//! BUILT OFF. The browser is behind one switch, `T3CODE_RUST_SERVER_BROWSER`, that defaults to off: with it off the server starts no browser,
//! advertises no `serverBrowser` capability and answers the routes as the original does when there is no such tab (`server_browser_http`).
//! Everything here is a port of the original's behaviour; where the original has no guard, none is added here, and the place says so with a
//! `TODO(original ...)` until the policy for what an agent's browser may reach is decided.
//!
//! A tab is a `Page` of a `ServerBrowserContexts` context. Everything that changes it is serialised by its `SessionControl`: the agent's
//! operations, a viewer's input while it has taken control, and system changes (size, appearance). A tab is made when the manager publishes
//! a `server` session, whether an agent opened it, a person did, or a page opened a popup.
use crate::{
    preview_automation_broker::PreviewAutomationBroker,
    preview_manager::{PreviewManager, normalize_preview_url},
    server_browser_contexts::{BrowserContext, LaunchFailure, ServerBrowserContexts},
    server_browser_input::{Point, PointerPhase, PointerReporter},
    server_browser_page::{DIAGNOSTIC_BUFFER_LIMIT, OpError, OpResult, Page, PageEvent, PageSession, Readiness, SnapshotDiagnostics, parse_net_error},
    session_control::{BrowserControlInterrupted, SessionControl},
};
use chrono::SecondsFormat;
use futures_util::{FutureExt, future::{BoxFuture, Shared}};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use t3_contracts::{
    INCOGNITO_BROWSER_PROFILE_ID, PROVIDER_SEND_TURN_MAX_FILE_BYTES, PreviewAutomationClickInput, PreviewAutomationDialogInput, PreviewAutomationDragInput, PreviewAutomationEvaluateInput,
    PreviewAutomationHoverInput, PreviewAutomationNavigateInput, PreviewAutomationOpenInput, PreviewAutomationOperation, PreviewAutomationPressInput, PreviewAutomationRequest,
    PreviewAutomationResizeInput, PreviewAutomationScrollInput, PreviewAutomationSelectInput, PreviewAutomationSetColorSchemeInput, PreviewAutomationTypeInput,
    PreviewAutomationUploadInput, PreviewAutomationWaitForInput, PreviewResizeMode, PreviewResizeOrientation, SERVER_BROWSER_AUTOMATION_CLIENT_ID,
};
use tokio::{
    sync::{Mutex as AsyncMutex, mpsc},
    task::JoinHandle,
    time::Instant,
};

/// The one switch. The server browser exists only where this is `1` or `true`; its absence is off.
pub const SWITCH: &str = "T3CODE_RUST_SERVER_BROWSER";

/// Whether an operator switched the server browser on.
pub fn switch_on(variable: impl Fn(&str) -> Option<String>) -> bool {
    matches!(variable(SWITCH).as_deref().map(str::trim), Some("1") | Some("true"))
}

pub(crate) const RENDER_SCALE: f64 = 2.0;
pub(crate) const UNATTACHED_FILL_VIEWPORT: (u32, u32) = (1280, 800);
pub(crate) const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(15);
const NAVIGATION_TIMEOUT_MS: u64 = 15_000;
const ACTION_TIMELINE_LIMIT: usize = 50;
const RECORDING_QUALITY: u32 = 90;
/// One agent session and the whole server; each tab holds a renderer process.
pub const AGENT_TAB_LIMIT: usize = 8;
pub const SERVER_TAB_LIMIT: usize = 32;
/// Agent tabs nobody watches close after this long without an agent request.
pub const AGENT_TAB_IDLE: Duration = Duration::from_secs(30 * 60);
pub const IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const HOST_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const DOWNLOAD_LIMIT: usize = 20;
const DOWNLOAD_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// The agent cursor glides to its target, then pulses just before the press, like desktop tabs.
const AGENT_CURSOR_MOVE: Duration = Duration::from_millis(160);
const AGENT_CURSOR_CLICK_LEAD: Duration = Duration::from_millis(40);

/// A tab the viewer or the agent named is not there.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerBrowserError {
    TabNotFound { thread_id: String, tab_id: String },
    /// The browser could not start or the tab could not be made.
    Launch(LaunchCause),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LaunchCause {
    Browser(LaunchFailure),
    Operation(OpError),
}

impl ServerBrowserError {
    pub fn message(&self) -> String {
        match self {
            Self::TabNotFound { .. } => "The server preview tab does not exist.".into(),
            Self::Launch(_) => "The server preview browser could not start.".into(),
        }
    }
    /// The host setup the browser is missing, if that is why it could not start.
    pub fn host_setup(&self) -> Option<t3_contracts::PreviewStreamHostSetup> {
        let Self::Launch(LaunchCause::Browser(LaunchFailure::Host(host))) = self else { return None };
        let (need, command) = match host {
            crate::preview_browser_host::PreviewBrowserHostError::Sandbox { setup_command } => ("sandbox", setup_command),
            crate::preview_browser_host::PreviewBrowserHostError::Libraries { setup_command, .. } => ("libraries", setup_command),
        };
        serde_json::from_value(json!({"need": need, "command": command})).ok()
    }
}
impl std::fmt::Display for ServerBrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}
impl std::error::Error for ServerBrowserError {}

pub(crate) fn launch_error(error: OpError) -> ServerBrowserError {
    ServerBrowserError::Launch(LaunchCause::Operation(error))
}
fn failure_error(failure: LaunchFailure) -> ServerBrowserError {
    ServerBrowserError::Launch(LaunchCause::Browser(failure))
}

pub(crate) fn tab_key(thread_id: &str, tab_id: &str) -> String {
    format!("{thread_id}\u{0}{tab_id}")
}

pub(crate) fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn push_bounded(buffer: &mut Vec<Value>, entry: Value, limit: usize) {
    buffer.push(entry);
    if buffer.len() > limit {
        buffer.drain(..buffer.len() - limit);
    }
}

/// The fixed size a viewport setting asks for; none for fill.
fn fixed_size(setting: &Value) -> Option<(u32, u32)> {
    if setting["_tag"] == "fill" {
        return None;
    }
    Some((setting["width"].as_u64()? as u32, setting["height"].as_u64()? as u32))
}

fn fill_setting() -> Value {
    json!({"_tag": "fill"})
}

#[derive(Debug, Clone)]
pub(crate) struct ServerDownload {
    pub id: String,
    pub file_name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub url: String,
    pub completed_at: String,
}

/// A page's open file picker, waiting for the controlling viewer's files.
#[derive(Debug, Clone)]
pub(crate) struct OpenChooser {
    pub id: String,
    pub backend_node_id: i64,
    pub multiple: bool,
    pub accept: String,
}

#[derive(Debug, Clone)]
pub(crate) struct OpenDialog {
    pub kind: String,
    pub message: String,
    pub default_value: String,
}

/// A recording of a tab: the page that encodes it, and the screencast that feeds it.
pub(crate) struct Recording {
    pub encoder: crate::server_browser_contexts::ScratchPage,
    pub session: PageSession,
    pub started_at: String,
    /// Frames still being handed to the encoder; stopping waits for them.
    pub in_flight: Arc<tokio::sync::watch::Sender<usize>>,
}

pub(crate) struct TabState {
    pub used_at: Instant,
    pub console: Vec<Value>,
    pub network: Vec<Value>,
    pub timeline: Vec<Value>,
    pub downloads: Vec<ServerDownload>,
    pub file_chooser: Option<OpenChooser>,
    pub dialog: Option<OpenDialog>,
    /// The viewport setting as the manager publishes it.
    pub setting: Value,
    pub color_scheme: String,
    pub zoom_factor: f64,
}

pub(crate) struct Tab {
    pub thread_id: String,
    pub tab_id: String,
    pub page: Page,
    pub context: Arc<BrowserContext>,
    pub created_at: Instant,
    pub state: Mutex<TabState>,
    pub viewers: Mutex<Vec<Arc<crate::server_browser_viewer::Viewer>>>,
    pub control: SessionControl,
    /// The tab's own storage context, closed with it. Popups share their opener's.
    pub isolated_context: bool,
    pub profile_id: Option<String>,
    /// Set when a page in another tab opened this one with `window.open` or a link.
    pub opener_tab_id: Option<String>,
    pub loading: AtomicBool,
    pub closing: AtomicBool,
    pub recording: AsyncMutex<Option<Recording>>,
    /// Serialises captures and recording start/stop.
    pub capture_lock: AsyncMutex<()>,
    /// Scaled captures rendering now; screencasts stay stopped meanwhile.
    pub capturing: AtomicUsize,
    pub initial_navigation: Mutex<Option<Shared<BoxFuture<'static, OpResult<()>>>>>,
    /// How many of the page's events the tab has acted on, for a test to wait for one it caused.
    pub handled: tokio::sync::watch::Sender<u64>,
    /// The work that closes the tab's page and removes its files once it is dropped.
    pub ended: Mutex<Option<JoinHandle<()>>>,
}

impl Tab {
    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, TabState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    /// Resolves once a dropped tab has closed its page and removed its files.
    #[cfg(test)]
    pub(crate) async fn ended(&self) {
        let ended = Core::lock(&self.ended).take();
        if let Some(ended) = ended {
            let _ = ended.await;
        }
    }
    pub(crate) fn viewers(&self) -> Vec<Arc<crate::server_browser_viewer::Viewer>> {
        self.viewers.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
    pub(crate) fn key(&self) -> String {
        tab_key(&self.thread_id, &self.tab_id)
    }
    /// The viewer that has taken control, if it is attached.
    pub(crate) fn controller(&self) -> Option<Arc<crate::server_browser_viewer::Viewer>> {
        let id = self.control.controller()?;
        self.viewers().into_iter().find(|viewer| viewer.id == id)
    }
}

pub(crate) struct Adopted {
    pub page: Page,
    pub events: mpsc::UnboundedReceiver<PageEvent>,
    pub context: Arc<BrowserContext>,
    pub opener_tab_id: String,
}

type Opening = Shared<BoxFuture<'static, Result<Arc<Tab>, ServerBrowserError>>>;

pub struct ServerBrowserOptions {
    pub contexts: ServerBrowserContexts,
    pub manager: Arc<PreviewManager>,
    pub broker: Arc<PreviewAutomationBroker>,
    pub environment_id: String,
    pub downloads_dir: PathBuf,
    pub attachments_dir: PathBuf,
    /// The command that sets the host up, as a warning at startup names it.
    pub setup_command: String,
    /// Whether the host blocks the sandbox the browser runs in, checked once at startup.
    pub sandbox_blocked: bool,
}

pub(crate) struct Core {
    pub contexts: ServerBrowserContexts,
    pub manager: Arc<PreviewManager>,
    pub broker: Arc<PreviewAutomationBroker>,
    pub environment_id: String,
    pub downloads_dir: PathBuf,
    pub attachments_dir: PathBuf,
    pub tabs: Mutex<HashMap<String, Arc<Tab>>>,
    pub pending: Mutex<HashMap<String, Opening>>,
    /// Sessions closed while their tab was still opening; the open discards its page.
    pub closed_pending: Mutex<HashSet<String>>,
    pub adopted: Mutex<HashMap<String, Adopted>>,
    pub host_connection: Mutex<Option<String>>,
    pub viewer_resize_order: AtomicU64,
    pub pointer_sequence: AtomicU64,
    /// Downloads begun and not finished: guid to (tab key, url, suggested name).
    pub pending_downloads: Mutex<HashMap<String, (String, String, String)>>,
    pub tasks: Mutex<Vec<JoinHandle<()>>>,
    pub stopped: AtomicBool,
    /// How many of the browsers' own events have been acted on, for a test to wait for one it caused.
    pub browser_handled: tokio::sync::watch::Sender<u64>,
    /// How many of the manager's events have been mirrored, for a test to wait for one it caused.
    pub mirrored: tokio::sync::watch::Sender<u64>,
}

/// The server's browser. Cloning shares it.
#[derive(Clone)]
pub struct ServerBrowser(pub(crate) Arc<Core>);

impl Core {
    pub(crate) fn tabs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Tab>>> {
        self.tabs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    pub(crate) fn tab_list(&self) -> Vec<Arc<Tab>> {
        self.tabs().values().cloned().collect()
    }
    pub(crate) fn tab(&self, thread_id: &str, tab_id: &str) -> Option<Arc<Tab>> {
        self.tabs().get(&tab_key(thread_id, tab_id)).cloned()
    }
    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn dialog_status(tab: &Tab) -> Value {
        match &tab.state().dialog {
            Some(dialog) => json!({"type": dialog.kind, "message": truncate(&dialog.message, 4000), "defaultValue": truncate(&dialog.default_value, 4000)}),
            None => Value::Null,
        }
    }

    pub(crate) fn broadcast_control(&self, tab: &Tab) {
        let dialog = Self::dialog_status(tab);
        let controller = tab.control.controller();
        for viewer in tab.viewers() {
            let who = if controller.as_deref() == Some(viewer.id.as_str()) {
                "you"
            } else if controller.is_some() {
                "another-viewer"
            } else if tab.control.agent_id().is_some() {
                "agent"
            } else {
                "unclaimed"
            };
            viewer.push(crate::server_browser_viewer::ViewerOutput::Control { can_operate: viewer.can_operate, controller: who, generation: tab.control.generation(), dialog: (!dialog.is_null()).then(|| dialog.clone()) }, tab);
        }
    }

    pub(crate) async fn resolve_dialog(&self, tab: &Tab, input: &PreviewAutomationDialogInput) -> OpResult<()> {
        if tab.state().dialog.is_none() {
            return Err(OpError::execution("No dialog is pending."));
        }
        tab.page.handle_dialog(input.accept.unwrap_or(false), input.prompt_text.as_deref()).await?;
        tab.state().dialog = None;
        tab.page.invalidate_refs();
        self.broadcast_control(tab);
        Ok(())
    }

    /// Tells the manager where the page is, for the tab's status and back and forward buttons.
    fn report(self: &Arc<Self>, tab: &Arc<Tab>, nav_status: Value) {
        let (core, tab) = (self.clone(), tab.clone());
        tokio::spawn(async move {
            let (index, count) = tab.page.history_position().await.unwrap_or((0, 0));
            let report = json!({"threadId": tab.thread_id, "tabId": tab.tab_id, "navStatus": nav_status, "canGoBack": index > 0, "canGoForward": (index as usize) + 1 < count});
            if let Ok(input) = serde_json::from_value(report) {
                let _ = core.manager.report_status(input, true);
            }
        });
    }

    async fn report_loaded(self: &Arc<Self>, tab: &Arc<Tab>) {
        let url = tab.page.url();
        // Chromium's error page loads after the request failed and must not clear the failure.
        if url == "about:blank" || url.starts_with("chrome-error://") {
            return;
        }
        let title: String = tab.page.title().await.chars().take(512).collect();
        // A navigation that started while reading the title owns the status now.
        if tab.loading.load(Ordering::SeqCst) || tab.page.url() != url {
            return;
        }
        self.report(tab, json!({"_tag": "Success", "url": truncate(&url, 2048), "title": title}));
    }

    pub(crate) fn report_live_tabs(self: &Arc<Self>) {
        let Some(connection_id) = Self::lock(&self.host_connection).clone() else { return };
        let tabs: Vec<Value> = self.tab_list().iter().map(|tab| json!({"threadId": tab.thread_id, "tabId": tab.tab_id, "visible": !tab.viewers().is_empty()})).collect();
        let focus = json!({"clientId": SERVER_BROWSER_AUTOMATION_CLIENT_ID, "environmentId": self.environment_id, "connectionId": connection_id, "focused": true, "liveTabs": tabs});
        if let Ok(focus) = serde_json::from_value(focus) {
            self.broker.focus_host(focus);
        }
    }

    /// The CSS size the page lays out in: the viewport shrunk by the tab's zoom, as Chrome zooms.
    pub(crate) fn layout_size(tab: &Tab) -> Option<(u32, u32)> {
        let (width, height) = tab.page.viewport()?;
        let zoom = tab.state().zoom_factor;
        Some(((f64::from(width) / zoom).round().max(1.0) as u32, (f64::from(height) / zoom).round().max(1.0) as u32))
    }

    /// Zooms a headless page as Chrome's zoom does: it lays out in fewer CSS pixels and draws each one larger, so frames keep their size.
    async fn apply_zoom(&self, tab: &Tab) -> OpResult<()> {
        let Some((width, height)) = tab.page.viewport() else { return Ok(()) };
        let zoom = tab.state().zoom_factor;
        if zoom == 1.0 {
            // The plain viewport.
            return tab.page.set_viewport(width, height).await;
        }
        let (layout_width, layout_height) = Self::layout_size(tab).unwrap_or((width, height));
        tab.page.set_layout(layout_width, layout_height, RENDER_SCALE * zoom).await
    }

    pub(crate) fn broadcast_viewport(&self, tab: &Tab) {
        let Some((width, height)) = Self::layout_size(tab) else { return };
        for viewer in tab.viewers() {
            viewer.push(crate::server_browser_viewer::ViewerOutput::Viewport { width, height }, tab);
        }
    }

    pub(crate) async fn apply_setting(&self, tab: &Tab, setting: Value) -> OpResult<()> {
        let size = fixed_size(&setting).or_else(|| {
            let mut requested: Vec<(u32, u32, u64)> = tab.viewers().iter().filter_map(|viewer| viewer.requested_size()).collect();
            requested.sort_by_key(|(_, _, order)| std::cmp::Reverse(*order));
            requested.first().map(|(width, height, _)| (*width, *height))
        });
        tab.state().setting = setting;
        let (width, height) = size.unwrap_or(UNATTACHED_FILL_VIEWPORT);
        tab.page.set_viewport(width, height).await?;
        self.apply_zoom(tab).await?;
        self.broadcast_viewport(tab);
        Ok(())
    }

    /// Applies a tab's published appearance and zoom.
    pub(crate) async fn apply_rendering(&self, tab: &Tab, snapshot: &Value) -> OpResult<()> {
        let color_scheme = snapshot["colorScheme"].as_str().unwrap_or("system").to_owned();
        if color_scheme != tab.state().color_scheme {
            tab.state().color_scheme = color_scheme.clone();
            tab.page.emulate_color_scheme((color_scheme != "system").then_some(color_scheme.as_str())).await?;
        }
        let zoom = snapshot["zoomFactor"].as_f64().unwrap_or(1.0);
        if (zoom - tab.state().zoom_factor).abs() > f64::EPSILON {
            tab.state().zoom_factor = zoom;
            self.apply_zoom(tab).await?;
            self.broadcast_viewport(tab);
        }
        Ok(())
    }

    pub(crate) fn drop_tab(self: &Arc<Self>, tab: &Arc<Tab>, close_session: bool) {
        let key = tab.key();
        {
            let mut tabs = self.tabs();
            if !tabs.get(&key).is_some_and(|existing| Arc::ptr_eq(existing, tab)) {
                return;
            }
            tabs.remove(&key);
        }
        tab.closing.store(true, Ordering::SeqCst);
        // The viewer reconnects only when the page outlives the connection; here the page always goes with its tab.
        for viewer in tab.viewers() {
            viewer.push(crate::server_browser_viewer::ViewerOutput::Gone, tab);
        }
        let (downloads, ending) = (self.download_dir(tab), tab.clone());
        let ended = tokio::spawn(async move {
            ending.control.close().await;
            ending.page.close().await;
            if ending.isolated_context {
                ending.context.close().await;
            }
            let recording = ending.recording.lock().await.take();
            if let Some(recording) = recording {
                recording.encoder.close().await;
            }
            let _ = tokio::fs::remove_dir_all(downloads).await;
        });
        *Self::lock(&tab.ended) = Some(ended);
        self.report_live_tabs();
        if close_session {
            if let (Ok(thread_id), Ok(tab_id)) = (serde_json::from_value(json!(tab.thread_id)), serde_json::from_value(json!(tab.tab_id))) {
                self.manager.close(t3_contracts::PreviewCloseInput { thread_id, tab_id: Some(Some(tab_id)) });
            }
        }
    }

    fn download_dir(&self, tab: &Tab) -> PathBuf {
        // Tab ids are opaque server strings; hashing keeps them out of the path.
        let digest = sha2::Sha256::digest(tab_key(&tab.thread_id, &tab.tab_id).as_bytes());
        self.downloads_dir.join(digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
    }

    // ---- Tabs

    fn at_tab_limit(&self, agent_session_id: &str) -> bool {
        let tabs = self.tab_list();
        tabs.len() + Self::lock(&self.pending).len() >= SERVER_TAB_LIMIT || tabs.iter().filter(|tab| tab.control.agent_id() == Some(agent_session_id)).count() >= AGENT_TAB_LIMIT
    }

    fn assert_tab_capacity(&self, agent_session_id: &str) -> OpResult<()> {
        if self.at_tab_limit(agent_session_id) {
            return Err(BrowserControlInterrupted::new(
                format!("Too many server browser tabs are open ({AGENT_TAB_LIMIT} per agent session, {SERVER_TAB_LIMIT} per server). Close one with t3_preview_close or reuse one from preview_status tabs."),
                t3_contracts::PreviewAutomationControlReason::TabLimit,
            )
            .into());
        }
        Ok(())
    }

    /// Closes agent tabs that no one watches and the agent stopped using.
    pub(crate) fn close_idle_agent_tabs(self: &Arc<Self>) {
        let Some(cutoff) = Instant::now().checked_sub(AGENT_TAB_IDLE) else { return };
        for tab in self.tab_list() {
            if tab.control.agent_id().is_some() && tab.viewers().is_empty() && tab.state().used_at < cutoff {
                self.drop_tab(&tab, true);
            }
        }
    }

    fn latest_thread_tab(&self, thread_id: &str, agent_session_id: Option<&str>) -> Option<Arc<Tab>> {
        self.tab_list().into_iter().filter(|tab| tab.thread_id == thread_id && tab.control.agent_id() == agent_session_id).max_by_key(|tab| tab.created_at)
    }

    /// Creates the tab for a published session: a page in the right context, and the task that follows its events.
    async fn create_tab(self: &Arc<Self>, snapshot: &Value) -> Result<Arc<Tab>, ServerBrowserError> {
        let thread_id = snapshot["threadId"].as_str().unwrap_or_default().to_owned();
        let tab_id = snapshot["tabId"].as_str().unwrap_or_default().to_owned();
        let key = tab_key(&thread_id, &tab_id);
        let adopted = Self::lock(&self.adopted).remove(&key);
        let automation_owner = snapshot["automationOwner"].as_str().map(str::to_owned);
        let profile_id = snapshot["profileId"].as_str().map(str::to_owned);
        let isolated_context = adopted.is_none() && (automation_owner.is_some() || profile_id.as_deref() == Some(INCOGNITO_BROWSER_PROFILE_ID));
        let (context, page, events, opener_tab_id) = match adopted {
            Some(adopted) => (adopted.context, adopted.page, adopted.events, Some(adopted.opener_tab_id)),
            None => {
                let context = self.contexts.context_for(profile_id.as_deref().unwrap_or("default"), isolated_context.then_some(key.as_str())).await.map_err(failure_error)?;
                let (page, events) = context.new_page(true).await.map_err(launch_error)?;
                (context, page, events, None)
            }
        };
        let setting = snapshot.get("viewport").filter(|setting| !setting.is_null()).cloned().unwrap_or_else(fill_setting);
        let control = {
            let page = page.clone();
            SessionControl::new(automation_owner.clone(), move || page.invalidate_refs())
        };
        let tab = Arc::new(Tab {
            thread_id,
            tab_id,
            page: page.clone(),
            context: context.clone(),
            created_at: Instant::now(),
            state: Mutex::new(TabState {
                used_at: Instant::now(),
                console: Vec::new(),
                network: Vec::new(),
                timeline: Vec::new(),
                downloads: Vec::new(),
                file_chooser: None,
                dialog: None,
                setting: setting.clone(),
                color_scheme: "system".into(),
                zoom_factor: 1.0,
            }),
            viewers: Mutex::default(),
            control,
            isolated_context,
            profile_id,
            opener_tab_id,
            loading: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            recording: AsyncMutex::new(None),
            capture_lock: AsyncMutex::new(()),
            capturing: AtomicUsize::new(0),
            initial_navigation: Mutex::new(None),
            handled: tokio::sync::watch::channel(0).0,
            ended: Mutex::new(None),
        });
        let (width, height) = fixed_size(&setting).unwrap_or(UNATTACHED_FILL_VIEWPORT);
        let setup = async {
            page.set_viewport(width, height).await?;
            self.apply_rendering(&tab, snapshot).await
        };
        if let Err(error) = setup.await {
            tab.control.close().await;
            page.close().await;
            if isolated_context {
                context.close().await;
            }
            return Err(launch_error(error));
        }
        // A session closed while its tab was opening leaves nothing behind.
        if Self::lock(&self.closed_pending).remove(&key) {
            tab.control.close().await;
            page.close().await;
            if isolated_context {
                context.close().await;
            }
            return Err(ServerBrowserError::TabNotFound { thread_id: tab.thread_id.clone(), tab_id: tab.tab_id.clone() });
        }
        self.tabs().insert(key, tab.clone());
        self.spawn_tab_events(&tab, events);
        self.report_live_tabs();
        // A popup is already loading its own URL.
        if tab.opener_tab_id.is_none() && snapshot["navStatus"]["_tag"] == "Loading" {
            let url = snapshot["navStatus"]["url"].as_str().unwrap_or_default().to_owned();
            let navigating = tab.page.clone();
            // Background creation keeps the failed tab; automation awaits the original error.
            let handle = tokio::spawn(async move { navigating.goto(&url, Readiness::Commit, NAVIGATION_TIMEOUT).await });
            let shared: Shared<BoxFuture<'static, OpResult<()>>> = (async move { handle.await.unwrap_or_else(|error| Err(OpError::execution(error.to_string()))) }).boxed().shared();
            *Self::lock(&tab.initial_navigation) = Some(shared);
        }
        Ok(tab)
    }

    /// Follows a page's events: they become the tab's status, diagnostics, dialogs, file picker and copies.
    fn spawn_tab_events(self: &Arc<Self>, tab: &Arc<Tab>, mut events: mpsc::UnboundedReceiver<PageEvent>) {
        let (core, tab) = (Arc::downgrade(self), tab.clone());
        let task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(core) = core.upgrade() else { return };
                core.tab_event(&tab, event).await;
                tab.handled.send_modify(|count| *count += 1);
            }
        });
        Self::lock(&self.tasks).push(task);
    }

    async fn tab_event(self: &Arc<Self>, tab: &Arc<Tab>, event: PageEvent) {
        match event {
            PageEvent::Request { url, main } => {
                if main {
                    tab.loading.store(true, Ordering::SeqCst);
                    self.report(tab, json!({"_tag": "Loading", "url": truncate(&url, 2048), "title": ""}));
                }
            }
            PageEvent::Load => {
                tab.loading.store(false, Ordering::SeqCst);
                self.report_loaded(tab).await;
            }
            PageEvent::Navigated => {
                // Same-document navigations (SPA routes) fire no load event.
                if !tab.loading.load(Ordering::SeqCst) {
                    self.report_loaded(tab).await;
                }
            }
            PageEvent::RequestFailed { url, method, error_text, main } => {
                push_bounded(&mut tab.state().network, json!({"url": url, "method": method, "status": null, "failed": true, "errorText": error_text, "timestamp": now_iso()}), DIAGNOSTIC_BUFFER_LIMIT);
                if !main || error_text.contains("ERR_ABORTED") {
                    return;
                }
                tab.loading.store(false, Ordering::SeqCst);
                let (description, code) = parse_net_error(&error_text);
                self.report(tab, json!({"_tag": "LoadFailed", "url": truncate(&url, 2048), "title": "", "code": code, "description": description}));
            }
            PageEvent::Response { url, method, status } => {
                push_bounded(&mut tab.state().network, json!({"url": url, "method": method, "status": status, "failed": false, "timestamp": now_iso()}), DIAGNOSTIC_BUFFER_LIMIT);
            }
            PageEvent::Console { level, text } => {
                push_bounded(&mut tab.state().console, json!({"level": level, "text": truncate(&text, 2000), "timestamp": now_iso()}), DIAGNOSTIC_BUFFER_LIMIT);
            }
            PageEvent::Dialog { kind, message, default_value } => {
                tab.state().dialog = Some(OpenDialog { kind, message, default_value });
                self.broadcast_control(tab);
            }
            PageEvent::DialogClosed => {
                if tab.state().dialog.take().is_some() {
                    self.broadcast_control(tab);
                }
            }
            PageEvent::FileChooser { multiple, backend_node_id } => self.offer_file_chooser(tab, multiple, backend_node_id).await,
            PageEvent::Clipboard(text) => {
                let Some(controller) = tab.controller() else { return };
                if controller.input_age().is_none_or(|age| age > crate::server_browser_viewer::CLIPBOARD_GESTURE) {
                    return;
                }
                controller.push(crate::server_browser_viewer::ViewerOutput::Clipboard { text: text.chars().take(crate::server_browser_page::CLIPBOARD_TEXT_LIMIT).collect() }, tab);
            }
            // Playwright cannot reload a crashed page, so it must leave the tab list.
            PageEvent::Crashed | PageEvent::Closed => self.drop_tab(tab, true),
        }
    }

    /// The tab for a published session, made if it is not there. Callers that ask while it is being made share the one opening.
    pub(crate) fn ensure_tab(self: &Arc<Self>, snapshot: Value) -> Opening {
        let key = tab_key(snapshot["threadId"].as_str().unwrap_or_default(), snapshot["tabId"].as_str().unwrap_or_default());
        // Checked and recorded under one lock, so two callers asking at once share one opening.
        let mut pending = Self::lock(&self.pending);
        if let Some(pending) = pending.get(&key) {
            return pending.clone();
        }
        if let Some(existing) = self.tabs().get(&key).cloned() {
            return async move { Ok(existing) }.boxed().shared();
        }
        let core = self.clone();
        let opening_key = key.clone();
        let handle = tokio::spawn(async move {
            let made = core.create_tab(&snapshot).await;
            if let Err(error) = &made {
                match error {
                    ServerBrowserError::Launch(LaunchCause::Browser(LaunchFailure::Host(host))) => tracing::warn!(%host, "server preview tab failed to start"),
                    other => tracing::warn!(error = %other, "server preview tab failed to start"),
                }
            }
            Self::lock(&core.pending).remove(&opening_key);
            Self::lock(&core.closed_pending).remove(&opening_key);
            made
        });
        let opening: Opening = (async move { handle.await.unwrap_or_else(|error| Err(launch_error(OpError::execution(error.to_string())))) }).boxed().shared();
        pending.insert(key, opening.clone());
        opening
    }

    pub(crate) async fn find_tab(self: &Arc<Self>, thread_id: &str, tab_id: &str) -> Result<Arc<Tab>, ServerBrowserError> {
        if let Some(existing) = self.tab(thread_id, tab_id) {
            return Ok(existing);
        }
        let listed = self.manager.list(serde_json::from_value(json!({"threadId": thread_id})).map_err(|_| ServerBrowserError::TabNotFound { thread_id: thread_id.into(), tab_id: tab_id.into() })?);
        let sessions = serde_json::to_value(&listed.sessions).unwrap_or_default();
        let snapshot = sessions.as_array().and_then(|sessions| sessions.iter().find(|session| session["tabId"] == tab_id && session["runtime"] == "server")).cloned();
        let Some(snapshot) = snapshot else { return Err(ServerBrowserError::TabNotFound { thread_id: thread_id.into(), tab_id: tab_id.into() }) };
        self.ensure_tab(snapshot).await
    }

    // ---- Popups, downloads and file pickers

    /// A page a tab opened becomes a tab of its own and keeps `window.opener`, so sign-in popups can report back.
    ///
    /// TODO(original: a popup's URL is not normalized). Only whether it is `http(s)` decides whether the session records it; the popup page
    /// itself is wherever the opening page sent it, as the browser allows.
    pub(crate) async fn adopt_popup(self: &Arc<Self>, opener: &Arc<Tab>, target_id: &str, url: &str) {
        let agent_id = opener.control.agent_id().map(str::to_owned);
        let over_limit = opener.closing.load(Ordering::SeqCst) || agent_id.as_deref().is_some_and(|agent| self.at_tab_limit(agent));
        let attached = if over_limit { None } else { Page::attach(opener.context.browser().connection(), target_id, opener.context.context_id(), true).await.ok() };
        // A popup past an agent's limit closes; its page sees window.open return a closed window.
        let Some((page, events)) = attached else {
            let _ = opener.context.browser().connection().send("Target.closeTarget", json!({"targetId": target_id}), None).await;
            return;
        };
        let mut open = json!({"threadId": opener.thread_id, "runtime": "server"});
        if url.get(..5).is_some_and(|scheme| scheme.eq_ignore_ascii_case("http:")) || url.get(..6).is_some_and(|scheme| scheme.eq_ignore_ascii_case("https:")) {
            open["url"] = json!(url);
        }
        if let Some(profile) = &opener.profile_id {
            open["profileId"] = json!(profile);
        }
        // Agent popups stay with the agent and only float when it asks, like its own opens.
        if agent_id.is_some() {
            open["reveal"] = json!(false);
        }
        let Ok(input) = serde_json::from_value(open) else { return };
        let (context, opener_tab_id) = (opener.context.clone(), opener.tab_id.clone());
        let held = Mutex::new(Some((page.clone(), events)));
        let core = self.clone();
        let result = self.manager.open_with(input, agent_id.clone(), |snapshot| {
            if let Some((page, events)) = Self::lock(&held).take() {
                Self::lock(&core.adopted).insert(tab_key(snapshot.thread_id.as_str(), snapshot.tab_id.as_str()), Adopted { page, events, context: context.clone(), opener_tab_id: opener_tab_id.clone() });
            }
        });
        if result.is_err() {
            page.close().await;
        }
    }

    /// A browser-level download that began: remember whose it is.
    pub(crate) fn download_began(&self, params: &Value) {
        let Some(frame_id) = params["frameId"].as_str() else { return };
        let Some(tab) = self.tab_list().into_iter().find(|tab| tab.page.owns_frame(frame_id)) else { return };
        let (guid, url, name) = (params["guid"].as_str().unwrap_or_default(), params["url"].as_str().unwrap_or_default(), params["suggestedFilename"].as_str().unwrap_or_default());
        Self::lock(&self.pending_downloads).insert(guid.to_owned(), (tab.key(), url.to_owned(), name.to_owned()));
    }

    pub(crate) async fn download_progress(self: &Arc<Self>, params: &Value) {
        let guid = params["guid"].as_str().unwrap_or_default().to_owned();
        let state = params["state"].as_str().unwrap_or_default();
        if state == "inProgress" {
            return;
        }
        let Some((key, url, name)) = Self::lock(&self.pending_downloads).remove(&guid) else { return };
        let incoming = self.downloads_dir.join("incoming").join(&guid);
        let Some(tab) = self.tabs().get(&key).cloned() else {
            let _ = tokio::fs::remove_file(&incoming).await;
            return;
        };
        if state != "completed" {
            let _ = tokio::fs::remove_file(&incoming).await;
            return;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let directory = self.download_dir(&tab);
        let path = directory.join(&id);
        let saved = async {
            tokio::fs::create_dir_all(&directory).await?;
            tokio::fs::rename(&incoming, &path).await?;
            tokio::fs::metadata(&path).await
        };
        match saved.await {
            Ok(metadata) if !tab.closing.load(Ordering::SeqCst) && metadata.len() <= DOWNLOAD_MAX_BYTES => {
                let file_name: String = if name.is_empty() { "download".into() } else { name.chars().take(255).collect() };
                let download = ServerDownload { id: id.clone(), file_name: file_name.clone(), path, size_bytes: metadata.len(), url: truncate(&url, 2048), completed_at: now_iso() };
                let evicted = {
                    let mut state = tab.state();
                    state.downloads.push(download);
                    let excess = state.downloads.len().saturating_sub(DOWNLOAD_LIMIT);
                    state.downloads.drain(..excess).collect::<Vec<_>>()
                };
                for evicted in evicted {
                    let _ = tokio::fs::remove_file(&evicted.path).await;
                }
                // Only the person driving the page gets the file offered; agents read it from status.
                if let Some(controller) = tab.controller() {
                    controller.push(crate::server_browser_viewer::ViewerOutput::Download { id, file_name, size_bytes: metadata.len() }, &tab);
                }
            }
            Ok(_) => {
                let _ = tokio::fs::remove_file(&path).await;
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&incoming).await;
                let _ = tokio::fs::remove_file(&path).await;
                tracing::warn!(%error, "server preview download failed");
            }
        }
    }

    pub(crate) fn file_chooser_message(tab: &Tab) -> Option<crate::server_browser_viewer::ViewerOutput> {
        tab.state().file_chooser.as_ref().map(|chooser| crate::server_browser_viewer::ViewerOutput::FileChooser { id: chooser.id.clone(), multiple: chooser.multiple, accept: chooser.accept.clone() })
    }

    pub(crate) fn push_file_chooser(&self, tab: &Tab) {
        if let (Some(message), Some(controller)) = (Self::file_chooser_message(tab), tab.controller()) {
            controller.push(message, tab);
        }
    }

    pub(crate) fn close_file_chooser(&self, tab: &Tab) {
        let Some(open) = tab.state().file_chooser.take() else { return };
        for viewer in tab.viewers() {
            viewer.push(crate::server_browser_viewer::ViewerOutput::FileChooserClosed { id: open.id.clone() }, tab);
        }
    }

    async fn offer_file_chooser(&self, tab: &Arc<Tab>, multiple: bool, backend_node_id: i64) {
        let accept: String = tab.page.chooser_accept(backend_node_id).await.chars().take(1024).collect();
        // A newer picker replaces an unanswered one, as a real browser allows only one.
        if tab.state().file_chooser.is_some() {
            self.close_file_chooser(tab);
        }
        tab.state().file_chooser = Some(OpenChooser { id: uuid::Uuid::new_v4().to_string(), backend_node_id, multiple, accept });
        self.push_file_chooser(tab);
    }

    /// The agent's files go to a named file input, or else to the page's open picker.
    async fn upload_files(&self, tab: &Tab, input: &PreviewAutomationUploadInput) -> OpResult<()> {
        let paths = input.paths.clone().unwrap_or_default();
        if let Some(relative) = paths.iter().find(|path| !Path::new(path).is_absolute()) {
            return Err(OpError::execution(format!("Upload paths must be absolute: {relative}")));
        }
        if tab.page.set_input_files(input.target(), &paths, input.timeout_ms).await? {
            return Ok(());
        }
        let Some(open) = tab.state().file_chooser.clone() else {
            return Err(OpError::execution("No file picker is open. Click the page's upload control first, or pass the file input's locator."));
        };
        if paths.len() > 1 && !open.multiple {
            return Err(OpError::execution("This file picker accepts one file."));
        }
        if !paths.is_empty() {
            tab.page.set_chooser_files(open.backend_node_id, &paths).await?;
        }
        if tab.state().file_chooser.as_ref().is_some_and(|current| current.id == open.id) {
            self.close_file_chooser(tab);
        }
        Ok(())
    }

    // ---- Status and navigation

    pub(crate) async fn status_with_title(&self, tab: Option<&Arc<Tab>>, agent_session_id: Option<&str>) -> Value {
        let Some(tab) = tab else {
            return json!({"available": false, "visible": false, "tabId": null, "url": null, "title": null, "loading": false});
        };
        let url = tab.page.url();
        let tabs: Vec<Value> = self
            .tab_list()
            .iter()
            .filter(|candidate| candidate.thread_id == tab.thread_id && candidate.control.agent_id() == agent_session_id)
            .map(|candidate| {
                let url = candidate.page.url();
                let mut entry = json!({"tabId": candidate.tab_id, "url": if url == "about:blank" { Value::Null } else { json!(url) }});
                if let Some(opener) = &candidate.opener_tab_id {
                    entry["openerTabId"] = json!(opener);
                }
                entry
            })
            .collect();
        let controller = tab.control.controller();
        let (setting, file_chooser, downloads) = {
            let state = tab.state();
            (
                state.setting.clone(),
                state.file_chooser.as_ref().map(|chooser| json!({"multiple": chooser.multiple, "accept": chooser.accept})),
                state.downloads.iter().map(|download| json!({"fileName": download.file_name, "path": download.path, "sizeBytes": download.size_bytes, "url": download.url, "completedAt": download.completed_at})).collect::<Vec<_>>(),
            )
        };
        let dialog = Self::dialog_status(tab);
        let mut status = json!({
            "available": true,
            "visible": !tab.viewers().is_empty(),
            "tabId": tab.tab_id,
            "url": if url == "about:blank" { Value::Null } else { json!(url) },
            "title": null,
            "loading": tab.loading.load(Ordering::SeqCst),
            "control": {
                "owner": if controller.is_some() { "human" } else if tab.control.agent_id().is_some() { "agent" } else { "unclaimed" },
                "ownedByCaller": tab.control.agent_id() == agent_session_id,
                "generation": tab.control.generation(),
            },
            "dialog": dialog,
            "fileChooser": file_chooser,
            "viewportSetting": setting,
            "tabs": tabs,
            "downloads": downloads,
        });
        if let Some((width, height)) = tab.page.viewport() {
            status["viewport"] = json!({"width": width, "height": height});
        }
        if status["url"].is_null() || !dialog.is_null() {
            return status;
        }
        let title = tab.page.title().await;
        status["title"] = if title.is_empty() { Value::Null } else { json!(title) };
        status
    }

    fn resolve_navigation_url(input: &PreviewAutomationNavigateInput) -> OpResult<String> {
        let normalize = |raw: &str| normalize_preview_url(raw).map_err(|error| OpError::execution(error.message()));
        // TODO(original: a URL is checked for its scheme only). `http` and `https` to any host, including loopback, private-network and
        // link-local addresses, are accepted as given.
        if let Some(url) = &input.url {
            return normalize(url);
        }
        match &input.target {
            Some(t3_contracts::BrowserNavigationTarget::Url { url }) => normalize(url),
            // The browser runs inside the environment, so its ports are loopback.
            Some(t3_contracts::BrowserNavigationTarget::EnvironmentPort { port, protocol, path }) => {
                let path = path.as_deref().unwrap_or_default();
                let path = if path.starts_with('/') || path.is_empty() { path.to_owned() } else { format!("/{path}") };
                Ok(format!("{}://localhost:{port}{path}", protocol.map_or("http", |protocol| protocol.as_str())))
            }
            None => Err(OpError::execution("Provide exactly one of url or target.")),
        }
    }

    async fn navigate(&self, tab: &Arc<Tab>, url: String, readiness: &str, timeout: Duration) -> OpResult<()> {
        let page = tab.page.clone();
        if readiness == "none" {
            // The action answers before the navigation commits, while later actions still wait for it.
            tab.control.track(async move {
                let _ = page.goto(&url, Readiness::Commit, timeout).await;
            });
            return Ok(());
        }
        let wait = if readiness == "domContentLoaded" { Readiness::DomContentLoaded } else { Readiness::Load };
        page.goto(&url, wait, timeout).await.map_err(|error| match ERR_PATTERN.find(&error.message) {
            Some(found) => OpError::execution(format!("Navigation to {url} failed: {}", parse_net_error(found.as_str()).0)),
            None => error,
        })
    }

    /// Serialises captures and recording starts and stops, and refuses once the tab is closing.
    pub(crate) async fn with_capture_lock<A>(&self, tab: &Tab, operation: impl std::future::Future<Output = OpResult<A>>) -> OpResult<A> {
        let _held = tab.capture_lock.lock().await;
        if tab.closing.load(Ordering::SeqCst) {
            return Err(OpError::execution("The preview tab closed."));
        }
        operation.await
    }

    /// Scaled captures repaint every screencast; pause them to avoid leaking that frame.
    async fn with_screencasts_paused<A>(&self, tab: &Tab, capture: impl std::future::Future<Output = OpResult<A>>) -> OpResult<A> {
        let _held = tab.capture_lock.lock().await;
        if tab.closing.load(Ordering::SeqCst) {
            return Err(OpError::execution("The preview tab closed."));
        }
        tab.capturing.fetch_add(1, Ordering::SeqCst);
        let viewers = tab.viewers();
        futures_util::future::join_all(viewers.iter().map(|viewer| viewer.pause())).await;
        {
            let recording = tab.recording.lock().await;
            if let Some(recording) = recording.as_ref() {
                let _ = recording.session.send("Page.stopScreencast", json!({})).await;
            }
        }
        let result = capture.await;
        tab.capturing.fetch_sub(1, Ordering::SeqCst);
        // Viewers that attached during the capture start here too.
        futures_util::future::join_all(tab.viewers().iter().map(|viewer| viewer.resume(tab))).await;
        {
            let recording = tab.recording.lock().await;
            if let Some(recording) = recording.as_ref() {
                let _ = recording.session.send("Page.startScreencast", recording_screencast()).await;
            }
        }
        result
    }

    async fn start_recording(&self, tab: &Arc<Tab>) -> OpResult<String> {
        let _held = tab.capture_lock.lock().await;
        if tab.closing.load(Ordering::SeqCst) {
            return Err(OpError::execution("The preview tab closed."));
        }
        let mut slot = tab.recording.lock().await;
        if let Some(recording) = slot.as_ref() {
            return Ok(recording.started_at.clone());
        }
        let scratch = self.contexts.scratch_page().await.map_err(OpError::from)?;
        match self.begin_recording(tab, &scratch).await {
            Ok((session, in_flight)) => {
                let started_at = now_iso();
                *slot = Some(Recording { encoder: scratch, session, started_at: started_at.clone(), in_flight });
                Ok(started_at)
            }
            Err(error) => {
                // A start that fails partway must not leave its encoder page behind.
                scratch.close().await;
                Err(error)
            }
        }
    }

    /// Starts the screencast that feeds a throwaway page which encodes the video in the browser.
    async fn begin_recording(&self, tab: &Arc<Tab>, scratch: &crate::server_browser_contexts::ScratchPage) -> OpResult<(PageSession, Arc<tokio::sync::watch::Sender<usize>>)> {
        scratch.page.evaluate_value(crate::server_browser_page::recording_encoder_script(), false).await?;
        // Seed idle pages before returning so an immediate stop has a frame to encode.
        let first = tab.page.capture_viewport(None, "jpeg", Some(RECORDING_QUALITY), 1.0).await?;
        let width = tab.page.viewport().map_or(UNATTACHED_FILL_VIEWPORT.0, |(width, _)| width);
        scratch.page.evaluate_value(format!("globalThis.__t3Recorder.frame({}, {width})", json!(first)), true).await?;
        let session = tab.page.new_session().await?;
        let in_flight = Arc::new(tokio::sync::watch::channel(0usize).0);
        let (encoder, page, poster, session_id, counted) = (scratch.page.clone(), tab.page.clone(), session.connection().poster(), session.id.clone(), in_flight.clone());
        session.connection().on_event(Some(&session.id), move |event| {
            if event.method != "Page.screencastFrame" {
                return;
            }
            let css_width = page.viewport().map_or(f64::from(UNATTACHED_FILL_VIEWPORT.0), |(width, _)| f64::from(width));
            let (data, ack_id) = (event.params["data"].as_str().unwrap_or_default().to_owned(), event.params["sessionId"].clone());
            counted.send_modify(|count| *count += 1);
            let (encoder, poster, session_id, counted) = (encoder.clone(), poster.clone(), session_id.clone(), counted.clone());
            tokio::spawn(async move {
                let accepted = encoder.evaluate_value(format!("globalThis.__t3Recorder.frame({}, {css_width})", json!(data)), true).await;
                if accepted.is_ok_and(|accepted| accepted == json!(false)) {
                    poster.post("Page.stopScreencast", json!({}), Some(&session_id));
                }
                poster.post("Page.screencastFrameAck", json!({"sessionId": ack_id}), Some(&session_id));
                counted.send_modify(|count| *count -= 1);
            });
        });
        session.send("Page.startScreencast", recording_screencast()).await.map_err(crate::server_browser_page::map_cdp)?;
        if tab.closing.load(Ordering::SeqCst) {
            session.detach();
            return Err(OpError::execution("The tab closed while the recording started."));
        }
        Ok((session, in_flight))
    }

    async fn stop_recording(&self, tab: &Arc<Tab>) -> OpResult<Value> {
        let _held = tab.capture_lock.lock().await;
        if tab.closing.load(Ordering::SeqCst) {
            return Err(OpError::execution("The preview tab closed."));
        }
        let Some(recording) = tab.recording.lock().await.take() else {
            return Err(OpError::new("PreviewAutomationRecordingNotActiveError", "No recording is active for this tab."));
        };
        let stopped = async {
            let _ = recording.session.send("Page.stopScreencast", json!({})).await;
            // The last frames may still be on their way into the encoder.
            let _ = recording.in_flight.subscribe().wait_for(|count| *count == 0).await;
            recording.session.detach();
            let stopped = recording.encoder.page.evaluate_value("globalThis.__t3Recorder.stop()".into(), true).await?;
            let mime = stopped["mimeType"].as_str().map(str::to_owned);
            // Checked before the transfer so an oversized video never lands in this process.
            if stopped["bytes"].as_u64().unwrap_or_default() > PROVIDER_SEND_TURN_MAX_FILE_BYTES {
                return Err(OpError::new("PreviewAutomationRecordingTooLargeError", "The recording is larger than the attachment limit."));
            }
            let mut data = Vec::new();
            for index in 0..stopped["count"].as_u64().unwrap_or_default() {
                let chunk = recording.encoder.page.evaluate_value(format!("globalThis.__t3Recorder.chunk({index})"), true).await?;
                data.extend(base64::Engine::decode(&base64::engine::general_purpose::STANDARD, chunk.as_str().unwrap_or_default()).map_err(|error| OpError::execution(error.to_string()))?);
            }
            Ok::<_, OpError>((mime, data))
        }
        .await;
        recording.encoder.close().await;
        let (mime, data) = stopped?;
        let Some(mime) = mime.filter(|_| !data.is_empty()) else { return Err(OpError::execution("The recording captured no frames.")) };
        // Use the desktop upload location so the MCP handler can claim the recording.
        let extension = if mime.starts_with("video/mp4") { "mp4" } else { "webm" };
        let pending_id = format!("{}-{}-{extension}", crate::attachment_store::PENDING_ATTACHMENT_THREAD_SEGMENT, uuid::Uuid::new_v4());
        let path = self.attachments_dir.join(format!("{pending_id}.{extension}"));
        tokio::fs::create_dir_all(&self.attachments_dir).await.map_err(|error| OpError::execution(error.to_string()))?;
        tokio::fs::write(&path, &data).await.map_err(|error| OpError::execution(error.to_string()))?;
        Ok(json!({
            "id": pending_id,
            "tabId": tab.tab_id,
            "path": path,
            "mimeType": mime.split(';').next().unwrap_or_default(),
            "sizeBytes": data.len(),
            "createdAt": now_iso(),
            "uploadedAttachmentId": pending_id,
        }))
    }

    /// Shows viewers and recordings where the agent is about to act. Nobody is watching a headless agent tab, so it pays no glide delay.
    fn pointer_for(self: &Arc<Self>, tab: &Arc<Tab>) -> PointerReporter {
        let (core, tab) = (self.clone(), tab.clone());
        Arc::new(move |point: Point, phase: PointerPhase| {
            let (core, tab) = (core.clone(), tab.clone());
            Box::pin(async move {
                let recording_page = tab.recording.lock().await.as_ref().map(|recording| recording.encoder.page.clone());
                if tab.viewers().is_empty() && recording_page.is_none() {
                    return;
                }
                let show = |next: &'static str| {
                    let sequence = core.pointer_sequence.fetch_add(1, Ordering::SeqCst) + 1;
                    for viewer in tab.viewers() {
                        viewer.push(crate::server_browser_viewer::ViewerOutput::Pointer { phase: next, x: point.x, y: point.y, sequence }, &tab);
                    }
                    if let Some(encoder) = recording_page.clone() {
                        let click = next == "click";
                        tokio::spawn(async move {
                            let _ = encoder.evaluate_value(format!("globalThis.__t3Recorder?.cursor({}, {}, {click})", point.x, point.y), false).await;
                        });
                    }
                };
                show("move");
                tokio::time::sleep(AGENT_CURSOR_MOVE).await;
                if phase != PointerPhase::Click {
                    return;
                }
                show("click");
                tokio::time::sleep(AGENT_CURSOR_CLICK_LEAD).await;
            })
        })
    }

    /// Runs an action and keeps its record in the tab's timeline.
    async fn record_action<A>(&self, tab: &Tab, action: &str, run: impl std::future::Future<Output = OpResult<A>>) -> OpResult<A> {
        let id = uuid::Uuid::new_v4().to_string();
        push_bounded(&mut tab.state().timeline, json!({"id": id, "action": action, "status": "running", "startedAt": now_iso()}), ACTION_TIMELINE_LIMIT);
        let result = run.await;
        let mut state = tab.state();
        if let Some(event) = state.timeline.iter_mut().find(|event| event["id"] == json!(id)) {
            event["completedAt"] = json!(now_iso());
            match &result {
                Ok(_) => event["status"] = json!("succeeded"),
                Err(error) => {
                    event["status"] = json!("failed");
                    event["error"] = json!(error.message.lines().next().unwrap_or_default());
                }
            }
        }
        result
    }

    // ---- Requests

    /// The agent's tab for a request.
    async fn require_tab(self: &Arc<Self>, request: &RequestView) -> OpResult<Arc<Tab>> {
        let Some(agent) = request.agent_session_id.as_deref() else {
            return Err(BrowserControlInterrupted::new("The agent session is missing. Reconnect the provider.", t3_contracts::PreviewAutomationControlReason::Interrupted).into());
        };
        let owned = self.tab_list().into_iter().filter(|tab| tab.thread_id == request.thread_id && tab.control.agent_id() == Some(agent)).count();
        if !request.tab_id_explicit && owned > 1 {
            return Err(BrowserControlInterrupted::new("Multiple tabs belong to this agent session. Pass a tabId from preview_open or preview_status tabs.", t3_contracts::PreviewAutomationControlReason::TabRequired).into());
        }
        let tab = match &request.tab_id {
            None => self.latest_thread_tab(&request.thread_id, Some(agent)),
            Some(tab_id) => self.find_tab(&request.thread_id, tab_id).await.ok(),
        };
        let Some(tab) = tab else { return Err(OpError::new("PreviewAutomationTabNotFoundError", "No server preview tab is open for this thread. Call preview_open first.")) };
        if tab.control.agent_id() != Some(agent) {
            return Err(BrowserControlInterrupted::new("This tab belongs to another agent session or a human. Open your own tab.", t3_contracts::PreviewAutomationControlReason::AgentMismatch).into());
        }
        Ok(tab)
    }

    /// Any request, even a failed one, keeps the agent's tabs on the thread from idling out.
    fn mark_used(&self, request: &RequestView) {
        let now = Instant::now();
        for tab in self.tab_list() {
            if tab.thread_id == request.thread_id && tab.control.agent_id() == request.agent_session_id.as_deref() {
                tab.state().used_at = now;
            }
        }
    }

    pub(crate) async fn run_operation(self: &Arc<Self>, request: &RequestView) -> OpResult<Value> {
        let dialog_pending = || OpError::from(BrowserControlInterrupted::new("A browser dialog is pending. Read preview_status and use preview_dialog first.", t3_contracts::PreviewAutomationControlReason::DialogPending));
        let agent_missing = || OpError::from(BrowserControlInterrupted::new("The agent session is missing. Reconnect the provider.", t3_contracts::PreviewAutomationControlReason::Interrupted));
        let agent = request.agent_session_id.as_deref();
        match request.operation {
            PreviewAutomationOperation::Status => {
                let tab = match &request.tab_id {
                    None => self.latest_thread_tab(&request.thread_id, agent),
                    Some(tab_id) => self.tab(&request.thread_id, tab_id),
                };
                return Ok(self.status_with_title(tab.as_ref(), agent).await);
            }
            PreviewAutomationOperation::Open => {
                let agent = agent.ok_or_else(agent_missing)?.to_owned();
                let open: PreviewAutomationOpenInput = decode(&request.input)?;
                // TODO(original: a URL is checked for its scheme only); see `resolve_navigation_url`.
                let url = open.url.as_deref().map(|raw| normalize_preview_url(raw).map_err(|error| OpError::execution(error.message()))).transpose()?;
                let reuse = open.reuse_existing_tab.unwrap_or(true);
                let owned_count = self.tab_list().iter().filter(|tab| tab.thread_id == request.thread_id && tab.control.agent_id() == Some(agent.as_str())).count();
                if reuse && !request.tab_id_explicit && owned_count > 1 {
                    return Err(BrowserControlInterrupted::new("Multiple tabs are open. Pass tabId or reuseExistingTab=false.", t3_contracts::PreviewAutomationControlReason::TabRequired).into());
                }
                // A tab still launching exists only as a session, so resolve it like a viewer would.
                let existing = match (&request.tab_id, reuse) {
                    (Some(tab_id), true) => self.find_tab(&request.thread_id, tab_id).await.ok(),
                    _ => None,
                };
                let navigation_timeout = Duration::from_millis(request.timeout_ms.min(NAVIGATION_TIMEOUT_MS));
                if existing.is_none() {
                    self.close_idle_agent_tabs();
                    self.assert_tab_capacity(&agent)?;
                }
                let tab = match &existing {
                    Some(existing) => existing.clone(),
                    None => {
                        let mut open_input = json!({"threadId": request.thread_id, "runtime": "server", "reveal": false});
                        if let Some(url) = &url {
                            open_input["url"] = json!(url);
                        }
                        let input = serde_json::from_value(open_input).map_err(|error| OpError::execution(error.to_string()))?;
                        let snapshot = self.manager.open_with(input, Some(agent.clone()), |_| {}).map_err(|error| OpError::execution(error.message()))?;
                        // `open_with` published the session; the mirror makes the tab, and this waits on the same opening.
                        let snapshot = serde_json::to_value(&snapshot).unwrap_or_default();
                        self.ensure_tab(snapshot).await.map_err(|error| match error {
                            ServerBrowserError::Launch(LaunchCause::Browser(failure)) => OpError::execution(failure.to_string()),
                            ServerBrowserError::Launch(LaunchCause::Operation(error)) => error,
                            other => OpError::execution(other.message()),
                        })?
                    }
                };
                if existing.is_some() && tab.state().dialog.is_some() {
                    return Err(dialog_pending());
                }
                let (core, running) = (self.clone(), tab.clone());
                let reveal = open.open.or(open.show);
                let session = agent.clone();
                return tab
                    .control
                    .agent(&agent, move || async move {
                        let (tab, agent) = (running, session);
                        if tab.state().dialog.is_some() {
                            return Err(dialog_pending());
                        }
                        if existing.is_some() {
                            if let Some(url) = url.clone() {
                                core.navigate(&tab, url, "load", navigation_timeout).await?;
                            }
                        } else {
                            // Await the original navigation failure even though background creation keeps the tab.
                            let initial = Self::lock(&tab.initial_navigation).clone();
                            if let Some(initial) = initial {
                                initial.await?;
                            }
                        }
                        if reveal != Some(false) {
                            let _ = core.manager.request_reveal(&tab.thread_id, &tab.tab_id, reveal == Some(true));
                        }
                        if existing.is_none() && url.is_some() {
                            let _ = tab.page.wait_for_load(navigation_timeout).await;
                        }
                        Ok(core.status_with_title(Some(&tab), Some(&agent)).await)
                    })
                    .await;
            }
            PreviewAutomationOperation::RecordingStop => {
                let owned = |tab: &Arc<Tab>| tab.thread_id == request.thread_id && tab.control.agent_id() == agent;
                if !request.tab_id_explicit && self.tab_list().iter().filter(|tab| owned(tab)).count() > 1 {
                    return Err(BrowserControlInterrupted::new("Multiple tabs belong to this agent session. Pass a tabId from preview_open or preview_status tabs.", t3_contracts::PreviewAutomationControlReason::TabRequired).into());
                }
                let mut recordings = Vec::new();
                for tab in self.tab_list().into_iter().filter(|tab| owned(tab)) {
                    if tab.recording.lock().await.is_some() {
                        recordings.push(tab);
                    }
                }
                let target = request.tab_id.clone().or_else(|| self.latest_thread_tab(&request.thread_id, agent).map(|tab| tab.tab_id.clone()));
                let tab = recordings
                    .iter()
                    .find(|candidate| Some(&candidate.tab_id) == target.as_ref())
                    .cloned()
                    .or_else(|| (!request.tab_id_explicit && recordings.len() == 1).then(|| recordings[0].clone()));
                let Some(tab) = tab else { return Err(OpError::new("PreviewAutomationRecordingNotActiveError", "No recording is active for this thread.")) };
                let (core, running) = (self.clone(), tab.clone());
                return tab.control.agent(agent.unwrap_or_default(), move || async move { core.stop_recording(&running).await }).await;
            }
            _ => {}
        }
        let tab = self.require_tab(request).await?;
        let agent = agent.unwrap_or_default().to_owned();
        // Closing must unblock an action waiting on a dialog, without queueing behind it.
        if request.operation == PreviewAutomationOperation::Close {
            if tab.control.controller().is_some() {
                return Err(BrowserControlInterrupted::new("A human controls this tab.", t3_contracts::PreviewAutomationControlReason::HumanControl).into());
            }
            let closing = tab.clone();
            tokio::spawn(async move {
                closing.control.close().await;
            });
            if let (Ok(thread_id), Ok(tab_id)) = (serde_json::from_value(json!(tab.thread_id)), serde_json::from_value(json!(tab.tab_id))) {
                self.manager.close(t3_contracts::PreviewCloseInput { thread_id, tab_id: Some(Some(tab_id)) });
            }
            self.drop_tab(&tab, false);
            return Ok(json!({}));
        }
        // A click can be waiting for its dialog. Resolve it outside the serial queue, with the same owner check, so the operation can finish
        // and control can drain.
        if request.operation == PreviewAutomationOperation::Dialog {
            if tab.control.controller().is_some() {
                return Err(BrowserControlInterrupted::new("A human controls this tab.", t3_contracts::PreviewAutomationControlReason::HumanControl).into());
            }
            self.resolve_dialog(&tab, &decode(&request.input)?).await?;
            return Ok(self.status_with_title(Some(&tab), Some(&agent)).await);
        }
        let (core, running, request) = (self.clone(), tab.clone(), request.clone());
        tab.control
            .agent(&agent.clone(), move || async move {
                if running.state().dialog.is_some() {
                    return Err(dialog_pending());
                }
                let generation = running.control.generation();
                let result = core.execute_tab_operation(&running, &request, &agent).await;
                if generation != running.control.generation() {
                    running.page.invalidate_refs();
                }
                result
            })
            .await
    }

    async fn execute_tab_operation(self: &Arc<Self>, tab: &Arc<Tab>, request: &RequestView, agent: &str) -> OpResult<Value> {
        let input = &request.input;
        match request.operation {
            PreviewAutomationOperation::Navigate => {
                let navigate: PreviewAutomationNavigateInput = decode(input)?;
                let url = Self::resolve_navigation_url(&navigate)?;
                let readiness = match navigate.readiness {
                    Some(t3_contracts::PreviewNavigationReadiness::DomContentLoaded) => "domContentLoaded",
                    Some(t3_contracts::PreviewNavigationReadiness::None) => "none",
                    _ => "load",
                };
                let timeout = Duration::from_millis(navigate.timeout_ms.unwrap_or(request.timeout_ms));
                self.record_action(tab, "navigate", self.navigate(tab, url, readiness, timeout)).await?;
                Ok(self.status_with_title(Some(tab), Some(agent)).await)
            }
            PreviewAutomationOperation::Resize => {
                let resize: PreviewAutomationResizeInput = decode(input)?;
                let setting = resolve_preview_viewport(&resize)?;
                let resized = self.manager.resize(serde_json::from_value(json!({"threadId": tab.thread_id, "tabId": tab.tab_id, "viewport": setting})).map_err(|error| OpError::execution(error.to_string()))?).map_err(|error| OpError::execution(error.message()))?;
                let _ = resized;
                self.apply_setting(tab, setting.clone()).await?;
                let (width, height) = tab.page.viewport().unwrap_or(UNATTACHED_FILL_VIEWPORT);
                Ok(json!({"tabId": tab.tab_id, "setting": setting, "viewport": {"width": width, "height": height}}))
            }
            PreviewAutomationOperation::SetColorScheme => {
                let scheme: PreviewAutomationSetColorSchemeInput = decode(input)?;
                let color_scheme = scheme.color_scheme.map_or("system", |scheme| scheme.as_str());
                let adjust = serde_json::from_value(json!({"threadId": tab.thread_id, "tabId": tab.tab_id, "colorScheme": color_scheme})).map_err(|error| OpError::execution(error.to_string()))?;
                let snapshot = self.manager.adjust(adjust).map_err(|error| OpError::execution(error.message()))?;
                self.apply_rendering(tab, &serde_json::to_value(&snapshot).unwrap_or_default()).await?;
                Ok(json!({"tabId": tab.tab_id, "colorScheme": color_scheme}))
            }
            PreviewAutomationOperation::Snapshot => {
                let diagnostics = || {
                    let state = tab.state();
                    SnapshotDiagnostics { console_entries: state.console.clone(), network_entries: state.network.clone(), action_timeline: state.timeline.clone() }
                };
                self.with_screencasts_paused(tab, async { tab.page.snapshot(diagnostics()).await }).await
            }
            PreviewAutomationOperation::Click => {
                let click: PreviewAutomationClickInput = decode(input)?;
                let pointer = self.pointer_for(tab);
                self.record_action(tab, "click", tab.page.click(&click, &pointer)).await?;
                Ok(Value::Null)
            }
            PreviewAutomationOperation::Hover => {
                let hover: PreviewAutomationHoverInput = decode(input)?;
                let pointer = self.pointer_for(tab);
                self.record_action(tab, "hover", tab.page.hover(&hover, &pointer)).await?;
                Ok(Value::Null)
            }
            PreviewAutomationOperation::Select => {
                let select: PreviewAutomationSelectInput = decode(input)?;
                let selected = self.record_action(tab, "select", tab.page.select(&select)).await?;
                Ok(json!({"selected": selected}))
            }
            PreviewAutomationOperation::Drag => {
                let drag: PreviewAutomationDragInput = decode(input)?;
                let pointer = self.pointer_for(tab);
                self.record_action(tab, "drag", tab.page.drag(&drag, &pointer)).await?;
                Ok(Value::Null)
            }
            PreviewAutomationOperation::Upload => {
                let upload: PreviewAutomationUploadInput = decode(input)?;
                self.record_action(tab, "upload", self.upload_files(tab, &upload)).await?;
                Ok(Value::Null)
            }
            PreviewAutomationOperation::Type => {
                let typing: PreviewAutomationTypeInput = decode(input)?;
                self.record_action(tab, "type", tab.page.type_text(&typing)).await?;
                Ok(Value::Null)
            }
            PreviewAutomationOperation::Press => {
                let press: PreviewAutomationPressInput = decode(input)?;
                self.record_action(tab, "press", tab.page.press(&press)).await?;
                Ok(Value::Null)
            }
            PreviewAutomationOperation::Scroll => {
                let scroll: PreviewAutomationScrollInput = decode(input)?;
                self.record_action(tab, "scroll", tab.page.scroll(&scroll)).await?;
                Ok(Value::Null)
            }
            PreviewAutomationOperation::Evaluate => {
                // TODO(original: the expression is not inspected or confined). It runs in the page's main world with that page's powers, on
                // whatever origin the tab is on, and what it returns (up to 64 KB) goes back to the agent.
                let evaluate: PreviewAutomationEvaluateInput = decode(input)?;
                tab.page.evaluate(evaluate.expression.as_deref().unwrap_or_default(), evaluate.await_promise.unwrap_or(true), evaluate.return_by_value.unwrap_or(true)).await
            }
            PreviewAutomationOperation::WaitFor => {
                let wait: PreviewAutomationWaitForInput = decode(input)?;
                tab.page.wait_for(wait.target(), wait.text.as_deref(), wait.url_includes.as_deref(), wait.timeout_ms.unwrap_or(crate::server_browser_page::DEFAULT_TIMEOUT_MS)).await?;
                Ok(Value::Null)
            }
            PreviewAutomationOperation::RecordingStart => {
                let started_at = self.start_recording(tab).await?;
                Ok(json!({"tabId": tab.tab_id, "recording": true, "startedAt": started_at}))
            }
            other => Err(OpError::execution(format!("The server browser does not run {}.", other.as_str()))),
        }
    }

    /// Runs a request the broker routed here and answers it.
    pub(crate) async fn handle_request(self: &Arc<Self>, connection_id: String, request: PreviewAutomationRequest) {
        let view = RequestView::of(&request);
        let outcome = self.run_operation(&view).await;
        self.mark_used(&view);
        let mut response = json!({"clientId": SERVER_BROWSER_AUTOMATION_CLIENT_ID, "connectionId": connection_id, "requestId": view.request_id});
        match outcome {
            Ok(result) => {
                response["ok"] = json!(true);
                response["result"] = result;
            }
            Err(error) => {
                response["ok"] = json!(false);
                response["error"] = json!({"_tag": error.tag, "message": error.message});
                if let Some(detail) = error.detail {
                    response["error"]["detail"] = detail;
                }
            }
        }
        if let Ok(response) = serde_json::from_value(response) {
            self.broker.respond(response);
        }
    }

    /// Any client or agent may change a tab; the page follows what was published.
    pub(crate) async fn mirror_manager_event(self: &Arc<Self>, event: Value) {
        let (thread_id, tab_id) = (event["threadId"].as_str().unwrap_or_default().to_owned(), event["tabId"].as_str().unwrap_or_default().to_owned());
        let key = tab_key(&thread_id, &tab_id);
        if event["type"] == "opened" && event["snapshot"]["runtime"] == "server" {
            let _ = self.ensure_tab(event["snapshot"].clone()).await;
            return;
        }
        let tab = self.tabs().get(&key).cloned();
        if event["type"] == "closed" && tab.is_none() && Self::lock(&self.pending).contains_key(&key) {
            Self::lock(&self.closed_pending).insert(key.clone());
        }
        let Some(tab) = tab else { return };
        if event["type"] == "closed" {
            self.drop_tab(&tab, false);
            return;
        }
        if event["type"] != "resized" {
            return;
        }
        let (core, running, event) = (self.clone(), tab.clone(), event);
        let applied = tab
            .control
            .system::<_, OpError, _, _>(move || async move {
                let setting = event["snapshot"].get("viewport").filter(|setting| !setting.is_null()).cloned().unwrap_or_else(fill_setting);
                if setting != running.state().setting {
                    core.apply_setting(&running, setting).await?;
                }
                core.apply_rendering(&running, &event["snapshot"]).await?;
                let request = &event["request"];
                if request["clear"] == "cookies" {
                    running.page.send("Network.clearBrowserCookies", json!({})).await?;
                }
                if request["clear"] == "cache" {
                    running.page.send("Network.clearBrowserCache", json!({})).await?;
                }
                if request["hardReload"] == true {
                    running.page.send("Page.reload", json!({"ignoreCache": true})).await?;
                }
                Ok(())
            })
            .await;
        if let Err(error) = applied {
            tracing::warn!(error = %error, "server preview could not apply a tab setting");
        }
    }

    /// A browser's own events: popups its pages opened, and downloads.
    async fn browser_event(self: &Arc<Self>, browser: u64, event: &crate::cdp::CdpEvent) {
        match event.method.as_str() {
            "Target.targetCreated" => {
                let info = &event.params["targetInfo"];
                if info["type"] != "page" {
                    return;
                }
                let (Some(opener), Some(target)) = (info["openerId"].as_str(), info["targetId"].as_str()) else { return };
                let opener = self.tab_list().into_iter().find(|tab| tab.context.browser().id() == browser && tab.page.target_id() == opener);
                if let Some(opener) = opener {
                    self.adopt_popup(&opener, target, info["url"].as_str().unwrap_or_default()).await;
                }
            }
            "Browser.downloadWillBegin" => self.download_began(&event.params),
            "Browser.downloadProgress" => self.download_progress(&event.params).await,
            _ => {}
        }
    }
}

static ERR_PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"ERR_[A-Z_]+").expect("a valid pattern"));

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn decode<T: serde::de::DeserializeOwned>(input: &Value) -> OpResult<T> {
    serde_json::from_value(input.clone()).map_err(|error| OpError::execution(format!("The tool input is malformed: {error}")))
}

fn recording_screencast() -> Value {
    json!({"format": "jpeg", "quality": RECORDING_QUALITY, "everyNthFrame": 1})
}

/// What a request asks, read once from the contract value.
#[derive(Clone)]
pub(crate) struct RequestView {
    pub request_id: String,
    pub thread_id: String,
    pub tab_id: Option<String>,
    pub tab_id_explicit: bool,
    pub agent_session_id: Option<String>,
    pub operation: PreviewAutomationOperation,
    pub input: Value,
    pub timeout_ms: u64,
}

impl RequestView {
    pub(crate) fn of(request: &PreviewAutomationRequest) -> Self {
        Self {
            request_id: request.request_id.as_str().to_owned(),
            thread_id: request.thread_id.as_str().to_owned(),
            tab_id: request.tab_id.clone().flatten().map(|tab| tab.as_str().to_owned()),
            tab_id_explicit: request.tab_id_explicit.flatten().unwrap_or(false),
            agent_session_id: request.agent_session_id.clone().flatten(),
            operation: request.operation,
            input: request.input.clone(),
            timeout_ms: request.timeout_ms.0 as u64,
        }
    }
}

/// The setting a resize asks for (packages/shared/src/previewViewport.ts).
pub fn resolve_preview_viewport(input: &PreviewAutomationResizeInput) -> OpResult<Value> {
    const PRESETS: [(&str, u32, u32); 17] = [
        ("iphone-se", 375, 667),
        ("iphone-xr", 414, 896),
        ("iphone-12-pro", 390, 844),
        ("iphone-14-pro-max", 430, 932),
        ("pixel-7", 412, 915),
        ("samsung-galaxy-s8-plus", 360, 740),
        ("samsung-galaxy-s20-ultra", 412, 915),
        ("ipad-mini", 768, 1024),
        ("ipad-air", 820, 1180),
        ("ipad-pro", 1024, 1366),
        ("surface-pro-7", 912, 1368),
        ("surface-duo", 540, 720),
        ("galaxy-z-fold-5", 344, 882),
        ("asus-zenbook-fold", 853, 1280),
        ("samsung-galaxy-a51-71", 412, 914),
        ("nest-hub", 1024, 600),
        ("nest-hub-max", 1280, 800),
    ];
    match input.mode {
        Some(PreviewResizeMode::Fill) => Ok(fill_setting()),
        Some(PreviewResizeMode::Preset) if input.preset.is_some() => {
            let id = input.preset.as_deref().unwrap_or_default();
            let (id, width, height) = PRESETS.iter().find(|(candidate, ..)| *candidate == id).copied().ok_or_else(|| OpError::execution(format!("Unknown preview viewport preset: {id}")))?;
            let native_portrait = height >= width;
            let swap = (input.orientation == Some(PreviewResizeOrientation::Landscape) && native_portrait) || (input.orientation == Some(PreviewResizeOrientation::Portrait) && !native_portrait);
            let (width, height) = if swap { (height, width) } else { (width, height) };
            Ok(json!({"_tag": "preset", "width": width, "height": height, "presetId": id}))
        }
        _ => match (input.width, input.height) {
            (Some(width), Some(height)) => Ok(json!({"_tag": "freeform", "width": width, "height": height})),
            _ => Err(OpError::execution("Custom preview viewport requires width and height")),
        },
    }
}

impl ServerBrowser {
    /// Starts the browser's work: it follows the manager's sessions, serves the automation broker as the preferred host, closes idle agent
    /// tabs, and warns once at startup if the host blocks the sandbox it needs. Nothing launches until the first tab is asked for.
    pub fn start(options: ServerBrowserOptions) -> Self {
        let core = Arc::new(Core {
            contexts: options.contexts,
            manager: options.manager,
            broker: options.broker,
            environment_id: options.environment_id,
            downloads_dir: options.downloads_dir,
            attachments_dir: options.attachments_dir,
            tabs: Mutex::default(),
            pending: Mutex::default(),
            closed_pending: Mutex::default(),
            adopted: Mutex::default(),
            host_connection: Mutex::default(),
            viewer_resize_order: AtomicU64::new(0),
            pointer_sequence: AtomicU64::new(0),
            pending_downloads: Mutex::default(),
            tasks: Mutex::default(),
            stopped: AtomicBool::new(false),
            browser_handled: tokio::sync::watch::channel(0).0,
            mirrored: tokio::sync::watch::channel(0).0,
        });
        // A context that closes takes its tabs with it.
        let weak = Arc::downgrade(&core);
        core.contexts.on_context_close(move |context| {
            if let Some(core) = weak.upgrade() {
                for tab in core.tab_list() {
                    if Arc::ptr_eq(&tab.context, &context) {
                        core.drop_tab(&tab, true);
                    }
                }
            }
        });
        let mut tasks = Vec::new();
        // The manager's sessions, from now on. Subscribing is synchronous, so none is missed between here and the task's first poll.
        let mut sessions = core.manager.subscribe();
        tasks.push({
            let weak = Arc::downgrade(&core);
            tokio::spawn(async move {
                while let Some(event) = sessions.recv().await {
                    let Some(core) = weak.upgrade() else { return };
                    core.mirror_manager_event(serde_json::to_value(&event).unwrap_or_default()).await;
                    core.mirrored.send_modify(|count| *count += 1);
                }
            })
        });
        if let Some(mut browser_events) = core.contexts.take_events() {
            let weak = Arc::downgrade(&core);
            tasks.push(tokio::spawn(async move {
                while let Some(event) = browser_events.recv().await {
                    let Some(core) = weak.upgrade() else { return };
                    core.browser_event(event.browser, &event.event).await;
                    core.browser_handled.send_modify(|count| *count += 1);
                }
            }));
        }
        // Whoever runs the server learns the fix before anyone opens a tab.
        if options.sandbox_blocked {
            tracing::warn!("This host blocks the sandbox T3's browser runs in, so browser tabs and HTML previews will not start. Run `{}` once to allow it.", options.setup_command);
        }
        tasks.push({
            let weak = Arc::downgrade(&core);
            tokio::spawn(async move {
                let mut sweep = tokio::time::interval(IDLE_SWEEP_INTERVAL);
                sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // The first tick is immediate; the sweep starts a period later.
                sweep.tick().await;
                loop {
                    sweep.tick().await;
                    let Some(core) = weak.upgrade() else { return };
                    core.close_idle_agent_tabs();
                }
            })
        });
        // Registered before this returns, so a call made at once finds the browser as its host.
        let host_for = |core: &Core| {
            serde_json::from_value(json!({
                "clientId": SERVER_BROWSER_AUTOMATION_CLIENT_ID,
                "environmentId": core.environment_id,
                "supportedOperations": t3_contracts::PREVIEW_AUTOMATION_SERVER_OPERATIONS.iter().map(|operation| operation.as_str()).collect::<Vec<_>>(),
            }))
            .expect("the browser's own host description is valid")
        };
        let first = core.broker.connect(host_for(&core), true);
        tasks.push({
            let weak = Arc::downgrade(&core);
            tokio::spawn(async move {
                let mut connection = first;
                loop {
                    while let Some(event) = connection.next().await {
                        let Some(core) = weak.upgrade() else { return };
                        match event {
                            t3_contracts::PreviewAutomationStreamEvent::Connected(connected) => {
                                *Core::lock(&core.host_connection) = Some(connected.connection_id.as_str().to_owned());
                                core.report_live_tabs();
                            }
                            t3_contracts::PreviewAutomationStreamEvent::Request(request) => {
                                let connection_id = request.connection_id.as_str().to_owned();
                                let handler = core.clone();
                                tokio::spawn(async move { handler.handle_request(connection_id, request.request).await });
                            }
                        }
                    }
                    drop(connection);
                    // The broker disconnects timed-out hosts, including slow first installs. Reconnect.
                    tokio::time::sleep(HOST_RECONNECT_DELAY).await;
                    let Some(core) = weak.upgrade() else { return };
                    connection = core.broker.connect(host_for(&core), true);
                }
            })
        });
        Core::lock(&core.tasks).extend(tasks);
        Self(core)
    }

    /// Stops the browser: its tasks end, every tab closes and every browser process is shut down.
    pub async fn shutdown(&self) {
        if self.0.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let tasks: Vec<JoinHandle<()>> = std::mem::take(&mut *Core::lock(&self.0.tasks));
        for task in &tasks {
            task.abort();
        }
        self.0.contexts.close().await;
    }

    /// Deletes a human profile's server-side storage, closing its open tabs first.
    pub async fn clear_profile(&self, profile_id: &str) -> std::io::Result<()> {
        self.0.contexts.clear_profile(profile_id).await
    }

    /// Gives a page's open file picker the files a viewer uploaded, by path, and closes it. No files cancels the pick. False when that picker
    /// is gone.
    pub async fn answer_file_chooser(&self, thread_id: &str, tab_id: &str, chooser_id: &str, files: Vec<UploadedFile>) -> bool {
        let Some(tab) = self.0.tab(thread_id, tab_id) else { return false };
        let Some(open) = tab.state().file_chooser.clone().filter(|open| open.id == chooser_id) else { return false };
        if !files.is_empty() {
            // The viewer's bytes are written under a directory of their own, because the browser takes paths.
            let directory = self.0.downloads_dir.join("uploads").join(uuid::Uuid::new_v4().to_string());
            let Ok(paths) = write_uploads(&directory, if open.multiple { &files[..] } else { &files[..1] }).await else { return false };
            let given = tab.page.set_chooser_files(open.backend_node_id, &paths).await;
            let _ = tokio::fs::remove_dir_all(&directory).await;
            if given.is_err() {
                return false;
            }
        }
        if tab.state().file_chooser.as_ref().is_some_and(|current| current.id == open.id) {
            self.0.close_file_chooser(&tab);
        }
        true
    }

    /// A finished download of a live tab, for the authenticated download route.
    pub fn open_download(&self, thread_id: &str, tab_id: &str, download_id: &str) -> Option<(PathBuf, String)> {
        let tab = self.0.tab(thread_id, tab_id)?;
        let state = tab.state();
        let download = state.downloads.iter().find(|candidate| candidate.id == download_id)?;
        Some((download.path.clone(), download.file_name.clone()))
    }

    /// How many tabs are open, for tests and diagnostics.
    pub fn tab_count(&self) -> usize {
        self.0.tabs().len()
    }
}

/// A file a viewer gave a page's picker.
pub struct UploadedFile {
    pub name: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// Writes uploads where the browser can read them, each under the last component of its name.
async fn write_uploads(directory: &Path, files: &[UploadedFile]) -> std::io::Result<Vec<String>> {
    tokio::fs::create_dir_all(directory).await?;
    let mut paths = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let name = file.name.rsplit(['/', '\\']).next().unwrap_or_default().trim();
        let name = if name.is_empty() || name == "." || name == ".." { format!("upload-{index}") } else { name.chars().filter(|c| !c.is_control()).collect() };
        // One directory per file, so two files of the same name both keep it.
        let folder = directory.join(index.to_string());
        tokio::fs::create_dir_all(&folder).await?;
        let path = folder.join(name);
        tokio::fs::write(&path, &file.bytes).await?;
        paths.push(path.to_string_lossy().into_owned());
    }
    Ok(paths)
}

/// The browser an installation runs, once started.
#[derive(Default)]
struct Running(Mutex<Option<ServerBrowser>>);

/// Starts the server browser of the installation `store` belongs to, if the switch says so, and gives the routes and the profile-clearing
/// method their handle on it. Nothing is launched yet: the first tab asked for installs and starts the browser. None when it is off, or
/// when the installation has nowhere to keep a browser's files (the in-memory store).
pub async fn start_for_store(store: &crate::persistence::Store, environment_id: &str, enabled: bool) -> Option<ServerBrowser> {
    if !enabled {
        return None;
    }
    let state_dir = store.state_dir()?.to_owned();
    let browser_install = crate::html_render::preview_browser_for(store)?;
    let attachments_dir = crate::attachment_service::AttachmentService::for_store(store)?.attachments_dir().to_owned();
    let no_sandbox = crate::preview_browser_host::sandbox_disabled(|name| std::env::var(name).ok());
    let root = state_dir.join("server-browser");
    let contexts = ServerBrowserContexts::new(crate::server_browser_contexts::ContextsOptions {
        profiles_dir: crate::server_browser_profiles::profiles_dir(&state_dir),
        downloads_dir: root.join("downloads"),
        executable: Arc::new(browser_install),
        launcher: Arc::new(crate::headless_chrome::DirectProcessLauncher),
        no_sandbox,
        setup_command: crate::html_render::root_setup_command(),
        scratch_root: std::env::temp_dir(),
    });
    let sandbox_blocked = !no_sandbox && crate::preview_browser_host::sandbox_blocked().await;
    let browser = ServerBrowser::start(ServerBrowserOptions {
        contexts,
        manager: store.extension(PreviewManager::default),
        broker: store.extension(PreviewAutomationBroker::default),
        environment_id: environment_id.to_owned(),
        downloads_dir: root.join("downloads"),
        attachments_dir,
        setup_command: crate::html_render::root_setup_command(),
        sandbox_blocked,
    });
    *store.extension(Running::default).0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(browser.clone());
    crate::server_browser_http::install(store, Arc::new(browser.clone()));
    Some(browser)
}

/// Ends the server browser of an installation, as the end of the server does: every tab and browser process goes.
pub async fn shutdown_for_store(store: &crate::persistence::Store) {
    let running = store.extension(Running::default).0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    if let Some(browser) = running {
        browser.shutdown().await;
    }
}

#[cfg(test)]
#[path = "server_browser_tests.rs"]
mod tests;
