//! Provider settings policy shared by web and native clients.
//!
//! Config field changes preserve opaque provider keys. A field's source schema
//! annotation decides whether its default value is omitted or persisted.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, sync::LazyLock};
use t3_contracts::{AuthEnvironmentScope, SessionGrantInput, session_grants_scope};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Field {
    pub key: String,
    pub control: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    pub clear_when_empty: ClearWhenEmpty,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_boolean_value: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<FieldOption>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClearWhenEmpty {
    Omit,
    Persist,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct FieldOption {
    pub value: String,
    pub label: String,
}

pub fn drivers() -> &'static [Value] {
    static DRIVERS: LazyLock<Vec<Value>> = LazyLock::new(|| {
        serde_json::from_str(include_str!("provider_settings/drivers.json"))
            .expect("source-generated provider metadata")
    });
    &DRIVERS
}

pub fn default_providers() -> &'static Value {
    static DEFAULTS: LazyLock<Value> = LazyLock::new(|| {
        serde_json::from_str(include_str!("provider_settings/defaults.json"))
            .expect("source-generated provider defaults")
    });
    &DEFAULTS
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRow {
    pub instance_id: String,
    pub instance: Value,
    pub driver: String,
    pub is_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_dirty: Option<bool>,
}

pub fn enabled(instance: &Value) -> bool {
    serde_json::from_value::<t3_contracts::ProviderInstanceConfig>(instance.clone())
        .map(|instance| t3_contracts::resolve_provider_instance_enabled(&instance))
        .unwrap_or(false)
}

/// Synthesized legacy slots omit the old in-config flag. Explicit instances
/// retain their config unchanged, including unknown fork drivers and keys.
pub fn instance_rows(settings: &Value, live: &[Value], target: Option<&str>) -> Vec<InstanceRow> {
    let configured = settings["providerInstances"].as_object();
    let mut rows = Vec::new();
    let visible: Vec<&str> = drivers()
        .iter()
        .filter_map(|definition| definition["value"].as_str())
        .filter(|driver| *driver != "cursor" || live.iter().any(|p| p["instanceId"] == "cursor"))
        .collect();
    for driver in &visible {
        let explicit = configured.and_then(|values| values.get(*driver));
        let legacy = settings["providers"].get(*driver);
        let synthesized = legacy.map(|legacy| {
            let mut config = object_properties(legacy);
            let flag = config.remove("enabled");
            let mut instance = Map::from_iter([
                ("driver".into(), Value::String((*driver).into())),
                ("config".into(), Value::Object(config)),
            ]);
            if let Some(flag) = flag {
                instance.insert("enabled".into(), flag);
            }
            Value::Object(instance)
        });
        if let Some(instance) = explicit.or(synthesized.as_ref()) {
            let dirty = explicit.is_some() || legacy != default_providers().get(*driver);
            if matches!(*driver, "codex" | "claudeAgent")
                || dirty
                || enabled(instance)
                || target == Some(*driver)
            {
                rows.push(InstanceRow {
                    instance_id: (*driver).into(),
                    instance: instance.clone(),
                    driver: (*driver).into(),
                    is_default: true,
                    is_dirty: Some(dirty),
                });
            }
        }
        if let Some(configured) = configured {
            for (id, instance) in configured {
                if id != driver && instance["driver"].as_str() == Some(*driver) {
                    rows.push(InstanceRow {
                        instance_id: id.clone(),
                        driver: (*driver).into(),
                        instance: instance.clone(),
                        is_default: false,
                        is_dirty: None,
                    });
                }
            }
        }
    }
    if let Some(configured) = configured {
        for (id, instance) in configured {
            let driver = instance["driver"].as_str().unwrap_or_default();
            if !visible.contains(&driver) {
                rows.push(InstanceRow {
                    instance_id: id.clone(),
                    driver: driver.into(),
                    instance: instance.clone(),
                    is_default: visible.contains(&id.as_str()),
                    is_dirty: None,
                });
            }
        }
    }
    rows
}

pub fn selected_instance<'a>(
    rows: &'a [InstanceRow],
    selected: Option<&str>,
    target: Option<&str>,
) -> Option<&'a InstanceRow> {
    rows.iter()
        .find(|row| Some(row.instance_id.as_str()) == selected)
        .or_else(|| {
            if target.is_some() && selected == target {
                None
            } else {
                rows.first()
            }
        })
}

pub fn upsert_input(settings: &Value, row: &InstanceRow, instance: Value) -> Value {
    let mut patch = Map::new();
    if row.is_default {
        if let Some(default) = default_providers().get(&row.driver) {
            let mut providers = object_properties(&settings["providers"]);
            providers.insert(row.driver.clone(), default.clone());
            patch.insert("providers".into(), Value::Object(providers));
        }
    }
    serde_json::json!({"patch":patch,"providerInstanceMutation":{"operation":"upsert","instanceId":row.instance_id,"instance":instance}})
}

/// Presentation metadata extracted from the original client settings schemas.
pub fn fields(driver: &str, config: &Value) -> &'static [Field] {
    static FIELDS: LazyLock<BTreeMap<String, Vec<Field>>> = LazyLock::new(|| {
        serde_json::from_str(include_str!("provider_settings/fields.json"))
            .expect("source-generated provider field metadata")
    });
    let key = if driver == "acpRegistry" && config["source"].as_str() == Some("local") {
        "acpLocal"
    } else {
        driver
    };
    FIELDS.get(key).map(Vec::as_slice).unwrap_or_default()
}

/// Mirrors object spread at the original form boundary, including array keys.
fn object_properties(config: &Value) -> Map<String, Value> {
    match config {
        Value::Object(values) => values.clone(),
        Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), value.clone()))
            .collect(),
        _ => Map::new(),
    }
}

fn js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

pub fn next_field_value(config: &Value, field: &Field, value: &Value) -> Option<Value> {
    let mut next = object_properties(config);
    let empty = match value {
        Value::Bool(value) => *value == field.default_boolean_value.unwrap_or(false),
        Value::String(value) => value.trim_matches(js_whitespace).is_empty(),
        _ => return None,
    };
    if field.clear_when_empty == ClearWhenEmpty::Omit && empty {
        next.remove(&field.key);
    } else {
        next.insert(field.key.clone(), value.clone());
    }
    (!next.is_empty()).then_some(Value::Object(next))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperateAccess {
    Granted,
    Denied,
    Pending,
}

/// Cached grants stay authoritative during revalidation. A failed lookup denies.
pub fn operate_access(
    session: Option<&SessionGrantInput>,
    pending: bool,
    has_error: bool,
) -> OperateAccess {
    if has_error {
        OperateAccess::Denied
    } else if let Some(session) = session {
        if session_grants_scope(session, AuthEnvironmentScope::ProvidersManage) {
            OperateAccess::Granted
        } else {
            OperateAccess::Denied
        }
    } else if pending {
        OperateAccess::Pending
    } else {
        OperateAccess::Denied
    }
}

pub fn environment_access(
    connection_phase: &str,
    has_server_config: bool,
    operate: OperateAccess,
) -> Value {
    if connection_phase == "error" {
        serde_json::json!({"kind":"error"})
    } else if connection_phase != "connected" {
        serde_json::json!({"kind":"unavailable"})
    } else if !has_server_config {
        serde_json::json!({"kind":"loading","reason":"config"})
    } else {
        match operate {
            OperateAccess::Pending => serde_json::json!({"kind":"loading","reason":"permissions"}),
            OperateAccess::Denied => serde_json::json!({"kind":"read-only"}),
            OperateAccess::Granted => serde_json::json!({"kind":"editable"}),
        }
    }
}

pub fn selected_environment<'a>(
    environment_ids: &'a [String],
    selected: Option<&str>,
    primary: Option<&str>,
) -> Option<&'a str> {
    selected
        .and_then(|selected| environment_ids.iter().find(|id| id.as_str() == selected))
        .or_else(|| {
            primary.and_then(|primary| environment_ids.iter().find(|id| id.as_str() == primary))
        })
        .or_else(|| environment_ids.first())
        .map(String::as_str)
}
