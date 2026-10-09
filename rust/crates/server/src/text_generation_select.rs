//! Which provider and model write the text (port of `resolveTextGenerationProvider` in
//! `serverSettings.ts`, `resolveSourceControlWriterModelSelection` in `shared/serverSettings.ts` and
//! the text generation keys of `resolveProjectSettings`), over the settings as JSON.
//!
//! The original falls back only on what settings and provider snapshots say: a chosen provider that
//! is disabled gives way to the first enabled built-in provider, and a dedicated source control
//! writer that is disabled, unavailable or unable to write text gives way to the general choice. A
//! provider that is enabled but not installed or not signed in is not skipped; asking it fails, and
//! the failure is the caller's. Providers this server has no adapter for (Cursor, Grok, Pi,
//! OpenCode, Antigravity, ACP Registry instances) are skipped everywhere as if not installed.
use serde_json::{Value, json};

/// The drivers this server can run a one-shot generation with.
pub const SUPPORTED_DRIVERS: [&str; 2] = ["codex", "claudeAgent"];

fn instance_config(settings: &Value, instance_id: &str) -> Option<t3_contracts::ProviderInstanceConfig> {
    serde_json::from_value(settings["providerInstances"].get(instance_id)?.clone()).ok()
}

/// `isModelSelectionProviderEnabled`: an explicit provider instance wins over the legacy map.
pub fn is_selection_provider_enabled(settings: &Value, selection: &Value) -> bool {
    let id = selection["instanceId"].as_str().unwrap_or("");
    if let Some(instance) = instance_config(settings, id) {
        return t3_contracts::resolve_provider_instance_enabled(&instance);
    }
    is_driver_slug(id) && settings["providers"][id]["enabled"] == true
}

fn is_driver_slug(value: &str) -> bool {
    value.len() <= 64 && value.chars().next().is_some_and(|first| first.is_ascii_alphabetic()) && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The driver an instance id names: its configured driver, or the id itself for a legacy built-in.
pub fn driver_of(settings: &Value, instance_id: &str) -> Option<String> {
    if let Some(driver) = settings["providerInstances"][instance_id]["driver"].as_str() {
        return Some(driver.to_string());
    }
    settings["providers"].get(instance_id).map(|_| instance_id.to_string())
}

fn selection_is_runnable(settings: &Value, selection: &Value) -> bool {
    driver_of(settings, selection["instanceId"].as_str().unwrap_or("")).is_some_and(|driver| SUPPORTED_DRIVERS.contains(&driver.as_str()))
}

/// `resolveTextGenerationProvider`: the chosen provider if it is enabled and can write text here,
/// else the first enabled one in the order of `providers`, with that provider's text model.
pub fn resolve_text_generation_provider(settings: &Value) -> Value {
    let selection = &settings["textGenerationModelSelection"];
    if is_selection_provider_enabled(settings, selection) && selection_is_runnable(settings, selection) {
        return settings.clone();
    }
    let defaults = &crate::text_generation::data()["defaults"];
    let fallback = settings["providers"].as_object().into_iter().flatten().find_map(|(driver, provider)| {
        let enabled = match instance_config(settings, driver) {
            Some(instance) => t3_contracts::resolve_provider_instance_enabled(&instance),
            None => provider["enabled"] == true,
        };
        (enabled && SUPPORTED_DRIVERS.contains(&driver.as_str())).then_some(driver.clone())
    });
    let Some(driver) = fallback else { return settings.clone() };
    let model = defaults["textGenerationModelByProvider"][&driver]
        .as_str()
        .or_else(|| defaults["modelByProvider"][&driver].as_str())
        .or_else(|| defaults["textGenerationModel"].as_str())
        .unwrap_or_default()
        .to_string();
    let mut resolved = settings.clone();
    resolved["textGenerationModelSelection"] = json!({"instanceId":driver,"model":model});
    resolved
}

/// The keys of a project's overrides that steer text generation.
const PROJECT_KEYS: [&str; 6] = ["textGenerationModelSelection", "sourceControlWriterModelSelection", "sourceControlWritingStyle", "branchNamingMode", "branchNamePrefix", "branchNameInstructions"];

/// `resolveProjectSettings` for the keys above. A model override whose provider is disabled on this
/// environment falls back to the environment value.
pub fn resolve_project_settings(settings: &Value, project_id: Option<&str>) -> Value {
    let Some(overrides) = project_id.and_then(|id| settings["projectSettingsOverrides"].get(id)).and_then(Value::as_object) else { return settings.clone() };
    let mut effective = settings.clone();
    for key in PROJECT_KEYS {
        let Some(value) = overrides.get(key) else { continue };
        if key == "textGenerationModelSelection" && !value.is_null() && !is_selection_provider_enabled(settings, value) {
            continue;
        }
        effective[key] = value.clone();
    }
    effective
}

/// `resolveSourceControlWriterModelSelection`: the dedicated writer when it is enabled, available and
/// able to write text, else the general choice.
pub fn source_control_writer_selection(settings: &Value, providers: &[Value]) -> Value {
    let general = settings["textGenerationModelSelection"].clone();
    let selection = &settings["sourceControlWriterModelSelection"];
    if selection.is_null() || !is_selection_provider_enabled(settings, selection) {
        return general;
    }
    let usable = providers.iter().find(|provider| provider["instanceId"] == selection["instanceId"]).is_some_and(|provider| {
        provider["enabled"] == true && provider["availability"] != "unavailable" && provider["supportsTextGeneration"] != false
    });
    if usable { selection.clone() } else { general }
}

/// What the stacked git action writes with: the writer selection when the settings name one, else
/// the general choice (`GitManager.runStackedAction`).
pub fn stacked_action_selection(settings: &Value, providers: &[Value]) -> Value {
    if settings["sourceControlWriterModelSelection"].is_null() {
        settings["textGenerationModelSelection"].clone()
    } else {
        source_control_writer_selection(settings, providers)
    }
}
