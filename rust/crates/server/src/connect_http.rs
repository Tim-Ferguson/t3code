//! The `/api/connect/*` and `/api/t3-connect/*` routes (`apps/server/src/cloud/http.ts`, the `connect` group of
//! `packages/contracts/src/environmentHttp.ts`). Each decodes its body, calls one method of the link service, and maps the
//! failure the way the original does (`CloudLinkError::to_http_failure`).
//!
//! The first five take a signed-in session with `relay:read` or `relay:write`. The other three (`health` and the two
//! `mint-credential` routes) take none: the signed proof from the relay in the body is the credential, and what they answer
//! is not to be cached. A server that never installed T3 Connect has none of them.
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use t3_contracts::AuthEnvironmentScope;
use t3_remote::{cloud_link::CloudLinkError, link_checks::InboundRequest};

use crate::{
    connect::Connect,
    transport::{ApiState, authenticate, require},
};

pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/connect/link-proof", post(link_proof))
        .route("/api/connect/relay-config", post(relay_config))
        .route("/api/connect/link-state", get(link_state))
        .route("/api/connect/unlink", post(unlink))
        .route("/api/connect/preferences", post(preferences))
        .route("/api/t3-connect/health", post(health))
        .route("/api/connect/mint-credential", post(mint_credential))
        .route("/api/t3-connect/mint-credential", post(mint_credential))
}

/// What the routes that answer a credential or a proof say about caching.
fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn failure(error: &CloudLinkError) -> Response {
    let failure = error.to_http_failure();
    if failure.status == 500 {
        tracing::error!(error = %error, "environment connect route failed");
    }
    (StatusCode::from_u16(failure.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), axum::Json(failure.body())).into_response()
}

/// A body the schema of the route does not accept answers an empty 400, as the original's HTTP API renders it.
fn undecodable() -> Response {
    (StatusCode::BAD_REQUEST, axum::Json(Value::Null)).into_response()
}

fn decode<T: DeserializeOwned>(body: &Bytes) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|_| undecodable())
}

fn connect(state: &ApiState) -> Result<std::sync::Arc<Connect>, Response> {
    crate::connect::current(&state.store).ok_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// The session behind a route that needs one: authenticated first, then the body, then the scope, which is the order the
/// original's HTTP API runs them in.
fn authenticated(state: &ApiState, headers: &HeaderMap) -> Result<crate::auth::Session, Response> {
    authenticate(state, headers).map_err(IntoResponse::into_response)
}

fn scoped(session: &crate::auth::Session, scope: AuthEnvironmentScope) -> Result<(), Response> {
    require(session, scope).map_err(IntoResponse::into_response)
}

async fn link_proof(State(state): State<ApiState>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let run = || -> Result<Response, Response> {
        let connect = connect(&state)?;
        let session = authenticated(&state, &headers)?;
        let request = decode(&body)?;
        scoped(&session, AuthEnvironmentScope::RelayWrite)?;
        let inbound = InboundRequest {
            original_url: uri.path_and_query().map_or_else(|| uri.to_string(), |target| target.as_str().to_owned()),
            headers: headers
                .iter()
                .filter_map(|(name, value)| Some((name.as_str().to_lowercase(), value.to_str().ok()?.to_owned())))
                .collect(),
        };
        let proof = connect.services.link_proof(&request, &inbound).map_err(|error| failure(&error))?;
        Ok(no_store(axum::Json(proof).into_response()))
    };
    run().unwrap_or_else(|response| response)
}

async fn relay_config(State(state): State<ApiState>, headers: HeaderMap, body: Bytes) -> Response {
    let run = async {
        let connect = connect(&state)?;
        let session = authenticated(&state, &headers)?;
        let payload = decode(&body)?;
        scoped(&session, AuthEnvironmentScope::RelayWrite)?;
        let result = connect.services.apply_relay_config(&payload).await.map_err(|error| failure(&error))?;
        Ok::<_, Response>(axum::Json(result).into_response())
    };
    run.await.unwrap_or_else(|response| response)
}

async fn link_state(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let run = || -> Result<Response, Response> {
        let connect = connect(&state)?;
        let session = authenticated(&state, &headers)?;
        scoped(&session, AuthEnvironmentScope::RelayRead)?;
        Ok(axum::Json(connect.services.link_state().map_err(|error| failure(&error))?).into_response())
    };
    run().unwrap_or_else(|response| response)
}

async fn unlink(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let run = async {
        let connect = connect(&state)?;
        let session = authenticated(&state, &headers)?;
        scoped(&session, AuthEnvironmentScope::RelayWrite)?;
        Ok::<_, Response>(axum::Json(connect.services.unlink().await.map_err(|error| failure(&error))?).into_response())
    };
    run.await.unwrap_or_else(|response| response)
}

async fn preferences(State(state): State<ApiState>, headers: HeaderMap, body: Bytes) -> Response {
    let run = async {
        let connect = connect(&state)?;
        let session = authenticated(&state, &headers)?;
        let payload = decode(&body)?;
        scoped(&session, AuthEnvironmentScope::RelayWrite)?;
        Ok::<_, Response>(axum::Json(connect.services.update_preferences(payload).await.map_err(|error| failure(&error))?).into_response())
    };
    run.await.unwrap_or_else(|response| response)
}

async fn health(State(state): State<ApiState>, body: Bytes) -> Response {
    let run = || -> Result<Response, Response> {
        let connect = connect(&state)?;
        let request = decode(&body)?;
        let answer = connect.services.answer_health_request(&request).map_err(|error| failure(&error))?;
        Ok(no_store(axum::Json(answer).into_response()))
    };
    run().unwrap_or_else(|response| response)
}

async fn mint_credential(State(state): State<ApiState>, body: Bytes) -> Response {
    let run = async {
        let connect = connect(&state)?;
        let request = decode(&body)?;
        let answer = connect.services.mint_credential(&request).await.map_err(|error| failure(&error))?;
        Ok::<_, Response>(no_store(axum::Json(answer).into_response()))
    };
    run.await.unwrap_or_else(|response| response)
}
