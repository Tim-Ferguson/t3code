//! Native startup configuration, using shared source-backed defaults.
use crate::{
    acp_registry_support::{Catalog, RegistryError},
    provider_process::ProcessError,
    provider_registry::{ProviderRegistry, redact_settings},
    server_secret_store::ServerSecretStore,
    server_settings::{SettingsOptions, SettingsService},
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use t3_contracts::ServerSettings;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Configuration I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Configuration is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Provider(#[from] ProcessError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Settings(#[from] crate::server_settings::SettingsError),
    #[error(transparent)]
    Secret(#[from] crate::server_secret_store::SecretStoreError),
}
#[derive(Clone)]
pub struct NativeConfig {
    pub settings: ServerSettings,
    pub providers: ProviderRegistry,
    pub snapshot: Value,
    pub settings_service: Option<SettingsService>,
}
impl NativeConfig {
    pub async fn load(
        state_dir: &Path,
        cwd: &Path,
        settings_path: Option<&Path>,
        environment: &Value,
        auth: &Value,
        history: crate::persistence::Store,
    ) -> Result<Self, ConfigError> {
        let path = settings_path
            .map(PathBuf::from)
            .unwrap_or_else(|| state_dir.join("settings.json"));
        let directory = state_dir.join("secrets");
        let secrets = tokio::task::spawn_blocking(move || ServerSecretStore::open(directory))
            .await
            .map_err(std::io::Error::other)??;
        let mut options = SettingsOptions::file(path, secrets);
        options.history = Some(history);
        let service = SettingsService::start(options).await?;
        let settings = service.snapshot().await?;
        let mut config = Self::from_settings(settings, state_dir, cwd, environment, auth).await?;
        config.settings_service = Some(service);
        Ok(config)
    }
    pub async fn from_settings(
        settings: ServerSettings,
        state_dir: &Path,
        cwd: &Path,
        environment: &Value,
        auth: &Value,
    ) -> Result<Self, ConfigError> {
        let catalog = Catalog::new(state_dir.join("caches"), state_dir.join("tools"))?;
        let providers =
            ProviderRegistry::discover_with_catalog(&settings, cwd, Some(catalog)).await?;
        let logs = state_dir.join("logs");
        tokio::fs::create_dir_all(&logs).await?;
        // Advertise only installed native services. The editor, OTLP, workspace
        // management and history paging ports are not yet exposed by this build.
        let snapshot = json!({"environment":environment,"auth":auth,"cwd":cwd,"keybindingsConfigPath":state_dir.join("keybindings.json"),"keybindings":t3_contracts::default_resolved_keybindings(),"issues":[],"providers":providers.snapshots(),"availableEditors":[],"remoteOpenTargets":[],"directEndpoints":[],"observability":{"logsDirectoryPath":logs,"localTracingEnabled":false,"otlpTracesEnabled":false,"otlpMetricsEnabled":false,"otlpLogsEnabled":false},"settings":redact_settings(&settings),"shellResumeCompletionMarker":true,"threadResumeCompletionMarker":true,"threadSnapshotPagination":true});
        let typed: t3_contracts::ServerConfig = serde_json::from_value(snapshot)?;
        Ok(Self {
            settings,
            providers,
            snapshot: serde_json::to_value(typed)?,
            settings_service: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn enabled_provider_without_optional_environment_roundtrips_config() {
        let directory = tempfile::tempdir().unwrap();
        let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
        let settings: ServerSettings = serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","config":{"binaryPath":binary}}}})).unwrap();
        let environment = json!({"environmentId":"fixture-environment","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}});
        let auth = json!({"policy":"loopback-browser","bootstrapMethods":["one-time-token"],"sessionMethods":["browser-session-cookie","bearer-access-token"],"sessionCookieName":"fixture-session","serverUpdateScope":"environment:maintain"});
        let config = NativeConfig::from_settings(
            settings,
            directory.path(),
            directory.path(),
            &environment,
            &auth,
        )
        .await
        .unwrap();
        assert!(
            config.snapshot["settings"]["providerInstances"]["codex"]
                .get("environment")
                .is_none()
        );
        let provider = config.snapshot["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["instanceId"] == "codex")
            .unwrap();
        assert_eq!(provider["status"], "ready");
        assert_eq!(provider["models"][0]["slug"], "fixture-model");
        serde_json::from_value::<t3_contracts::ServerConfig>(config.snapshot).unwrap();
    }
    #[tokio::test]
    async fn real_config_uses_defaults_environment_and_grants_without_secret_leakage() {
        let directory = tempfile::tempdir().unwrap();
        let settings:ServerSettings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false,"environment":[{"name":"SECRET","value":"fixture-secret","sensitive":true}]}}})).unwrap();
        let environment = json!({"environmentId":"fixture-environment","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}});
        let auth = json!({"policy":"loopback-browser","bootstrapMethods":["one-time-token"],"sessionMethods":["browser-session-cookie","bearer-access-token"],"sessionCookieName":"fixture-session","serverUpdateScope":"environment:maintain"});
        let config = NativeConfig::from_settings(
            settings,
            directory.path(),
            directory.path(),
            &environment,
            &auth,
        )
        .await
        .unwrap();
        assert_eq!(
            config.snapshot["environment"]["environmentId"],
            "fixture-environment"
        );
        assert_eq!(config.snapshot["keybindings"].as_array().unwrap().len(), 80);
        assert_eq!(
            config.snapshot["settings"]["providers"]["codex"]["binaryPath"],
            "codex"
        );
        assert!(!config.snapshot.to_string().contains("fixture-secret"));
        assert_eq!(
            config.settings.provider_instances[&"codex".parse().unwrap()]
                .environment
                .as_ref()
                .unwrap()[0]
                .value,
            "fixture-secret"
        );
    }
}
