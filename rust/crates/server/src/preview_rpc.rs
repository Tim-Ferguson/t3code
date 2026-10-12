//! The `preview.*` methods of the WebSocket RPC group (apps/server/src/ws.ts): each decodes its payload, calls one method of the
//! environment's `PreviewManager` and maps the answer. `subscribePreviewEvents` is a stream and lives with the others in `transport.rs`.
use crate::{preview_manager::PreviewManager, transport::ApiState};
use serde_json::{Value, json};
use std::sync::Arc;
use t3_contracts::{Defect, PreviewClearProfileError, PreviewClearProfileErrorTag, RpcRequest};

pub const METHODS: [&str; 9] = [
    "preview.open",
    "preview.navigate",
    "preview.resize",
    "preview.adjust",
    "preview.refresh",
    "preview.close",
    "preview.list",
    "preview.clearProfile",
    "preview.reportStatus",
];

/// The manager of this environment, made with its store.
pub fn manager(state: &ApiState) -> Arc<PreviewManager> {
    state.store.extension(PreviewManager::default)
}

fn decode<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T, Value> {
    serde_json::from_value(request.payload.clone()).map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))
}

fn wire(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("a contract value is JSON")
}

pub async fn command(state: &ApiState, request: &RpcRequest) -> Result<Value, Value> {
    let manager = manager(state);
    let failed = |error: t3_contracts::PreviewError| wire(error);
    match request.tag.as_str() {
        "preview.open" => manager.open(decode(request)?).map(wire).map_err(failed),
        "preview.navigate" => manager.navigate(decode(request)?).map(wire).map_err(failed),
        "preview.resize" => manager.resize(decode(request)?).map(wire).map_err(failed),
        "preview.adjust" => manager.adjust(decode(request)?).map(wire).map_err(failed),
        "preview.refresh" => manager.refresh(decode(request)?).map(|()| Value::Null).map_err(failed),
        "preview.reportStatus" => manager.report_status(decode(request)?, false).map(|()| Value::Null).map_err(failed),
        "preview.close" => {
            manager.close(decode(request)?);
            Ok(Value::Null)
        }
        "preview.list" => Ok(wire(manager.list(decode(request)?))),
        "preview.clearProfile" => {
            let input: t3_contracts::PreviewClearProfileInput = decode(request)?;
            let Some(state_dir) = state.store.state_dir() else { return Ok(Value::Null) };
            // A running server browser closes the profile's open tabs before its storage goes; without one nothing has the storage open.
            let cleared = match crate::server_browser_http::installed(&state.store) {
                Some(browser) => browser.clear_profile(input.profile_id.as_str()).await,
                None => crate::server_browser_profiles::clear_profile(&crate::server_browser_profiles::profiles_dir(state_dir), input.profile_id.as_str()).await,
            };
            cleared
                .map(|()| Value::Null)
                .map_err(|error| {
                    wire(PreviewClearProfileError {
                        tag: PreviewClearProfileErrorTag,
                        profile_id: input.profile_id.as_str().to_owned(),
                        cause: Defect(json!({"name":"Error","message":error.to_string()})),
                    })
                })
        }
        other => unreachable!("{other} is not a preview method"),
    }
}
