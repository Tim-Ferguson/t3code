//! Settings RPC and configuration stream envelopes from rpc.ts/server.ts.
use crate::{
    EnvironmentTheme, ForwardCompatibleArray, LiteralInt, ProviderInstanceMutation,
    ResolvedKeybindingsConfig, ServerConfig, ServerConfigIssue, ServerProvider, ServerSettings,
    ServerSettingsPatch, UsageLimitSourceSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
/// Effect's empty Struct preserves every non-null JSON value.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct GetServerSettingsInput(pub Value);
impl<'de> Deserialize<'de> for GetServerSettingsInput {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if value.is_null() {
            return Err(serde::de::Error::custom("Expected a non-null value."));
        }
        Ok(Self(value))
    }
}
pub type GetServerConfigInput = GetServerSettingsInput;
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateServerSettingsInput {
    pub patch: ServerSettingsPatch,
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_instance_mutation: Option<ProviderInstanceMutation>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(remote = "UpdateServerSettingsInput")]
struct UpdateServerSettingsInputFields {
    pub patch: ServerSettingsPatch,
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_instance_mutation: Option<ProviderInstanceMutation>,
}
impl<'de> Deserialize<'de> for UpdateServerSettingsInput {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom("Expected an object."));
        }
        UpdateServerSettingsInputFields::deserialize(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeServerConfigInput {
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub environment_themes: Option<Option<bool>>,
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_limit_sources: Option<Option<bool>>,
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_limits_command: Option<Option<bool>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(remote = "SubscribeServerConfigInput")]
struct SubscribeServerConfigInputFields {
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub environment_themes: Option<Option<bool>>,
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_limit_sources: Option<Option<bool>>,
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_limits_command: Option<Option<bool>>,
}
impl<'de> Deserialize<'de> for SubscribeServerConfigInput {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom("Expected an object."));
        }
        SubscribeServerConfigInputFields::deserialize(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigProviderStatusesPayload {
    pub providers: ForwardCompatibleArray<ServerProvider>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSettingsUpdatedPayload {
    pub settings: ServerSettings,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigKeybindingsUpdatedPayload {
    pub keybindings: ResolvedKeybindingsConfig,
    pub issues: ForwardCompatibleArray<ServerConfigIssue>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigThemesUpdatedPayload {
    pub themes: Vec<EnvironmentTheme>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUsageLimitSourcesUpdatedPayload {
    pub sources: ForwardCompatibleArray<UsageLimitSourceSnapshot>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerConfigStreamEvent {
    #[serde(rename = "snapshot")]
    Snapshot {
        version: LiteralInt<1>,
        config: ServerConfig,
    },
    #[serde(rename = "keybindingsUpdated")]
    KeybindingsUpdated {
        version: LiteralInt<1>,
        payload: ConfigKeybindingsUpdatedPayload,
    },
    #[serde(rename = "providerStatuses")]
    ProviderStatuses {
        version: LiteralInt<1>,
        payload: ConfigProviderStatusesPayload,
    },
    #[serde(rename = "settingsUpdated")]
    SettingsUpdated {
        version: LiteralInt<1>,
        payload: ConfigSettingsUpdatedPayload,
    },
    #[serde(rename = "environmentThemesUpdated")]
    EnvironmentThemesUpdated {
        version: LiteralInt<1>,
        payload: ConfigThemesUpdatedPayload,
    },
    #[serde(rename = "usageLimitSourcesUpdated")]
    UsageLimitSourcesUpdated {
        version: LiteralInt<1>,
        payload: ConfigUsageLimitSourcesUpdatedPayload,
    },
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ServerSettingsOperation {
    #[serde(rename = "normalize")]
    Normalize,
    #[serde(rename = "check-exists")]
    CheckExists,
    #[serde(rename = "create-provider-instance")]
    CreateProviderInstance,
    #[serde(rename = "read-file")]
    ReadFile,
    #[serde(rename = "read-provider-history")]
    ReadProviderHistory,
    #[serde(rename = "read-project-settings")]
    ReadProjectSettings,
    #[serde(rename = "read-secret")]
    ReadSecret,
    #[serde(rename = "remove-secret")]
    RemoveSecret,
    #[serde(rename = "remove-stale-secret")]
    RemoveStaleSecret,
    #[serde(rename = "write-secret")]
    WriteSecret,
    #[serde(rename = "write-file")]
    WriteFile,
    #[serde(rename = "prepare-directory")]
    PrepareDirectory,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerSettingsErrorTag {
    ServerSettingsError,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettingsError {
    #[serde(rename = "_tag")]
    pub tag: ServerSettingsErrorTag,
    pub settings_path: String,
    pub operation: ServerSettingsOperation,
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_instance_id: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub environment_variable: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "crate::deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub cause: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn compare<T: serde::de::DeserializeOwned + Serialize>(
        row: &Value,
        index: usize,
    ) -> Result<(), String> {
        let decoded = serde_json::from_value::<T>(row["input"].clone());
        if decoded.is_ok() != row["valid"].as_bool().unwrap() {
            return Err(format!(
                "{} witness {index}: input={} expected acceptance={} actual={}",
                row["schema"],
                row["input"],
                row["valid"],
                decoded.is_ok()
            ));
        }
        if let Ok(output) = decoded {
            let output = serde_json::to_value(output).unwrap();
            if output != row["output"] {
                return Err(format!(
                    "{} witness {index}: encoded mismatch: actual={output}, expected={}",
                    row["schema"], row["output"]
                ));
            }
        }
        Ok(())
    }
    #[test]
    fn original_settings_rpc_and_config_stream_codecs() {
        let mut failures = Vec::new();
        for (index, line) in include_str!("../tests/fixtures/settings-rpc.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let result = match row["schema"].as_str().unwrap() {
                "SubscribeServerConfigInput" => compare::<SubscribeServerConfigInput>(&row, index),
                "UpdateServerSettingsInput" => compare::<UpdateServerSettingsInput>(&row, index),
                "GetServerSettingsInput" => compare::<GetServerSettingsInput>(&row, index),
                "ServerConfigStreamEvent" => compare::<ServerConfigStreamEvent>(&row, index),
                "ServerSettingsError" => compare::<ServerSettingsError>(&row, index),
                _ => unreachable!(),
            };
            if let Err(error) = result {
                failures.push(error);
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
