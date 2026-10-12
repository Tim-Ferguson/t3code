//! The preview sessions of the environment (apps/server/src/preview/Manager.ts): which browser tabs each
//! thread has, kept in memory, and the events that follow them.
//!
//! Sessions are keyed by `(thread, tab)`; a thread can have several tabs, and `open` always makes a new one.
//! The tab's page itself is owned by whoever renders it: a desktop client reports what its webview does
//! (`report_status`), a tab of the server's own browser is driven by that browser. Every change publishes an
//! event carrying a revision that rises by one per event, shared with `list`, so a client can tell a list
//! it fetched from one it has already seen past.
//!
//! An event is published while the state lock is held, so subscribers see events in the order the state changed.
//! A subscriber is an unbounded queue: a slow one never holds up a change, and one that has gone away is dropped.
use chrono::{DateTime, SecondsFormat, Utc};
use indexmap::IndexMap;
use serde_json::Value;
use std::sync::{Arc, Mutex, atomic::AtomicU64};
use t3_contracts::{
    Defect, PositiveInt, PreviewAdjustInput, PreviewClosedEvent, PreviewCloseInput, PreviewControlRequiredError, PreviewError, PreviewEvent, PreviewFailedEvent,
    PreviewInvalidUrlError, PreviewInvalidUrlReason, PreviewListInput, PreviewListResult, PreviewNavStatus, PreviewNavigateInput, PreviewNavigatedEvent,
    PreviewNavigatedType, PreviewOpenInput, PreviewOpenedEvent, PreviewOpenedType, PreviewClosedType, PreviewFailedType, PreviewRefreshInput,
    PreviewReportStatusInput, PreviewResizeInput, PreviewResizeRequest, PreviewResizedEvent, PreviewResizedType, PreviewRevealRequest, PreviewRuntime,
    PreviewSessionLookupError, PreviewSessionSnapshot, PreviewTabId, PreviewUrl, PreviewViewportSetting, TrimmedNonEmptyString, NonNegativeInt,
};
use tokio::sync::mpsc;

/// What the original's `normalizePreviewUrl` refuses a URL for, without the URL itself: the failure is shown to the person
/// who typed it and logged, and a URL can carry credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlNormalizationError {
    /// UTF-16 length of what was given, as JavaScript counts it.
    pub input_length: usize,
    pub reason: PreviewInvalidUrlReason,
    pub protocol: Option<String>,
}

impl UrlNormalizationError {
    pub fn message(&self) -> String {
        let protocol = self.protocol.as_ref().map(|protocol| format!(": {protocol}")).unwrap_or_default();
        let reason = match self.reason {
            PreviewInvalidUrlReason::Empty => "empty",
            PreviewInvalidUrlReason::Parse => "parse",
            PreviewInvalidUrlReason::UnsupportedProtocol => "unsupported-protocol",
            PreviewInvalidUrlReason::Unexpected => "unexpected",
        };
        format!("Invalid preview URL ({reason}{protocol}; input length {}).", self.input_length)
    }
}

/// Whether `value` starts with a loopback host: `localhost`, `127.0.0.1`, `0.0.0.0` or `[::1]` (and `[::]`), then end, `:` or `/`.
fn starts_with_loopback_host(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    ["localhost", "127.0.0.1", "0.0.0.0", "[::1]", "[::]"].iter().any(|host| {
        lower.strip_prefix(host).is_some_and(|rest| rest.is_empty() || rest.starts_with(':') || rest.starts_with('/'))
    })
}

/// The scheme of `candidate` written as JavaScript's `previewUrlProtocol` reads it: letters, digits, `+`, `-`, `.`, then `:`.
fn url_protocol(candidate: &str) -> Option<String> {
    let end = candidate.find(':')?;
    let scheme = &candidate[..end];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))).then(|| format!("{}:", scheme.to_ascii_lowercase()))
}

/// A free-form URL as a fully qualified `http(s)` URL: a bare loopback host gets `http`, any other bare host `https`, and a
/// qualified URL is validated and written as the URL standard writes it.
pub fn normalize_preview_url(raw: &str) -> Result<String, UrlNormalizationError> {
    let input_length = raw.encode_utf16().count();
    let trimmed = t3_contracts::trim_wire_string(raw);
    if trimmed.is_empty() {
        return Err(UrlNormalizationError { input_length, reason: PreviewInvalidUrlReason::Empty, protocol: None });
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("{}://{trimmed}", if starts_with_loopback_host(trimmed) { "http" } else { "https" })
    };
    let parsed = url::Url::parse(&candidate).map_err(|_| UrlNormalizationError { input_length, reason: PreviewInvalidUrlReason::Parse, protocol: url_protocol(&candidate) })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(UrlNormalizationError { input_length, reason: PreviewInvalidUrlReason::UnsupportedProtocol, protocol: Some(format!("{}:", parsed.scheme())) });
    }
    Ok(parsed.as_str().to_owned())
}

/// A fresh tab id: `tab_` and a counter in base 36.
pub fn new_preview_tab_id() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let mut n = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(char::from_digit((n % 36) as u32, 36).expect("a base 36 digit"));
        n /= 36;
    }
    digits.reverse();
    format!("tab_{}", digits.into_iter().collect::<String>())
}

/// The failure the original makes of a URL it cannot normalize. Its `cause` is the normalization error as a `Schema.Defect` writes it.
fn invalid_url(error: UrlNormalizationError) -> PreviewError {
    let mut cause = serde_json::json!({"name":"PreviewUrlNormalizationError","message":error.message()});
    if error.reason == PreviewInvalidUrlReason::Parse {
        cause["cause"] = serde_json::json!({"name":"TypeError","message":"Invalid URL"});
    }
    PreviewError::InvalidUrl(PreviewInvalidUrlError {
        tag: Default::default(),
        input_length: serde_json::Number::from(error.input_length),
        reason: error.reason,
        // The key is present with no value, which the original's encoder writes as null.
        protocol: Some(error.protocol),
        cause: Defect(cause),
    })
}

fn fill() -> Option<Option<PreviewViewportSetting>> {
    Some(Some(PreviewViewportSetting::Fill))
}

/// Everything about a tab an event or `list` reports.
type Sessions = IndexMap<(String, String), PreviewSessionSnapshot>;

struct State {
    /// Every session of every thread, in the order they were made.
    sessions: Sessions,
    /// Rises by one per published event.
    revision: u64,
    subscribers: Vec<mpsc::UnboundedSender<PreviewEvent>>,
}

struct Inner {
    server_epoch: String,
    /// How many subscribers are listening, for a caller that must know its subscription has taken effect before it acts.
    subscriptions: tokio::sync::watch::Sender<usize>,
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    state: Mutex<State>,
}

/// The sessions of one environment. Cloning shares them.
#[derive(Clone)]
pub struct PreviewManager(Arc<Inner>);

impl Default for PreviewManager {
    fn default() -> Self {
        Self::with_clock(Arc::new(Utc::now))
    }
}

/// How a session is changed under the lock: the session it becomes, the event announcing it (if any) and what the caller gets.
struct Mutation<R> {
    next: PreviewSessionSnapshot,
    emit: Option<EventDraft>,
    result: R,
}

/// An event before it has its revision and epoch.
enum EventDraft {
    Navigated(PreviewSessionSnapshot),
    Resized(PreviewSessionSnapshot, Option<PreviewResizeRequest>),
    Failed { url: PreviewUrl, title: t3_contracts::PreviewTitle, code: t3_contracts::SafeInt, description: String },
}

impl PreviewManager {
    pub fn with_clock(clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>) -> Self {
        Self(Arc::new(Inner { server_epoch: uuid::Uuid::new_v4().to_string(), subscriptions: tokio::sync::watch::channel(0).0, clock, state: Mutex::new(State { sessions: IndexMap::new(), revision: 0, subscribers: Vec::new() }) }))
    }

    /// Identifies this server process, so a client can tell a restart from a change of state.
    pub fn server_epoch(&self) -> &str {
        &self.0.server_epoch
    }

    fn now(&self) -> String {
        (self.0.clock)().to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn epoch(&self) -> TrimmedNonEmptyString {
        TrimmedNonEmptyString::new(&self.0.server_epoch).expect("the epoch is not blank")
    }

    /// Every event from now on, in the order the state changed.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<PreviewEvent> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut state = self.lock();
        state.subscribers.push(sender);
        self.0.subscriptions.send_replace(state.subscribers.len());
        receiver
    }

    /// The number of subscribers listening, as it changes.
    pub fn subscriptions(&self) -> tokio::sync::watch::Receiver<usize> {
        self.0.subscriptions.subscribe()
    }

    fn publish(&self, state: &mut State, event: PreviewEvent) {
        state.subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
        self.0.subscriptions.send_replace(state.subscribers.len());
    }

    /// The event for `snapshot`'s tab at `revision`.
    fn event(&self, draft: EventDraft, session: &PreviewSessionSnapshot, revision: u64) -> PreviewEvent {
        let (thread_id, tab_id) = (session.thread_id.clone(), session.tab_id.clone());
        let (created_at, server_epoch, revision) = (session.updated_at.clone(), self.epoch(), PositiveInt(revision));
        match draft {
            EventDraft::Navigated(snapshot) => PreviewEvent::Navigated(PreviewNavigatedEvent { thread_id, tab_id, created_at, server_epoch, revision, r#type: PreviewNavigatedType, snapshot }),
            EventDraft::Resized(snapshot, request) => {
                PreviewEvent::Resized(PreviewResizedEvent { thread_id, tab_id, created_at, server_epoch, revision, r#type: PreviewResizedType, snapshot, request: request.map(Some) })
            }
            EventDraft::Failed { url, title, code, description } => {
                PreviewEvent::Failed(PreviewFailedEvent { thread_id, tab_id, created_at, server_epoch, revision, r#type: PreviewFailedType, url, title, code, description })
            }
        }
    }

    /// Reads, changes and writes back the session for `(thread, tab)` as one step, and publishes the change inside it.
    fn mutate_existing<R>(&self, thread_id: &str, tab_id: &str, mutator: impl FnOnce(&PreviewSessionSnapshot, String) -> Result<Mutation<R>, PreviewError>) -> Result<R, PreviewError> {
        let updated_at = self.now();
        let mut state = self.lock();
        let key = (thread_id.to_owned(), tab_id.to_owned());
        let Some(session) = state.sessions.get(&key) else {
            return Err(PreviewError::SessionLookup(PreviewSessionLookupError::new(thread_id, tab_id)));
        };
        let Mutation { next, emit, result } = mutator(session, updated_at)?;
        let mut revision = state.revision;
        if let Some(draft) = emit {
            revision += 1;
            let event = self.event(draft, &next, revision);
            self.publish(&mut state, event);
        }
        state.sessions.insert(key, next);
        state.revision = revision;
        Ok(result)
    }

    pub fn open(&self, input: PreviewOpenInput) -> Result<PreviewSessionSnapshot, PreviewError> {
        self.open_with(input, None, |_| {})
    }

    /// Opens a tab. `automation_owner` names the provider session that owns an isolated server tab, and `before_publish` runs
    /// once the tab exists and before its `opened` event is published, so a subscriber finds state keyed by the tab.
    pub fn open_with(&self, input: PreviewOpenInput, automation_owner: Option<String>, before_publish: impl FnOnce(&PreviewSessionSnapshot)) -> Result<PreviewSessionSnapshot, PreviewError> {
        let runtime = input.runtime.flatten();
        // A persisted client surface must not bind to a different tab after the server restarts.
        let tab_id = format!("{}{}", new_preview_tab_id(), if runtime == Some(PreviewRuntime::Server) { format!("_{}", self.0.server_epoch) } else { String::new() });
        let updated_at = self.now();
        let nav_status = match input.url.flatten() {
            Some(url) => {
                let normalized = normalize_preview_url(url.0.as_str()).map_err(invalid_url)?;
                PreviewNavStatus::loading(preview_url(&normalized), t3_contracts::BoundedString(String::new()))
            }
            None => PreviewNavStatus::idle(),
        };
        let snapshot = PreviewSessionSnapshot {
            thread_id: TrimmedNonEmptyString::new(input.thread_id.as_str()).expect("a thread id is not blank"),
            tab_id: PreviewTabId::new(&tab_id).expect("a tab id made here is within the bound"),
            nav_status,
            can_go_back: false,
            can_go_forward: false,
            viewport: input.viewport.flatten().map(Some).or_else(fill),
            color_scheme: None,
            zoom_factor: None,
            profile_id: input.profile_id.flatten().map(Some),
            runtime: runtime.map(Some),
            automation_owner: if runtime == Some(PreviewRuntime::Server) { automation_owner.map(Some) } else { None },
            reveal: input.reveal.flatten().map(Some),
            reveal_request: None,
            updated_at,
        };
        let mut state = self.lock();
        state.revision += 1;
        state.sessions.insert((input.thread_id.as_str().to_owned(), tab_id), snapshot.clone());
        before_publish(&snapshot);
        let event = PreviewEvent::Opened(PreviewOpenedEvent {
            thread_id: snapshot.thread_id.clone(),
            tab_id: snapshot.tab_id.clone(),
            created_at: snapshot.updated_at.clone(),
            server_epoch: self.epoch(),
            revision: PositiveInt(state.revision),
            r#type: PreviewOpenedType,
            snapshot: snapshot.clone(),
        });
        self.publish(&mut state, event);
        Ok(snapshot)
    }

    /// Records where a desktop tab went. A server tab is navigated by its browser and refuses it.
    pub fn navigate(&self, input: PreviewNavigateInput) -> Result<PreviewSessionSnapshot, PreviewError> {
        let url = preview_url(&normalize_preview_url(input.url.0.as_str()).map_err(invalid_url)?);
        self.mutate_existing(input.thread_id.as_str(), input.tab_id.as_str(), |session, updated_at| {
            if session.runtime == Some(Some(PreviewRuntime::Server)) {
                return Err(PreviewError::ControlRequired(PreviewControlRequiredError::new(input.tab_id.as_str())));
            }
            let title = input.resolved_title.flatten().map(|title| title.0).unwrap_or_else(|| session.nav_status.title().to_owned());
            let snapshot = PreviewSessionSnapshot {
                nav_status: PreviewNavStatus::success(url, t3_contracts::BoundedString(title)),
                viewport: session.viewport.clone().or_else(fill),
                updated_at,
                ..session.clone()
            };
            Ok(Mutation { next: snapshot.clone(), emit: Some(EventDraft::Navigated(snapshot.clone())), result: snapshot })
        })
    }

    /// What a client's webview reports. A tab of the server's browser takes its status only from that browser (`server_controlled`).
    pub fn report_status(&self, input: PreviewReportStatusInput, server_controlled: bool) -> Result<(), PreviewError> {
        self.mutate_existing(input.thread_id.as_str(), input.tab_id.as_str(), |session, updated_at| {
            if session.runtime == Some(Some(PreviewRuntime::Server)) && !server_controlled {
                return Err(PreviewError::ControlRequired(PreviewControlRequiredError::new(input.tab_id.as_str())));
            }
            let snapshot = PreviewSessionSnapshot {
                nav_status: input.nav_status.clone(),
                can_go_back: input.can_go_back,
                can_go_forward: input.can_go_forward,
                viewport: session.viewport.clone().or_else(fill),
                updated_at,
                ..session.clone()
            };
            let emit = match input.nav_status {
                PreviewNavStatus::LoadFailed(failed) => EventDraft::Failed { url: failed.url, title: failed.title, code: failed.code, description: failed.description },
                _ => EventDraft::Navigated(snapshot.clone()),
            };
            Ok(Mutation { next: snapshot, emit: Some(emit), result: () })
        })
    }

    /// Asks viewers to show a tab, as an agent that opened it wants. `force` overrides a viewer's automatic-floating setting.
    pub fn request_reveal(&self, thread_id: &str, tab_id: &str, force: bool) -> Result<(), PreviewError> {
        self.mutate_existing(thread_id, tab_id, |session, updated_at| {
            let snapshot = PreviewSessionSnapshot {
                reveal: Some(Some(true)),
                reveal_request: Some(Some(PreviewRevealRequest { id: uuid::Uuid::new_v4().to_string(), force })),
                updated_at,
                ..session.clone()
            };
            Ok(Mutation { next: snapshot.clone(), emit: Some(EventDraft::Navigated(snapshot)), result: () })
        })
    }

    /// Any client may size a tab, as the desktop has always allowed; the server's browser follows the published setting for its own tabs.
    pub fn resize(&self, input: PreviewResizeInput) -> Result<PreviewSessionSnapshot, PreviewError> {
        self.mutate_existing(input.thread_id.as_str(), input.tab_id.as_str(), |session, updated_at| {
            let snapshot = PreviewSessionSnapshot { viewport: Some(Some(input.viewport.clone())), updated_at, ..session.clone() };
            Ok(Mutation { next: snapshot.clone(), emit: Some(EventDraft::Resized(snapshot.clone(), None)), result: snapshot })
        })
    }

    /// Records a server tab's appearance or zoom and publishes it; the server's browser applies it to the page. Any client may send
    /// it, as it changes how the page renders, not what it does. A hard reload or a clear rides on the event, once.
    pub fn adjust(&self, input: PreviewAdjustInput) -> Result<PreviewSessionSnapshot, PreviewError> {
        self.mutate_existing(input.thread_id.as_str(), input.tab_id.as_str(), |session, updated_at| {
            let mut snapshot = PreviewSessionSnapshot { updated_at, ..session.clone() };
            if let Some(color_scheme) = input.color_scheme.flatten() {
                snapshot.color_scheme = Some(Some(color_scheme));
            }
            if let Some(zoom_factor) = input.zoom_factor.flatten() {
                snapshot.zoom_factor = Some(Some(zoom_factor));
            }
            let hard_reload = input.hard_reload.flatten().unwrap_or(false);
            let clear = input.clear.flatten();
            let request = (hard_reload || clear.is_some()).then(|| PreviewResizeRequest { hard_reload: hard_reload.then_some(Some(true)), clear: clear.map(Some) });
            Ok(Mutation { next: snapshot.clone(), emit: Some(EventDraft::Resized(snapshot.clone(), request)), result: snapshot })
        })
    }

    /// Checks the tab exists. Whoever renders it reloads it and reports progress through `report_status`, so nothing is published.
    pub fn refresh(&self, input: PreviewRefreshInput) -> Result<(), PreviewError> {
        self.mutate_existing(input.thread_id.as_str(), input.tab_id.as_str(), |session, _| Ok(Mutation { next: session.clone(), emit: None, result: () }))
    }

    /// Closes one tab, or every tab of the thread. A tab that is not there is left alone, so closing twice is not an error.
    pub fn close(&self, input: PreviewCloseInput) {
        let created_at = self.now();
        let mut state = self.lock();
        let thread_id = input.thread_id.as_str();
        let targets: Vec<(String, String)> = match input.tab_id.flatten() {
            Some(tab_id) => state.sessions.contains_key(&(thread_id.to_owned(), tab_id.as_str().to_owned())).then(|| (thread_id.to_owned(), tab_id.as_str().to_owned())).into_iter().collect(),
            None => state.sessions.keys().filter(|(thread, _)| thread == thread_id).cloned().collect(),
        };
        for key in targets {
            let Some(session) = state.sessions.shift_remove(&key) else { continue };
            state.revision += 1;
            let event = PreviewEvent::Closed(PreviewClosedEvent {
                thread_id: session.thread_id,
                tab_id: session.tab_id,
                created_at: created_at.clone(),
                server_epoch: self.epoch(),
                revision: PositiveInt(state.revision),
                r#type: PreviewClosedType,
            });
            self.publish(&mut state, event);
        }
    }

    /// Whether the tab is there, and what it is.
    pub fn session(&self, thread_id: &str, tab_id: &str) -> Option<PreviewSessionSnapshot> {
        self.lock().sessions.get(&(thread_id.to_owned(), tab_id.to_owned())).cloned()
    }

    /// The thread's tabs, oldest change first.
    pub fn list(&self, input: PreviewListInput) -> PreviewListResult {
        let state = self.lock();
        let mut sessions: Vec<PreviewSessionSnapshot> = state.sessions.iter().filter(|((thread, _), _)| thread == input.thread_id.as_str()).map(|(_, session)| session.clone()).collect();
        // A stable sort, so tabs changed in the same millisecond stay in the order they were made.
        sessions.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
        PreviewListResult { sessions, server_epoch: self.epoch(), revision: NonNegativeInt(state.revision) }
    }
}

fn preview_url(normalized: &str) -> PreviewUrl {
    serde_json::from_value(Value::String(normalized.to_owned())).expect("a normalized URL is within the bound")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manager() -> PreviewManager {
        PreviewManager::default()
    }
    fn open_input(value: Value) -> PreviewOpenInput {
        serde_json::from_value(value).expect("a valid open input")
    }
    fn input<T: serde::de::DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).expect("a valid input")
    }
    fn events(receiver: &mut mpsc::UnboundedReceiver<PreviewEvent>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            out.push(serde_json::to_value(event).unwrap());
        }
        out
    }
    fn error_tag(error: PreviewError) -> String {
        serde_json::to_value(error).unwrap()["_tag"].as_str().unwrap().to_owned()
    }
    fn list(manager: &PreviewManager, thread: &str) -> PreviewListResult {
        manager.list(input(json!({"threadId":thread})))
    }

    #[test]
    fn opening_a_tab_publishes_it_with_a_normalized_url() {
        let manager = manager();
        let mut events_ = manager.subscribe();
        let snapshot = manager.open(open_input(json!({"threadId":"t1","url":"localhost:5173"}))).unwrap();
        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(wire["navStatus"], json!({"_tag":"Loading","url":"http://localhost:5173/","title":""}));
        assert_eq!(wire["viewport"], json!({"_tag":"fill"}));
        assert!(wire["tabId"].as_str().unwrap().starts_with("tab_"));
        let published = events(&mut events_);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0]["type"], "opened");
        assert_eq!(published[0]["revision"], 1);
        assert_eq!(published[0]["serverEpoch"], manager.server_epoch());
        assert_eq!(published[0]["snapshot"], wire);
    }

    #[test]
    fn a_tab_without_a_url_is_idle_and_every_open_makes_its_own_tab() {
        let manager = manager();
        let first = manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        let second = manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        assert_eq!(serde_json::to_value(&first.nav_status).unwrap(), json!({"_tag":"Idle"}));
        assert_ne!(first.tab_id, second.tab_id);
        assert_eq!(list(&manager, "t1").sessions.len(), 2);
    }

    #[test]
    fn a_server_tab_id_carries_the_epoch_so_a_restart_cannot_rebind_it() {
        let manager = manager();
        let desktop = manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        let server = manager.open(open_input(json!({"threadId":"t1","runtime":"server"}))).unwrap();
        assert!(!desktop.tab_id.as_str().contains(manager.server_epoch()));
        assert!(server.tab_id.as_str().ends_with(&format!("_{}", manager.server_epoch())));
    }

    #[test]
    fn the_owner_is_kept_for_a_server_tab_only_and_the_hook_runs_before_the_event() {
        let manager = manager();
        let mut events_ = manager.subscribe();
        let mut seen = None;
        let server = manager
            .open_with(open_input(json!({"threadId":"t1","runtime":"server","reveal":true})), Some("env\u{0}session".into()), |snapshot| {
                seen = Some(snapshot.tab_id.as_str().to_owned());
                assert!(events_.try_recv().is_err(), "the hook runs before the event is published");
            })
            .unwrap();
        assert_eq!(seen.as_deref(), Some(server.tab_id.as_str()));
        let wire = serde_json::to_value(&server).unwrap();
        assert_eq!((wire["automationOwner"].as_str(), wire["reveal"].as_bool()), (Some("env\u{0}session"), Some(true)));
        let desktop = manager.open_with(open_input(json!({"threadId":"t1"})), Some("owner".into()), |_| {}).unwrap();
        assert!(serde_json::to_value(&desktop).unwrap().get("automationOwner").is_none());
    }

    #[test]
    fn a_profile_and_a_viewport_given_at_open_are_kept_across_navigation_and_status() {
        let manager = manager();
        let tab = manager.open(open_input(json!({"threadId":"t1","profileId":"work","viewport":{"_tag":"freeform","width":800,"height":600}}))).unwrap();
        let id = tab.tab_id.as_str();
        manager.navigate(input(json!({"threadId":"t1","tabId":id,"url":"https://example.com","resolvedTitle":"Example"}))).unwrap();
        manager.report_status(input(json!({"threadId":"t1","tabId":id,"navStatus":{"_tag":"Success","url":"https://example.com/","title":"Example"},"canGoBack":true,"canGoForward":false})), false).unwrap();
        let wire = serde_json::to_value(&list(&manager, "t1").sessions[0]).unwrap();
        assert_eq!(wire["profileId"], "work");
        assert_eq!(wire["viewport"], json!({"_tag":"freeform","width":800,"height":600}));
        assert_eq!(wire["canGoBack"], true);
    }

    #[test]
    fn navigating_keeps_the_previous_title_unless_a_new_one_is_given() {
        let manager = manager();
        let tab = manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        let id = tab.tab_id.as_str();
        let first = manager.navigate(input(json!({"threadId":"t1","tabId":id,"url":"example.com/a","resolvedTitle":"First"}))).unwrap();
        assert_eq!(serde_json::to_value(&first.nav_status).unwrap(), json!({"_tag":"Success","url":"https://example.com/a","title":"First"}));
        let second = manager.navigate(input(json!({"threadId":"t1","tabId":id,"url":"example.com/b"}))).unwrap();
        assert_eq!(second.nav_status.title(), "First");
    }

    #[test]
    fn a_url_that_cannot_be_normalized_is_refused_without_echoing_it() {
        let manager = manager();
        let mut events_ = manager.subscribe();
        let secret = "https://user:password@example.com:bad/path?access_token=secret#fragment";
        let wire = |error: PreviewError| serde_json::to_value(error).unwrap();
        let error = wire(manager.open(open_input(json!({"threadId":"t1","url":secret}))).unwrap_err());
        assert_eq!(error["reason"], "parse");
        assert_eq!(error["protocol"], "https:");
        assert_eq!(error["inputLength"], secret.len());
        assert_eq!(error["cause"], json!({"name":"PreviewUrlNormalizationError","message":"Invalid preview URL (parse: https:; input length 71).","cause":{"name":"TypeError","message":"Invalid URL"}}));
        let rendered = error.to_string();
        assert!(!["user", "password", "access_token", "fragment"].iter().any(|word| rendered.replace("PreviewUrlNormalizationError", "").contains(word)), "{rendered}");
        let unsupported = wire(manager.open(open_input(json!({"threadId":"t1","url":"ftp://example.com"}))).unwrap_err());
        assert_eq!((unsupported["reason"].as_str(), unsupported["protocol"].as_str()), (Some("unsupported-protocol"), Some("ftp:")));
        assert!(unsupported["cause"].get("cause").is_none());
        assert!(events(&mut events_).is_empty() && list(&manager, "t1").sessions.is_empty());
        assert_eq!(list(&manager, "t1").revision.0, 0);
    }

    #[test]
    fn normalizing_matches_the_originals_table() {
        let ok = |raw: &str, expected: &str| assert_eq!(normalize_preview_url(raw).as_deref(), Ok(expected), "{raw}");
        ok("localhost:5173", "http://localhost:5173/");
        ok("127.0.0.1:3000", "http://127.0.0.1:3000/");
        ok("LOCALHOST:5173/x", "http://localhost:5173/x");
        ok("localhost", "http://localhost/");
        ok("[::1]:3000", "http://[::1]:3000/");
        ok("0.0.0.0:1", "http://0.0.0.0:1/");
        ok("example.com", "https://example.com/");
        ok("example.com:8080", "https://example.com:8080/");
        ok("localhost.example.com", "https://localhost.example.com/");
        ok("//x.com", "https://x.com/");
        ok("a.b/c d", "https://a.b/c%20d");
        ok("HTTP://EXAMPLE.com:80/a b?x=1#h", "http://example.com/a%20b?x=1#h");
        ok("https://localhost:5173", "https://localhost:5173/");
        ok("http://example.com/path?q=1", "http://example.com/path?q=1");
        ok("http://u@h/", "http://u@h/");
        ok("https://例え.jp", "https://xn--r8jz45g.jp/");
        ok("http://127.1/", "http://127.0.0.1/");
        ok("  http://x.com  ", "http://x.com/");
        ok("mailto:x@y.z", "https://mailto:x@y.z/");
        let err = |raw: &str, reason, protocol: Option<&str>| {
            let error = normalize_preview_url(raw).unwrap_err();
            assert_eq!((error.reason, error.protocol.as_deref(), error.input_length), (reason, protocol, raw.encode_utf16().count()), "{raw}");
        };
        err("   ", PreviewInvalidUrlReason::Empty, None);
        err("ftp://example.com", PreviewInvalidUrlReason::UnsupportedProtocol, Some("ftp:"));
        err("file:///etc/passwd", PreviewInvalidUrlReason::UnsupportedProtocol, Some("file:"));
        err("view-source:http://x.com", PreviewInvalidUrlReason::UnsupportedProtocol, Some("view-source:"));
        err("javascript:alert(1)", PreviewInvalidUrlReason::Parse, Some("https:"));
        err("data:text/html,hi", PreviewInvalidUrlReason::Parse, Some("https:"));
        err("about:blank", PreviewInvalidUrlReason::Parse, Some("https:"));
        err("1.2.3.4:abc", PreviewInvalidUrlReason::Parse, Some("https:"));
        err("http://exa mple.com", PreviewInvalidUrlReason::Parse, Some("http:"));
        err("http://[::1", PreviewInvalidUrlReason::Parse, Some("http:"));
        err("x y", PreviewInvalidUrlReason::Parse, Some("https:"));
        // No scheme can be read from a candidate that starts with "://".
        err("://x", PreviewInvalidUrlReason::Parse, None);
    }

    #[test]
    fn navigating_an_unknown_tab_fails_and_so_does_resizing_it() {
        let manager = manager();
        let lookup = manager.navigate(input(json!({"threadId":"t1","tabId":"missing","url":"https://example.com"}))).unwrap_err();
        assert_eq!(serde_json::to_value(&lookup).unwrap(), json!({"_tag":"PreviewSessionLookupError","threadId":"t1","tabId":"missing"}));
        assert_eq!(lookup.message(), "Unknown preview session: thread=t1, tab=missing");
        let resize = manager.resize(input(json!({"threadId":"t1","tabId":"missing","viewport":{"_tag":"fill"}}))).unwrap_err();
        assert_eq!(error_tag(resize), "PreviewSessionLookupError");
        assert_eq!(error_tag(manager.refresh(input(json!({"threadId":"t1","tabId":"missing"}))).unwrap_err()), "PreviewSessionLookupError");
    }

    #[test]
    fn a_url_error_comes_before_the_lookup() {
        let manager = manager();
        let error = manager.navigate(input(json!({"threadId":"t1","tabId":"missing","url":"ftp://example.com"}))).unwrap_err();
        assert_eq!(error_tag(error), "PreviewInvalidUrlError");
    }

    #[test]
    fn a_client_cannot_navigate_or_report_for_a_server_tab_and_nothing_changes() {
        let manager = manager();
        let tab = manager.open(open_input(json!({"threadId":"t1","runtime":"server","reveal":false}))).unwrap();
        let id = tab.tab_id.as_str();
        let mut events_ = manager.subscribe();
        let before = serde_json::to_value(list(&manager, "t1")).unwrap();
        let report = json!({"threadId":"t1","tabId":id,"navStatus":{"_tag":"Success","url":"http://localhost:5173/changed","title":"Changed"},"canGoBack":true,"canGoForward":true});
        for refused in [
            manager.report_status(input(report.clone()), false).unwrap_err(),
            manager.navigate(input(json!({"threadId":"t1","tabId":id,"url":"https://example.com"}))).unwrap_err(),
        ] {
            assert_eq!(serde_json::to_value(&refused).unwrap(), json!({"_tag":"PreviewControlRequiredError","tabId":id}));
            assert_eq!(refused.message(), "Take control of the server browser and use its viewer controls.");
        }
        assert_eq!(serde_json::to_value(list(&manager, "t1")).unwrap(), before);
        assert!(events(&mut events_).is_empty());
        // The server's own browser reports with control.
        manager.report_status(input(report), true).unwrap();
        let published = events(&mut events_);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0]["type"], "navigated");
        assert_eq!(published[0]["snapshot"]["navStatus"]["title"], "Changed");
    }

    #[test]
    fn a_failed_load_publishes_failed_and_still_records_the_status() {
        let manager = manager();
        let tab = manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        let mut events_ = manager.subscribe();
        manager
            .report_status(input(json!({"threadId":"t1","tabId":tab.tab_id.as_str(),"navStatus":{"_tag":"LoadFailed","url":"http://localhost:1/","title":"","code":-102,"description":"ERR_CONNECTION_REFUSED"},"canGoBack":false,"canGoForward":false})), false)
            .unwrap();
        let published = events(&mut events_);
        assert_eq!(published.len(), 1);
        assert_eq!((published[0]["type"].as_str(), published[0]["code"].as_i64(), published[0]["description"].as_str()), (Some("failed"), Some(-102), Some("ERR_CONNECTION_REFUSED")));
        assert!(published[0].get("snapshot").is_none());
        assert_eq!(serde_json::to_value(&list(&manager, "t1").sessions[0].nav_status).unwrap()["_tag"], "LoadFailed");
    }

    #[test]
    fn any_client_can_size_theme_and_zoom_a_server_tab() {
        let manager = manager();
        let tab = manager.open(open_input(json!({"threadId":"t1","runtime":"server"}))).unwrap();
        let id = tab.tab_id.as_str();
        let mut events_ = manager.subscribe();
        let resized = manager.resize(input(json!({"threadId":"t1","tabId":id,"viewport":{"_tag":"preset","width":390,"height":844,"presetId":"iphone-12-pro"}}))).unwrap();
        assert_eq!(serde_json::to_value(&resized).unwrap()["viewport"]["presetId"], "iphone-12-pro");
        let adjusted = manager.adjust(input(json!({"threadId":"t1","tabId":id,"colorScheme":"dark","zoomFactor":1.5}))).unwrap();
        let wire = serde_json::to_value(&adjusted).unwrap();
        assert_eq!((wire["colorScheme"].as_str(), wire["zoomFactor"].as_f64()), (Some("dark"), Some(1.5)));
        // A resize keeps what an adjustment set, and the reverse.
        assert_eq!(serde_json::to_value(&adjusted).unwrap()["viewport"]["presetId"], "iphone-12-pro");
        let published = events(&mut events_);
        assert_eq!(published.iter().map(|event| event["type"].as_str().unwrap()).collect::<Vec<_>>(), ["resized", "resized"]);
        assert!(published[1].get("request").is_none());
    }

    #[test]
    fn a_hard_reload_and_a_clear_ride_on_the_resized_event_once() {
        let manager = manager();
        let tab = manager.open(open_input(json!({"threadId":"t1","runtime":"server"}))).unwrap();
        let id = tab.tab_id.as_str();
        let mut events_ = manager.subscribe();
        manager.adjust(input(json!({"threadId":"t1","tabId":id,"hardReload":true}))).unwrap();
        manager.adjust(input(json!({"threadId":"t1","tabId":id,"hardReload":true,"clear":"cookies"}))).unwrap();
        manager.adjust(input(json!({"threadId":"t1","tabId":id,"hardReload":false}))).unwrap();
        let published = events(&mut events_);
        assert_eq!(published[0]["request"], json!({"hardReload":true}));
        assert_eq!(published[1]["request"], json!({"hardReload":true,"clear":"cookies"}));
        assert!(published[2].get("request").is_none());
        // The request is not part of the session.
        assert!(serde_json::to_value(&list(&manager, "t1").sessions[0]).unwrap().get("request").is_none());
    }

    #[test]
    fn a_reveal_request_is_reissued_each_time_and_not_replayed_by_navigation() {
        let manager = manager();
        let tab = manager.open(open_input(json!({"threadId":"t1","runtime":"server"}))).unwrap();
        let id = tab.tab_id.as_str();
        manager.request_reveal("t1", id, false).unwrap();
        let first = serde_json::to_value(&list(&manager, "t1").sessions[0]).unwrap();
        manager.request_reveal("t1", id, true).unwrap();
        let second = serde_json::to_value(&list(&manager, "t1").sessions[0]).unwrap();
        assert_eq!((first["reveal"].as_bool(), first["revealRequest"]["force"].as_bool()), (Some(true), Some(false)));
        assert_eq!(second["revealRequest"]["force"], true);
        assert_ne!(first["revealRequest"]["id"], second["revealRequest"]["id"]);
        assert_eq!(error_tag(manager.request_reveal("t1", "missing", false).unwrap_err()), "PreviewSessionLookupError");
    }

    #[test]
    fn refresh_publishes_nothing() {
        let manager = manager();
        let tab = manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        let mut events_ = manager.subscribe();
        let before = list(&manager, "t1").revision;
        manager.refresh(input(json!({"threadId":"t1","tabId":tab.tab_id.as_str()}))).unwrap();
        assert!(events(&mut events_).is_empty());
        assert_eq!(list(&manager, "t1").revision, before);
    }

    #[test]
    fn list_and_events_share_one_rising_revision() {
        let manager = manager();
        let mut events_ = manager.subscribe();
        let a = manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        let b = manager.open(open_input(json!({"threadId":"t2"}))).unwrap();
        manager.navigate(input(json!({"threadId":"t1","tabId":a.tab_id.as_str(),"url":"example.com"}))).unwrap();
        manager.close(input(json!({"threadId":"t2","tabId":b.tab_id.as_str()})));
        let revisions: Vec<u64> = events(&mut events_).iter().map(|event| event["revision"].as_u64().unwrap()).collect();
        assert_eq!(revisions, [1, 2, 3, 4]);
        let listed = list(&manager, "t1");
        assert_eq!((listed.revision.0, listed.server_epoch.as_str()), (4, manager.server_epoch()));
        assert_eq!(listed.sessions.len(), 1);
    }

    #[test]
    fn closing_removes_what_it_names_and_each_closed_tab_has_its_own_revision() {
        let manager = manager();
        let a = manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        manager.open(open_input(json!({"threadId":"t2"}))).unwrap();
        let mut events_ = manager.subscribe();
        // A tab that is not there, and a thread that is not there, change nothing.
        manager.close(input(json!({"threadId":"t1","tabId":"missing"})));
        manager.close(input(json!({"threadId":"nobody"})));
        assert!(events(&mut events_).is_empty());
        manager.close(input(json!({"threadId":"t1","tabId":a.tab_id.as_str()})));
        assert_eq!(list(&manager, "t1").sessions.len(), 1);
        manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        events(&mut events_);
        manager.close(input(json!({"threadId":"t1"})));
        let closed = events(&mut events_);
        assert_eq!(closed.iter().map(|event| event["type"].as_str().unwrap()).collect::<Vec<_>>(), ["closed", "closed"]);
        assert_eq!(closed[1]["revision"].as_u64().unwrap(), closed[0]["revision"].as_u64().unwrap() + 1);
        assert_eq!(closed[0]["createdAt"], closed[1]["createdAt"]);
        assert!(list(&manager, "t1").sessions.is_empty());
        assert_eq!(list(&manager, "t2").sessions.len(), 1);
    }

    #[test]
    fn list_orders_a_threads_tabs_by_their_last_change() {
        let tick = Arc::new(Mutex::new(0i64));
        let clock = {
            let tick = tick.clone();
            Arc::new(move || {
                let mut tick = tick.lock().unwrap();
                *tick += 1;
                DateTime::from_timestamp_millis(1_700_000_000_000 + *tick).unwrap()
            })
        };
        let manager = PreviewManager::with_clock(clock);
        let first = manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        let second = manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        manager.navigate(input(json!({"threadId":"t1","tabId":first.tab_id.as_str(),"url":"example.com"}))).unwrap();
        let order: Vec<&str> = list(&manager, "t1").sessions.iter().map(|session| session.tab_id.as_str().to_owned()).collect::<Vec<_>>().iter().map(|id| if id == second.tab_id.as_str() { "second" } else { "first" }).collect();
        assert_eq!(order, ["second", "first"]);
        let stamp = serde_json::to_value(&list(&manager, "t1").sessions[0]).unwrap()["updatedAt"].as_str().unwrap().to_owned();
        assert_eq!(stamp, "2023-11-14T22:13:20.002Z");
    }

    #[test]
    fn every_subscriber_gets_every_event_and_a_gone_one_is_dropped() {
        let manager = manager();
        let mut a = manager.subscribe();
        let b = manager.subscribe();
        let mut c = manager.subscribe();
        drop(b);
        manager.open(open_input(json!({"threadId":"t1"}))).unwrap();
        assert_eq!((events(&mut a).len(), events(&mut c).len()), (1, 1));
        assert_eq!(manager.lock().subscribers.len(), 2);
        // A subscriber that joins late sees only what follows.
        let mut late = manager.subscribe();
        assert!(events(&mut late).is_empty());
        manager.close(input(json!({"threadId":"t1"})));
        assert_eq!(events(&mut late).len(), 1);
    }

    #[test]
    fn tab_ids_count_in_base_36() {
        let first = new_preview_tab_id();
        let second = new_preview_tab_id();
        assert!(first.starts_with("tab_") && first != second);
        assert!(first[4..].chars().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()));
    }
}
