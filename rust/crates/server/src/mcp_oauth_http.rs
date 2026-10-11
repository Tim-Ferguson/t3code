//! The OAuth routes of the `mcpOAuth` group (`apps/server/src/auth/mcpOAuthHttp.ts`): discovery metadata, client registration, the
//! authorize redirect, the approval page's two calls, and the token endpoint. Each decodes its input, calls one method of
//! [`McpOAuth`], and maps the failure the way the original's HTTP API renders it. A server that installed no authorization server
//! answers `404`.
use crate::{
    mcp_oauth::{self, Approval, AuthorizationFailure, AuthorizationRequest, McpOAuth, McpOAuthUrls},
    transport::{ApiState, Peer, named_cookie, user_agent},
};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use t3_contracts::{AuthMcpApprovalDecision, AuthMcpApprovalDecisionRequest, AuthMcpAuthorizationRequest, AuthMcpClientAccess, AuthMcpClientRegistration, AuthMcpTokenRequest};

/// Where the approval page lives in the web app.
const APPROVAL_PAGE_PATH: &str = "/connect-agent";

pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(protected_resource))
        .route("/.well-known/oauth-protected-resource/mcp", get(protected_resource))
        .route("/.well-known/oauth-authorization-server", get(authorization_server))
        .route("/oauth/mcp/register", post(register))
        .route("/oauth/mcp/authorize", get(authorize))
        .route("/oauth/mcp/approval", post(approval))
        .route("/oauth/mcp/decision", post(decision))
        .route("/oauth/mcp/token", post(token))
}

fn oauth(state: &ApiState) -> Result<Arc<McpOAuth>, Response> {
    mcp_oauth::current(&state.store).ok_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// `noStore`: OAuth responses carry codes, tokens and client ids, none of which may be cached.
fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

/// A body the schema of the route does not accept answers an empty 400, as the original's HTTP API renders it.
fn undecodable() -> Response {
    (StatusCode::BAD_REQUEST, axum::Json(Value::Null)).into_response()
}

fn decode_json<T: DeserializeOwned>(body: &Bytes) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|_| undecodable())
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `requestUrls`: the origin this request reached.
fn urls_of(headers: &HeaderMap, uri: &Uri) -> McpOAuthUrls {
    mcp_oauth::request_urls(header_text(headers, "host"), header_text(headers, "x-forwarded-proto"), uri.path_and_query().map_or("/", |target| target.as_str()))
}

async fn protected_resource(State(state): State<ApiState>, headers: HeaderMap, uri: Uri) -> Response {
    if let Err(response) = oauth(&state) {
        return response;
    }
    axum::Json(mcp_oauth::protected_resource_metadata(&urls_of(&headers, &uri))).into_response()
}

async fn authorization_server(State(state): State<ApiState>, headers: HeaderMap, uri: Uri) -> Response {
    if let Err(response) = oauth(&state) {
        return response;
    }
    axum::Json(mcp_oauth::authorization_server_metadata(&urls_of(&headers, &uri))).into_response()
}

async fn register(State(state): State<ApiState>, body: Bytes) -> Response {
    let run = || -> Result<Response, Response> {
        let oauth = oauth(&state)?;
        let metadata: AuthMcpClientRegistration = decode_json(&body)?;
        Ok(match oauth.register(&metadata) {
            Ok(client) => (
                StatusCode::CREATED,
                axum::Json(json!({
                    "client_id": client.client_id,
                    "client_name": client.name,
                    "redirect_uris": client.redirect_uris,
                    "grant_types": ["authorization_code"],
                    "response_types": ["code"],
                    "token_endpoint_auth_method": "none",
                })),
            )
                .into_response(),
            Err(error) => (StatusCode::BAD_REQUEST, axum::Json(error)).into_response(),
        })
    };
    no_store(run().unwrap_or_else(|response| response))
}

/// Redirects to `location` with the headers every redirect of this flow carries.
fn redirect(location: &str) -> Response {
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(HeaderName::from_static("referrer-policy"), HeaderValue::from_static("no-referrer"));
    response
}

/// The browser lands here from the agent. A valid request is handed to the web app's approval page with its query intact.
async fn authorize(State(state): State<ApiState>, headers: HeaderMap, uri: Uri) -> Response {
    let oauth = match oauth(&state) {
        Ok(oauth) => oauth,
        Err(response) => return response,
    };
    let urls = urls_of(&headers, &uri);
    let target = uri.path_and_query().map_or("/", |target| target.as_str());
    let url = url::Url::parse(&urls.issuer).and_then(|base| base.join(target));
    // `Object.fromEntries(url.searchParams)`: a name sent twice keeps its last value.
    let mut request = serde_json::Map::new();
    if let Ok(url) = &url {
        for (name, value) in url.query_pairs() {
            request.insert(name.into_owned(), Value::String(value.into_owned()));
        }
    }
    let request: AuthMcpAuthorizationRequest = serde_json::from_value(Value::Object(request)).unwrap_or(AuthMcpAuthorizationRequest { response_type: None, client_id: None, redirect_uri: None, code_challenge: None, code_challenge_method: None, state: None, resource: None });
    match oauth.validate_authorization(&urls, &request) {
        Ok(_) => {
            let search = url.ok().and_then(|url| url.query().filter(|query| !query.is_empty()).map(|query| format!("?{query}"))).unwrap_or_default();
            redirect(&format!("{APPROVAL_PAGE_PATH}{search}"))
        }
        Err(AuthorizationFailure::Page(error)) => {
            let mut response = (StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], mcp_oauth::render_error_page(&error.0)).into_response();
            let page_headers = response.headers_mut();
            page_headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'"));
            page_headers.insert(HeaderName::from_static("x-frame-options"), HeaderValue::from_static("DENY"));
            page_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            page_headers.insert(HeaderName::from_static("referrer-policy"), HeaderValue::from_static("no-referrer"));
            page_headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
            response
        }
        Err(AuthorizationFailure::Redirect(error)) => redirect(&mcp_oauth::redirect_for_error(&error, &urls.issuer)),
    }
}

/// What a forwarded request resolves to: validated, or a redirect the page must follow, or a message to show.
enum Forwarded {
    Valid(AuthorizationRequest),
    Redirect(String),
}

fn forwarded(oauth: &McpOAuth, urls: &McpOAuthUrls, request: &AuthMcpAuthorizationRequest) -> Result<Forwarded, Response> {
    match oauth.validate_authorization(urls, request) {
        Ok(valid) => Ok(Forwarded::Valid(valid)),
        Err(AuthorizationFailure::Page(error)) => Err(approval_error(&error.0)),
        Err(AuthorizationFailure::Redirect(error)) => Ok(Forwarded::Redirect(mcp_oauth::redirect_for_error(&error, &urls.issuer))),
    }
}

/// `AuthMcpApprovalError`: a problem the approval page shows the user without redirecting anywhere.
fn approval_error(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, axum::Json(json!({"_tag":"AuthMcpApprovalError","message":message}))).into_response()
}

fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|url| url.host_str().map(|host| format!("{host}{}", url.port().map_or(String::new(), |port| format!(":{port}"))))).unwrap_or_default()
}

/// What the approval page shows, and whether one click may approve.
async fn approval(State(state): State<ApiState>, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    let run = || -> Result<Response, Response> {
        let oauth = oauth(&state)?;
        let request: AuthMcpAuthorizationRequest = decode_json(&body)?;
        let urls = urls_of(&headers, &uri);
        let resolved = match forwarded(&oauth, &urls, &request)? {
            Forwarded::Redirect(to) => return Ok(axum::Json(json!({"redirectTo": to})).into_response()),
            Forwarded::Valid(resolved) => resolved,
        };
        let cookie = crate::transport::browser_session_cookie(&state, &headers);
        let session = oauth.approving_browser_session(cookie, &resolved);
        let mut details = json!({"clientName": resolved.client.name, "redirectHost": host_of(&resolved.redirect_uri), "environmentHost": host_of(&urls.issuer)});
        if let Some(session) = session {
            details["csrfToken"] = json!(session.csrf_token);
            details["oneClickAccess"] = json!(
                [AuthMcpClientAccess::ReadOnly, AuthMcpClientAccess::ApprovalRequired, AuthMcpClientAccess::AutoAcceptEdits, AuthMcpClientAccess::Auto, AuthMcpClientAccess::FullAccess]
                    .into_iter()
                    .filter(|access| crate::auth::mcp_client_scopes(*access).iter().all(|scope| session.scopes.contains(scope)))
                    .collect::<Vec<_>>()
            );
        }
        Ok(axum::Json(details).into_response())
    };
    no_store(run().unwrap_or_else(|response| response))
}

async fn decision(State(state): State<ApiState>, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    let run = || -> Result<Response, Response> {
        let oauth = oauth(&state)?;
        let request: AuthMcpApprovalDecisionRequest = decode_json(&body)?;
        let urls = urls_of(&headers, &uri);
        let resolved = match forwarded(&oauth, &urls, &request.authorization)? {
            Forwarded::Redirect(to) => return Ok(axum::Json(json!({"redirectTo": to})).into_response()),
            Forwarded::Valid(resolved) => resolved,
        };
        if matches!(request.decision, AuthMcpApprovalDecision::Deny) {
            return Ok(axum::Json(json!({"redirectTo": oauth.deny(&resolved)})).into_response());
        }
        let approval: Approval<'_> = mcp_oauth::approval_of(&request.decision).expect("a decision other than deny approves");
        let cookie = crate::transport::browser_session_cookie(&state, &headers);
        match oauth.approve(cookie, &resolved, approval) {
            Ok(to) => Ok(axum::Json(json!({"redirectTo": to})).into_response()),
            Err(message) => Err(approval_error(&message)),
        }
    };
    no_store(run().unwrap_or_else(|response| response))
}

/// `application/x-www-form-urlencoded` is the one encoding the token request has.
fn decode_form(headers: &HeaderMap, body: &Bytes) -> Result<AuthMcpTokenRequest, Response> {
    let form = header_text(headers, "content-type").and_then(|value| value.split(';').next()).map(str::trim);
    if !form.is_some_and(|media| media.eq_ignore_ascii_case("application/x-www-form-urlencoded")) {
        return Err(undecodable());
    }
    // `URLSearchParams` into a struct: a name sent twice keeps its last value.
    let mut fields = serde_json::Map::new();
    for (name, value) in url::form_urlencoded::parse(body) {
        fields.insert(name.into_owned(), Value::String(value.into_owned()));
    }
    serde_json::from_value(Value::Object(fields)).map_err(|_| undecodable())
}

async fn token(State(state): State<ApiState>, headers: HeaderMap, uri: Uri, Peer(peer): Peer, body: Bytes) -> Response {
    let run = || -> Result<Response, Response> {
        let oauth = oauth(&state)?;
        let request = decode_form(&headers, &body)?;
        let client = crate::auth::derive_client_metadata(user_agent(&headers), peer, &Default::default());
        Ok(match oauth.exchange_code(client, &urls_of(&headers, &uri), &request) {
            Ok(result) => axum::Json(result).into_response(),
            Err(error) => (StatusCode::BAD_REQUEST, axum::Json(error)).into_response(),
        })
    };
    no_store(run().unwrap_or_else(|response| response))
}
