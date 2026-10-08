use crate::base::{ValidationError, string_type};
use crate::*;
use serde::{Deserialize, Serialize};

fn provider_slug(value: &str) -> Result<(), ValidationError> {
    if value.len() <= 64
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a provider slug starting with a letter, containing 1..64 letters, digits, dashes or underscores",
        })
    }
}
string_type!(ProviderDriverKind, provider_slug);
string_type!(ProviderInstanceId, provider_slug);

fn environment_variable_name(value: &str) -> Result<(), ValidationError> {
    if value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a valid environment variable name of 1..128 characters",
        })
    }
}
string_type!(
    ProviderInstanceEnvironmentVariableName,
    environment_variable_name
);

pub fn default_instance_id_for_driver(driver: &ProviderDriverKind) -> ProviderInstanceId {
    ProviderInstanceId::new(driver.as_str()).expect("driver and instance share slug validation")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub instance_id: ProviderInstanceId,
    pub model: TrimmedNonEmptyString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<ProviderOptionSelection>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProviderOptionSelectionValue {
    String(TrimmedNonEmptyString),
    Boolean(bool),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderOptionSelection {
    pub id: TrimmedNonEmptyString,
    pub value: ProviderOptionSelectionValue,
}

pub fn decode_provider_options(
    value: serde_json::Value,
) -> Result<Vec<ProviderOptionSelection>, serde_json::Error> {
    if let serde_json::Value::Object(object) = value {
        // Object.entries lists canonical array-index keys first, in numeric
        // order, then other keys in insertion order.
        let mut entries: Vec<_> = object.into_iter().collect();
        entries.sort_by_key(|(key, _)| {
            key.parse::<u32>()
                .ok()
                .filter(|n| *n != u32::MAX && n.to_string() == *key)
                .map(|n| (0, n))
                .unwrap_or((1, 0))
        });
        Ok(entries
            .into_iter()
            .filter_map(|(id, raw)| {
                let id = TrimmedNonEmptyString::new(id).ok()?;
                let value = match raw {
                    serde_json::Value::String(s) => {
                        ProviderOptionSelectionValue::String(TrimmedNonEmptyString::new(s).ok()?)
                    }
                    serde_json::Value::Bool(b) => ProviderOptionSelectionValue::Boolean(b),
                    _ => return None,
                };
                Some(ProviderOptionSelection { id, value })
            })
            .collect())
    } else {
        serde_json::from_value(value)
    }
}

impl<'de> Deserialize<'de> for ModelSelection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        let object = raw
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("expected a model selection object"))?;
        // An explicitly present malformed instanceId never falls back to provider.
        let instance = object
            .get("instanceId")
            .or_else(|| object.get("provider"))
            .ok_or_else(|| serde::de::Error::missing_field("instanceId"))?;
        let instance_id =
            serde_json::from_value(instance.clone()).map_err(serde::de::Error::custom)?;
        let model = serde_json::from_value(
            object
                .get("model")
                .cloned()
                .ok_or_else(|| serde::de::Error::missing_field("model"))?,
        )
        .map_err(serde::de::Error::custom)?;
        let options = object
            .get("options")
            .map(|v| decode_provider_options(v.clone()))
            .transpose()
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            instance_id,
            model,
            options,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    ApprovalRequired,
    AutoAcceptEdits,
    Auto,
    #[default]
    FullAccess,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProviderInteractionMode {
    #[default]
    Default,
    Plan,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderApprovalPolicy {
    Untrusted,
    OnFailure,
    OnRequest,
    Never,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderSandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderApprovalDecision {
    Accept,
    AcceptForSession,
    AcceptAlways,
    Decline,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderSessionStatus {
    Connecting,
    Ready,
    Running,
    Error,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSession {
    pub provider: ProviderDriverKind,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_optional"
    )]
    pub provider_instance_id: Option<Option<ProviderInstanceId>>,
    pub status: ProviderSessionStatus,
    pub runtime_mode: RuntimeMode,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_optional"
    )]
    pub cwd: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_optional"
    )]
    pub model: Option<Option<TrimmedNonEmptyString>>,
    pub thread_id: ThreadId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_optional"
    )]
    pub resume_cursor: Option<serde_json::Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_optional"
    )]
    pub active_turn_id: Option<Option<TurnId>>,
    pub created_at: IsoDateTime,
    pub updated_at: IsoDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::deserialize_optional"
    )]
    pub last_error: Option<Option<TrimmedNonEmptyString>>,
}

// Provider configuration and model picker contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderInstanceMutationOperation {
    #[serde(rename = "create")]
    Create,
    #[serde(rename = "upsert")]
    Upsert,
    #[serde(rename = "remove")]
    Remove,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstanceRef {
    pub instance_id: ProviderInstanceId,
    pub driver: ProviderDriverKind,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstanceEnvironmentVariable {
    pub name: ProviderInstanceEnvironmentVariableName,
    #[serde(default, deserialize_with = "deserialize_default_string")]
    pub value: String,
    #[serde(default, deserialize_with = "deserialize_default_bool")]
    pub sensitive: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub value_redacted: Option<bool>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstanceConfig {
    pub driver: ProviderDriverKind,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub display_name: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub accent_color: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub environment: Option<Vec<ProviderInstanceEnvironmentVariable>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub config: Option<serde_json::Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOptionChoice {
    pub id: TrimmedNonEmptyString,
    pub label: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub description: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub is_default: Option<Option<bool>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectProviderOptionDescriptorFields {
    pub id: TrimmedNonEmptyString,
    pub label: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub description: Option<Option<TrimmedNonEmptyString>>,
    pub options: Vec<ProviderOptionChoice>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub current_value: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub prompt_injected_values: Option<Option<Vec<TrimmedNonEmptyString>>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BooleanProviderOptionDescriptorFields {
    pub id: TrimmedNonEmptyString,
    pub label: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub description: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub current_value: Option<Option<bool>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilities {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub option_descriptors: Option<Option<Vec<ProviderOptionDescriptor>>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModelEntry {
    pub slug: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub name: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub capabilities: Option<Option<ModelCapabilities>>,
}

pub(crate) fn deserialize_default_string<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}
pub(crate) fn deserialize_default_bool<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(d)?.unwrap_or_default())
}
pub(crate) fn deserialize_default_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all_fields = "camelCase")]
pub enum ProviderInstanceMutation {
    #[serde(rename = "create")]
    Create {
        instance_id: ProviderInstanceId,
        instance: ProviderInstanceConfig,
    },
    #[serde(rename = "upsert")]
    Upsert {
        instance_id: ProviderInstanceId,
        instance: ProviderInstanceConfig,
    },
    #[serde(rename = "remove")]
    Remove { instance_id: ProviderInstanceId },
}
pub type ProviderInstanceConfigMap =
    std::collections::BTreeMap<ProviderInstanceId, ProviderInstanceConfig>;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProviderOptionDescriptor {
    #[serde(rename = "select")]
    Select {
        #[serde(flatten)]
        fields: SelectProviderOptionDescriptorFields,
    },
    #[serde(rename = "boolean")]
    Boolean {
        #[serde(flatten)]
        fields: BooleanProviderOptionDescriptorFields,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CustomModelSetting {
    String(String),
    Entry(CustomModelEntry),
}
