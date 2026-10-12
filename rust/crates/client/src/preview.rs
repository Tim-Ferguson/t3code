//! The browser tab of the right panel, as far as it is logic: the per-thread index of the server's preview sessions
//! (`previewStateStore.ts`), the URL a person typed (`packages/shared/src/preview.ts`), the messages of the server browser's
//! live stream and what its close codes mean (`serverBrowserStream.ts`), and the small rules of the chrome row and its menu
//! (`PreviewView.tsx`, `PreviewMoreMenu.tsx`, `errorCodeMessages.ts`).
//!
//! Only server tabs exist here: the original's other tab kind runs in an Electron webview this app has no counterpart for.
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use t3_contracts::{Present, PreviewEvent, PreviewListResult, PreviewNavStatus, PreviewSessionSnapshot, PreviewStreamHostSetup, PreviewStreamHostSetupNeed};

/// Cap for the per-thread "recently seen" URL list shown in the empty state (`PREVIEW_RECENT_URL_LIMIT`).
pub const RECENT_URL_LIMIT: usize = 10;

// ---- the URL a person typed --------------------------------------------------------------------------------------------

/// Why a typed address was refused (`PreviewUrlNormalizationError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlError {
    pub input_length: usize,
    pub reason: &'static str,
    pub protocol: Option<String>,
}
impl UrlError {
    pub fn message(&self) -> String {
        let protocol = self.protocol.as_ref().map(|protocol| format!(": {protocol}")).unwrap_or_default();
        format!("Invalid preview URL ({}{protocol}; input length {}).", self.reason, self.input_length)
    }
}

/// `normalizePreviewUrl`: a free-form address as an `http(s)` URL. A bare loopback host gets `http://`, any other bare host
/// `https://`; an address that has a scheme is only checked.
pub fn normalize_url(raw: &str) -> Result<String, UrlError> {
    let input_length = raw.encode_utf16().count();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(UrlError { input_length, reason: "empty", protocol: None });
    }
    let lower = trimmed.to_ascii_lowercase();
    let loopback = ["localhost", "127.0.0.1", "0.0.0.0", "[::1]", "[::]"].iter().any(|host| lower.strip_prefix(host).is_some_and(|rest| rest.is_empty() || rest.starts_with(':') || rest.starts_with('/')));
    let candidate = if trimmed.contains("://") { trimmed.to_owned() } else { format!("{}://{trimmed}", if loopback { "http" } else { "https" }) };
    let protocol = || {
        let end = candidate.find(':')?;
        let scheme = &candidate[..end];
        (scheme.chars().next()?.is_ascii_alphabetic() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))).then(|| format!("{}:", scheme.to_ascii_lowercase()))
    };
    let parsed = url::Url::parse(&candidate).map_err(|_| UrlError { input_length, reason: "parse", protocol: protocol() })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(UrlError { input_length, reason: "unsupported-protocol", protocol: Some(format!("{}:", parsed.scheme())) });
    }
    Ok(parsed.to_string())
}

// ---- the error page ----------------------------------------------------------------------------------------------------

/// `describePreviewError`: Chromium's error name as a short sentence, else the name itself, else a generic line.
pub fn describe_error(description: &str) -> String {
    let friendly = match description {
        "ERR_NAME_NOT_RESOLVED" | "ERR_NAME_RESOLUTION_FAILED" => "DNS address could not be found",
        "ERR_CONNECTION_REFUSED" => "Connection refused",
        "ERR_CONNECTION_RESET" => "Connection was reset",
        "ERR_CONNECTION_CLOSED" => "Connection was closed",
        "ERR_CONNECTION_TIMED_OUT" | "ERR_TIMED_OUT" => "Connection timed out",
        "ERR_INTERNET_DISCONNECTED" => "No internet connection",
        "ERR_CERT_AUTHORITY_INVALID" => "Certificate authority is not trusted",
        "ERR_CERT_COMMON_NAME_INVALID" => "Certificate hostname mismatch",
        "ERR_CERT_DATE_INVALID" => "Certificate is expired or not yet valid",
        "ERR_TOO_MANY_REDIRECTS" => "Too many redirects",
        _ => "",
    };
    if !friendly.is_empty() {
        friendly.to_owned()
    } else if !description.is_empty() {
        description.to_owned()
    } else {
        "Network error".to_owned()
    }
}

/// The small label under the error page's message: Chromium's name, else its numeric code.
pub fn error_label(description: &str, code: i64) -> String {
    if description.is_empty() { format!("ERR_{}", if code == 0 { "FAILED".to_owned() } else { code.abs().to_string() }) } else { description.to_owned() }
}

// ---- the session index -------------------------------------------------------------------------------------------------

/// What one thread knows of its preview sessions (`ThreadPreviewState`, minus the desktop webview's overlay).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadPreviewState {
    pub sessions: BTreeMap<String, PreviewSessionSnapshot>,
    /// Tabs this client closed on purpose: a list read before the close landed must not bring them back.
    pub suppressed: BTreeSet<String>,
    pub active_tab: Option<String>,
    pub recent_urls: Vec<String>,
    /// Whether the first authoritative list has arrived.
    pub list_loaded: bool,
    /// The server process whose revisions are ordered against each other.
    pub server_epoch: Option<String>,
    pub server_revision: u64,
}

fn url_of(snapshot: &PreviewSessionSnapshot) -> Option<String> {
    match &snapshot.nav_status {
        PreviewNavStatus::Idle(_) => None,
        PreviewNavStatus::Loading(status) => Some(status.url.0.as_str().to_owned()),
        PreviewNavStatus::Success(status) => Some(status.url.0.as_str().to_owned()),
        PreviewNavStatus::LoadFailed(status) => Some(status.url.0.as_str().to_owned()),
    }
}

impl ThreadPreviewState {
    pub fn snapshot(&self) -> Option<&PreviewSessionSnapshot> {
        self.active_tab.as_ref().and_then(|tab| self.sessions.get(tab))
    }
    fn latest(sessions: &BTreeMap<String, PreviewSessionSnapshot>) -> Option<&PreviewSessionSnapshot> {
        // Equal times keep the later one, as the stable sort of the original does.
        sessions.values().fold(None, |best: Option<&PreviewSessionSnapshot>, next| match best {
            Some(best) if best.updated_at > next.updated_at => Some(best),
            _ => Some(next),
        })
    }
    /// `rememberPreviewUrl`.
    pub fn remember_url(&mut self, url: &str) {
        if url.trim().is_empty() {
            return;
        }
        self.recent_urls.retain(|entry| entry != url);
        self.recent_urls.insert(0, url.to_owned());
        self.recent_urls.truncate(RECENT_URL_LIMIT);
    }
    fn remember_snapshot(&mut self, snapshot: &PreviewSessionSnapshot) {
        if let Some(url) = url_of(snapshot) {
            self.remember_url(&url);
        }
    }
    fn remove(&mut self, tab: &str) {
        if self.sessions.remove(tab).is_none() {
            return;
        }
        if self.active_tab.as_deref() == Some(tab) {
            self.active_tab = Self::latest(&self.sessions).map(|snapshot| snapshot.tab_id.as_str().to_owned());
        }
    }
    /// `applyPreviewServerEvent`: an event of another server process, or older than what the index holds, changes nothing.
    pub fn apply_event(&mut self, event: &PreviewEvent) {
        let (epoch, revision) = match event {
            PreviewEvent::Opened(event) => (event.server_epoch.as_str(), event.revision.0),
            PreviewEvent::Navigated(event) => (event.server_epoch.as_str(), event.revision.0),
            PreviewEvent::Resized(event) => (event.server_epoch.as_str(), event.revision.0),
            PreviewEvent::Failed(event) => (event.server_epoch.as_str(), event.revision.0),
            PreviewEvent::Closed(event) => (event.server_epoch.as_str(), event.revision.0),
        };
        if self.server_epoch.as_deref().is_some_and(|current| current != epoch) || revision < self.server_revision {
            return;
        }
        match event {
            PreviewEvent::Opened(event) => self.take_snapshot(&event.snapshot, true),
            PreviewEvent::Navigated(event) => self.take_snapshot(&event.snapshot, false),
            PreviewEvent::Resized(event) => self.take_snapshot(&event.snapshot, false),
            PreviewEvent::Failed(event) => {
                if let Some(existing) = self.sessions.get_mut(event.tab_id.as_str()) {
                    existing.nav_status = PreviewNavStatus::LoadFailed(t3_contracts::PreviewNavLoadFailed {
                        tag: Default::default(),
                        url: event.url.clone(),
                        title: event.title.clone(),
                        code: event.code,
                        description: event.description.clone(),
                    });
                    existing.updated_at = event.created_at.clone();
                }
            }
            PreviewEvent::Closed(event) => {
                self.remove(event.tab_id.as_str());
                self.suppressed.remove(event.tab_id.as_str());
            }
        }
        self.server_epoch = Some(epoch.to_owned());
        self.server_revision = revision;
    }
    fn take_snapshot(&mut self, snapshot: &PreviewSessionSnapshot, activate: bool) {
        let tab = snapshot.tab_id.as_str().to_owned();
        if self.suppressed.contains(&tab) {
            return;
        }
        self.remember_snapshot(snapshot);
        self.sessions.insert(tab.clone(), snapshot.clone());
        if activate || self.active_tab.is_none() {
            self.active_tab = Some(tab);
        }
    }
    /// `applyPreviewServerSnapshot`: a tab this client just opened is the one it looks at.
    pub fn apply_snapshot(&mut self, snapshot: &PreviewSessionSnapshot) {
        let tab = snapshot.tab_id.as_str().to_owned();
        if self.suppressed.contains(&tab) || self.sessions.get(&tab).is_some_and(|existing| existing.updated_at > snapshot.updated_at) {
            return;
        }
        self.remember_snapshot(snapshot);
        self.sessions.insert(tab.clone(), snapshot.clone());
        self.active_tab = Some(tab);
    }
    /// `updatePreviewServerSnapshot`: the answer to a change of a tab is authoritative for it and does not move the view.
    pub fn update_snapshot(&mut self, snapshot: &PreviewSessionSnapshot) {
        let tab = snapshot.tab_id.as_str().to_owned();
        if self.suppressed.contains(&tab) || self.sessions.get(&tab).is_some_and(|existing| existing.updated_at > snapshot.updated_at) {
            return;
        }
        self.remember_snapshot(snapshot);
        self.sessions.insert(tab.clone(), snapshot.clone());
        if !self.active_tab.as_ref().is_some_and(|active| self.sessions.contains_key(active)) {
            self.active_tab = Some(tab);
        }
    }
    /// `reconcilePreviewServerSessions`: the list is the truth; the shown tab stays while it is in it.
    pub fn reconcile(&mut self, result: &PreviewListResult) {
        let epoch = result.server_epoch.as_str();
        let same_server = self.server_epoch.as_deref() == Some(epoch);
        if same_server && result.revision.0 < self.server_revision {
            return;
        }
        let suppressed = if same_server { self.suppressed.clone() } else { BTreeSet::new() };
        let mut sessions = BTreeMap::new();
        for snapshot in &result.sessions {
            let tab = snapshot.tab_id.as_str();
            if suppressed.contains(tab) {
                continue;
            }
            let existing = if same_server { self.sessions.get(tab) } else { None };
            let next = match existing {
                Some(existing) if existing.updated_at > snapshot.updated_at => existing.clone(),
                _ => snapshot.clone(),
            };
            self.remember_snapshot(&next);
            sessions.insert(tab.to_owned(), next);
        }
        let fallback = Self::latest(&sessions).map(|snapshot| snapshot.tab_id.as_str().to_owned());
        self.active_tab = match &self.active_tab {
            Some(active) if sessions.contains_key(active) => Some(active.clone()),
            _ => fallback,
        };
        self.suppressed = suppressed.into_iter().filter(|tab| result.sessions.iter().any(|snapshot| snapshot.tab_id.as_str() == tab)).collect();
        self.sessions = sessions;
        self.list_loaded = true;
        self.server_epoch = Some(epoch.to_owned());
        self.server_revision = result.revision.0;
    }
    /// `beginPreviewSessionClose`: the tab goes now, and a list that still has it does not bring it back.
    pub fn begin_close(&mut self, tab: &str) {
        self.remove(tab);
        self.suppressed.insert(tab.to_owned());
    }
    /// `cancelPreviewSessionClose`: the close failed, so the tab is back.
    pub fn cancel_close(&mut self, snapshot: Option<&PreviewSessionSnapshot>, tab: &str) {
        if !self.suppressed.remove(tab) {
            return;
        }
        if let Some(snapshot) = snapshot {
            self.remember_snapshot(snapshot);
            self.sessions.insert(snapshot.tab_id.as_str().to_owned(), snapshot.clone());
            self.active_tab = Some(snapshot.tab_id.as_str().to_owned());
        }
    }
    /// `setActivePreviewTab`.
    pub fn set_active(&mut self, tab: &str) {
        if self.sessions.contains_key(tab) {
            self.active_tab = Some(tab.to_owned());
        }
    }
    /// The tabs that are not closing, newest list order, for the panel to hold a surface for each.
    pub fn tab_ids(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }
}

// ---- the chrome row ----------------------------------------------------------------------------------------------------

/// `shouldShowPreviewEmptyState`: no session, or one that has not been given an address.
pub fn show_empty_state(snapshot: Option<&PreviewSessionSnapshot>) -> bool {
    snapshot.is_none_or(|snapshot| matches!(snapshot.nav_status, PreviewNavStatus::Idle(_)))
}

/// The tab's title in the tab bar (`surfaceTitle`, preview case): the page's title, else its host, else "Browser".
pub fn tab_title(snapshot: Option<&PreviewSessionSnapshot>) -> String {
    let Some(snapshot) = snapshot else { return "Browser".into() };
    if matches!(snapshot.nav_status, PreviewNavStatus::Idle(_)) {
        return "Browser".into();
    }
    let title = snapshot.nav_status.title();
    if !title.trim().is_empty() {
        return title.to_owned();
    }
    url_of(snapshot).and_then(|url| url::Url::parse(&url).ok()).and_then(|url| url.host_str().filter(|host| !host.is_empty()).map(|host| match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })).unwrap_or_else(|| "Browser".into())
}

/// The address the chrome row shows: empty until the tab has one.
pub fn address(snapshot: Option<&PreviewSessionSnapshot>) -> String {
    snapshot.and_then(url_of).unwrap_or_default()
}

pub fn is_loading(snapshot: Option<&PreviewSessionSnapshot>) -> bool {
    snapshot.is_some_and(|snapshot| matches!(snapshot.nav_status, PreviewNavStatus::Loading(_)))
}

/// `PREVIEW_ZOOM_LEVELS`, and the level the original starts at (index 7).
pub const ZOOM_LEVELS: [f64; 17] = [0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0];
pub const DEFAULT_ZOOM: f64 = 1.0;

/// `stepServerZoom`: the next level up or down, or the default; `None` when nothing changes.
pub fn step_zoom(current: f64, direction: i32) -> Option<f64> {
    let index = ZOOM_LEVELS.iter().position(|level| *level == current);
    let next = if direction == 0 { DEFAULT_ZOOM } else { ZOOM_LEVELS[((index.unwrap_or(7) as i32 + direction).clamp(0, ZOOM_LEVELS.len() as i32 - 1)) as usize] };
    (next != current).then_some(next)
}

/// What a new tab is changed to so it starts at the person's default zoom and appearance (`browserDefaultTabState`): only what differs
/// from a tab's own start (100%, the system's scheme), as a `preview.adjust` change; none when the defaults are those.
pub fn default_tab_change(zoom_factor: f64, appearance: &str) -> Option<Value> {
    let mut change = serde_json::Map::new();
    if zoom_factor != DEFAULT_ZOOM && ZOOM_LEVELS.contains(&zoom_factor) {
        change.insert("zoomFactor".into(), json!(zoom_factor));
    }
    if matches!(appearance, "light" | "dark") {
        change.insert("colorScheme".into(), json!(appearance));
    }
    (!change.is_empty()).then(|| Value::Object(change))
}

/// The servers a snapshot of `subscribeDiscoveredLocalServers` lists, as `useDiscoveredLocalServers` keeps them: web servers on a loopback
/// host, by port. (A configured address that the scanner also found would come first; this app configures none.)
pub fn previewable_servers(snapshot: &Value) -> Vec<Value> {
    let mut servers: Vec<Value> = snapshot["servers"].as_array().into_iter().flatten().filter(|server| server["url"].as_str().is_some_and(|url| !url.is_empty()) && server["port"].is_u64()).cloned().collect();
    servers.sort_by_key(|server| server["port"].as_u64());
    servers
}

/// `resolveLinkTarget`: whether a click on a link in a message opens it in the app's browser instead of the system's: the person asked
/// for it, a tab can be had, the link is a web page, and neither Command nor Control is held (the way out when the default is the app).
pub fn opens_in_app(prefers_app: bool, can_open_in_app: bool, url: &str, meta_or_control: bool) -> bool {
    if meta_or_control || !prefers_app || !can_open_in_app {
        return false;
    }
    url::Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// The tab's zoom, appearance and viewport as the session reports them (older servers send none: 1, system, fill).
pub fn zoom_of(snapshot: &PreviewSessionSnapshot) -> f64 {
    snapshot.zoom_factor.present().map_or(DEFAULT_ZOOM, |zoom| zoom.0)
}
pub fn appearance_of(snapshot: &PreviewSessionSnapshot) -> &'static str {
    use t3_contracts::PreviewAppearancePreference as Preference;
    match snapshot.color_scheme.present() {
        Some(Preference::Light) => "light",
        Some(Preference::Dark) => "dark",
        _ => "system",
    }
}
pub fn is_fill(snapshot: &PreviewSessionSnapshot) -> bool {
    snapshot.viewport.present().is_none_or(|viewport| matches!(viewport, t3_contracts::PreviewViewportSetting::Fill))
}
/// The profile the tab runs in; a tab from before profiles ran in the built-in default.
pub fn profile_of(snapshot: &PreviewSessionSnapshot) -> String {
    snapshot.profile_id.present().map(|profile| profile.as_str().to_owned()).unwrap_or_else(|| "default".to_owned())
}

// ---- requests ----------------------------------------------------------------------------------------------------------

/// `preview.open` for a new tab: an empty one the person types into, or one at `url`. The viewport and profile are the
/// configured defaults (`openPreviewSession`), and the server runs it (`runtime: "server"`).
pub fn open_input(thread_id: &str, url: Option<&str>, viewport: Value, profile_id: &str) -> Value {
    let mut input = json!({"threadId": thread_id, "viewport": viewport, "profileId": profile_id, "runtime": "server"});
    if let Some(url) = url {
        input["url"] = json!(url);
    }
    input
}
pub fn adjust_input(thread_id: &str, tab_id: &str, change: Value) -> Value {
    let mut input = json!({"threadId": thread_id, "tabId": tab_id});
    for (key, value) in change.as_object().into_iter().flatten() {
        input[key] = value.clone();
    }
    input
}
pub fn close_input(thread_id: &str, tab_id: &str) -> Value {
    json!({"threadId": thread_id, "tabId": tab_id})
}

// ---- the live stream ---------------------------------------------------------------------------------------------------

/// Who drives the page (`PreviewStreamControl.controller`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Controller {
    Agent,
    You,
    AnotherViewer,
    Unclaimed,
}
/// A JavaScript dialog the page has open, waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    pub kind: String,
    pub message: String,
    pub default_value: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub can_operate: bool,
    pub controller: Controller,
    pub generation: u64,
    pub dialog: Option<Dialog>,
}
impl Control {
    pub fn is_yours(&self) -> bool {
        self.can_operate && self.controller == Controller::You
    }
}

/// `previewStreamControlLabel`: the status line above a viewer; no control yet means the socket is not connected.
pub fn control_label(control: Option<&Control>) -> &'static str {
    match control {
        None => "Connecting...",
        Some(control) if !control.can_operate => "Read-only",
        Some(control) => match control.controller {
            Controller::You => "You have control",
            Controller::Agent => "Agent has control",
            Controller::AnotherViewer => "Another viewer has control",
            Controller::Unclaimed => "Watching",
        },
    }
}

/// A file picker the page opened (`PreviewStreamFileChooser`, minus the URL the files go to).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChooser {
    pub id: String,
    pub multiple: bool,
    pub accept: String,
}

/// What the server tells a viewer in text (the binary messages are frames).
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Viewport { width: f64, height: f64 },
    Control(Control),
    Probe { x: f64, y: f64, editable: bool },
    Clipboard(String),
    Pointer { phase: String, x: f64, y: f64, sequence: u64 },
    FileChooser(FileChooser),
    FileChooserClosed(String),
    Download { id: String, file_name: String, size_bytes: u64 },
}

/// One server message; one that is not what the stream sends is ignored, as the original's checks do.
pub fn parse_message(text: &str) -> Option<Message> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    let string = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |key: &str| object.get(key).and_then(Value::as_f64);
    let flag = |key: &str| object.get(key).and_then(Value::as_bool);
    match object.get("type")?.as_str()? {
        "fileChooser" => Some(Message::FileChooser(FileChooser { id: string("id")?, multiple: flag("multiple")?, accept: string("accept")? })),
        "fileChooserClosed" => Some(Message::FileChooserClosed(string("id")?)),
        "pointer" => {
            let phase = string("phase").filter(|phase| matches!(phase.as_str(), "move" | "click"))?;
            Some(Message::Pointer { phase, x: number("x")?, y: number("y")?, sequence: object.get("sequence")?.as_u64()? })
        }
        "clipboard" => Some(Message::Clipboard(string("text")?)),
        "download" => Some(Message::Download { id: string("id")?, file_name: string("fileName")?, size_bytes: object.get("sizeBytes")?.as_u64()? }),
        "viewport" => Some(Message::Viewport { width: number("width")?, height: number("height")? }),
        "probe" => Some(Message::Probe { x: number("x")?, y: number("y")?, editable: flag("editable")? }),
        "control" => {
            let controller = match object.get("controller")?.as_str()? {
                "agent" => Controller::Agent,
                "you" => Controller::You,
                "another-viewer" => Controller::AnotherViewer,
                "unclaimed" => Controller::Unclaimed,
                _ => return None,
            };
            let dialog = match object.get("dialog")? {
                Value::Null => None,
                Value::Object(dialog) => Some(Dialog {
                    kind: dialog.get("type")?.as_str()?.to_owned(),
                    message: dialog.get("message")?.as_str()?.to_owned(),
                    default_value: dialog.get("defaultValue")?.as_str()?.to_owned(),
                }),
                _ => return None,
            };
            Some(Message::Control(Control { can_operate: flag("canOperate")?, controller, generation: object.get("generation")?.as_u64()?, dialog }))
        }
        _ => None,
    }
}

/// What a closed socket means for the viewer (`createPreviewStreamClient`'s close handler).
#[derive(Debug, Clone, PartialEq)]
pub enum Closed {
    /// 4404: the tab was closed on the server; the viewer stops.
    Gone,
    /// 4503: the server's host cannot start its browser until it is set up; the viewer stops.
    HostSetup(PreviewStreamHostSetup),
    /// The upgrade was refused (1008, 4401, or a socket that never opened): the access is asked for again.
    Unauthorized,
    /// Anything else, including 1012: try again after a pause.
    Retry,
}
pub const TAB_GONE_CODE: u16 = 4404;
pub const HOST_SETUP_CODE: u16 = 4503;
pub fn closed(code: u16, reason: &str, opened: bool) -> Closed {
    match code {
        TAB_GONE_CODE => Closed::Gone,
        HOST_SETUP_CODE => Closed::HostSetup(host_setup(reason)),
        1008 | 4401 => Closed::Unauthorized,
        1006 if !opened => Closed::Unauthorized,
        _ => Closed::Retry,
    }
}
/// `decodeHostSetup`: a reason that cannot be read still stops the viewer and offers the usual command.
pub fn host_setup(reason: &str) -> PreviewStreamHostSetup {
    serde_json::from_str::<PreviewStreamHostSetup>(reason).unwrap_or_else(|_| PreviewStreamHostSetup { need: PreviewStreamHostSetupNeed::Sandbox, command: "sudo npx t3 browser setup".into() })
}
/// `previewStreamHostSetupMessage`: what the viewer tells the person; the command is shown beside it, ready to copy.
pub fn host_setup_message(setup: &PreviewStreamHostSetup) -> &'static str {
    match setup.need {
        PreviewStreamHostSetupNeed::Sandbox => "This server's host blocks the sandbox its browser runs in. Run this once on the host, then try again:",
        PreviewStreamHostSetupNeed::Libraries => "This server's host is missing libraries its browser needs. Run this once on the host, then try again:",
    }
}
/// `500 * 2 ** failures`, at most ten seconds.
pub fn retry_delay_ms(failures: u32) -> u64 {
    (500u64.saturating_mul(1u64 << failures.min(10))).min(10_000)
}

/// The stream's address (`createPreviewStreamClient`): `maxWidth` and `maxHeight` are the frame cap in device pixels.
pub fn stream_query(thread_id: &str, tab_id: &str, max_width: f64, max_height: f64, interactive: bool) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("threadId", thread_id).append_pair("tabId", tab_id).append_pair("maxWidth", &(max_width.round().max(1.0) as u64).to_string()).append_pair("maxHeight", &(max_height.round().max(1.0) as u64).to_string());
    if !interactive {
        query.append_pair("interactive", "false");
    }
    query.finish()
}
/// The address of a file a page downloaded (`previewStreamDownloadUrl`).
pub fn download_path(thread_id: &str, tab_id: &str, id: &str) -> String {
    format!("api/preview-stream/download?{}", url::form_urlencoded::Serializer::new(String::new()).append_pair("threadId", thread_id).append_pair("tabId", tab_id).append_pair("id", id).finish())
}
/// Where the files for a page's picker go (`previewStreamUploadUrl`).
pub fn upload_path(thread_id: &str, tab_id: &str, chooser: &str) -> String {
    format!("api/preview-stream/upload?{}", url::form_urlencoded::Serializer::new(String::new()).append_pair("threadId", thread_id).append_pair("tabId", tab_id).append_pair("chooser", chooser).finish())
}

/// The messages a viewer sends that the panel's own controls make (the page's input is the surface's): all of them
/// need the viewer to have control, `takeControl` aside (`createPreviewStreamClient.send`).
pub mod input {
    use serde_json::{Value, json};
    pub fn navigate(url: &str) -> Value {
        json!({"type": "navigate", "url": url})
    }
    pub fn history(delta: i32) -> Value {
        json!({"type": "history", "delta": delta})
    }
    pub fn reload(ignore_cache: bool) -> Value {
        if ignore_cache { json!({"type": "reload", "ignoreCache": true}) } else { json!({"type": "reload"}) }
    }
    pub fn viewport(setting: Value) -> Value {
        json!({"type": "viewport", "setting": setting})
    }
    pub fn take_control() -> Value {
        json!({"type": "takeControl"})
    }
    pub fn release_control() -> Value {
        json!({"type": "releaseControl"})
    }
    pub fn dialog(accept: bool, prompt_text: Option<&str>) -> Value {
        match prompt_text {
            Some(text) => json!({"type": "dialog", "accept": accept, "promptText": text}),
            None => json!({"type": "dialog", "accept": accept}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(tab: &str, url: Option<&str>, updated_at: &str) -> PreviewSessionSnapshot {
        let nav = match url {
            None => json!({"_tag": "Idle"}),
            Some(url) => json!({"_tag": "Success", "url": url, "title": ""}),
        };
        serde_json::from_value(json!({"threadId": "thread", "tabId": tab, "navStatus": nav, "canGoBack": false, "canGoForward": false, "updatedAt": updated_at})).unwrap()
    }
    fn event(kind: &str, epoch: &str, revision: u64, snapshot: &PreviewSessionSnapshot) -> PreviewEvent {
        let mut value = json!({"threadId": "thread", "tabId": snapshot.tab_id.as_str(), "createdAt": snapshot.updated_at, "serverEpoch": epoch, "revision": revision, "type": kind});
        if kind != "closed" {
            value["snapshot"] = serde_json::to_value(snapshot).unwrap();
        }
        serde_json::from_value(value).unwrap()
    }
    fn list(epoch: &str, revision: u64, sessions: &[PreviewSessionSnapshot]) -> PreviewListResult {
        serde_json::from_value(json!({"sessions": sessions, "serverEpoch": epoch, "revision": revision})).unwrap()
    }

    #[test]
    fn a_typed_address_gets_the_scheme_the_original_gives_it() {
        assert_eq!(normalize_url(" localhost:5173 ").unwrap(), "http://localhost:5173/");
        assert_eq!(normalize_url("127.0.0.1/app").unwrap(), "http://127.0.0.1/app");
        assert_eq!(normalize_url("[::1]:8080").unwrap(), "http://[::1]:8080/");
        assert_eq!(normalize_url("example.com").unwrap(), "https://example.com/");
        assert_eq!(normalize_url("localhost.example.com").unwrap(), "https://localhost.example.com/");
        assert_eq!(normalize_url("http://Example.com/a b").unwrap(), "http://example.com/a%20b");
        let empty = normalize_url("   ").unwrap_err();
        assert_eq!((empty.reason, empty.input_length), ("empty", 3));
        assert_eq!(normalize_url("ftp://files.example.com").unwrap_err().message(), "Invalid preview URL (unsupported-protocol: ftp:; input length 23).");
        assert_eq!(normalize_url("http://").unwrap_err().reason, "parse");
        assert_eq!(normalize_url("file:///etc/passwd").unwrap_err().protocol.as_deref(), Some("file:"));
    }

    #[test]
    fn errors_have_friendly_words_and_a_label() {
        assert_eq!(describe_error("ERR_CONNECTION_REFUSED"), "Connection refused");
        assert_eq!(describe_error("ERR_SOMETHING_NEW"), "ERR_SOMETHING_NEW");
        assert_eq!(describe_error(""), "Network error");
        assert_eq!(error_label("", -105), "ERR_105");
        assert_eq!(error_label("", 0), "ERR_FAILED");
        assert_eq!(error_label("ERR_X", -1), "ERR_X");
    }

    #[test]
    fn an_older_event_or_another_servers_changes_nothing_and_an_opened_tab_becomes_the_shown_one() {
        let mut state = ThreadPreviewState::default();
        let a = snapshot("a", Some("http://one.test/"), "2024-01-01T00:00:01Z");
        let b = snapshot("b", None, "2024-01-01T00:00:02Z");
        state.apply_event(&event("opened", "epoch", 5, &a));
        state.apply_event(&event("opened", "epoch", 6, &b));
        assert_eq!((state.active_tab.as_deref(), state.server_revision), (Some("b"), 6));
        assert_eq!(state.recent_urls, ["http://one.test/"], "an empty tab has no address to remember");
        // A navigation does not move the view; a stale or foreign event is ignored.
        let moved = snapshot("a", Some("http://two.test/"), "2024-01-01T00:00:03Z");
        state.apply_event(&event("navigated", "epoch", 7, &moved));
        assert_eq!(state.active_tab.as_deref(), Some("b"));
        assert_eq!(address(state.sessions.get("a")), "http://two.test/");
        state.apply_event(&event("navigated", "epoch", 3, &a));
        state.apply_event(&event("navigated", "other", 99, &a));
        assert_eq!(address(state.sessions.get("a")), "http://two.test/");
        assert_eq!((state.server_epoch.as_deref(), state.server_revision), (Some("epoch"), 7));
        // A failed load keeps the tab and shows the failure.
        let failed: PreviewEvent = serde_json::from_value(json!({"threadId":"thread","tabId":"a","createdAt":"2024-01-01T00:00:09Z","serverEpoch":"epoch","revision":8,"type":"failed","url":"http://two.test/","title":"","code":-102,"description":"ERR_CONNECTION_REFUSED"})).unwrap();
        state.apply_event(&failed);
        assert!(matches!(state.sessions["a"].nav_status, PreviewNavStatus::LoadFailed(_)));
        // Closing the shown tab shows the newest of the others.
        state.apply_event(&event("closed", "epoch", 9, &b));
        assert_eq!(state.active_tab.as_deref(), Some("a"));
    }

    #[test]
    fn a_close_in_flight_is_not_undone_by_a_list_that_still_has_the_tab_and_a_failed_close_brings_it_back() {
        let mut state = ThreadPreviewState::default();
        let (a, b) = (snapshot("a", Some("http://a.test/"), "2024-01-01T00:00:01Z"), snapshot("b", Some("http://b.test/"), "2024-01-01T00:00:02Z"));
        state.reconcile(&list("epoch", 1, &[a.clone(), b.clone()]));
        assert!(state.list_loaded);
        assert_eq!(state.active_tab.as_deref(), Some("b"), "the newest tab is shown when none was");
        state.set_active("a");
        state.begin_close("a");
        assert_eq!(state.active_tab.as_deref(), Some("b"));
        state.reconcile(&list("epoch", 2, &[a.clone(), b.clone()]));
        assert!(!state.sessions.contains_key("a"), "the list read before the close landed");
        assert!(state.suppressed.contains("a"));
        // The server's own event ends the suppression; a list without the tab does as well.
        state.reconcile(&list("epoch", 3, std::slice::from_ref(&b)));
        assert!(state.suppressed.is_empty());
        state.begin_close("b");
        state.cancel_close(Some(&b), "b");
        assert_eq!((state.active_tab.as_deref(), state.suppressed.is_empty()), (Some("b"), true));
        // A list of an older revision of the same server, or of another server, is handled as the original does.
        state.reconcile(&list("epoch", 1, &[]));
        assert!(state.sessions.contains_key("b"));
        state.reconcile(&list("restarted", 1, &[]));
        assert!(state.sessions.is_empty() && state.active_tab.is_none());
    }

    #[test]
    fn titles_addresses_and_the_empty_state_follow_the_session() {
        assert_eq!(tab_title(None), "Browser");
        let idle = snapshot("a", None, "2024-01-01T00:00:01Z");
        assert!(show_empty_state(Some(&idle)) && show_empty_state(None));
        assert_eq!(tab_title(Some(&idle)), "Browser");
        let page = snapshot("a", Some("http://localhost:5173/x"), "2024-01-01T00:00:01Z");
        assert!(!show_empty_state(Some(&page)));
        assert_eq!(tab_title(Some(&page)), "localhost:5173");
        let titled: PreviewSessionSnapshot = serde_json::from_value(json!({"threadId":"thread","tabId":"a","navStatus":{"_tag":"Loading","url":"http://x.test/","title":"Hello"},"canGoBack":false,"canGoForward":false,"updatedAt":"x"})).unwrap();
        assert_eq!((tab_title(Some(&titled)), is_loading(Some(&titled))), ("Hello".to_owned(), true));
        assert_eq!((zoom_of(&page), appearance_of(&page), is_fill(&page), profile_of(&page).as_str()), (1.0, "system", true, "default"));
    }

    #[test]
    fn a_new_tab_is_changed_only_where_the_defaults_differ_from_its_own_start() {
        assert_eq!(default_tab_change(1.0, "system"), None);
        assert_eq!(default_tab_change(1.25, "system"), Some(json!({"zoomFactor": 1.25})));
        assert_eq!(default_tab_change(1.0, "dark"), Some(json!({"colorScheme": "dark"})));
        assert_eq!(default_tab_change(0.5, "light"), Some(json!({"zoomFactor": 0.5, "colorScheme": "light"})));
        assert_eq!(default_tab_change(1.3, "system"), None, "a zoom that is not a level is not sent");
    }

    #[test]
    fn local_servers_are_listed_by_port_and_one_without_an_address_is_left_out() {
        let snapshot = json!({"servers": [
            {"host": "localhost", "port": 5173, "url": "http://localhost:5173", "processName": "node", "pid": 1, "terminal": null},
            {"host": "localhost", "port": 3000, "url": "http://localhost:3000", "processName": null, "pid": null, "terminal": null},
            {"host": "localhost", "port": 9, "url": "", "processName": null, "pid": null, "terminal": null},
        ], "scannedAt": "2026-10-12T00:00:00.000Z"});
        assert_eq!(previewable_servers(&snapshot).iter().map(|server| server["port"].as_u64().unwrap()).collect::<Vec<_>>(), [3000, 5173]);
        assert!(previewable_servers(&json!({})).is_empty());
    }

    #[test]
    fn a_link_opens_in_the_app_only_when_asked_for_possible_a_web_page_and_without_the_way_out() {
        assert!(opens_in_app(true, true, "https://example.com/a", false));
        assert!(!opens_in_app(false, true, "https://example.com/a", false), "the system's browser is the default");
        assert!(!opens_in_app(true, false, "https://example.com/a", false), "no browser tab to be had");
        assert!(!opens_in_app(true, true, "https://example.com/a", true), "Command or Control opens it in the system's browser");
        assert!(!opens_in_app(true, true, "mailto:someone@example.com", false) && !opens_in_app(true, true, "vscode://file/x", false) && !opens_in_app(true, true, "#fragment", false));
    }

    #[test]
    fn zoom_steps_through_the_levels_and_stops_at_the_ends() {
        assert_eq!(step_zoom(1.0, 1), Some(1.1));
        assert_eq!(step_zoom(1.0, -1), Some(0.9));
        assert_eq!(step_zoom(1.5, 0), Some(1.0));
        assert_eq!(step_zoom(1.0, 0), None);
        assert_eq!(step_zoom(5.0, 1), None);
        assert_eq!(step_zoom(0.25, -1), None);
        assert_eq!(step_zoom(1.2, 1), Some(1.1), "a level the table lacks counts from 100%");
    }

    #[test]
    fn server_messages_are_read_as_the_original_reads_them() {
        assert_eq!(parse_message(r#"{"type":"viewport","width":1280,"height":800}"#), Some(Message::Viewport { width: 1280.0, height: 800.0 }));
        let Some(Message::Control(control)) = parse_message(r#"{"type":"control","canOperate":true,"controller":"you","generation":3,"dialog":{"type":"prompt","message":"Name?","defaultValue":"x"}}"#) else { panic!() };
        assert!(control.is_yours());
        assert_eq!(control.dialog, Some(Dialog { kind: "prompt".into(), message: "Name?".into(), default_value: "x".into() }));
        assert_eq!(parse_message(r#"{"type":"control","canOperate":true,"controller":"nobody","generation":3,"dialog":null}"#), None);
        assert_eq!(parse_message(r#"{"type":"download","id":"d","fileName":"a.csv","sizeBytes":12}"#), Some(Message::Download { id: "d".into(), file_name: "a.csv".into(), size_bytes: 12 }));
        assert_eq!(parse_message(r#"{"type":"fileChooser","id":"c","multiple":false,"accept":".png"}"#), Some(Message::FileChooser(FileChooser { id: "c".into(), multiple: false, accept: ".png".into() })));
        assert_eq!(parse_message(r#"{"type":"pointer","phase":"drag","x":1,"y":2,"sequence":1}"#), None);
        assert_eq!(parse_message(r#"{"type":"clipboard","text":"hi"}"#), Some(Message::Clipboard("hi".into())));
        assert_eq!(parse_message("not json"), None);
        assert_eq!(parse_message(r#"{"type":"mystery"}"#), None);
    }

    #[test]
    fn a_closed_stream_stops_retries_or_asks_for_access_again() {
        assert_eq!(closed(4404, "tab closed", true), Closed::Gone);
        assert_eq!(closed(1012, "", true), Closed::Retry);
        assert_eq!(closed(1006, "", true), Closed::Retry, "a socket that dropped after it opened is tried again");
        assert_eq!(closed(1006, "", false), Closed::Unauthorized, "a refused upgrade reads as 1006 before it opens");
        assert_eq!(closed(1008, "", true), Closed::Unauthorized);
        let Closed::HostSetup(setup) = closed(4503, r#"{"need":"libraries","command":"sudo npx t3 browser setup --libs"}"#, true) else { panic!() };
        assert_eq!((setup.need, setup.command.as_str()), (PreviewStreamHostSetupNeed::Libraries, "sudo npx t3 browser setup --libs"));
        let Closed::HostSetup(fallback) = closed(4503, "garbage", true) else { panic!() };
        assert_eq!((fallback.need, fallback.command.as_str()), (PreviewStreamHostSetupNeed::Sandbox, "sudo npx t3 browser setup"));
        assert!(host_setup_message(&fallback).starts_with("This server's host blocks the sandbox"));
        assert_eq!([0, 1, 4, 9].map(retry_delay_ms), [500, 1000, 8000, 10_000]);
    }

    #[test]
    fn the_control_label_says_who_drives_the_page() {
        let control = |can_operate, controller| Control { can_operate, controller, generation: 1, dialog: None };
        assert_eq!(control_label(None), "Connecting...");
        assert_eq!(control_label(Some(&control(false, Controller::You))), "Read-only");
        assert_eq!(control_label(Some(&control(true, Controller::You))), "You have control");
        assert_eq!(control_label(Some(&control(true, Controller::Agent))), "Agent has control");
        assert_eq!(control_label(Some(&control(true, Controller::AnotherViewer))), "Another viewer has control");
        assert_eq!(control_label(Some(&control(true, Controller::Unclaimed))), "Watching");
    }

    #[test]
    fn the_stream_and_its_files_have_the_addresses_the_server_routes() {
        assert_eq!(stream_query("thread-a", "tab_1", 1280.4, 0.2, true), "threadId=thread-a&tabId=tab_1&maxWidth=1280&maxHeight=1");
        assert!(stream_query("t", "x", 10.0, 10.0, false).ends_with("&interactive=false"));
        assert_eq!(download_path("t", "tab", "id 1"), "api/preview-stream/download?threadId=t&tabId=tab&id=id+1");
        assert_eq!(upload_path("t", "tab", "c"), "api/preview-stream/upload?threadId=t&tabId=tab&chooser=c");
        assert_eq!(input::history(-1), json!({"type":"history","delta":-1}));
        assert_eq!(input::reload(true), json!({"type":"reload","ignoreCache":true}));
        assert_eq!(input::dialog(true, Some("x")), json!({"type":"dialog","accept":true,"promptText":"x"}));
        let open = open_input("thread", None, json!({"_tag":"fill"}), "default");
        assert_eq!(open, json!({"threadId":"thread","viewport":{"_tag":"fill"},"profileId":"default","runtime":"server"}));
    }
}
