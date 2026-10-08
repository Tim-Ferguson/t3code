//! Load-time settings migrations. Only the settings owner's trusted-file path
//! may persist these changes; malformed files are retained for repair.
use crate::{
    persistence::{Store, StoreError, read_projections},
    server_settings_model::derive_legacy_project_overrides,
    server_settings_secrets::{REDACTED, SecretBackend, github_secret_name},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use t3_contracts::{ModelSelection, ProjectScript, ServerSettings};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderHistoryRow {
    pub provider_name: String,
    pub provider_instance_id: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyProjectRow {
    pub project_id: String,
    pub default_model_selection: Option<String>,
    pub default_thread_env_mode: Option<String>,
    pub auto_pull: i64,
    pub scripts: String,
}
fn table_exists(connection: &rusqlite::Connection, table: &str) -> Result<bool, StoreError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}
pub fn provider_history(store: &Store) -> Result<Vec<ProviderHistoryRow>, StoreError> {
    store.read(|connection| {
        let mut rows=Vec::new();
        for table in ["projection_thread_sessions","provider_session_runtime"] {
            if table_exists(connection,table)? {
                let mut query=connection.prepare(&format!("SELECT DISTINCT provider_name,provider_instance_id FROM {table} WHERE provider_name IN ('cursor','grok','opencode')"))?;
                for row in query.query_map([],|row|Ok(ProviderHistoryRow{provider_name:row.get(0)?,provider_instance_id:row.get(1)?}))? {rows.push(row?);}
            }
        }
        // The Rust projections retain the same provider/session identities.
        // This also works before importing the legacy relational projection.
        for thread in read_projections(connection,"thread")? {
            for session in thread["providerSessions"].as_array().into_iter().flatten() {
                if let Some(driver)=session["driver"].as_str().filter(|driver|matches!(*driver,"cursor"|"grok"|"opencode")) {
                    rows.push(ProviderHistoryRow{provider_name:driver.into(),provider_instance_id:session["providerInstanceId"].as_str().map(str::to_owned)});
                }
            }
        }
        Ok(rows)
    })
}
pub fn legacy_projects(store: &Store) -> Result<Vec<LegacyProjectRow>, StoreError> {
    store.read(|connection| {
        if table_exists(connection,"projection_projects")? {
            let mut query=connection.prepare("SELECT project_id,default_model_selection_json,default_thread_env_mode,auto_pull,scripts_json FROM projection_projects WHERE deleted_at IS NULL")?;
            return Ok(query.query_map([],|row|Ok(LegacyProjectRow{project_id:row.get(0)?,default_model_selection:row.get(1)?,default_thread_env_mode:row.get(2)?,auto_pull:row.get(3)?,scripts:row.get(4)?}))?.collect::<Result<_,_>>()?);
        }
        Ok(read_projections(connection,"project")?.into_iter().filter(|project|project["deletedAt"].is_null()).map(|project|LegacyProjectRow {
            project_id:project["projectId"].as_str().unwrap_or_default().into(),
            default_model_selection:project.get("defaultModelSelection").map(Value::to_string),
            default_thread_env_mode:project["defaultThreadEnvMode"].as_str().map(str::to_owned),
            auto_pull:i64::from(project["autoPull"]==true),
            scripts:project.get("scripts").unwrap_or(&Value::Null).to_string(),
        }).collect())
    })
}
pub fn restore_used_providers(
    settings: &ServerSettings,
    persisted: &Value,
    history: &[ProviderHistoryRow],
) -> Result<ServerSettings, serde_json::Error> {
    let used: HashSet<_> = history
        .iter()
        .map(|row| row.provider_name.as_str())
        .collect();
    let instances: HashSet<_> = history
        .iter()
        .map(|row| {
            row.provider_instance_id
                .as_deref()
                .unwrap_or(&row.provider_name)
        })
        .collect();
    let mut value = serde_json::to_value(settings)?;
    for driver in ["cursor", "grok", "opencode"] {
        value["providers"][driver]["enabled"] = json!(
            persisted["providers"][driver]["enabled"]
                .as_bool()
                .unwrap_or_else(|| used.contains(driver))
        );
    }
    for (id, instance) in value["providerInstances"].as_object_mut().unwrap() {
        if instance.get("enabled").is_none()
            && matches!(
                instance["driver"].as_str(),
                Some("cursor" | "grok" | "opencode")
            )
            && instances.contains(id.as_str())
        {
            instance["enabled"] = json!(true);
        }
    }
    serde_json::from_value(value)
}
fn set(entries: &mut Map<String, Value>, id: &str, key: &str, value: Value) {
    let entry = entries
        .entry(id.to_owned())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .unwrap();
    if !entry.contains_key(key) {
        entry.insert(key.into(), value);
    }
}
fn decode_scripts(raw: &str) -> Option<Vec<ProjectScript>> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let scripts = value.as_array()?;
    if scripts.iter().any(|script| {
        !script.is_object()
            || ["runOnSettle", "async", "previewUrl", "autoOpenPreview"]
                .iter()
                .any(|key| script.get(key).is_some_and(Value::is_null))
    }) {
        return None;
    }
    serde_json::from_value(value).ok()
}
pub fn fold_legacy_projects(
    settings: &ServerSettings,
    rows: &[LegacyProjectRow],
) -> Result<ServerSettings, serde_json::Error> {
    let mut value = serde_json::to_value(settings)?;
    if settings.project_settings_folded
        || (rows.is_empty()
            && [
                "projectAgentBrowserAccessOverrides",
                "projectAutoPullOverrides",
                "projectScriptOverrides",
            ]
            .iter()
            .all(|key| value[key].as_object().unwrap().is_empty()))
    {
        return Ok(settings.clone());
    }
    let mut entries = value["projectSettingsOverrides"]
        .as_object()
        .unwrap()
        .clone();
    let mut reset_scripts = HashSet::new();
    for (legacy, key) in [
        (
            "projectAgentBrowserAccessOverrides",
            "enableAgentBrowserAccess",
        ),
        ("projectAutoPullOverrides", "defaultAutoPull"),
        ("projectScriptOverrides", "defaultProjectScripts"),
    ] {
        for (id, setting) in value[legacy].as_object().unwrap() {
            if legacy == "projectScriptOverrides" && setting.is_null() {
                reset_scripts.insert(id.as_str());
            } else {
                set(&mut entries, id, key, setting.clone());
            }
        }
    }
    for row in rows {
        if let Ok(Some(model)) = serde_json::from_str::<Option<ModelSelection>>(
            row.default_model_selection.as_deref().unwrap_or("null"),
        ) {
            set(
                &mut entries,
                &row.project_id,
                "defaultModelSelection",
                serde_json::to_value(model)?,
            );
        }
        if let Some(mode) = row
            .default_thread_env_mode
            .as_deref()
            .filter(|mode| matches!(*mode, "local" | "worktree"))
        {
            set(
                &mut entries,
                &row.project_id,
                "defaultThreadEnvMode",
                json!(mode),
            );
        }
        if row.auto_pull == 1 {
            set(
                &mut entries,
                &row.project_id,
                "defaultAutoPull",
                json!(true),
            );
        }
        if !reset_scripts.contains(row.project_id.as_str()) {
            if let Some(scripts) = decode_scripts(&row.scripts) {
                if !scripts.is_empty() {
                    set(
                        &mut entries,
                        &row.project_id,
                        "defaultProjectScripts",
                        serde_json::to_value(scripts)?,
                    );
                }
            }
        }
    }
    value["projectSettingsOverrides"] = json!(
        entries
            .into_iter()
            .filter(|(_, entry)| !entry.as_object().unwrap().is_empty())
            .collect::<Map<_, _>>()
    );
    value["projectSettingsFolded"] = json!(true);
    derive_legacy_project_overrides(&mut value);
    serde_json::from_value(value)
}
/// Source migration is best effort per token. A failed secret write leaves its
/// inline value working and retried on the next uncached load.
pub fn move_inline_tokens(
    settings: &ServerSettings,
    secrets: &dyn SecretBackend,
) -> Result<ServerSettings, serde_json::Error> {
    let mut value = serde_json::to_value(settings)?;
    for (field, name) in [
        ("accessToken", "bitbucket-access-token"),
        ("apiToken", "bitbucket-api-token"),
    ] {
        let token = value["bitbucket"][field].as_str().unwrap();
        if !token.is_empty() && token != REDACTED {
            if secrets.set(name, token.as_bytes()).is_ok() {
                value["bitbucket"][field] = json!(REDACTED);
            } else {
                tracing::warn!("failed to migrate inline Bitbucket credential");
            }
        }
    }
    for (host, token) in value["github"]["tokens"].as_object_mut().unwrap() {
        let raw = token.as_str().unwrap();
        if !raw.is_empty() && raw != REDACTED {
            if secrets
                .set(&github_secret_name(host), raw.as_bytes())
                .is_ok()
            {
                *token = json!(REDACTED);
            } else {
                tracing::warn!("failed to migrate inline GitHub credential");
            }
        }
    }
    serde_json::from_value(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        persistence::Store,
        server_secret_store::{Operation, SecretStoreError, ServerSecretStore},
        server_settings::{SettingsOptions, SettingsService, SettingsWriter},
    };
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    fn options(path: &Path, secrets: &ServerSecretStore, history: &Store) -> SettingsOptions {
        let mut options = SettingsOptions::file(path.into(), secrets.clone());
        options.watch = false;
        options.history = Some(history.clone());
        options
    }
    fn sql(store: &Store, query: &str) {
        store
            .transaction(|connection| {
                connection.execute_batch(query)?;
                Ok(())
            })
            .unwrap();
    }
    fn wire(settings: ServerSettings) -> Value {
        serde_json::to_value(settings).unwrap()
    }
    async fn startup_error(options: SettingsOptions) -> crate::server_settings::SettingsError {
        match SettingsService::start(options).await {
            Err(error) => error,
            Ok(service) => {
                service.shutdown().await;
                panic!("expected settings acquisition failure");
            }
        }
    }

    #[tokio::test]
    async fn unrelated_invalid_setting_preserves_successfully_decoded_explicit_disable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let secrets = ServerSecretStore::open(temp.path().join("secrets")).unwrap();
        let history = Store::open(temp.path().join("state.sqlite")).unwrap();
        sql(
            &history,
            "CREATE TABLE projection_thread_sessions(provider_name TEXT,provider_instance_id TEXT); INSERT INTO projection_thread_sessions VALUES('cursor',NULL);",
        );
        // Full schema decoding fails, but the independent provider envelope is valid.
        let contents = r#"{"defaultAutoPull":"invalid","providers":{"cursor":{"enabled":false}},"bitbucket":{"accessToken":"untrusted-inline"}}"#;
        std::fs::write(&path, contents).unwrap();
        let service = SettingsService::start(options(&path, &secrets, &history))
            .await
            .unwrap();
        let actual = wire(service.snapshot().await.unwrap());
        assert_eq!(actual["providers"]["cursor"]["enabled"], false);
        assert_eq!(actual["projectSettingsFolded"], false);
        assert_eq!(actual["bitbucket"]["accessToken"], "");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        assert!(secrets.get("bitbucket-access-token").unwrap().is_none());
        service.shutdown().await;

        // A bad optional envelope makes the file untrusted, even if the full
        // wire codec would normalize its null flag. History then supplies defaults.
        std::fs::write(
            &path,
            r#"{"providers":{"cursor":{"enabled":null}},"defaultAutoPull":true}"#,
        )
        .unwrap();
        let service = SettingsService::start(options(&path, &secrets, &history))
            .await
            .unwrap();
        let actual = wire(service.snapshot().await.unwrap());
        assert_eq!(actual["providers"]["cursor"]["enabled"], true);
        assert_eq!(actual["defaultAutoPull"], false);
        service.shutdown().await;
    }

    #[tokio::test]
    async fn actual_store_fold_preserves_resets_disables_and_persisted_marker_across_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let secrets = ServerSecretStore::open(temp.path().join("secrets")).unwrap();
        let history = Store::open(temp.path().join("state.sqlite")).unwrap();
        sql(
            &history,
            "CREATE TABLE projection_thread_sessions(provider_name TEXT,provider_instance_id TEXT); INSERT INTO projection_thread_sessions VALUES('cursor','work'),('opencode','custom'),('grok',NULL); CREATE TABLE projection_projects(project_id TEXT,default_model_selection_json TEXT,default_thread_env_mode TEXT,auto_pull INTEGER,scripts_json TEXT,deleted_at TEXT);",
        );
        let script = json!({"id":"check","name":"Check","command":"npm test","icon":"play","runOnWorktreeCreate":false});
        history
            .transaction(|connection| {
                for id in ["legacy", "scripted"] {
                    connection.execute(
                        "INSERT INTO projection_projects VALUES(?1,?2,'worktree',1,?3,NULL)",
                        rusqlite::params![
                            id,
                            json!({"instanceId":"codex","model":"old-model"}).to_string(),
                            json!([script]).to_string()
                        ],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        std::fs::write(&path,json!({
            "providers":{"cursor":{"enabled":false},"grok":{"enabled":false}},
            "providerInstances":{"work":{"driver":"cursor","config":{"enabled":false}},"custom":{"driver":"opencode"}},
            "projectScriptOverrides":{"legacy":null},
            "projectAgentBrowserAccessOverrides":{"legacy":false},
            "projectSettingsOverrides":{"legacy":{"defaultModelSelection":null},"scripted":{"defaultAutoPull":false}},
            "bitbucket":{"accessToken":"inline-bitbucket"},"github":{"tokens":{"GitHub.COM":"inline-github"}}
        }).to_string()).unwrap();
        let service = SettingsService::start(options(&path, &secrets, &history))
            .await
            .unwrap();
        let actual = wire(service.snapshot().await.unwrap());
        assert_eq!(actual["providers"]["cursor"]["enabled"], false);
        assert_eq!(actual["providerInstances"]["work"]["enabled"], false);
        assert_eq!(actual["providerInstances"]["custom"]["enabled"], true);
        assert_eq!(actual["providers"]["grok"]["enabled"], false);
        assert_eq!(
            actual["projectSettingsOverrides"]["legacy"]["defaultModelSelection"],
            Value::Null
        );
        assert!(
            actual["projectSettingsOverrides"]["legacy"]
                .get("defaultProjectScripts")
                .is_none()
        );
        assert_eq!(
            actual["projectSettingsOverrides"]["legacy"]["enableAgentBrowserAccess"],
            false
        );
        assert_eq!(
            actual["projectSettingsOverrides"]["scripted"]["defaultAutoPull"],
            false
        );
        assert_eq!(
            actual["projectSettingsOverrides"]["scripted"]["defaultProjectScripts"],
            json!([script])
        );
        assert_eq!(actual["bitbucket"]["accessToken"], "inline-bitbucket");
        let persisted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["projectSettingsFolded"], true);
        assert_eq!(persisted["bitbucket"]["accessToken"], REDACTED);
        assert_eq!(persisted["github"]["tokens"]["github.com"], REDACTED);
        assert_eq!(
            secrets.get(&github_secret_name("GitHub.COM")).unwrap(),
            Some(b"inline-github".to_vec())
        );
        service
            .update(
                serde_json::from_value(json!({"projectSettingsOverrides":{"legacy":null}}))
                    .unwrap(),
            )
            .await
            .unwrap();
        service.shutdown().await;
        // Marker means a reset remains reset and obsolete DB fields aren't read.
        sql(
            &history,
            "DROP TABLE projection_projects; CREATE TABLE projection_projects(wrong_column TEXT);",
        );
        let service = SettingsService::start(options(&path, &secrets, &history))
            .await
            .unwrap();
        let actual = wire(service.snapshot().await.unwrap());
        assert_eq!(actual["projectSettingsFolded"], true);
        assert!(actual["projectSettingsOverrides"].get("legacy").is_none());
        service.shutdown().await;
    }

    struct FailWriter;
    impl SettingsWriter for FailWriter {
        fn write(&self, _: &Path, _: &str) -> std::io::Result<()> {
            Err(std::io::Error::other("injected migration persist failure"))
        }
    }
    #[tokio::test]
    async fn migration_write_failure_is_fatal_and_retains_successful_secret_moves() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let secrets = ServerSecretStore::open(temp.path().join("secrets")).unwrap();
        let history = Store::memory().unwrap();
        let contents = r#"{"bitbucket":{"accessToken":"retry-after-file-failure"}}"#;
        std::fs::write(&path, contents).unwrap();
        let mut injected = options(&path, &secrets, &history);
        injected.writer = Arc::new(FailWriter);
        assert_eq!(startup_error(injected).await.operation, "write-file");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        assert_eq!(
            secrets.get("bitbucket-access-token").unwrap(),
            Some(b"retry-after-file-failure".to_vec())
        );
        let service = SettingsService::start(options(&path, &secrets, &history))
            .await
            .unwrap();
        assert_eq!(
            wire(service.snapshot().await.unwrap())["bitbucket"]["accessToken"],
            "retry-after-file-failure"
        );
        let persisted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["bitbucket"]["accessToken"], REDACTED);
        service.shutdown().await;
    }
    struct FailFirstSecret {
        store: ServerSecretStore,
        fail: AtomicBool,
    }
    impl SecretBackend for FailFirstSecret {
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
            self.store.get(name)
        }
        fn remove(&self, name: &str) -> Result<(), SecretStoreError> {
            self.store.remove(name)
        }
        fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretStoreError> {
            if name == "bitbucket-access-token" && self.fail.swap(false, Ordering::AcqRel) {
                return Err(SecretStoreError {
                    operation: Operation::Persist,
                    resource: "injected secret".into(),
                    cause: None,
                });
            }
            self.store.set(name, value)
        }
    }
    #[tokio::test]
    async fn failed_secret_migration_is_best_effort_and_retries_on_next_load() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let secrets = ServerSecretStore::open(temp.path().join("secrets")).unwrap();
        let history = Store::memory().unwrap();
        std::fs::write(&path,r#"{"bitbucket":{"accessToken":"best-effort"},"github":{"tokens":{"github.com":"working"}}}"#).unwrap();
        let mut injected = options(&path, &secrets, &history);
        injected.secrets = Arc::new(FailFirstSecret {
            store: secrets.clone(),
            fail: AtomicBool::new(true),
        });
        let service = SettingsService::start(injected).await.unwrap();
        assert_eq!(
            wire(service.snapshot().await.unwrap())["bitbucket"]["accessToken"],
            "best-effort"
        );
        let persisted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["bitbucket"]["accessToken"], "best-effort");
        assert_eq!(persisted["github"]["tokens"]["github.com"], REDACTED);
        service.reload().await.unwrap();
        let persisted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["bitbucket"]["accessToken"], REDACTED);
        service.shutdown().await;
    }
    #[tokio::test]
    async fn history_and_project_query_failures_are_fatal_and_not_cached() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let secrets = ServerSecretStore::open(temp.path().join("secrets")).unwrap();
        let history = Store::open(temp.path().join("state.sqlite")).unwrap();
        std::fs::write(&path, "{}").unwrap();
        let service = SettingsService::start(options(&path, &secrets, &history))
            .await
            .unwrap();
        sql(
            &history,
            "CREATE TABLE projection_thread_sessions(wrong_column TEXT);",
        );
        assert_eq!(
            service.reload().await.unwrap_err().operation,
            "read-provider-history"
        );
        sql(
            &history,
            "DROP TABLE projection_thread_sessions; CREATE TABLE projection_projects(wrong_column TEXT);",
        );
        assert_eq!(
            service.snapshot().await.unwrap_err().operation,
            "read-project-settings"
        );
        sql(&history, "DROP TABLE projection_projects;");
        assert_eq!(
            wire(service.snapshot().await.unwrap())["projectSettingsFolded"],
            false
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{}");
        service.shutdown().await;
    }

    #[test]
    fn load_decisions_match_unchanged_original_source() {
        for (index, line) in include_str!("../tests/fixtures/settings-migrations.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            if row["op"] == "metadata" {
                let result = crate::server_settings::decode_persisted_optional_providers(
                    row["raw"].as_str().unwrap().as_bytes(),
                );
                assert_eq!(
                    result.is_ok(),
                    row["accepted"].as_bool().unwrap(),
                    "source metadata witness {index}"
                );
                if let Ok(output) = result {
                    assert_eq!(output, row["output"], "source metadata witness {index}");
                }
                continue;
            }
            let input: ServerSettings = serde_json::from_value(row["input"].clone()).unwrap();
            let output = match row["op"].as_str().unwrap() {
                "restore" => restore_used_providers(
                    &input,
                    &row["persisted"],
                    &serde_json::from_value::<Vec<ProviderHistoryRow>>(row["history"].clone())
                        .unwrap(),
                ),
                "fold" => fold_legacy_projects(
                    &input,
                    &serde_json::from_value::<Vec<LegacyProjectRow>>(row["projects"].clone())
                        .unwrap(),
                ),
                _ => unreachable!(),
            }
            .unwrap();
            assert_eq!(
                serde_json::to_value(output).unwrap(),
                row["output"],
                "source load migration witness {index}"
            );
        }
    }
}
