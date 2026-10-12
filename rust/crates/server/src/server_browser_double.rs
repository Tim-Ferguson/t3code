//! A scripted Chrome for the server browser's tests: it answers the commands of targets, pages, navigation and evaluation the way the
//! protocol's documented replies read, over the scripted endpoint of `cdp_fake`. No browser is started. The replies were written by hand
//! from the protocol's documentation, not captured from a running Chrome.
use crate::{
    cdp_fake::{Command, FakeBrowser},
    headless_chrome::{BrowserHandle, BrowserLauncher, HtmlRenderBrowserError, LaunchInput, LaunchedBrowser},
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct State {
    contexts: usize,
    targets: usize,
    sessions: usize,
    loaders: usize,
    /// Target ids and the context each was made in.
    target_contexts: HashMap<String, Option<String>>,
    session_targets: HashMap<String, String>,
    urls: HashMap<String, String>,
    evaluations: Vec<(String, Value)>,
    replies: HashMap<String, Value>,
    /// Replies a method gives in turn; the last one repeats.
    sequences: HashMap<String, Vec<Value>>,
    failures: HashMap<String, String>,
    navigation_errors: Vec<(String, String)>,
    /// A navigation that never commits, until it is released.
    hold_navigation: bool,
    history: Option<Value>,
    holds: HashMap<String, Hold>,
}

/// Commands of a method whose reply is kept back, after `after` of them have been answered.
struct Hold {
    after: usize,
    seen: usize,
    arrived: Arc<tokio::sync::Notify>,
    held: Vec<(u64, Option<String>)>,
}

/// The scripted browser. Cloning shares it.
#[derive(Clone, Default)]
pub struct Double(Arc<Mutex<State>>);

fn session_frame(session: &Option<String>, method: &str, params: Value) -> Value {
    json!({"method": method, "sessionId": session, "params": params})
}

impl Double {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    /// A `Runtime.evaluate` whose expression contains `needle` answers `value` (the result's value). The first matching script wins.
    pub fn on_evaluate(&self, needle: &str, value: Value) {
        self.lock().evaluations.push((needle.to_owned(), value));
    }

    /// A fixed result for every command of this method.
    pub fn reply(&self, method: &str, result: Value) {
        self.lock().replies.insert(method.to_owned(), result);
    }

    /// The commands of this method are answered in turn with these results (the last one repeats). A fixed `reply` of the method wins.
    pub fn reply_in_turn(&self, method: &str, results: Vec<Value>) {
        self.lock().sequences.insert(method.to_owned(), results);
    }

    /// Every command of this method fails with this message.
    pub fn fail(&self, method: &str, message: &str) {
        self.lock().failures.insert(method.to_owned(), message.to_owned());
    }

    /// A navigation to a URL containing `needle` fails with this error text.
    pub fn navigation_error(&self, needle: &str, error_text: &str) {
        self.lock().navigation_errors.push((needle.to_owned(), error_text.to_owned()));
    }

    /// Navigations are accepted but nothing commits until `release_navigation`.
    pub fn hold_navigation(&self, hold: bool) {
        self.lock().hold_navigation = hold;
    }

    /// Keeps back the reply to commands of `method` once `after` have been answered; the returned signal is raised when one is held.
    pub fn hold(&self, method: &str, after: usize) -> Arc<tokio::sync::Notify> {
        let arrived = Arc::new(tokio::sync::Notify::new());
        self.lock().holds.insert(method.to_owned(), Hold { after, seen: 0, arrived: arrived.clone(), held: Vec::new() });
        arrived
    }

    /// Answers the commands of `method` that were held, through the browser they came to, and holds no more.
    pub fn release(&self, browser: &FakeBrowser, method: &str) {
        let Some(hold) = self.lock().holds.remove(method) else { return };
        for (id, session) in hold.held {
            browser.push(json!({"id": id, "sessionId": session, "result": {}}));
        }
    }

    /// The history `Page.getNavigationHistory` answers.
    pub fn history(&self, history: Value) {
        self.lock().history = Some(history);
    }

    pub fn targets(&self) -> usize {
        self.lock().targets
    }

    /// The context a target was made in.
    pub fn context_of(&self, target: &str) -> Option<String> {
        self.lock().target_contexts.get(target).cloned().flatten()
    }

    /// The frames a navigation to `url` produces on a page: the request, its response, the commit and the lifecycle.
    pub fn commit_frames(&self, session: &str, url: &str) -> Vec<Value> {
        let mut state = self.lock();
        let target = state.session_targets.get(session).cloned().unwrap_or_default();
        state.loaders += 1;
        let loader = format!("L{}", state.loaders);
        state.urls.insert(target.clone(), url.to_owned());
        let session = Some(session.to_owned());
        let (base, fragment) = url.split_once('#').map_or((url.to_owned(), String::new()), |(base, fragment)| (base.to_owned(), format!("#{fragment}")));
        vec![
            session_frame(&session, "Network.requestWillBeSent", json!({"requestId": format!("R-{loader}"), "loaderId": loader, "frameId": target, "type": "Document", "request": {"url": base, "method": "GET"}})),
            session_frame(&session, "Network.responseReceived", json!({"requestId": format!("R-{loader}"), "frameId": target, "type": "Document", "response": {"url": base, "status": 200}})),
            session_frame(&session, "Page.frameNavigated", json!({"frame": {"id": target, "loaderId": loader, "url": base, "urlFragment": fragment}})),
            session_frame(&session, "Page.lifecycleEventFired", json!({"frameId": target, "loaderId": loader, "name": "DOMContentLoaded"})),
            session_frame(&session, "Page.lifecycleEventFired", json!({"frameId": target, "loaderId": loader, "name": "load"})),
        ]
    }

    fn answer(&self, command: &Command) -> Vec<Value> {
        let session = &command.session_id;
        let reply = |result: Value| json!({"id": command.id, "sessionId": session, "result": result});
        let mut state = self.lock();
        if let Some(hold) = state.holds.get_mut(&command.method) {
            hold.seen += 1;
            if hold.seen > hold.after {
                hold.held.push((command.id, session.clone()));
                hold.arrived.notify_one();
                return Vec::new();
            }
        }
        if let Some(message) = state.failures.get(&command.method) {
            return vec![json!({"id": command.id, "sessionId": session, "error": {"code": -32000, "message": message}})];
        }
        if let Some(result) = state.replies.get(&command.method) {
            return vec![reply(result.clone())];
        }
        if let Some(results) = state.sequences.get_mut(&command.method) {
            let result = if results.len() > 1 { results.remove(0) } else { results[0].clone() };
            return vec![reply(result)];
        }
        match command.method.as_str() {
            "Target.createBrowserContext" => {
                state.contexts += 1;
                vec![reply(json!({"browserContextId": format!("CTX-{}", state.contexts)}))]
            }
            "Target.createTarget" => {
                state.targets += 1;
                let id = format!("T-{}", state.targets);
                let context = command.params["browserContextId"].as_str().map(str::to_owned);
                state.target_contexts.insert(id.clone(), context);
                vec![reply(json!({"targetId": id}))]
            }
            "Target.attachToTarget" => {
                state.sessions += 1;
                let id = format!("S-{}", state.sessions);
                state.session_targets.insert(id.clone(), command.params["targetId"].as_str().unwrap_or_default().to_owned());
                vec![reply(json!({"sessionId": id}))]
            }
            "Target.closeTarget" => {
                let target = command.params["targetId"].as_str().unwrap_or_default();
                vec![reply(json!({"success": true})), json!({"method": "Target.targetDestroyed", "params": {"targetId": target}})]
            }
            "Page.navigate" => {
                let url = command.params["url"].as_str().unwrap_or_default().to_owned();
                let target = state.session_targets.get(session.as_deref().unwrap_or_default()).cloned().unwrap_or_default();
                if let Some((_, error_text)) = state.navigation_errors.iter().find(|(needle, _)| url.contains(needle.as_str())).cloned() {
                    return vec![reply(json!({"frameId": target, "loaderId": "LF", "errorText": error_text}))];
                }
                if state.hold_navigation {
                    state.loaders += 1;
                    return vec![reply(json!({"frameId": target, "loaderId": format!("L{}", state.loaders)}))];
                }
                drop(state);
                let loader = format!("L{}", self.lock().loaders + 1);
                let mut frames = vec![reply(json!({"frameId": target, "loaderId": loader}))];
                frames.extend(self.commit_frames(session.as_deref().unwrap_or_default(), &url));
                frames
            }
            // A reload and a step through the history commit a new document, as a navigation does.
            "Page.reload" | "Page.navigateToHistoryEntry" => {
                let target = state.session_targets.get(session.as_deref().unwrap_or_default()).cloned().unwrap_or_default();
                let url = state.urls.get(&target).cloned().unwrap_or_else(|| "about:blank".into());
                drop(state);
                let mut frames = vec![reply(json!({}))];
                frames.extend(self.commit_frames(session.as_deref().unwrap_or_default(), &url));
                frames
            }
            "Runtime.evaluate" => {
                let expression = command.params["expression"].as_str().unwrap_or_default();
                let value = state.evaluations.iter().find(|(needle, _)| expression.contains(needle.as_str())).map(|(_, value)| value.clone());
                match value {
                    // A script that answers an element: a remote object, as a reply by reference reads.
                    Some(Value::Object(object)) if object.contains_key("__objectId") => vec![reply(json!({"result": {"type": "object", "subtype": "node", "objectId": object["__objectId"]}}))],
                    Some(Value::Object(object)) if object.contains_key("__exception") => vec![reply(json!({"result": {"type": "object"}, "exceptionDetails": {"text": "Uncaught", "exception": {"description": object["__exception"]}}}))],
                    Some(value) => vec![reply(json!({"result": {"type": "object", "value": value}}))],
                    None => vec![reply(json!({"result": {"type": "undefined"}}))],
                }
            }
            "Page.getNavigationHistory" => vec![reply(state.history.clone().unwrap_or_else(|| json!({"currentIndex": 0, "entries": [{"id": 1}]})))],
            "Page.getLayoutMetrics" => vec![reply(json!({"cssVisualViewport": {"pageX": 0, "pageY": 0}}))],
            "Page.captureScreenshot" => vec![reply(json!({"data": "ZnJhbWU="}))],
            _ => vec![reply(json!({}))],
        }
    }

    /// The browser to hand to `FakeBrowser::pipe`.
    pub fn browser(&self) -> FakeBrowser {
        let double = self.clone();
        FakeBrowser::with_handler(move |command| double.answer(command))
    }
}

/// Holds a browser's shutdown until the test lets it go.
#[derive(Default)]
pub struct Gate {
    /// Signalled when a shutdown has begun.
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

struct Handle {
    shutdowns: Arc<AtomicUsize>,
    exit_output: String,
    gate: Option<Arc<Gate>>,
}

#[async_trait]
impl BrowserHandle for Handle {
    async fn exited(&self) -> HtmlRenderBrowserError {
        HtmlRenderBrowserError { reason: "the browser exited unexpectedly".into(), cause: None, output: Some(self.exit_output.clone()) }
    }
    async fn shutdown(&self) {
        if let Some(gate) = &self.gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
    }
}

/// What a launch was asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub executable: std::path::PathBuf,
    pub no_sandbox: bool,
    pub profile_directory: std::path::PathBuf,
}

/// A launcher that starts a scripted browser per launch. The first `failures` launches fail as a browser that aborts would, saying
/// `exit_output` on its stderr.
pub struct DoubleLauncher {
    pub double: Double,
    pub browsers: Mutex<Vec<FakeBrowser>>,
    launches: Mutex<Vec<Launch>>,
    shutdowns: Arc<AtomicUsize>,
    failures: AtomicUsize,
    exit_output: String,
    gate: Option<Arc<Gate>>,
}

impl DoubleLauncher {
    pub fn new(double: Double) -> Self {
        Self { double, browsers: Mutex::default(), launches: Mutex::default(), shutdowns: Arc::default(), failures: AtomicUsize::new(0), exit_output: "fake stderr".into(), gate: None }
    }
    /// The first `count` launches abort with `output` on their stderr.
    pub fn failing(self, count: usize, output: &str) -> Self {
        self.failures.store(count, Ordering::SeqCst);
        Self { exit_output: output.into(), ..self }
    }
    /// Every browser's shutdown waits on the returned gate.
    pub fn gated(mut self) -> (Self, Arc<Gate>) {
        let gate = Arc::new(Gate::default());
        self.gate = Some(gate.clone());
        (self, gate)
    }
    pub fn launches(&self) -> Vec<Launch> {
        self.launches.lock().unwrap().clone()
    }
    pub fn shutdowns(&self) -> usize {
        self.shutdowns.load(Ordering::SeqCst)
    }
    /// The browser the nth launch started.
    pub fn browser(&self, index: usize) -> FakeBrowser {
        self.browsers.lock().unwrap()[index].clone()
    }
}

#[async_trait]
impl BrowserLauncher for DoubleLauncher {
    async fn launch(&self, input: &LaunchInput) -> Result<LaunchedBrowser, HtmlRenderBrowserError> {
        self.launches.lock().unwrap().push(Launch { executable: input.executable.clone(), no_sandbox: input.no_sandbox, profile_directory: input.profile_directory.clone() });
        if self.failures.load(Ordering::SeqCst) > 0 {
            self.failures.fetch_sub(1, Ordering::SeqCst);
            // A browser that aborts at startup: its connection is gone before the first command is answered.
            let browser = FakeBrowser::with_handler(|_| vec![json!({"$hangUp": true})]);
            self.browsers.lock().unwrap().push(browser.clone());
            let (reader, writer) = browser.pipe();
            return Ok(LaunchedBrowser { connection: crate::cdp::CdpConnection::over_pipe(reader, writer), handle: Arc::new(Handle { shutdowns: self.shutdowns.clone(), exit_output: self.exit_output.clone(), gate: self.gate.clone() }) });
        }
        let browser = self.double.browser();
        self.browsers.lock().unwrap().push(browser.clone());
        let (reader, writer) = browser.pipe();
        Ok(LaunchedBrowser { connection: crate::cdp::CdpConnection::over_pipe(reader, writer), handle: Arc::new(Handle { shutdowns: self.shutdowns.clone(), exit_output: self.exit_output.clone(), gate: self.gate.clone() }) })
    }
}
