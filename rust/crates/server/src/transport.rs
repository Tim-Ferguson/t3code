use crate::{
    auth::{AuthService, Session},
    persistence::{Store, StoreError, StoredEvent, read_projection},
    project::ProjectService,
    thread::ThreadService,
};
use axum::{
    Form, Json, Router,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use t3_contracts::{
    AuthEnvironmentScope, RpcCause, RpcClientMessage, RpcExit, RpcRequest, RpcRequestId,
    RpcServerMessage,
};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct ApiState {
    pub store: Store,
    pub auth: AuthService,
    pub environment: Value,
    /// Supplied by the configuration service, never invented by RPC handlers.
    pub config: Option<Value>,
    pub settings: Option<crate::server_settings::SettingsService>,
    /// None uses the packaged wildcard policy; development names explicit origins.
    pub cors_origins: Option<Vec<String>>,
    /// Built Rust web assets, served with the API for cookie authentication.
    pub assets: Option<std::path::PathBuf>,
    pub providers: Option<crate::provider_registry::ProviderRegistry>,
    pub execution: Option<crate::execution::ExecutionService>,
    pub workspace: Option<crate::workspace_entries::WorkspaceEntries>,
    pub terminals: Option<crate::terminal_manager::TerminalManager>,
    pub discovery: Option<crate::resource_discovery::PortDiscovery>,
    pub resource_telemetry: Option<crate::resource_telemetry_service::ResourceTelemetry>,
    pub host_resources: Option<crate::host_resources::HostResources>,
    pub background: Option<crate::background_policy::BackgroundPolicy>,
    pub device_hosts: Option<crate::device_host_resolver::DeviceHostResolver>,
    pub devices: Option<crate::device_service::DeviceService>,
    pub provider_auth: Option<crate::provider_auth_service::ProviderAuthService>,
}

type ApiError = (StatusCode, Json<Value>);
fn unauthorized() -> ApiError {
    (
        StatusCode::UNAUTHORIZED,
        Json(
            json!({"_tag":"EnvironmentAuthInvalidError","code":"auth_invalid","reason":"invalid_credential","traceId":"unavailable"}),
        ),
    )
}
fn invalid_request(reason: &str) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(
            json!({"_tag":"EnvironmentRequestInvalidError","code":"invalid_request","reason":reason,"traceId":"unavailable"}),
        ),
    )
}
fn internal(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(error=%error,"native API service failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(
            json!({"_tag":"EnvironmentInternalError","code":"internal_error","reason":"internal_error","traceId":"unavailable"}),
        ),
    )
}
fn scope_error(scope: AuthEnvironmentScope) -> ApiError {
    let response = t3_contracts::auth_scope_required_response(scope);
    (
        StatusCode::FORBIDDEN,
        Json(
            json!({"_tag":"EnvironmentScopeRequiredError","code":"insufficient_scope","requiredScope":response.required_scope,"requiredPermission":response.required_permission,"traceId":"unavailable"}),
        ),
    )
}

pub fn router(state: ApiState) -> Router {
    let cors_origins = state.cors_origins.clone();
    Router::new()
        .route("/.well-known/t3/environment", get(environment))
        .route("/api/auth/session", get(session_state))
        .route("/api/auth/browser-session", post(browser_session))
        .route("/api/auth/websocket-ticket", post(websocket_ticket))
        .route("/oauth/token", post(exchange_token))
        .route("/api/orchestration/shell", get(shell_snapshot))
        .route(
            "/api/orchestration/threads/{threadId}",
            get(thread_snapshot),
        )
        .route(
            "/api/orchestration/threads/{threadId}/bounded",
            get(thread_bounded_snapshot),
        )
        .route(
            "/api/orchestration/threads/{threadId}/history",
            get(thread_history),
        )
        .route(
            "/api/device-hub/{*path}",
            axum::routing::any(crate::device_hub_proxy::handle),
        )
        .route("/ws", get(upgrade))
        .fallback(get(static_asset))
        .layer(axum::middleware::map_response(no_cache))
        .layer(axum::middleware::from_fn_with_state(cors_origins, cors))
        .with_state(state)
}

async fn static_asset(State(state): State<ApiState>, uri: axum::http::Uri) -> Response {
    let Some(root) = state.assets else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/")
        || path.starts_with("oauth/")
        || path.starts_with(".well-known/")
        || path.split('/').any(|part| part == ".." || part == ".")
        || path.contains('\\')
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let candidate = if path.is_empty() {
        root.join("index.html")
    } else {
        root.join(path)
    };
    let candidate = match tokio::fs::canonicalize(candidate).await {
        Ok(path) => path,
        Err(_) if std::path::Path::new(path).extension().is_none() => {
            match tokio::fs::canonicalize(root.join("index.html")).await {
                Ok(path) => path,
                Err(_) => return StatusCode::NOT_FOUND.into_response(),
            }
        }
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let root = match tokio::fs::canonicalize(root).await {
        Ok(root) => root,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    if !candidate.starts_with(root)
        || !tokio::fs::metadata(&candidate)
            .await
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= 64 * 1024 * 1024)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let content_type = match candidate
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    match tokio::fs::read(candidate).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type)], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn cors(
    State(origins): State<Option<Vec<String>>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let origin = request.headers().get(header::ORIGIN).cloned();
    let allowed = match (&origins, &origin) {
        (None, Some(_)) => true,
        (Some(origins), Some(origin)) => origin
            .to_str()
            .ok()
            .is_some_and(|origin| origins.iter().any(|allowed| allowed == origin)),
        _ => false,
    };
    let preflight = request.method() == axum::http::Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut response = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    if allowed {
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            if origins.is_none() {
                "*".parse().unwrap()
            } else {
                origin.unwrap()
            },
        );
        if origins.is_some() {
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                "true".parse().unwrap(),
            );
            response
                .headers_mut()
                .append(header::VARY, "Origin".parse().unwrap());
        }
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, POST, OPTIONS".parse().unwrap(),
        );
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            "authorization, b3, traceparent, content-type, dpop, x-t3-orchestration-protocol"
                .parse()
                .unwrap(),
        );
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_MAX_AGE, "600".parse().unwrap());
    }
    response
}

async fn no_cache(mut response: Response) -> Response {
    response
        .headers_mut()
        .entry(header::CACHE_CONTROL)
        .or_insert_with(|| "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::PRAGMA, "no-cache".parse().unwrap());
    response
}
async fn environment(State(state): State<ApiState>) -> Json<Value> {
    Json(state.environment)
}

fn authenticate(state: &ApiState, headers: &HeaderMap) -> Result<Session, ApiError> {
    let cookies = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok());
    let cookie_token = |name: &str| {
        cookies.and_then(|value| {
            value.split(';').find_map(|part| {
                let (key, value) = part.trim().split_once('=')?;
                (key == name).then_some(value)
            })
        })
    };
    if let Some(token) = cookie_token(&state.auth.cookie_name) {
        return state
            .auth
            .verify_session(token, Utc::now())
            .map_err(|_| unauthorized());
    }
    if let Some(authorization) = headers.get(header::AUTHORIZATION) {
        let value = authorization.to_str().map_err(|_| unauthorized())?;
        if let Some(token) = value
            .strip_prefix("Bearer ")
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            return state
                .auth
                .verify_session(token, Utc::now())
                .map_err(|_| unauthorized());
        }
        if value
            .strip_prefix("DPoP ")
            .map(str::trim)
            .is_some_and(|token| !token.is_empty())
        {
            return Err(unauthorized());
        }
    }
    if state.auth.policy == "remote-reachable" {
        if let Some(token) = cookie_token("t3_session") {
            return state
                .auth
                .verify_session(token, Utc::now())
                .map_err(|_| unauthorized());
        }
    }
    Err(unauthorized())
}
pub(crate) fn authenticate_media_request(
    state: &ApiState,
    headers: &HeaderMap,
    ticket: Option<&str>,
    scope: AuthEnvironmentScope,
) -> Result<(), Response> {
    let session = match ticket.filter(|ticket| !ticket.trim().is_empty()) {
        Some(ticket) => state
            .auth
            .verify_websocket_ticket(ticket, Utc::now())
            .map_err(|_| unauthorized()),
        None => authenticate(state, headers),
    }
    .map_err(IntoResponse::into_response)?;
    require(&session, scope).map_err(IntoResponse::into_response)
}
fn require(session: &Session, scope: AuthEnvironmentScope) -> Result<(), ApiError> {
    if session.scopes.contains(&scope) {
        Ok(())
    } else {
        Err(scope_error(scope))
    }
}

async fn session_state(State(state): State<ApiState>, headers: HeaderMap) -> Json<Value> {
    let session = authenticate(&state, &headers).ok();
    Json(state.auth.session_state(session.as_ref()))
}
async fn browser_session(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Result<Response, ApiError> {
    let credential = input["credential"].as_str().ok_or_else(unauthorized)?;
    let client = json!({"deviceType":"unknown","userAgent":headers.get(header::USER_AGENT).and_then(|value|value.to_str().ok())});
    let (session, token) = state
        .auth
        .exchange_pairing_credential(credential, client, Utc::now())
        .map_err(|_| unauthorized())?;
    let grants = t3_contracts::auth_scope_response(&session.scopes);
    let mut response=Json(json!({"authenticated":true,"scopes":grants.scopes,"permissions":grants.permissions,"sessionMethod":session.method,"expiresAt":session.expires_at})).into_response();
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=2592000",
        state.auth.cookie_name, token
    );
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie.parse().map_err(internal)?);
    Ok(response)
}
async fn websocket_ticket(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let session = authenticate(&state, &headers)?;
    state
        .auth
        .issue_websocket_ticket(&session, Utc::now())
        .map(Json)
        .map_err(internal)
}

async fn exchange_token(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Form(input): Form<t3_contracts::AuthTokenExchangeRequest>,
) -> Result<Json<Value>, ApiError> {
    if headers.contains_key("dpop") {
        return Err(unauthorized());
    }
    let requested = input
        .scope
        .as_ref()
        .map(|scope| requested_scopes(scope.as_str()))
        .transpose()?;
    let now = Utc::now();
    let mut client = json!({"deviceType":input.client_device_type.unwrap_or(t3_contracts::AuthClientMetadataDeviceType::Unknown)});
    if let Some(label) = input.client_label {
        client["label"] = json!(label);
    }
    if let Some(os) = input.client_os {
        client["os"] = json!(os);
    }
    if let Some(agent) = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
    {
        client["userAgent"] = json!(agent);
    }
    let (session, token) = state
        .auth
        .exchange_pairing_bearer(
            input.subject_token.as_str(),
            requested.as_deref(),
            client,
            now,
        )
        .map_err(|error| match error {
            crate::auth::AuthError::ScopeNotGranted => invalid_request("scope_not_granted"),
            crate::auth::AuthError::Invalid(_) => unauthorized(),
            error => internal(error),
        })?;
    let scopes = session
        .scopes
        .iter()
        .map(|scope| {
            serde_json::to_value(scope)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Json(
        json!({"access_token":token,"issued_token_type":"urn:ietf:params:oauth:token-type:access_token","token_type":"Bearer","expires_in":(session.expires_at-now).num_seconds().max(0),"scope":scopes}),
    ))
}
fn requested_scopes(value: &str) -> Result<Vec<AuthEnvironmentScope>, ApiError> {
    let tokens = value.split(' ').collect::<Vec<_>>();
    if tokens.iter().any(|scope| {
        scope.is_empty()
            || !scope.bytes().all(|byte| {
                byte == 0x21 || (0x23..=0x5b).contains(&byte) || (0x5d..=0x7e).contains(&byte)
            })
    }) {
        return Err(invalid_request("invalid_scope"));
    }
    let mut scopes = Vec::new();
    for token in tokens {
        if let Ok(scope) = serde_json::from_value::<AuthEnvironmentScope>(json!(token)) {
            if scope.is_grantable() && !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
    }
    if scopes.is_empty() {
        Err(invalid_request("invalid_scope"))
    } else {
        Ok(scopes)
    }
}
async fn shell_snapshot(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let session = authenticate(&state, &headers)?;
    require(&session, AuthEnvironmentScope::OrchestrationRead)?;
    ThreadService::new(state.store)
        .shell_snapshot()
        .map(|mut snapshot| {
            snapshot["archivedThreads"] = json!([]);
            Json(snapshot)
        })
        .map_err(internal)
}
async fn thread_snapshot(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let session = authenticate(&state, &headers)?;
    require(&session, AuthEnvironmentScope::OrchestrationRead)?;
    crate::history::HistoryService::new(state.store)
        .snapshot(&id, false)
        .map(Json)
        .map_err(history_error)
}
fn history_error(error: StoreError) -> ApiError {
    if matches!(&error,StoreError::InvalidCommand(message) if message=="Thread not found.") {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({"_tag":"EnvironmentNotFoundError","code":"not_found","reason":"thread_not_found","traceId":"unavailable"}),
            ),
        );
    }
    if matches!(&error,StoreError::InvalidCommand(message) if message=="Invalid thread history cursor.")
    {
        return invalid_request("invalid_history_cursor");
    }
    internal(error)
}
async fn thread_bounded_snapshot(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let session = authenticate(&state, &headers)?;
    require(&session, AuthEnvironmentScope::OrchestrationRead)?;
    crate::history::HistoryService::new(state.store)
        .snapshot(&id, true)
        .map(Json)
        .map_err(history_error)
}
async fn thread_history(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let session = authenticate(&state, &headers)?;
    require(&session, AuthEnvironmentScope::OrchestrationRead)?;
    let query: t3_contracts::EnvironmentOrchestrationThreadHistoryQuery =
        serde_json::from_value(json!(query))
            .map_err(|_| invalid_request("invalid_history_cursor"))?;
    crate::history::HistoryService::new(state.store)
        .page(&id, query.cursor.as_str())
        .map(Json)
        .map_err(history_error)
}
fn read_thread_snapshot(store: &Store, id: &str) -> Result<Value, StoreError> {
    store.read(|connection| {
        let sequence: u64 = connection.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM rust_application_events",
            [],
            |row| row.get(0),
        )?;
        let projection = read_projection(connection, "thread", id)?
            .ok_or_else(|| StoreError::InvalidCommand(format!("Thread {id} does not exist.")))?;
        Ok(json!({"snapshotSequence":sequence,"projection":projection}))
    })
}

async fn upgrade(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if query.get("orchestrationProtocol").map(String::as_str) != Some("2") {
        return Err((
            StatusCode::UPGRADE_REQUIRED,
            Json(
                json!({"code":"orchestration_protocol_incompatible","message":"Update this client to one that supports orchestration protocol 2.","orchestrationProtocolVersion":2}),
            ),
        ));
    }
    let session = match query
        .get("wsTicket")
        .filter(|ticket| !ticket.trim().is_empty())
    {
        Some(ticket) => state
            .auth
            .verify_websocket_ticket(ticket, Utc::now())
            .map_err(|_| unauthorized())?,
        None => authenticate(&state, &headers)?,
    };
    Ok(ws
        .max_message_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| connection(socket, state, session)))
}

#[derive(Clone)]
struct SocketSender {
    channel: mpsc::Sender<Message>,
}
impl SocketSender {
    async fn send(&self, message: RpcServerMessage) -> Result<(), String> {
        let encoded = serde_json::to_string(&message).map_err(|error| error.to_string())?;
        self.channel
            .send(Message::Text(encoded.into()))
            .await
            .map_err(|error| error.to_string())
    }
}
struct Subscription {
    ack: Option<mpsc::Sender<()>>,
    generation: u64,
    task: tokio::task::JoinHandle<()>,
}
async fn connection(socket: WebSocket, state: ApiState, session: Session) {
    static NEXT_CLIENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let rpc_client = t3_contracts::NonNegativeInt(
        NEXT_CLIENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );
    let reported = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct Leases {
        reported: std::sync::Arc<std::sync::atomic::AtomicBool>,
        policy: Option<crate::background_policy::BackgroundPolicy>,
        session: t3_contracts::AuthSessionId,
        rpc: t3_contracts::RpcClientId,
        active: bool,
        handle: tokio::runtime::Handle,
    }
    impl Drop for Leases {
        fn drop(&mut self) {
            if self.active && self.reported.load(std::sync::atomic::Ordering::Acquire) {
                if let Some(policy) = self.policy.clone() {
                    let session = self.session.clone();
                    let rpc = self.rpc;
                    self.handle.spawn(async move {
                        policy.remove_rpc_client(&session, &rpc).await;
                    });
                }
            }
        }
    }
    let mut leases = Leases {
        reported: reported.clone(),
        policy: state.background.clone(),
        session: t3_contracts::AuthSessionId::new(&session.session_id).expect("stored session ID"),
        rpc: rpc_client,
        active: true,
        handle: tokio::runtime::Handle::current(),
    };
    let (mut sink, mut source) = socket.split();
    let (channel, mut output) = mpsc::channel::<Message>(64);
    let outgoing = SocketSender { channel };
    let mut writer = tokio::spawn(async move {
        while let Some(frame) = output.recv().await {
            if sink.send(frame).await.is_err() {
                break;
            }
        }
    });
    let mut subscriptions: HashMap<RpcRequestId, Subscription> = HashMap::new();
    let (finished, mut completed) =
        mpsc::channel::<(RpcRequestId, u64, Option<RpcServerMessage>)>(64);
    let mut generation = 0u64;
    loop {
        tokio::select! {
            frame=source.next()=>{
                let Some(Ok(frame))=frame else {break};
                let text=match frame {Message::Text(text)=>text.to_string(),Message::Binary(bytes)=>match String::from_utf8(bytes.to_vec()){Ok(text)=>text,Err(_)=>break},Message::Close(_)=>break,Message::Ping(payload)=>{if outgoing.channel.send(Message::Pong(payload)).await.is_err(){break;}continue},_=>continue};
                let values=match serde_json::from_str::<Value>(&text){Ok(Value::Array(values))=>values,Ok(value)=>vec![value],Err(error)=>{let _=outgoing.send(RpcServerMessage::ClientProtocolError{error:json!({"_tag":"RpcClientError","reason":"Protocol","message":error.to_string()})}).await;break}};
                for value in values {
                    let message=match serde_json::from_value::<RpcClientMessage>(value){Ok(message)=>message,Err(error)=>{let _=outgoing.send(RpcServerMessage::ClientProtocolError{error:json!({"_tag":"RpcClientError","reason":"Protocol","message":error.to_string()})}).await;continue}};
                    match message {
                        RpcClientMessage::Ping=>{let _=outgoing.send(RpcServerMessage::Pong).await;},
                        RpcClientMessage::Eof=>{},
                        RpcClientMessage::Ack{request_id}=>{if let Some(ack)=subscriptions.get(&request_id).and_then(|task|task.ack.as_ref()){let _=ack.try_send(());}},
                        RpcClientMessage::Interrupt{request_id}=>{if let Some(stream)=subscriptions.remove(&request_id){stream.task.abort();let _=outgoing.send(RpcServerMessage::Exit{request_id,exit:RpcExit::Failure{cause:vec![RpcCause::Interrupt{fiber_id:None}]}}).await;}},
                        RpcClientMessage::Request{request}=>{
                            if let Some(previous)=subscriptions.remove(&request.id){previous.task.abort();}
                            let session=match state.auth.active_session(&session.session_id,Utc::now()){Ok(session)=>session,Err(_)=>{let _=outgoing.send(failure(request.id,json!({"_tag":"EnvironmentAuthorizationError","message":"Session expired or revoked."}))).await;continue}};
                            if let Err(error)=authorize_request(&session,&request){let _=outgoing.send(failure(request.id,error)).await;continue}
                            generation += 1;
                            let current_generation = generation;
                            let id=request.id.clone();let state=state.clone();let output=outgoing.clone();let finished=finished.clone();let completed_id=id.clone();let reported=reported.clone();
                            let (ack, task)=if matches!(request.tag.as_str(),"orchestration.subscribeShell"|"orchestration.subscribeThread"|"orchestration.subscribeArchivedShell"|"terminal.attach"|"terminal.observe"|"subscribeTerminalMetadata"|"subscribeTerminalEvents"|"subscribeDiscoveredLocalServers"|"subscribeServerConfig"|"subscribeResourceTelemetry"|"subscribeBackgroundPolicy"|"subscribeDeviceState"|"provider.auth.subscribe") {
                                let(ack,acknowledged)=mpsc::channel(1);
                                let task=tokio::spawn(async move {if request.tag=="provider.auth.subscribe" {provider_auth_stream(request,state,session,output,acknowledged).await;}else if request.tag=="subscribeDeviceState" {device_stream(request,state,session,output,acknowledged).await;}else if request.tag=="subscribeBackgroundPolicy" {background_stream(request,state,session,output,acknowledged).await;}else if request.tag=="subscribeServerConfig" {config_stream(request,state,session,output,acknowledged).await;}else if request.tag=="subscribeResourceTelemetry" {resource_telemetry_stream(request,state,session,output,acknowledged).await;}else if request.tag=="subscribeDiscoveredLocalServers" {discovery_stream(request,state,session,output,acknowledged).await;}else if request.tag.starts_with("terminal.") || request.tag.starts_with("subscribeTerminal") {terminal_stream(request,state,session,output,acknowledged).await;}else{stream(request,state,session,output,acknowledged).await;}let _=finished.send((completed_id,current_generation,None)).await;});
                                (Some(ack),task)
                            }else{
                                let task=tokio::spawn(async move {
                                    let result=execute_session_unary(state,request.clone(),&session,rpc_client,&reported).await;
                                    let response=if request.is_notification==Some(true){None}else{Some(match result{Ok(value)=>RpcServerMessage::Exit{request_id:request.id,exit:RpcExit::Success{value}},Err(error)=>failure(request.id,error)})};
                                    let _=finished.send((completed_id,current_generation,response)).await;
                                });
                                (None,task)
                            };
                            subscriptions.insert(id,Subscription{ack,generation:current_generation,task});
                        },
                    }
                }
            },
            _=&mut writer=>break,
            Some((id,generation,response))=completed.recv()=>{if subscriptions.get(&id).is_some_and(|subscription|subscription.generation==generation){subscriptions.remove(&id);if let Some(response)=response{if outgoing.send(response).await.is_err(){break;}}}}
        }
    }
    for (_, subscription) in subscriptions {
        subscription.task.abort();
        let _ = subscription.task.await;
    }
    writer.abort();
    if reported.load(std::sync::atomic::Ordering::Acquire) {
        if let Some(policy) = &leases.policy {
            policy.remove_rpc_client(&leases.session, &leases.rpc).await;
        }
    }
    leases.active = false;
}

fn background_decode<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T, Value> {
    serde_json::from_value(request.payload.clone()).map_err(
        |_| json!({"_tag":"SchemaDecodeError","message":"Invalid background request payload."}),
    )
}

async fn execute_session_unary(
    state: ApiState,
    request: RpcRequest,
    session: &Session,
    rpc_client: t3_contracts::RpcClientId,
    reported: &std::sync::atomic::AtomicBool,
) -> Result<Value, Value> {
    if request.tag.starts_with("provider.auth.") {
        return crate::provider_auth_rpc::command(
            state.provider_auth.as_ref(),
            &request.tag,
            request.payload,
            &session.session_id,
        )
        .await;
    }

    if matches!(
        request.tag.as_str(),
        "server.reportClientActivity"
            | "server.reportHostPowerState"
            | "server.getBackgroundPolicy"
    ) {
        let service=state.background.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Background policy is not configured."}))?;
        return match request.tag.as_str() {
            "server.reportClientActivity" => {
                let input: t3_contracts::ClientActivityReportInput = background_decode(&request)?;
                reported.store(true, std::sync::atomic::Ordering::Release);
                service
                    .report_client_activity(
                        t3_contracts::AuthSessionId::new(&session.session_id).unwrap(),
                        rpc_client,
                        input,
                    )
                    .await;
                Ok(Value::Null)
            }
            "server.reportHostPowerState" => {
                let input: t3_contracts::HostPowerSnapshot = background_decode(&request)?;
                service.report_host_power_state(input);
                Ok(Value::Null)
            }
            _ => {
                let _: t3_contracts::GetServerSettingsInput = background_decode(&request)?;
                Ok(serde_json::to_value(service.snapshot().await).unwrap())
            }
        };
    }
    execute_unary(state, request).await
}

async fn execute_unary(state: ApiState, request: RpcRequest) -> Result<Value, Value> {
    if matches!(
        request.tag.as_str(),
        "server.getSettings"
            | "server.updateSettings"
            | "server.getConfig"
            | "server.prepareAcpRegistryAgent"
            | "server.searchAcpRegistry"
            | "server.uninstallAcpRegistryManagedBinary"
            | "server.acceptAcpRegistryUrlAuth"
    ) {
        if matches!(
            request.tag.as_str(),
            "server.getSettings" | "server.getConfig"
        ) {
            let _: t3_contracts::GetServerSettingsInput =
                serde_json::from_value(request.payload.clone()).map_err(
                    |_| json!({"_tag":"SchemaDecodeError","message":"Expected a non-null value."}),
                )?;
        } else if !request.payload.is_object() {
            return Err(
                json!({"_tag":"SchemaDecodeError","message":"Expected an object payload."}),
            );
        }
        if request.tag == "server.acceptAcpRegistryUrlAuth" {
            let input:t3_contracts::AcpRegistryAcceptUrlAuthInput=serde_json::from_value(request.payload)
                .map_err(|_|json!({"_tag":"SchemaDecodeError","message":"Invalid ACP URL consent request."}))?;
            let providers=state.providers.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Provider registry is not configured."}))?;
            return Ok(
                json!({"accepted":providers.coordinator().accept_url_authentication(&input)}),
            );
        }
        if request.tag == "server.searchAcpRegistry" {
            let input: t3_contracts::AcpRegistrySearchInput = serde_json::from_value(request.payload).map_err(|_|json!({"_tag":"SchemaDecodeError","message":"Invalid ACP registry search request."}))?;
            let catalog = state.providers.as_ref().and_then(|providers| providers.catalog()).ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"ACP registry catalog is not configured."}))?;
            let result = catalog.search(&input).await.map_err(|error|json!({"_tag":"AcpRegistryOperationError","reason":error.reason,"message":error.detail}))?;
            return serde_json::to_value(result).map_err(|_|json!({"_tag":"NativeServiceError","message":"ACP search result could not be encoded."}));
        }
        if request.tag == "server.uninstallAcpRegistryManagedBinary" {
            let input: t3_contracts::AcpRegistryManagedBinaryUninstallInput = serde_json::from_value(request.payload).map_err(|_|json!({"_tag":"SchemaDecodeError","message":"Invalid ACP registry uninstall request."}))?;
            let catalog = state.providers.as_ref().and_then(|providers| providers.catalog()).ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"ACP registry catalog is not configured."}))?;
            let service = state.settings.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Settings service is not configured."}))?;
            let result = catalog.uninstall_managed_binary(&input,service).await.map_err(|error|json!({"_tag":"AcpRegistryOperationError","reason":error.reason,"message":error.detail}))?;
            return serde_json::to_value(result).map_err(|_|json!({"_tag":"NativeServiceError","message":"ACP uninstall result could not be encoded."}));
        }
        if request.tag == "server.prepareAcpRegistryAgent" {
            let input:t3_contracts::AcpRegistryPrepareInput=serde_json::from_value(request.payload).map_err(|_|json!({"_tag":"SchemaDecodeError","message":"Invalid ACP preparation request."}))?;
            let providers=state.providers.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Provider registry is not configured."}))?;
            let catalog=providers.catalog().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"ACP registry catalog is not configured."}))?;
            let result=catalog.prepare(&input).await.map_err(|error|json!({"_tag":"AcpRegistryOperationError","reason":error.reason,"message":error.detail}))?;
            return serde_json::to_value(result).map_err(|_|json!({"_tag":"NativeServiceError","message":"ACP preparation result could not be encoded."}));
        }
        if let Some(service) = &state.settings {
            let settings = if request.tag == "server.updateSettings" {
                let mut input:t3_contracts::UpdateServerSettingsInput=serde_json::from_value(request.payload).map_err(|_|json!({"_tag":"SchemaDecodeError","message":"Invalid settings update payload."}))?;
                if let Some(hosts) = input.patch.device_hosts.take() {
                    let hosts = if hosts.0.is_empty() {
                        hosts.0
                    } else {
                        let resolver = state.device_hosts.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Device-host resolver is not configured."}))?;
                        resolver.remote_hosts(hosts.0).await
                    };
                    input.patch.device_hosts = Some(t3_contracts::SshDeviceHostConfigs(hosts));
                }
                match input.provider_instance_mutation {
                    Some(mutation) => {
                        service
                            .update_provider_instance(mutation, input.patch)
                            .await
                    }
                    None => service.update(input.patch).await,
                }
                .map_err(|error| error.wire())?
            } else {
                service.snapshot().await.map_err(|error| error.wire())?
            };
            let settings = crate::provider_registry::redact_settings(&settings);
            if request.tag == "server.getConfig" {
                let mut config=state.config.clone().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Native configuration is unavailable."}))?;
                config["settings"] = settings;
                if let Some(providers) = state.providers {
                    config["providers"] = json!(providers.snapshots());
                }
                return Ok(config);
            }
            return Ok(settings);
        }
        if request.tag != "server.getConfig" {
            return Err(
                json!({"_tag":"NativeServiceUnavailableError","message":"Settings service is not configured."}),
            );
        }
    }
    if matches!(
        request.tag.as_str(),
        "server.getHostResources"
            | "server.getProcessDiagnostics"
            | "server.getProcessResourceHistory"
            | "server.signalProcess"
    ) {
        let failure = |error: String| json!({"_tag":"NativeServiceError","message":error});
        if matches!(
            request.tag.as_str(),
            "server.getHostResources" | "server.getProcessDiagnostics"
        ) && request.payload.is_null()
        {
            return Err(json!({"_tag":"SchemaDecodeError","message":"Expected a non-null value."}));
        }
        if request.tag == "server.getHostResources" {
            let host=state.host_resources.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Host resources is not configured."}))?;
            return Ok(serde_json::to_value(
                host.read()
                    .await
                    .map_err(|error| failure(error.to_string()))?,
            )
            .unwrap());
        }
        let telemetry=state.resource_telemetry.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Resource telemetry is not configured."}))?;
        return match request.tag.as_str() {
            "server.getProcessResourceHistory" => {
                let input: t3_contracts::ServerProcessResourceHistoryInput =
                    terminal_decode(&request)?;
                Ok(serde_json::to_value(
                    crate::process_diagnostics::ProcessResourceMonitor(telemetry.clone())
                        .read_history(&input)
                        .await
                        .map_err(|error| failure(error.to_string()))?,
                )
                .unwrap())
            }
            "server.getProcessDiagnostics" => Ok(serde_json::to_value(
                crate::process_diagnostics::ProcessDiagnostics::new(
                    telemetry.clone(),
                    u64::from(std::process::id()),
                )
                .read()
                .await
                .map_err(|error| failure(error.to_string()))?,
            )
            .unwrap()),
            _ => {
                let input: t3_contracts::ServerSignalProcessInput = terminal_decode(&request)?;
                Ok(serde_json::to_value(
                    crate::process_diagnostics::ProcessDiagnostics::new(
                        telemetry.clone(),
                        u64::from(std::process::id()),
                    )
                    .signal(&input)
                    .await,
                )
                .unwrap())
            }
        };
    }

    if matches!(
        request.tag.as_str(),
        "server.getResourceTelemetryHistory" | "server.retryResourceTelemetry"
    ) {
        let service = state.resource_telemetry.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Resource telemetry is not configured."}))?;
        return match request.tag.as_str() {
            "server.getResourceTelemetryHistory" => {
                let input: t3_contracts::ResourceTelemetryHistoryInput = terminal_decode(&request)?;
                let history = service.read_history(&input).await;
                let typed = history.wire().map_err(
                    |error| json!({"_tag":"NativeServiceError","message":error.to_string()}),
                )?;
                Ok(serde_json::to_value(typed).unwrap())
            }
            _ => {
                if request.payload.is_null() {
                    return Err(
                        json!({"_tag":"SchemaDecodeError","message":"Expected a non-null value."}),
                    );
                }
                let typed: t3_contracts::ResourceTelemetryRetryResult =
                    serde_json::from_value(service.retry()).map_err(
                        |error| json!({"_tag":"NativeServiceError","message":error.to_string()}),
                    )?;
                Ok(serde_json::to_value(typed).unwrap())
            }
        };
    }
    if request.tag.starts_with("terminal.") {
        return terminal_rpc(&state, &request).await;
    }
    if matches!(
        request.tag.as_str(),
        "device.list"
            | "device.configure"
            | "device.open"
            | "device.close"
            | "device.shutdown"
            | "device.detail"
            | "device.action"
    ) {
        return device_rpc(&state, &request).await;
    }
    if request.tag == "filesystem.browse"
        || matches!(
            request.tag.as_str(),
            "projects.searchEntries" | "projects.searchContents" | "projects.listEntries"
        )
    {
        return workspace_rpc(&state, &request).await;
    }
    let workspace = state.workspace.clone();
    let cwd = request.payload["cwd"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let write = request.tag == "projects.writeFile";
    let result = tokio::task::spawn_blocking(move || unary(&state, &request))
        .await
        .unwrap_or_else(|error| {
            Err(json!({"_tag":"NativeServiceError","message":error.to_string()}))
        });
    if write && result.is_ok() {
        if let Some(workspace) = workspace {
            tokio::task::spawn_blocking(move || workspace.refresh(&cwd))
                .await
                .map_err(
                    |error| json!({"_tag":"NativeServiceError","message":error.to_string()}),
                )?;
        }
    }
    result
}

fn failure(id: RpcRequestId, error: Value) -> RpcServerMessage {
    RpcServerMessage::Exit {
        request_id: id,
        exit: RpcExit::Failure {
            cause: vec![if error["_tag"] == "SchemaDecodeError" {
                RpcCause::Die {
                    defect: error["message"].clone(),
                }
            } else {
                RpcCause::Fail { error }
            }],
        },
    }
}
fn authorize_rpc(session: &Session, method: &str) -> Result<(), Value> {
    let scope=t3_contracts::rpc_required_scope(method).map_err(|_|json!({"_tag":"NativeMethodUnsupportedError","method":method,"message":"Unknown RPC method."}))?;
    require_scope(session, method, scope)
}
fn require_scope(
    session: &Session,
    method: &str,
    scope: AuthEnvironmentScope,
) -> Result<(), Value> {
    if session.scopes.contains(&scope) {
        Ok(())
    } else {
        let required = t3_contracts::auth_scope_required_response(scope);
        Err(
            json!({"_tag":"EnvironmentAuthorizationError","operation":method,"requiredScope":required.required_scope,"requiredPermission":required.required_permission,"message":"The client does not have permission for this operation."}),
        )
    }
}
fn authorize_request(session: &Session, request: &RpcRequest) -> Result<(), Value> {
    if request.tag == "device.list" {
        let input: t3_contracts::DeviceListInput = device_decode(request)?;
        return require_scope(
            session,
            &request.tag,
            if input.update_tool.flatten().is_some()
                || (input.inspect_only.flatten() != Some(true)
                    && input.retry_host_id.flatten().is_some())
            {
                AuthEnvironmentScope::OrchestrationOperate
            } else {
                AuthEnvironmentScope::OrchestrationRead
            },
        );
    }
    if request.tag != "server.updateSettings" {
        return authorize_rpc(session, &request.tag);
    }
    let input: t3_contracts::UpdateServerSettingsInput =
        serde_json::from_value(request.payload.clone()).map_err(
            |_| json!({"_tag":"SchemaDecodeError","message":"Invalid settings update payload."}),
        )?;
    let mut scopes = input.patch.required_scopes();
    if input.provider_instance_mutation.is_some() {
        // Source grants provider-only mutations independently of settings; an
        // actual patch retains all of its own required grants.
        if serde_json::to_value(&input.patch)
            .unwrap()
            .as_object()
            .unwrap()
            .is_empty()
        {
            scopes.clear();
        }
        if !scopes.contains(&AuthEnvironmentScope::ProvidersManage) {
            scopes.push(AuthEnvironmentScope::ProvidersManage);
        }
    }
    for scope in scopes {
        require_scope(session, &request.tag, scope)?;
    }
    Ok(())
}

fn unary(state: &ApiState, request: &RpcRequest) -> Result<Value, Value> {
    let error = |error: StoreError| json!({"_tag":"NativeServiceError","method":request.tag,"message":error.to_string()});
    match request.tag.as_str(){
        "server.probe"=>Ok(json!({})),
        "server.getConfig"=>state.config.clone().ok_or_else(||json!({"_tag":"NativeMethodUnsupportedError","method":request.tag,"message":"Native configuration service is not yet available."})),
        "orchestration.launchThread"=>{
            let service=match &state.providers{Some(providers)=>crate::launch::ThreadLaunchService::with_providers(state.store.clone(),providers.clone()),None=>crate::launch::ThreadLaunchService::new(state.store.clone())};
            service.launch(request.payload.clone(),Utc::now()).map_err(error)
        },
        "orchestration.dispatchCommand"=>{
            let receipt=if request.payload["type"].as_str().is_some_and(|kind|kind.starts_with("thread.")) {ThreadService::new(state.store.clone()).dispatch(&request.payload,Utc::now()).map_err(error)?}else{state.execution.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Native provider execution is not configured."}))?.dispatch(&request.payload,Utc::now()).map_err(error)?};
            if receipt.status=="rejected"{return Err(json!({"_tag":"OrchestrationV2DispatchCommandError","commandId":receipt.command_id,"commandType":receipt.command_type,"message":"Command rejected.","detail":receipt.error.unwrap_or(Value::Null).to_string()}));}
            Ok(json!({"sequence":receipt.result_sequence}))
        },
        "orchestration.getThreadProjection"=>{
            let id=request.payload["threadId"].as_str().ok_or_else(||json!({"_tag":"OrchestrationV2GetThreadProjectionError","threadId":"","message":"threadId is required."}))?;
            ThreadService::new(state.store.clone()).projection(id).map_err(error)?.map(|value|crate::wire_projection::projection(&value)).ok_or_else(||json!({"_tag":"OrchestrationV2GetThreadProjectionError","threadId":id,"message":"Thread does not exist."}))
        },
        "orchestration.getTurnItem"=>crate::history::HistoryService::new(state.store.clone()).detail(request.payload.clone()).map_err(error),
        "orchestration.getArchivedShellSnapshot"=>{
            let mut snapshot=ThreadService::new(state.store.clone()).shell_snapshot().map_err(error)?;
            snapshot["threads"]=snapshot["archivedThreads"].take();snapshot.as_object_mut().unwrap().remove("archivedThreads");Ok(snapshot)
        },
        "projects.mutate"=>ProjectService::new(state.store.clone()).mutate(request.payload.clone(),Utc::now()).map_err(|error|json!({"_tag":"ProjectMutationError","commandId":request.payload["commandId"],"message":error.to_string()})),
        "projects.readFile"=>{
            let input:t3_contracts::ProjectReadFileInput=serde_json::from_value(request.payload.clone()).map_err(|error|json!({"_tag":"ProjectReadFileError","message":error.to_string()}))?;
            crate::workspace_files::read_file(&input).map_err(|error|error.rpc_error(false,input.cwd.as_str(),input.relative_path.0.as_str()))
        },
        "projects.writeFile"=>{
            let input:t3_contracts::ProjectWriteFileInput=serde_json::from_value(request.payload.clone()).map_err(|error|json!({"_tag":"ProjectWriteFileError","message":error.to_string()}))?;
            crate::workspace_files::write_file(&input).map_err(|error|error.rpc_error(true,input.cwd.as_str(),input.relative_path.0.as_str()))
        },
        _=>Err(json!({"_tag":"NativeMethodUnsupportedError","method":request.tag,"message":"This method has not yet been ported."})),
    }
}
async fn workspace_rpc(state: &ApiState, request: &RpcRequest) -> Result<Value, Value> {
    let service=state.workspace.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Workspace index service is not configured."}))?.clone();
    let input = request.payload.clone();
    let tag = request.tag.as_str();
    let error_tag = match tag {
        "filesystem.browse" => "FilesystemBrowseError",
        "projects.searchEntries" => "ProjectSearchEntriesError",
        "projects.searchContents" => "ProjectSearchContentsError",
        _ => "ProjectListEntriesError",
    };
    let invalid = |cause: serde_json::Error| json!({"_tag":error_tag,"message":cause.to_string()});
    let result = match tag {
        "filesystem.browse" => {
            let decoded =
                serde_json::from_value::<t3_contracts::FilesystemBrowseInput>(input.clone())
                    .map_err(invalid)?;
            crate::workspace_entries::blocking(move || service.browse(&decoded)).await
        }
        "projects.searchEntries" => {
            let decoded =
                serde_json::from_value::<t3_contracts::ProjectSearchEntriesInput>(input.clone())
                    .map_err(invalid)?;
            crate::workspace_entries::blocking(move || service.search(&decoded)).await
        }
        "projects.searchContents" => {
            let decoded =
                serde_json::from_value::<t3_contracts::ProjectSearchContentsInput>(input.clone())
                    .map_err(invalid)?;
            crate::workspace_entries::blocking(move || service.search_contents(&decoded)).await
        }
        _ => {
            let decoded =
                serde_json::from_value::<t3_contracts::ProjectListEntriesInput>(input.clone())
                    .map_err(invalid)?;
            service.list(&decoded).await
        }
    };
    result.map_err(|cause| cause.rpc_error(error_tag, &input))
}

fn terminal_decode<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T, Value> {
    serde_json::from_value(request.payload.clone()).map_err(
        |error| json!({"_tag":"RpcServerError","reason":"Decode","message":error.to_string()}),
    )
}
async fn terminal_rpc(state: &ApiState, request: &RpcRequest) -> Result<Value, Value> {
    let service=state.terminals.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Terminal service is not configured."}))?;
    let result = match request.tag.as_str() {
        "terminal.open" => {
            return service
                .open(terminal_decode(request)?)
                .await
                .map(|snapshot| serde_json::to_value(snapshot).unwrap())
                .map_err(|error| error.wire());
        }
        "terminal.restart" => {
            return service
                .restart(terminal_decode(request)?)
                .await
                .map(|snapshot| serde_json::to_value(snapshot).unwrap())
                .map_err(|error| error.wire());
        }
        "terminal.write" => service.write(terminal_decode(request)?).await,
        "terminal.resize" => service.resize(terminal_decode(request)?).await,
        "terminal.clear" => service.clear(terminal_decode(request)?).await,
        "terminal.close" => service.close(terminal_decode(request)?).await,
        _ => {
            return Err(
                json!({"_tag":"NativeMethodUnsupportedError","method":request.tag,"message":"This method has not yet been ported."}),
            );
        }
    };
    result.map(|()| Value::Null).map_err(|error| error.wire())
}
fn discovery_value(
    servers: Vec<crate::resource_ports::LocalServer>,
    scanned_at: i64,
) -> Result<Value, Value> {
    let value = json!({"servers":servers,"scannedAt":chrono::DateTime::from_timestamp_millis(scanned_at).ok_or_else(||json!({"_tag":"NativeServiceError","message":"Invalid discovery timestamp."}))?.to_rfc3339_opts(chrono::SecondsFormat::Millis,true),"configuredUrlProbing":true});
    let typed: t3_contracts::DiscoveredLocalServerList = serde_json::from_value(value)
        .map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))?;
    serde_json::to_value(typed)
        .map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))
}

async fn provider_auth_stream(
    request: RpcRequest,
    state: ApiState,
    session: Session,
    output: SocketSender,
    mut ack: mpsc::Receiver<()>,
) {
    let result = async {
        let mut states = crate::provider_auth_rpc::subscribe(state.provider_auth.as_ref(),request.payload.clone(),&session.session_id).await?;
        while let Some(state_value) = states.recv().await {
            let active = state.auth.active_session(&session.session_id,Utc::now()).map_err(|_|json!({"_tag":"EnvironmentAuthorizationError","message":"Session expired or revoked."}))?;
            authorize_rpc(&active,&request.tag)?;
            let value = state_value.map_err(|error|serde_json::to_value(error).unwrap())?;
            if !chunk(&output,&mut ack,&request.id,vec![serde_json::to_value(value).unwrap()]).await { return Ok(()); }
        }
        Ok::<_,Value>(())
    }.await;
    let message = match result {
        Ok(()) => RpcServerMessage::Exit {
            request_id: request.id,
            exit: RpcExit::Success { value: Value::Null },
        },
        Err(error) => failure(request.id, error),
    };
    let _ = output.send(message).await;
}

async fn resource_telemetry_stream(
    request: RpcRequest,
    state: ApiState,
    session: Session,
    output: SocketSender,
    mut ack: mpsc::Receiver<()>,
) {
    let result=async {
        // Original Schema.Struct({}) accepts every non-null encoded value.
        if request.payload.is_null(){return Err(json!({"_tag":"SchemaDecodeError","message":"Expected a non-null value."}));}
        let service=state.resource_telemetry.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Resource telemetry is not configured."}))?;
        let mut subscription=service.subscribe().await.ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Resource telemetry stopped."}))?;
        let mut next=subscription.latest.clone();
        loop {
            let active=state.auth.active_session(&session.session_id,Utc::now()).map_err(|_|json!({"_tag":"EnvironmentAuthorizationError","message":"Session expired or revoked."}))?;
            authorize_rpc(&active,&request.tag)?;
            let typed: t3_contracts::ResourceTelemetrySnapshot=serde_json::from_value(next).map_err(|error|json!({"_tag":"NativeServiceError","message":error.to_string()}))?;
            let sent=tokio::select! {biased;_=service.closed()=>return Ok(()),sent=chunk(&output,&mut ack,&request.id,vec![serde_json::to_value(typed).unwrap()])=>sent};
            if !sent{return Ok(());}
            match subscription.recv().await {Some(value)=>next=value,None=>return Ok(())}
        }
    }.await;
    let response = match result {
        Ok(()) => RpcServerMessage::Exit {
            request_id: request.id,
            exit: RpcExit::Success { value: Value::Null },
        },
        Err(error) => failure(request.id, error),
    };
    let _ = output.send(response).await;
}

fn device_decode<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T, Value> {
    serde_json::from_value(request.payload.clone())
        .map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))
}
async fn device_rpc(state: &ApiState, request: &RpcRequest) -> Result<Value, Value> {
    let service=state.devices.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Device service is not configured."}))?;
    let result = match request.tag.as_str() {
        "device.list" => {
            let input: t3_contracts::DeviceListInput = device_decode(request)?;
            let value = if let Some(tool) = input.update_tool.flatten() {
                service.update_tool(tool).await
            } else if input.inspect_only.flatten() == Some(true) {
                Ok(service.inspect().await)
            } else if let Some(host) = input.retry_host_id.flatten() {
                service.retry_host(host).await
            } else {
                service.list().await
            };
            value.map(|value| serde_json::to_value(value).unwrap())
        }
        "device.configure" => service
            .configure(device_decode(request)?)
            .await
            .map(|value| serde_json::to_value(value).unwrap()),
        "device.open" => service
            .open(device_decode(request)?)
            .await
            .map(|value| serde_json::to_value(value).unwrap()),
        "device.detail" => service
            .detail(device_decode(request)?)
            .await
            .map(|value| serde_json::to_value(value).unwrap()),
        "device.action" => service
            .action(device_decode(request)?)
            .await
            .map(|value| serde_json::to_value(value).unwrap()),
        "device.close" => service
            .close(device_decode(request)?)
            .await
            .map(|()| Value::Null),
        "device.shutdown" => service
            .shutdown_device(device_decode(request)?)
            .await
            .map(|()| Value::Null),
        _ => unreachable!(),
    };
    result.map_err(|error| serde_json::to_value(error).unwrap())
}
async fn device_stream(
    request: RpcRequest,
    state: ApiState,
    session: Session,
    output: SocketSender,
    mut ack: mpsc::Receiver<()>,
) {
    let result=async {
        let _:t3_contracts::GetServerSettingsInput=device_decode(&request)?;
        let service=state.devices.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Device service is not configured."}))?;
        let mut subscription=service.subscribe();let mut next=subscription.snapshot.clone();
        loop {
            let active=state.auth.active_session(&session.session_id,Utc::now()).map_err(|_|json!({"_tag":"EnvironmentAuthorizationError","message":"Session expired or revoked."}))?;
            authorize_rpc(&active,&request.tag)?;
            let sent=tokio::select! {biased;_=service.closed()=>return Ok::<(),Value>(()),sent=chunk(&output,&mut ack,&request.id,vec![serde_json::to_value(next).unwrap()])=>sent};
            if !sent{return Ok(());}
            match subscription.recv().await {Some(value)=>next=value,None=>return Ok(())}
        }
    }.await;
    let response = match result {
        Ok(()) => RpcServerMessage::Exit {
            request_id: request.id,
            exit: RpcExit::Success { value: Value::Null },
        },
        Err(error) => failure(request.id, error),
    };
    let _ = output.send(response).await;
}

async fn background_stream(
    request: RpcRequest,
    state: ApiState,
    session: Session,
    output: SocketSender,
    mut ack: mpsc::Receiver<()>,
) {
    let result=async {
        let _:t3_contracts::GetServerSettingsInput=background_decode(&request)?;
        let service=state.background.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Background policy is not configured."}))?;
        let mut subscription=service.subscribe().await;let mut next=subscription.latest.clone();
        loop {
            let active=state.auth.active_session(&session.session_id,Utc::now()).map_err(|_|json!({"_tag":"EnvironmentAuthorizationError","message":"Session expired or revoked."}))?;
            authorize_rpc(&active,&request.tag)?;
            let sent=tokio::select!{biased;_=service.closed()=>return Ok::<(),Value>(()),sent=chunk(&output,&mut ack,&request.id,vec![serde_json::to_value(next).unwrap()])=>sent};
            if !sent{return Ok(());}
            match subscription.recv().await{Some(snapshot)=>next=snapshot,None=>return Ok(())}
        }
    }.await;
    let response = match result {
        Ok(()) => RpcServerMessage::Exit {
            request_id: request.id,
            exit: RpcExit::Success { value: Value::Null },
        },
        Err(error) => failure(request.id, error),
    };
    let _ = output.send(response).await;
}

async fn discovery_stream(
    request: RpcRequest,
    state: ApiState,
    session: Session,
    output: SocketSender,
    mut ack: mpsc::Receiver<()>,
) {
    let result = async {
        let service = state.discovery.as_ref().ok_or_else(|| json!({"_tag":"NativeServiceUnavailableError","message":"Port discovery is not configured."}))?;
        let input: t3_contracts::SubscribeDiscoveredLocalServersInput = terminal_decode(&request)?;
        let urls = input.configured_urls.flatten().map(|urls| urls.0.into_iter().map(|url|url.0.as_str().to_owned()).collect::<Vec<_>>()).unwrap_or_default();
        let _retention = service.retain().await.map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))?;
        let initial = service.scan(&urls).await.map_err(|error|json!({"_tag":"NativeServiceError","message":error.to_string()}))?;
        let clock = service.clock();
        let mut update = discovery_value(initial.clone(),clock())?;
        let (updates, mut updated) = mpsc::unbounded_channel();
        let _subscription = service.subscribe_callback(&urls, initial, std::sync::Arc::new(move |servers| {
            let updates = updates.clone();
            let clock = clock.clone();
            Box::pin(async move {let _ = updates.send(discovery_value(servers,clock()));})
        })).map_err(|error|json!({"_tag":"NativeServiceError","message":error.to_string()}))?;
        loop {
            let active = state.auth.active_session(&session.session_id,Utc::now()).map_err(|_|json!({"_tag":"EnvironmentAuthorizationError","message":"Session expired or revoked."}))?;
            authorize_rpc(&active,&request.tag)?;
            let value = update;
            let admitted = tokio::select! {biased;_=service.closed()=>return Ok(()),admitted=chunk(&output,&mut ack,&request.id,vec![value])=>admitted};
            if !admitted {return Ok(());}
            let next = tokio::select! {biased;_=service.closed()=>return Ok(()),next=updated.recv()=>next};
            let Some(next) = next else {return Ok(());};
            update = next?;
        }
    }.await;
    let response = match result {
        Ok(()) => RpcServerMessage::Exit {
            request_id: request.id,
            exit: RpcExit::Success { value: Value::Null },
        },
        Err(error) => failure(request.id, error),
    };
    let _ = output.send(response).await;
}

async fn terminal_stream(
    request: RpcRequest,
    state: ApiState,
    session: Session,
    output: SocketSender,
    mut ack: mpsc::Receiver<()>,
) {
    let id = request.id.clone();
    let result=async {
        let service=state.terminals.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Terminal service is not configured."}))?;
        let mut subscription=match request.tag.as_str() {
            "terminal.attach"=>service.attach(terminal_decode(&request)?).await.map_err(|error|error.wire())?,
            "terminal.observe"=>service.observe(terminal_decode(&request)?).await.map_err(|error|error.wire())?,
            "subscribeTerminalMetadata"=>service.metadata(),
            "subscribeTerminalEvents"=>service.events(),
            _=>return Err(json!({"_tag":"NativeMethodUnsupportedError","method":request.tag})),
        };
        loop {
            let Some(item)=subscription.recv().await.map_err(|message|json!({"_tag":"RpcServerError","reason":"TerminalStreamContinuity","message":message}))? else {return Ok(());};
            let active=state.auth.active_session(&session.session_id,Utc::now()).map_err(|_|json!({"_tag":"EnvironmentAuthorizationError","message":"Session expired or revoked."}))?;
            authorize_rpc(&active,&request.tag)?;
            if !chunk(&output,&mut ack,&request.id,vec![item]).await{return Ok(());}
        }
    }.await;
    if let Err(error) = result {
        let _ = output.send(failure(id, error)).await;
    } else {
        let _ = output
            .send(RpcServerMessage::Exit {
                request_id: id,
                exit: RpcExit::Success { value: Value::Null },
            })
            .await;
    }
}

async fn chunk(
    output: &SocketSender,
    ack: &mut mpsc::Receiver<()>,
    id: &RpcRequestId,
    values: Vec<Value>,
) -> bool {
    output
        .send(RpcServerMessage::Chunk {
            request_id: id.clone(),
            values,
        })
        .await
        .is_ok()
        && ack.recv().await.is_some()
}

async fn stream(
    request: RpcRequest,
    state: ApiState,
    session: Session,
    output: SocketSender,
    mut ack: mpsc::Receiver<()>,
) {
    let id = request.id.clone();
    let result = stream_inner(&request, &state, &session, &output, &mut ack).await;
    if let Err(error) = result {
        let _ = output.send(failure(id, error)).await;
    }
}

async fn pump_provider_statuses(
    source: impl futures_util::Stream<Item = std::sync::Arc<Vec<Value>>>,
    mut previous: String,
    output: mpsc::UnboundedSender<std::sync::Arc<Vec<Value>>>,
) {
    futures_util::pin_mut!(source);
    let mut pending: Option<(std::sync::Arc<Vec<Value>>, tokio::time::Instant)> = None;
    loop {
        tokio::select! {
            next=source.next()=>{
                let Some(next)=next else {break};
                let encoded=serde_json::to_string(next.as_ref()).unwrap();
                if encoded!=previous {
                    previous=encoded;
                    pending=Some((next,tokio::time::Instant::now()+std::time::Duration::from_millis(200)));
                }
            },
            _=async {match &pending {Some((_,deadline))=>tokio::time::sleep_until(*deadline).await,None=>std::future::pending().await}}=>{
                let (providers,_)=pending.take().unwrap();
                if output.send(providers).is_err() {break;}
            },
        }
    }
}

async fn config_stream(
    request: RpcRequest,
    state: ApiState,
    session: Session,
    output: SocketSender,
    mut ack: mpsc::Receiver<()>,
) {
    let result=async {
        let flags:t3_contracts::SubscribeServerConfigInput=serde_json::from_value(request.payload.clone()).map_err(|_|json!({"_tag":"SchemaDecodeError","message":"Invalid config subscription payload."}))?;
        // Optional sources are merged once their native services are available.
        let _=(flags.environment_themes,flags.usage_limit_sources,flags.usage_limits_command);
        let service=state.settings.as_ref().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Settings service is not configured."}))?;
        let mut settings=service.subscribe().await.map_err(|error|error.wire())?;
        let (providers,provider_changes)=match &state.providers {
            Some(registry)=>{let (snapshot,changes)=registry.snapshot_and_subscribe();(Some(snapshot),Some(changes))},
            None=>(None,None),
        };
        let mut config=state.config.clone().ok_or_else(||json!({"_tag":"NativeServiceUnavailableError","message":"Native configuration is unavailable."}))?;
        config["settings"]=crate::provider_registry::redact_settings(&settings.snapshot);
        if let Some(providers)=providers {config["providers"]=json!(providers);}
        let (changes,mut provider_events)=mpsc::unbounded_channel();
        struct Abort(tokio::task::JoinHandle<()>);
        impl Drop for Abort {fn drop(&mut self) {self.0.abort();}}
        let initial=serde_json::to_string(&config["providers"]).unwrap();
        let _provider_task=provider_changes.map(|provider_changes|Abort(tokio::spawn(async move {
            let source=futures_util::stream::unfold(provider_changes,|mut changes|async move {
                changes.recv().await.map(|next|(next,changes))
            });
            pump_provider_statuses(source,initial,changes).await;
        })));
        let mut provider_active = _provider_task.is_some();
        let mut item=json!({"version":1,"type":"snapshot","config":config});
        loop {
            let typed:t3_contracts::ServerConfigStreamEvent=serde_json::from_value(item).map_err(|_|json!({"_tag":"NativeServiceError","message":"Native config event did not match its contract."}))?;
            item=serde_json::to_value(typed).unwrap();
            state.auth.active_session(&session.session_id,Utc::now()).map_err(|_|json!({"_tag":"EnvironmentAuthorizationError","message":"Session expired or revoked."}))?;
            let sent=tokio::select! {biased;_=service.closed()=>return Ok::<(),Value>(()),sent=chunk(&output,&mut ack,&request.id,vec![item])=>sent};
            if !sent {return Ok(());}
            item=loop {
                tokio::select! {
                    biased;
                    _=service.closed()=>return Ok(()),
                    next=settings.recv()=>break match next {Some(settings)=>json!({"version":1,"type":"settingsUpdated","payload":{"settings":crate::provider_registry::redact_settings(&settings)}}),None=>return Ok(())},
                    next=provider_events.recv(),if provider_active=>match next {
                        Some(providers)=>break json!({"version":1,"type":"providerStatuses","payload":{"providers":providers.as_ref()}}),
                        None=>provider_active=false,
                    },
                }
            };
        }
    }.await;
    let response = match result {
        Ok(()) => RpcServerMessage::Exit {
            request_id: request.id,
            exit: RpcExit::Success { value: Value::Null },
        },
        Err(error) => failure(request.id, error),
    };
    let _ = output.send(response).await;
}

async fn stream_inner(
    request: &RpcRequest,
    state: &ApiState,
    session: &Session,
    output: &SocketSender,
    ack: &mut mpsc::Receiver<()>,
) -> Result<(), Value> {
    let mut live = state.store.subscribe();
    let is_thread = request.tag == "orchestration.subscribeThread";
    let archive = request.tag == "orchestration.subscribeArchivedShell";
    let thread_id = request.payload["threadId"].as_str();
    if is_thread && thread_id.is_none() {
        return Err(
            json!({"_tag":"OrchestrationV2GetThreadProjectionError","threadId":"","message":"threadId is required."}),
        );
    }
    let mut snapshot = if is_thread {
        read_thread_snapshot(&state.store, thread_id.unwrap())
    } else {
        ThreadService::new(state.store.clone()).shell_snapshot()
    }
    .map_err(
        |error| json!({"_tag":"OrchestrationV2GetShellSnapshotError","message":error.to_string()}),
    )?;
    let through = snapshot["snapshotSequence"].as_u64().unwrap();
    if !is_thread && !archive {
        snapshot["archivedThreads"] = json!([]);
    }
    let after = request.payload.get("afterSequence").and_then(Value::as_u64);
    if let Some(after) = after.filter(|after| *after <= through) {
        let mut cursor = after;
        loop {
            let events=state.store.events(cursor,Some(through),None,None,256).map_err(|error|json!({"_tag":"OrchestrationV2GetShellSnapshotError","message":error.to_string()}))?;
            if events.is_empty() {
                break;
            }
            for event in events {
                cursor = event.sequence;
                if let Some(value) = stream_item(state, &event, is_thread, archive, thread_id)? {
                    if !chunk(output, ack, &request.id, vec![value]).await {
                        return Ok(());
                    }
                }
            }
        }
    } else {
        let value = if is_thread {
            let projection = crate::wire_projection::projection(&snapshot["projection"]);
            if request.payload["acceptBoundedSnapshot"] == true {
                let mut bounded = crate::history::bounded_projection(
                    &projection,
                    through,
                    crate::history::PagePolicy::default(),
                );
                bounded["kind"] = json!("snapshot");
                bounded
            } else {
                json!({"kind":"snapshot","snapshotSequence":through,"projection":projection})
            }
        } else if archive {
            let mut snapshot = snapshot.clone();
            snapshot["threads"] = snapshot["archivedThreads"].take();
            snapshot.as_object_mut().unwrap().remove("archivedThreads");
            json!({"kind":"snapshot","snapshot":snapshot})
        } else {
            json!({"kind":"snapshot","snapshot":snapshot})
        };
        if !chunk(output, ack, &request.id, vec![value]).await {
            return Ok(());
        }
    }
    if request.payload["requestCompletionMarker"] == true && !archive {
        if !chunk(
            output,
            ack,
            &request.id,
            vec![json!({"kind":"synchronized"})],
        )
        .await
        {
            return Ok(());
        }
    }
    loop {
        let events=live.recv().await.map_err(|error|json!({"_tag":"OrchestrationV2GetShellSnapshotError","message":format!("Live subscription lost continuity; reconnect with its sequence: {error}")}))?;
        let active=state.auth.active_session(&session.session_id,Utc::now()).map_err(|_|json!({"_tag":"EnvironmentAuthorizationError","message":"Session expired or revoked."}))?;
        authorize_rpc(&active, &request.tag)?;
        for event in events.into_iter().filter(|event| event.sequence > through) {
            if let Some(value) = stream_item(state, &event, is_thread, archive, thread_id)? {
                if !chunk(output, ack, &request.id, vec![value]).await {
                    return Ok(());
                }
            }
        }
    }
}

fn stream_item(
    state: &ApiState,
    event: &StoredEvent,
    is_thread: bool,
    archive: bool,
    thread_id: Option<&str>,
) -> Result<Option<Value>, Value> {
    if is_thread {
        return Ok((event.event.aggregate_kind=="thread"&&Some(event.event.aggregate_id.as_str())==thread_id).then(||json!({"kind":"event","sequence":event.sequence,"event":crate::wire_projection::domain_event(crate::thread::wire_event(event))})));
    }
    if event.event.aggregate_kind == "project" {
        if archive {
            return Ok(None);
        }
        if event.event.event_type == "project.deleted" {
            return Ok(Some(
                json!({"kind":"project.removed","sequence":event.sequence,"projectId":event.event.aggregate_id}),
            ));
        }
        let project = ProjectService::new(state.store.clone())
            .get(&event.event.aggregate_id)
            .map_err(|error| json!({"message":error.to_string()}))?;
        return Ok(project.map(
            |project| json!({"kind":"project.updated","sequence":event.sequence,"project":project}),
        ));
    }
    if event.event.aggregate_kind != "thread" {
        return Ok(None);
    }
    let projection = ThreadService::new(state.store.clone())
        .projection(&event.event.aggregate_id)
        .map_err(|error| json!({"message":error.to_string()}))?;
    let shell = projection
        .filter(|projection| projection["thread"]["deletedAt"].is_null())
        .map(|projection| crate::thread::shell(&projection));
    if archive {
        if let Some(shell) = shell.filter(|shell| !shell["archivedAt"].is_null()) {
            return Ok(Some(
                json!({"kind":"thread.updated","sequence":event.sequence,"thread":shell}),
            ));
        }
        if event.event.event_type == "thread.unarchived"
            || (event.event.event_type == "thread.deleted"
                && !event.event.payload["archivedAt"].is_null())
        {
            return Ok(Some(
                json!({"kind":"thread.removed","sequence":event.sequence,"threadId":event.event.aggregate_id}),
            ));
        }
        return Ok(None);
    }
    match shell.filter(|shell| shell["archivedAt"].is_null()) {
        Some(shell) => Ok(Some(
            json!({"kind":"thread.updated","sequence":event.sequence,"location":"active","thread":shell}),
        )),
        None => Ok(Some(
            json!({"kind":"thread.removed","sequence":event.sequence,"location":"active","threadId":event.event.aggregate_id}),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    #[tokio::test(start_paused = true)]
    async fn config_provider_pump_deduplicates_before_two_hundred_millisecond_debounce() {
        use std::sync::Arc;
        let initial = Arc::new(vec![
            json!({"instanceId":"fixture","displayName":"Initial"}),
        ]);
        let changed = Arc::new(vec![
            json!({"instanceId":"fixture","displayName":"Changed"}),
        ]);
        let last = Arc::new(vec![json!({"instanceId":"fixture","displayName":"Final"})]);
        let (input, mut inbox) =
            mpsc::unbounded_channel::<(Arc<Vec<Value>>, tokio::sync::oneshot::Sender<()>)>();
        let source = futures_util::stream::poll_fn(move |cx| {
            inbox.poll_recv(cx).map(|next| {
                next.map(|(value, admitted)| {
                    let _ = admitted.send(());
                    value
                })
            })
        });
        let (output, mut events) = mpsc::unbounded_channel();
        let task = tokio::spawn(pump_provider_statuses(
            source,
            serde_json::to_string(initial.as_ref()).unwrap(),
            output,
        ));
        struct Abort(tokio::task::AbortHandle);
        impl Drop for Abort {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _owned = Abort(task.abort_handle());
        async fn admit(
            input: &mpsc::UnboundedSender<(Arc<Vec<Value>>, tokio::sync::oneshot::Sender<()>)>,
            value: Arc<Vec<Value>>,
        ) {
            let (sent, read) = tokio::sync::oneshot::channel();
            input.send((value, sent)).unwrap();
            read.await.unwrap();
        }
        admit(&input, initial).await;
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        admit(&input, changed.clone()).await;
        tokio::time::advance(std::time::Duration::from_millis(100)).await;
        admit(&input, last.clone()).await;
        // Equal snapshots do not reset a pending debounce timer.
        tokio::time::advance(std::time::Duration::from_millis(100)).await;
        admit(&input, last.clone()).await;
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        tokio::time::advance(std::time::Duration::from_millis(99)).await;
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        assert_eq!(events.recv().await.unwrap().as_ref(), last.as_ref());
        admit(&input, last).await;
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        drop(input);
        task.await.unwrap();
        assert!(events.recv().await.is_none());
    }
    #[tokio::test]
    async fn config_socket_merges_typed_debounced_provider_statuses_with_settings_behind_ack() {
        use crate::{
            server_secret_store::ServerSecretStore,
            server_settings::{SettingsOptions, SettingsService},
        };
        use tokio_tungstenite::{
            MaybeTlsStream, WebSocketStream,
            tungstenite::{Message, client::IntoClientRequest},
        };
        type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
        async fn send(socket: &mut Socket, value: Value) {
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
        }
        async fn next(socket: &mut Socket) -> Value {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            serde_json::from_str(frame.to_text().unwrap()).unwrap()
        }
        fn event(frame: Value) -> Value {
            assert_eq!(frame["_tag"], "Chunk");
            let value = frame["values"][0].clone();
            serde_json::from_value::<t3_contracts::ServerConfigStreamEvent>(value.clone()).unwrap();
            value
        }
        let directory = tempfile::tempdir().unwrap();
        let mut initial = json!({"providers":{"codex":{"enabled":false},"claudeAgent":{"enabled":false},"cursor":{"enabled":false},"grok":{"enabled":false},"pi":{"enabled":false},"opencode":{"enabled":false},"antigravity":{"enabled":false}},"providerInstances":{"codex":{"driver":"codex","enabled":false,"displayName":"Initial"}}});
        let initial_settings = serde_json::from_value(initial.clone()).unwrap();
        let path = directory.path().join("settings.json");
        std::fs::write(&path, initial.to_string()).unwrap();
        let mut options = SettingsOptions::file(
            path,
            ServerSecretStore::open(directory.path().join("secrets")).unwrap(),
        );
        options.watch = false;
        let settings = SettingsService::start(options).await.unwrap();
        let mut api = state();
        let config = crate::config::NativeConfig::from_settings(
            initial_settings,
            directory.path(),
            directory.path(),
            &api.environment,
            &api.auth.descriptor(),
        )
        .await
        .unwrap();
        let registry = config.providers.clone();
        api.providers = Some(config.providers);
        api.config = Some(config.snapshot);
        api.settings = Some(settings.clone());
        let bearer = token(&api, vec![AuthEnvironmentScope::OrchestrationRead]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(api))
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        struct Abort(tokio::task::AbortHandle);
        impl Drop for Abort {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _owned = Abort(server.abort_handle());
        let mut request = format!("ws://{address}/ws?orchestrationProtocol=2")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
        let mut socket = tokio_tungstenite::connect_async(request).await.unwrap().0;
        send(&mut socket,json!({"_tag":"Request","id":1,"tag":"subscribeServerConfig","payload":{},"headers":[]})).await;
        assert_eq!(event(next(&mut socket).await)["type"], "snapshot");
        // Both sources change while the client holds the snapshot Ack. Provider
        // refreshes use the real shared registry; no external executable runs.
        initial["providerInstances"]["codex"]["displayName"] = json!("Intermediate");
        registry
            .reconfigure(
                &serde_json::from_value(initial.clone()).unwrap(),
                directory.path(),
            )
            .await
            .unwrap();
        initial["providerInstances"]["codex"]["displayName"] = json!("Final");
        registry
            .reconfigure(&serde_json::from_value(initial).unwrap(), directory.path())
            .await
            .unwrap();
        settings
            .update(serde_json::from_value(json!({"responseStreamingMode":"paragraph"})).unwrap())
            .await
            .unwrap();
        send(&mut socket, json!({"_tag":"Ack","requestId":1})).await;
        let mut saw_settings = false;
        let mut saw_providers = false;
        while !saw_settings || !saw_providers {
            let item = event(next(&mut socket).await);
            match item["type"].as_str().unwrap() {
                "settingsUpdated" => {
                    assert!(!saw_settings);
                    saw_settings = true;
                    assert_eq!(
                        item["payload"]["settings"]["responseStreamingMode"],
                        "paragraph"
                    );
                }
                "providerStatuses" => {
                    assert!(!saw_providers);
                    saw_providers = true;
                    let codex = item["payload"]["providers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|p| p["instanceId"] == "codex")
                        .unwrap();
                    assert_eq!(codex["displayName"], "Final");
                }
                tag => panic!("unexpected config event {tag}"),
            }
            send(&mut socket, json!({"_tag":"Ack","requestId":1})).await;
        }
        settings.shutdown().await;
        assert_eq!(next(&mut socket).await["exit"]["_tag"], "Success");
        socket.close(None).await.unwrap();
        stop.send(()).unwrap();
        server.await.unwrap();
    }
    #[tokio::test]
    async fn settings_socket_redacts_secrets_rejects_malformed_updates_and_survives_no_provider_stream()
     {
        use crate::server_secret_store::ServerSecretStore;
        use crate::server_settings::{SettingsOptions, SettingsService};
        use tokio_tungstenite::{
            MaybeTlsStream, WebSocketStream,
            tungstenite::{Message, client::IntoClientRequest},
        };
        type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
        async fn send(socket: &mut Socket, value: Value) {
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
        }
        async fn request(socket: &mut Socket, id: u64, tag: &str, payload: Value) {
            send(
                socket,
                json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}),
            )
            .await;
        }
        async fn next(socket: &mut Socket) -> Value {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            serde_json::from_str(frame.to_text().unwrap()).unwrap()
        }
        async fn connect(address: std::net::SocketAddr, bearer: &str) -> Socket {
            let mut request = format!("ws://{address}/ws?orchestrationProtocol=2")
                .into_client_request()
                .unwrap();
            request
                .headers_mut()
                .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
            tokio_tungstenite::connect_async(request).await.unwrap().0
        }
        fn config_event(frame: &Value) -> Value {
            assert_eq!(frame["_tag"], "Chunk");
            let value = frame["values"][0].clone();
            serde_json::from_value::<t3_contracts::ServerConfigStreamEvent>(value.clone()).unwrap();
            assert!(!value.to_string().contains("private-settings-token"));
            value
        }
        let directory = tempfile::tempdir().unwrap();
        let disabled:t3_contracts::ServerSettings=serde_json::from_value(json!({"providers":{"codex":{"enabled":false},"claudeAgent":{"enabled":false},"cursor":{"enabled":false},"grok":{"enabled":false},"pi":{"enabled":false},"opencode":{"enabled":false},"antigravity":{"enabled":false}}})).unwrap();
        let path = directory.path().join("settings.json");
        std::fs::write(&path, serde_json::to_vec(&disabled).unwrap()).unwrap();
        let mut options = SettingsOptions::file(
            path.clone(),
            ServerSecretStore::open(directory.path().join("secrets")).unwrap(),
        );
        options.watch = false;
        let settings = SettingsService::start(options).await.unwrap();
        let mut api = state();
        let config = crate::config::NativeConfig::from_settings(
            disabled,
            directory.path(),
            directory.path(),
            &api.environment,
            &api.auth.descriptor(),
        )
        .await
        .unwrap();
        api.config = Some(config.snapshot);
        api.settings = Some(settings.clone());
        assert!(api.providers.is_none()); // A closed merge source must not block settings.
        let readonly = token(&api, vec![AuthEnvironmentScope::OrchestrationRead]);
        let writable = token(
            &api,
            vec![
                AuthEnvironmentScope::OrchestrationRead,
                AuthEnvironmentScope::SettingsWrite,
            ],
        );
        let provider_grant = token(&api, vec![AuthEnvironmentScope::ProvidersManage]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(api))
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        struct AbortServer(tokio::task::AbortHandle);
        impl Drop for AbortServer {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _owned_server = AbortServer(server.abort_handle());
        let mut reader = connect(address, &readonly).await;
        let mut operator = connect(address, &writable).await;
        request(
            &mut reader,
            1,
            "server.updateSettings",
            json!({"patch":{"github":{"tokens":{"github.com":"private-settings-token"}}}}),
        )
        .await;
        let denied = next(&mut reader).await;
        assert_eq!(
            denied["exit"]["cause"][0]["error"]["requiredScope"],
            "orchestration:operate"
        );
        assert_eq!(
            denied["exit"]["cause"][0]["error"]["requiredPermission"],
            "settings:write"
        );
        let mut provider_operator = connect(address, &provider_grant).await;
        let mutation = json!({"operation":"remove","instanceId":"does-not-exist"});
        request(
            &mut operator,
            90,
            "server.updateSettings",
            json!({"patch":{},"providerInstanceMutation":mutation}),
        )
        .await;
        let denied = next(&mut operator).await;
        assert_eq!(
            denied["exit"]["cause"][0]["error"]["requiredPermission"],
            "providers:manage"
        );
        request(&mut provider_operator,91,"server.updateSettings",json!({"patch":{"responseStreamingMode":"paragraph"},"providerInstanceMutation":mutation})).await;
        let denied = next(&mut provider_operator).await;
        assert_eq!(
            denied["exit"]["cause"][0]["error"]["requiredPermission"],
            "settings:write"
        );
        request(
            &mut provider_operator,
            92,
            "server.updateSettings",
            json!({"patch":{},"providerInstanceMutation":mutation}),
        )
        .await;
        assert_eq!(
            next(&mut provider_operator).await["exit"]["_tag"],
            "Success"
        );
        provider_operator.close(None).await.unwrap();
        for (id, payload) in [
            (93, json!([])),
            (94, json!(false)),
            (95, json!(0)),
            (96, json!("")),
            (97, json!({"extra":"ignored"})),
        ] {
            request(&mut operator, id, "server.getSettings", payload).await;
            assert_eq!(next(&mut operator).await["exit"]["_tag"], "Success");
        }
        let before = std::fs::read(&path).unwrap();
        for (id, tag, payload) in [
            (2, "server.updateSettings", json!({"patch":[]})),
            (3, "server.getSettings", Value::Null),
            (4, "server.getConfig", Value::Null),
            (5, "subscribeServerConfig", json!([])),
        ] {
            request(&mut operator, id, tag, payload).await;
            let rejected = next(&mut operator).await;
            assert_eq!(rejected["requestId"], id);
            assert_eq!(rejected["exit"]["cause"][0]["_tag"], "Die");
            assert!(
                rejected["exit"]["cause"][0]["defect"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        request(
            &mut reader,
            6,
            "subscribeServerConfig",
            json!({"environmentThemes":null,"usageLimitSources":false}),
        )
        .await;
        let snapshot = config_event(&next(&mut reader).await);
        assert_eq!(snapshot["type"], "snapshot");
        send(&mut reader, json!({"_tag":"Ack","requestId":6})).await;
        request(&mut operator,7,"server.updateSettings",json!({"patch":{"github":{"tokens":{"github.com":"private-settings-token"}},"responseStreamingMode":"paragraph"}})).await;
        let changed = next(&mut operator).await;
        assert_eq!(changed["exit"]["_tag"], "Success");
        assert!(!changed.to_string().contains("private-settings-token"));
        let update = config_event(&next(&mut reader).await);
        assert_eq!(update["type"], "settingsUpdated");
        assert_eq!(
            update["payload"]["settings"]["responseStreamingMode"],
            "paragraph"
        );
        // A second update queues behind the held Ack; heartbeat/unary remain live.
        request(
            &mut operator,
            8,
            "server.updateSettings",
            json!({"patch":{"responseStreamingMode":"turn"}}),
        )
        .await;
        assert_eq!(next(&mut operator).await["exit"]["_tag"], "Success");
        send(&mut reader, json!({"_tag":"Ping"})).await;
        assert_eq!(next(&mut reader).await["_tag"], "Pong");
        send(&mut reader, json!({"_tag":"Ack","requestId":6})).await;
        let update = config_event(&next(&mut reader).await);
        assert_eq!(
            update["payload"]["settings"]["responseStreamingMode"],
            "turn"
        );
        send(&mut reader, json!({"_tag":"Ack","requestId":6})).await;
        // No more input or provider events: service shutdown must end the stream.
        settings.shutdown().await;
        let exit = next(&mut reader).await;
        assert_eq!(exit["requestId"], 6);
        assert_eq!(exit["exit"]["_tag"], "Success");
        send(&mut reader, json!({"_tag":"Ping"})).await;
        assert_eq!(next(&mut reader).await["_tag"], "Pong");
        reader.close(None).await.unwrap();
        operator.close(None).await.unwrap();
        stop.send(()).unwrap();
        server.await.unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_rpc_uses_real_pty_scopes_ack_streams_and_owned_close() {
        use std::os::unix::fs::PermissionsExt;
        use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
        type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
        async fn send(socket: &mut Socket, value: Value) {
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
        }
        async fn next(socket: &mut Socket) -> Value {
            serde_json::from_str(
                tokio::time::timeout(std::time::Duration::from_secs(3), socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .to_text()
                    .unwrap(),
            )
            .unwrap()
        }
        async fn request(socket: &mut Socket, id: u64, tag: &str, payload: Value) {
            send(
                socket,
                json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}),
            )
            .await;
        }
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("pty-fixture.py");
        std::fs::write(&script,"#!/usr/bin/env python3\nimport os,sys,tty\ntty.setraw(0)\nos.write(1,b'READY\\n')\nfor line in sys.stdin: os.write(1,('NATIVE:'+line).encode())\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut options = crate::terminal_manager::TerminalManagerOptions::host(
            directory.path().join("logs"),
            &t3_contracts::ServerSettings::default(),
        );
        options.shell = Some(script.to_string_lossy().into_owned());
        options.kill_grace = std::time::Duration::ZERO;
        let terminals = crate::terminal_manager::TerminalManager::new(options)
            .await
            .unwrap();
        let mut api = state();
        api.terminals = Some(terminals.clone());
        let read = token(&api, vec![AuthEnvironmentScope::TerminalRead]);
        let operate = token(
            &api,
            vec![
                AuthEnvironmentScope::TerminalRead,
                AuthEnvironmentScope::TerminalOperate,
            ],
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(api))
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            let mut request = format!("ws://{address}/ws?orchestrationProtocol=2")
                .into_client_request()
                .unwrap();
            request
                .headers_mut()
                .insert("Authorization", format!("Bearer {token}").parse().unwrap());
            tokio_tungstenite::connect_async(request).await.unwrap().0
        }
        let mut reader = connect(address, &read).await;
        let mut operator = connect(address, &operate).await;
        let session = json!({"threadId":"socket-thread","terminalId":"term-1"});
        let mut opening = session.clone();
        opening["cwd"] = json!(directory.path());
        request(&mut reader, 1, "terminal.open", opening.clone()).await;
        let denied = next(&mut reader).await;
        assert_eq!(denied["exit"]["_tag"], "Failure");
        assert_eq!(
            denied["exit"]["cause"][0]["error"]["requiredScope"],
            "terminal:operate"
        );
        request(&mut operator, 2, "terminal.open", opening).await;
        let opened = next(&mut operator).await;
        assert_eq!(opened["exit"]["_tag"], "Success");
        let pid = opened["exit"]["value"]["pid"].as_u64().unwrap();
        let _: t3_contracts::TerminalSessionSnapshot =
            serde_json::from_value(opened["exit"]["value"].clone()).unwrap();
        request(&mut reader, 3, "terminal.observe", session.clone()).await;
        let snapshot = next(&mut reader).await;
        assert_eq!(snapshot["_tag"], "Chunk");
        assert_eq!(snapshot["values"][0]["type"], "snapshot");
        let _: t3_contracts::TerminalAttachStreamEvent =
            serde_json::from_value(snapshot["values"][0].clone()).unwrap();
        // Holding the snapshot Ack must not block heartbeat or another unary.
        send(&mut reader, json!({"_tag":"Ping"})).await;
        assert_eq!(next(&mut reader).await["_tag"], "Pong");
        send(&mut reader, json!({"_tag":"Ack","requestId":3})).await;
        let mut ready = snapshot["values"][0]["snapshot"]["history"]
            .as_str()
            .unwrap()
            .to_owned();
        while !ready.contains("READY\n") {
            let chunk = next(&mut reader).await;
            assert_eq!(chunk["_tag"], "Chunk");
            ready.push_str(chunk["values"][0]["data"].as_str().unwrap());
            send(&mut reader, json!({"_tag":"Ack","requestId":3})).await;
        }
        let mut write = session.clone();
        write["data"] = json!("fixture-前😀\n");
        request(&mut operator, 4, "terminal.write", write.clone()).await;
        assert_eq!(next(&mut operator).await["exit"]["_tag"], "Success");
        let mut native_output = String::new();
        loop {
            let chunk = next(&mut reader).await;
            assert_eq!(chunk["_tag"], "Chunk");
            let item = &chunk["values"][0];
            let _: t3_contracts::TerminalAttachStreamEvent =
                serde_json::from_value(item.clone()).unwrap();
            if item["type"] == "output" {
                native_output.push_str(item["data"].as_str().unwrap());
            }
            let done = native_output.contains("NATIVE:fixture-前😀\n");
            send(&mut reader, json!({"_tag":"Ack","requestId":3})).await;
            if done {
                break;
            }
        }
        request(&mut reader, 5, "terminal.write", write).await;
        assert_eq!(next(&mut reader).await["exit"]["_tag"], "Failure");
        send(&mut reader, json!({"_tag":"Interrupt","requestId":3})).await;
        assert_eq!(
            next(&mut reader).await["exit"]["cause"][0]["_tag"],
            "Interrupt"
        );
        // Removing a read subscription leaves the owned terminal running.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        request(&mut operator, 6, "terminal.close", session.clone()).await;
        assert_eq!(next(&mut operator).await["exit"]["_tag"], "Success");
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        request(&mut reader, 7, "terminal.observe", session).await;
        let missing = next(&mut reader).await;
        assert_eq!(
            missing["exit"]["cause"][0]["error"]["_tag"],
            "TerminalSessionLookupError"
        );
        reader.close(None).await.unwrap();
        operator.close(None).await.unwrap();
        terminals.shutdown().await;
        stop.send(()).unwrap();
        server.await.unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn resource_telemetry_socket_scopes_history_retry_ack_and_owned_shutdown() {
        use crate::{
            desktop_telemetry::{DesktopTelemetryOptions, DesktopTelemetryReceiver},
            native_telemetry::{NativeTelemetryClient, NativeTelemetryOptions},
            resource_attribution::ResourceAttribution,
            resource_telemetry_service::ResourceTelemetry,
        };
        use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("monitor.py");
        std::fs::write(
            &script,
            include_str!("../tests/fixtures/resource-monitor.py")
                .replace("__MODE__", "normal")
                .replace("__LOG__", ""),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut options = NativeTelemetryOptions::host(directory.path().to_owned(), None);
        options.binary.overrides = vec![script];
        options.binary.directories.clear();
        let native = NativeTelemetryClient::new(options);
        let mut health = native.subscribe_health();
        tokio::time::timeout(
            Duration::from_secs(5),
            health.wait_for(|health| {
                health.status == t3_contracts::ResourceTelemetrySourceStatus::Healthy
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let desktop =
            DesktopTelemetryReceiver::new(DesktopTelemetryOptions::unavailable("web")).await;
        let telemetry = ResourceTelemetry::new(
            native.clone(),
            desktop.clone(),
            ResourceAttribution::default(),
            std::process::id() as u64,
            Arc::new(|| 5000),
        )
        .await;
        let mut api = state();
        api.resource_telemetry = Some(telemetry.clone());
        let host = crate::host_resources::HostResources::new(
            crate::host_resources::HostResourcesOptions::host(Arc::new(|| 5000)),
        );
        api.host_resources = Some(host.clone());
        let diagnostic = token(
            &api,
            vec![
                AuthEnvironmentScope::DiagnosticsRead,
                AuthEnvironmentScope::EnvironmentMaintain,
            ],
        );
        let readonly = token(&api, vec![AuthEnvironmentScope::OrchestrationRead]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(api))
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let mut request = format!("ws://{address}/ws?orchestrationProtocol=2")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {diagnostic}").parse().unwrap(),
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        async fn next(
            socket: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        ) -> Value {
            let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            serde_json::from_str(message.to_text().unwrap()).unwrap()
        }
        socket.send(Message::Text(json!({"_tag":"Request","id":1,"tag":"subscribeResourceTelemetry","payload":{},"headers":[]}).to_string().into())).await.unwrap();
        let initial = next(&mut socket).await;
        assert_eq!(initial["_tag"], "Chunk");
        let _: t3_contracts::ResourceTelemetrySnapshot =
            serde_json::from_value(initial["values"][0].clone()).unwrap();
        // Unary work remains usable on the same socket while its stream awaits Ack.
        for (id, tag, payload) in [
            (
                2,
                "server.getResourceTelemetryHistory",
                json!({"windowMs":5000,"bucketMs":1000}),
            ),
            (3, "server.retryResourceTelemetry", json!({})),
            (5, "server.getProcessDiagnostics", json!({})),
            (
                6,
                "server.getProcessResourceHistory",
                json!({"windowMs":5000,"bucketMs":1000}),
            ),
            (
                7,
                "server.signalProcess",
                json!({"pid":std::process::id(),"startTimeMs":0,"signal":"SIGKILL"}),
            ),
        ] {
            socket
                .send(Message::Text(
                    json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let reply = next(&mut socket).await;
            assert_eq!(reply["requestId"], id);
            assert_eq!(reply["exit"]["_tag"], "Success");
            if id == 2 {
                let _: t3_contracts::ResourceTelemetryHistory =
                    serde_json::from_value(reply["exit"]["value"].clone()).unwrap();
            } else if id == 3 {
                let _: t3_contracts::ResourceTelemetryRetryResult =
                    serde_json::from_value(reply["exit"]["value"].clone()).unwrap();
            } else if id == 5 {
                let _: t3_contracts::ServerProcessDiagnosticsResult =
                    serde_json::from_value(reply["exit"]["value"].clone()).unwrap();
            } else if id == 6 {
                let _: t3_contracts::ServerProcessResourceHistoryResult =
                    serde_json::from_value(reply["exit"]["value"].clone()).unwrap();
            } else {
                let value: t3_contracts::ServerSignalProcessResult =
                    serde_json::from_value(reply["exit"]["value"].clone()).unwrap();
                assert!(!value.signaled);
            }
        }
        let mut request = format!("ws://{address}/ws?orchestrationProtocol=2")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {readonly}").parse().unwrap(),
        );
        let (mut reader, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        reader.send(Message::Text(json!({"_tag":"Request","id":4,"tag":"subscribeResourceTelemetry","payload":{},"headers":[]}).to_string().into())).await.unwrap();
        let refused = next(&mut reader).await;
        assert_eq!(refused["exit"]["_tag"], "Failure");
        assert!(refused.to_string().contains("diagnostics:read"));
        for (id, tag, payload) in [
            (8, "server.getHostResources", json!({})),
            (9, "server.getHostResources", json!(1)),
        ] {
            reader
                .send(Message::Text(
                    json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let response = next(&mut reader).await;
            assert_eq!(response["exit"]["_tag"], "Success");
            let _: t3_contracts::HostResourcesSnapshot =
                serde_json::from_value(response["exit"]["value"].clone()).unwrap();
        }
        reader.send(Message::Text(json!({"_tag":"Request","id":10,"tag":"server.signalProcess","payload":{"pid":std::process::id(),"startTimeMs":0,"signal":"SIGKILL"},"headers":[]}).to_string().into())).await.unwrap();
        let denied = next(&mut reader).await;
        assert_eq!(denied["exit"]["_tag"], "Failure");
        assert!(denied.to_string().contains("environment:maintain"));
        tokio::join!(telemetry.shutdown(), desktop.shutdown(), host.shutdown());
        let exit = next(&mut socket).await;
        assert_eq!(exit["requestId"], 1);
        assert_eq!(exit["_tag"], "Exit");
        socket.close(None).await.unwrap();
        reader.close(None).await.unwrap();
        stop.send(()).unwrap();
        server.await.unwrap();
        native.shutdown().await;
    }
    #[tokio::test]
    async fn discovery_socket_preserves_queued_scan_time_and_exits_on_service_shutdown() {
        use crate::{
            resource_discovery::{PortDiscovery, PortDiscoveryOptions},
            resource_ports::{LocalServer, TerminalRegistry},
        };
        use futures_util::FutureExt;
        use std::sync::{Arc, Mutex};
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let clock = Arc::new(std::sync::atomic::AtomicI64::new(1000));
        let rows = Arc::new(Mutex::new(vec![LocalServer {
            host: "localhost".into(),
            port: 5123,
            url: "http://localhost:5123".into(),
            pid: None,
            process_name: None,
            terminal: None,
        }]));
        let mut options = PortDiscoveryOptions::custom(
            TerminalRegistry::default(),
            Arc::new({
                let rows = rows.clone();
                move || {
                    let result = rows.lock().unwrap().clone();
                    async move { Ok(result) }.boxed()
                }
            }),
            Arc::new(|_| async { true }.boxed()),
            Arc::new({
                let clock = clock.clone();
                move || clock.load(std::sync::atomic::Ordering::SeqCst)
            }),
        );
        options.poll_interval = std::time::Duration::from_millis(10);
        let discovery = PortDiscovery::new(options);
        let mut api = state();
        api.discovery = Some(discovery.clone());
        let bearer = token(&api, vec![AuthEnvironmentScope::OrchestrationRead]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(api))
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let mut request = format!("ws://{address}/ws?orchestrationProtocol=2")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket.send(Message::Text(json!({"_tag":"Request","id":1,"tag":"subscribeDiscoveredLocalServers","payload":{"configuredUrls":null},"headers":[]}).to_string().into())).await.unwrap();
        async fn next(
            socket: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        ) -> Value {
            let item = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            serde_json::from_str(item.to_text().unwrap()).unwrap()
        }
        let initial = next(&mut socket).await;
        assert_eq!(initial["_tag"], "Chunk");
        let _: t3_contracts::DiscoveredLocalServerList =
            serde_json::from_value(initial["values"][0].clone()).unwrap();
        assert_eq!(initial["values"][0]["servers"][0]["port"], 5123);
        let published = Arc::new(tokio::sync::Notify::new());
        let _listener = discovery
            .subscribe_callback(
                &[],
                rows.lock().unwrap().clone(),
                Arc::new({
                    let published = published.clone();
                    move |_| {
                        let published = published.clone();
                        async move {
                            published.notify_one();
                        }
                        .boxed()
                    }
                }),
            )
            .unwrap();
        rows.lock().unwrap()[0].process_name = Some("changed-before-ack".into());
        tokio::time::timeout(std::time::Duration::from_secs(5), published.notified())
            .await
            .unwrap();
        clock.store(2000, std::sync::atomic::Ordering::SeqCst);
        socket
            .send(Message::Text(
                json!({"_tag":"Ack","requestId":1}).to_string().into(),
            ))
            .await
            .unwrap();
        let changed = next(&mut socket).await;
        assert_eq!(
            changed["values"][0]["servers"][0]["processName"],
            "changed-before-ack"
        );
        let scanned = chrono::DateTime::parse_from_rfc3339(
            changed["values"][0]["scannedAt"].as_str().unwrap(),
        )
        .unwrap()
        .timestamp_millis();
        assert_eq!(
            scanned, 1000,
            "queued notification was restamped during delivery"
        );
        // Leave the changed chunk unacknowledged: shutdown must unblock Ack too.
        discovery.shutdown().await;
        let exit = next(&mut socket).await;
        assert_eq!(exit["_tag"], "Exit");
        assert_eq!(exit["exit"]["_tag"], "Success");
        socket
            .send(Message::Text(json!({"_tag":"Ping"}).to_string().into()))
            .await
            .unwrap();
        assert_eq!(next(&mut socket).await["_tag"], "Pong");
        socket.close(None).await.unwrap();
        stop.send(()).unwrap();
        server.await.unwrap();
    }
    fn state() -> ApiState {
        let store = Store::memory().unwrap();
        let auth = AuthService::new(
            store.clone(),
            [9; 32],
            "t3_session_test".into(),
            "loopback-browser".into(),
        )
        .unwrap();
        ApiState {
            store,
            auth,
            environment: json!({"environmentId":"test-environment","label":"Tests","platform":{"os":"linux","arch":"x64"},"serverVersion":"rust-test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
            config: None,
            settings: None,
            cors_origins: None,
            assets: None,
            providers: None,
            execution: None,
            workspace: None,
            terminals: None,
            discovery: None,
            resource_telemetry: None,
            host_resources: None,
            background: None,
            device_hosts: None,
            devices: None,
            provider_auth: None,
        }
    }
    fn token(state: &ApiState, scopes: Vec<AuthEnvironmentScope>) -> String {
        state
            .auth
            .issue_session(
                "client",
                "bearer-access-token",
                scopes,
                json!({"deviceType":"unknown"}),
                Utc::now(),
                chrono::Duration::hours(1),
            )
            .unwrap()
            .1
    }
    async fn json_body(response: Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn pairing_form_issues_native_bearer_and_invalid_grants_do_not_consume_it() {
        let state = state();
        let credential = state
            .auth
            .create_pairing_credential(
                &[AuthEnvironmentScope::OrchestrationRead],
                Utc::now(),
                chrono::Duration::minutes(5),
            )
            .unwrap();
        let app = router(state.clone());
        let request = |scope: &str| {
            Request::builder().method("POST").uri("/oauth/token").header(header::CONTENT_TYPE,"application/x-www-form-urlencoded").body(Body::from(format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange&subject_token={credential}&subject_token_type=urn%3At3%3Aparams%3Aoauth%3Atoken-type%3Aenvironment-bootstrap&requested_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aaccess_token&scope={scope}&client_device_type=desktop&client_label=Native+Client"))).unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(request("orchestration%3Aoperate"))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        for scope in [
            "unknown%3Afuture",
            "orchestration%3Aread++unknown%3Afuture",
            "orchestration%3Aread%09unknown%3Afuture",
            "orchestration%3Aread+invalid%5Cscope",
        ] {
            let response = app.clone().oneshot(request(scope)).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(json_body(response).await["reason"], "invalid_scope");
        }
        let response = app
            .clone()
            .oneshot(request(
                "orchestration%3Aread+access%3Awrite+unknown%3Afuture+orchestration%3Aread+review%3Awrite",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        let issued = json_body(response).await;
        assert_eq!(issued["token_type"], "Bearer");
        assert_eq!(issued["scope"], "orchestration:read");
        let session = state
            .auth
            .verify_session(issued["access_token"].as_str().unwrap(), Utc::now())
            .unwrap();
        assert_eq!(session.method, "bearer-access-token");
        assert_eq!(session.client["deviceType"], "desktop");
        assert_eq!(session.client["label"], "Native Client");
        assert_eq!(
            app.oneshot(request("orchestration%3Aread"))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn web_assets_share_api_origin_and_reject_symlinks_outside_asset_root() {
        let directory = tempfile::tempdir().unwrap();
        let assets = directory.path().join("web");
        std::fs::create_dir(&assets).unwrap();
        std::fs::write(assets.join("index.html"), "<html>Rust UI</html>").unwrap();
        std::fs::write(assets.join("app.wasm"), [0, 97, 115, 109]).unwrap();
        std::fs::write(directory.path().join("secret"), "private").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(directory.path().join("secret"), assets.join("escape.txt"))
            .unwrap();
        let mut state = state();
        state.assets = Some(assets);
        let app = router(state);
        for path in ["/", "/thread/example"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/html; charset=utf-8"
            );
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app.wasm")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/wasm");
        for path in ["/api/unknown", "/missing.js", "/../secret", "/escape.txt"] {
            assert_eq!(
                app.clone()
                    .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn metadata_auth_ticket_and_snapshot_http_endpoints_preserve_grants() {
        let state = state();
        let access = token(&state, vec![AuthEnvironmentScope::OrchestrationRead]);
        let app = router(state.clone());
        let descriptor = json_body(
            app.clone()
                .oneshot(
                    Request::builder()
                        .uri("/.well-known/t3/environment")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(descriptor["environmentId"], "test-environment");
        assert_eq!(descriptor["orchestrationProtocolVersion"], 2);
        let request =
            || Request::builder().header(header::AUTHORIZATION, format!("Bearer {access}"));
        let session = json_body(
            app.clone()
                .oneshot(
                    request()
                        .uri("/api/auth/session")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(session["permissions"], json!(["orchestration:read"]));
        let response = app
            .clone()
            .oneshot(
                request()
                    .method("POST")
                    .uri("/api/auth/websocket-ticket")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let ticket = json_body(response).await;
        assert!(
            state
                .auth
                .verify_websocket_ticket(ticket["ticket"].as_str().unwrap(), Utc::now())
                .is_ok()
        );
        let snapshot = json_body(
            app.oneshot(
                request()
                    .uri("/api/orchestration/shell")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(snapshot["snapshotSequence"], 0);
        assert_eq!(snapshot["projects"], json!([]));
    }

    #[tokio::test]
    async fn persisted_history_http_routes_enforce_grants_and_map_cursor_and_missing_thread_errors()
    {
        let state = state();
        let directory = tempfile::tempdir().unwrap();
        ProjectService::new(state.store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":directory.path()}),Utc::now()).unwrap();
        crate::launch::ThreadLaunchService::new(state.store.clone()).launch(json!({"commandId":"launch","threadId":"thread","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}),Utc::now()).unwrap();
        let access = token(&state, vec![AuthEnvironmentScope::OrchestrationRead]);
        let unrelated = token(&state, vec![AuthEnvironmentScope::TerminalRead]);
        let app = router(state.clone());
        let cursor = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            r#"{"v":1,"seq":0,"st":"thread","si":"none","p":0}"#,
        );
        for (uri, token, status) in [
            (
                "/api/orchestration/threads/thread/bounded".to_owned(),
                None,
                StatusCode::UNAUTHORIZED,
            ),
            (
                "/api/orchestration/threads/thread/bounded".to_owned(),
                Some(&unrelated),
                StatusCode::FORBIDDEN,
            ),
            (
                "/api/orchestration/threads/thread/history?cursor=invalid".to_owned(),
                Some(&access),
                StatusCode::BAD_REQUEST,
            ),
            (
                format!("/api/orchestration/threads/missing/history?cursor={cursor}"),
                Some(&access),
                StatusCode::NOT_FOUND,
            ),
        ] {
            let mut request = Request::builder()
                .uri(uri)
                .header("x-t3-orchestration-protocol", "2");
            if let Some(token) = token {
                request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
        let request = |uri: String| {
            Request::builder()
                .uri(uri)
                .header("x-t3-orchestration-protocol", "2")
                .header(header::AUTHORIZATION, format!("Bearer {access}"))
                .body(Body::empty())
                .unwrap()
        };
        let bounded = json_body(
            app.clone()
                .oneshot(request("/api/orchestration/threads/thread/bounded".into()))
                .await
                .unwrap(),
        )
        .await;
        serde_json::from_value::<t3_contracts::ThreadBoundedSnapshot>(bounded.clone()).unwrap();
        assert_eq!(bounded["hasMoreHistory"], false);
        assert_eq!(bounded["historyCursor"], Value::Null);
        let page = json_body(
            app.oneshot(request(format!(
                "/api/orchestration/threads/thread/history?cursor={cursor}"
            )))
            .await
            .unwrap(),
        )
        .await;
        serde_json::from_value::<t3_contracts::ThreadHistoryPage>(page.clone()).unwrap();
        assert_eq!(page["items"], json!([]));
        let result = unary(
            &state,
            &RpcRequest {
                id: RpcRequestId::String("detail".into()),
                tag: "orchestration.getTurnItem".into(),
                payload: json!({"threadId":"thread","itemId":"missing","revision":"ignored"}),
                headers: vec![],
                trace_id: None,
                span_id: None,
                sampled: None,
                is_notification: None,
            },
        );
        assert_eq!(result.unwrap()["item"], Value::Null);
    }

    #[tokio::test]
    async fn cookie_precedes_bearer_and_bearer_tokens_are_trimmed() {
        let state = state();
        let read = token(&state, vec![AuthEnvironmentScope::OrchestrationRead]);
        let operate = token(&state, vec![AuthEnvironmentScope::OrchestrationOperate]);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer  {operate} ").parse().unwrap(),
        );
        assert!(
            authenticate(&state, &headers)
                .unwrap()
                .scopes
                .contains(&AuthEnvironmentScope::OrchestrationOperate)
        );
        headers.insert(
            header::COOKIE,
            format!("{}={read}", state.auth.cookie_name)
                .parse()
                .unwrap(),
        );
        assert!(
            !authenticate(&state, &headers)
                .unwrap()
                .scopes
                .contains(&AuthEnvironmentScope::OrchestrationOperate)
        );
        headers.insert(
            header::COOKIE,
            format!("{}=invalid", state.auth.cookie_name)
                .parse()
                .unwrap(),
        );
        assert!(authenticate(&state, &headers).is_err());
    }

    #[tokio::test]
    async fn remote_cors_supports_bearer_and_dev_credentialed_origins() {
        let state = state();
        let app = router(state.clone());
        let preflight = || {
            Request::builder()
                .method("OPTIONS")
                .uri("/api/auth/websocket-ticket")
                .header(header::ORIGIN, "https://app.t3.codes")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization,x-t3-orchestration-protocol",
                )
                .body(Body::empty())
                .unwrap()
        };
        let response = app.oneshot(preflight()).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
        );
        let mut dev = state;
        dev.cors_origins = Some(vec!["https://app.t3.codes".into(), "t3code://app".into()]);
        let response = router(dev).oneshot(preflight()).await.unwrap();
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://app.t3.codes"
        );
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_CREDENTIALS],
            "true"
        );
        assert!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
                .to_str()
                .unwrap()
                .contains("dpop")
        );
    }

    #[test]
    fn readonly_scope_cannot_mutate_and_invalid_ids_never_persist_receipts() {
        let state = state();
        let session = state
            .auth
            .issue_session(
                "client",
                "bearer-access-token",
                vec![AuthEnvironmentScope::OrchestrationRead],
                json!({}),
                Utc::now(),
                chrono::Duration::hours(1),
            )
            .unwrap()
            .0;
        assert!(authorize_rpc(&session, "orchestration.dispatchCommand").is_err());
        let invalid = json!({"type":"thread.archive","commandId":"","threadId":"thread"});
        assert!(
            ThreadService::new(state.store.clone())
                .dispatch(&invalid, Utc::now())
                .is_err()
        );
        assert!(state.store.receipt("").unwrap().is_none());
        assert_eq!(state.store.latest_sequence().unwrap(), 0);
    }

    #[tokio::test]
    async fn workspace_rpc_reads_searches_and_refreshes_writes_over_authorized_effect_socket() {
        use tokio_tungstenite::tungstenite::Message;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("before.txt"), "before").unwrap();
        let mut state = state();
        state.workspace = Some(crate::workspace_entries::WorkspaceEntries::new(
            root.path().to_owned(),
            root.path().to_owned(),
        ));
        let workspace = state.workspace.as_ref().unwrap().clone();
        let read_token = token(&state, vec![AuthEnvironmentScope::FilesystemRead]);
        let write_token = token(
            &state,
            vec![
                AuthEnvironmentScope::FilesystemRead,
                AuthEnvironmentScope::FilesystemWrite,
            ],
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, shutdown) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state))
                .with_graceful_shutdown(async {
                    let _ = shutdown.await;
                })
                .await
                .unwrap();
        });
        let mut read_request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                format!("ws://{address}/ws?orchestrationProtocol=2"),
            )
            .unwrap();
        read_request.headers_mut().insert(
            "Authorization",
            format!("Bearer {read_token}").parse().unwrap(),
        );
        let (mut reader, _) = tokio_tungstenite::connect_async(read_request)
            .await
            .unwrap();
        async fn call(
            socket: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            id: u64,
            method: &str,
            payload: Value,
        ) -> Value {
            socket
                .send(Message::Text(
                    json!({"_tag":"Request","id":id,"tag":method,"payload":payload,"headers":[]})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let response = tokio::time::timeout(std::time::Duration::from_secs(20), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let response: Value = serde_json::from_str(response.to_text().unwrap()).unwrap();
            assert_eq!(response["requestId"], id);
            response["exit"].clone()
        }
        let cwd = root.path().to_string_lossy();
        let result = call(&mut reader, 1, "projects.listEntries", json!({"cwd":cwd})).await;
        assert_eq!(result["_tag"], "Success");
        assert_eq!(result["value"]["entries"][0]["path"], "before.txt");
        let result=call(&mut reader,2,"projects.searchContents",json!({"cwd":cwd,"query":"after","limit":10,"caseSensitive":true,"wholeWord":false,"useRegex":false})).await;
        assert_eq!(result["value"]["matches"], json!([]));
        let result = call(
            &mut reader,
            3,
            "projects.writeFile",
            json!({"cwd":cwd,"relativePath":"new.txt","contents":"after\n"}),
        )
        .await;
        assert_eq!(result["_tag"], "Failure");
        assert!(!root.path().join("new.txt").exists());
        let mut write_request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                format!("ws://{address}/ws?orchestrationProtocol=2"),
            )
            .unwrap();
        write_request.headers_mut().insert(
            "Authorization",
            format!("Bearer {write_token}").parse().unwrap(),
        );
        let (mut writer, _) = tokio_tungstenite::connect_async(write_request)
            .await
            .unwrap();
        let result = call(
            &mut writer,
            4,
            "projects.writeFile",
            json!({"cwd":cwd,"relativePath":"new.txt","contents":"after\n"}),
        )
        .await;
        assert_eq!(result["_tag"], "Success");
        let result = call(
            &mut reader,
            5,
            "projects.searchEntries",
            json!({"cwd":cwd,"query":"new","limit":10}),
        )
        .await;
        assert_eq!(result["value"]["entries"][0]["path"], "new.txt");
        let result=call(&mut reader,6,"projects.searchContents",json!({"cwd":cwd,"query":"after","limit":10,"caseSensitive":true,"wholeWord":false,"useRegex":false})).await;
        assert_eq!(result["value"]["matches"][0]["path"], "new.txt");
        let result = call(
            &mut reader,
            7,
            "projects.readFile",
            json!({"cwd":cwd,"relativePath":"new.txt"}),
        )
        .await;
        assert_eq!(result["value"]["contents"], "after\n");
        let result = call(
            &mut reader,
            8,
            "filesystem.browse",
            json!({"partialPath":"./","cwd":cwd}),
        )
        .await;
        assert_eq!(result["_tag"], "Success");
        // A deterministic native-index barrier proves the reader keeps handling
        // Ping, another RPC, Interrupt and ID reuse while the index job is blocked.
        let (entered, release) = workspace.block_next_index();
        reader.send(Message::Text(json!({"_tag":"Request","id":9,"tag":"projects.searchEntries","payload":{"cwd":cwd,"query":"new","limit":10},"headers":[]}).to_string().into())).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), entered)
            .await
            .unwrap()
            .unwrap();
        reader
            .send(Message::Text(json!({"_tag":"Ping"}).to_string().into()))
            .await
            .unwrap();
        let pong = tokio::time::timeout(std::time::Duration::from_secs(3), reader.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(pong.to_text().unwrap()).unwrap()["_tag"],
            "Pong"
        );
        let independent = call(
            &mut reader,
            10,
            "filesystem.browse",
            json!({"partialPath":"./","cwd":cwd}),
        )
        .await;
        assert_eq!(independent["_tag"], "Success");
        reader
            .send(Message::Text(
                json!({"_tag":"Interrupt","requestId":9}).to_string().into(),
            ))
            .await
            .unwrap();
        let interrupted = tokio::time::timeout(std::time::Duration::from_secs(3), reader.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let interrupted: Value = serde_json::from_str(interrupted.to_text().unwrap()).unwrap();
        assert_eq!(interrupted["requestId"], 9);
        assert_eq!(interrupted["exit"]["cause"][0]["_tag"], "Interrupt");
        let reused = call(
            &mut reader,
            9,
            "filesystem.browse",
            json!({"partialPath":"./","cwd":cwd}),
        )
        .await;
        assert_eq!(reused["_tag"], "Success");
        release.send(()).unwrap();
        reader.close(None).await.unwrap();
        writer.close(None).await.unwrap();
        let _ = stop.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn effect_ws_stream_ack_ping_mutation_and_interrupt_are_compatible() {
        let state = state();
        let session = state
            .auth
            .issue_session(
                "client",
                "bearer-access-token",
                vec![
                    AuthEnvironmentScope::OrchestrationRead,
                    AuthEnvironmentScope::OrchestrationOperate,
                ],
                json!({}),
                Utc::now(),
                chrono::Duration::hours(1),
            )
            .unwrap()
            .0;
        let ticket = state
            .auth
            .issue_websocket_ticket(&session, Utc::now())
            .unwrap()["ticket"]
            .as_str()
            .unwrap()
            .to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, shutdown) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state))
                .with_graceful_shutdown(async {
                    let _ = shutdown.await;
                })
                .await
                .unwrap();
        });
        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "ws://{address}/ws?orchestrationProtocol=2&wsTicket={ticket}"
        ))
        .await
        .unwrap();
        async fn send(
            socket: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            value: Value,
        ) {
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    value.to_string().into(),
                ))
                .await
                .unwrap();
        }
        async fn receive(
            socket: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        ) -> Value {
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap()
        }
        send(&mut socket,json!({"_tag":"Request","id":1,"tag":"orchestration.subscribeShell","payload":{"requestCompletionMarker":true},"headers":[]})).await;
        let initial = receive(&mut socket).await;
        assert_eq!(initial["_tag"], "Chunk");
        assert_eq!(initial["values"][0]["kind"], "snapshot");
        send(&mut socket, json!({"_tag":"Ping"})).await;
        assert_eq!(receive(&mut socket).await["_tag"], "Pong");
        send(&mut socket, json!({"_tag":"Ack","requestId":1})).await;
        assert_eq!(
            receive(&mut socket).await["values"][0]["kind"],
            "synchronized"
        );
        send(&mut socket, json!({"_tag":"Ack","requestId":1})).await;
        send(&mut socket,json!({"_tag":"Request","id":2,"tag":"projects.mutate","payload":{"type":"project.create","commandId":"project-command","projectId":"project","title":"Project","workspaceRoot":"/tmp/project"},"headers":[]})).await;
        let a = receive(&mut socket).await;
        let b = receive(&mut socket).await;
        let (exit, update) = if a["_tag"] == "Exit" { (a, b) } else { (b, a) };
        assert_eq!(exit["exit"]["_tag"], "Success");
        assert_eq!(exit["exit"]["value"]["id"], "project");
        assert_eq!(update["values"][0]["kind"], "project.updated");
        send(&mut socket, json!({"_tag":"Interrupt","requestId":1})).await;
        assert_eq!(
            receive(&mut socket).await["exit"]["cause"][0]["_tag"],
            "Interrupt"
        );
        send(
            &mut socket,
            Value::Array((0..80).map(|_| json!({"_tag":"Ping"})).collect()),
        )
        .await;
        for _ in 0..80 {
            assert_eq!(receive(&mut socket).await["_tag"], "Pong");
        }
        socket.close(None).await.unwrap();
        let _ = stop.send(());
        server.await.unwrap();
    }
}
