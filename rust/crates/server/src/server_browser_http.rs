//! The server browser's HTTP routes (apps/server/src/preview/ServerBrowserStream.ts): the live view of a tab over a WebSocket, a file a tab
//! downloaded, and the files a viewer gives a page's open file picker.
//!
//! - `GET /api/preview-stream/ws?threadId&tabId[&interactive=false&maxWidth&maxHeight&quality]` (upgrade): frames as binary messages, state as
//!   JSON text messages (`ViewerOutput::to_json`). A viewer sends `{"type":"ack"}` for each frame, and, with operate scope, its input.
//!   Close codes: 4404 (the tab is gone), 4503 (the host needs setup; the reason is JSON naming the command), 1012 (reconnect).
//! - `GET /api/preview-stream/download?threadId&tabId&id`: a file a tab downloaded. Needs read scope.
//! - `POST /api/preview-stream/upload?threadId&tabId&chooser` with multipart `file` parts: answers the page's open picker; no parts
//!   cancels it. Needs operate scope. 409 when the picker is gone.
//!
//! Off by default: with the server browser switched off (`server_browser::SWITCH`) every one of these answers a plain 404, before it
//! looks at the credential, as a path no route serves.
use crate::{
    asset_access::ResolvedAsset,
    asset_response::file_response,
    server_browser::UploadedFile,
    server_browser_viewer::{Ack, ServerBrowserApi, ViewerOutput, ViewerStream},
    transport::ApiState,
};
use axum::{
    Router,
    extract::{
        DefaultBodyLimit, Multipart, State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use t3_contracts::{AuthEnvironmentScope, PREVIEW_STREAM_HOST_SETUP_CLOSE_CODE};

pub const PREVIEW_STREAM_ROUTE_PREFIX: &str = "/api/preview-stream";
/// Matches `PREVIEW_STREAM_TAB_GONE_CODE` in the client.
const TAB_GONE_CODE: u16 = 4404;
const DEFAULT_QUALITY: u32 = 70;
const MAX_UNACKNOWLEDGED_FRAMES: usize = 64;
const UPLOAD_MAX_FILE_BYTES: usize = 100 * 1024 * 1024;
const UPLOAD_MAX_FILES: usize = 20;

/// The server browser of an installation, once it is switched on. Stored beside the installation's other services.
#[derive(Default)]
struct Installed(Mutex<Option<Arc<dyn ServerBrowserApi>>>);

/// Makes the routes serve this browser.
pub fn install(store: &crate::persistence::Store, browser: Arc<dyn ServerBrowserApi>) {
    *store.extension(Installed::default).0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(browser);
}

/// The browser the routes serve, if the switch is on.
pub fn installed(store: &crate::persistence::Store) -> Option<Arc<dyn ServerBrowserApi>> {
    store.extension(Installed::default).0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/preview-stream/upload", post(upload).fallback(|method: Method| async move { upload_refusal(method) }).layer(DefaultBodyLimit::max(UPLOAD_MAX_FILE_BYTES + 1024 * 1024)))
        .route("/api/preview-stream/{*rest}", get(stream).fallback(|| async { not_found() }))
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// A method the upload route does not take is the router's 404, except a GET, which the original's handler refuses as it would any path.
fn upload_refusal(method: Method) -> Response {
    if method == Method::GET { (StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").into_response() } else { not_found() }
}

fn int_param(params: &HashMap<String, String>, name: &str, fallback: u32, max: u32) -> u32 {
    match params.get(name).and_then(|value| value.parse::<f64>().ok()) {
        Some(value) if value.is_finite() && value > 0.0 => (value.round() as u32).min(max),
        _ => fallback,
    }
}

fn query(uri: &Uri) -> HashMap<String, String> {
    url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()).into_owned().collect()
}

async fn stream(State(state): State<ApiState>, uri: Uri, headers: HeaderMap, ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>) -> Response {
    let Some(browser) = installed(&state.store) else { return not_found() };
    let params = query(&uri);
    match uri.path().strip_prefix(&format!("{PREVIEW_STREAM_ROUTE_PREFIX}/")).unwrap_or_default() {
        "download" => download(&state, browser.as_ref(), &headers, &params).await,
        "upload" => (StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").into_response(),
        "ws" if headers.get(header::UPGRADE).and_then(|value| value.to_str().ok()).is_some_and(|value| value.eq_ignore_ascii_case("websocket")) => {
            let ticket = params.get("wsTicket").map(String::as_str);
            let session = match crate::transport::authenticate_media_session(&state, &headers, ticket, AuthEnvironmentScope::OrchestrationRead) {
                Ok(session) => session,
                Err(response) => return response,
            };
            // The scopes are read from the one authentication: a one-use ticket is never checked a second time for operate scope.
            let can_operate = session.scopes.contains(&AuthEnvironmentScope::OrchestrationOperate) && params.get("interactive").map(String::as_str) != Some("false");
            let (thread_id, tab_id) = (params.get("threadId").cloned().unwrap_or_default(), params.get("tabId").cloned().unwrap_or_default());
            if thread_id.is_empty() || tab_id.is_empty() {
                return not_found();
            }
            let Ok(ws) = ws else { return StatusCode::BAD_REQUEST.into_response() };
            let attached = browser
                .attach_viewer(&thread_id, &tab_id, int_param(&params, "maxWidth", 1280, 7680), int_param(&params, "maxHeight", 800, 4320), int_param(&params, "quality", DEFAULT_QUALITY, 100), can_operate)
                .await;
            // A refused upgrade reads as an auth failure to ticket clients, so a missing tab is a close code they stop on.
            let outcome = match attached {
                Ok(viewer) => Attached::Viewer(Arc::from(viewer), can_operate),
                Err(crate::server_browser::ServerBrowserError::TabNotFound { .. }) => Attached::Close(TAB_GONE_CODE, "tab closed".into()),
                Err(error) => match error.host_setup() {
                    Some(setup) => Attached::Close(PREVIEW_STREAM_HOST_SETUP_CLOSE_CODE, serde_json::to_string(&setup).unwrap_or_default()),
                    None => {
                        tracing::warn!(error = %error, "server preview browser failed to start");
                        return (StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable").into_response();
                    }
                },
            };
            ws.max_message_size(16 * 1024 * 1024).on_upgrade(move |socket| serve_socket(socket, outcome))
        }
        _ => not_found(),
    }
}

enum Attached {
    Viewer(Arc<dyn ViewerStream>, bool),
    Close(u16, String),
}

async fn serve_socket(mut socket: WebSocket, attached: Attached) {
    let (viewer, can_operate) = match attached {
        Attached::Close(code, reason) => {
            let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
            return;
        }
        Attached::Viewer(viewer, can_operate) => (viewer, can_operate),
    };
    // Chromium's ack for each frame waits for the viewer's `ack` message.
    let mut unacknowledged: VecDeque<Ack> = VecDeque::new();
    loop {
        tokio::select! {
            output = viewer.next() => {
                let Some(output) = output else { break };
                match output {
                    ViewerOutput::Frame { data, ack } => {
                        if unacknowledged.len() >= MAX_UNACKNOWLEDGED_FRAMES {
                            // A viewer this far behind is disconnected.
                            break;
                        }
                        if socket.send(Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                        unacknowledged.push_back(ack);
                    }
                    ViewerOutput::Gone => {
                        let _ = socket.send(Message::Close(Some(CloseFrame { code: TAB_GONE_CODE, reason: "tab closed".into() }))).await;
                        break;
                    }
                    ViewerOutput::Reconnect => {
                        let _ = socket.send(Message::Close(Some(CloseFrame { code: 1012, reason: "reconnect".into() }))).await;
                        break;
                    }
                    other => {
                        let Some(json) = other.to_json() else { continue };
                        if socket.send(Message::Text(json.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            message = socket.recv() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text.as_str().to_owned(),
                    Some(Ok(Message::Binary(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    _ => break,
                };
                // Anything that is not JSON is read as null, which the viewer ignores.
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
                if parsed["type"] == "ack" {
                    // Paced by the browser, so it must not hold up input.
                    if let Some(ack) = unacknowledged.pop_front() {
                        ack.release();
                    }
                } else if can_operate {
                    // Ownership serialises actions in the service. Keep reading so dialog replies and takeover can unblock an action
                    // already waiting on the page.
                    let viewer = viewer.clone();
                    tokio::spawn(async move { viewer.input(parsed).await });
                }
            }
        }
    }
    viewer.close().await;
}

async fn download(state: &ApiState, browser: &dyn ServerBrowserApi, headers: &HeaderMap, params: &HashMap<String, String>) -> Response {
    let ticket = params.get("wsTicket").map(String::as_str);
    if let Err(response) = crate::transport::authenticate_media_request(state, headers, ticket, AuthEnvironmentScope::OrchestrationRead) {
        return response;
    }
    let get = |name: &str| params.get(name).map_or("", String::as_str);
    let Some((path, file_name)) = browser.open_download(get("threadId"), get("tabId"), get("id")) else { return not_found() };
    let asset = ResolvedAsset::File { path, download: true, file_name: Some(file_name), mime_type: None, file: None };
    file_response(&asset, headers.get(header::RANGE).and_then(|value| value.to_str().ok()), headers.get(header::IF_RANGE).and_then(|value| value.to_str().ok()), false).await
}

async fn upload(State(state): State<ApiState>, uri: Uri, headers: HeaderMap, multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>) -> Response {
    let Some(browser) = installed(&state.store) else { return not_found() };
    let params = query(&uri);
    if let Err(response) = crate::transport::authenticate_media_request(&state, &headers, params.get("wsTicket").map(String::as_str), AuthEnvironmentScope::OrchestrationOperate) {
        return response;
    }
    let Ok(mut multipart) = multipart else { return StatusCode::BAD_REQUEST.into_response() };
    let mut files = Vec::new();
    let mut total = 0usize;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        };
        // Only file parts count; a part without a file name is a plain field.
        let Some(name) = field.file_name().map(str::to_owned) else { continue };
        let mime_type = field.content_type().unwrap_or("application/octet-stream").to_owned();
        let Ok(bytes) = field.bytes().await else { return StatusCode::BAD_REQUEST.into_response() };
        total += bytes.len();
        if files.len() >= UPLOAD_MAX_FILES || bytes.len() > UPLOAD_MAX_FILE_BYTES || total > UPLOAD_MAX_FILE_BYTES {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        files.push(UploadedFile { name, mime_type, bytes: bytes.to_vec() });
    }
    let get = |name: &str| params.get(name).cloned().unwrap_or_default();
    if browser.answer_file_chooser(&get("threadId"), &get("tabId"), &get("chooser"), files).await {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::CONFLICT, "The page's file picker is no longer open.").into_response()
    }
}

#[cfg(test)]
#[path = "server_browser_http_tests.rs"]
mod tests;
