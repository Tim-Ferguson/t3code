//! Pure settings decisions. The service owns disk, secrets and publication.
use crate::background_settings;
use serde_json::{Map, Value, json};
use t3_contracts::{ServerSettings, ServerSettingsPatch};
pub fn resolve_text_generation(mut settings: ServerSettings) -> ServerSettings {
    let selection = &settings.text_generation_model_selection;
    let encoded = serde_json::to_value(&settings).expect("typed settings");
    let enabled = settings
        .provider_instances
        .get(&selection.instance_id)
        .map(t3_contracts::resolve_provider_instance_enabled)
        .unwrap_or_else(|| encoded["providers"][selection.instance_id.as_str()]["enabled"] == true);
    let supports = settings
        .provider_instances
        .get(&selection.instance_id)
        .is_none_or(|instance| instance.driver.as_str() != "acpRegistry");
    if enabled && supports {
        return settings;
    }
    // Source fallback follows the fixed declaration order of legacy settings,
    // even when a custom instance elsewhere in the map is enabled.
    for (driver, model) in [
        ("codex", "gpt-6-luna"),
        ("claudeAgent", "claude-haiku-4-5"),
        ("cursor", "composer-2"),
        ("grok", "grok-build"),
        ("pi", "default"),
        ("opencode", "openai/gpt-5"),
        ("antigravity", "antigravity-default"),
    ] {
        let id: t3_contracts::ProviderInstanceId = driver.parse().expect("builtin instance id");
        let enabled = settings
            .provider_instances
            .get(&id)
            .map(t3_contracts::resolve_provider_instance_enabled)
            .unwrap_or_else(|| encoded["providers"][driver]["enabled"] == true);
        if enabled {
            settings.text_generation_model_selection =
                serde_json::from_value(json!({"instanceId":driver,"model":model}))
                    .expect("source fallback model");
            break;
        }
    }
    settings
}
fn merge(current: &Value, patch: &Value) -> Value {
    match (current.as_object(), patch.as_object()) {
        (Some(current), Some(patch)) => {
            let mut next = current.clone();
            for (key, value) in patch {
                next.insert(
                    key.clone(),
                    next.get(key)
                        .map(|current| merge(current, value))
                        .unwrap_or_else(|| value.clone()),
                );
            }
            Value::Object(next)
        }
        _ => patch.clone(),
    }
}
fn merge_entries(current: &Value, patch: &Value) -> Value {
    let mut next = current.as_object().cloned().unwrap_or_default();
    for (key, value) in patch.as_object().unwrap() {
        if value.is_null() {
            next.remove(key);
        } else {
            next.insert(key.clone(), value.clone());
        }
    }
    Value::Object(next)
}
pub fn derive_legacy_project_overrides(settings: &mut Value) {
    for (legacy, canonical) in [
        (
            "projectAgentBrowserAccessOverrides",
            "enableAgentBrowserAccess",
        ),
        ("projectAutoPullOverrides", "defaultAutoPull"),
        ("projectScriptOverrides", "defaultProjectScripts"),
    ] {
        let mut entries = Map::new();
        for (id, entry) in settings["projectSettingsOverrides"].as_object().unwrap() {
            if let Some(value) = entry.get(canonical) {
                entries.insert(id.clone(), value.clone());
            }
        }
        settings[legacy] = Value::Object(entries);
    }
}
fn translate_legacy_patch(current: &Value, patch: &Value) -> Value {
    let mut patch = patch.clone();
    let canonical = patch
        .get("projectSettingsOverrides")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut entries = canonical.clone();
    let mut changed = false;
    for (legacy, key) in [
        (
            "projectAgentBrowserAccessOverrides",
            "enableAgentBrowserAccess",
        ),
        ("projectAutoPullOverrides", "defaultAutoPull"),
        ("projectScriptOverrides", "defaultProjectScripts"),
    ] {
        if let Some(map) = patch.as_object_mut().unwrap().remove(legacy) {
            changed = true;
            for (id, value) in map.as_object().unwrap() {
                if canonical.contains_key(id) {
                    continue;
                }
                let mut entry = entries
                    .get(id)
                    .filter(|value| !value.is_null())
                    .or_else(|| current["projectSettingsOverrides"].get(id))
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                if value.is_null() {
                    entry.remove(key);
                } else {
                    entry.insert(key.into(), value.clone());
                }
                entries.insert(
                    id.clone(),
                    if entry.is_empty() {
                        Value::Null
                    } else {
                        Value::Object(entry)
                    },
                );
            }
        }
    }
    if changed {
        patch["projectSettingsOverrides"] = Value::Object(entries);
    }
    patch
}
pub fn apply_patch(
    current: &ServerSettings,
    patch: &ServerSettingsPatch,
) -> Result<ServerSettings, serde_json::Error> {
    let empty = json!({});
    let current = serde_json::to_value(current)?;
    let patch = translate_legacy_patch(&current, &serde_json::to_value(patch)?);
    let mut merge_patch = patch.clone();
    for field in [
        "automaticGitFetchInterval",
        "providerHealthRefreshInterval",
        "backgroundActivityProfile",
        "backgroundActivity",
        "worktreeCleanup",
        "usageLimitSources",
        "usagePriceOverrides",
        "usageModelAliases",
        "projectSettingsOverrides",
    ] {
        merge_patch.as_object_mut().unwrap().remove(field);
    }
    let mut next = merge(&current, &merge_patch);
    if let Some(cleanup) = patch.get("worktreeCleanup") {
        next["worktreeCleanup"] = if cleanup["mode"] == "custom" {
            let mut rules = Map::new();
            for key in [
                "worktreeAfterDays",
                "worktreeOnMerge",
                "worktreeOnDelete",
                "worktreeUnchanged",
            ] {
                rules.insert(key.into(), next["storageCleanup"][key].clone());
            }
            let rules = merge(
                &Value::Object(rules),
                current["worktreeCleanup"].get("rules").unwrap_or(&empty),
            );
            let rules = merge(&rules, &cleanup["rules"]);
            json!({"mode":"custom","rules":rules})
        } else {
            cleanup.clone()
        };
    }
    let background = background_settings::normalize_server(&current);
    next["backgroundActivity"] = if let Some(patch) = patch.get("backgroundActivity") {
        let mut result = merge(&background, patch);
        if let Some(overrides) = patch.get("overrides") {
            result["overrides"] = overrides.clone();
        }
        result
    } else {
        background.clone()
    };
    if patch.get("backgroundActivity").is_none() {
        let mut overrides = Map::new();
        for field in ["automaticGitFetchInterval", "providerHealthRefreshInterval"] {
            if let Some(value) = patch.get(field) {
                overrides.insert(field.into(), value.clone());
            }
        }
        if let Some(profile) = patch.get("backgroundActivityProfile") {
            next["backgroundActivity"] = if overrides.is_empty() {
                json!({"schemaVersion":1,"profile":profile,"overrides":{}})
            } else {
                json!({"schemaVersion":1,"profile":"custom","baseProfile":profile,"overrides":overrides})
            };
        } else if !overrides.is_empty() {
            let overrides = merge(
                if background["profile"] == "custom" {
                    &background["overrides"]
                } else {
                    &empty
                },
                &Value::Object(overrides),
            );
            next["backgroundActivity"] = json!({"schemaVersion":1,"profile":"custom","baseProfile":background_settings::base_profile(&background),"overrides":overrides});
        }
    }
    if let Some(instances) = patch.get("providerInstances") {
        next["providerInstances"] = instances.clone();
    }
    if let Some(directory) = patch
        .get("worktreesDirectory")
        .filter(|value| *value != &current["worktreesDirectory"])
    {
        let mut previous = current["previousWorktreesDirectories"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|value| *value != directory)
            .cloned()
            .collect::<Vec<_>>();
        if current["worktreesDirectory"] != ""
            && !current["previousWorktreesDirectories"]
                .as_array()
                .unwrap()
                .contains(&current["worktreesDirectory"])
        {
            previous.push(current["worktreesDirectory"].clone());
        }
        next["previousWorktreesDirectories"] = Value::Array(previous);
    }
    if let Some(hosts) = patch["github"].get("hosts") {
        next["github"]["hosts"] = hosts.clone();
    }
    for field in [
        "projectSettingsOverrides",
        "usageLimitSources",
        "usagePriceOverrides",
        "usageModelAliases",
    ] {
        if let Some(patch) = patch.get(field) {
            let mut entries = merge_entries(&current[field], patch);
            if field == "projectSettingsOverrides" {
                entries
                    .as_object_mut()
                    .unwrap()
                    .retain(|_, entry| !entry.as_object().unwrap().is_empty());
            }
            next[field] = entries;
        }
    }
    for field in [
        "defaultModelSelection",
        "defaultProjectScripts",
        "sourceControlWriterModelSelection",
    ] {
        if let Some(value) = patch.get(field) {
            next[field] = value.clone();
        }
    }
    derive_legacy_project_overrides(&mut next);
    next["backgroundActivity"] = background_settings::normalize(&next["backgroundActivity"]);
    let resolved = background_settings::resolve(&next["backgroundActivity"]);
    for field in ["automaticGitFetchInterval", "providerHealthRefreshInterval"] {
        next[field] = resolved[field].clone();
    }
    next["backgroundActivityProfile"] = resolved["profile"].clone();
    if let Some(selection) = patch.get("textGenerationModelSelection") {
        let original = &current["textGenerationModelSelection"];
        let instance = selection
            .get("instanceId")
            .unwrap_or(&original["instanceId"]);
        let model = selection.get("model").unwrap_or(&original["model"]);
        let options = if selection.get("instanceId").is_some() || selection.get("model").is_some() {
            selection.get("options").cloned()
        } else {
            match selection.get("options") {
                None => original.get("options").cloned(),
                Some(options) if options.as_array().unwrap().is_empty() => None,
                Some(options) => {
                    let mut merged = original
                        .get("options")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    for option in options.as_array().unwrap() {
                        if let Some(entry) =
                            merged.iter_mut().find(|entry| entry["id"] == option["id"])
                        {
                            *entry = option.clone();
                        } else {
                            merged.push(option.clone());
                        }
                    }
                    Some(Value::Array(merged))
                }
            }
        };
        let mut result = json!({"instanceId":instance,"model":model});
        if let Some(options) = options.filter(|options| !options.as_array().unwrap().is_empty()) {
            result["options"] = options;
        }
        next["textGenerationModelSelection"] = result;
    }
    serde_json::from_value(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patches_match_original_shared_service_decisions() {
        let mut count = 0;
        for row in include_str!("../tests/fixtures/server-settings-model.jsonl").lines() {
            let row: Value = serde_json::from_str(row).unwrap();
            let input: ServerSettings = serde_json::from_value(row["input"].clone()).unwrap();
            match row["op"].as_str().unwrap() {
                "patch" => {
                    let patch: ServerSettingsPatch =
                        serde_json::from_value(row["patch"].clone()).unwrap();
                    let result = apply_patch(&input, &patch);
                    assert_eq!(result.is_ok(), row["valid"].as_bool().unwrap(), "{row}");
                    if row["valid"] == true {
                        assert_eq!(
                            serde_json::to_value(result.unwrap()).unwrap(),
                            row["output"],
                            "patch={} input={}",
                            row["patch"],
                            row["input"]
                        );
                    }
                }
                "normalize" => assert_eq!(
                    serde_json::to_value(normalize_settings(&input).unwrap()).unwrap(),
                    row["output"],
                    "{row}"
                ),
                "sparse" => assert_eq!(sparse_settings(&input).unwrap(), row["output"], "{row}"),
                _ => unreachable!(),
            }
            count += 1;
        }
        assert_eq!(count, 320);
    }
}

pub fn normalize_settings(settings: &ServerSettings) -> Result<ServerSettings, serde_json::Error> {
    // Encode/decode performs the same shared contract normalization before the
    // service folds enabled flags and derives the old project views.
    let mut value = serde_json::to_value(serde_json::from_value::<ServerSettings>(
        serde_json::to_value(settings)?,
    )?)?;
    for (_, instance) in value["providerInstances"].as_object_mut().unwrap() {
        let flag = instance
            .get("config")
            .and_then(|config| config.get("enabled"))
            .and_then(Value::as_bool);
        if let Some(flag) = flag {
            let enabled = instance.get("enabled").and_then(Value::as_bool);
            instance["enabled"] = json!(enabled != Some(false) && flag);
            instance["config"]
                .as_object_mut()
                .unwrap()
                .remove("enabled");
        }
    }
    derive_legacy_project_overrides(&mut value);
    serde_json::from_value(value)
}
const ATOMIC_KEYS: [&str; 6] = [
    "backgroundActivity",
    "automaticGitFetchInterval",
    "providerHealthRefreshInterval",
    "sourceControlWriterModelSelection",
    "textGenerationModelSelection",
    "pullRequestMergeMethod",
];
fn equal_json(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| equal_json(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right.get(key).is_some_and(|right| equal_json(value, right))
                })
        }
        _ => left == right,
    }
}
fn strip(current: &Value, defaults: Option<&Value>) -> Option<Value> {
    if let (Some(current), Some(defaults)) =
        (current.as_object(), defaults.and_then(Value::as_object))
    {
        let mut output = Map::new();
        for (key, value) in current {
            let default = defaults.get(key);
            if ATOMIC_KEYS.contains(&key.as_str()) {
                if !default.is_some_and(|default| equal_json(value, default)) {
                    output.insert(key.clone(), value.clone());
                }
            } else if let Some(value) = strip(value, default) {
                output.insert(key.clone(), value);
            }
        }
        return (!output.is_empty()).then_some(Value::Object(output));
    }
    if defaults.is_some_and(|default| equal_json(current, default)) {
        None
    } else {
        Some(current.clone())
    }
}
pub fn sparse_settings(settings: &ServerSettings) -> Result<Value, serde_json::Error> {
    let mut defaults = serde_json::to_value(ServerSettings::default())?;
    for driver in ["cursor", "grok", "opencode"] {
        defaults["providers"][driver]
            .as_object_mut()
            .unwrap()
            .remove("enabled");
    }
    Ok(strip(&serde_json::to_value(settings)?, Some(&defaults)).unwrap_or(json!({})))
}
