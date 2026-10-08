//! Explicit sign-in confirmation, never agent credentials or discovery success.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use t3_contracts::{
    AcpRegistrySettings, AcpRegistrySettingsSource, ProviderInstanceEnvironmentVariable,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Saved {
    binding: String,
    authenticated: bool,
}

fn hash(value: &Value) -> String {
    let digest = Sha256::digest(serde_json::to_vec(value).expect("JSON value serialization"));
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Input {
        instance_id: String,
        settings: AcpRegistrySettings,
        environment: Vec<ProviderInstanceEnvironmentVariable>,
        process_environment: HashMap<String, String>,
    }
    async fn open(root: &std::path::Path, input: &Input) -> AuthenticationState {
        AuthenticationState::open(
            root.to_owned(),
            &input.instance_id,
            &input.settings,
            &input.environment,
            &input.process_environment,
        )
        .await
    }
    fn saved(root: &std::path::Path) -> Vec<Value> {
        let mut values:Vec<Value> = std::fs::read_dir(root).unwrap().map(|entry| {
            let entry = entry.unwrap();
            let saved:Saved = serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
            json!({"name":entry.file_name().to_string_lossy(),"binding":saved.binding,"authenticated":saved.authenticated})
        }).collect();
        values.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        values
    }
    #[tokio::test]
    async fn actual_source_hashes_and_confirmations_survive_cosmetic_rebuilds_and_invalidate_credentials()
     {
        for line in include_str!("../tests/fixtures/acp-authentication-state.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let base: Input = serde_json::from_value(row["base"].clone()).unwrap();
            let input: Input = serde_json::from_value(row["input"].clone()).unwrap();
            let root = tempfile::tempdir().unwrap();
            let first = open(root.path(), &base).await;
            assert_eq!(
                json!(first.get()),
                row["output"]["initial"],
                "{}",
                row["label"]
            );
            first.set(true).await;
            assert_eq!(
                saved(root.path())[0],
                row["output"]["before"],
                "{}",
                row["label"]
            );
            let changed = open(root.path(), &input).await;
            assert_eq!(
                json!(changed.get()),
                row["output"]["confirmed"],
                "{}",
                row["label"]
            );
            changed.set(true).await;
            let mut expected: Vec<Value> =
                serde_json::from_value(row["output"]["after"].clone()).unwrap();
            expected.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            assert_eq!(saved(root.path()), expected, "{}", row["label"]);
            assert_eq!(
                json!(open(root.path(), &base).await.get()),
                row["output"]["restored"],
                "{}",
                row["label"]
            );
            for entry in std::fs::read_dir(root.path()).unwrap() {
                let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
                for secret in ["test-only-secret", "different-secret", "second-secret"] {
                    assert!(!text.contains(secret));
                }
            }
        }
    }
    #[tokio::test]
    async fn malformed_records_and_persistence_failures_never_revoke_in_memory_success() {
        let root = tempfile::tempdir().unwrap();
        let settings = AcpRegistrySettings::default();
        let environment = HashMap::new();
        let state =
            AuthenticationState::open(root.path().into(), "local", &settings, &[], &environment)
                .await;
        std::fs::write(&state.path, b"invalid").unwrap();
        assert!(
            !AuthenticationState::open(root.path().into(), "local", &settings, &[], &environment)
                .await
                .get()
        );
        std::fs::remove_file(&state.path).unwrap();
        std::fs::create_dir(&state.path).unwrap();
        state.set(true).await;
        assert!(
            state.get(),
            "saving confirmation is warning-only, not credential failure"
        );
        state.set(false).await;
        assert!(!state.get());
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "temporary files removed on failed rename"
        );
    }
}

/// The property order is intentional: source hashes JSON.stringify, including
/// local command arguments and the complete configured environment records.
pub(crate) fn binding(
    settings: &AcpRegistrySettings,
    environment: &[ProviderInstanceEnvironmentVariable],
    process_environment: &HashMap<String, String>,
) -> String {
    let mut payload = serde_json::Map::new();
    if settings.source == AcpRegistrySettingsSource::Local {
        payload.insert("source".into(), json!("local"));
        payload.insert("commandArgs".into(), json!(settings.command_args));
    }
    payload.insert("agentId".into(), json!(settings.agent_id));
    payload.insert("commandPath".into(), json!(settings.command_path));
    payload.insert("distribution".into(), json!(settings.distribution));
    payload.insert("authMethodId".into(), json!(settings.auth_method_id));
    let mut environment = environment.to_vec();
    environment.sort_by(|a, b| crate::workspace_entries::collate(a.name.as_str(), b.name.as_str()));
    payload.insert("environment".into(), json!(environment));
    payload.insert(
        "profiles".into(),
        json!(
            [
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_STATE_HOME",
                "APPDATA",
                "LOCALAPPDATA"
            ]
            .map(|name| process_environment.get(name))
        ),
    );
    hash(&Value::Object(payload))
}

#[derive(Clone, Debug)]
pub struct AuthenticationState {
    path: PathBuf,
    binding: String,
    confirmed: Arc<Mutex<bool>>,
    writer: Arc<tokio::sync::Mutex<()>>,
}
impl AuthenticationState {
    pub async fn open(
        cache_dir: PathBuf,
        instance_id: &str,
        settings: &AcpRegistrySettings,
        environment: &[ProviderInstanceEnvironmentVariable],
        process_environment: &HashMap<String, String>,
    ) -> Self {
        let path = cache_dir.join(format!("acp-auth-{}.json", hash(&json!(instance_id))));
        let binding = binding(settings, environment, process_environment);
        let saved = tokio::fs::read(&path)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok());
        let confirmed = saved
            .as_ref()
            .is_some_and(|saved| saved.binding == binding && saved.authenticated);
        let state = Self {
            path,
            binding,
            confirmed: Arc::new(Mutex::new(confirmed)),
            writer: Arc::new(tokio::sync::Mutex::new(())),
        };
        if saved.is_some_and(|saved| saved.binding != state.binding) {
            state.set(false).await;
        }
        state
    }
    pub fn get(&self) -> bool {
        *self.confirmed.lock().unwrap()
    }

    /// Keep the writer and temporary-file guard owned until the filesystem
    /// operation completes even when its caller no longer awaits the result.
    pub async fn set(&self, authenticated: bool) {
        let state = self.clone();
        let job = tokio::spawn(async move {
            let _writer = state.writer.lock().await;
            *state.confirmed.lock().unwrap() = authenticated;
            let saved = Saved {
                binding: state.binding,
                authenticated,
            };
            let path = state.path;
            let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                struct Temporary(PathBuf);
                impl Drop for Temporary {
                    fn drop(&mut self) {
                        let _ = std::fs::remove_file(&self.0);
                    }
                }
                let parent = path
                    .parent()
                    .ok_or_else(|| std::io::Error::other("missing confirmation parent"))?;
                std::fs::create_dir_all(parent)?;
                let temporary =
                    Temporary(parent.join(format!(".acp-auth-{}.tmp", uuid::Uuid::new_v4())));
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary.0)?;
                use std::io::Write;
                file.write_all(&serde_json::to_vec(&saved)?)?;
                drop(file);
                std::fs::rename(&temporary.0, path)?;
                Ok(())
            })
            .await;
            if !matches!(result, Ok(Ok(()))) {
                // Native causes can contain credential paths; match the source
                // safe warning rather than logging error details or bindings.
                tracing::warn!("Could not save ACP sign-in confirmation.");
            }
        });
        let _ = job.await;
    }
}
