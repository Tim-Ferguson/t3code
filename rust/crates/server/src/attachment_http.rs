//! Signed-URL routes: `POST /api/attachments/upload/<token>` stores an upload and
//! `GET|HEAD /api/assets/<token>/<name>` serves an asset. Neither takes a session: the
//! signed token is the whole credential, which is what lets an `<img>` or a plain
//! `fetch` use the URL a socket call returned.
use crate::{
    asset_access::ResolvedAsset,
    asset_response::{bytes_response, file_response}, attachment_service::AttachmentService,
    attachment_upload::StoreOutcome, transport::ApiState,
};
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;

pub const ATTACHMENT_UPLOAD_ROUTE: &str = "/api/attachments/upload/{*token}";
pub const ASSET_ROUTE: &str = "/api/assets/{*suffix}";

pub fn routes() -> Router<ApiState> {
    // Any other method, and an empty token, is a plain 404 as in the original router.
    Router::new()
        .route(
            ATTACHMENT_UPLOAD_ROUTE,
            post(upload).fallback(|| async { not_found() }),
        )
        .route(
            "/api/attachments/upload/",
            axum::routing::any(|| async { not_found() }),
        )
        .route(
            ASSET_ROUTE,
            get(download).fallback(|| async { not_found() }),
        )
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

async fn upload(
    State(state): State<ApiState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(files) = AttachmentService::for_store(&state.store) else {
        return not_found();
    };
    let now_ms = Utc::now().timestamp_millis();
    let validator = files.clone();
    let claims =
        tokio::task::spawn_blocking(move || validator.validate_upload_token(&token, now_ms))
            .await
            .ok()
            .flatten();
    let Some(claims) = claims else {
        return not_found();
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok());
    if !claims.content_length_matches(declared) {
        return (
            StatusCode::BAD_REQUEST,
            "Content-Length must match the upload size.",
        )
            .into_response();
    }
    match files.store_upload(&claims, body.into_data_stream()).await {
        StoreOutcome::Stored => StatusCode::NO_CONTENT.into_response(),
        StoreOutcome::Rejected { status, detail } => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            detail,
        )
            .into_response(),
    }
}

async fn download(
    State(state): State<ApiState>,
    uri: Uri,
    method: Method,
    headers: HeaderMap,
) -> Response {
    // The part after the prefix is read from the raw request target, still percent-encoded: a
    // workspace URL serves siblings by this path, and decoding it twice would let `%252e%252e`
    // become `..`.
    let suffix = uri.path().strip_prefix("/api/assets/").unwrap_or_default().to_owned();
    let (Some(files), Some((token, name))) = (
        AttachmentService::for_store(&state.store),
        suffix
            .split_once('/')
            .filter(|(token, _)| !token.is_empty()),
    ) else {
        return not_found();
    };
    let (token, name) = (token.to_owned(), name.to_owned());
    let now_ms = Utc::now().timestamp_millis();
    let store = state.store.clone();
    let resolved = tokio::task::spawn_blocking(move || files.resolve_asset_request(&token, &name, now_ms, Some(&store)))
        .await
        .ok()
        .flatten();
    let Some(asset) = resolved else {
        return not_found();
    };
    let header_text = |name| headers.get(name).and_then(|value| value.to_str().ok());
    match &asset {
        ResolvedAsset::Bytes { bytes, mime_type } => bytes_response(bytes, mime_type),
        ResolvedAsset::GithubMedia { url, expires_at, .. } => {
            let host = crate::source_control_rpc::host(&state);
            // Both media hosts are served by github.com's account, which is the host `gh` stores it under.
            let token = crate::source_control_github_api::credential(&host, "github.com").await.ok().map(|(token, _)| token);
            let client = crate::asset_github_media::client();
            Box::pin(crate::asset_github_media::response(
                &client,
                &crate::asset_github_media::Request {
                    url,
                    expires_at: *expires_at,
                    now_ms,
                    range: header_text(header::RANGE),
                    if_range: header_text(header::IF_RANGE),
                    token: token.as_ref().map(|token| token.expose()),
                    connect_to: &|target| target.to_string(),
                },
            ))
            .await
        }
        ResolvedAsset::File { .. } => {
            file_response(
                &asset,
                header_text(header::RANGE),
                header_text(header::IF_RANGE),
                method == Method::HEAD,
            )
            .await
        }
    }
}
