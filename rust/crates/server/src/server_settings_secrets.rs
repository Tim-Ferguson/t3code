//! Pure secret decisions and scoped rollback for settings persistence.
use crate::server_secret_store::{SecretStoreError, ServerSecretStore};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};
use t3_contracts::{ServerSettings, trim_wire_string};
pub const REDACTED: &str = "••••••";
pub trait SecretBackend: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretStoreError>;
    fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretStoreError>;
    fn remove(&self, name: &str) -> Result<(), SecretStoreError>;
}
impl SecretBackend for ServerSecretStore {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
        self.get(name)
    }
    fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretStoreError> {
        self.set(name, value)
    }
    fn remove(&self, name: &str) -> Result<(), SecretStoreError> {
        self.remove(name)
    }
}
#[derive(Debug, thiserror::Error)]
#[error("Settings secret operation {operation} failed.")]
pub struct SettingsSecretError {
    pub operation: &'static str,
    pub provider_instance_id: Option<String>,
    pub environment_variable: Option<String>,
    #[source]
    pub cause: SecretStoreError,
}
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ChangeKind {
    Write { value: Vec<u8> },
    Remove { operation: &'static str },
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretChange {
    pub secret_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_variable: Option<String>,
    #[serde(flatten)]
    pub kind: ChangeKind,
}
impl SecretChange {
    fn new(
        name: String,
        value: Option<&str>,
        operation: &'static str,
        instance: Option<&str>,
        variable: Option<&str>,
    ) -> Self {
        Self {
            secret_name: name,
            provider_instance_id: instance.map(str::to_owned),
            environment_variable: variable.map(str::to_owned),
            kind: value
                .map(|value| ChangeKind::Write {
                    value: value.as_bytes().to_vec(),
                })
                .unwrap_or(ChangeKind::Remove { operation }),
        }
    }
    fn error(&self, operation: &'static str, cause: SecretStoreError) -> SettingsSecretError {
        SettingsSecretError {
            operation,
            provider_instance_id: self.provider_instance_id.clone(),
            environment_variable: self.environment_variable.clone(),
            cause,
        }
    }
}
pub fn provider_secret_name(instance: &str, name: &str) -> String {
    format!(
        "provider-env-{}-{}",
        URL_SAFE_NO_PAD.encode(instance.as_bytes()),
        URL_SAFE_NO_PAD.encode(name.as_bytes())
    )
}
pub fn usage_secret_name(id: &str) -> String {
    format!(
        "usage-limit-source-{}",
        URL_SAFE_NO_PAD.encode(id.as_bytes())
    )
}
pub fn github_secret_name(host: &str) -> String {
    format!(
        "github-token-{}",
        URL_SAFE_NO_PAD.encode(trim_wire_string(host).to_lowercase().as_bytes())
    )
}
const BITBUCKET_FIELDS: [(&str, &str); 2] = [
    ("accessToken", "bitbucket-access-token"),
    ("apiToken", "bitbucket-api-token"),
];
fn redact_variable(variable: &Value) -> Value {
    let mut result = variable.clone();
    if variable["sensitive"] != true {
        result.as_object_mut().unwrap().remove("valueRedacted");
    } else {
        result["value"] = json!("");
        if !variable["value"].as_str().unwrap().is_empty() || variable["valueRedacted"] == true {
            result["valueRedacted"] = json!(true);
        }
    }
    result
}
pub fn plan(
    current: &ServerSettings,
    next: &ServerSettings,
) -> Result<(ServerSettings, Vec<SecretChange>), serde_json::Error> {
    let current = serde_json::to_value(current)?;
    let mut next = serde_json::to_value(next)?;
    let mut changes = Vec::new();
    let mut keys = HashSet::new();
    for (id, instance) in next["providerInstances"].as_object_mut().unwrap() {
        let Some(environment) = instance
            .get_mut("environment")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for variable in environment {
            let name = variable["name"].as_str().unwrap().to_owned();
            let secret = provider_secret_name(id, &name);
            if variable["sensitive"] != true {
                changes.push(SecretChange::new(
                    secret,
                    None,
                    "remove-secret",
                    Some(id),
                    Some(&name),
                ));
                *variable = redact_variable(variable);
                continue;
            }
            keys.insert(secret.clone());
            let previous = if variable["valueRedacted"] == true {
                current["providerInstances"][id]
                    .get("environment")
                    .and_then(Value::as_array)
                    .and_then(|environment| {
                        environment.iter().rev().find(|entry| entry["name"] == name)
                    })
            } else {
                None
            };
            let inline = previous
                .filter(|previous| {
                    previous["sensitive"] == true && previous["valueRedacted"] != true
                })
                .and_then(|previous| previous["value"].as_str())
                .filter(|value| !value.is_empty());
            if variable["valueRedacted"] != true || inline.is_some() {
                let value = inline.unwrap_or(variable["value"].as_str().unwrap());
                let write = (!value.is_empty()).then_some(value);
                changes.push(SecretChange::new(
                    secret,
                    write,
                    "remove-secret",
                    Some(id),
                    Some(&name),
                ));
                if write.is_some() {
                    variable["value"] = json!("");
                    variable["valueRedacted"] = json!(true);
                } else {
                    variable.as_object_mut().unwrap().remove("valueRedacted");
                }
            } else {
                *variable = redact_variable(variable);
            }
        }
    }
    for (id, instance) in current["providerInstances"].as_object().unwrap() {
        for variable in instance
            .get("environment")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if variable["sensitive"] != true {
                continue;
            }
            let name = variable["name"].as_str().unwrap();
            let secret = provider_secret_name(id, name);
            if !keys.contains(&secret) {
                changes.push(SecretChange::new(
                    secret,
                    None,
                    "remove-stale-secret",
                    Some(id),
                    Some(name),
                ));
            }
        }
    }
    for (id, source) in next["usageLimitSources"].as_object_mut().unwrap() {
        let value = source["managementKey"].as_str().unwrap();
        if value == REDACTED {
            continue;
        }
        let write = (!value.is_empty()).then_some(value);
        changes.push(SecretChange::new(
            usage_secret_name(id),
            write,
            "remove-secret",
            None,
            None,
        ));
        if write.is_some() {
            source["managementKey"] = json!(REDACTED);
        }
    }
    for id in current["usageLimitSources"].as_object().unwrap().keys() {
        if !next["usageLimitSources"]
            .as_object()
            .unwrap()
            .contains_key(id)
        {
            changes.push(SecretChange::new(
                usage_secret_name(id),
                None,
                "remove-stale-secret",
                None,
                None,
            ));
        }
    }
    for (field, name) in BITBUCKET_FIELDS {
        let mut value = next["bitbucket"][field].as_str().unwrap();
        if value == REDACTED {
            let inline = current["bitbucket"][field].as_str().unwrap();
            if inline == REDACTED || inline.is_empty() {
                continue;
            }
            value = inline;
        }
        let write = (!value.is_empty()).then_some(value);
        changes.push(SecretChange::new(
            name.into(),
            write,
            "remove-secret",
            None,
            None,
        ));
        if write.is_some() {
            next["bitbucket"][field] = json!(REDACTED);
        }
    }
    let mut tokens = serde_json::Map::new();
    for (raw, value) in next["github"]["tokens"].as_object().unwrap() {
        let host = trim_wire_string(raw).to_lowercase();
        let mut value = value.as_str().unwrap();
        if value == REDACTED {
            let inline = current["github"]["tokens"]
                .get(&host)
                .and_then(Value::as_str);
            if inline.is_none_or(|value| value == REDACTED || value.is_empty()) {
                tokens.insert(host, json!(REDACTED));
                continue;
            }
            value = inline.unwrap();
        }
        let write = (!value.is_empty()).then_some(value);
        changes.push(SecretChange::new(
            github_secret_name(&host),
            write,
            "remove-secret",
            None,
            None,
        ));
        if write.is_some() {
            tokens.insert(host, json!(REDACTED));
        }
    }
    let next_hosts = next["github"]["tokens"]
        .as_object()
        .unwrap()
        .keys()
        .map(|host| trim_wire_string(host).to_lowercase())
        .collect::<HashSet<_>>();
    for host in current["github"]["tokens"].as_object().unwrap().keys() {
        if !next_hosts.contains(&trim_wire_string(host).to_lowercase()) {
            changes.push(SecretChange::new(
                github_secret_name(host),
                None,
                "remove-stale-secret",
                None,
                None,
            ));
        }
    }
    next["github"]["tokens"] = Value::Object(tokens);
    Ok((serde_json::from_value(next)?, changes))
}
fn secret_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .strip_prefix('\u{feff}')
        .unwrap_or(&String::from_utf8_lossy(bytes))
        .to_owned()
}
pub fn materialize(
    settings: &ServerSettings,
    store: &dyn SecretBackend,
) -> Result<ServerSettings, MaterializeError> {
    let mut value = serde_json::to_value(settings)?;
    let read = |name: String,
                instance: Option<&str>,
                variable: Option<&str>|
     -> Result<String, SettingsSecretError> {
        store
            .get(&name)
            .map(|bytes| bytes.map(|bytes| secret_text(&bytes)).unwrap_or_default())
            .map_err(|cause| SettingsSecretError {
                operation: "read-secret",
                provider_instance_id: instance.map(str::to_owned),
                environment_variable: variable.map(str::to_owned),
                cause,
            })
    };
    for (id, instance) in value["providerInstances"].as_object_mut().unwrap() {
        for variable in instance
            .get_mut("environment")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if variable["sensitive"] == true && variable["valueRedacted"] == true {
                let name = variable["name"].as_str().unwrap();
                variable["value"] =
                    json!(read(provider_secret_name(id, name), Some(id), Some(name))?);
            }
        }
    }
    for (id, source) in value["usageLimitSources"].as_object_mut().unwrap() {
        if source["managementKey"] == REDACTED {
            source["managementKey"] = json!(read(usage_secret_name(id), None, None)?);
        }
    }
    for (field, name) in BITBUCKET_FIELDS {
        if value["bitbucket"][field] == REDACTED {
            value["bitbucket"][field] = json!(read(name.into(), None, None)?);
        }
    }
    for (host, token) in value["github"]["tokens"].as_object_mut().unwrap() {
        if token == REDACTED {
            *token = json!(read(github_secret_name(host), None, None)?);
        }
    }
    Ok(serde_json::from_value(value)?)
}
#[derive(Debug, thiserror::Error)]
pub enum MaterializeError {
    #[error(transparent)]
    Secret(#[from] SettingsSecretError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
/// Remains armed until the settings file lands and cache/publication is committed.
/// The failing operation is recorded before it runs: it may mutate then report
/// failure (e.g. chmod after rename), and must be rolled back as well.
pub struct AppliedSecrets {
    backend: Arc<dyn SecretBackend>,
    applied: Vec<(SecretChange, Option<Vec<u8>>)>,
    armed: bool,
}
impl AppliedSecrets {
    pub fn apply(
        backend: Arc<dyn SecretBackend>,
        changes: Vec<SecretChange>,
    ) -> Result<Self, SettingsSecretError> {
        let mut guard = Self {
            backend,
            applied: Vec::new(),
            armed: true,
        };
        for change in changes {
            let previous = guard
                .backend
                .get(&change.secret_name)
                .map_err(|cause| change.error("read-secret", cause))?;
            guard.applied.push((change, previous));
            let change = &guard.applied.last().unwrap().0;
            let (operation, result) = match &change.kind {
                ChangeKind::Write { value } => (
                    "write-secret",
                    guard.backend.set(&change.secret_name, value),
                ),
                ChangeKind::Remove { operation } => {
                    (*operation, guard.backend.remove(&change.secret_name))
                }
            };
            result.map_err(|cause| change.error(operation, cause))?;
        }
        Ok(guard)
    }
    pub fn commit(mut self) {
        self.armed = false;
    }
}
impl Drop for AppliedSecrets {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for (change, previous) in self.applied.iter().rev() {
            let result = match previous {
                Some(value) => self.backend.set(&change.secret_name, value),
                None => self.backend.remove(&change.secret_name),
            };
            if let Err(cause) = result {
                tracing::warn!(provider_instance_id=?change.provider_instance_id,environment_variable=?change.environment_variable,error=%cause,"failed to roll back provider environment secret");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_secret_store::Operation;
    use std::{
        collections::BTreeMap,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };
    #[derive(Default)]
    struct Store {
        values: Mutex<BTreeMap<String, Vec<u8>>>,
        fail: AtomicBool,
    }
    impl SecretBackend for Store {
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
            Ok(self.values.lock().unwrap().get(name).cloned())
        }
        fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretStoreError> {
            self.values
                .lock()
                .unwrap()
                .insert(name.into(), value.into());
            if name == "second" && self.fail.swap(false, Ordering::SeqCst) {
                return Err(SecretStoreError {
                    operation: Operation::Persist,
                    resource: "secret second".into(),
                    cause: Some(std::io::Error::other("chmod failed after rename")),
                });
            }
            Ok(())
        }
        fn remove(&self, name: &str) -> Result<(), SecretStoreError> {
            self.values.lock().unwrap().remove(name);
            Ok(())
        }
    }
    #[test]
    fn original_secret_and_jsonc_source_witnesses() {
        for (index, line) in include_str!("../tests/fixtures/server-settings-secrets.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            match row["op"].as_str().unwrap() {
                "fallback" => {
                    let input: ServerSettings =
                        serde_json::from_value(row["input"].clone()).unwrap();
                    let output = crate::server_settings_model::resolve_text_generation(input);
                    assert_eq!(
                        serde_json::to_value(output).unwrap(),
                        row["output"],
                        "fallback witness {index}"
                    );
                }
                "plan" => {
                    let current: ServerSettings =
                        serde_json::from_value(row["current"].clone()).unwrap();
                    let next: ServerSettings = serde_json::from_value(row["next"].clone()).unwrap();
                    let (settings, changes) = plan(&current, &next).unwrap();
                    assert_eq!(
                        serde_json::to_value(settings).unwrap(),
                        row["settings"],
                        "settings witness {index}"
                    );
                    assert_eq!(
                        serde_json::to_value(changes).unwrap(),
                        row["changes"],
                        "changes witness {index}"
                    );
                }
                "materialize" => {
                    let settings: ServerSettings =
                        serde_json::from_value(row["input"].clone()).unwrap();
                    struct Single(Option<Vec<u8>>);
                    impl SecretBackend for Single {
                        fn get(&self, _: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
                            Ok(self.0.clone())
                        }
                        fn set(&self, _: &str, _: &[u8]) -> Result<(), SecretStoreError> {
                            unreachable!()
                        }
                        fn remove(&self, _: &str) -> Result<(), SecretStoreError> {
                            unreachable!()
                        }
                    }
                    let backend = Single(
                        row.get("bytes")
                            .map(|bytes| serde_json::from_value(bytes.clone()).unwrap()),
                    );
                    let output = materialize(&settings, &backend).unwrap();
                    assert_eq!(
                        serde_json::to_value(output).unwrap(),
                        row["output"],
                        "materialize witness {index}"
                    );
                }
                "decode" => {
                    let output = crate::server_settings::decode_settings(
                        row["input"].as_str().unwrap().as_bytes(),
                    );
                    assert_eq!(
                        output.is_ok(),
                        row["valid"].as_bool().unwrap(),
                        "decode acceptance witness {index}"
                    );
                    if let Ok(output) = output {
                        assert_eq!(
                            serde_json::to_value(output).unwrap(),
                            row["output"],
                            "decode witness {index}"
                        );
                    }
                }
                _ => unreachable!(),
            }
        }
    }
    #[test]
    fn rollback_includes_failing_write_after_mutation_and_reverses_duplicate_writes() {
        let store = Arc::new(Store::default());
        store.set("first", b"original").unwrap();
        store.set("second", b"previous").unwrap();
        store.fail.store(true, Ordering::SeqCst);
        let changes = vec![
            SecretChange::new("first".into(), Some("one"), "remove-secret", None, None),
            SecretChange::new("first".into(), Some("two"), "remove-secret", None, None),
            SecretChange::new(
                "second".into(),
                Some("mutated"),
                "remove-secret",
                None,
                None,
            ),
        ];
        let error = match AppliedSecrets::apply(store.clone(), changes) {
            Ok(_) => panic!("mutation should fail"),
            Err(error) => error,
        };
        assert_eq!(error.operation, "write-secret");
        assert_eq!(store.get("first").unwrap(), Some(b"original".to_vec()));
        assert_eq!(store.get("second").unwrap(), Some(b"previous".to_vec()));
        let guard = AppliedSecrets::apply(
            store.clone(),
            vec![SecretChange::new(
                "first".into(),
                Some("committed"),
                "remove-secret",
                None,
                None,
            )],
        )
        .unwrap();
        guard.commit();
        assert_eq!(store.get("first").unwrap(), Some(b"committed".to_vec()));
        let guard = AppliedSecrets::apply(
            store.clone(),
            vec![SecretChange::new(
                "new".into(),
                Some("temporary"),
                "remove-secret",
                None,
                None,
            )],
        )
        .unwrap();
        drop(guard);
        assert_eq!(store.get("new").unwrap(), None);
    }
    #[test]
    fn typed_plan_keeps_redacted_secret_and_last_inline_duplicate_then_materializes() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(ServerSecretStore::open(root.path()).unwrap());
        let current:ServerSettings=serde_json::from_value(json!({"providerInstances":{"fixture":{"driver":"codex","environment":[{"name":"KEY","value":"old-first","sensitive":true},{"name":"KEY","value":"inline-last","sensitive":true}]}}})).unwrap();
        let next:ServerSettings=serde_json::from_value(json!({"providerInstances":{"fixture":{"driver":"codex","environment":[{"name":"KEY","value":"","sensitive":true,"valueRedacted":true}]}},"github":{"tokens":{"GitHub.com":"actual-token"}},"bitbucket":{"accessToken":"bitbucket-token"}})).unwrap();
        let (persisted, changes) = plan(&current, &next).unwrap();
        let guard = AppliedSecrets::apply(store.clone(), changes).unwrap();
        let materialized = materialize(&persisted, store.as_ref()).unwrap();
        guard.commit();
        let wire = serde_json::to_value(&persisted).unwrap();
        assert!(!wire.to_string().contains("inline-last"));
        assert!(!wire.to_string().contains("actual-token"));
        let materialized = serde_json::to_value(&materialized).unwrap();
        assert_eq!(
            materialized["providerInstances"]["fixture"]["environment"][0]["value"],
            "inline-last"
        );
        assert_eq!(
            materialized["github"]["tokens"]["github.com"],
            "actual-token"
        );
        let unchanged: ServerSettings = serde_json::from_value(wire).unwrap();
        let (unchanged, changes) = plan(&persisted, &unchanged).unwrap();
        let guard = AppliedSecrets::apply(store.clone(), changes).unwrap();
        assert_eq!(
            serde_json::to_value(materialize(&unchanged, store.as_ref()).unwrap()).unwrap()["bitbucket"]
                ["accessToken"],
            "bitbucket-token"
        );
        guard.commit();
        assert_eq!(secret_text(b"\xef\xbb\xbfhello\xff"), "hello�");
    }
}
