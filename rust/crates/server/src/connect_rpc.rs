//! The two WebSocket methods of T3 Connect (`cloud.getRelayClientStatus`, `cloud.installRelayClient`), as services the transport
//! calls: the status of the relay client (`cloudflared`) this environment would run its tunnel with, and its installation with
//! progress. The wire shapes are the `Serialize` impls of `t3_remote::relay_client`.
use serde_json::{Value, json};
use t3_remote::relay_client::{InstallError, InstallProgressEvent};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::persistence::Store;

pub const STATUS: &str = "cloud.getRelayClientStatus";
pub const INSTALL: &str = "cloud.installRelayClient";

fn unavailable() -> Value {
    json!({"_tag":"NativeServiceUnavailableError","message":"T3 Connect is not available on this server."})
}

/// `cloud.getRelayClientStatus`.
pub async fn relay_client_status(store: &Store) -> Result<Value, Value> {
    let connect = crate::connect::current(store).ok_or_else(unavailable)?;
    Ok(serde_json::to_value(connect.services.relay_client_status().await).expect("a relay client status encodes"))
}

/// `cloud.installRelayClient`: the events of the installation as they happen, ending with `complete`, or with the failure.
pub fn install_relay_client(store: &Store) -> Result<UnboundedReceiver<Result<InstallProgressEvent, InstallError>>, Value> {
    Ok(crate::connect::current(store).ok_or_else(unavailable)?.services.install_relay_client())
}

/// The wire form of an installation failure (`RelayClientInstallFailedError`).
pub fn install_failure(error: &InstallError) -> Value {
    json!({"_tag":"RelayClientInstallFailedError","reason":serde_json::to_value(error.reason).expect("a reason encodes"),"message":error.message})
}
