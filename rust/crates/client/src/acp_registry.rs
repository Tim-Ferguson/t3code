//! Adding an agent from the ACP registry: what the search step asks and shows, and the instance a prepared agent becomes. Port
//! of `AcpRegistrySearchStep.tsx` and the registry parts of `AddProviderInstanceDialog.tsx`/`.logic.ts`.
use serde_json::{Value, json};

pub const SEARCH: &str = "server.searchAcpRegistry";
pub const PREPARE: &str = "server.prepareAcpRegistryAgent";
pub const ACCEPT_URL_AUTH: &str = "server.acceptAcpRegistryUrlAuth";
pub const UNINSTALL_MANAGED_BINARY: &str = "server.uninstallAcpRegistryManagedBinary";
/// How long the search box waits after the last keystroke before it asks.
pub const SEARCH_DEBOUNCE_MS: u64 = 300;
/// The longest query the server takes.
pub const QUERY_MAX: usize = 120;

pub fn search_input(query: &str) -> Value {
    json!({"query": query.trim().chars().take(QUERY_MAX).collect::<String>()})
}

/// `isConfiguredAcpRegistryAgent`: whether an instance already runs this registry agent. `instances` is the settings'
/// `providerInstances`.
pub fn is_configured(instances: &Value, agent_id: &str) -> bool {
    instances.as_object().into_iter().flatten().any(|(_, instance)| {
        instance["driver"] == "acpRegistry" && instance["config"].is_object() && instance["config"]["source"] != "local" && instance["config"]["agentId"] == agent_id
    })
}

/// The button of a result: Add, Added, or the work in progress (a binary is downloaded, anything else prepared).
pub fn button_label(agent: &Value, added: bool, preparing: bool) -> &'static str {
    if added {
        "Added"
    } else if preparing {
        if agent["distribution"] == "binary" { "Downloading" } else { "Preparing" }
    } else {
        "Add"
    }
}

/// `applyAcpRegistryPrepareResult`: the agent as the server prepared it.
pub fn applied(agent: &Value, prepared: &Value) -> Value {
    let mut agent = agent.clone();
    agent["id"] = prepared["agentId"].clone();
    agent["version"] = prepared["version"].clone();
    agent["distribution"] = prepared["distribution"].clone();
    agent
}

/// The settings of the instance a prepared agent becomes: the agent by its id, left to pick its distribution, with the icon
/// the official registry names for it when there is one.
pub fn instance_config(agent: &Value) -> Value {
    let mut config = json!({"agentId": agent["id"], "distribution": "auto"});
    if let Some(icon) = official_icon(agent["icon"].as_str()) {
        config["registryIconUrl"] = json!(icon);
    }
    config
}
const CDN_HOSTNAME: &str = "cdn.agentclientprotocol.com";
/// `resolveOfficialAcpRegistryIconUrl`: an icon is shown only from the official registry's credential-free origin.
fn official_icon(icon: Option<&str>) -> Option<String> {
    let url = url::Url::parse(icon.filter(|icon| !icon.is_empty())?).ok()?;
    (url.scheme() == "https" && url.host_str() == Some(CDN_HOSTNAME) && url.port().is_none() && url.username().is_empty() && url.password().is_none()).then(|| url.to_string())
}

/// The agent a registry instance was added from, for the cleanup of its managed files when the instance is deleted: none for a
/// local command.
pub fn managed_agent(instance: &Value) -> Option<String> {
    if instance["driver"] != "acpRegistry" || instance["config"]["source"] == "local" {
        return None;
    }
    instance["config"]["agentId"].as_str().map(str::to_owned)
}

pub const PREPARE_FAILED: &str = "The ACP could not be prepared.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_already_run_by_an_instance_is_added_and_a_local_command_is_not_an_agent() {
        let instances = json!({
            "mine": {"driver": "acpRegistry", "config": {"agentId": "goose", "distribution": "auto"}},
            "local": {"driver": "acpRegistry", "config": {"source": "local", "agentId": "amp", "commandPath": "/bin/amp"}},
            "other": {"driver": "codex", "config": {"agentId": "gemini"}},
        });
        assert!(is_configured(&instances, "goose"));
        assert!(!is_configured(&instances, "amp"), "a local command is not the registry's agent");
        assert!(!is_configured(&instances, "gemini"));
        assert_eq!(managed_agent(&instances["mine"]).as_deref(), Some("goose"));
        assert_eq!(managed_agent(&instances["local"]), None);
        assert_eq!(managed_agent(&instances["other"]), None);
    }

    #[test]
    fn a_result_says_what_its_button_is_doing_and_a_prepared_agent_becomes_an_instance() {
        let agent = json!({"id": "goose", "name": "Goose", "version": "1.0.0", "distribution": "binary", "icon": "https://cdn.agentclientprotocol.com/registry/goose.svg"});
        assert_eq!(button_label(&agent, false, false), "Add");
        assert_eq!(button_label(&agent, false, true), "Downloading");
        assert_eq!(button_label(&json!({"distribution": "npx"}), false, true), "Preparing");
        assert_eq!(button_label(&agent, true, false), "Added");
        let prepared = json!({"agentId": "goose", "version": "1.1.0", "distribution": "npx", "prepared": true});
        let applied = applied(&agent, &prepared);
        assert_eq!((applied["version"].clone(), applied["distribution"].clone()), (json!("1.1.0"), json!("npx")));
        assert_eq!(instance_config(&applied), json!({"agentId": "goose", "distribution": "auto", "registryIconUrl": "https://cdn.agentclientprotocol.com/registry/goose.svg"}));
        assert_eq!(instance_config(&json!({"id": "x", "icon": "https://cdn.agentclientprotocol.com.evil.example/x.svg"})), json!({"agentId": "x", "distribution": "auto"}));
        assert_eq!(search_input("  goose  "), json!({"query": "goose"}));
    }
}
