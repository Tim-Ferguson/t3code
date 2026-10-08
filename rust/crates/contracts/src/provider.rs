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
        Ok(object
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_instance_id: Option<ProviderInstanceId>,
    pub status: ProviderSessionStatus,
    pub runtime_mode: RuntimeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<TrimmedNonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<TrimmedNonEmptyString>,
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_cursor: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn_id: Option<TurnId>,
    pub created_at: IsoDateTime,
    pub updated_at: IsoDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<TrimmedNonEmptyString>,
}
