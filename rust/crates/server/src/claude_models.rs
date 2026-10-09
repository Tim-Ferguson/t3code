//! Claude model catalog and selection compiler. The catalog is generated from the source
//! `model-manifest.json` by `rust/tools/generate_claude_model_fixtures.mjs`; the functions mirror
//! `ClaudeModelCatalog.ts`, `claudeModelOptions.ts` and the option helpers in `shared/model.ts`.
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

fn catalog() -> &'static Vec<Value> {
    static CATALOG: OnceLock<Vec<Value>> = OnceLock::new();
    CATALOG.get_or_init(|| serde_json::from_str(include_str!("claude-model-catalog.json")).expect("generated Claude catalog"))
}
fn trim_or_null(value: Option<&str>) -> Option<String> {
    value.map(t3_contracts::trim_wire_string).filter(|value| !value.is_empty()).map(str::to_owned)
}
/// A catalog entry for a slug or (case-insensitive) alias.
fn entry<'a>(catalog: &'a [Value], slug_or_alias: &str) -> Option<&'a Value> {
    let value = trim_or_null(Some(slug_or_alias))?;
    catalog
        .iter()
        .find(|entry| entry["model"]["slug"] == value)
        .or_else(|| {
            catalog.iter().find(|entry| {
                entry["model"]["aliases"].as_array().is_some_and(|aliases| aliases.iter().any(|alias| alias.as_str().is_some_and(|alias| alias.to_lowercase() == value.to_lowercase())))
            })
        })
}
fn capabilities(entry: Option<&Value>) -> Vec<Value> {
    entry.and_then(|entry| entry["model"]["capabilities"]["optionDescriptors"].as_array()).cloned().unwrap_or_default()
}
fn selection_value<'a>(selection: &'a Value, id: &str) -> Option<&'a Value> {
    selection["options"].as_array()?.iter().find(|option| option["id"] == id).map(|option| &option["value"])
}
fn choice_value(descriptor: &Value, raw: Option<&str>) -> Option<String> {
    let default = || {
        descriptor["currentValue"].as_str().map(str::to_owned).or_else(|| {
            descriptor["options"].as_array()?.iter().find(|option| option["isDefault"] == true).and_then(|option| option["id"].as_str()).map(str::to_owned)
        })
    };
    let Some(trimmed) = trim_or_null(raw) else { return default() };
    let options = descriptor["options"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if options.is_empty() {
        return Some(trimmed);
    }
    let known = options.iter().any(|option| option["id"] == trimmed.as_str());
    let injected = descriptor["promptInjectedValues"].as_array().is_some_and(|values| values.iter().any(|value| value == trimmed.as_str()));
    if injected && known {
        return options.iter().find(|option| option["isDefault"] == true).and_then(|option| option["id"].as_str()).map(str::to_owned);
    }
    if known { Some(trimmed) } else { default() }
}
/// `getProviderOptionDescriptors`: descriptors with `currentValue` resolved against the selection.
fn descriptors(descriptors: &[Value], selection: Option<&Value>) -> Vec<Value> {
    descriptors
        .iter()
        .map(|descriptor| {
            let mut descriptor = descriptor.clone();
            let id = descriptor["id"].as_str().unwrap_or("").to_owned();
            let raw = selection.and_then(|selection| selection_value(selection, &id)).filter(|value| !value.is_null()).cloned().or_else(|| descriptor.get("currentValue").cloned());
            if descriptor["type"] == "boolean" {
                if let Some(value) = raw.filter(Value::is_boolean) {
                    descriptor["currentValue"] = value;
                }
                return descriptor;
            }
            let resolved = match raw.as_ref().and_then(Value::as_str) {
                Some(text) => choice_value(&descriptor, Some(text)),
                None => choice_value(&descriptor, descriptor["currentValue"].as_str()),
            };
            match resolved {
                Some(value) => descriptor["currentValue"] = json!(value),
                None => {
                    descriptor.as_object_mut().unwrap().remove("currentValue");
                }
            }
            descriptor
        })
        .collect()
}
fn current_value(descriptor: Option<&Value>) -> Option<Value> {
    let descriptor = descriptor?;
    if descriptor["type"] == "boolean" {
        return descriptor.get("currentValue").cloned();
    }
    if let Some(current) = descriptor["currentValue"].as_str().filter(|value| !value.is_empty()) {
        return Some(json!(current));
    }
    descriptor["options"].as_array()?.iter().find(|option| option["isDefault"] == true).map(|option| option["id"].clone())
}
fn string_current(descriptors: &[Value], id: &str) -> Option<String> {
    current_value(descriptors.iter().find(|descriptor| descriptor["id"] == id))?.as_str().map(str::to_owned)
}

/// A selection compiled into what the CLI needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    pub api_model_id: String,
    pub effort: Option<String>,
    pub prompt_effort: Option<String>,
    /// `--settings` entries implied by the selection (`alwaysThinkingEnabled`, `fastMode`, `ultracode`).
    pub settings: Map<String, Value>,
    /// Changes whenever a running CLI cannot serve the selection.
    pub query_identity: String,
}
pub fn compile_selection(selection: &Value) -> Compiled {
    compile_with(catalog(), selection)
}
fn compile_with(catalog: &[Value], selection: &Value) -> Compiled {
    let model = selection["model"].as_str().unwrap_or("");
    let entry = entry(catalog, model);
    let base = capabilities(entry);
    let supports_boolean = |id: &str| descriptors(&base, None).iter().any(|descriptor| descriptor["type"] == "boolean" && descriptor["id"] == id);
    let raw_effort = selection_value(selection, "effort").and_then(Value::as_str);
    let effort_selection = raw_effort.filter(|raw| !raw.is_empty()).map(|raw| json!({"options":[{"id":"effort","value":raw}]}));
    let resolved_effort = string_current(&descriptors(&base, effort_selection.as_ref()), "effort");
    let effort = resolved_effort.as_deref().and_then(|effort| {
        match entry.and_then(|entry| entry["runtime"]["effortMap"].as_object()).filter(|map| map.contains_key(effort)) {
            Some(map) => map[effort].as_str().map(str::to_owned),
            None => Some(effort.to_owned()),
        }
    });
    let flag = |id: &str| selection_value(selection, id).and_then(Value::as_bool);
    let fast_mode = supports_boolean("fastMode").then(|| flag("fastMode").unwrap_or(false));
    let thinking = if supports_boolean("thinking") { flag("thinking") } else { None };
    let mut settings = Map::new();
    if let Some(thinking) = thinking {
        settings.insert("alwaysThinkingEnabled".into(), json!(thinking));
    }
    if let Some(fast) = fast_mode {
        settings.insert("fastMode".into(), json!(fast));
    }
    if resolved_effort.as_deref() == Some("ultracode") {
        settings.insert("ultracode".into(), json!(true));
    }
    let api_model_id = api_model_id(entry, selection);
    let prompt_effort = trim_or_null(raw_effort).filter(|trimmed| {
        descriptors(&base, None).iter().any(|descriptor| descriptor["type"] == "select" && descriptor["promptInjectedValues"].as_array().is_some_and(|values| values.iter().any(|value| value == trimmed.as_str())))
    });
    let query_identity = json!({"apiModelId":api_model_id,"effort":effort,"settings":settings}).to_string();
    Compiled { api_model_id, effort, prompt_effort, settings, query_identity }
}
fn api_model_id(entry: Option<&Value>, selection: &Value) -> String {
    let slug = entry.and_then(|entry| entry["model"]["slug"].as_str()).unwrap_or_else(|| selection["model"].as_str().unwrap_or("")).to_owned();
    let resolved = descriptors(&capabilities(entry), Some(selection));
    for (option_id, suffixes) in entry.and_then(|entry| entry["runtime"]["modelSuffixes"].as_object()).into_iter().flatten() {
        if let Some(suffix) = string_current(&resolved, option_id).and_then(|value| suffixes[&value].as_str().filter(|suffix| !suffix.is_empty()).map(str::to_owned)) {
            return format!("{slug}{suffix}");
        }
    }
    slug
}
/// Context window of the selected model in tokens, when the catalog knows it.
pub fn context_window_tokens(selection: &Value) -> Option<u64> {
    let entry = entry(catalog(), selection["model"].as_str()?)?;
    if let Some(fixed) = entry["runtime"]["fixedContextWindowTokens"].as_u64().filter(|tokens| *tokens > 0) {
        return Some(fixed);
    }
    let resolved = descriptors(&capabilities(Some(entry)), Some(selection));
    let window = string_current(&resolved, "contextWindow")?;
    entry["runtime"]["contextWindowTokens"][&window].as_u64()
}
/// The slug a model name or alias refers to; unknown names pass through.
#[cfg(test)]
pub fn resolve_slug(name: &str) -> String {
    entry(catalog(), name).and_then(|entry| entry["model"]["slug"].as_str()).unwrap_or(name).to_owned()
}

fn semver(value: &str) -> Option<(u64, u64, u64, Vec<String>)> {
    let trimmed = value.trim();
    let (main, prerelease) = match trimmed.split_once('-') {
        Some((main, rest)) => (main, Some(rest.split('-').next().unwrap_or(""))),
        None => (trimmed, None),
    };
    let mut segments: Vec<&str> = main.split('.').map(str::trim).filter(|segment| !segment.is_empty()).collect();
    while !segments.is_empty() && segments.len() < 3 {
        segments.push("0");
    }
    let main = segments.join(".");
    let main = main.strip_prefix('v').unwrap_or(&main);
    let parts: Vec<&str> = main.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit())) {
        return None;
    }
    let prerelease = prerelease.map(|text| text.split('.').map(str::trim).filter(|segment| !segment.is_empty()).map(str::to_owned).collect()).unwrap_or_default();
    Some((parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?, prerelease))
}
fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let (Some(a), Some(b)) = (semver(left), semver(right)) else { return left.cmp(right) };
    (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)).then_with(|| match (a.3.is_empty(), b.3.is_empty()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => {
            for (x, y) in a.3.iter().zip(&b.3) {
                let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => std::cmp::Ordering::Less,
                    (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                    _ => x.cmp(y),
                };
                if order.is_ne() {
                    return order;
                }
            }
            a.3.len().cmp(&b.3.len())
        }
    })
}
fn version_supported(compatibility: &Value, version: Option<&str>) -> bool {
    let (min, max) = (compatibility["minVersion"].as_str(), compatibility["maxVersionExclusive"].as_str());
    if min.is_none() && max.is_none() {
        return true;
    }
    let Some(version) = version.filter(|version| !version.is_empty()) else { return false };
    min.is_none_or(|min| compare_versions(version, min).is_ge()) && max.is_none_or(|max| compare_versions(version, max).is_lt())
}
/// Every catalog model, whatever CLI version is installed.
pub fn all_models() -> Vec<Value> {
    catalog().iter().map(|entry| entry["model"].clone()).collect()
}
/// Catalog models the installed CLI can run.
pub fn models_for_version(version: Option<&str>) -> Vec<Value> {
    catalog().iter().filter(|entry| version_supported(&entry["compatibility"], version)).map(|entry| entry["model"].clone()).collect()
}
pub fn upgrade_message(version: Option<&str>) -> Option<String> {
    let version = version.filter(|version| !version.is_empty());
    let unavailable = catalog()
        .iter()
        .filter(|entry| entry["compatibility"]["minVersion"].as_str().is_some_and(|min| version.is_none_or(|version| compare_versions(version, min).is_lt())))
        .min_by(|a, b| compare_versions(a["compatibility"]["minVersion"].as_str().unwrap(), b["compatibility"]["minVersion"].as_str().unwrap()))?;
    let label = version.map_or("the installed version".to_owned(), |version| format!("v{version}"));
    Some(format!(
        "Claude Code {label} is too old for {}. Upgrade to v{} or newer to access it.",
        unavailable["model"]["name"].as_str()?,
        unavailable["compatibility"]["minVersion"].as_str()?
    ))
}
/// Built-in models followed by custom slugs (`providerModelsFromSettings`). A bare slug gets no options.
pub fn provider_models(builtin: Vec<Value>, custom: &[Value]) -> Vec<Value> {
    let mut seen: std::collections::HashSet<String> = builtin.iter().filter_map(|model| model["slug"].as_str().map(str::to_owned)).collect();
    let mut models = builtin;
    let mut slugs = std::collections::HashSet::new();
    for raw in custom {
        let record = if raw.is_string() { json!({"slug":raw}) } else if raw.is_object() { raw.clone() } else { continue };
        let Some(slug) = record["slug"].as_str().map(t3_contracts::trim_wire_string).filter(|slug| !slug.is_empty()) else { continue };
        if !slugs.insert(slug.to_owned()) || !seen.insert(slug.to_owned()) {
            continue;
        }
        let name = record["name"].as_str().map(t3_contracts::trim_wire_string).filter(|name| !name.is_empty()).unwrap_or(slug);
        let caps = record.get("capabilities").filter(|caps| caps["optionDescriptors"].is_array()).cloned().unwrap_or(json!({"optionDescriptors":[]}));
        models.push(json!({"slug":slug,"name":name,"isCustom":true,"capabilities":caps}));
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_catalog_selection_compiler_version_gate_and_custom_models() {
        let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/claude-models.json")).unwrap();
        let mut counts = [0usize; 4];
        for row in &rows {
            match row["operation"].as_str().unwrap() {
                "compile" => {
                    let compiled = compile_selection(&row["input"]);
                    let expected = &row["output"];
                    assert_eq!(compiled.api_model_id, expected["apiModelId"].as_str().unwrap(), "{row}");
                    assert_eq!(compiled.effort.as_deref(), expected["effort"].as_str(), "{row}");
                    assert_eq!(compiled.prompt_effort.as_deref(), expected["promptEffort"].as_str(), "{row}");
                    assert_eq!(Value::Object(compiled.settings.clone()), expected["settings"], "{row}");
                    assert_eq!(compiled.query_identity, expected["queryIdentity"].as_str().unwrap(), "{row}");
                    assert_eq!(context_window_tokens(&row["input"]), row["contextWindowTokens"].as_u64(), "{row}");
                    assert_eq!(resolve_slug(row["input"]["model"].as_str().unwrap()), row["slug"].as_str().unwrap(), "{row}");
                    counts[0] += 1;
                }
                "version" => {
                    let version = row["version"].as_str();
                    let slugs: Vec<String> = models_for_version(version).iter().map(|model| model["slug"].as_str().unwrap().to_owned()).collect();
                    assert_eq!(json!(slugs), row["slugs"], "{row}");
                    assert_eq!(upgrade_message(version).as_deref(), row["upgrade"].as_str(), "{row}");
                    counts[1] += 1;
                }
                "providerModels" => {
                    let models = provider_models(catalog().iter().map(|entry| entry["model"].clone()).collect(), row["custom"].as_array().unwrap());
                    let slugs: Vec<&str> = models.iter().map(|model| model["slug"].as_str().unwrap()).collect();
                    assert_eq!(json!(slugs), row["slugs"], "{row}");
                    let tail: Vec<Value> = models.iter().filter(|model| model["isCustom"] == true).cloned().collect();
                    assert_eq!(json!(tail), row["tail"], "{row}");
                    counts[2] += 1;
                }
                _ => counts[3] += 1,
            }
        }
        assert!(counts[0] > 400 && counts[1] == 13 && counts[2] == 4, "{counts:?}");
    }
}
