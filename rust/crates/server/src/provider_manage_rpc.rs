//! Provider refresh, update, managed installation and ACP registry session/provider management over the
//! socket. Handlers decode, make one service call and map errors to the original `_tag` envelopes.
use crate::transport::ApiState;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use t3_contracts::*;

pub const METHODS: &[&str] = &[
    "server.refreshProviders",
    "server.updateProvider",
    "provider.install.start",
    "provider.install.cancel",
    "provider.install.remove",
    "server.listAcpRegistrySessions",
    "server.importAcpRegistrySession",
    "server.deleteAcpRegistrySession",
    "server.listAcpRegistryProviders",
    "server.setAcpRegistryProvider",
    "server.disableAcpRegistryProvider",
    "server.logoutAcpRegistry",
];

pub(crate) fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, Value> {
    serde_json::from_value(payload).map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))
}
pub(crate) fn unavailable(what: &str) -> Value {
    json!({"_tag":"NativeServiceUnavailableError","message":format!("{what} is not configured.")})
}
pub(crate) fn encode<T: serde::Serialize>(value: T) -> Result<Value, Value> {
    serde_json::to_value(value).map_err(|_| json!({"_tag":"NativeServiceError","message":"Provider management result could not be encoded."}))
}
/// The providers payload every refresh and update answers with.
fn providers_payload(providers: Vec<Value>) -> Result<Value, Value> {
    for provider in &providers {
        serde_json::from_value::<ServerProvider>(provider.clone()).map_err(|error| json!({"_tag":"NativeServiceError","message":format!("Provider {} did not fit the contract: {error}: {provider}", provider["instanceId"])}))?;
    }
    let typed: ServerProviderUpdatedPayload = serde_json::from_value(json!({"providers":providers}))
        .map_err(|error| json!({"_tag":"NativeServiceError","message":format!("Provider snapshots did not fit the contract: {error}")}))?;
    encode(typed)
}

pub async fn command(state: &ApiState, tag: &str, payload: Value) -> Result<Value, Value> {
    let providers = state.providers.as_ref().ok_or_else(|| unavailable("Provider registry"))?;
    match tag {
        "server.refreshProviders" => {
            let input: ServerRefreshProvidersInput = decode(payload)?;
            // An untargeted refresh is "re-read everything's status", which includes quota from configured usage-limit sources.
            // Awaited, not spawned: the answer carries the providers, and a client that asked for a refresh expects the quota current.
            if input.instance_id.is_none() {
                if let Some(sources) = crate::usage_limit_sources::current(&state.store) {
                    sources.refresh().await;
                }
            }
            let snapshots = providers.management().refresh(providers, &input).await.map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))?;
            providers_payload(snapshots)
        }
        "server.updateProvider" => {
            let input: ServerProviderUpdateInput = decode(payload)?;
            providers_payload(providers.management().update_provider(providers, &input).await?)
        }
        "provider.install.start" => {
            let input: ProviderSetupInput = decode(payload)?;
            crate::provider_install::start(providers, &input.instance_id).await.map_err(setup_wire).and_then(install_state)
        }
        "provider.install.cancel" => {
            let input: ProviderInstallCancelInput = decode(payload)?;
            crate::provider_install::cancel(providers, &input).await.map_err(setup_wire).and_then(install_state)
        }
        "provider.install.remove" => {
            let input: ProviderSetupInput = decode(payload)?;
            crate::provider_install::remove(providers, &input.instance_id).await.map_err(setup_wire).and_then(install_state)
        }
        "server.listAcpRegistrySessions" => crate::acp_registry_manage::handle_list_sessions(state, decode(payload)?).await,
        "server.importAcpRegistrySession" => crate::acp_registry_manage::handle_import_session(state, decode(payload)?).await,
        "server.deleteAcpRegistrySession" => crate::acp_registry_manage::handle_delete_session(state, decode(payload)?).await,
        "server.listAcpRegistryProviders" => crate::acp_registry_manage::handle_list_providers(state, decode(payload)?).await,
        "server.setAcpRegistryProvider" => crate::acp_registry_manage::handle_set_provider(state, decode(payload)?).await,
        "server.disableAcpRegistryProvider" => crate::acp_registry_manage::handle_disable_provider(state, decode(payload)?).await,
        "server.logoutAcpRegistry" => crate::acp_registry_manage::handle_logout(state, decode(payload)?).await,
        _ => Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag,"message":"This method has not yet been ported."})),
    }
}

pub(crate) fn setup_wire(error: ProviderSetupError) -> Value {
    serde_json::to_value(error).expect("typed provider setup error")
}
/// An install state as clients read it, checked against the contract.
pub(crate) fn install_state(state: Value) -> Result<Value, Value> {
    let typed: ProviderInstallState = serde_json::from_value(state).map_err(|error| json!({"_tag":"NativeServiceError","message":format!("Install state did not fit the contract: {error}")}))?;
    encode(typed)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A debug build embeds each awaited future by value, so a handler's future that grows past this was
    /// built from something that should have been boxed (see tests/async_stack_budget.rs).
    const FUTURE_LIMIT_BYTES: usize = 32 * 1024;
    fn fits<F>(name: &str, future: &F) {
        let size = std::mem::size_of_val(future);
        assert!(size <= FUTURE_LIMIT_BYTES, "the future of {name} is {size} bytes (limit {FUTURE_LIMIT_BYTES}): box the large future it awaits");
    }
    #[tokio::test]
    async fn management_entry_points_stay_small() {
        // Every real provider is disabled, so building the registry runs nothing.
        let settings: ServerSettings = serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false}}})).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let registry = crate::provider_registry::ProviderRegistry::discover(&settings, directory.path()).await.unwrap();
        let management = registry.management();
        let update: ServerProviderUpdateInput = serde_json::from_value(json!({"provider":"codex"})).unwrap();
        let refresh: ServerRefreshProvidersInput = serde_json::from_value(json!({})).unwrap();
        let instance: ProviderInstanceId = "codex".parse().unwrap();
        fits("update_provider", &management.update_provider(&registry, &update));
        fits("refresh", &management.refresh(&registry, &refresh));
        fits("refresh_all", &registry.refresh_all());
        fits("refresh_workspace", &registry.refresh_workspace("codex", "/", true));
        fits("install start", &crate::provider_install::start(&registry, &instance));
        fits("install remove", &crate::provider_install::remove(&registry, &instance));
        fits("install subscribe", &crate::provider_install::subscribe(&registry, &instance));
    }
}
