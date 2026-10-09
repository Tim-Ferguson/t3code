//! The webhook route: `/api/hooks/{hookId}/{token}` starts a webhook scheduled task's run. It takes no
//! session; the token in the URL, and the optional HMAC signature, are the whole credential. It reads
//! the raw body so the signature is checked over the exact bytes, and answers with a short JSON body and
//! an `x-t3-hook-outcome` header naming what happened to the request.
//!
//! Not ported: the T3 Connect relay's signed delivery proof. Every request is handled as a direct one,
//! which is also what the original does when the environment is not linked to T3 Connect, so a relay
//! delivery id or receive time is never trusted.
use crate::{
    scheduled_tasks::{ScheduledTasks, WebhookTriggerRequest, WebhookTriggerResult},
    scheduled_tasks_webhook::WebhookRequest,
    transport::ApiState,
};
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use indexmap::IndexMap;
use serde_json::{Value, json};

/// Largest request body a webhook accepts. The relay enforces the same cap.
pub const WEBHOOK_MAX_BODY_BYTES: usize = 1024 * 1024;
/// Response header naming what happened to the request; the relay records it.
pub const WEBHOOK_OUTCOME_HEADER: &str = "x-t3-hook-outcome";

pub fn routes() -> Router<ApiState> {
    Router::new().route("/api/hooks/{hook_id}/{token}", any(handle))
}

fn reply(status: StatusCode, body: Value, outcome: &str) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(WEBHOOK_OUTCOME_HEADER, outcome.parse().expect("outcome names are header-safe"));
    response
}

async fn handle(
    State(state): State<ApiState>,
    Path((hook_id, token)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // The original accepts these four methods only.
    if !matches!(method, Method::POST | Method::PUT | Method::PATCH | Method::GET) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let declared = headers.get("content-length").and_then(|value| value.to_str().ok()).map_or(Some(0.0), |value| value.parse::<f64>().ok());
    if declared.is_none_or(|length| !length.is_finite() || length > WEBHOOK_MAX_BODY_BYTES as f64) {
        return reply(StatusCode::PAYLOAD_TOO_LARGE, json!({"error": "body_too_large"}), "body_too_large");
    }
    // Chunked requests carry no content-length, so the reader itself is capped.
    let Ok(bytes) = axum::body::to_bytes(body, WEBHOOK_MAX_BODY_BYTES).await else {
        return reply(StatusCode::PAYLOAD_TOO_LARGE, json!({"error": "body_too_large_or_unreadable"}), "body_too_large");
    };
    let mut request_headers: IndexMap<String, String> = IndexMap::new();
    for (name, value) in &headers {
        if let Ok(value) = value.to_str() {
            // Repeated headers are joined as Node does.
            request_headers.entry(name.as_str().to_lowercase()).and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(value);
            }).or_insert_with(|| value.to_owned());
        }
    }
    let service = match ScheduledTasks::of(&state.store) {
        Ok(service) => service,
        Err(error) => {
            tracing::warn!(message = %error.message, "Webhook delivery failed");
            return reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "internal_error"}), "error");
        }
    };
    service.ensure_launcher(state.execution.as_ref());
    let body_text = String::from_utf8_lossy(&bytes).into_owned();
    let request = WebhookTriggerRequest {
        hook_id: hook_id.clone(),
        token,
        request: WebhookRequest {
            method: method.to_string(),
            path: format!("{}/{}", crate::scheduled_tasks::WEBHOOK_ROUTE_PREFIX, encode_component(&hook_id)),
            query: uri.query().unwrap_or_default().to_owned(),
            headers: request_headers,
            body_text,
        },
        body: bytes.to_vec(),
        relay_delivery_id: None,
        received_at: None,
    };
    match service.trigger_webhook(request).await {
        Ok(WebhookTriggerResult::Accepted { delivery_id, outcome }) => reply(StatusCode::ACCEPTED, json!({"deliveryId": delivery_id}), outcome),
        Ok(WebhookTriggerResult::NotFound) => reply(StatusCode::NOT_FOUND, json!({"error": "hook_not_found"}), "not_found"),
        Ok(WebhookTriggerResult::RejectedSignature) => reply(StatusCode::UNAUTHORIZED, json!({"error": "invalid_signature"}), "rejected_signature"),
        Ok(WebhookTriggerResult::Disabled) => reply(StatusCode::CONFLICT, json!({"error": "hook_disabled"}), "disabled"),
        Ok(WebhookTriggerResult::RateLimited { outcome }) => reply(StatusCode::TOO_MANY_REQUESTS, json!({"error": "rate_limited"}), outcome),
        Ok(WebhookTriggerResult::Expired) => reply(StatusCode::GONE, json!({"error": "delivery_too_old"}), "expired"),
        // The sender only ever sees the fixed error body.
        Err(error) => {
            tracing::warn!(hook_id = %hook_id, message = %error.message, "Webhook delivery failed");
            reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "internal_error"}), "error")
        }
    }
}

fn encode_component(value: &str) -> String {
    value.bytes().map(|byte| if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) { (byte as char).to_string() } else { format!("%{byte:02X}") }).collect()
}
