//! Instance routing and source-compatible legacy configuration hydration.
use crate::{
    acp_runtime::AcpInstance,
    codex::{CodexConfig, CodexInstance},
    provider_process::ProcessError,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, sync::Arc};
use t3_contracts::{
    ProviderInstanceConfig, ProviderInstanceConfigMap, ProviderInstanceId, ServerSettings,
};

pub fn derive_instance_configs(settings: &ServerSettings) -> ProviderInstanceConfigMap {
    let mut instances = settings.provider_instances.clone();
    let legacy =
        serde_json::to_value(&settings.providers).expect("typed legacy settings serialize");
    for (driver, config) in legacy.as_object().unwrap() {
        let id: ProviderInstanceId = driver.parse().unwrap();
        instances.entry(id).or_insert_with(|| {
            serde_json::from_value(json!({"driver":driver,"config":config}))
                .expect("legacy settings envelope")
        });
    }
    instances
}
enum RegisteredInstance {
    Codex(CodexInstance),
    Acp(AcpInstance),
}
#[derive(Clone)]
pub struct ProviderRegistry {
    codex: Arc<HashMap<String, CodexInstance>>,
    acp: Arc<HashMap<String, AcpInstance>>,
    snapshots: Arc<Vec<Value>>,
    settings: Arc<ServerSettings>,
}
impl ProviderRegistry {
    pub async fn discover(settings: &ServerSettings, cwd: &Path) -> Result<Self, ProcessError> {
        let entries = derive_instance_configs(settings);
        let mut results = futures_util::stream::iter(entries)
            .map(|(id, entry)| async move {
                let id = id.to_string();
                if entry.driver.as_str() == "acpRegistry" {
                    let config = match serde_json::from_value::<t3_contracts::AcpRegistrySettings>(
                        entry.config.clone().unwrap_or(json!({})),
                    ) {
                        Ok(config) => config,
                        Err(error) => {
                            return Ok((
                                id.clone(),
                                None,
                                unavailable(&id, &entry, format!("Invalid ACP config: {error}"))?,
                            ));
                        }
                    };
                    let instance = AcpInstance {
                        instance_id: id.clone(),
                        display_name: entry
                            .display_name
                            .as_ref()
                            .and_then(Option::as_ref)
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "ACP Registry".into()),
                        accent_color: entry
                            .accent_color
                            .as_ref()
                            .and_then(Option::as_ref)
                            .map(ToString::to_string),
                        enabled: t3_contracts::resolve_provider_instance_enabled(&entry),
                        config,
                        environment: entry
                            .environment
                            .as_ref()
                            .map(|variables| {
                                variables
                                    .iter()
                                    .map(|variable| {
                                        (variable.name.to_string(), variable.value.clone())
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    };
                    let snapshot = instance
                        .discover(cwd)
                        .await
                        .map_err(|error| ProcessError::Protocol(error.to_string()))?;
                    return Ok((id, Some(RegisteredInstance::Acp(instance)), snapshot));
                }
                if entry.driver.as_str() != "codex" {
                    return Ok((
                        id.clone(),
                        None,
                        unavailable(
                            &id,
                            &entry,
                            format!(
                                "Driver '{}' is not registered in this native build.",
                                entry.driver
                            ),
                        )?,
                    ));
                }
                let config = match serde_json::from_value::<CodexConfig>(
                    entry.config.clone().unwrap_or(json!({})),
                ) {
                    Ok(config) => config,
                    Err(error) => {
                        return Ok((
                            id.clone(),
                            None,
                            unavailable(
                                &id,
                                &entry,
                                format!("Invalid config for instance '{id}': {error}"),
                            )?,
                        ));
                    }
                };
                let environment = entry
                    .environment
                    .as_ref()
                    .map(|variables| {
                        variables
                            .iter()
                            .map(|variable| (variable.name.to_string(), variable.value.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                let instance = CodexInstance {
                    instance_id: id.clone(),
                    display_name: entry
                        .display_name
                        .as_ref()
                        .and_then(Option::as_ref)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "Codex".into()),
                    accent_color: entry
                        .accent_color
                        .as_ref()
                        .and_then(Option::as_ref)
                        .map(ToString::to_string),
                    enabled: t3_contracts::resolve_provider_instance_enabled(&entry),
                    config,
                    environment,
                };
                let snapshot = instance.discover(cwd).await?;
                Ok::<_, ProcessError>((id, Some(RegisteredInstance::Codex(instance)), snapshot))
            })
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        // Keep deterministic instance ordering even when probes complete out of order.
        results.sort_by(|left, right| left.0.cmp(&right.0));
        let mut codex = HashMap::new();
        let mut acp = HashMap::new();
        let mut snapshots = vec![];
        for (id, instance, snapshot) in results {
            if let Some(instance) = instance {
                match instance {
                    RegisteredInstance::Codex(instance) => {
                        codex.insert(id, instance);
                    }
                    RegisteredInstance::Acp(instance) => {
                        acp.insert(id, instance);
                    }
                }
            }
            snapshots.push(snapshot);
        }
        Ok(Self {
            codex: Arc::new(codex),
            acp: Arc::new(acp),
            snapshots: Arc::new(snapshots),
            settings: Arc::new(settings.clone()),
        })
    }
    pub fn snapshots(&self) -> &[Value] {
        &self.snapshots
    }
    pub(crate) fn settings(&self) -> &ServerSettings {
        &self.settings
    }
    pub fn driver(&self, instance_id: &str) -> Result<&str, ProcessError> {
        let snapshot = self
            .snapshots
            .iter()
            .find(|snapshot| snapshot["instanceId"] == instance_id)
            .ok_or_else(|| {
                ProcessError::Protocol(format!("Provider instance '{instance_id}' is unavailable."))
            })?;
        if snapshot["enabled"] != true || snapshot["status"] != "ready" {
            return Err(ProcessError::Protocol(
                snapshot["message"]
                    .as_str()
                    .unwrap_or("Provider instance is not ready.")
                    .into(),
            ));
        }
        Ok(snapshot["driver"].as_str().unwrap())
    }
    pub fn acp(&self, instance_id: &str) -> Result<AcpInstance, ProcessError> {
        if self.driver(instance_id)? != "acpRegistry" {
            return Err(ProcessError::Protocol(
                "Provider instance is not ACP Registry.".into(),
            ));
        }
        self.acp
            .get(instance_id)
            .cloned()
            .ok_or_else(|| ProcessError::Protocol("ACP provider instance is unavailable.".into()))
    }
    pub fn codex(&self, instance_id: &str) -> Result<CodexInstance, ProcessError> {
        let instance = self
            .codex
            .get(instance_id)
            .filter(|instance| instance.enabled)
            .ok_or_else(|| {
                ProcessError::Protocol(format!(
                    "Provider instance '{instance_id}' is unavailable or disabled."
                ))
            })?;
        let snapshot = self
            .snapshots
            .iter()
            .find(|snapshot| snapshot["instanceId"] == instance_id)
            .unwrap();
        if snapshot["status"] != "ready" {
            return Err(ProcessError::Protocol(
                snapshot["message"]
                    .as_str()
                    .unwrap_or("Provider is not ready.")
                    .into(),
            ));
        }
        Ok(instance.clone())
    }
}
fn unavailable(
    id: &str,
    entry: &ProviderInstanceConfig,
    reason: String,
) -> Result<Value, ProcessError> {
    let mut value = json!({"instanceId":id,"driver":entry.driver,"displayName":entry.display_name.as_ref().and_then(Option::as_ref).map(ToString::to_string).unwrap_or_else(||entry.driver.to_string()),"enabled":false,"installed":false,"version":null,"status":"disabled","auth":{"status":"unknown"},"checkedAt":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),"models":[],"slashCommands":[],"skills":[],"availability":"unavailable","unavailableReason":reason,"message":reason});
    if let Some(accent) = entry.accent_color.as_ref().and_then(Option::as_ref) {
        value["accentColor"] = json!(accent);
    }
    let typed: t3_contracts::ServerProvider =
        serde_json::from_value(value).map_err(|error| ProcessError::Protocol(error.to_string()))?;
    Ok(serde_json::to_value(typed).unwrap())
}
/// Apply the original client settings redaction without changing runtime secrets.
pub fn redact_settings(settings: &ServerSettings) -> Value {
    let mut value = serde_json::to_value(settings).unwrap();
    for instance in value["providerInstances"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        if let Some(environment) = instance
            .get_mut("environment")
            .and_then(Value::as_array_mut)
        {
            for variable in environment {
                if variable["sensitive"] == true {
                    let redacted = variable["value"]
                        .as_str()
                        .is_some_and(|value| !value.is_empty())
                        || variable["valueRedacted"] == true;
                    variable["value"] = json!("");
                    if redacted {
                        variable["valueRedacted"] = json!(true);
                    }
                } else {
                    variable.as_object_mut().unwrap().remove("valueRedacted");
                }
            }
        }
    }
    let redact = |value: &mut Value| {
        if value.as_str().is_some_and(|value| !value.is_empty()) {
            *value = json!("••••••");
        }
    };
    if let Some(sources) = value["usageLimitSources"].as_object_mut() {
        for source in sources.values_mut() {
            if let Some(key) = source.get_mut("managementKey") {
                redact(key);
            }
        }
    }
    for field in ["accessToken", "apiToken"] {
        redact(&mut value["bitbucket"][field]);
    }
    if let Some(tokens) = value["github"]["tokens"].as_object_mut() {
        for token in tokens.values_mut() {
            redact(token);
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_instances_win_legacy_slots_and_unknown_drivers_survive() {
        let settings:ServerSettings=serde_json::from_value(json!({"providers":{"codex":{"enabled":true}},"providerInstances":{"codex":{"driver":"fork","config":{"opaque":true}},"codex_work":{"driver":"codex","enabled":false}}})).unwrap();
        let instances = derive_instance_configs(&settings);
        assert_eq!(instances[&"codex".parse().unwrap()].driver.as_str(), "fork");
        assert_eq!(
            instances[&"codex".parse().unwrap()]
                .config
                .as_ref()
                .unwrap()["opaque"],
            true
        );
        assert!(instances.contains_key(&"pi".parse().unwrap()));
        assert_eq!(
            instances[&"codex_work".parse().unwrap()].enabled,
            Some(false)
        );
    }
    #[tokio::test]
    async fn unavailable_and_disabled_instances_never_launch_executables() {
        let settings:ServerSettings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false,"config":{"binaryPath":"/must-not-execute"}},"unknown":{"driver":"fork","config":{}}}})).unwrap();
        let registry = ProviderRegistry::discover(&settings, std::env::temp_dir().as_path())
            .await
            .unwrap();
        let snapshots = registry.snapshots();
        assert_eq!(
            snapshots
                .iter()
                .find(|value| value["instanceId"] == "codex")
                .unwrap()["status"],
            "disabled"
        );
        assert_eq!(
            snapshots
                .iter()
                .find(|value| value["instanceId"] == "unknown")
                .unwrap()["availability"],
            "unavailable"
        );
        assert!(registry.codex("codex").is_err());
    }
    #[tokio::test]
    async fn either_explicit_disable_flag_prevents_executable_launch() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("provider");
        let marker = directory.path().join("launched");
        std::fs::write(
            &script,
            "#!/usr/bin/env python3\nimport os\nopen(os.environ['MARKER'],'w').write('started')\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let settings:ServerSettings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":true,"config":{"enabled":false,"binaryPath":script},"environment":[{"name":"MARKER","value":marker}]},"codex_work":{"driver":"codex","enabled":false,"config":{"enabled":true,"binaryPath":script},"environment":[{"name":"MARKER","value":marker}]}}})).unwrap();
        let registry = ProviderRegistry::discover(&settings, directory.path())
            .await
            .unwrap();
        for id in ["codex", "codex_work"] {
            assert_eq!(
                registry
                    .snapshots()
                    .iter()
                    .find(|value| value["instanceId"] == id)
                    .unwrap()["status"],
                "disabled"
            );
            assert!(registry.codex(id).is_err());
        }
        assert!(!marker.exists());
    }
    #[test]
    fn settings_redact_sensitive_environment_without_mutating_runtime_values() {
        let settings:ServerSettings=serde_json::from_value(json!({"providerInstances":{"work":{"driver":"codex","environment":[{"name":"SECRET","value":"private","sensitive":true},{"name":"PUBLIC","value":"public","valueRedacted":true}]}},"github":{"tokens":{"github.com":"private"}},"bitbucket":{"apiToken":"private"}})).unwrap();
        let redacted = redact_settings(&settings);
        assert_eq!(
            redacted["providerInstances"]["work"]["environment"][0]["value"],
            ""
        );
        assert_eq!(
            redacted["providerInstances"]["work"]["environment"][0]["valueRedacted"],
            true
        );
        assert!(
            redacted["providerInstances"]["work"]["environment"][1]
                .get("valueRedacted")
                .is_none()
        );
        assert_ne!(redacted["github"]["tokens"]["github.com"], "private");
        assert_eq!(
            settings.provider_instances[&"work".parse().unwrap()]
                .environment
                .as_ref()
                .unwrap()[0]
                .value,
            "private"
        );
    }
}
