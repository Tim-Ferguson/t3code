//! What `orchestration.dispatchCommand` makes of a command before it plans anything: a thread command (and a provider
//! switch) is a `ThreadCommand`, every other command a `ProviderCommand`, each re-serialized to the value the planner reads.
//! `rpc_handler_decoders.rs` runs this over the generated cases.
use serde_json::{Value, json};

/// Whether a command is planned by the thread service. `thread.stop` and `thread.user-input.dismiss` are named like a
/// thread command and are run commands.
pub(crate) fn routes_to_thread_service(kind: &str) -> bool {
    !crate::service_commands::is_service_command(kind) && (kind.starts_with("thread.") && !matches!(kind, "thread.stop" | "thread.user-input.dismiss")) || kind == "provider.switch"
}

/// The command as the planner reads it, or why it is not one.
#[cfg(test)]
pub(crate) fn command(payload: &Value) -> Result<Value, String> {
    let mut input = payload.clone();
    let kind = input["type"].as_str().unwrap_or_default().to_owned();
    if crate::service_commands::is_service_command(&kind) {
        crate::service_commands::command(&input)
    } else if routes_to_thread_service(&kind) {
        crate::thread::normalize_command(&input)
    } else {
        if kind == "message.dispatch" {
            if input.get("createdBy").is_none() {
                input["createdBy"] = json!("user");
            }
            if input.get("creationSource").is_none() {
                input["creationSource"] = json!("web");
            }
        }
        serde_json::from_value::<t3_contracts::ProviderCommand>(input).map_err(|error| error.to_string()).and_then(|decoded| decoded.service_payload().map_err(|error| error.to_string()))
    }
}
