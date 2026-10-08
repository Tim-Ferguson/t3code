use crate::{AcpError, Peer, PeerEvent, RequestId, RpcError, normalize, schema};
use futures_util::{FutureExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    V1,
    V2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Client,
    Agent,
}
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub request_id: String,
    pub method: String,
    pub wire_id: RequestId,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Notification {
    SessionUpdate(Value),
    ElicitationComplete(Value),
    Extension { method: String, params: Value },
}
#[derive(Debug, Clone)]
pub enum ClientEvent {
    RequestHandlerFailed { request_id: String, error: AcpError },
    Notification(Notification),
    ResponseAcknowledged { request_id: String },
    ResponseFailed { request_id: String, error: AcpError },
    Terminated(AcpError),
}
pub type RequestHandler =
    Arc<dyn Fn(Value, RequestContext) -> BoxFuture<'static, Result<Value, RpcError>> + Send + Sync>;
pub type NotificationHandler =
    Arc<dyn Fn(Value) -> BoxFuture<'static, Result<(), AcpError>> + Send + Sync>;
pub type SessionUpdateTransform = Arc<dyn Fn(Value) -> Value + Send + Sync>;
#[derive(Clone, Default)]
pub struct ClientOptions {
    /// Receives the compatibility-normalized update; its result is retained
    /// in the raw stream and delivered to handlers without another decode.
    pub transform_session_update: Option<SessionUpdateTransform>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMethod {
    Authenticate,
    Logout,
    CreateSession,
    LoadSession,
    ListSessions,
    ForkSession,
    ResumeSession,
    CloseSession,
    DeleteSession,
    ListProviders,
    SetProvider,
    DisableProvider,
    SetSessionModel,
    SetSessionMode,
    SetSessionConfigOption,
}
struct Registration {
    handlers: Vec<NotificationHandler>,
    pending: Vec<Value>,
}
struct Completion {
    token: u64,
    sender: oneshot::Sender<Result<Value, AcpError>>,
}
enum NotificationJob {
    Incoming {
        method: String,
        params: Value,
        complete: oneshot::Sender<()>,
    },
    Register {
        method: String,
        handler: NotificationHandler,
        complete: oneshot::Sender<()>,
    },
    Terminal(AcpError),
}
struct State {
    role: Role,
    options: ClientOptions,
    generation: Mutex<Option<Generation>>,
    closed: Mutex<Option<AcpError>>,
    requests: Mutex<HashMap<String, RequestHandler>>,
    extensions: Mutex<HashMap<String, RequestHandler>>,
    unknown_request: Mutex<Option<RequestHandler>>,
    notifications: Mutex<HashMap<String, Registration>>,
    unknown_notification: Mutex<Option<NotificationHandler>>,
    raw_notifications: Arc<crate::notifications::NotificationQueue>,
    completions: Mutex<HashMap<String, Completion>>,
    next_token: AtomicU64,
    events: broadcast::Sender<ClientEvent>,
    termination: tokio::sync::Notify,
    closed_signal: watch::Sender<Option<AcpError>>,
    notification_jobs: mpsc::UnboundedSender<NotificationJob>,
}
impl State {
    fn active(&self) -> Result<(), AcpError> {
        match self.closed.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn terminate(&self, error: AcpError) {
        let mut closed = self.closed.lock().unwrap();
        if closed.is_some() {
            return;
        }
        *closed = Some(error.clone());
        drop(closed);
        self.closed_signal.send_replace(Some(error.clone()));
        for (_, pending) in self.completions.lock().unwrap().drain() {
            let _ = pending.sender.send(Err(error.clone()));
        }
        let _ = self.events.send(ClientEvent::Terminated(error));
        self.termination.notify_one();
    }
}
struct Inner {
    peer: Arc<dyn Peer>,
    state: Arc<State>,
    timeout: Duration,
    dispatch: Mutex<Option<JoinHandle<()>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.state.terminate(AcpError::Closed);
        if let Some(task) = self.dispatch.get_mut().unwrap().take() {
            task.abort();
        }
    }
}
#[derive(Clone)]
pub struct Client(Arc<Inner>);
struct CompletionGuard {
    state: Arc<State>,
    session: String,
    token: u64,
}
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        let mut pending = self.state.completions.lock().unwrap();
        if pending
            .get(&self.session)
            .is_some_and(|p| p.token == self.token)
        {
            pending.remove(&self.session);
        }
    }
}

impl Client {
    /// The timeout is caller policy; the ACP protocol itself specifies none.
    /// This constructor subscribes before any initialization write.
    pub fn new(peer: Arc<dyn Peer>, request_timeout: Duration) -> Self {
        Self::with_options(peer, request_timeout, ClientOptions::default())
    }
    pub fn with_options(
        peer: Arc<dyn Peer>,
        request_timeout: Duration,
        options: ClientOptions,
    ) -> Self {
        Self::with_role_options(peer, request_timeout, Role::Client, options)
    }
    pub(crate) fn with_role(peer: Arc<dyn Peer>, request_timeout: Duration, role: Role) -> Self {
        Self::with_role_options(peer, request_timeout, role, ClientOptions::default())
    }
    pub(crate) fn with_role_options(
        peer: Arc<dyn Peer>,
        request_timeout: Duration,
        role: Role,
        options: ClientOptions,
    ) -> Self {
        peer.enable_ordered_ingress();
        let incoming = peer.subscribe();
        let (events, _) = broadcast::channel(32);
        let (closed_signal, _) = watch::channel(None);
        let (notification_jobs, jobs) = mpsc::unbounded_channel();
        let state = Arc::new(State {
            role,
            options,
            generation: Mutex::new(None),
            closed: Mutex::new(None),
            requests: Mutex::new(HashMap::new()),
            extensions: Mutex::new(HashMap::new()),
            unknown_request: Mutex::new(None),
            notifications: Mutex::new(HashMap::new()),
            unknown_notification: Mutex::new(None),
            raw_notifications: Arc::new(crate::notifications::NotificationQueue::default()),
            completions: Mutex::new(HashMap::new()),
            next_token: AtomicU64::new(1),
            events,
            termination: tokio::sync::Notify::new(),
            closed_signal,
            notification_jobs,
        });
        let task = tokio::spawn(dispatch(peer.clone(), state.clone(), incoming, jobs));
        Self(Arc::new(Inner {
            peer,
            state,
            timeout: request_timeout,
            dispatch: Mutex::new(Some(task)),
        }))
    }
    pub fn generation(&self) -> Option<Generation> {
        *self.0.state.generation.lock().unwrap()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        self.0.state.events.subscribe()
    }
    /// Each stream consumes the same bounded queue, matching source
    /// Stream.fromQueue rather than creating independent subscriptions.
    pub fn notifications(&self) -> crate::NotificationStream {
        crate::NotificationStream(self.0.state.raw_notifications.clone())
    }
    pub fn shutdown(&self) {
        self.0.state.terminate(AcpError::Closed);
        if let Some(task) = self.0.dispatch.lock().unwrap().take() {
            task.abort();
        }
    }
    pub fn handle_request(&self, method: impl Into<String>, handler: RequestHandler) {
        self.0
            .state
            .requests
            .lock()
            .unwrap()
            .insert(method.into(), handler);
    }
    pub(crate) fn handle_extension_request(&self, method: &str, handler: RequestHandler) {
        self.0
            .state
            .extensions
            .lock()
            .unwrap()
            .insert(method.into(), handler);
    }
    pub(crate) fn handler_events(&self) -> broadcast::Sender<ClientEvent> {
        self.0.state.events.clone()
    }
    pub fn handle_unknown_request(&self, handler: RequestHandler) {
        *self.0.state.unknown_request.lock().unwrap() = Some(handler);
    }
    pub fn handle_unknown_notification(&self, handler: NotificationHandler) {
        *self.0.state.unknown_notification.lock().unwrap() = Some(handler);
    }
    /// Core updates arriving before registration are replayed in arrival order.
    pub async fn handle_notification(
        &self,
        method: impl Into<String>,
        handler: NotificationHandler,
    ) {
        let (sender, receiver) = oneshot::channel();
        if self
            .0
            .state
            .notification_jobs
            .send(NotificationJob::Register {
                method: method.into(),
                handler,
                complete: sender,
            })
            .is_ok()
        {
            let _ = receiver.await;
        }
    }
    pub async fn raw_request(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        let mut closed = self.0.state.closed_signal.subscribe();
        self.0.state.active()?;
        tokio::select! {biased;_=closed.changed()=>Err(closed.borrow().clone().unwrap_or(AcpError::Closed)),result=self.0.peer.request(method,params,self.0.timeout)=>result}
    }
    pub async fn raw_notify(&self, method: &str, params: Value) -> Result<(), AcpError> {
        let mut closed = self.0.state.closed_signal.subscribe();
        self.0.state.active()?;
        tokio::select! {biased;_=closed.changed()=>Err(closed.borrow().clone().unwrap_or(AcpError::Closed)),result=self.0.peer.notify(method,params)=>result}
    }
    pub(crate) async fn core_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, AcpError> {
        self.raw_request(method, params)
            .await
            .map_err(|error| match error {
                AcpError::ResponseError { error, .. } => {
                    crate::errors::RequestError::from_protocol_error(error, method, None, None)
                        .into()
                }
                AcpError::Request(error) => {
                    crate::errors::RequestError::from_protocol_error(error, method, None, None)
                        .into()
                }
                AcpError::ResponseCause {
                    request_id, cause, ..
                } => {
                    for (index, reason) in cause.data.iter().enumerate() {
                        if let crate::EffectCauseReason::Fail { error } = reason {
                            if let Err(mut decode_error) = schema::decode("v2.Error", error.clone())
                            {
                                decode_error.issue = schema::ValidationIssue::pointer(
                                    "cause".into(),
                                    schema::ValidationIssue::pointer_index(
                                        index,
                                        schema::ValidationIssue::pointer(
                                            "error".into(),
                                            decode_error.issue,
                                        ),
                                    ),
                                );
                                return AcpError::ResponseDefect {
                                    method: method.into(),
                                    request_id,
                                    cause,
                                    decode_error: Some(Arc::new(decode_error)),
                                };
                            }
                        }
                    }
                    if let Some(error) = cause.protocol_error() {
                        crate::errors::RequestError::from_protocol_error(error, method, None, None)
                            .into()
                    } else {
                        AcpError::ResponseDefect {
                            method: method.into(),
                            request_id,
                            cause,
                            decode_error: None,
                        }
                    }
                }
                other => other,
            })
    }
    pub async fn initialize(&self, request: Value) -> Result<Value, AcpError> {
        let response = self
            .core_request("initialize", normalize::negotiating_initialize(request))
            .await?;
        let response = decode_any(
            &["v2.InitializeResponse", "v1.InitializeResponse"],
            response,
        )?;
        if response.get("info").is_some() {
            *self.0.state.generation.lock().unwrap() = Some(Generation::V2);
            Ok(normalize::initialize_response(response))
        } else {
            *self.0.state.generation.lock().unwrap() = Some(Generation::V1);
            Ok(response)
        }
    }
    pub async fn call(&self, method: AgentMethod, mut params: Value) -> Result<Value, AcpError> {
        let v1 = self.generation() == Some(Generation::V1);
        use AgentMethod::*;
        if v1
            && matches!(
                method,
                DeleteSession | ListProviders | SetProvider | DisableProvider
            )
        {
            return Err(RpcError::method_not_found(method_spec(method, false).0).into());
        }
        if !v1 && matches!(method, SetSessionModel | SetSessionMode) {
            return Err(RpcError::method_not_found(method_spec(method, true).0).into());
        }
        if !v1
            && matches!(
                method,
                CreateSession | LoadSession | ForkSession | ResumeSession
            )
        {
            params = normalize::mcp_servers(params);
            if method == LoadSession {
                params["replayFrom"] = json!({"type":"start"});
            }
        }
        if !v1 && method == SetSessionConfigOption {
            if params["type"] != "boolean" {
                params["type"] = json!("id");
            }
        }
        let (wire, request, response) = method_spec(method, v1);
        let params = decode_any(request, params)?;
        let response = decode_any(response, self.core_request(wire, params).await?)?;
        Ok(
            if !v1
                && matches!(
                    method,
                    CreateSession | LoadSession | ForkSession | ResumeSession
                )
            {
                normalize::setup_response(response)
            } else if !v1 && method == SetSessionConfigOption {
                json!({"configOptions":response["configOptions"].as_array().map(|a|a.iter().cloned().filter_map(normalize::config_option).collect::<Vec<_>>()).unwrap_or_default()})
            } else {
                response
            },
        )
    }
    pub async fn prompt(&self, params: Value) -> Result<Value, AcpError> {
        let params = decode_any(&["v2.PromptRequest", "v1.PromptRequest"], params)?;
        if self.generation() == Some(Generation::V1) {
            return decode_any(
                &["v1.PromptResponse", "v2.PromptResponse"],
                self.core_request("session/prompt", params).await?,
            )
            .map_err(Into::into);
        }
        let session = params["sessionId"].as_str().unwrap().to_owned();
        let token = self.0.state.next_token.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.0.state.completions.lock().unwrap();
            if pending.contains_key(&session) {
                return Err(RpcError {
                    code: -32603,
                    message: format!("ACP session '{session}' already has an active prompt."),
                    data: None,
                }
                .into());
            }
            pending.insert(session.clone(), Completion { token, sender });
        }
        let _guard = CompletionGuard {
            state: self.0.state.clone(),
            session,
            token,
        };
        decode_any(
            &["v1.PromptResponse", "v2.PromptResponse"],
            self.core_request("session/prompt", params).await?,
        )?;
        receiver.await.map_err(|_| AcpError::Closed)?
    }
    pub async fn cancel(&self, params: Value) -> Result<(), AcpError> {
        let params = schema::decode("v2.CancelNotification", params)?;
        self.raw_notify("session/cancel", params).await
    }
}

pub(crate) fn decode_any(names: &[&str], value: Value) -> Result<Value, schema::SchemaError> {
    let mut error = None;
    for name in names {
        match schema::decode(name, value.clone()) {
            Ok(decoded) => return Ok(decoded),
            Err(e) => error = Some(e),
        }
    }
    Err(error.unwrap())
}
fn method_spec(
    method: AgentMethod,
    v1: bool,
) -> (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
) {
    use AgentMethod::*;
    match (method, v1) {
        (Authenticate, true) => (
            "authenticate",
            &["v1.AuthenticateRequest"],
            &["v1.AuthenticateResponse"],
        ),
        (Authenticate, false) => (
            "auth/login",
            &["v2.LoginAuthRequest"],
            &["v2.LoginAuthResponse"],
        ),
        (Logout, true) => ("logout", &["v1.LogoutRequest"], &["v1.LogoutResponse"]),
        (Logout, false) => (
            "auth/logout",
            &["v2.LogoutAuthRequest"],
            &["v2.LogoutAuthResponse"],
        ),
        (CreateSession, _) => (
            "session/new",
            &["v2.NewSessionRequest", "v1.NewSessionRequest"],
            &["compat.NewSessionResponse", "v2.NewSessionResponse"],
        ),
        (LoadSession, true) => (
            "session/load",
            &["v1.LoadSessionRequest"],
            &["compat.LoadSessionResponse"],
        ),
        (LoadSession, false) | (ResumeSession, _) => (
            "session/resume",
            &["v2.ResumeSessionRequest", "v1.ResumeSessionRequest"],
            &["compat.ResumeSessionResponse", "v2.ResumeSessionResponse"],
        ),
        (ListSessions, _) => (
            "session/list",
            &["v2.ListSessionsRequest", "v1.ListSessionsRequest"],
            &["v1.ListSessionsResponse", "v2.ListSessionsResponse"],
        ),
        (ForkSession, _) => (
            "session/fork",
            &["v2.ForkSessionRequest", "v1.ForkSessionRequest"],
            &["compat.ForkSessionResponse", "v2.ForkSessionResponse"],
        ),
        (CloseSession, _) => (
            "session/close",
            &["v2.CloseSessionRequest", "v1.CloseSessionRequest"],
            &["v2.CloseSessionResponse", "v1.CloseSessionResponse"],
        ),
        (DeleteSession, _) => (
            "session/delete",
            &["v2.DeleteSessionRequest"],
            &["v2.DeleteSessionResponse"],
        ),
        (ListProviders, _) => (
            "providers/list",
            &["v2.ListProvidersRequest"],
            &["v2.ListProvidersResponse"],
        ),
        (SetProvider, _) => (
            "providers/set",
            &["v2.SetProviderRequest"],
            &["v2.SetProviderResponse"],
        ),
        (DisableProvider, _) => (
            "providers/disable",
            &["v2.DisableProviderRequest"],
            &["v2.DisableProviderResponse"],
        ),
        (SetSessionModel, _) => (
            "session/set_model",
            &["compat.SetSessionModelRequest"],
            &["compat.SetSessionModelResponse"],
        ),
        (SetSessionMode, _) => (
            "session/set_mode",
            &["v1.SetSessionModeRequest"],
            &["v1.SetSessionModeResponse"],
        ),
        (SetSessionConfigOption, _) => (
            "session/set_config_option",
            &[
                "v2.SetSessionConfigOptionRequest",
                "v1.SetSessionConfigOptionRequest",
            ],
            &[
                "v1.SetSessionConfigOptionResponse",
                "v2.SetSessionConfigOptionResponse",
            ],
        ),
    }
}
fn request_spec(method: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    Some(match method {
        "session/request_permission" => (
            &["v2.RequestPermissionRequest", "v1.RequestPermissionRequest"],
            &[
                "v2.RequestPermissionResponse",
                "v1.RequestPermissionResponse",
            ],
        ),
        "elicitation/create" => (
            &["v2.CreateElicitationRequest", "v1.CreateElicitationRequest"],
            &[
                "v2.CreateElicitationResponse",
                "v1.CreateElicitationResponse",
            ],
        ),
        "session/elicitation" | "_session/elicitation" => (
            &["v2.CreateElicitationRequest"],
            &["v2.CreateElicitationResponse"],
        ),
        "mcp/connect" => (&["v2.ConnectMcpRequest"], &["v2.ConnectMcpResponse"]),
        "mcp/message" => (&["v2.MessageMcpRequest"], &["v2.MessageMcpResponse"]),
        "mcp/disconnect" => (&["v2.DisconnectMcpRequest"], &["v2.DisconnectMcpResponse"]),
        "fs/read_text_file" => (&["v1.ReadTextFileRequest"], &["v1.ReadTextFileResponse"]),
        "fs/write_text_file" => (&["v1.WriteTextFileRequest"], &["v1.WriteTextFileResponse"]),
        "terminal/create" => (
            &["v1.CreateTerminalRequest"],
            &["v1.CreateTerminalResponse"],
        ),
        "terminal/output" => (
            &["v1.TerminalOutputRequest"],
            &["v1.TerminalOutputResponse"],
        ),
        "terminal/release" => (
            &["v1.ReleaseTerminalRequest"],
            &["v1.ReleaseTerminalResponse"],
        ),
        "terminal/wait_for_exit" => (
            &["v1.WaitForTerminalExitRequest"],
            &["v1.WaitForTerminalExitResponse"],
        ),
        "terminal/kill" => (&["v1.KillTerminalRequest"], &["v1.KillTerminalResponse"]),
        _ => return None,
    })
}
fn inbound_spec(
    role: Role,
    method: &str,
) -> Option<(&'static [&'static str], &'static [&'static str])> {
    if role == Role::Client {
        return request_spec(method);
    }
    Some(match method {
        "initialize" => (&["v2.InitializeRequest"], &["v2.InitializeResponse"]),
        "auth/login" => (&["v2.LoginAuthRequest"], &["v2.LoginAuthResponse"]),
        "auth/logout" => (&["v2.LogoutAuthRequest"], &["v2.LogoutAuthResponse"]),
        "session/new" => (&["v2.NewSessionRequest"], &["v2.NewSessionResponse"]),
        "session/list" => (&["v2.ListSessionsRequest"], &["v2.ListSessionsResponse"]),
        "session/fork" => (&["v2.ForkSessionRequest"], &["v2.ForkSessionResponse"]),
        "session/resume" => (&["v2.ResumeSessionRequest"], &["v2.ResumeSessionResponse"]),
        "session/close" => (&["v2.CloseSessionRequest"], &["v2.CloseSessionResponse"]),
        "session/delete" => (&["v2.DeleteSessionRequest"], &["v2.DeleteSessionResponse"]),
        "providers/list" => (&["v2.ListProvidersRequest"], &["v2.ListProvidersResponse"]),
        "providers/set" => (&["v2.SetProviderRequest"], &["v2.SetProviderResponse"]),
        "providers/disable" => (
            &["v2.DisableProviderRequest"],
            &["v2.DisableProviderResponse"],
        ),
        "session/set_config_option" => (
            &["v2.SetSessionConfigOptionRequest"],
            &["v2.SetSessionConfigOptionResponse"],
        ),
        "session/prompt" => (&["v2.PromptRequest"], &["v2.PromptResponse"]),
        _ => return None,
    })
}
async fn incoming_request(
    peer: Arc<dyn Peer>,
    state: Arc<State>,
    id: RequestId,
    method: String,
    params: Value,
) {
    enum ReplyError {
        Protocol(RpcError),
        Cause(crate::EffectCause),
    }
    impl From<RpcError> for ReplyError {
        fn from(error: RpcError) -> Self {
            Self::Protocol(error)
        }
    }
    let identity = id.identity();
    let context = RequestContext {
        request_id: identity.clone(),
        method: method.clone(),
        wire_id: id.clone(),
    };
    let spec = inbound_spec(state.role, &method);
    let handler = if spec.is_none() {
        state.extensions.lock().unwrap().get(&method).cloned()
    } else {
        None
    }
    .or_else(|| {
        let handlers = state.requests.lock().unwrap();
        if state.role == Role::Agent && method == "session/resume" {
            let load = params.pointer("/replayFrom/type").and_then(Value::as_str) == Some("start");
            let (primary, fallback) = if load {
                ("session/load", "session/resume")
            } else {
                ("session/resume", "session/load")
            };
            handlers
                .get(primary)
                .or_else(|| handlers.get(fallback))
                .cloned()
        } else {
            handlers.get(&method).cloned()
        }
    })
    .or_else(|| {
        if spec.is_none() {
            state.unknown_request.lock().unwrap().clone()
        } else {
            None
        }
    });
    let result = async {
        let mut params = match spec {
            Some((request, _)) => decode_any(request, params).map_err(|error| {
                ReplyError::Cause(crate::EffectCause::defect(Value::String(
                    error.issue.formatted(),
                )))
            })?,
            None => params,
        };
        if state.role == Role::Client && method == "session/request_permission" {
            params = normalize::permission(params, &identity);
        }
        let handler = handler.ok_or_else(|| RpcError::method_not_found(&method))?;
        let response = AssertUnwindSafe(async move { handler(params, context).await })
            .catch_unwind()
            .await
            .map_err(|panic| {
                if spec.is_some() {
                    let message = panic
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("Rust handler panicked");
                    ReplyError::Cause(crate::EffectCause::defect(
                        json!({"name":"Error","message":message}),
                    ))
                } else {
                    ReplyError::Protocol(RpcError::internal())
                }
            })??;
        let response = match spec {
            Some((_, response_schemas)) => {
                decode_any(response_schemas, response).map_err(|_| RpcError::internal())?
            }
            None => response,
        };
        if state.role == Role::Client
            && matches!(
                method.as_str(),
                "session/elicitation" | "_session/elicitation"
            )
        {
            let mut action = response;
            let meta = action.as_object_mut().unwrap().remove("_meta");
            let mut response = json!({"action":action});
            if let Some(meta) = meta {
                response["_meta"] = meta;
            }
            Ok::<_, ReplyError>(response)
        } else {
            Ok(response)
        }
    }
    .await;
    let written = match result {
        Ok(value) => peer.respond(id, Ok(value)).await,
        Err(ReplyError::Protocol(error)) => peer.respond(id, Err(error)).await,
        Err(ReplyError::Cause(cause)) => peer.respond_cause(id, cause).await,
    };
    match written {
        Ok(()) => {
            let _ = state.events.send(ClientEvent::ResponseAcknowledged {
                request_id: identity,
            });
        }
        Err(error) => {
            let _ = state.events.send(ClientEvent::ResponseFailed {
                request_id: identity,
                error: error.clone(),
            });
            state.terminate(error);
        }
    }
}
async fn run_notifications(handlers: &[NotificationHandler], value: Value) {
    for handler in handlers {
        let _ = AssertUnwindSafe(async { handler(value.clone()).await })
            .catch_unwind()
            .await;
    }
}
fn incoming_notification(
    state: &State,
    method: String,
    params: Value,
) -> Result<(Vec<NotificationHandler>, Value, Notification), AcpError> {
    let (method, notification, value) = match method.as_str() {
        "session/update" => {
            let value = decode_any(
                &["v2.UpdateSessionNotification", "v1.SessionNotification"],
                params,
            )
            .map_err(|error| {
                crate::errors::ProtocolParseError::from_schema_error(
                    crate::errors::ProtocolParseOperation::DecodeNotificationPayload,
                    "session/update",
                    error,
                )
            })?;
            let v1 = *state.generation.lock().unwrap() == Some(Generation::V1);
            let value = if state.role == Role::Client {
                normalize::notification(value, v1)
            } else {
                value
            };
            let value = match &state.options.transform_session_update {
                Some(transform) => std::panic::catch_unwind(AssertUnwindSafe(|| transform(value)))
                    .map_err(read_input_defect)?,
                None => value,
            };
            (
                "session/update".to_owned(),
                Notification::SessionUpdate(value.clone()),
                value,
            )
        }
        "elicitation/complete" | "session/elicitation/complete" => {
            let value =
                schema::decode("v2.CompleteElicitationNotification", params).map_err(|error| {
                    crate::errors::ProtocolParseError::from_schema_error(
                        crate::errors::ProtocolParseOperation::DecodeNotificationPayload,
                        &method,
                        error,
                    )
                })?;
            (
                "elicitation/complete".into(),
                Notification::ElicitationComplete(value.clone()),
                value,
            )
        }
        _ => (
            method.clone(),
            Notification::Extension {
                method,
                params: params.clone(),
            },
            params,
        ),
    };
    let handlers = {
        let mut registrations = state.notifications.lock().unwrap();
        if matches!(method.as_str(), "session/update" | "elicitation/complete") {
            let entry = registrations.entry(method).or_insert_with(|| Registration {
                handlers: Vec::new(),
                pending: Vec::new(),
            });
            if entry.handlers.is_empty() {
                entry.pending.push(value.clone());
            }
            entry.handlers.clone()
        } else {
            registrations
                .get(&method)
                .map(|r| r.handlers.clone())
                .unwrap_or_else(|| {
                    state
                        .unknown_notification
                        .lock()
                        .unwrap()
                        .iter()
                        .cloned()
                        .collect()
                })
        }
    };
    Ok((handlers, value, notification))
}
fn read_input_defect(panic: Box<dyn std::any::Any + Send>) -> AcpError {
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("Rust protocol callback panicked");
    crate::errors::TransportError {
        operation: Some(crate::errors::TransportOperation::ReadInputStream),
        method: None,
        detail: None,
        pid: None,
        cause: crate::errors::FailureCause::Value(json!({"name":"Error", "message": message})),
    }
    .into()
}
fn complete_prompt(state: &State, notification: &Notification) {
    let Notification::SessionUpdate(value) = notification else {
        return;
    };
    if value
        .pointer("/update/sessionUpdate")
        .and_then(Value::as_str)
        != Some("state_update")
        || value.pointer("/update/state").and_then(Value::as_str) != Some("idle")
    {
        return;
    }
    if let Some(session) = value.get("sessionId").and_then(Value::as_str) {
        if let Some(completion) = state.completions.lock().unwrap().remove(session) {
            let update = &value["update"];
            let mut result = json!({"stopReason":update.get("stopReason").filter(|v|!v.is_null()).cloned().unwrap_or_else(||json!("end_turn"))});
            for name in ["usage", "_meta"] {
                if let Some(v) = update.get(name) {
                    result[name] = v.clone();
                }
            }
            let _ = completion.sender.send(Ok(result));
        }
    }
}
async fn dispatch(
    peer: Arc<dyn Peer>,
    state: Arc<State>,
    mut incoming: broadcast::Receiver<PeerEvent>,
    mut jobs: mpsc::UnboundedReceiver<NotificationJob>,
) {
    let mut callbacks = JoinSet::new();
    let worker_state = state.clone();
    callbacks.spawn(async move {
        while let Some(job) = jobs.recv().await {
            match job {
                NotificationJob::Incoming {
                    method,
                    params,
                    complete,
                } => {
                    let original_method = method.clone();
                    match incoming_notification(&worker_state, method, params) {
                        Ok((handlers, value, notification)) => {
                            use crate::IncomingNotification;
                            let raw = match &notification {
                                Notification::SessionUpdate(params) => {
                                    IncomingNotification::SessionUpdate {
                                        method: original_method,
                                        params: params.clone(),
                                    }
                                }
                                Notification::ElicitationComplete(params) => {
                                    IncomingNotification::ElicitationComplete {
                                        method: original_method,
                                        params: params.clone(),
                                    }
                                }
                                Notification::Extension { method, params } => {
                                    IncomingNotification::ExtNotification {
                                        method: method.clone(),
                                        params: params.clone(),
                                    }
                                }
                            };
                            worker_state.raw_notifications.offer(raw);
                            complete_prompt(&worker_state, &notification);
                            run_notifications(&handlers, value).await;
                            let _ = worker_state
                                .events
                                .send(ClientEvent::Notification(notification));
                            let _ = complete.send(());
                        }
                        Err(error) => {
                            worker_state.terminate(error);
                            break;
                        }
                    }
                }
                NotificationJob::Register {
                    method,
                    handler,
                    complete,
                } => {
                    let (pending, handlers) = {
                        let mut registry = worker_state.notifications.lock().unwrap();
                        let entry = registry.entry(method).or_insert_with(|| Registration {
                            handlers: Vec::new(),
                            pending: Vec::new(),
                        });
                        entry.handlers.push(handler);
                        (std::mem::take(&mut entry.pending), entry.handlers.clone())
                    };
                    for value in pending {
                        run_notifications(&handlers, value).await;
                    }
                    let _ = complete.send(());
                }
                NotificationJob::Terminal(error) => {
                    worker_state.terminate(error);
                    break;
                }
            }
        }
    });
    let mut input_finished = false;
    loop {
        tokio::select! {
            _=state.termination.notified()=>break,
            _=callbacks.join_next(),if !callbacks.is_empty()=>{},
            event=incoming.recv(),if !input_finished=>match event{
                Ok(PeerEvent::IngressBarrier{acknowledgement})=>{acknowledgement.acknowledge();},
                Ok(PeerEvent::Request{id,method,params})=>{callbacks.spawn(incoming_request(peer.clone(),state.clone(),id,method,params));},
                Ok(PeerEvent::Notification{method,params})=>{
                    let (complete, finished)=oneshot::channel();
                    if state.notification_jobs.send(NotificationJob::Incoming{method,params,complete}).is_err() { break; }
                    // The source reader awaits handlers before routing any later
                    // ingress, including callbacks, replies and input termination.
                    let mut closed=state.closed_signal.subscribe();
                    if closed.borrow().is_some() { break; }
                    tokio::select! {
                        _=finished=>{},
                        _=closed.changed()=>break,
                    }
                },
                Ok(PeerEvent::Closed(error))=>{input_finished=true;let _=state.notification_jobs.send(NotificationJob::Terminal(error));},
                Err(error)=>{input_finished=true;let _=state.notification_jobs.send(NotificationJob::Terminal(AcpError::Transport(format!("incoming continuity lost: {error}"))));},
            }
        }
        if state.closed.lock().unwrap().is_some() {
            break;
        }
    }
}
