//! The `server.upsertKeybinding` and `server.removeKeybinding` socket methods, and the pieces
//! `server.getConfig` and `subscribeServerConfig` take from the keybindings owner. Handlers
//! decode, call one `KeybindingsService` method, and map the failure to its wire `_tag`.
use crate::{
    keybindings::rule_from_parts,
    keybindings_service::{ConfigState, KeybindingsService, Subscription, UpsertInput},
};
use serde_json::{Value, json};
use std::path::Path;
use t3_contracts::{
    ServerKeybindingTarget, ServerRemoveKeybindingInput, ServerUpsertKeybindingInput,
    ServerUpsertKeybindingResult,
};

pub const METHODS: [&str; 2] = ["server.upsertKeybinding", "server.removeKeybinding"];

fn decode_failure(message: impl ToString) -> Value {
    json!({"_tag":"SchemaDecodeError","message":message.to_string()})
}
/// What the original answers where it cannot reach the keybindings file: a `KeybindingsConfigError` (tagged
/// `KeybindingsConfigParseError`, the only failure its keybindings methods and `server.getConfig` have), naming the file it keeps
/// beside the settings, `keybindings.json` in the state directory. A server without a configuration has no other file to name.
fn unavailable(state_dir: Option<&Path>) -> Value {
    let path = state_dir.map_or_else(|| Path::new("keybindings.json").to_path_buf(), |directory| directory.join("keybindings.json"));
    json!({"_tag":"KeybindingsConfigParseError","configPath":path.to_string_lossy(),"detail":"failed to access keybindings config"})
}

/// The service for the file the server configuration names.
async fn service_for(config: &Value) -> Result<KeybindingsService, Value> {
    let path = config["keybindingsConfigPath"].as_str().ok_or_else(|| unavailable(None))?;
    Ok(KeybindingsService::shared(Path::new(path)).await)
}

fn target_rule(target: &ServerKeybindingTarget) -> crate::keybindings::Rule {
    rule_from_parts(
        target.key.0.as_str(),
        target.command.as_str(),
        target.when.as_ref().map(|when| when.0.as_str()),
    )
}

pub async fn command(config: Option<&Value>, state_dir: Option<&Path>, tag: &str, payload: Value) -> Result<Value, Value> {
    let config = config.ok_or_else(|| unavailable(state_dir))?;
    let service = service_for(config).await?;
    let resolved = match tag {
        "server.upsertKeybinding" => {
            let input: ServerUpsertKeybindingInput =
                serde_json::from_value(payload).map_err(decode_failure)?;
            let rule = rule_from_parts(
                input.key.0.as_str(),
                input.command.as_str(),
                input.when.as_ref().map(|when| when.0.as_str()),
            );
            let replace = input.replace.as_ref().map(target_rule);
            service.upsert(UpsertInput { rule, replace }).await
        }
        "server.removeKeybinding" => {
            let input: ServerRemoveKeybindingInput =
                serde_json::from_value(payload).map_err(decode_failure)?;
            service.remove(target_rule(&input)).await
        }
        _ => return Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    }
    .map_err(|error| error.wire())?;
    // The original returns no issues here: the file was just written from valid rules.
    let result = json!({"keybindings":resolved.iter().map(|rule| rule.to_value()).collect::<Vec<_>>(),"issues":[]});
    serde_json::from_value::<ServerUpsertKeybindingResult>(result)
        .and_then(|typed| serde_json::to_value(typed))
        .map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))
}

/// Replaces the snapshot's keybindings and issues with the file's current state.
pub async fn overlay(config: &mut Value) -> Result<(), Value> {
    let state = service_for(config).await?.state().await.map_err(|error| error.wire())?;
    apply(config, &state);
    Ok(())
}

/// Like `overlay`, and subscribes in the same step so no change falls between the snapshot and
/// the stream that follows it.
pub async fn open_stream(config: &mut Value) -> Result<Subscription, Value> {
    let subscription = service_for(config).await?.subscribe().await.map_err(|error| error.wire())?;
    apply(config, &subscription.snapshot);
    Ok(subscription)
}

pub(crate) fn apply(config: &mut Value, state: &ConfigState) {
    config["keybindings"] = state.keybindings_json();
    config["issues"] = Value::Array(state.issues.clone());
}

/// The `subscribeServerConfig` event for a published state.
pub fn updated_event(state: &ConfigState) -> Value {
    json!({"version":1,"type":"keybindingsUpdated","payload":{"keybindings":state.keybindings_json(),"issues":state.issues}})
}
