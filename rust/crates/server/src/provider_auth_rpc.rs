//! Typed credential commands shared by the socket transport and future adapters.
use crate::provider_auth_service::{ProviderAuthChanges, ProviderAuthService};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use t3_contracts::*;

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, Value> {
    serde_json::from_value(payload)
        .map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))
}
fn unavailable() -> Value {
    json!({"_tag":"NativeServiceUnavailableError","message":"Provider authentication is not configured."})
}
fn wire(error: ProviderSetupError) -> Value {
    serde_json::to_value(error).expect("typed provider setup error")
}
pub async fn command(
    service: Option<&ProviderAuthService>,
    tag: &str,
    payload: Value,
    owner: &str,
) -> Result<Value, Value> {
    let result = match tag {
        "provider.auth.start" => {
            let input: ProviderAuthStartInput = decode(payload)?;
            service
                .ok_or_else(unavailable)?
                .start(input, owner.into())
                .await
        }
        "provider.auth.respond" => {
            let input: ProviderAuthRespondInput = decode(payload)?;
            service.ok_or_else(unavailable)?.respond(input, owner).await
        }
        "provider.auth.complete" => {
            let input: ProviderAuthCompleteInput = decode(payload)?;
            service
                .ok_or_else(unavailable)?
                .complete(input, owner)
                .await
        }
        "provider.auth.cancel" => {
            let input: ProviderAuthCancelInput = decode(payload)?;
            service.ok_or_else(unavailable)?.cancel(input, owner).await
        }
        "provider.auth.logout" => {
            let input: ProviderSetupInput = decode(payload)?;
            service
                .ok_or_else(unavailable)?
                .logout(input.instance_id.as_str())
                .await
        }
        _ => return Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    };
    result
        .map(|state| serde_json::to_value(state).expect("typed authentication state"))
        .map_err(wire)
}
pub async fn subscribe(
    service: Option<&ProviderAuthService>,
    payload: Value,
    owner: &str,
) -> Result<ProviderAuthChanges, Value> {
    let input: ProviderSetupInput = decode(payload)?;
    service
        .ok_or_else(unavailable)?
        .subscribe(input.instance_id.as_str(), owner.into())
        .await
        .map_err(wire)
}
