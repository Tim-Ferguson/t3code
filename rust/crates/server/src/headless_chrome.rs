//! Renders an agent's HTML page in the headless shell and reports what it looks like (apps/server/src/htmlRender/headlessChrome.ts): a PNG of the
//! top of the page, the height it needs, and what it logged.
//!
//! Pages load from a made-up web origin, never from a file. Chrome refuses local files to every web page, frame, worker and popup, so a page
//! cannot read files the agent's provider withholds; local images reach it already inlined as data URIs. `.localhost` keeps it a secure context
//! that may still load plain-http resources, and the request never leaves the browser. All of the browser's traffic goes through the public-address
//! proxy (`public_proxy.rs`), so a page cannot reach this machine's local network. The main frame also stays on the page and the browser opens
//! no popups, so a capture always shows the page itself.
use crate::cdp::{CdpConnection, CdpError, CdpEvent, CdpPoster};
use async_trait::async_trait;
use base64::Engine;
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Debug, Clone, PartialEq)]
pub struct HtmlRenderBrowserError {
    pub reason: String,
    pub cause: Option<String>,
    /// The end of the browser's stderr when it exited, for diagnosing host setup.
    pub output: Option<String>,
}

impl HtmlRenderBrowserError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self { reason: reason.into(), cause: None, output: None }
    }
    fn caused(reason: impl Into<String>, cause: impl std::fmt::Display) -> Self {
        Self { reason: reason.into(), cause: Some(cause.to_string()), output: None }
    }
    pub fn message(&self) -> String {
        format!("Headless Chrome could not render the page: {}.", self.reason)
    }
}
impl std::fmt::Display for HtmlRenderBrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}
impl std::error::Error for HtmlRenderBrowserError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLevel {
    Log,
    Info,
    Warning,
    Error,
}

impl ConsoleLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleMessage {
    pub level: ConsoleLevel,
    pub text: String,
}

const MAX_CONSOLE_MESSAGES: usize = 20;
const MAX_CONSOLE_TEXT_CHARS: usize = 500;
const VIEWPORT_HEIGHT: u32 = 800;
const MAX_CAPTURE_HEIGHT: u32 = 4_000;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);
const PAGE_ORIGIN: &str = "http://t3-page.localhost";
const PAGE_URL: &str = "http://t3-page.localhost/page.html";
// Stack traces and load errors name the page this way instead of its URL.
const PAGE_NAME: &str = "page.html";
// Each measuring load reads its own copy of the page off the pipe.
const MEASURE_CONCURRENCY: usize = 3;

// Resolves after web fonts load and two frames paint, so late layout lands in the capture.
const SETTLE_EXPRESSION: &str = "document.fonts.ready.then(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve(true)))))";
// The root's scroll height never drops below the viewport, so a short page reports its own box height instead.
const MEASURE_EXPRESSION: &str = "(() => { const root = document.documentElement; return root.scrollHeight > root.clientHeight ? root.scrollHeight : root.getBoundingClientRect().height; })()";

/// A page as the base64 bytes `PAGE_URL` serves, encoded once however often it loads.
fn page_body(html: &str) -> Arc<[u8]> {
    Arc::from(base64::engine::general_purpose::STANDARD.encode(html.as_bytes()).into_bytes())
}

/// What `String(value)` gives in JavaScript for a JSON value.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".into(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() => js_number(float),
            _ => number.to_string(),
        },
        Value::Array(items) => items.iter().map(|item| if item.is_null() { String::new() } else { js_string(item) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

fn js_number(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let magnitude = value.abs();
    if (1e-6..1e21).contains(&magnitude) {
        return format!("{value}");
    }
    // Rust writes `1e21` and `1.5e-7`; JavaScript writes the sign of a positive exponent.
    let text = format!("{value:e}");
    match text.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => format!("{mantissa}e+{exponent}"),
        _ => text,
    }
}

/// A remote object of the protocol: its type, and what it shows as.
pub(crate) fn remote_object_text(object: &Value) -> Option<String> {
    let kind = object.get("type")?.as_str()?;
    let description = match object.get("description") {
        None => None,
        Some(Value::String(text)) => Some(text.as_str()),
        Some(_) => return None,
    };
    Some(match object.get("value") {
        Some(Value::String(text)) => text.clone(),
        value => match description {
            Some(text) => text.to_owned(),
            None => value.map_or_else(|| kind.to_owned(), js_string),
        },
    })
}

fn console_level(kind: &str) -> Option<ConsoleLevel> {
    Some(match kind {
        "log" | "debug" | "dir" | "dirxml" | "table" | "trace" => ConsoleLevel::Log,
        "info" => ConsoleLevel::Info,
        "warning" => ConsoleLevel::Warning,
        "error" | "assert" => ConsoleLevel::Error,
        _ => return None,
    })
}

/// Everything the page logs, uncaught exceptions, and the browser's own load errors.
fn console_message_from_event(method: &str, params: &Value) -> Option<ConsoleMessage> {
    match method {
        "Runtime.consoleAPICalled" => {
            let level = console_level(params.get("type")?.as_str()?)?;
            let arguments = params.get("args")?.as_array()?;
            let texts: Option<Vec<String>> = arguments.iter().map(remote_object_text).collect();
            Some(ConsoleMessage { level, text: texts?.join(" ") })
        }
        "Runtime.exceptionThrown" => {
            let details = params.get("exceptionDetails")?;
            let text = details.get("text")?.as_str()?;
            let exception = match details.get("exception") {
                None => None,
                Some(object) => {
                    remote_object_text(object)?;
                    Some(object)
                }
            };
            let description = exception.and_then(|object| object.get("description")).and_then(Value::as_str);
            Some(ConsoleMessage { level: ConsoleLevel::Error, text: description.unwrap_or(text).to_owned() })
        }
        "Log.entryAdded" => {
            let entry = params.get("entry")?;
            let (level, text) = (entry.get("level")?.as_str()?, entry.get("text")?.as_str()?);
            let url = match entry.get("url") {
                None => None,
                Some(Value::String(url)) => Some(url.as_str()),
                Some(_) => return None,
            };
            let level = match level {
                "error" => ConsoleLevel::Error,
                "warning" => ConsoleLevel::Warning,
                _ => return None,
            };
            Some(ConsoleMessage { level, text: url.filter(|url| !url.is_empty()).map_or_else(|| text.to_owned(), |url| format!("{text} {url}")) })
        }
        _ => None,
    }
}

#[derive(Default)]
struct PageState {
    /// The page served for `PAGE_URL`, from `page_body`.
    body: Option<Arc<[u8]>>,
    loaded: Option<oneshot::Sender<()>>,
    console: Vec<ConsoleMessage>,
    omitted_console: usize,
}

/// What a page's events do, as they arrive: serve the page, keep the main frame on it, count the load, and keep what it logs.
fn page_events(poster: CdpPoster, session_id: String, main_frame_id: String, state: Arc<Mutex<PageState>>) -> impl FnMut(&CdpEvent) + Send + 'static {
    move |event| {
        let mut page = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match event.method.as_str() {
            "Page.loadEventFired" => {
                if let Some(loaded) = page.loaded.take() {
                    let _ = loaded.send(());
                }
            }
            "Fetch.requestPaused" => {
                let (Some(request_id), Some(url)) = (event.params["requestId"].as_str(), event.params["request"]["url"].as_str()) else { return };
                let url = url.split('#').next().unwrap_or_default();
                if url == PAGE_URL {
                    if let Some(body) = page.body.clone() {
                        // The body is shared bytes between two small JSON halves, so loading a page of up to 25 MiB at several widths never copies it.
                        let (session, request) = (json!(session_id).to_string(), json!(request_id).to_string());
                        poster.post_parts(|id| {
                            vec![
                                Arc::from(format!("{{\"id\":{id},\"sessionId\":{session},\"method\":\"Fetch.fulfillRequest\",\"params\":{{\"requestId\":{request},\"responseCode\":200,\"responseHeaders\":[{{\"name\":\"Content-Type\",\"value\":\"text/html; charset=utf-8\"}}],\"body\":\"").into_bytes()),
                                body,
                                Arc::from(&b"\"}}"[..]),
                            ]
                        });
                        return;
                    }
                }
                // A frame inside the page may show another site; Local Network Access still covers it. The main frame and T3's own origin serve
                // nothing else.
                let other_site_frame = event.params["resourceType"].as_str() == Some("Document") && event.params["frameId"].as_str() != Some(main_frame_id.as_str()) && !url.starts_with(&format!("{PAGE_ORIGIN}/"));
                if other_site_frame {
                    poster.post("Fetch.continueRequest", json!({"requestId": request_id}), Some(&session_id));
                } else {
                    poster.post("Fetch.failRequest", json!({"requestId": request_id, "errorReason": "AccessDenied"}), Some(&session_id));
                }
            }
            method => {
                let Some(message) = console_message_from_event(method, &event.params) else { return };
                if page.console.len() >= MAX_CONSOLE_MESSAGES {
                    page.omitted_console += 1;
                    return;
                }
                let text = message.text.replace(PAGE_URL, PAGE_NAME);
                page.console.push(ConsoleMessage { level: message.level, text: crate::mcp_preview::cut_text(&text, MAX_CONSOLE_TEXT_CHARS) });
            }
        }
    }
}

/// A browser this module started or was handed, and how to end it.
#[async_trait]
pub trait BrowserHandle: Send + Sync {
    /// The error for a browser that went away: that it did, and the end of what it said on stderr.
    async fn exited(&self) -> HtmlRenderBrowserError;
    /// Ends the browser and everything it started.
    async fn shutdown(&self);
}

pub struct LaunchInput {
    pub executable: PathBuf,
    pub no_sandbox: bool,
    pub profile_directory: PathBuf,
}

pub struct LaunchedBrowser {
    pub connection: CdpConnection,
    pub handle: Arc<dyn BrowserHandle>,
}

#[async_trait]
pub trait BrowserLauncher: Send + Sync {
    async fn launch(&self, input: &LaunchInput) -> Result<LaunchedBrowser, HtmlRenderBrowserError>;
}

struct Browser {
    connection: CdpConnection,
    handle: Arc<dyn BrowserHandle>,
}

#[derive(Deserialize)]
struct Created {
    #[serde(rename = "targetId")]
    target_id: String,
}
#[derive(Deserialize)]
struct Attached {
    #[serde(rename = "sessionId")]
    session_id: String,
}
#[derive(Deserialize)]
struct Navigation {
    #[serde(rename = "errorText")]
    error_text: Option<String>,
}
#[derive(Deserialize)]
struct Measured {
    result: MeasuredValue,
}
#[derive(Deserialize)]
struct MeasuredValue {
    value: f64,
}
#[derive(Deserialize)]
struct Screenshot {
    data: String,
}

impl Browser {
    async fn send<T: serde::de::DeserializeOwned>(&self, method: &str, params: Value, session_id: Option<&str>) -> Result<T, HtmlRenderBrowserError> {
        let reply = match self.connection.send(method, params, session_id).await {
            Ok(reply) => reply,
            Err(CdpError::Remote { message, .. }) => return Err(HtmlRenderBrowserError::caused(format!("{method} failed"), message)),
            Err(CdpError::Closed) => return Err(self.handle.exited().await),
        };
        serde_json::from_value(reply).map_err(|error| HtmlRenderBrowserError::caused(format!("{method} returned an unexpected result"), error))
    }

    /// A fresh page at `width`; each `load` navigates it and measures the settled layout.
    async fn open_page(&self, width: u32) -> Result<Page<'_>, HtmlRenderBrowserError> {
        let created: Created = self.send("Target.createTarget", json!({"url": "about:blank"}), None).await?;
        let attached: Attached = self.send("Target.attachToTarget", json!({"targetId": created.target_id, "flatten": true}), None).await?;
        let session_id = attached.session_id;
        let state = Arc::new(Mutex::new(PageState::default()));
        // A page target's id is its main frame's id.
        self.connection.on_event(Some(&session_id), page_events(self.connection.poster(), session_id.clone(), created.target_id.clone(), state.clone()));
        let page = Page { browser: self, session_id, target_id: created.target_id, state, width };
        let _: Value = self.send("Page.enable", json!({}), Some(&page.session_id)).await?;
        let _: Value = self.send("Runtime.enable", json!({}), Some(&page.session_id)).await?;
        let _: Value = self.send("Log.enable", json!({}), Some(&page.session_id)).await?;
        // Previews have no use for WebRTC, whose ICE servers can make the browser resolve names outside the proxy. Runs before page scripts in
        // every frame.
        let _: Value = self
            .send("Page.addScriptToEvaluateOnNewDocument", json!({"source": "delete window.RTCPeerConnection; delete window.webkitRTCPeerConnection;", "runImmediately": true}), Some(&page.session_id))
            .await?;
        // Pauses T3's own origin, to serve the page, and every document, to keep the main frame on it.
        let _: Value = self
            .send("Fetch.enable", json!({"patterns": [{"urlPattern": format!("{PAGE_ORIGIN}/*")}, {"urlPattern": "*", "resourceType": "Document"}]}), Some(&page.session_id))
            .await?;
        let _: Value = self
            .send("Emulation.setDeviceMetricsOverride", json!({"width": width, "height": VIEWPORT_HEIGHT, "deviceScaleFactor": 1, "mobile": false}), Some(&page.session_id))
            .await?;
        Ok(page)
    }
}

struct Page<'a> {
    browser: &'a Browser,
    session_id: String,
    target_id: String,
    state: Arc<Mutex<PageState>>,
    width: u32,
}

impl Page<'_> {
    fn state(&self) -> std::sync::MutexGuard<'_, PageState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Loads a page from `page_body`, so callers loading it often encode it once, and returns the height of what settled.
    async fn load(&self, body: Arc<[u8]>, url_fragment: &str) -> Result<u32, HtmlRenderBrowserError> {
        let (loaded, load) = oneshot::channel();
        {
            let mut state = self.state();
            state.body = Some(body);
            state.loaded = Some(loaded);
        }
        let navigation: Navigation = self.browser.send("Page.navigate", json!({"url": format!("{PAGE_URL}{url_fragment}")}), Some(&self.session_id)).await?;
        if navigation.error_text.is_some_and(|text| !text.is_empty()) {
            return Err(HtmlRenderBrowserError::new("the page could not be opened"));
        }
        tokio::select! {
            loaded = load => {
                if loaded.is_err() {
                    return Err(HtmlRenderBrowserError::new("the page could not be opened"));
                }
            }
            () = self.browser.connection.closed() => return Err(self.browser.handle.exited().await),
        }
        let _: Value = self.browser.send("Runtime.evaluate", json!({"expression": SETTLE_EXPRESSION, "awaitPromise": true}), Some(&self.session_id)).await?;
        let measured: Measured = self.browser.send("Runtime.evaluate", json!({"expression": MEASURE_EXPRESSION, "returnByValue": true}), Some(&self.session_id)).await?;
        Ok(measured.result.value.ceil().max(0.0) as u32)
    }

    async fn screenshot(&self, height: u32) -> Result<String, HtmlRenderBrowserError> {
        let screenshot: Screenshot = self
            .browser
            .send("Page.captureScreenshot", json!({"format": "png", "captureBeyondViewport": true, "clip": {"x": 0, "y": 0, "width": self.width, "height": height, "scale": 1}}), Some(&self.session_id))
            .await?;
        Ok(screenshot.data)
    }

    fn console_messages(&self) -> Vec<ConsoleMessage> {
        let state = self.state();
        let mut messages = state.console.clone();
        if state.omitted_console > 0 {
            messages.push(ConsoleMessage { level: ConsoleLevel::Warning, text: format!("{} more console messages were omitted.", state.omitted_console) });
        }
        messages
    }

    /// Frees the page in the browser once it is no longer needed.
    async fn close(&self) {
        let _: Result<Value, _> = self.browser.send("Target.closeTarget", json!({"targetId": self.target_id}), None).await;
    }
}

/// A temporary directory for the browser profile, removed with the value.
struct Scratch(PathBuf);

impl Scratch {
    fn create() -> Result<Self, HtmlRenderBrowserError> {
        let path = std::env::temp_dir().join(format!("t3-html-preview-{}", uuid::Uuid::new_v4().simple()));
        let mut directory = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut directory, 0o700);
        directory.create(&path).map_err(|cause| HtmlRenderBrowserError::caused("a temporary directory could not be created", cause))?;
        Ok(Self(path))
    }
    fn profile(&self) -> PathBuf {
        self.0.join("profile")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn launch(launcher: &dyn BrowserLauncher, scratch: &Scratch, executable: &Path, no_sandbox: bool) -> Result<Browser, HtmlRenderBrowserError> {
    let launched = launcher.launch(&LaunchInput { executable: executable.to_owned(), no_sandbox, profile_directory: scratch.profile() }).await?;
    Ok(Browser { connection: launched.connection, handle: launched.handle })
}

pub struct CaptureInput<'a> {
    pub executable: &'a Path,
    pub no_sandbox: bool,
    pub html: &'a str,
    pub width: u32,
    pub url_fragment: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    /// Base64 PNG of the top `captured_height` pixels.
    pub png: String,
    pub content_height: u32,
    pub captured_height: u32,
    pub console_messages: Vec<ConsoleMessage>,
}

/// Loads `html` at `width` and returns a PNG of the top of the page. The browser and its profile are gone when this returns, however it ends.
pub async fn capture_html_screenshot(launcher: &dyn BrowserLauncher, input: CaptureInput<'_>) -> Result<Capture, HtmlRenderBrowserError> {
    let run = async {
        let scratch = Scratch::create()?;
        let browser = launch(launcher, &scratch, input.executable, input.no_sandbox).await?;
        let captured = async {
            let page = browser.open_page(input.width).await?;
            let content_height = page.load(page_body(input.html), input.url_fragment).await?;
            let captured_height = content_height.min(MAX_CAPTURE_HEIGHT).max(1);
            let png = page.screenshot(captured_height).await?;
            Ok(Capture { png, content_height, captured_height, console_messages: page.console_messages() })
        }
        .await;
        browser.handle.shutdown().await;
        captured
    };
    match tokio::time::timeout(CAPTURE_TIMEOUT, run).await {
        Ok(captured) => captured,
        Err(_) => Err(HtmlRenderBrowserError::new(format!("the page did not finish loading within {} seconds", CAPTURE_TIMEOUT.as_secs()))),
    }
}

/// Content heights of the page at each width, each from a fresh load, since pages often lay themselves out from the width once at load. One
/// browser, a few widths at a time.
pub async fn measure_html_heights(launcher: &dyn BrowserLauncher, executable: &Path, no_sandbox: bool, html: &str, widths: &[u32], url_fragment: &str) -> Result<Vec<(u32, u32)>, HtmlRenderBrowserError> {
    let scratch = Scratch::create()?;
    let browser = launch(launcher, &scratch, executable, no_sandbox).await?;
    let body = page_body(html);
    let measured = stream::iter(widths.iter().copied())
        .map(|width| {
            let (browser, body) = (&browser, body.clone());
            async move {
                let page = browser.open_page(width).await?;
                let height = page.load(body, url_fragment).await;
                page.close().await;
                Ok::<_, HtmlRenderBrowserError>((width, height?))
            }
        })
        .buffered(MEASURE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await;
    browser.handle.shutdown().await;
    measured
}

#[cfg(unix)]
mod process {
    use super::*;
    use crate::public_proxy::PublicProxy;
    use std::{
        os::fd::{AsRawFd, OwnedFd},
        sync::atomic::{AtomicBool, Ordering},
    };
    use tokio::{io::AsyncReadExt, sync::watch};

    /// How much of the browser's stderr is kept for diagnosing a launch that failed.
    const STDERR_BYTES: usize = 8 * 1024;
    const STDERR_CHARS: usize = 2_048;
    const FORCE_KILL_AFTER: Duration = Duration::from_secs(2);

    /// A running headless shell and the proxy its traffic goes through. Dropping it kills the browser's whole process group.
    struct ChromeHandle {
        pid: u32,
        child: tokio::sync::Mutex<tokio::process::Child>,
        reaped: AtomicBool,
        stderr_tail: Arc<Mutex<Vec<u8>>>,
        stderr_done: watch::Receiver<bool>,
        _proxy: Option<PublicProxy>,
    }

    impl ChromeHandle {
        /// Signals the browser's process group. Not once the browser has been waited for: its pid may be someone else's by then.
        fn signal(&self, signal: i32) {
            if !self.reaped.load(Ordering::SeqCst) {
                // The browser leads its own process group (`process_group(0)`), which holds the processes it starts.
                unsafe { libc::kill(-(self.pid as i32), signal) };
            }
        }
    }

    impl Drop for ChromeHandle {
        fn drop(&mut self) {
            self.signal(libc::SIGKILL);
        }
    }

    #[async_trait]
    impl BrowserHandle for ChromeHandle {
        async fn exited(&self) -> HtmlRenderBrowserError {
            // The pipe closes when the browser exits. A startup abort, such as a missing sandbox, says why on stderr, which may still be draining, so
            // give it a moment.
            let mut done = self.stderr_done.clone();
            let _ = tokio::time::timeout(Duration::from_secs(1), done.wait_for(|done| *done)).await;
            let tail = String::from_utf8_lossy(&self.stderr_tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner())).into_owned();
            let units: Vec<u16> = tail.encode_utf16().collect();
            let output = String::from_utf16_lossy(&units[units.len().saturating_sub(STDERR_CHARS)..]);
            HtmlRenderBrowserError { reason: "the browser exited unexpectedly".into(), cause: None, output: Some(output) }
        }

        async fn shutdown(&self) {
            self.signal(libc::SIGTERM);
            let mut child = self.child.lock().await;
            if tokio::time::timeout(FORCE_KILL_AFTER, child.wait()).await.is_err() {
                self.signal(libc::SIGKILL);
                let _ = child.wait().await;
            }
            self.reaped.store(true, Ordering::SeqCst);
        }
    }

    /// Starts the headless shell speaking CDP over `--remote-debugging-pipe` (fd 3 in, fd 4 out, NUL-delimited JSON). Every connection of the
    /// browser goes through a proxy that only reaches public addresses.
    pub struct ProcessLauncher;

    #[async_trait]
    impl BrowserLauncher for ProcessLauncher {
        async fn launch(&self, input: &LaunchInput) -> Result<LaunchedBrowser, HtmlRenderBrowserError> {
            let proxy = PublicProxy::start().await.map_err(|cause| HtmlRenderBrowserError::caused("the preview proxy could not start", cause))?;
            start(input, Some(proxy))
        }
    }

    /// The server browser's launcher (apps/server/src/preview/ServerBrowserContexts.ts): the same shell and pipe, but with the network as the
    /// machine has it, because the original's server browser has no proxy and tabs open the dev servers on this machine's loopback.
    ///
    /// TODO(original: the server browser has no network policy). Nothing here stops a page, or the agent driving it, from reaching loopback
    /// services, the private network or a cloud metadata address, or from leaving a downloaded or uploaded file on the way. A stricter
    /// policy is a different launcher; `ProcessLauncher` is the one that confines the HTML renderer.
    pub struct DirectProcessLauncher;

    #[async_trait]
    impl BrowserLauncher for DirectProcessLauncher {
        async fn launch(&self, input: &LaunchInput) -> Result<LaunchedBrowser, HtmlRenderBrowserError> {
            start(input, None)
        }
    }

    fn start(input: &LaunchInput, proxy: Option<PublicProxy>) -> Result<LaunchedBrowser, HtmlRenderBrowserError> {
        {
            let unable = |cause: std::io::Error| HtmlRenderBrowserError::caused("the browser could not be started", cause);
            // The browser reads commands from fd 3 and writes replies to fd 4; the parent keeps the other end of each.
            let (to_browser_read, to_browser_write) = std::io::pipe().map_err(unable)?;
            let (from_browser_read, from_browser_write) = std::io::pipe().map_err(unable)?;
            let (browser_in, browser_out): (OwnedFd, OwnedFd) = (to_browser_read.into(), from_browser_write.into());
            let (in_fd, out_fd) = (browser_in.as_raw_fd(), browser_out.as_raw_fd());

            let mut command = tokio::process::Command::new(&input.executable);
            command
                .args(["--headless=new", "--remote-debugging-pipe", "--no-first-run", "--no-default-browser-check", "--disable-gpu", "--hide-scrollbars", "--mute-audio"]);
            match &proxy {
                Some(proxy) => {
                    command
                        .arg("--block-new-web-contents")
                        .arg(format!("--proxy-server=socks5://127.0.0.1:{}", proxy.port()))
                        // Loopback would otherwise skip the proxy.
                        .arg("--proxy-bypass-list=<-loopback>")
                        // WebRTC would otherwise send UDP, which no proxy carries.
                        .arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp");
                }
                // Tabs open popups and render at twice the scale; no storage prompt reaches a headless shell.
                None => {
                    command.args(["--force-device-scale-factor=2", "--disable-popup-blocking", "--disable-extensions", "--password-store=basic", "--use-mock-keychain", "--force-color-profile=srgb"]);
                }
            }
            if input.no_sandbox {
                command.arg("--no-sandbox");
            }
            command.arg(format!("--user-data-dir={}", input.profile_directory.display())).arg("about:blank");
            command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).process_group(0).kill_on_drop(true);
            // SAFETY: the closure runs between fork and exec and only calls async-signal-safe functions (fcntl, dup2, close). The two fds it moves are
            // open in the child because the parent holds `browser_in` and `browser_out` until after the spawn.
            unsafe {
                command.pre_exec(move || {
                    // Copies first, above the fds about to be claimed, so neither pipe end is overwritten by the other's move.
                    let moved_in = libc::fcntl(in_fd, libc::F_DUPFD, 10);
                    let moved_out = libc::fcntl(out_fd, libc::F_DUPFD, 10);
                    if moved_in < 0 || moved_out < 0 || libc::dup2(moved_in, 3) < 0 || libc::dup2(moved_out, 4) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(moved_in);
                    libc::close(moved_out);
                    Ok(())
                });
            }
            let mut child = command.spawn().map_err(unable)?;
            drop((browser_in, browser_out, command));
            let pid = child.id().ok_or_else(|| HtmlRenderBrowserError::new("the browser could not be started"))?;

            let mut stderr = child.stderr.take();
            let stderr_tail = Arc::new(Mutex::new(Vec::new()));
            let (stderr_finished, stderr_done) = watch::channel(false);
            let tail = stderr_tail.clone();
            // Ends with the pipe, however long the handle lives.
            tokio::spawn(async move {
                if let Some(stderr) = stderr.as_mut() {
                    let mut chunk = [0u8; 4096];
                    while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
                        let mut kept = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        kept.extend_from_slice(&chunk[..read]);
                        let excess = kept.len().saturating_sub(STDERR_BYTES);
                        kept.drain(..excess);
                    }
                }
                stderr_finished.send_replace(true);
            });

            let sender = tokio::net::unix::pipe::Sender::from_owned_fd(OwnedFd::from(to_browser_write)).map_err(unable)?;
            let receiver = tokio::net::unix::pipe::Receiver::from_owned_fd(OwnedFd::from(from_browser_read)).map_err(unable)?;
            let connection = CdpConnection::over_pipe(receiver, sender);
            let handle = Arc::new(ChromeHandle { pid, child: tokio::sync::Mutex::new(child), reaped: AtomicBool::new(false), stderr_tail, stderr_done, _proxy: proxy });
            Ok(LaunchedBrowser { connection, handle })
        }
    }
}

#[cfg(unix)]
pub use process::{DirectProcessLauncher, ProcessLauncher};

/// Where the browser cannot be started: this host has no pipe transport for it.
#[cfg(not(unix))]
pub struct ProcessLauncher;
#[cfg(not(unix))]
pub struct DirectProcessLauncher;

#[cfg(not(unix))]
#[async_trait]
impl BrowserLauncher for DirectProcessLauncher {
    async fn launch(&self, _: &LaunchInput) -> Result<LaunchedBrowser, HtmlRenderBrowserError> {
        Err(HtmlRenderBrowserError::new("the browser cannot be started on this platform"))
    }
}

#[cfg(not(unix))]
#[async_trait]
impl BrowserLauncher for ProcessLauncher {
    async fn launch(&self, _: &LaunchInput) -> Result<LaunchedBrowser, HtmlRenderBrowserError> {
        Err(HtmlRenderBrowserError::new("the browser cannot be started on this platform"))
    }
}
