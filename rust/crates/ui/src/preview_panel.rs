//! The browser tab of the right panel (`PreviewView.tsx`, `PreviewChromeRow.tsx`, `PreviewMoreMenu.tsx`, `PreviewEmptyState.tsx`,
//! `PreviewUnreachable.tsx`, `usePreviewSession.ts` and `ServerBrowserSurface.tsx`): a session of the environment's own browser.
//! The session list and its events are followed for as long as the panel is open on a thread (`Sync`); the chrome row drives the tab
//! through the stream where the original's does (address, back, forward, reload), and through `preview.*` where the original's does
//! (viewport, zoom, appearance, hard reload, clearing data, closing). The page itself is `preview_surface_abi.js`: it holds the socket and
//! paints, and Rust says what each message means (`t3_client::preview`).
//!
//! Not ported: the original's Electron webview tabs, annotations, recordings, the floating player and the touch keyboard probe.
use crate::{
    notice::{self, Level},
    right_panel,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use t3_client::{
    preview::{self as logic, Closed, Control, FileChooser, Message, ThreadPreviewState},
    right_panel::ClosedBrowser,
};
use t3_contracts::{AuthEnvironmentScope, Present, PreviewEvent, PreviewListResult, PreviewSessionSnapshot, PreviewStreamHostSetup};

pub(crate) const UNAVAILABLE: &str = "Preview is only available in the T3 Code desktop app.";
pub(crate) const NEEDS_ACCESS: &str = "Pair this client again with preview access to control browser previews.";
/// The browser opens in the environment's profile `incognito` when a person picks it; the picked default never is that one.
const DEFAULT_PROFILE: &str = "default";

/// What each thread knows of its preview sessions, by `environment:thread` (`previewStateStore`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    threads: BTreeMap<String, ThreadPreviewState>,
    /// The last thing a shortcut asked of the shown browser tab, and how many have been asked: the tab acts on each once.
    action: Option<(u64, Action)>,
}

/// What the browser shortcuts ask of the tab that is shown (`previewActionBus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Refresh,
    FocusUrl,
    ZoomIn,
    ZoomOut,
    ResetZoom,
}

/// Asks the shown browser tab to do `action`; the tab does it when it next renders.
pub(crate) fn dispatch(state: Store<UiModel>, action: Action) {
    let mut field = state.previews();
    let mut model = field.write();
    let count = model.action.map_or(0, |(count, _)| count) + 1;
    model.action = Some((count, action));
}

/// `preview.toggle` (`togglePreviewPanel`): hides the panel when it shows a browser; else shows the thread's browser, a new tab when it
/// has none. Where the environment hosts no browser it says so, as the original does.
pub(crate) fn toggle(transport: &TransportHandle, state: Store<UiModel>) {
    let Some(owner) = right_panel::active_owner(state) else { return };
    let Some(thread) = owner.thread.clone() else { return };
    let shown = right_panel::shown_now(state, t3_client::right_panel::Kind::Preview);
    if !shown && !can_operate(state) {
        return;
    }
    if !shown && !*state.typed_config().peek().as_ref().map(|config| config.environment.capabilities.server_browser == Some(true)).get_or_insert(false) {
        notice::push(state, Level::Info, "Preview is desktop-only", "Open T3 Code in the desktop app to use the in-app preview.");
        return;
    }
    if shown {
        right_panel::toggle_kind(state, t3_client::right_panel::Kind::Preview);
        return;
    }
    let key = right_panel::key_of(&owner.environment, Some(&thread));
    let sessions = thread_of_peek(state, &key);
    match sessions.active_tab.clone().or_else(|| sessions.tab_ids().into_iter().next()) {
        Some(tab) => right_panel::open_preview(state, &owner.environment, &thread, Some(&tab)),
        None => add_browser(transport, state, &owner),
    }
}

// ---- access ---------------------------------------------------------------------------------------------------------------

/// Whether the environment hosts browser tabs (`capabilities.serverBrowser`), subscribing the caller.
pub(crate) fn hosted(state: Store<UiModel>) -> bool {
    state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.server_browser == Some(true))
}
/// The connection's grant to control browser tabs, subscribing the caller.
pub(crate) fn can_operate(state: Store<UiModel>) -> bool {
    connected_allows(state, AuthEnvironmentScope::PreviewOperate)
}
/// Whether the Browser row can be opened: the environment hosts tabs and the connection may control them (`browserAvailable`).
pub(crate) fn available(state: Store<UiModel>) -> bool {
    hosted(state) && can_operate(state)
}

fn thread_of(state: Store<UiModel>, key: &str) -> ThreadPreviewState {
    state.previews().read().threads.get(key).cloned().unwrap_or_default()
}
fn edit_thread<R>(state: Store<UiModel>, key: &str, change: impl FnOnce(&mut ThreadPreviewState) -> R) -> R {
    let mut field = state.previews();
    let mut model = field.write();
    change(model.threads.entry(key.to_owned()).or_default())
}

fn event_thread(event: &PreviewEvent) -> &str {
    match event {
        PreviewEvent::Opened(event) => event.thread_id.as_str(),
        PreviewEvent::Navigated(event) => event.thread_id.as_str(),
        PreviewEvent::Resized(event) => event.thread_id.as_str(),
        PreviewEvent::Failed(event) => event.thread_id.as_str(),
        PreviewEvent::Closed(event) => event.thread_id.as_str(),
    }
}
fn event_epoch(event: &PreviewEvent) -> &str {
    match event {
        PreviewEvent::Opened(event) => event.server_epoch.as_str(),
        PreviewEvent::Navigated(event) => event.server_epoch.as_str(),
        PreviewEvent::Resized(event) => event.server_epoch.as_str(),
        PreviewEvent::Failed(event) => event.server_epoch.as_str(),
        PreviewEvent::Closed(event) => event.server_epoch.as_str(),
    }
}

// ---- following the sessions ---------------------------------------------------------------------------------------------

/// Keeps the thread's session index current while the panel is open on it, and the panel's browser tabs in step with it.
#[component]
pub(crate) fn Sync(state: Store<UiModel>, transport: TransportHandle, environment_key: String, thread_id: String) -> Element {
    let key = right_panel::key_of(&environment_key, Some(&thread_id));
    let live = use_memo(move || *state.status().read() == t3_client::connection::ConnectionStatus::Connected && available(state));
    let follow_key = key.clone();
    let _ = use_resource(move || {
        let (transport, key, thread) = (transport.clone(), follow_key.clone(), thread_id.clone());
        async move {
            if !live() {
                return;
            }
            follow(&transport, state, &key, &thread).await;
        }
    });
    // The panel holds a tab for each session the server lists, once it has listed them.
    let reconcile_key = key.clone();
    use_effect(move || {
        let sessions = thread_of(state, &reconcile_key);
        if !sessions.list_loaded {
            return;
        }
        let hidden = sessions
            .sessions
            .values()
            .filter(|session| session.runtime.present() == Some(&t3_contracts::PreviewRuntime::Server) && session.reveal.present() == Some(&false))
            .map(|session| session.tab_id.as_str().to_owned())
            .collect();
        let tabs = sessions.tab_ids();
        let mut field = state.right_panel();
        let mut model = field.write();
        model.panels.reconcile_previews(&reconcile_key, &tabs, &hidden);
    });
    rsx! {}
}

async fn list(transport: &TransportHandle, state: Store<UiModel>, key: &str, thread: &str) {
    if let Ok(value) = runtime::request_value_reported(transport.clone(), state, "preview.list", json!({"threadId": thread})).await {
        if let Ok(result) = serde_json::from_value::<PreviewListResult>(value) {
            edit_thread(state, key, |sessions| sessions.reconcile(&result));
        }
    }
}
async fn follow(transport: &TransportHandle, state: Store<UiModel>, key: &str, thread: &str) {
    loop {
        // The events first, so that nothing between the list and the stream is missed; the revisions put them in order.
        let Ok(mut events) = runtime::request_stream(transport, state, "subscribePreviewEvents", json!({})) else { return };
        list(transport, state, key, thread).await;
        while let Some(batch) = events.next().await {
            let Ok(values) = batch else { break };
            for value in values {
                let Ok(event) = serde_json::from_value::<PreviewEvent>(value) else { continue };
                if event_thread(&event) != thread {
                    continue;
                }
                // Another server process numbers its revisions again: the list is read anew.
                let epoch = thread_of_peek(state, key).server_epoch;
                if epoch.as_deref().is_some_and(|epoch| epoch != event_epoch(&event)) {
                    list(transport, state, key, thread).await;
                    continue;
                }
                edit_thread(state, key, |sessions| sessions.apply_event(&event));
            }
        }
        // The stream ended with the connection or the server; whoever started this ends it when the panel goes.
        runtime::delay(500).await;
        if *state.status().peek() != t3_client::connection::ConnectionStatus::Connected {
            return;
        }
    }
}
fn thread_of_peek(state: Store<UiModel>, key: &str) -> ThreadPreviewState {
    state.previews().peek().threads.get(key).cloned().unwrap_or_default()
}

// ---- opening and closing -----------------------------------------------------------------------------------------------

/// The viewport and profile a new tab opens with (`browserDefaultOpenViewport`, `browserDefaultOpenProfileId`): the person's defaults,
/// and the built-in profile when the default one was deleted or is the private one.
fn open_defaults(state: Store<UiModel>) -> (Value, String) {
    let settings = state.client_settings().peek().clone();
    let viewport = serde_json::to_value(&settings.browser_default_viewport).unwrap_or_else(|_| json!({"_tag": "fill"}));
    let profiles = t3_contracts::resolve_browser_profiles(&settings.browser_profiles);
    let profile = profiles
        .iter()
        .find(|profile| profile.id == settings.browser_default_profile_id && profile.kind != t3_contracts::BrowserProfileKind::Incognito)
        .map_or_else(|| DEFAULT_PROFILE.to_owned(), |profile| profile.id.as_str().to_owned());
    (viewport, profile)
}

/// `openPreviewSession`: a new tab, empty or at `url`, which this window then looks at. Returns it.
pub(crate) async fn open_session(transport: TransportHandle, state: Store<UiModel>, environment: String, thread_id: String, url: Option<String>) -> Result<PreviewSessionSnapshot, String> {
    let (viewport, profile) = open_defaults(state);
    let input = logic::open_input(&thread_id, url.as_deref(), viewport, &profile);
    let value = runtime::request_value_reported(transport.clone(), state, "preview.open", input).await?;
    let mut snapshot: PreviewSessionSnapshot = serde_json::from_value(value).map_err(|error| format!("Invalid preview session: {error}"))?;
    // The tab starts at the person's default zoom and appearance (`browserDefaultTabState`); a tab that cannot take them keeps its own.
    let (zoom, appearance) = {
        let field = state.client_settings();
        let settings = field.peek();
        (settings.browser_default_zoom_factor.0, serde_json::to_value(settings.browser_default_appearance).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default())
    };
    if let Some(change) = logic::default_tab_change(zoom, &appearance) {
        let input = logic::adjust_input(&thread_id, snapshot.tab_id.as_str(), change);
        if let Some(adjusted) = runtime::request_value_reported(transport.clone(), state, "preview.adjust", input).await.ok().and_then(|value| serde_json::from_value(value).ok()) {
            snapshot = adjusted;
        }
    }
    let key = right_panel::key_of(&environment, Some(&thread_id));
    edit_thread(state, &key, |sessions| {
        sessions.apply_snapshot(&snapshot);
        if let Some(url) = url.as_deref() {
            sessions.remember_url(url);
        }
    });
    Ok(snapshot)
}

/// The Browser row of the add menu and the launcher (`addBrowserSurface`): a new empty tab in the profile the person chose.
pub(crate) fn add_browser(transport: &TransportHandle, state: Store<UiModel>, owner: &right_panel::Owner) {
    let Some(thread) = owner.thread.clone() else { return };
    let (transport, environment) = (transport.clone(), owner.environment.clone());
    // The menu or launcher it was chosen in is gone before the session is.
    dioxus::dioxus_core::spawn_forever(async move {
        match open_session(transport, state, environment.clone(), thread.clone(), None).await {
            Ok(snapshot) => right_panel::open_preview(state, &environment, &thread, Some(snapshot.tab_id.as_str())),
            Err(message) => notice::push(state, Level::Error, "Unable to open browser", message),
        }
    });
}

/// What closing a browser tab does to the session behind it (`closePreviewSession`): it goes at once, and comes back if the server
/// cannot close it. A tab that had an address is remembered for "Reopen closed tab".
pub(crate) fn close_session(transport: &TransportHandle, state: Store<UiModel>, environment: &str, thread: &str, tab: &str) {
    if !can_operate(state) {
        // Without the grant only the panel's tab goes; the session is the server's bookkeeping and stays listed.
        return;
    }
    let key = right_panel::key_of(environment, Some(thread));
    let snapshot = thread_of_peek(state, &key).sessions.get(tab).cloned();
    edit_thread(state, &key, |sessions| sessions.begin_close(tab));
    let (transport, thread, tab) = (transport.clone(), thread.to_owned(), tab.to_owned());
    spawn(async move {
        let result = runtime::request_value_reported(transport, state, "preview.close", logic::close_input(&thread, &tab)).await;
        if result.is_err() {
            edit_thread(state, &key, |sessions| sessions.cancel_close(snapshot.as_ref(), &tab));
        }
    });
}
/// What reopening needs of a tab that is closed: its address, size and profile, when it had a session.
pub(crate) fn closed_browser(state: Store<UiModel>, environment: &str, thread: &str, tab: &str) -> Option<ClosedBrowser> {
    let key = right_panel::key_of(environment, Some(thread));
    let snapshot = thread_of_peek(state, &key).sessions.get(tab).cloned()?;
    let url = match logic::address(Some(&snapshot)) {
        url if url.is_empty() => None,
        url => Some(url),
    };
    Some(ClosedBrowser { url, viewport: snapshot.viewport.present().and_then(|viewport| serde_json::to_value(viewport).ok()), profile_id: Some(logic::profile_of(&snapshot)) })
}
/// `reopenClosedView` for a browser tab: a new session at the old address, size and profile.
pub(crate) fn reopen_session(transport: &TransportHandle, state: Store<UiModel>, environment: &str, thread: &str, closed: &ClosedBrowser) {
    let (transport, environment, thread, closed) = (transport.clone(), environment.to_owned(), thread.to_owned(), closed.clone());
    dioxus::dioxus_core::spawn_forever(async move {
        let (default_viewport, default_profile) = open_defaults(state);
        let input = logic::open_input(&thread, closed.url.as_deref(), closed.viewport.clone().unwrap_or(default_viewport), closed.profile_id.as_deref().unwrap_or(&default_profile));
        let Ok(value) = runtime::request_value_reported(transport, state, "preview.open", input).await else { return };
        let Ok(snapshot) = serde_json::from_value::<PreviewSessionSnapshot>(value) else { return };
        let key = right_panel::key_of(&environment, Some(&thread));
        edit_thread(state, &key, |sessions| sessions.apply_snapshot(&snapshot));
        right_panel::open_preview(state, &environment, &thread, Some(snapshot.tab_id.as_str()));
    });
}

// ---- the page ---------------------------------------------------------------------------------------------------------

/// What the page reports (`preview_surface_abi.js`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PageEvent {
    Open,
    Text(String),
    Frame,
    Close { code: u16, reason: String, opened: bool },
    Size { width: f64, height: f64, ratio: f64 },
}
impl PageEvent {
    #[cfg(not(test))]
    fn of(value: &Value) -> Option<Self> {
        Some(match value["t"].as_str()? {
            "open" => Self::Open,
            "text" => Self::Text(value["data"].as_str()?.to_owned()),
            "frame" => Self::Frame,
            "close" => Self::Close { code: value["code"].as_u64().unwrap_or(1006) as u16, reason: value["reason"].as_str().unwrap_or_default().to_owned(), opened: value["opened"].as_bool().unwrap_or(false) },
            "size" => Self::Size { width: value["width"].as_f64()?, height: value["height"].as_f64()?, ratio: value["ratio"].as_f64().unwrap_or(1.0) },
            _ => return None,
        })
    }
}
/// What is asked of the page.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PageCmd {
    Connect(String),
    Send(Value),
    Stop,
}
impl PageCmd {
    #[cfg(not(test))]
    fn wire(&self) -> Value {
        match self {
            Self::Connect(url) => json!({"type": "connect", "url": url}),
            Self::Send(message) => json!({"type": "send", "message": message}),
            Self::Stop => json!({"type": "stop"}),
        }
    }
}
/// The two ends of the conversation with the page: what it reports, and what is asked of it. Dropping the commands ends the page.
pub(crate) struct Page {
    pub events: UnboundedReceiver<PageEvent>,
    pub commands: UnboundedSender<PageCmd>,
}

/// The commands of the viewer in use, for the chrome row beside it.
#[derive(Clone, Default)]
pub(crate) struct Link(Rc<RefCell<Option<UnboundedSender<PageCmd>>>>);
impl PartialEq for Link {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}
impl Link {
    fn set(&self, sender: Option<UnboundedSender<PageCmd>>) {
        *self.0.borrow_mut() = sender;
    }
    pub(crate) fn set_sender(&self, sender: Option<UnboundedSender<PageCmd>>) {
        self.set(sender);
    }
    /// Ends the page this link reaches.
    pub(crate) fn stop(&self) {
        if let Some(sender) = self.0.borrow().as_ref() {
            let _ = sender.unbounded_send(PageCmd::Stop);
        }
        self.set(None);
    }
    /// A packet for a device's helper: a tag byte and its JSON body.
    pub(crate) fn send_tagged(&self, tag: u8, body: Value) -> bool {
        self.send(json!({"tag": tag, "body": body}))
    }
    pub(crate) fn send(&self, message: Value) -> bool {
        self.0.borrow().as_ref().is_some_and(|sender| sender.unbounded_send(PageCmd::Send(message)).is_ok())
    }
}

#[cfg(test)]
thread_local! {
    /// Under test there is no page to run the script: the test says what stands in for it (`preview_tests`).
    pub(crate) static TEST_PAGE: RefCell<Option<Box<dyn Fn(UnboundedSender<PageEvent>, UnboundedReceiver<PageCmd>)>>> = const { RefCell::new(None) };
}

/// Which script runs the page: the browser tab's, or the input of a device's picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Script {
    Preview,
    Device,
}

/// Starts the page's script on the surface `surface` and returns its two ends.
pub(crate) fn open_page(script: Script, surface: &str, follow_size: bool) -> Page {
    let (event_sender, events) = unbounded::<PageEvent>();
    let (commands, command_receiver) = unbounded::<PageCmd>();
    #[cfg(test)]
    {
        let _ = (script, surface, follow_size);
        TEST_PAGE.with(|page| {
            if let Some(start) = page.borrow().as_ref() {
                start(event_sender, command_receiver);
            }
        });
        return Page { events, commands };
    }
    #[cfg(not(test))]
    {
        let surface = surface.to_owned();
        let mut command_receiver = command_receiver;
        spawn(async move {
            let mut eval = document::eval(match script {
                Script::Preview => include_str!("../assets/preview_surface_abi.js"),
                Script::Device => include_str!("../assets/device_surface_abi.js"),
            });
            if eval.send(json!({"type": "init", "surface": surface, "followSize": follow_size})).is_err() {
                return;
            }
            let reader = spawn(async move {
                while let Ok(value) = eval.recv::<Value>().await {
                    if let Some(event) = PageEvent::of(&value) {
                        if event_sender.unbounded_send(event).is_err() {
                            break;
                        }
                    }
                }
            });
            // The commands end with the viewer: the script is told to stop, and the reader goes with it.
            while let Some(command) = command_receiver.next().await {
                let stop = command == PageCmd::Stop;
                let _ = eval.send(command.wire());
                if stop {
                    break;
                }
            }
            let _ = eval.send(PageCmd::Stop.wire());
            reader.cancel();
        });
        Page { events, commands }
    }
}

// ---- the viewer -------------------------------------------------------------------------------------------------------

/// A file the page downloaded while this viewer had control.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Download {
    pub id: String,
    pub file_name: String,
    pub size_bytes: u64,
}

/// What the viewer shows of the stream.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct View {
    pub connected: bool,
    pub control: Option<Control>,
    pub first_frame: bool,
    pub gone: bool,
    pub host_setup: Option<PreviewStreamHostSetup>,
    pub denied: bool,
    pub chooser: Option<FileChooser>,
    pub prompt: String,
    pub downloads: Vec<Download>,
    /// The page's own size, from the last `viewport` message.
    pub viewport: Option<(f64, f64)>,
}
impl View {
    fn yours(&self) -> bool {
        self.control.as_ref().is_some_and(Control::is_yours)
    }
}

const MAX_UNAUTHORIZED: u32 = 3;
/// How long to wait before asking again for the address of a stream that could not be minted (`ACCESS_RETRY_MS`).
const ACCESS_RETRY_MS: u64 = 10_000;

/// What asking the page to connect came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Connecting,
    /// No ticket or address could be had (the server is restarting): ask again later.
    Later,
    /// The page is gone.
    Gone,
}

/// Reads what the stream sends and mints the addresses it connects to, until the viewer goes or the stream ends for good.
async fn run(transport: TransportHandle, state: Store<UiModel>, thread: String, tab: String, mut view: Signal<View>, link: Link, mut page: Page, notify: UnboundedSender<Notify>) {
    link.set(Some(page.commands.clone()));
    let mut cap: Option<(f64, f64)> = None;
    let (mut failures, mut refusals) = (0u32, 0u32);
    let connect = |cap: (f64, f64)| {
        let (transport, thread, tab, commands) = (transport.clone(), thread.clone(), tab.clone(), page.commands.clone());
        async move {
            let mut query = logic::stream_query(&thread, &tab, cap.0, cap.1, true);
            match runtime::media_ticket(&transport, state).await {
                Ok(Some(ticket)) => query.push_str(&format!("&wsTicket={}", url::form_urlencoded::byte_serialize(ticket.as_bytes()).collect::<String>())),
                Ok(None) => {}
                Err(_) => return Asked::Later,
            }
            let Some(url) = runtime::environment_socket_url(&transport, &format!("api/preview-stream/ws?{query}")) else { return Asked::Later };
            if commands.unbounded_send(PageCmd::Connect(url)).is_ok() { Asked::Connecting } else { Asked::Gone }
        }
    };
    while let Some(event) = page.events.next().await {
        match event {
            PageEvent::Size { width, height, ratio } => {
                // The cap is in device pixels and only grows: outgrowing it is what reconnects.
                let wanted = ((width * ratio).round(), (height * ratio).round());
                let next = cap.map_or(wanted, |cap| (cap.0.max(wanted.0), cap.1.max(wanted.1)));
                if cap != Some(next) {
                    cap = Some(next);
                    loop {
                        match connect(next).await {
                            Asked::Connecting => break,
                            Asked::Gone => {
                                link.set(None);
                                return;
                            }
                            Asked::Later => runtime::delay(ACCESS_RETRY_MS).await,
                        }
                    }
                }
            }
            PageEvent::Open => view.with_mut(|view| view.connected = true),
            PageEvent::Frame => {
                refusals = 0;
                failures = 0;
                view.with_mut(|view| view.first_frame = true);
            }
            PageEvent::Text(text) => {
                let Some(message) = logic::parse_message(&text) else { continue };
                match message {
                    Message::Viewport { width, height } => {
                        failures = 0;
                        view.with_mut(|view| view.viewport = Some((width, height)));
                    }
                    Message::Control(control) => view.with_mut(|view| {
                        let dialog = |control: &Control| control.dialog.as_ref().map(|dialog| (dialog.message.clone(), dialog.default_value.clone()));
                        if view.control.as_ref().and_then(dialog) != dialog(&control) {
                            view.prompt = control.dialog.as_ref().map(|dialog| dialog.default_value.clone()).unwrap_or_default();
                        }
                        view.control = Some(control);
                    }),
                    Message::Clipboard(text) => {
                        let _ = notify.unbounded_send(Notify::Copy(text));
                    }
                    Message::Download { id, file_name, size_bytes } => view.with_mut(|view| {
                        view.downloads.retain(|download| download.id != id);
                        view.downloads.push(Download { id, file_name, size_bytes });
                    }),
                    Message::FileChooser(chooser) => view.with_mut(|view| view.chooser = Some(chooser)),
                    Message::FileChooserClosed(id) => view.with_mut(|view| {
                        if view.chooser.as_ref().is_some_and(|chooser| chooser.id == id) {
                            view.chooser = None;
                        }
                    }),
                    Message::Probe { .. } | Message::Pointer { .. } => {}
                }
            }
            PageEvent::Close { code, reason, opened } => {
                view.with_mut(|view| {
                    view.connected = false;
                    view.control = None;
                    view.chooser = None;
                    view.first_frame = false;
                });
                match logic::closed(code, &reason, opened) {
                    Closed::Gone => {
                        view.with_mut(|view| view.gone = true);
                        break;
                    }
                    Closed::HostSetup(setup) => {
                        view.with_mut(|view| view.host_setup = Some(setup));
                        break;
                    }
                    Closed::Unauthorized => {
                        // A new ticket answers the first refusal at once; repeated refusals wait for the person.
                        refusals += 1;
                        if refusals >= MAX_UNAUTHORIZED {
                            view.with_mut(|view| view.denied = true);
                            break;
                        }
                        if refusals > 1 {
                            runtime::delay(1000 * 2u64.pow(refusals - 1)).await;
                        }
                        if let Some(cap) = cap {
                            connect(cap).await;
                        }
                    }
                    Closed::Retry => {
                        runtime::delay(logic::retry_delay_ms(failures)).await;
                        failures += 1;
                        if let Some(cap) = cap {
                            while connect(cap).await == Asked::Later {
                                runtime::delay(ACCESS_RETRY_MS).await;
                            }
                        }
                    }
                }
            }
        }
    }
    link.set(None);
}

/// What the loop asks the component to do, which needs a scope of its own.
#[derive(Debug)]
enum Notify {
    Copy(String),
}

#[component]
fn Viewer(state: Store<UiModel>, transport: TransportHandle, environment_key: String, thread_id: String, tab: String, view: Signal<View>, link: Link, hidden: bool, retry: Signal<u32>) -> Element {
    let surface = use_hook(|| format!("preview-surface-{}", uuid::Uuid::new_v4()));
    let (notify_sender, notify_receiver) = use_hook(|| {
        let (sender, receiver) = unbounded::<Notify>();
        (sender, Rc::new(RefCell::new(Some(receiver))))
    });
    use_future(move || {
        let receiver = notify_receiver.borrow_mut().take();
        async move {
            let Some(mut receiver) = receiver else { return };
            while let Some(Notify::Copy(text)) = receiver.next().await {
                crate::sidebar_rows::copy_text(&text);
            }
        }
    });
    let run_surface = surface.clone();
    let run_link = link.clone();
    let _ = use_resource({
        let (transport, thread, tab) = (transport.clone(), thread_id.clone(), tab.clone());
        move || {
            // Bumping `retry` ("Try again", "Reconnect") starts the whole conversation again.
            let _ = retry();
            let (transport, thread, tab, surface, link, notify) = (transport.clone(), thread.clone(), tab.clone(), run_surface.clone(), run_link.clone(), notify_sender.clone());
            async move {
                // Not until the surface is in the page, whose script looks it up.
                runtime::delay(0).await;
                let page = open_page(Script::Preview, &surface, true);
                run(transport, state, thread, tab, view, link, page, notify).await;
            }
        }
    });
    use_drop({
        let link = link.clone();
        move || link.stop()
    });
    let current = view.read().clone();
    let label = logic::control_label(current.control.as_ref());
    let yours = current.yours();
    let operate = current.control.as_ref().is_some_and(|control| control.can_operate);
    let taken = current.control.as_ref().is_some_and(|control| control.controller == logic::Controller::AnotherViewer);
    let (send_take, send_link) = (link.clone(), link.clone());
    rsx! {
        div { id: "{surface}", class: if hidden { "preview-surface invisible" } else { "preview-surface" }, "data-server-browser-surface": "{tab}",
            div { class: "preview-control",
                span { role: "status", "{label}" }
                if operate {
                    button { r#type: "button", "aria-label": if yours { "Release control" } else { "Take control" }, disabled: taken, onclick: move |_| { send_take.send(if yours { logic::input::release_control() } else { logic::input::take_control() }); },
                        if yours { "Release control" } else { "Take control" } }
                }
            }
            div { class: "preview-frame",
                canvas { class: "preview-canvas", "aria-label": "Browser page frame" }
                textarea { class: "preview-input", "aria-label": "Browser page", "aria-description": "Press Shift+Escape to leave the browser page.", autocapitalize: "off", autocomplete: "off", spellcheck: "false", disabled: !yours, initial_value: "\u{200b}" }
                if let (Some(chooser), true) = (current.chooser.clone(), yours) {
                    FileChooserPrompt { state, transport: transport.clone(), thread_id: thread_id.clone(), tab: tab.clone(), chooser, view }
                }
                if let Some(dialog) = current.control.as_ref().and_then(|control| control.dialog.clone()) {
                    div { class: "preview-dialog", role: "dialog", "aria-label": "Browser dialog", "data-tab": "{tab}",
                        p { "{dialog.message}" }
                        if yours {
                            if dialog.kind == "prompt" {
                                input { "aria-label": "Dialog response", value: "{current.prompt}", oninput: move |event| view.with_mut(|view| view.prompt = event.value()) }
                            }
                            div { class: "preview-dialog-actions",
                                button { r#type: "button", "aria-label": "Dismiss dialog", onclick: move |_| { send_link.send(logic::input::dialog(false, None)); }, "Dismiss" }
                                button { r#type: "button", "aria-label": "Accept dialog", onclick: { let link = link.clone(); let prompt = current.prompt.clone(); let kind = dialog.kind.clone(); move |_| { link.send(logic::input::dialog(true, (kind == "prompt").then_some(prompt.as_str()))); } }, "Accept" }
                            }
                        } else {
                            p { class: "muted", "Take control to respond." }
                        }
                    }
                }
            }
            if !current.downloads.is_empty() {
                div { class: "preview-downloads", role: "list", "aria-label": "Downloaded files",
                    for download in current.downloads.iter().cloned() {
                        DownloadRow { key: "{download.id}", state, transport: transport.clone(), thread_id: thread_id.clone(), tab: tab.clone(), download, view }
                    }
                }
            }
            if let Some(setup) = current.host_setup.clone() {
                div { class: "preview-overlay",
                    p { role: "alert", "{logic::host_setup_message(&setup)}" }
                    code { class: "preview-command", "{setup.command}" }
                    button { r#type: "button", "aria-label": "Try again", onclick: move |_| { view.with_mut(|view| view.host_setup = None); retry.set(retry() + 1); }, "Try again" }
                }
            }
            if current.denied {
                div { class: "preview-overlay",
                    p { role: "alert", "Browser connection was refused." }
                    button { r#type: "button", "aria-label": "Reconnect", onclick: move |_| { view.with_mut(|view| view.denied = false); retry.set(retry() + 1); }, "Reconnect" }
                }
            }
            if current.gone {
                div { class: "preview-overlay", p { role: "status", "This browser tab was closed." } }
            }
        }
    }
}

/// The page opened a file picker: the person chooses what to give it, or declines (`answerFileChooser`).
#[component]
fn FileChooserPrompt(state: Store<UiModel>, transport: TransportHandle, thread_id: String, tab: String, chooser: FileChooser, mut view: Signal<View>) -> Element {
    let noun = if chooser.multiple { "files" } else { "a file" };
    let answer = {
        let (transport, thread, tab, id) = (transport.clone(), thread_id.clone(), tab.clone(), chooser.id.clone());
        use_callback(move |files: Vec<(String, String, Vec<u8>)>| {
            view.with_mut(|view| view.chooser = None);
            let (transport, thread, tab, id) = (transport.clone(), thread.clone(), tab.clone(), id.clone());
            // The prompt is gone as soon as it is answered, and the files still have to get to the page.
            dioxus::dioxus_core::spawn_forever(async move {
                let (content_type, body) = multipart(&files);
                let path = logic::upload_path(&thread, &tab, &id);
                if let Err(message) = runtime::environment_bytes(&transport, state, reqwest::Method::POST, &path, Some((content_type, body))).await {
                    notice::push(state, Level::Error, "Could not send the files to the page", message);
                }
            });
        })
    };
    rsx! {
        div { class: "preview-dialog", role: "dialog", "aria-label": "Choose files for the page", "data-tab": "{tab}",
            p { "The page asks for {noun}." }
            input { r#type: "file", "aria-label": "Choose {noun}", multiple: chooser.multiple, accept: "{chooser.accept}",
                onchange: move |event| {
                    let files = event.files();
                    spawn(async move {
                        let mut read = Vec::new();
                        for file in files {
                            if let Ok(bytes) = file.read_bytes().await {
                                read.push((file.name(), file.content_type().unwrap_or_else(|| "application/octet-stream".into()), bytes.to_vec()));
                            }
                        }
                        if !read.is_empty() {
                            answer.call(read);
                        }
                    });
                } }
            div { class: "preview-dialog-actions", button { r#type: "button", "aria-label": "Cancel file choice", onclick: move |_| answer.call(Vec::new()), "Cancel" } }
        }
    }
}

/// The body of a form with one `file` part per file; no parts is how the page's picker is cancelled.
fn multipart(files: &[(String, String, Vec<u8>)]) -> (String, Vec<u8>) {
    let boundary = format!("t3-boundary-{}", uuid::Uuid::new_v4().simple());
    let mut body = Vec::new();
    for (name, content_type, bytes) in files {
        let name = name.replace(['"', '\r', '\n', '\\'], "_");
        body.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n").into_bytes());
        body.extend(bytes);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{boundary}--\r\n").into_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// A file the page downloaded on the environment: this device keeps it when the person says so (`offerDownload`).
#[component]
fn DownloadRow(state: Store<UiModel>, transport: TransportHandle, thread_id: String, tab: String, download: Download, mut view: Signal<View>) -> Element {
    let name = download.file_name.clone();
    let save = {
        let (transport, thread, tab, id, name) = (transport.clone(), thread_id.clone(), tab.clone(), download.id.clone(), download.file_name.clone());
        move |_| {
            let (transport, thread, tab, id, name) = (transport.clone(), thread.clone(), tab.clone(), id.clone(), name.clone());
            spawn(async move {
                match runtime::environment_bytes(&transport, state, reqwest::Method::GET, &logic::download_path(&thread, &tab, &id), None).await {
                    Ok(bytes) => crate::media_save::save(state, &name, bytes).await,
                    Err(message) => notice::push(state, Level::Error, "Could not download the file", message),
                }
            });
        }
    };
    let id = download.id.clone();
    rsx! {
        div { class: "preview-download", role: "listitem",
            span { "Downloaded {name}" }
            button { r#type: "button", "aria-label": "Save {name}", onclick: save, "Save" }
            button { r#type: "button", "aria-label": "Dismiss {name}", onclick: move |_| view.with_mut(|view| view.downloads.retain(|download| download.id != id)), "\u{00d7}" }
        }
    }
}

// ---- the panel --------------------------------------------------------------------------------------------------------

/// The browser tab: its chrome row and the page below it, or the line that says why there is none.
#[component]
pub(crate) fn Panel(state: Store<UiModel>, transport: TransportHandle, environment_key: String, thread_id: String, tab: Option<String>) -> Element {
    let (operate, hosted_here) = (can_operate(state), hosted(state));
    if !operate || !hosted_here {
        return rsx! { div { class: "preview-panel", div { class: "preview-unavailable", p { class: "muted", role: "status", if operate { "{UNAVAILABLE}" } else { "{NEEDS_ACCESS}" } } } } };
    }
    rsx! { Browser { state, transport, environment_key, thread_id, tab } }
}

#[component]
fn Browser(state: Store<UiModel>, transport: TransportHandle, environment_key: String, thread_id: String, tab: Option<String>) -> Element {
    let key = right_panel::key_of(&environment_key, Some(&thread_id));
    let sessions = use_memo({
        let key = key.clone();
        move || thread_of(state, &key)
    });
    let sessions = sessions();
    let tab_id = tab.clone().or_else(|| sessions.active_tab.clone());
    let snapshot = tab_id.as_ref().and_then(|tab| sessions.sessions.get(tab)).cloned();
    let view = use_signal(View::default);
    let link = use_hook(Link::default);
    let retry = use_signal(|| 0u32);
    let mut draft = use_signal(|| None::<String>);
    let mut address_error = use_signal(|| None::<String>);
    let empty = logic::show_empty_state(snapshot.as_ref());
    let failed = snapshot.as_ref().and_then(|snapshot| match &snapshot.nav_status {
        t3_contracts::PreviewNavStatus::LoadFailed(status) => Some((status.url.0.as_str().to_owned(), status.code.0, status.description.clone())),
        _ => None,
    });
    let current = view.read().clone();
    let controlled = current.yours();
    let zoom = snapshot.as_ref().map_or(logic::DEFAULT_ZOOM, logic::zoom_of);
    let appearance = snapshot.as_ref().map_or("system", logic::appearance_of);
    let fill = snapshot.as_ref().is_none_or(logic::is_fill);
    let profile = snapshot.as_ref().map(logic::profile_of);
    let default_profile = open_defaults(state).1;
    let address = logic::address(snapshot.as_ref());
    let loading = logic::is_loading(snapshot.as_ref()) || (snapshot.is_some() && !empty && failed.is_none() && !current.first_frame && !current.gone && current.host_setup.is_none() && !current.denied);
    let can_back = snapshot.as_ref().is_some_and(|snapshot| snapshot.can_go_back) && controlled;
    let can_forward = snapshot.as_ref().is_some_and(|snapshot| snapshot.can_go_forward) && controlled;
    let input_disabled = snapshot.is_some() && !controlled;
    let shown = draft.read().clone().unwrap_or_else(|| address.clone());
    let go = {
        let (transport, key, env, thread, link) = (transport.clone(), key.clone(), environment_key.clone(), thread_id.clone(), link.clone());
        let has_tab = tab_id.is_some();
        use_callback(move |raw: String| match logic::normalize_url(&raw) {
            Err(error) => address_error.set(Some(error.message())),
            Ok(url) => {
                address_error.set(None);
                if has_tab {
                    if link.send(logic::input::navigate(&url)) {
                        edit_thread(state, &key, |sessions| sessions.remember_url(&url));
                    }
                } else {
                    let (transport, env, thread) = (transport.clone(), env.clone(), thread.clone());
                    spawn(async move {
                        if let Err(message) = open_session(transport, state, env, thread, Some(url)).await {
                            notice::push(state, Level::Error, "Unable to open browser", message);
                        }
                    });
                }
            }
        })
    };
    let adjust = {
        let (transport, key, thread, tab_id) = (transport.clone(), key.clone(), thread_id.clone(), tab_id.clone());
        use_callback(move |change: Value| {
            let Some(tab) = tab_id.clone() else { return };
            let (transport, key, thread) = (transport.clone(), key.clone(), thread.clone());
            spawn(async move {
                match runtime::request_value_reported(transport, state, "preview.adjust", logic::adjust_input(&thread, &tab, change)).await {
                    Ok(value) => {
                        if let Ok(snapshot) = serde_json::from_value::<PreviewSessionSnapshot>(value) {
                            edit_thread(state, &key, |sessions| sessions.update_snapshot(&snapshot));
                        }
                    }
                    Err(message) => notice::push(state, Level::Error, "Unable to change the browser tab", message),
                }
            });
        })
    };
    let resize = {
        let (transport, key, thread, tab_id) = (transport.clone(), key.clone(), thread_id.clone(), tab_id.clone());
        use_callback(move |viewport: Value| {
            let Some(tab) = tab_id.clone() else { return };
            let (transport, key, thread) = (transport.clone(), key.clone(), thread.clone());
            spawn(async move {
                match runtime::request_value_reported(transport, state, "preview.resize", json!({"threadId": thread, "tabId": tab, "viewport": viewport})).await {
                    Ok(value) => {
                        if let Ok(snapshot) = serde_json::from_value::<PreviewSessionSnapshot>(value) {
                            edit_thread(state, &key, |sessions| sessions.update_snapshot(&snapshot));
                        }
                    }
                    Err(message) => notice::push(state, Level::Error, "Unable to resize browser viewport", message),
                }
            });
        })
    };
    // What a shortcut asked: each ask is done once, by the tab that is shown.
    let mut seen = use_signal(|| state.previews().peek().action.map_or(0, |(count, _)| count));
    use_effect({
        let (adjust, link) = (adjust, link.clone());
        move || {
            let Some((count, action)) = state.previews().read().action else { return };
            if count == *seen.peek() {
                return;
            }
            seen.set(count);
            match action {
                Action::Refresh => {
                    link.send(logic::input::reload(false));
                }
                Action::FocusUrl => {
                    let _ = document::eval("const input=document.querySelector('[data-preview-url-input]');if(input){input.focus();input.select();}");
                }
                Action::ZoomIn | Action::ZoomOut | Action::ResetZoom => {
                    let direction = match action {
                        Action::ZoomIn => 1,
                        Action::ZoomOut => -1,
                        _ => 0,
                    };
                    if let Some(next) = logic::step_zoom(zoom, direction) {
                        adjust.call(json!({"zoomFactor": next}));
                    }
                }
            }
        }
    });
    let (back, forward, reload) = (link.clone(), link.clone(), link.clone());
    rsx! {
        div { class: "preview-panel", "data-thread-key": "{key}",
            form { class: "preview-chrome", onsubmit: move |event: FormEvent| event.prevent_default(),
                div { class: "preview-nav", role: "group", "aria-label": "Navigation",
                    button { class: "icon-button", r#type: "button", "aria-label": "Back", title: "Back", disabled: !can_back, onclick: move |_| { back.send(logic::input::history(-1)); }, "\u{2190}" }
                    button { class: "icon-button", r#type: "button", "aria-label": "Forward", title: "Forward", disabled: !can_forward, onclick: move |_| { forward.send(logic::input::history(1)); }, "\u{2192}" }
                    button { class: "icon-button", r#type: "button", "aria-label": if loading { "Stop" } else { "Refresh" }, title: if loading { "Loading\u{2026}" } else { "Refresh" }, disabled: empty || !controlled,
                        onclick: move |_| { reload.send(logic::input::reload(false)); },
                        if loading { "\u{23f3}" } else { "\u{21bb}" } }
                }
                if let Some(profile) = profile.as_ref().filter(|profile| **profile != default_profile) {
                    span { class: "preview-profile", title: "{profile}", "{profile}" }
                }
                input { class: "preview-address", "aria-label": "Search or enter URL", "data-preview-url-input": "", placeholder: "Search or enter URL", spellcheck: "false", disabled: input_disabled, value: "{shown}",
                    onfocus: { let address = address.clone(); move |_| draft.set(Some(address.clone())) },
                    oninput: move |event| draft.set(Some(event.value())),
                    onblur: move |_| draft.set(None),
                    onkeydown: { let address = address.clone(); move |event| {
                        if event.key() == Key::Enter {
                            event.prevent_default();
                            let raw = draft.peek().clone().unwrap_or_else(|| address.clone());
                            if !raw.trim().is_empty() {
                                draft.set(None);
                                go.call(raw.trim().to_owned());
                            }
                        } else if event.key() == Key::Escape {
                            event.prevent_default();
                            draft.set(None);
                        }
                    } } }
                if tab_id.is_some() && !address.is_empty() {
                    button { class: "icon-button", r#type: "button", "aria-label": "Open in system browser", title: "Open in system browser", onclick: { let address = address.clone(); move |_| crate::link_menus::open_external(&address) }, "\u{2197}" }
                }
                if tab_id.is_some() {
                    MoreMenu { zoom, appearance, fill, adjust, resize, profile: profile.clone() }
                }
            }
            if let Some(message) = address_error.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
            div { class: "preview-body",
                if let Some(tab) = tab_id.clone() {
                    Viewer { key: "{tab}", state, transport: transport.clone(), environment_key: environment_key.clone(), thread_id: thread_id.clone(), tab: tab.clone(), view, link: link.clone(), hidden: empty || failed.is_some(), retry }
                }
                if empty {
                    EmptyState { state, transport: transport.clone(), recents: sessions.recent_urls.clone(), on_open: move |url: String| go.call(url), on_forget: { let key = key.clone(); move |url: String| edit_thread(state, &key, |sessions| sessions.recent_urls.retain(|entry| *entry != url)) } }
                }
                if let Some((url, code, description)) = failed {
                    Unreachable { url, code, description, on_reload: { let link = link.clone(); move |_| { link.send(logic::input::reload(false)); } } }
                }
            }
        }
    }
}

/// `PreviewEmptyState`: the addresses used before and the servers listening on the environment's own machine
/// (`subscribeDiscoveredLocalServers`, `useDiscoveredLocalServers`), or a line that says what to do.
#[component]
fn EmptyState(state: Store<UiModel>, transport: TransportHandle, recents: Vec<String>, on_open: EventHandler<String>, on_forget: EventHandler<String>) -> Element {
    let recents: Vec<String> = recents.into_iter().filter(|url| url::Url::parse(url).is_ok()).take(8).collect();
    let mut servers = use_signal(Vec::<Value>::new);
    // The environment says which servers are up, and again when that changes; the list ends with the tab's empty state.
    use_resource({
        let transport = transport.clone();
        move || {
            let transport = transport.clone();
            async move {
                let owner = runtime::response_owner(&transport, state);
                let Ok(mut stream) = runtime::request_stream_reported(&transport, state, "subscribeDiscoveredLocalServers", json!({})) else { return };
                while let Some(chunk) = stream.next().await {
                    if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                        return;
                    }
                    let Ok(values) = chunk else { return };
                    if let Some(latest) = values.into_iter().last() {
                        servers.set(logic::previewable_servers(&latest));
                    }
                }
            }
        }
    });
    let live = servers.read().clone();
    if recents.is_empty() && live.is_empty() {
        return rsx! { div { class: "preview-empty", h3 { "No preview yet" } p { class: "muted", "Type a URL above, or run a dev script. Browser-ready localhost servers will show up here automatically." } } };
    }
    rsx! {
        div { class: "preview-empty",
            if !recents.is_empty() {
                h3 { "Recently used" }
                div { class: "preview-recents", role: "list",
                    for url in recents {
                        div { key: "{url}", class: "preview-recent", role: "listitem",
                            button { r#type: "button", "aria-label": "Open {url}", onclick: { let url = url.clone(); move |_| on_open.call(url.clone()) }, "{url}" }
                            button { r#type: "button", "aria-label": "Forget {url}", title: "Remove from recent addresses", onclick: { let url = url.clone(); move |_| on_forget.call(url.clone()) }, "\u{00d7}" }
                        }
                    }
                }
            }
            if !live.is_empty() {
                h3 { "Local servers" }
                div { class: "preview-recents", role: "list", "aria-label": "Local servers",
                    for server in live {
                        {
                            let (host, port, url) = (server["host"].as_str().unwrap_or_default().to_owned(), server["port"].as_u64().unwrap_or(0), server["url"].as_str().unwrap_or_default().to_owned());
                            let title = server["processName"].as_str().unwrap_or("Listening").to_owned();
                            rsx! { div { key: "{host}:{port}", class: "preview-recent", role: "listitem",
                                button { r#type: "button", "aria-label": "Open {host}:{port}", title: "{url}", onclick: { let url = url.clone(); move |_| on_open.call(url.clone()) }, "{title} " span { class: "muted", "{host}:{port}" } }
                            } }
                        }
                    }
                }
                p { class: "muted", "Select a live local server to open it in this browser tab." }
            }
        }
    }
}

/// `PreviewUnreachable`: Chromium's "This site can't be reached", in the app's words.
#[component]
fn Unreachable(url: String, code: i64, description: String, on_reload: EventHandler<()>) -> Element {
    let mut details = use_signal(|| false);
    let host = url::Url::parse(&url).ok().and_then(|parsed| parsed.host_str().map(|host| match parsed.port() { Some(port) => format!("{host}:{port}"), None => host.to_owned() })).unwrap_or_else(|| url.clone());
    let friendly = logic::describe_error(&description);
    let label = logic::error_label(&description, code);
    rsx! {
        div { class: "preview-unreachable", role: "alert",
            h2 { "This site can\u{2019}t be reached" }
            p { strong { "{host}" } ": {friendly}." }
            if details() {
                div { class: "preview-details", p { "Try:" } ul { li { "Checking your connection" } li { "Confirming the dev server is running" } li { "Checking the proxy and the firewall" } } }
            }
            small { class: "preview-error-label", "{label}" }
            div { class: "preview-unreachable-actions",
                button { r#type: "button", "aria-label": if details() { "Hide details" } else { "Details" }, onclick: move |_| details.set(!details()), if details() { "Hide details" } else { "Details" } }
                button { r#type: "button", "aria-label": "Reload page", onclick: move |_| on_reload.call(()), "Reload" }
            }
        }
    }
}

/// `PreviewMoreMenu`: the three-dot menu of the chrome row. Everything in it is the environment's, so every client sees one state.
#[component]
fn MoreMenu(zoom: f64, appearance: &'static str, fill: bool, adjust: Callback<Value>, resize: Callback<Value>, profile: Option<String>) -> Element {
    let mut open = use_signal(|| false);
    let percent = (zoom * 100.0).round() as i64;
    let profile_name = profile.unwrap_or_else(|| "default".into());
    rsx! {
        div { class: "preview-more",
            button { class: "icon-button", r#type: "button", "aria-label": "Preview menu", "aria-haspopup": "menu", "aria-expanded": "{open()}", title: "More", onclick: move |_| open.set(!open()), "\u{22ee}" }
            if open() {
                div { class: "menu-backdrop", onclick: move |_| open.set(false) }
                div { class: "context-menu preview-menu", role: "menu", "aria-label": "Preview menu",
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
                    button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "Hard reload", onclick: move |_| { open.set(false); adjust.call(json!({"hardReload": true})); }, "Hard reload" }
                    button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": if fill { "Show device toolbar" } else { "Hide device toolbar" }, onclick: move |_| { open.set(false); resize.call(if fill { json!({"_tag": "freeform", "width": 1024, "height": 768}) } else { json!({"_tag": "fill"}) }); }, if fill { "Show device toolbar" } else { "Hide device toolbar" } }
                    div { class: "menu-label", "Appearance" }
                    for (value, label) in [("system", "System"), ("light", "Light"), ("dark", "Dark")] {
                        button { key: "{value}", class: "menu-item", role: "menuitemradio", "aria-label": "Appearance {label}", "aria-checked": "{appearance == value}", r#type: "button", onclick: move |_| { open.set(false); adjust.call(json!({"colorScheme": value})); }, "{label}" }
                    }
                    div { class: "menu-zoom", role: "group", "aria-label": "Zoom",
                        span { "Zoom" }
                        button { class: "icon-button", r#type: "button", "aria-label": "Zoom out", onclick: move |_| { if let Some(next) = logic::step_zoom(zoom, -1) { adjust.call(json!({"zoomFactor": next})); } }, "\u{2212}" }
                        span { class: "muted", role: "status", "aria-label": "Zoom level", "{percent}%" }
                        button { class: "icon-button", r#type: "button", "aria-label": "Zoom in", onclick: move |_| { if let Some(next) = logic::step_zoom(zoom, 1) { adjust.call(json!({"zoomFactor": next})); } }, "+" }
                        button { class: "icon-button", r#type: "button", "aria-label": "Reset zoom", onclick: move |_| { if let Some(next) = logic::step_zoom(zoom, 0) { adjust.call(json!({"zoomFactor": next})); } }, "\u{21ba}" }
                    }
                    div { class: "menu-label", "Profile: {profile_name}" }
                    button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "Clear cookies", onclick: move |_| { open.set(false); adjust.call(json!({"clear": "cookies"})); }, "Clear cookies" }
                    button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "Clear cache", onclick: move |_| { open.set(false); adjust.call(json!({"clear": "cache"})); }, "Clear cache" }
                }
            }
        }
    }
}

/// The title of a browser tab in the tab bar, from the session it shows.
pub(crate) fn title_of(state: Store<UiModel>, key: &str, resource: Option<&str>) -> String {
    let sessions = thread_of(state, key);
    logic::tab_title(resource.and_then(|tab| sessions.sessions.get(tab)))
}
