//! Carries an agent's browser tool calls to the browser host that runs them (apps/server/src/mcp/PreviewAutomationBroker.ts).
//!
//! A host registers with `connect` and reads its requests from the stream that returns. A tool call is routed to one host,
//! `invoke` waits for the host's answer (`respond`) and fails if it is late, goes away or answers wrongly. Routing keeps one provider
//! session on one host for as long as that host's connection lives, so a multi-step interaction cannot jump between two browsers'
//! cookies and DOM; a session's current tab is remembered with it. The server's own browser registers as a preferred host.
use crate::mcp_invocation::{InvocationScope, ThreadCaller};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::{
    PreviewAutomationError, PreviewAutomationHost, PreviewAutomationHostFocus, PreviewAutomationOperation, PreviewAutomationRemoteError, PreviewAutomationRequest,
    PreviewAutomationResponse, PreviewAutomationStreamEvent, PreviewTabId, Present, PREVIEW_AUTOMATION_V1_OPERATIONS, SERVER_BROWSER_AUTOMATION_CLIENT_ID,
};
use tokio::sync::{Notify, oneshot};

/// How long a host gets to answer when the call names no budget.
const DEFAULT_TIMEOUT_MS: u64 = 15_000;
/// The environment's own browser may install Chromium on its first open (up to ten minutes). That request is kept alive without replaying it.
const FIRST_OPEN_TIMEOUT_MS: u64 = 660_000;
/// Enough of a browser's own error line, such as a refused connection and its URL.
const MAX_REASON_CHARS: usize = 500;

/// One call to run on a host.
pub struct Invocation<'a> {
    /// Preview tabs belong to a thread, so only a thread's credential reaches the broker.
    pub scope: &'a InvocationScope,
    pub operation: PreviewAutomationOperation,
    pub input: Value,
    pub tab_id: Option<PreviewTabId>,
    pub timeout_ms: Option<u64>,
    /// A background read of metadata must not change the agent's current tab, nor cut the host off when it is slow.
    pub update_current_tab: bool,
}

impl<'a> Invocation<'a> {
    pub fn new(scope: &'a InvocationScope, operation: PreviewAutomationOperation, input: Value) -> Self {
        Self { scope, operation, input, tab_id: None, timeout_ms: None, update_current_tab: true }
    }
}

/// What a host answered and the tab the request was routed to.
#[derive(Debug, Clone, PartialEq)]
pub struct Invoked {
    pub result: Value,
    /// The tab the request named or the session's current tab, as it was when the request was routed.
    pub target_tab: Option<PreviewTabId>,
}

/// The requests of one host, in order. A queue ends when the host is replaced, evicted or disconnected.
struct Queue {
    state: Mutex<(VecDeque<PreviewAutomationStreamEvent>, bool)>,
    wake: Notify,
}

impl Queue {
    fn new() -> Self {
        Self { state: Mutex::new((VecDeque::new(), false)), wake: Notify::new() }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, (VecDeque<PreviewAutomationStreamEvent>, bool)> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    /// Whether the event was queued; a closed queue takes nothing.
    fn offer(&self, event: PreviewAutomationStreamEvent) -> bool {
        let mut state = self.lock();
        if state.1 {
            return false;
        }
        state.0.push_back(event);
        drop(state);
        self.wake.notify_one();
        true
    }
    /// Drops what has not been read and ends the stream, so a host that was replaced or evicted never acts on a stale request.
    fn close(&self) {
        let mut state = self.lock();
        state.0.clear();
        state.1 = true;
        drop(state);
        self.wake.notify_one();
    }
}

/// A registered host's end of the broker: the stream of what to run. Dropping it unregisters the host.
pub struct HostConnection {
    broker: PreviewAutomationBroker,
    client_id: String,
    connection_id: String,
    queue: Arc<Queue>,
}

impl HostConnection {
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }
    /// The next event, or none once the broker ended the stream (the host was replaced or evicted) or the broker is gone.
    pub async fn next(&mut self) -> Option<PreviewAutomationStreamEvent> {
        loop {
            let notified = self.queue.wake.notified();
            {
                let mut state = self.queue.lock();
                if let Some(event) = state.0.pop_front() {
                    return Some(event);
                }
                if state.1 {
                    return None;
                }
            }
            notified.await;
        }
    }
}

impl Drop for HostConnection {
    fn drop(&mut self) {
        self.broker.disconnect(&self.client_id, &self.connection_id, false);
    }
}

struct Client {
    client_id: String,
    connection_id: String,
    environment_id: String,
    supported_operations: Vec<PreviewAutomationOperation>,
    focused: bool,
    preferred: bool,
    live_tabs: Vec<LiveTab>,
    focus_order: u64,
    queue: Arc<Queue>,
}

struct LiveTab {
    thread_id: String,
    tab_id: String,
    visible: Option<bool>,
}

/// A lease pinning one provider session to one host. It lives exactly as long as the connection it names, so a host that disconnected or
/// was replaced is dropped on the next lookup. It has no clock of its own: it once inherited the MCP credential's expiry, which tied a
/// host to an unrelated auth deadline and could move a live session to another host mid-flow.
#[derive(Clone)]
struct Assignment {
    client_id: String,
    connection_id: String,
    tab_id: Option<String>,
    tab_sequence: Option<u64>,
}

/// What an error about a request says about where it was running.
#[derive(Clone)]
struct RequestContext {
    operation: PreviewAutomationOperation,
    environment_id: String,
    thread_id: String,
    provider_session_id: String,
    provider_instance_id: String,
    client_id: String,
    connection_id: String,
    request_id: String,
    tab_id: Option<String>,
    timeout_ms: u64,
    selector_kind: Option<&'static str>,
    selector_length: Option<usize>,
}

struct Pending {
    connection_id: String,
    answer: oneshot::Sender<Result<Value, PreviewAutomationError>>,
    context: RequestContext,
}

#[derive(Default)]
struct State {
    clients: HashMap<String, Client>,
    assignments: HashMap<String, Assignment>,
    pending: HashMap<String, Pending>,
    request_sequence: u64,
    focus_sequence: u64,
}

/// The broker of one environment. Cloning shares it.
#[derive(Clone, Default)]
pub struct PreviewAutomationBroker(Arc<Mutex<State>>);

fn error_of(value: Value) -> PreviewAutomationError {
    serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("a broker error is a valid contract error ({error}): {value}"))
}

fn assignment_key(environment_id: &str, thread: &ThreadCaller) -> String {
    format!("{environment_id}\u{0}{}", thread.provider_session_id)
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn detail_kind(detail: &Value) -> &'static str {
    match detail {
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Object(_) => "object",
    }
}

/// The first `max` UTF-16 units of `text`, never splitting a character.
fn cut_utf16(text: &str, max: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect()
}

fn selector_diagnostics(input: &Value) -> (Option<&'static str>, Option<usize>) {
    if let Some(locator) = input.get("locator").and_then(Value::as_str) {
        return (Some("locator"), Some(utf16_len(locator)));
    }
    if let Some(selector) = input.get("selector").and_then(Value::as_str) {
        return (Some("selector"), Some(utf16_len(selector)));
    }
    (None, None)
}

impl RequestContext {
    /// The members every error about a request carries.
    fn fields(&self) -> Value {
        let mut fields = json!({
            "operation": self.operation.as_str(),
            "environmentId": self.environment_id,
            "threadId": self.thread_id,
            "providerSessionId": self.provider_session_id,
            "providerInstanceId": self.provider_instance_id,
            "clientId": self.client_id,
            "connectionId": self.connection_id,
            "requestId": self.request_id,
            "timeoutMs": self.timeout_ms,
        });
        if let Some(tab_id) = &self.tab_id {
            fields["tabId"] = json!(tab_id);
        }
        fields
    }
    fn error(&self, tag: &str) -> PreviewAutomationError {
        let mut fields = self.fields();
        fields["_tag"] = json!(tag);
        error_of(fields)
    }
}

/// What the original makes of a host's failure: the typed error for the tags it knows, with the host's own text kept out of the agent's
/// view, and an execution error for any other tag.
fn classify(context: &RequestContext, error: &PreviewAutomationRemoteError) -> PreviewAutomationError {
    let message = error.message.as_str();
    let detail = error.detail.present();
    let tag = error.tag.as_str();
    let thread_only = |tag: &str| error_of(json!({"_tag":tag,"threadId":context.thread_id,"cause":serde_json::to_value(error).expect("an error is JSON")}));
    let mut fields = context.fields();
    fields["remoteTag"] = json!(tag);
    fields["remoteMessageLength"] = json!(utf16_len(message));
    if let Some(detail) = detail {
        fields["remoteDetailKind"] = json!(detail_kind(detail));
    }
    fields["cause"] = serde_json::to_value(error).expect("an error is JSON");
    let finish = |tag: &str, mut fields: Value| {
        fields["_tag"] = json!(tag);
        error_of(fields)
    };
    match tag {
        "PreviewAutomationRecordingDesktopUpdateRequiredError"
        | "PreviewAutomationRecordingTooLargeError"
        | "PreviewAutomationRecordingDeadlineExpiredError"
        | "PreviewAutomationRecordingTransferError" => thread_only(tag),
        "PreviewAutomationNoAvailableHostError" | "PreviewAutomationUnsupportedClientError" | "PreviewAutomationTabNotFoundError" | "PreviewAutomationTimeoutError" => {
            finish(tag, fields)
        }
        "PreviewAutomationControlInterruptedError" => {
            let reason = detail.and_then(Value::as_str).filter(|reason| {
                ["agentMismatch", "humanControl", "tabRequired", "closed", "interrupted", "dialogPending", "tabLimit"].contains(reason)
            });
            if let Some(reason) = reason {
                fields["reason"] = json!(reason);
            }
            finish(tag, fields)
        }
        "PreviewAutomationInvalidSelectorError" => {
            if detail.and_then(|detail| detail.get("staleRef")) == Some(&Value::Bool(true)) {
                fields["staleRef"] = json!(true);
            }
            if let Some(kind) = context.selector_kind {
                fields["selectorKind"] = json!(kind);
            }
            if let Some(length) = context.selector_length {
                fields["selectorLength"] = json!(length);
            }
            finish(tag, fields)
        }
        "PreviewAutomationTargetNotEditableError" => {
            let remote_kind = detail
                .and_then(|detail| detail.get("selectorKind"))
                .and_then(Value::as_str)
                .filter(|kind| ["focused-element", "locator", "selector"].contains(kind));
            let remote_length = detail.and_then(|detail| detail.get("selectorLength")).and_then(Value::as_u64);
            if let Some(kind) = remote_kind.or(context.selector_kind) {
                fields["selectorKind"] = json!(kind);
            }
            if let Some(length) = remote_length.or(context.selector_length.map(|length| length as u64)) {
                fields["selectorLength"] = json!(length);
            }
            finish(tag, fields)
        }
        "PreviewAutomationResultTooLargeError" => {
            if let Some(bytes) = detail.and_then(|detail| detail.get("maximumBytes")).and_then(Value::as_u64).filter(|bytes| *bytes > 0) {
                fields["maximumBytes"] = json!(bytes);
            }
            finish(tag, fields)
        }
        "PreviewAutomationUnavailableError" => finish("PreviewAutomationRemoteUnavailableError", fields),
        _ => {
            // The server's own browser writes these; other hosts' text stays out of the agent's context.
            if context.client_id == SERVER_BROWSER_AUTOMATION_CLIENT_ID {
                fields["reason"] = json!(cut_utf16(message, MAX_REASON_CHARS));
            }
            finish("PreviewAutomationExecutionError", fields)
        }
    }
}

/// A result that names a tab: `Some(None)` for a null tab (the session has no current tab now), `None` for no tab to learn.
fn result_tab_id(result: &Value) -> Option<Option<String>> {
    let tab = result.as_object()?.get("tabId")?;
    match tab {
        Value::Null => Some(None),
        Value::String(_) => serde_json::from_value::<PreviewTabId>(tab.clone()).ok().map(|id| Some(id.as_str().to_owned())),
        _ => None,
    }
}

impl PreviewAutomationBroker {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Takes a client's connection out of `state` with the assignments and pending requests that belong to it. The caller closes its queue
    /// and tells the requests it was disconnected.
    fn remove_connection(state: &mut State, client_id: &str, connection_id: &str) -> Vec<Pending> {
        if state.clients.get(client_id).is_some_and(|client| client.connection_id == connection_id) {
            state.clients.remove(client_id);
        }
        state.assignments.retain(|_, assignment| assignment.connection_id != connection_id);
        let ids: Vec<String> = state.pending.iter().filter(|(_, entry)| entry.connection_id == connection_id).map(|(id, _)| id.clone()).collect();
        ids.into_iter().filter_map(|id| state.pending.remove(&id)).collect()
    }

    fn fail_disconnected(disconnected: Vec<Pending>) {
        for entry in disconnected {
            let error = entry.context.error("PreviewAutomationClientDisconnectedError");
            let _ = entry.answer.send(Err(error));
        }
    }

    /// Registers a host. A host that registers again replaces its earlier registration: the earlier stream ends and its requests fail as disconnected.
    /// A preferred host takes new work before any other.
    pub fn connect(&self, host: PreviewAutomationHost, preferred: bool) -> HostConnection {
        let client_id = host.client_id.as_str().to_owned();
        let connection_id = uuid::Uuid::new_v4().to_string();
        let queue = Arc::new(Queue::new());
        let connected = PreviewAutomationStreamEvent::Connected(
            serde_json::from_value(json!({"type":"connected","connectionId":connection_id})).expect("a connected event"),
        );
        queue.offer(connected);
        let operations = match host.supported_operations {
            Some(Some(operations)) => operations,
            _ => PREVIEW_AUTOMATION_V1_OPERATIONS.to_vec(),
        };
        let (previous, disconnected) = {
            let mut state = self.lock();
            let previous = state.clients.get(&client_id).map(|client| (client.connection_id.clone(), client.queue.clone()));
            let disconnected = previous.as_ref().map(|(id, _)| Self::remove_connection(&mut state, &client_id, id)).unwrap_or_default();
            state.focus_sequence += 1;
            let focus_order = state.focus_sequence;
            state.clients.insert(
                client_id.clone(),
                Client {
                    client_id: client_id.clone(),
                    connection_id: connection_id.clone(),
                    environment_id: host.environment_id.as_str().to_owned(),
                    supported_operations: operations,
                    focused: false,
                    preferred,
                    live_tabs: Vec::new(),
                    focus_order,
                    queue: queue.clone(),
                },
            );
            (previous, disconnected)
        };
        if let Some((_, previous_queue)) = previous {
            // The replaced registration must not reconnect and displace its successor.
            previous_queue.close();
            Self::fail_disconnected(disconnected);
        }
        HostConnection { broker: self.clone(), client_id, connection_id, queue }
    }

    /// Ends a host's connection. `complete_stream` is for a host evicted for answering too late: its stream completes so a responsive host can
    /// register again. A connection that has been replaced already was closed, and is left alone.
    fn disconnect(&self, client_id: &str, connection_id: &str, _complete_stream: bool) {
        let (queue, disconnected) = {
            let mut state = self.lock();
            let Some(queue) = state.clients.get(client_id).filter(|client| client.connection_id == connection_id).map(|client| client.queue.clone()) else {
                return;
            };
            let disconnected = Self::remove_connection(&mut state, client_id, connection_id);
            (queue, disconnected)
        };
        queue.close();
        Self::fail_disconnected(disconnected);
    }

    /// Records which host's window is focused and which tabs it shows. A report about a connection that is not the live one is ignored.
    pub fn focus_host(&self, host: PreviewAutomationHostFocus) {
        let mut guard = self.lock();
        let state = &mut *guard;
        let Some(client) = state.clients.get_mut(host.client_id.as_str()) else { return };
        if client.environment_id != host.environment_id.as_str() || client.connection_id != host.connection_id.as_str() {
            return;
        }
        client.focused = host.focused;
        if let Some(Some(tabs)) = host.live_tabs {
            client.live_tabs = tabs
                .into_iter()
                .map(|tab| LiveTab { thread_id: tab.thread_id.as_str().to_owned(), tab_id: tab.tab_id.as_str().to_owned(), visible: tab.visible.flatten() })
                .collect();
        }
        if host.focused {
            state.focus_sequence += 1;
            client.focus_order = state.focus_sequence;
        }
    }

    /// A host's answer to a request. One that is not from the connection the request went to, or for a request no longer waiting, is ignored.
    pub fn respond(&self, response: PreviewAutomationResponse) {
        let entry = {
            let mut state = self.lock();
            let matches = state
                .pending
                .get(response.request_id.as_str())
                .is_some_and(|entry| entry.context.client_id == response.client_id.as_str() && entry.context.connection_id == response.connection_id.as_str());
            if !matches {
                return;
            }
            state.pending.remove(response.request_id.as_str())
        };
        let Some(entry) = entry else { return };
        let outcome = if response.ok {
            Ok(response.result.flatten().unwrap_or(Value::Null))
        } else {
            match response.error.flatten() {
                Some(error) => Err(classify(&entry.context, &error)),
                None => Err(entry.context.error("PreviewAutomationMalformedResponseError")),
            }
        };
        let _ = entry.answer.send(outcome);
    }

    /// Runs one operation on a host and waits for its answer.
    pub async fn invoke(&self, invocation: Invocation<'_>) -> Result<Invoked, PreviewAutomationError> {
        let scope = invocation.scope;
        let thread = scope.thread.as_ref().expect("preview calls come from a thread's credential");
        let key = assignment_key(scope.environment_id.as_str(), thread);
        let operation = invocation.operation;
        let (answer, answered) = oneshot::channel();
        let Some(route) = self.route(&invocation, &key, thread, answer) else {
            return Err(error_of(json!({
                "_tag": "PreviewAutomationNoAvailableHostError",
                "operation": operation.as_str(),
                "environmentId": scope.environment_id,
                "threadId": thread.thread_id,
                "providerSessionId": thread.provider_session_id,
                "providerInstanceId": thread.provider_instance_id,
            })));
        };
        let target_tab = route.context.tab_id.as_deref().and_then(|id| PreviewTabId::new(id).ok());
        let outcome = self.deliver(&invocation, &route, answered).await;
        // However the wait ended, the request is no longer waiting.
        self.lock().pending.remove(&route.context.request_id);
        let result = outcome?;
        if invocation.update_current_tab {
            self.learn_tab(&key, &route, invocation.tab_id.as_ref(), &result);
        }
        Ok(Invoked { result, target_tab })
    }

    /// Picks the host, registers the request as waiting and records the session's assignment, all in one step.
    fn route(&self, invocation: &Invocation<'_>, key: &str, thread: &ThreadCaller, answer: oneshot::Sender<Result<Value, PreviewAutomationError>>) -> Option<Route> {
        let scope = invocation.scope;
        let environment_id = scope.environment_id.as_str();
        let thread_id = thread.thread_id.as_str();
        let operation = invocation.operation;
        let explicit_tab = invocation.tab_id.as_ref().map(PreviewTabId::as_str);
        let mut state = self.lock();
        // A lease that outlived its connection is dropped.
        let live: Vec<String> = state
            .assignments
            .iter()
            .filter(|(_, assignment)| state.clients.get(&assignment.client_id).is_some_and(|client| client.connection_id == assignment.connection_id))
            .map(|(key, _)| key.clone())
            .collect();
        state.assignments.retain(|key, _| live.contains(key));
        let assigned = state.assignments.get(key).cloned();
        let assigned_client = assigned.as_ref().and_then(|assignment| state.clients.get(&assignment.client_id));
        let has_live_assignment = assigned_client.is_some_and(|client| client.environment_id == environment_id);
        // One provider session stays on one host, so a multi-step interaction cannot jump between two browsers' state. A live lease that
        // predates an operation is not moved to a newer host: the caller gets a capability failure and can start a fresh provider session.
        // A dead lease is dropped above and may fail over.
        let owns = |client: &Client, visible_only: bool| {
            client.live_tabs.iter().any(|tab| tab.thread_id == thread_id && (!visible_only || tab.visible == Some(true)) && explicit_tab.is_none_or(|id| tab.tab_id == id))
        };
        let chosen: Option<String> = if has_live_assignment {
            assigned_client.filter(|client| client.supported_operations.contains(&operation)).map(|client| client.client_id.clone())
        } else {
            state
                .clients
                .values()
                .filter(|client| client.environment_id == environment_id && client.supported_operations.contains(&operation))
                .max_by_key(|client| (explicit_tab.is_some() && owns(client, false), client.preferred, owns(client, true), owns(client, false), client.focused, client.focus_order))
                .map(|client| client.client_id.clone())
        };
        let Some(chosen) = chosen else {
            if !has_live_assignment {
                state.assignments.remove(key);
            }
            return None;
        };
        let (client_id, connection_id, preferred, queue) = {
            let client = &state.clients[&chosen];
            (client.client_id.clone(), client.connection_id.clone(), client.preferred, client.queue.clone())
        };
        let timeout_ms = invocation.timeout_ms.unwrap_or(if operation == PreviewAutomationOperation::Open && preferred { FIRST_OPEN_TIMEOUT_MS } else { DEFAULT_TIMEOUT_MS });
        let reuse = assigned.as_ref().filter(|assignment| assignment.connection_id == connection_id);
        state.assignments.insert(
            key.to_owned(),
            Assignment {
                client_id: client_id.clone(),
                connection_id: connection_id.clone(),
                tab_id: reuse.and_then(|assignment| assignment.tab_id.clone()),
                tab_sequence: reuse.and_then(|assignment| assignment.tab_sequence),
            },
        );
        let sequence = state.request_sequence;
        state.request_sequence += 1;
        let request_id = format!("preview-{sequence}");
        let tab_id = explicit_tab.map(str::to_owned).or_else(|| reuse.and_then(|assignment| assignment.tab_id.clone()));
        let (selector_kind, selector_length) = selector_diagnostics(&invocation.input);
        let context = RequestContext {
            operation,
            environment_id: environment_id.to_owned(),
            thread_id: thread_id.to_owned(),
            provider_session_id: thread.provider_session_id.clone(),
            provider_instance_id: thread.provider_instance_id.as_str().to_owned(),
            client_id,
            connection_id: connection_id.clone(),
            request_id: request_id.clone(),
            tab_id,
            timeout_ms,
            selector_kind,
            selector_length,
        };
        state.pending.insert(request_id, Pending { connection_id, answer, context: context.clone() });
        Some(Route { context, queue, sequence })
    }

    /// Offers the request to the host and waits for its answer, for as long as the budget allows.
    async fn deliver(&self, invocation: &Invocation<'_>, route: &Route, answered: oneshot::Receiver<Result<Value, PreviewAutomationError>>) -> Result<Value, PreviewAutomationError> {
        let context = &route.context;
        let mut answered = answered;
        let offered = {
            // A route can outlive its generation while another request evicts it: the live-generation check and the offer are one step with
            // the queue's closure.
            let state = self.lock();
            let current = state.clients.get(&context.client_id).is_some_and(|client| client.connection_id == context.connection_id);
            current && state.pending.contains_key(&context.request_id) && {
                let thread = invocation.scope.thread.as_ref().expect("a thread's credential");
                let mut request = json!({
                    "requestId": context.request_id,
                    "threadId": context.thread_id,
                    "tabIdExplicit": invocation.tab_id.is_some(),
                    "agentSessionId": assignment_key(&context.environment_id, thread),
                    "operation": context.operation.as_str(),
                    "input": invocation.input,
                    "timeoutMs": context.timeout_ms,
                });
                if let Some(tab_id) = &context.tab_id {
                    request["tabId"] = json!(tab_id);
                }
                let request: PreviewAutomationRequest = serde_json::from_value(request).expect("a request to a host is a valid request");
                route.queue.offer(PreviewAutomationStreamEvent::Request(
                    serde_json::from_value(json!({"type":"request","connectionId":context.connection_id,"request":request})).expect("a request event"),
                ))
            }
        };
        if !offered {
            return match answered.try_recv() {
                Ok(outcome) => outcome,
                Err(_) => Err(context.error("PreviewAutomationRequestQueueClosedError")),
            };
        }
        match tokio::time::timeout(Duration::from_millis(context.timeout_ms), &mut answered).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(context.error("PreviewAutomationRequestQueueClosedError")),
            Err(_) => {
                // An unanswered request invalidates the connection. Actions are not replayed: the host may have applied them before it became
                // unreachable. A background metadata read has a short budget and changes nothing, so a slow one must not cut the host off.
                if invocation.update_current_tab {
                    self.disconnect(&context.client_id, &context.connection_id, true);
                }
                Err(context.error("PreviewAutomationTimeoutError"))
            }
        }
    }

    /// Remembers the tab a result names (or the one the call named) as the session's current tab, unless a later request already decided.
    fn learn_tab(&self, key: &str, route: &Route, named: Option<&PreviewTabId>, result: &Value) {
        let learned = match result_tab_id(result) {
            Some(tab) => Some(tab),
            None => named.map(|tab| Some(tab.as_str().to_owned())),
        };
        let Some(tab) = learned else { return };
        let mut state = self.lock();
        let Some(assignment) = state.assignments.get_mut(key) else { return };
        if assignment.connection_id != route.context.connection_id || assignment.tab_sequence.is_some_and(|sequence| sequence > route.sequence) {
            return;
        }
        assignment.tab_id = tab;
        assignment.tab_sequence = Some(route.sequence);
    }
}

struct Route {
    context: RequestContext,
    queue: Arc<Queue>,
    sequence: u64,
}
