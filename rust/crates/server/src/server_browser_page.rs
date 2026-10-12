//! One page of the server's browser, driven over the DevTools protocol (apps/server/src/preview/ServerBrowserPage.ts, with what Playwright's
//! `Page` gave it). The original hands a Playwright page to its operations; here a `Page` is a flat session of a `CdpConnection` plus what
//! Playwright kept for it: the page's URL, viewport and ref table, and the page's events, translated to `PageEvent`.
//!
//! What an agent targets is found by `server_browser_runtime.js`, a script installed in every document of the page that stands in for
//! Playwright's injected script (the selector engines, the checks an action waits for, the accessibility snapshot). Its input and pointer
//! operations live in `server_browser_input.rs`.
//!
//! TODO(original: nothing limits where a page may go). The original's browser has no network policy: a page, and the agent driving it, reaches
//! whatever this machine reaches, its loopback and private networks and cloud metadata addresses included (see `server_browser_contexts.rs`).
use crate::{
    cdp::{CdpConnection, CdpError, CdpEvent},
    session_control::BrowserControlInterrupted,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

pub(crate) const MAX_EVALUATION_BYTES: usize = 64_000;
pub(crate) const MAX_VISIBLE_TEXT_LENGTH: usize = 20_000;
const MAX_SCREENSHOT_WIDTH: f64 = 1280.0;
pub(crate) const WAIT_POLL: Duration = Duration::from_millis(100);
pub const DIAGNOSTIC_BUFFER_LIMIT: usize = 200;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 15_000;
/// The binding a page reports its own clipboard writes through.
pub const CLIPBOARD_BINDING: &str = "__t3PreviewClipboard";
pub const CLIPBOARD_TEXT_LIMIT: usize = 1024 * 1024;
const RENDER_SCALE: f64 = 2.0;

const RUNTIME_SOURCE: &str = include_str!("server_browser_runtime.js");
const CLIPBOARD_SOURCE: &str = include_str!("server_browser_clipboard.js");
pub const EDITABLE_AT_POINT_SCRIPT: &str = include_str!("server_browser_editable.js");
const RECORDER_SOURCE: &str = include_str!("server_browser_recorder.js");

/// A failure of an operation, as the agent's tool reports it: the tag of the typed error and the first line of what went wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct OpError {
    pub tag: String,
    pub message: String,
    pub detail: Option<Value>,
}

pub type OpResult<T> = Result<T, OpError>;

impl OpError {
    pub fn new(tag: &str, message: impl Into<String>) -> Self {
        Self { tag: tag.into(), message: message.into(), detail: None }
    }
    pub fn execution(message: impl Into<String>) -> Self {
        Self::new("PreviewAutomationExecutionError", message)
    }
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new("PreviewAutomationTimeoutError", message)
    }
    pub fn invalid_selector(message: impl Into<String>) -> Self {
        Self::new("PreviewAutomationInvalidSelectorError", message)
    }
    pub fn not_editable(message: impl Into<String>) -> Self {
        Self::new("PreviewAutomationTargetNotEditableError", message)
    }
    pub fn stale_ref() -> Self {
        Self { detail: Some(json!({"staleRef": true})), ..Self::invalid_selector("This element ref is stale or belongs to another tab. Take a fresh snapshot and use its locator.") }
    }
    /// What `toOperationError` makes of a failure that is not already typed: by what its message says.
    pub fn classify(message: &str) -> Self {
        let first = message.lines().next().unwrap_or(message);
        let lower = message.to_lowercase();
        if ["while parsing selector", "unknown engine", "unexpected token", "strict mode violation"].iter().any(|needle| lower.contains(needle)) {
            return Self::invalid_selector(first);
        }
        if ["not an <input>", "not editable", "not an editable"].iter().any(|needle| lower.contains(needle)) {
            return Self::not_editable(first);
        }
        Self::execution(first)
    }
}

impl From<BrowserControlInterrupted> for OpError {
    fn from(error: BrowserControlInterrupted) -> Self {
        Self { tag: "PreviewAutomationControlInterruptedError".into(), message: error.message, detail: Some(json!(error.reason.as_str())) }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for OpError {}

fn net_error_code(description: &str) -> i64 {
    match description {
        "ERR_FAILED" => -2,
        "ERR_TIMED_OUT" => -7,
        "ERR_CONNECTION_CLOSED" => -100,
        "ERR_CONNECTION_RESET" => -101,
        "ERR_CONNECTION_REFUSED" => -102,
        "ERR_NAME_NOT_RESOLVED" => -105,
        "ERR_INTERNET_DISCONNECTED" => -106,
        "ERR_ADDRESS_UNREACHABLE" => -109,
        "ERR_CERT_AUTHORITY_INVALID" => -202,
        "ERR_EMPTY_RESPONSE" => -324,
        _ => -2,
    }
}

/// The `ERR_*` name in a network error, and its code.
pub fn parse_net_error(error_text: &str) -> (String, i64) {
    let description = regex::Regex::new(r"ERR_[A-Z_]+").expect("a valid pattern").find(error_text).map_or(error_text, |found| found.as_str()).to_owned();
    let code = net_error_code(&description);
    (description, code)
}

/// What a page tells the tab that owns it, in the order it happened.
#[derive(Debug, Clone, PartialEq)]
pub enum PageEvent {
    /// A request began; `main` for the navigation of the main frame.
    Request { url: String, main: bool },
    Response { url: String, method: String, status: u16 },
    RequestFailed { url: String, method: String, error_text: String, main: bool },
    Load,
    /// The main frame committed a document or moved within one.
    Navigated,
    Console { level: String, text: String },
    Dialog { kind: String, message: String, default_value: String },
    DialogClosed,
    FileChooser { multiple: bool, backend_node_id: i64 },
    /// Text the page copied (reported through the clipboard binding).
    Clipboard(String),
    Crashed,
    Closed,
}

/// How far a navigation must get before it returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Commit,
    DomContentLoaded,
    Load,
}

#[derive(Default)]
struct Refs {
    generation: String,
    refs: HashMap<String, String>,
}

struct Shared {
    url: String,
    viewport: Option<(u32, u32)>,
    main_frame: String,
    loader_id: Option<String>,
    lifecycle: HashMap<String, HashSet<String>>,
    frames: HashSet<String>,
    requests: HashMap<String, (String, String, bool)>,
    refs: Refs,
    closed: bool,
    /// Data of a drag the page started, which Chromium holds until a drop is dispatched.
    drag: Option<Value>,
}

pub(crate) struct Inner {
    connection: CdpConnection,
    session_id: String,
    target_id: String,
    context_id: Option<String>,
    shared: Mutex<Shared>,
    /// Bumped whenever the main frame commits or reaches a lifecycle milestone.
    progress: watch::Sender<u64>,
    /// Bumped whenever a dialog opens.
    dialogs: watch::Sender<u64>,
    events: mpsc::UnboundedSender<PageEvent>,
}

#[derive(Clone)]
pub struct Page(pub(crate) Arc<Inner>);

// A compact runtime namespace so old refs never alias after a server restart.
fn ref_namespace() -> &'static str {
    static NAMESPACE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAMESPACE.get_or_init(|| uuid::Uuid::new_v4().simple().to_string()[..8].to_owned())
}

fn runtime_key() -> String {
    format!("__t3_{}", ref_namespace())
}

static SNAPSHOT_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_ref_generation() -> String {
    format!("{}-{}", ref_namespace(), radix36(SNAPSHOT_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1))
}

fn radix36(mut value: u64) -> String {
    if value == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(std::char::from_digit((value % 36) as u32, 36).expect("a base 36 digit"));
        value /= 36;
    }
    digits.iter().rev().collect()
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).to_owned()
}

impl Shared {
    fn invalidate_refs(&mut self) {
        self.refs.generation = next_ref_generation();
        self.refs.refs.clear();
    }
}

/// One viewer's or recorder's own session on a page, which carries its own screencast.
pub struct PageSession {
    pub id: String,
    connection: CdpConnection,
}

impl PageSession {
    pub async fn send(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.connection.send(method, params, Some(&self.id)).await
    }
    pub fn post(&self, method: &str, params: Value) {
        self.connection.post(method, params, Some(&self.id));
    }
    pub fn connection(&self) -> &CdpConnection {
        &self.connection
    }
    /// Ends the session; the page stays.
    pub fn detach(&self) {
        self.connection.post("Target.detachFromTarget", json!({"sessionId": self.id}), None);
    }
}

impl Page {
    /// Attaches to a page target and sets it up as the server's browser needs: the domains it reads events from, the helper script in every
    /// document, a focused page, and file picker and drag interception.
    pub async fn attach(connection: &CdpConnection, target_id: &str, context_id: Option<&str>, clipboard: bool) -> OpResult<(Page, mpsc::UnboundedReceiver<PageEvent>)> {
        let attached = connection.send("Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), None).await.map_err(map_cdp)?;
        let session_id = attached["sessionId"].as_str().ok_or_else(|| OpError::execution("The browser did not attach to the page."))?.to_owned();
        let (events, receiver) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            connection: connection.clone(),
            session_id: session_id.clone(),
            target_id: target_id.to_owned(),
            context_id: context_id.map(str::to_owned),
            shared: Mutex::new(Shared {
                url: "about:blank".into(),
                viewport: None,
                main_frame: target_id.to_owned(),
                loader_id: None,
                lifecycle: HashMap::new(),
                frames: HashSet::from([target_id.to_owned()]),
                requests: HashMap::new(),
                refs: Refs { generation: next_ref_generation(), refs: HashMap::new() },
                closed: false,
                drag: None,
            }),
            progress: watch::channel(0).0,
            dialogs: watch::channel(0).0,
            events,
        });
        let page = Page(inner);
        // Weakly, so the connection's callbacks do not keep the page alive.
        let weak = Arc::downgrade(&page.0);
        connection.on_event(Some(&session_id), {
            let weak = weak.clone();
            move |event| {
                if let Some(inner) = weak.upgrade() {
                    Page(inner).handle_event(event);
                }
            }
        });
        let (target, session) = (target_id.to_owned(), session_id.clone());
        connection.on_event(None, move |event| {
            let ends = match event.method.as_str() {
                "Target.targetDestroyed" => event.params["targetId"].as_str() == Some(target.as_str()),
                "Target.detachedFromTarget" => event.params["sessionId"].as_str() == Some(session.as_str()),
                _ => false,
            };
            if ends {
                if let Some(inner) = weak.upgrade() {
                    Page(inner).mark_closed();
                }
            }
        });
        let closing = Arc::downgrade(&page.0);
        let watched = connection.clone();
        tokio::spawn(async move {
            watched.closed().await;
            if let Some(inner) = closing.upgrade() {
                Page(inner).mark_closed();
            }
        });
        for (method, params) in [
            ("Page.enable", json!({})),
            ("Runtime.enable", json!({})),
            ("Network.enable", json!({})),
            ("Page.setLifecycleEventsEnabled", json!({"enabled": true})),
            ("Page.setInterceptFileChooserDialog", json!({"enabled": true})),
            ("Input.setInterceptDrags", json!({"enabled": true})),
            // A page in the background still behaves as the one in front: focus and blur events, `:focus`, and caret.
            ("Emulation.setFocusEmulationEnabled", json!({"enabled": true})),
            ("Page.addScriptToEvaluateOnNewDocument", json!({"source": runtime_install_script(), "runImmediately": true})),
        ] {
            page.send(method, params).await?;
        }
        if clipboard {
            page.send("Runtime.addBinding", json!({"name": CLIPBOARD_BINDING})).await?;
            page.send("Page.addScriptToEvaluateOnNewDocument", json!({"source": clipboard_script(), "runImmediately": true})).await?;
        }
        Ok((page, receiver))
    }

    pub fn session_id(&self) -> &str {
        &self.0.session_id
    }
    pub fn target_id(&self) -> &str {
        &self.0.target_id
    }
    /// The browser context the page lives in (none for a profile's default context).
    pub fn context_id(&self) -> Option<&str> {
        self.0.context_id.as_deref()
    }
    pub fn connection(&self) -> &CdpConnection {
        &self.0.connection
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.0.shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, event: PageEvent) {
        let _ = self.0.events.send(event);
    }

    fn bump(&self) {
        self.0.progress.send_modify(|version| *version += 1);
    }

    fn mark_closed(&self) {
        let first = {
            let mut shared = self.shared();
            !std::mem::replace(&mut shared.closed, true)
        };
        if first {
            self.bump();
            self.emit(PageEvent::Closed);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.shared().closed
    }

    /// Resolves once the page is closed.
    pub async fn closed(&self) {
        let mut progress = self.0.progress.subscribe();
        while !self.is_closed() {
            if progress.changed().await.is_err() {
                return;
            }
        }
    }

    /// The URL of the main frame, as Playwright's `page.url()`.
    pub fn url(&self) -> String {
        self.shared().url.clone()
    }

    /// The viewport last set, in CSS pixels.
    pub fn viewport(&self) -> Option<(u32, u32)> {
        self.shared().viewport
    }

    /// Whether a frame belongs to this page.
    pub fn owns_frame(&self, frame_id: &str) -> bool {
        self.shared().frames.contains(frame_id)
    }

    pub(crate) fn clear_drag(&self) {
        self.shared().drag = None;
    }

    pub(crate) fn take_drag(&self) -> Option<Value> {
        self.shared().drag.take()
    }

    /// A takeover revokes every ref issued before it, even when the document stays unchanged.
    pub fn invalidate_refs(&self) {
        self.shared().invalidate_refs();
    }

    fn handle_event(&self, event: &CdpEvent) {
        let params = &event.params;
        let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
        match event.method.as_str() {
            "Page.frameNavigated" => {
                let frame = &params["frame"];
                let id = text(&frame["id"]);
                let mut shared = self.shared();
                if frame["parentId"].is_null() {
                    shared.url = format!("{}{}", text(&frame["url"]), text(&frame["urlFragment"]));
                    shared.main_frame = id.clone();
                    shared.loader_id = frame["loaderId"].as_str().map(str::to_owned);
                    let loader = shared.loader_id.clone();
                    // Only the committed document's milestones matter; older ones are dropped with their navigation.
                    shared.lifecycle.retain(|key, _| Some(key) == loader.as_ref());
                    shared.frames.retain(|frame| *frame == id);
                }
                shared.frames.insert(id);
                shared.invalidate_refs();
                drop(shared);
                self.bump();
                if frame["parentId"].is_null() {
                    self.emit(PageEvent::Navigated);
                }
            }
            "Page.navigatedWithinDocument" => {
                let mut shared = self.shared();
                if params["frameId"].as_str() == Some(shared.main_frame.as_str()) {
                    shared.url = text(&params["url"]);
                    shared.invalidate_refs();
                    drop(shared);
                    self.bump();
                    self.emit(PageEvent::Navigated);
                }
            }
            "Page.frameAttached" => {
                self.shared().frames.insert(text(&params["frameId"]));
            }
            "Page.frameDetached" => {
                let mut shared = self.shared();
                shared.frames.remove(params["frameId"].as_str().unwrap_or_default());
                shared.invalidate_refs();
            }
            "Page.lifecycleEventFired" => {
                let (frame, loader, name) = (text(&params["frameId"]), text(&params["loaderId"]), text(&params["name"]));
                let mut shared = self.shared();
                if frame != shared.main_frame {
                    return;
                }
                shared.lifecycle.entry(loader).or_default().insert(name.clone());
                drop(shared);
                self.bump();
                if name == "load" {
                    self.emit(PageEvent::Load);
                }
            }
            "Network.requestWillBeSent" => {
                let request = &params["request"];
                let (id, url, method) = (text(&params["requestId"]), text(&request["url"]), text(&request["method"]));
                let main = params["type"].as_str() == Some("Document") && params["frameId"].as_str() == Some(self.shared().main_frame.as_str());
                if let Some(redirect) = params["redirectResponse"].as_object() {
                    let status = redirect.get("status").and_then(Value::as_u64).unwrap_or_default() as u16;
                    let from = redirect.get("url").and_then(Value::as_str).unwrap_or_default().to_owned();
                    self.emit(PageEvent::Response { url: from, method: method.clone(), status });
                }
                self.shared().requests.insert(id, (url.clone(), method, main));
                self.emit(PageEvent::Request { url, main });
            }
            "Network.responseReceived" => {
                let response = &params["response"];
                let method = self.shared().requests.get(params["requestId"].as_str().unwrap_or_default()).map(|(_, method, _)| method.clone()).unwrap_or_else(|| "GET".into());
                self.emit(PageEvent::Response { url: text(&response["url"]), method, status: response["status"].as_u64().unwrap_or_default() as u16 });
            }
            "Network.loadingFailed" => {
                let known = self.shared().requests.remove(params["requestId"].as_str().unwrap_or_default());
                let Some((url, method, main)) = known else { return };
                let error_text = match params["errorText"].as_str() {
                    Some(error) if !error.is_empty() && params["canceled"].as_bool() != Some(true) => error.to_owned(),
                    _ => "net::ERR_ABORTED".to_owned(),
                };
                self.emit(PageEvent::RequestFailed { url, method, error_text, main });
            }
            "Network.loadingFinished" => {
                self.shared().requests.remove(params["requestId"].as_str().unwrap_or_default());
            }
            "Runtime.consoleAPICalled" => {
                let arguments = params["args"].as_array().cloned().unwrap_or_default();
                let texts: Vec<String> = arguments.iter().map(|argument| crate::headless_chrome::remote_object_text(argument).unwrap_or_default()).collect();
                self.emit(PageEvent::Console { level: text(&params["type"]), text: texts.join(" ") });
            }
            "Runtime.bindingCalled" if params["name"].as_str() == Some(CLIPBOARD_BINDING) => {
                if let Some(copied) = params["payload"].as_str() {
                    self.emit(PageEvent::Clipboard(copied.to_owned()));
                }
            }
            "Page.javascriptDialogOpening" => {
                self.0.dialogs.send_modify(|count| *count += 1);
                self.emit(PageEvent::Dialog { kind: text(&params["type"]), message: text(&params["message"]), default_value: text(&params["defaultPrompt"]) });
            }
            "Page.javascriptDialogClosed" => self.emit(PageEvent::DialogClosed),
            "Page.fileChooserOpened" => {
                if let Some(backend_node_id) = params["backendNodeId"].as_i64() {
                    self.emit(PageEvent::FileChooser { multiple: params["mode"].as_str() == Some("selectMultiple"), backend_node_id });
                }
            }
            "Input.dragIntercepted" => {
                self.shared().drag = Some(params["data"].clone());
            }
            "Inspector.targetCrashed" => self.emit(PageEvent::Crashed),
            _ => {}
        }
    }

    // ---- Commands

    /// A command on the page's session.
    pub async fn send(&self, method: &str, params: Value) -> OpResult<Value> {
        self.0.connection.send(method, params, Some(&self.0.session_id)).await.map_err(map_cdp)
    }

    /// A command nobody waits for.
    pub fn post(&self, method: &str, params: Value) {
        self.0.connection.post(method, params, Some(&self.0.session_id));
    }

    /// Another session on this page, for a viewer or a recording.
    pub async fn new_session(&self) -> OpResult<PageSession> {
        let attached = self.0.connection.send("Target.attachToTarget", json!({"targetId": self.0.target_id, "flatten": true}), None).await.map_err(map_cdp)?;
        let id = attached["sessionId"].as_str().ok_or_else(|| OpError::execution("The browser did not attach to the page."))?.to_owned();
        Ok(PageSession { id, connection: self.0.connection.clone() })
    }

    /// Closes the page in the browser.
    pub async fn close(&self) {
        let _ = self.0.connection.send("Target.closeTarget", json!({"targetId": self.0.target_id}), None).await;
        self.mark_closed();
    }

    /// Evaluates an expression in the page's main world and returns its value, or the failure it threw.
    pub(crate) async fn evaluate_value(&self, expression: String, await_promise: bool) -> OpResult<Value> {
        let result = self.send("Runtime.evaluate", json!({"expression": expression, "awaitPromise": await_promise, "returnByValue": true})).await?;
        if let Some(details) = result.get("exceptionDetails") {
            return Err(exception_error(details));
        }
        Ok(result["result"].get("value").cloned().unwrap_or(Value::Null))
    }

    /// Calls a method of the helper script with JSON arguments.
    pub(crate) async fn runtime(&self, method: &str, arguments: &[Value], await_promise: bool) -> OpResult<Value> {
        let arguments: Vec<String> = arguments.iter().map(Value::to_string).collect();
        self.evaluate_value(format!("globalThis[{}].{method}({})", json!(runtime_key()), arguments.join(",")), await_promise).await
    }

    /// The helper script's result for a selector, as a remote object that later calls can act on.
    pub(crate) async fn runtime_object(&self, method: &str, arguments: &[Value]) -> OpResult<Option<String>> {
        let arguments: Vec<String> = arguments.iter().map(Value::to_string).collect();
        let result = self.send("Runtime.evaluate", json!({"expression": format!("globalThis[{}].{method}({})", json!(runtime_key()), arguments.join(","))})).await?;
        if let Some(details) = result.get("exceptionDetails") {
            return Err(exception_error(details));
        }
        Ok(result["result"]["objectId"].as_str().map(str::to_owned))
    }

    pub async fn title(&self) -> String {
        self.evaluate_value("document.title".into(), false).await.ok().and_then(|title| title.as_str().map(str::to_owned)).unwrap_or_default()
    }

    // ---- Refs

    pub fn ref_generation(&self) -> String {
        self.shared().refs.generation.clone()
    }

    /// The selector the page's helper script understands for what the agent wrote: an `aria-ref` becomes the page's own ref, and a ref this
    /// page did not issue in its latest snapshot is refused.
    pub(crate) fn target_selector(&self, selector: &str) -> OpResult<String> {
        // Ignore quoted and escaped CSS values when looking for a selector-engine boundary.
        let engines = regex::Regex::new(r#"\\.|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`"#).expect("a valid pattern").replace_all(selector, " ");
        if regex::Regex::new(r"(?:^|>>)\s*\*?aria-ref\s*=").expect("a valid pattern").is_match(&engines) {
            let reference = regex::Regex::new(r"^\s*aria-ref\s*=\s*(\S+)\s*$").expect("a valid pattern").captures(selector).map(|captures| captures[1].to_owned());
            let native = reference.and_then(|reference| self.shared().refs.refs.get(&reference).cloned());
            return match native {
                Some(native) => Ok(format!("aria-ref={native}")),
                None => Err(OpError::stale_ref()),
            };
        }
        Ok(selector.to_owned())
    }

    // ---- Viewport and rendering

    /// Sets the viewport as Playwright's `setViewportSize` does: a device-metrics override at the render scale.
    pub async fn set_viewport(&self, width: u32, height: u32) -> OpResult<()> {
        self.send("Emulation.setDeviceMetricsOverride", json!({"width": width, "height": height, "deviceScaleFactor": RENDER_SCALE, "mobile": false})).await?;
        self.shared().viewport = Some((width, height));
        Ok(())
    }

    /// Lays the page out in `width` x `height` CSS pixels and draws each one `scale` times larger.
    pub async fn set_layout(&self, width: u32, height: u32, scale: f64) -> OpResult<()> {
        self.send("Emulation.setDeviceMetricsOverride", json!({"width": width, "height": height, "deviceScaleFactor": scale, "mobile": false})).await?;
        Ok(())
    }

    /// `prefers-color-scheme`; none follows the system.
    pub async fn emulate_color_scheme(&self, scheme: Option<&str>) -> OpResult<()> {
        let features = match scheme {
            Some(scheme) => json!([{"name": "prefers-color-scheme", "value": scheme}]),
            None => json!([{"name": "prefers-color-scheme", "value": ""}]),
        };
        self.send("Emulation.setEmulatedMedia", json!({"features": features})).await?;
        Ok(())
    }

    // ---- Navigation

    async fn wait_progress(&self, timeout: Duration, mut reached: impl FnMut(&Page) -> bool) -> OpResult<()> {
        let mut progress = self.0.progress.subscribe();
        let wait = async {
            loop {
                if reached(self) {
                    return Ok(());
                }
                if self.is_closed() {
                    return Err(OpError::execution("Target page, context or browser has been closed"));
                }
                if progress.changed().await.is_err() {
                    return Err(OpError::execution("Target page, context or browser has been closed"));
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => Err(OpError::timeout(format!("page.goto: Timeout {}ms exceeded.", timeout.as_millis()))),
        }
    }

    fn milestone(&self, loader: &str, name: &str) -> bool {
        self.shared().lifecycle.get(loader).is_some_and(|names| names.contains(name))
    }

    /// Navigates and waits for `readiness`. A failure the browser names (`net::ERR_*`) is returned as it says.
    pub async fn goto(&self, url: &str, readiness: Readiness, timeout: Duration) -> OpResult<()> {
        let started = self.send("Page.navigate", json!({"url": url})).await?;
        if let Some(error) = started["errorText"].as_str().filter(|error| !error.is_empty()) {
            return Err(OpError::execution(format!("page.goto: {error} at {url}")));
        }
        // A navigation within the document commits with no loader of its own.
        let Some(loader) = started["loaderId"].as_str().map(str::to_owned) else { return Ok(()) };
        match readiness {
            Readiness::Commit => self.wait_progress(timeout, |page| page.shared().loader_id.as_deref() == Some(loader.as_str())).await,
            Readiness::DomContentLoaded => self.wait_progress(timeout, |page| page.milestone(&loader, "DOMContentLoaded") || page.milestone(&loader, "load")).await,
            Readiness::Load => self.wait_progress(timeout, |page| page.milestone(&loader, "load")).await,
        }
    }

    /// Waits for the current document to load, as `waitForLoadState("load")`.
    pub async fn wait_for_load(&self, timeout: Duration) -> OpResult<()> {
        let ready = |page: &Page| {
            let shared = page.shared();
            shared.loader_id.as_ref().is_none_or(|loader| shared.lifecycle.get(loader).is_some_and(|names| names.contains("load")))
        };
        self.wait_progress(timeout, ready).await
    }

    /// Moves through the navigation history; false when there is nowhere to go.
    pub async fn go_history(&self, delta: i64, timeout: Duration) -> OpResult<bool> {
        let history = self.send("Page.getNavigationHistory", json!({})).await?;
        let current = history["currentIndex"].as_i64().unwrap_or_default();
        let target = history["entries"].as_array().and_then(|entries| usize::try_from(current + delta).ok().and_then(|index| entries.get(index)));
        let Some(entry) = target.and_then(|entry| entry["id"].as_i64()) else { return Ok(false) };
        let before = self.shared().loader_id.clone();
        self.send("Page.navigateToHistoryEntry", json!({"entryId": entry})).await?;
        self.wait_progress(timeout, |page| page.shared().loader_id != before).await.ok();
        Ok(true)
    }

    pub async fn reload(&self, ignore_cache: bool, timeout: Duration) -> OpResult<()> {
        let before = self.shared().loader_id.clone();
        self.send("Page.reload", json!({"ignoreCache": ignore_cache})).await?;
        self.wait_progress(timeout, |page| page.shared().loader_id != before).await.ok();
        Ok(())
    }

    /// The navigation history position, for the tab's back and forward buttons.
    pub async fn history_position(&self) -> Option<(i64, usize)> {
        let history = self.send("Page.getNavigationHistory", json!({})).await.ok()?;
        Some((history["currentIndex"].as_i64().unwrap_or_default(), history["entries"].as_array().map_or(0, Vec::len)))
    }

    // ---- Dialogs

    /// Accepts or dismisses the dialog the page has open.
    pub async fn handle_dialog(&self, accept: bool, prompt_text: Option<&str>) -> OpResult<()> {
        let mut params = json!({"accept": accept});
        if let Some(text) = prompt_text {
            params["promptText"] = json!(text);
        }
        self.send("Page.handleJavaScriptDialog", params).await?;
        self.invalidate_refs();
        Ok(())
    }

    /// Resolves when a dialog opens after this call; `count` is how many had opened.
    pub(crate) fn dialog_count(&self) -> watch::Receiver<u64> {
        self.0.dialogs.subscribe()
    }

    // ---- Capture

    /// A screenshot of the viewport as base64. A scaled capture repaints live screencasts, so callers pause them; the clip uses document offsets.
    pub async fn capture_viewport(&self, session: Option<&PageSession>, format: &str, quality: Option<u32>, scale: f64) -> OpResult<String> {
        let send = |method: &'static str, params: Value| async move {
            match session {
                Some(session) => session.send(method, params).await.map_err(map_cdp),
                None => self.send(method, params).await,
            }
        };
        let mut params = json!({"format": format});
        if let Some(quality) = quality {
            params["quality"] = json!(quality);
        }
        if scale < 1.0 {
            let (width, height) = self.viewport().unwrap_or((1280, 800));
            let metrics = send("Page.getLayoutMetrics", json!({})).await?;
            let visual = &metrics["cssVisualViewport"];
            params["clip"] = json!({"x": visual["pageX"], "y": visual["pageY"], "width": width, "height": height, "scale": scale});
        }
        let captured = send("Page.captureScreenshot", params).await?;
        Ok(captured["data"].as_str().unwrap_or_default().to_owned())
    }

    /// The page as an agent reads it: its text, its accessibility tree with refs, its diagnostics and a screenshot.
    pub async fn snapshot(&self, diagnostics: SnapshotDiagnostics) -> OpResult<Value> {
        let (width, height) = self.viewport().unwrap_or((1280, 800));
        let scale = (MAX_SCREENSHOT_WIDTH / (width as f64 * RENDER_SCALE)).min(1.0);
        self.invalidate_refs();
        let generation = self.ref_generation();
        let (page, data) = tokio::join!(self.runtime("snapshot", &[], false), self.capture_viewport(None, "png", None, scale));
        let (page, data) = (page?, data?);
        if self.ref_generation() != generation {
            return Err(OpError::execution("The page changed while capturing its snapshot. Take another snapshot."));
        }
        let tree = page["tree"].as_str().unwrap_or_default();
        let tree: String = tree.chars().take(MAX_VISIBLE_TEXT_LENGTH).collect();
        let native = regex::Regex::new(r"\[ref=((?:f\d+)?e\d+)\]").expect("a valid pattern");
        let mut issued = Vec::new();
        let accessibility_tree = native
            .replace_all(&tree, |captures: &regex::Captures| {
                let reference = format!("t3-{generation}-{}", &captures[1]);
                issued.push((reference.clone(), captures[1].to_owned()));
                format!("[ref={reference}]")
            })
            .into_owned();
        {
            let mut shared = self.shared();
            // Only a snapshot nothing invalidated while it ran may hand out refs.
            if shared.refs.generation == generation {
                shared.refs.refs.extend(issued);
            }
        }
        Ok(json!({
            "url": page["url"],
            "title": page["title"],
            "loading": page["loading"],
            "visibleText": page["visibleText"],
            "interactiveElements": [],
            "accessibilityTree": accessibility_tree,
            "consoleEntries": diagnostics.console_entries,
            "networkEntries": diagnostics.network_entries,
            "actionTimeline": diagnostics.action_timeline,
            "screenshot": {
                "mimeType": "image/png",
                "data": data,
                "width": (width as f64 * RENDER_SCALE * scale).round() as i64,
                "height": (height as f64 * RENDER_SCALE * scale).round() as i64,
            },
        }))
    }

    /// `Runtime.evaluate` of the agent's expression, with the same limits as the original.
    pub async fn evaluate(&self, expression: &str, await_promise: bool, return_by_value: bool) -> OpResult<Value> {
        let result = self.send("Runtime.evaluate", json!({"expression": expression, "awaitPromise": await_promise, "returnByValue": return_by_value})).await?;
        if let Some(details) = result.get("exceptionDetails") {
            let description = details["exception"]["description"].as_str().or_else(|| details["text"].as_str()).unwrap_or_default();
            return Err(OpError::execution(description));
        }
        let object = &result["result"];
        let value = match object.get("value") {
            Some(value) => value.clone(),
            None => object.get("description").cloned().unwrap_or(Value::Null),
        };
        let bytes = serde_json::to_string(&value).map_or(0, |encoded| encoded.len());
        if bytes > MAX_EVALUATION_BYTES {
            return Err(OpError { tag: "PreviewAutomationResultTooLargeError".into(), message: format!("Evaluation result is {bytes} bytes; the limit is {MAX_EVALUATION_BYTES}."), detail: Some(json!({"maximumBytes": MAX_EVALUATION_BYTES})) });
        }
        Ok(value)
    }

    /// Waits until every condition the agent gave holds, polling as the original does.
    pub async fn wait_for(&self, selector: Option<&str>, text: Option<&str>, url_includes: Option<&str>, timeout_ms: u64) -> OpResult<()> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let target = selector.map(|selector| self.target_selector(selector)).transpose()?;
        loop {
            let mut holds = true;
            if let Some(selector) = &target {
                // Like the desktop host: the selector must match an element, visible or not.
                holds &= self.runtime("count", &[json!(selector)], false).await?.as_u64().unwrap_or_default() > 0;
            }
            if let Some(text) = text {
                // A navigation can destroy the context mid-check; that counts as not yet.
                let found = self.evaluate_value(format!("(document.body?.innerText ?? \"\").includes({})", json!(text)), false).await;
                holds &= found.is_ok_and(|found| found == json!(true));
            }
            if let Some(fragment) = url_includes {
                holds &= self.url().contains(fragment);
            }
            if holds {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(OpError::timeout(format!("Waited {timeout_ms}ms without every condition holding.")));
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }
}

/// The diagnostics a snapshot carries, from the tab.
pub struct SnapshotDiagnostics {
    pub console_entries: Vec<Value>,
    pub network_entries: Vec<Value>,
    pub action_timeline: Vec<Value>,
}

/// What a command's failure is to the agent.
pub(crate) fn map_cdp(error: CdpError) -> OpError {
    match error {
        CdpError::Remote { message, .. } => OpError::classify(&message),
        CdpError::Closed => OpError::execution("Target page, context or browser has been closed"),
    }
}

/// A failure the page's script threw, read by the kind the helper script prefixed it with.
pub(crate) fn exception_error(details: &Value) -> OpError {
    let description = details["exception"]["description"].as_str().or_else(|| details["text"].as_str()).unwrap_or_default();
    if let Some(marked) = description.find("T3:") {
        let rest = &description[marked + 3..];
        if let Some((kind, message)) = rest.split_once(':') {
            let message = first_line(message);
            match kind {
                "stale" => return OpError::stale_ref(),
                "invalid" => return OpError::invalid_selector(message),
                _ => {}
            }
        }
    }
    OpError::classify(description)
}

fn runtime_install_script() -> String {
    RUNTIME_SOURCE.replace("__KEY__", &runtime_key())
}

fn clipboard_script() -> String {
    CLIPBOARD_SOURCE.replace("__BINDING__", CLIPBOARD_BINDING).replace("__LIMIT__", &CLIPBOARD_TEXT_LIMIT.to_string())
}

/// The script a throwaway page runs to encode a recording in the browser.
pub fn recording_encoder_script() -> String {
    RECORDER_SOURCE.replace("__MAX_FILE_BYTES__", &t3_contracts::PROVIDER_SEND_TURN_MAX_FILE_BYTES.to_string())
}

#[cfg(test)]
#[path = "server_browser_page_tests.rs"]
pub(crate) mod tests;
