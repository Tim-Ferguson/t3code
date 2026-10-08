//! Server settings, provider configuration, and mutation boundaries.
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodexSetupMode {
    #[serde(rename = "managed")]
    Managed,
    #[serde(rename = "existing")]
    Existing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AntigravityAuthMethod {
    #[serde(rename = "oauth-personal")]
    OauthPersonal,
    #[serde(rename = "oauth-business")]
    OauthBusiness,
    #[serde(rename = "gemini-api-key")]
    GeminiApiKey,
    #[serde(rename = "agent-platform")]
    AgentPlatform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcpRegistryDistributionPreference {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "binary")]
    Binary,
    #[serde(rename = "npx")]
    Npx,
    #[serde(rename = "uvx")]
    Uvx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcpRegistrySettingsSource {
    #[serde(rename = "registry")]
    Registry,
    #[serde(rename = "local")]
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceControlWritingStyleMode {
    #[serde(rename = "repo_conventions")]
    RepoConventions,
    #[serde(rename = "conventional_commits")]
    ConventionalCommits,
    #[serde(rename = "custom")]
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchNamingMode {
    #[serde(rename = "static")]
    Static,
    #[serde(rename = "semantic")]
    Semantic,
    #[serde(rename = "custom")]
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackgroundActivityProfile {
    #[serde(rename = "balanced")]
    Balanced,
    #[serde(rename = "performance")]
    Performance,
    #[serde(rename = "battery-saver")]
    BatterySaver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackgroundActivityProfileSelection {
    #[serde(rename = "balanced")]
    Balanced,
    #[serde(rename = "performance")]
    Performance,
    #[serde(rename = "battery-saver")]
    BatterySaver,
    #[serde(rename = "custom")]
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseStreamingMode {
    #[serde(rename = "turn")]
    Turn,
    #[serde(rename = "paragraph")]
    Paragraph,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullRequestMergeMethod {
    #[serde(rename = "merge")]
    Merge,
    #[serde(rename = "squash")]
    Squash,
    #[serde(rename = "rebase")]
    Rebase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectScriptIcon {
    #[serde(rename = "play")]
    Play,
    #[serde(rename = "test")]
    Test,
    #[serde(rename = "lint")]
    Lint,
    #[serde(rename = "configure")]
    Configure,
    #[serde(rename = "build")]
    Build,
    #[serde(rename = "debug")]
    Debug,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageLimitSourceKind {
    #[serde(rename = "cliproxy")]
    Cliproxy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectScript {
    pub id: TrimmedNonEmptyString,

    pub name: TrimmedNonEmptyString,

    pub command: TrimmedNonEmptyString,

    pub icon: ProjectScriptIcon,

    pub run_on_worktree_create: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub run_on_settle: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub r#async: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub preview_url: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub auto_open_preview: Option<Option<bool>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageModelPriceOverride {
    pub input_cost_per_million_tokens: UsageTokenPrice,

    pub output_cost_per_million_tokens: UsageTokenPrice,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cache_read_cost_per_million_tokens: Option<UsageTokenPrice>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cache_write_cost_per_million_tokens: Option<UsageTokenPrice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSettings {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub setup_mode: Option<CodexSetupMode>,

    #[serde(
        default = "default_codex_settings_enabled",
        deserialize_with = "decode_codex_settings_enabled"
    )]
    pub enabled: bool,

    #[serde(
        default = "default_codex_settings_binary_path",
        deserialize_with = "decode_codex_settings_binary_path"
    )]
    pub binary_path: TrimmedString,

    #[serde(
        default = "default_codex_settings_home_path",
        deserialize_with = "decode_codex_settings_home_path"
    )]
    pub home_path: TrimmedString,

    #[serde(
        default = "default_codex_settings_shadow_home_path",
        deserialize_with = "decode_codex_settings_shadow_home_path"
    )]
    pub shadow_home_path: TrimmedString,

    #[serde(
        default = "default_codex_settings_launch_args",
        deserialize_with = "decode_codex_settings_launch_args"
    )]
    pub launch_args: TrimmedString,

    #[serde(
        default = "default_codex_settings_custom_models",
        deserialize_with = "decode_codex_settings_custom_models"
    )]
    pub custom_models: Vec<CustomModelSetting>,
}

fn default_codex_settings_enabled() -> bool {
    serde_json::from_str(r###"true"###).expect("source-backed settings default")
}
fn decode_codex_settings_enabled<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_codex_settings_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_codex_settings_binary_path() -> TrimmedString {
    serde_json::from_str(r###""codex""###).expect("source-backed settings default")
}
fn decode_codex_settings_binary_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_codex_settings_binary_path());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    if value.as_str().is_empty() {
        return Ok(default_codex_settings_binary_path());
    }

    Ok(value)
}

fn default_codex_settings_home_path() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_codex_settings_home_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_codex_settings_home_path());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_codex_settings_shadow_home_path() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_codex_settings_shadow_home_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_codex_settings_shadow_home_path());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_codex_settings_launch_args() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_codex_settings_launch_args<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_codex_settings_launch_args());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_codex_settings_custom_models() -> Vec<CustomModelSetting> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_codex_settings_custom_models<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<CustomModelSetting>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_codex_settings_custom_models());
    }

    let value: Vec<CustomModelSetting> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for CodexSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"enabled":true,"binaryPath":"codex","homePath":"","shadowHomePath":"","launchArgs":"","customModels":[]}"###).expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSettings {
    #[serde(
        default = "default_claude_settings_enabled",
        deserialize_with = "decode_claude_settings_enabled"
    )]
    pub enabled: bool,

    #[serde(
        default = "default_claude_settings_binary_path",
        deserialize_with = "decode_claude_settings_binary_path"
    )]
    pub binary_path: TrimmedString,

    #[serde(
        default = "default_claude_settings_home_path",
        deserialize_with = "decode_claude_settings_home_path"
    )]
    pub home_path: TrimmedString,

    #[serde(
        default = "default_claude_settings_custom_models",
        deserialize_with = "decode_claude_settings_custom_models"
    )]
    pub custom_models: Vec<CustomModelSetting>,

    #[serde(
        default = "default_claude_settings_launch_args",
        deserialize_with = "decode_claude_settings_launch_args"
    )]
    pub launch_args: String,

    #[serde(
        default = "default_claude_settings_auto_compact_window",
        deserialize_with = "decode_claude_settings_auto_compact_window"
    )]
    pub auto_compact_window: ClaudeAutoCompactWindow,
}

fn default_claude_settings_enabled() -> bool {
    serde_json::from_str(r###"true"###).expect("source-backed settings default")
}
fn decode_claude_settings_enabled<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_claude_settings_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_claude_settings_binary_path() -> TrimmedString {
    serde_json::from_str(r###""claude""###).expect("source-backed settings default")
}
fn decode_claude_settings_binary_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_claude_settings_binary_path());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    if value.as_str().is_empty() {
        return Ok(default_claude_settings_binary_path());
    }

    Ok(value)
}

fn default_claude_settings_home_path() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_claude_settings_home_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_claude_settings_home_path());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_claude_settings_custom_models() -> Vec<CustomModelSetting> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_claude_settings_custom_models<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<CustomModelSetting>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_claude_settings_custom_models());
    }

    let value: Vec<CustomModelSetting> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_claude_settings_launch_args() -> String {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_claude_settings_launch_args<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<String, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_claude_settings_launch_args());
    }

    let value: String = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_claude_settings_auto_compact_window() -> ClaudeAutoCompactWindow {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_claude_settings_auto_compact_window<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<ClaudeAutoCompactWindow, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_claude_settings_auto_compact_window());
    }

    let value: ClaudeAutoCompactWindow =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for ClaudeSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"enabled":true,"binaryPath":"claude","homePath":"","customModels":[],"launchArgs":"","autoCompactWindow":""}"###).expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorSettings {
    #[serde(
        default = "default_cursor_settings_enabled",
        deserialize_with = "decode_cursor_settings_enabled"
    )]
    pub enabled: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub binary_path: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub api_endpoint: Option<TrimmedString>,

    #[serde(
        default = "default_cursor_settings_custom_models",
        deserialize_with = "decode_cursor_settings_custom_models"
    )]
    pub custom_models: Vec<CustomModelSetting>,
}

fn default_cursor_settings_enabled() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_cursor_settings_enabled<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_cursor_settings_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_cursor_settings_custom_models() -> Vec<CustomModelSetting> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_cursor_settings_custom_models<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<CustomModelSetting>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_cursor_settings_custom_models());
    }

    let value: Vec<CustomModelSetting> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for CursorSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"enabled":false,"customModels":[]}"###)
            .expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokSettings {
    #[serde(
        default = "default_grok_settings_enabled",
        deserialize_with = "decode_grok_settings_enabled"
    )]
    pub enabled: bool,

    #[serde(
        default = "default_grok_settings_binary_path",
        deserialize_with = "decode_grok_settings_binary_path"
    )]
    pub binary_path: TrimmedString,

    #[serde(
        default = "default_grok_settings_custom_models",
        deserialize_with = "decode_grok_settings_custom_models"
    )]
    pub custom_models: Vec<CustomModelSetting>,
}

fn default_grok_settings_enabled() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_grok_settings_enabled<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_grok_settings_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_grok_settings_binary_path() -> TrimmedString {
    serde_json::from_str(r###""grok""###).expect("source-backed settings default")
}
fn decode_grok_settings_binary_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_grok_settings_binary_path());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    if value.as_str().is_empty() {
        return Ok(default_grok_settings_binary_path());
    }

    Ok(value)
}

fn default_grok_settings_custom_models() -> Vec<CustomModelSetting> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_grok_settings_custom_models<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<CustomModelSetting>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_grok_settings_custom_models());
    }

    let value: Vec<CustomModelSetting> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for GrokSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"enabled":false,"binaryPath":"grok","customModels":[]}"###)
            .expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySettings {
    #[serde(
        default = "default_antigravity_settings_enabled",
        deserialize_with = "decode_antigravity_settings_enabled"
    )]
    pub enabled: bool,

    #[serde(
        default = "default_antigravity_settings_auth_method",
        deserialize_with = "decode_antigravity_settings_auth_method"
    )]
    pub auth_method: AntigravityAuthMethod,

    #[serde(
        default = "default_antigravity_settings_api_key",
        deserialize_with = "decode_antigravity_settings_api_key"
    )]
    pub api_key: TrimmedString,

    #[serde(
        default = "default_antigravity_settings_gcp_project",
        deserialize_with = "decode_antigravity_settings_gcp_project"
    )]
    pub gcp_project: TrimmedString,

    #[serde(
        default = "default_antigravity_settings_gcp_location",
        deserialize_with = "decode_antigravity_settings_gcp_location"
    )]
    pub gcp_location: TrimmedString,

    #[serde(
        default = "default_antigravity_settings_binary_path",
        deserialize_with = "decode_antigravity_settings_binary_path"
    )]
    pub binary_path: TrimmedString,

    #[serde(
        default = "default_antigravity_settings_custom_models",
        deserialize_with = "decode_antigravity_settings_custom_models"
    )]
    pub custom_models: Vec<CustomModelSetting>,
}

fn default_antigravity_settings_enabled() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_antigravity_settings_enabled<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_antigravity_settings_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_antigravity_settings_auth_method() -> AntigravityAuthMethod {
    serde_json::from_str(r###""oauth-personal""###).expect("source-backed settings default")
}
fn decode_antigravity_settings_auth_method<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<AntigravityAuthMethod, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_antigravity_settings_auth_method());
    }

    let value: AntigravityAuthMethod =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_antigravity_settings_api_key() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_antigravity_settings_api_key<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_antigravity_settings_api_key());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_antigravity_settings_gcp_project() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_antigravity_settings_gcp_project<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_antigravity_settings_gcp_project());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_antigravity_settings_gcp_location() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_antigravity_settings_gcp_location<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_antigravity_settings_gcp_location());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_antigravity_settings_binary_path() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_antigravity_settings_binary_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_antigravity_settings_binary_path());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_antigravity_settings_custom_models() -> Vec<CustomModelSetting> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_antigravity_settings_custom_models<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<CustomModelSetting>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_antigravity_settings_custom_models());
    }

    let value: Vec<CustomModelSetting> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for AntigravitySettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"enabled":false,"authMethod":"oauth-personal","apiKey":"","gcpProject":"","gcpLocation":"","binaryPath":"","customModels":[]}"###).expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiSettings {
    #[serde(
        default = "default_pi_settings_enabled",
        deserialize_with = "decode_pi_settings_enabled"
    )]
    pub enabled: bool,

    #[serde(
        default = "default_pi_settings_binary_path",
        deserialize_with = "decode_pi_settings_binary_path"
    )]
    pub binary_path: TrimmedString,

    #[serde(
        default = "default_pi_settings_launch_args",
        deserialize_with = "decode_pi_settings_launch_args"
    )]
    pub launch_args: TrimmedString,

    #[serde(
        default = "default_pi_settings_custom_models",
        deserialize_with = "decode_pi_settings_custom_models"
    )]
    pub custom_models: Vec<CustomModelSetting>,
}

fn default_pi_settings_enabled() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_pi_settings_enabled<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_pi_settings_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_pi_settings_binary_path() -> TrimmedString {
    serde_json::from_str(r###""pi""###).expect("source-backed settings default")
}
fn decode_pi_settings_binary_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_pi_settings_binary_path());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    if value.as_str().is_empty() {
        return Ok(default_pi_settings_binary_path());
    }

    Ok(value)
}

fn default_pi_settings_launch_args() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_pi_settings_launch_args<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_pi_settings_launch_args());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_pi_settings_custom_models() -> Vec<CustomModelSetting> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_pi_settings_custom_models<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<CustomModelSetting>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_pi_settings_custom_models());
    }

    let value: Vec<CustomModelSetting> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for PiSettings {
    fn default() -> Self {
        serde_json::from_str(
            r###"{"enabled":false,"binaryPath":"pi","launchArgs":"","customModels":[]}"###,
        )
        .expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCodeSettings {
    #[serde(
        default = "default_open_code_settings_enabled",
        deserialize_with = "decode_open_code_settings_enabled"
    )]
    pub enabled: bool,

    #[serde(
        default = "default_open_code_settings_binary_path",
        deserialize_with = "decode_open_code_settings_binary_path"
    )]
    pub binary_path: TrimmedString,

    #[serde(
        default = "default_open_code_settings_server_url",
        deserialize_with = "decode_open_code_settings_server_url"
    )]
    pub server_url: TrimmedString,

    #[serde(
        default = "default_open_code_settings_server_password",
        deserialize_with = "decode_open_code_settings_server_password"
    )]
    pub server_password: TrimmedString,

    #[serde(
        default = "default_open_code_settings_custom_models",
        deserialize_with = "decode_open_code_settings_custom_models"
    )]
    pub custom_models: Vec<CustomModelSetting>,
}

fn default_open_code_settings_enabled() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_open_code_settings_enabled<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_open_code_settings_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_open_code_settings_binary_path() -> TrimmedString {
    serde_json::from_str(r###""opencode""###).expect("source-backed settings default")
}
fn decode_open_code_settings_binary_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_open_code_settings_binary_path());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    if value.as_str().is_empty() {
        return Ok(default_open_code_settings_binary_path());
    }

    Ok(value)
}

fn default_open_code_settings_server_url() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_open_code_settings_server_url<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_open_code_settings_server_url());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_open_code_settings_server_password() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_open_code_settings_server_password<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_open_code_settings_server_password());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_open_code_settings_custom_models() -> Vec<CustomModelSetting> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_open_code_settings_custom_models<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<CustomModelSetting>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_open_code_settings_custom_models());
    }

    let value: Vec<CustomModelSetting> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for OpenCodeSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"enabled":false,"binaryPath":"opencode","serverUrl":"","serverPassword":"","customModels":[]}"###).expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpRegistrySettings {
    #[serde(
        default = "default_acp_registry_settings_source",
        deserialize_with = "decode_acp_registry_settings_source"
    )]
    pub source: AcpRegistrySettingsSource,

    #[serde(
        default = "default_acp_registry_settings_enabled",
        deserialize_with = "decode_acp_registry_settings_enabled"
    )]
    pub enabled: bool,

    #[serde(
        default = "default_acp_registry_settings_agent_id",
        deserialize_with = "decode_acp_registry_settings_agent_id"
    )]
    pub agent_id: TrimmedString,

    #[serde(
        default = "default_acp_registry_settings_command_path",
        deserialize_with = "decode_acp_registry_settings_command_path"
    )]
    pub command_path: TrimmedString,

    #[serde(
        default = "default_acp_registry_settings_command_args",
        deserialize_with = "decode_acp_registry_settings_command_args"
    )]
    pub command_args: Vec<String>,

    #[serde(
        default = "default_acp_registry_settings_auth_method_id",
        deserialize_with = "decode_acp_registry_settings_auth_method_id"
    )]
    pub auth_method_id: TrimmedString,

    #[serde(
        default = "default_acp_registry_settings_distribution",
        deserialize_with = "decode_acp_registry_settings_distribution"
    )]
    pub distribution: AcpRegistryDistributionPreference,

    #[serde(
        default = "default_acp_registry_settings_custom_models",
        deserialize_with = "decode_acp_registry_settings_custom_models"
    )]
    pub custom_models: Vec<String>,
}

fn default_acp_registry_settings_source() -> AcpRegistrySettingsSource {
    serde_json::from_str(r###""registry""###).expect("source-backed settings default")
}
fn decode_acp_registry_settings_source<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<AcpRegistrySettingsSource, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_acp_registry_settings_source());
    }

    let value: AcpRegistrySettingsSource =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_acp_registry_settings_enabled() -> bool {
    serde_json::from_str(r###"true"###).expect("source-backed settings default")
}
fn decode_acp_registry_settings_enabled<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_acp_registry_settings_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_acp_registry_settings_agent_id() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_acp_registry_settings_agent_id<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_acp_registry_settings_agent_id());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_acp_registry_settings_command_path() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_acp_registry_settings_command_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_acp_registry_settings_command_path());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_acp_registry_settings_command_args() -> Vec<String> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_acp_registry_settings_command_args<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<String>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_acp_registry_settings_command_args());
    }

    let value: Vec<String> = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_acp_registry_settings_auth_method_id() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_acp_registry_settings_auth_method_id<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_acp_registry_settings_auth_method_id());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_acp_registry_settings_distribution() -> AcpRegistryDistributionPreference {
    serde_json::from_str(r###""auto""###).expect("source-backed settings default")
}
fn decode_acp_registry_settings_distribution<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<AcpRegistryDistributionPreference, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_acp_registry_settings_distribution());
    }

    let value: AcpRegistryDistributionPreference =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_acp_registry_settings_custom_models() -> Vec<String> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_acp_registry_settings_custom_models<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<String>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_acp_registry_settings_custom_models());
    }

    let value: Vec<String> = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for AcpRegistrySettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"source":"registry","enabled":true,"agentId":"","commandPath":"","commandArgs":[],"authMethodId":"","distribution":"auto","customModels":[]}"###).expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitSourceConfig {
    pub kind: UsageLimitSourceKind,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub label: Option<Option<TrimmedNonEmptyString>>,

    pub url: TrimmedNonEmptyString,

    #[serde(
        default = "default_usage_limit_source_config_management_key",
        deserialize_with = "decode_usage_limit_source_config_management_key"
    )]
    pub management_key: TrimmedString,

    #[serde(
        default = "default_usage_limit_source_config_enabled",
        deserialize_with = "decode_usage_limit_source_config_enabled"
    )]
    pub enabled: bool,
}

fn default_usage_limit_source_config_management_key() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_usage_limit_source_config_management_key<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_usage_limit_source_config_management_key());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_usage_limit_source_config_enabled() -> bool {
    serde_json::from_str(r###"true"###).expect("source-backed settings default")
}
fn decode_usage_limit_source_config_enabled<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_usage_limit_source_config_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BitbucketSettings {
    #[serde(
        default = "default_bitbucket_settings_email",
        deserialize_with = "decode_bitbucket_settings_email"
    )]
    pub email: TrimmedString,

    #[serde(
        default = "default_bitbucket_settings_access_token",
        deserialize_with = "decode_bitbucket_settings_access_token"
    )]
    pub access_token: TrimmedString,

    #[serde(
        default = "default_bitbucket_settings_api_token",
        deserialize_with = "decode_bitbucket_settings_api_token"
    )]
    pub api_token: TrimmedString,
}

fn default_bitbucket_settings_email() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_bitbucket_settings_email<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_bitbucket_settings_email());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_bitbucket_settings_access_token() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_bitbucket_settings_access_token<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_bitbucket_settings_access_token());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_bitbucket_settings_api_token() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_bitbucket_settings_api_token<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_bitbucket_settings_api_token());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for BitbucketSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"email":"","accessToken":"","apiToken":""}"###)
            .expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubHostSettings {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub account: Option<TrimmedNonEmptyString>,

    #[serde(
        default = "default_git_hub_host_settings_enabled",
        deserialize_with = "decode_git_hub_host_settings_enabled"
    )]
    pub enabled: bool,
}

fn default_git_hub_host_settings_enabled() -> bool {
    serde_json::from_str(r###"true"###).expect("source-backed settings default")
}
fn decode_git_hub_host_settings_enabled<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_git_hub_host_settings_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for GitHubHostSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"enabled":true}"###).expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubSettings {
    #[serde(
        default = "default_git_hub_settings_hosts",
        deserialize_with = "decode_git_hub_settings_hosts"
    )]
    pub hosts: BTreeMap<GitHubHost, GitHubHostSettings>,

    #[serde(
        default = "default_git_hub_settings_tokens",
        deserialize_with = "decode_git_hub_settings_tokens"
    )]
    pub tokens: BTreeMap<GitHubHost, TrimmedString>,
}

fn default_git_hub_settings_hosts() -> BTreeMap<GitHubHost, GitHubHostSettings> {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_git_hub_settings_hosts<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<GitHubHost, GitHubHostSettings>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_git_hub_settings_hosts());
    }

    let value: BTreeMap<GitHubHost, GitHubHostSettings> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_git_hub_settings_tokens() -> BTreeMap<GitHubHost, TrimmedString> {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_git_hub_settings_tokens<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<GitHubHost, TrimmedString>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_git_hub_settings_tokens());
    }

    let value: BTreeMap<GitHubHost, TrimmedString> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for GitHubSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"hosts":{},"tokens":{}}"###)
            .expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilitySettings {
    #[serde(
        default = "default_observability_settings_otlp_traces_url",
        deserialize_with = "decode_observability_settings_otlp_traces_url"
    )]
    pub otlp_traces_url: TrimmedString,

    #[serde(
        default = "default_observability_settings_otlp_metrics_url",
        deserialize_with = "decode_observability_settings_otlp_metrics_url"
    )]
    pub otlp_metrics_url: TrimmedString,

    #[serde(
        default = "default_observability_settings_otlp_logs_url",
        deserialize_with = "decode_observability_settings_otlp_logs_url"
    )]
    pub otlp_logs_url: TrimmedString,
}

fn default_observability_settings_otlp_traces_url() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_observability_settings_otlp_traces_url<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_observability_settings_otlp_traces_url());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_observability_settings_otlp_metrics_url() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_observability_settings_otlp_metrics_url<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_observability_settings_otlp_metrics_url());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_observability_settings_otlp_logs_url() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_observability_settings_otlp_logs_url<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_observability_settings_otlp_logs_url());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for ObservabilitySettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"otlpTracesUrl":"","otlpMetricsUrl":"","otlpLogsUrl":""}"###)
            .expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlWritingStyleSettings {
    #[serde(
        default = "default_source_control_writing_style_settings_mode",
        deserialize_with = "decode_source_control_writing_style_settings_mode"
    )]
    pub mode: SourceControlWritingStyleMode,

    #[serde(
        default = "default_source_control_writing_style_settings_custom_instructions",
        deserialize_with = "decode_source_control_writing_style_settings_custom_instructions"
    )]
    pub custom_instructions: TrimmedString,

    #[serde(
        default = "default_source_control_writing_style_settings_follow_change_request_templates",
        deserialize_with = "decode_source_control_writing_style_settings_follow_change_request_templates"
    )]
    pub follow_change_request_templates: bool,
}

fn default_source_control_writing_style_settings_mode() -> SourceControlWritingStyleMode {
    serde_json::from_str(r###""repo_conventions""###).expect("source-backed settings default")
}
fn decode_source_control_writing_style_settings_mode<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<SourceControlWritingStyleMode, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_source_control_writing_style_settings_mode());
    }

    let value: SourceControlWritingStyleMode =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_source_control_writing_style_settings_custom_instructions() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_source_control_writing_style_settings_custom_instructions<
    'de,
    D: serde::Deserializer<'de>,
>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_source_control_writing_style_settings_custom_instructions());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_source_control_writing_style_settings_follow_change_request_templates() -> bool {
    serde_json::from_str(r###"true"###).expect("source-backed settings default")
}
fn decode_source_control_writing_style_settings_follow_change_request_templates<
    'de,
    D: serde::Deserializer<'de>,
>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_source_control_writing_style_settings_follow_change_request_templates());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for SourceControlWritingStyleSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"mode":"repo_conventions","customInstructions":"","followChangeRequestTemplates":true}"###).expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundActivityOverrides {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub automatic_git_fetch_interval: Option<DurationMillis>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub provider_health_refresh_interval: Option<DurationMillis>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub host_power_monitor_active_interval: Option<DurationMillis>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub host_power_monitor_idle_interval: Option<DurationMillis>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub idle_client_ttl: Option<DurationMillis>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pause_when_host_locked: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pause_when_host_low_power: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pause_when_client_low_power: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pause_when_on_battery: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundActivitySettings {
    #[serde(
        default = "default_background_activity_settings_schema_version",
        deserialize_with = "decode_background_activity_settings_schema_version"
    )]
    pub schema_version: LiteralInt<1>,

    #[serde(
        default = "default_background_activity_settings_profile",
        deserialize_with = "decode_background_activity_settings_profile"
    )]
    pub profile: BackgroundActivityProfileSelection,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub base_profile: Option<BackgroundActivityProfile>,

    #[serde(
        default = "default_background_activity_settings_overrides",
        deserialize_with = "decode_background_activity_settings_overrides"
    )]
    pub overrides: BackgroundActivityOverrides,
}

fn default_background_activity_settings_schema_version() -> LiteralInt<1> {
    serde_json::from_str(r###"1"###).expect("source-backed settings default")
}
fn decode_background_activity_settings_schema_version<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<LiteralInt<1>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_background_activity_settings_schema_version());
    }

    let value: LiteralInt<1> = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_background_activity_settings_profile() -> BackgroundActivityProfileSelection {
    serde_json::from_str(r###""balanced""###).expect("source-backed settings default")
}
fn decode_background_activity_settings_profile<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BackgroundActivityProfileSelection, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_background_activity_settings_profile());
    }

    let value: BackgroundActivityProfileSelection =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_background_activity_settings_overrides() -> BackgroundActivityOverrides {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_background_activity_settings_overrides<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BackgroundActivityOverrides, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_background_activity_settings_overrides());
    }

    let value: BackgroundActivityOverrides =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for BackgroundActivitySettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"schemaVersion":1,"profile":"balanced","overrides":{}}"###)
            .expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeCleanupRules {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub worktree_after_days: Option<StorageRetentionDays>,

    pub worktree_on_merge: bool,

    pub worktree_on_delete: bool,

    pub worktree_unchanged: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCleanupSettings {
    #[serde(
        default = "default_storage_cleanup_settings_worktree_after_days",
        deserialize_with = "decode_storage_cleanup_settings_worktree_after_days"
    )]
    pub worktree_after_days: Option<StorageRetentionDays>,

    #[serde(
        default = "default_storage_cleanup_settings_worktree_on_merge",
        deserialize_with = "decode_storage_cleanup_settings_worktree_on_merge"
    )]
    pub worktree_on_merge: bool,

    #[serde(
        default = "default_storage_cleanup_settings_worktree_on_delete",
        deserialize_with = "decode_storage_cleanup_settings_worktree_on_delete"
    )]
    pub worktree_on_delete: bool,

    #[serde(
        default = "default_storage_cleanup_settings_worktree_unchanged",
        deserialize_with = "decode_storage_cleanup_settings_worktree_unchanged"
    )]
    pub worktree_unchanged: bool,

    #[serde(
        default = "default_storage_cleanup_settings_browser_artifacts_after_days",
        deserialize_with = "decode_storage_cleanup_settings_browser_artifacts_after_days"
    )]
    pub browser_artifacts_after_days: Option<StorageRetentionDays>,

    #[serde(
        default = "default_storage_cleanup_settings_logs_after_days",
        deserialize_with = "decode_storage_cleanup_settings_logs_after_days"
    )]
    pub logs_after_days: Option<StorageRetentionDays>,
}

fn default_storage_cleanup_settings_worktree_after_days() -> Option<StorageRetentionDays> {
    serde_json::from_str(r###"null"###).expect("source-backed settings default")
}
fn decode_storage_cleanup_settings_worktree_after_days<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<StorageRetentionDays>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    let value: Option<StorageRetentionDays> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_storage_cleanup_settings_worktree_on_merge() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_storage_cleanup_settings_worktree_on_merge<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_storage_cleanup_settings_worktree_on_merge());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_storage_cleanup_settings_worktree_on_delete() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_storage_cleanup_settings_worktree_on_delete<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_storage_cleanup_settings_worktree_on_delete());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_storage_cleanup_settings_worktree_unchanged() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_storage_cleanup_settings_worktree_unchanged<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_storage_cleanup_settings_worktree_unchanged());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_storage_cleanup_settings_browser_artifacts_after_days() -> Option<StorageRetentionDays> {
    serde_json::from_str(r###"null"###).expect("source-backed settings default")
}
fn decode_storage_cleanup_settings_browser_artifacts_after_days<
    'de,
    D: serde::Deserializer<'de>,
>(
    d: D,
) -> Result<Option<StorageRetentionDays>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    let value: Option<StorageRetentionDays> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_storage_cleanup_settings_logs_after_days() -> Option<StorageRetentionDays> {
    serde_json::from_str(r###"null"###).expect("source-backed settings default")
}
fn decode_storage_cleanup_settings_logs_after_days<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<StorageRetentionDays>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    let value: Option<StorageRetentionDays> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for StorageCleanupSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"worktreeAfterDays":null,"worktreeOnMerge":false,"worktreeOnDelete":false,"worktreeUnchanged":false,"browserArtifactsAfterDays":null,"logsAfterDays":null}"###).expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSettingsOverrides {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_cleanup: Option<Option<WorktreeCleanupPolicy>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub default_model_selection: Option<Option<ModelSelection>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub default_runtime_mode: Option<RuntimeMode>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub default_thread_env_mode: Option<ThreadEnvMode>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub new_worktrees_start_from_origin: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_forward_optional",
        serialize_with = "serialize_forward_optional"
    )]
    pub worktree_submodules: Option<Option<WorktreeSubmodules>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub default_auto_pull: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub default_project_scripts: Option<Vec<ProjectScript>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub enable_agent_browser_access: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub enable_agent_device_access: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub text_generation_model_selection: Option<ModelSelection>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub source_control_writer_model_selection: Option<Option<ModelSelection>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub source_control_writing_style: Option<SourceControlWritingStyleSettings>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub remove_agent_credits_on_merge: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch_naming_mode: Option<BranchNamingMode>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch_name_prefix: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch_name_instructions: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pull_request_merge_method: Option<Option<PullRequestMergeMethod>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sidebar_auto_settle_on_merge: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sidebar_auto_settle_after_days: Option<Option<SidebarAutoSettleAfterDays>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub continue_threads_after_server_update: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub response_streaming_mode: Option<ResponseStreamingMode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyProviderSettings {
    #[serde(
        default = "default_legacy_provider_settings_codex",
        deserialize_with = "decode_legacy_provider_settings_codex"
    )]
    pub codex: CodexSettings,

    #[serde(
        default = "default_legacy_provider_settings_claude_agent",
        deserialize_with = "decode_legacy_provider_settings_claude_agent"
    )]
    pub claude_agent: ClaudeSettings,

    #[serde(
        default = "default_legacy_provider_settings_cursor",
        deserialize_with = "decode_legacy_provider_settings_cursor"
    )]
    pub cursor: CursorSettings,

    #[serde(
        default = "default_legacy_provider_settings_grok",
        deserialize_with = "decode_legacy_provider_settings_grok"
    )]
    pub grok: GrokSettings,

    #[serde(
        default = "default_legacy_provider_settings_pi",
        deserialize_with = "decode_legacy_provider_settings_pi"
    )]
    pub pi: PiSettings,

    #[serde(
        default = "default_legacy_provider_settings_opencode",
        deserialize_with = "decode_legacy_provider_settings_opencode"
    )]
    pub opencode: OpenCodeSettings,

    #[serde(
        default = "default_legacy_provider_settings_antigravity",
        deserialize_with = "decode_legacy_provider_settings_antigravity"
    )]
    pub antigravity: AntigravitySettings,
}

fn default_legacy_provider_settings_codex() -> CodexSettings {
    serde_json::from_str(r###"{"enabled":true,"binaryPath":"codex","homePath":"","shadowHomePath":"","launchArgs":"","customModels":[]}"###).expect("source-backed settings default")
}
fn decode_legacy_provider_settings_codex<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<CodexSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_legacy_provider_settings_codex());
    }

    let value: CodexSettings = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_legacy_provider_settings_claude_agent() -> ClaudeSettings {
    serde_json::from_str(r###"{"enabled":true,"binaryPath":"claude","homePath":"","customModels":[],"launchArgs":"","autoCompactWindow":""}"###).expect("source-backed settings default")
}
fn decode_legacy_provider_settings_claude_agent<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<ClaudeSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_legacy_provider_settings_claude_agent());
    }

    let value: ClaudeSettings = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_legacy_provider_settings_cursor() -> CursorSettings {
    serde_json::from_str(r###"{"enabled":false,"customModels":[]}"###)
        .expect("source-backed settings default")
}
fn decode_legacy_provider_settings_cursor<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<CursorSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_legacy_provider_settings_cursor());
    }

    let value: CursorSettings = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_legacy_provider_settings_grok() -> GrokSettings {
    serde_json::from_str(r###"{"enabled":false,"binaryPath":"grok","customModels":[]}"###)
        .expect("source-backed settings default")
}
fn decode_legacy_provider_settings_grok<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<GrokSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_legacy_provider_settings_grok());
    }

    let value: GrokSettings = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_legacy_provider_settings_pi() -> PiSettings {
    serde_json::from_str(
        r###"{"enabled":false,"binaryPath":"pi","launchArgs":"","customModels":[]}"###,
    )
    .expect("source-backed settings default")
}
fn decode_legacy_provider_settings_pi<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<PiSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_legacy_provider_settings_pi());
    }

    let value: PiSettings = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_legacy_provider_settings_opencode() -> OpenCodeSettings {
    serde_json::from_str(r###"{"enabled":false,"binaryPath":"opencode","serverUrl":"","serverPassword":"","customModels":[]}"###).expect("source-backed settings default")
}
fn decode_legacy_provider_settings_opencode<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<OpenCodeSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_legacy_provider_settings_opencode());
    }

    let value: OpenCodeSettings =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_legacy_provider_settings_antigravity() -> AntigravitySettings {
    serde_json::from_str(r###"{"enabled":false,"authMethod":"oauth-personal","apiKey":"","gcpProject":"","gcpLocation":"","binaryPath":"","customModels":[]}"###).expect("source-backed settings default")
}
fn decode_legacy_provider_settings_antigravity<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<AntigravitySettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_legacy_provider_settings_antigravity());
    }

    let value: AntigravitySettings =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for LegacyProviderSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"codex":{"enabled":true,"binaryPath":"codex","homePath":"","shadowHomePath":"","launchArgs":"","customModels":[]},"claudeAgent":{"enabled":true,"binaryPath":"claude","homePath":"","customModels":[],"launchArgs":"","autoCompactWindow":""},"cursor":{"enabled":false,"customModels":[]},"grok":{"enabled":false,"binaryPath":"grok","customModels":[]},"pi":{"enabled":false,"binaryPath":"pi","launchArgs":"","customModels":[]},"opencode":{"enabled":false,"binaryPath":"opencode","serverUrl":"","serverPassword":"","customModels":[]},"antigravity":{"enabled":false,"authMethod":"oauth-personal","apiKey":"","gcpProject":"","gcpLocation":"","binaryPath":"","customModels":[]}}"###).expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    #[serde(flatten, skip_serializing)]
    _object_fields: crate::base::DiscardUnknownFields,
    #[serde(
        default = "default_server_settings_worktree_cleanup",
        deserialize_with = "decode_server_settings_worktree_cleanup"
    )]
    pub worktree_cleanup: Option<WorktreeCleanupPolicy>,

    #[serde(
        default = "default_server_settings_storage_cleanup",
        deserialize_with = "decode_server_settings_storage_cleanup"
    )]
    pub storage_cleanup: StorageCleanupSettings,

    #[serde(
        default = "default_server_settings_worktrees_directory",
        deserialize_with = "decode_server_settings_worktrees_directory"
    )]
    pub worktrees_directory: TrimmedString,

    #[serde(
        default = "default_server_settings_previous_worktrees_directories",
        deserialize_with = "decode_server_settings_previous_worktrees_directories"
    )]
    pub previous_worktrees_directories: Vec<TrimmedString>,

    #[serde(
        default = "default_server_settings_response_streaming_mode",
        deserialize_with = "decode_server_settings_response_streaming_mode"
    )]
    pub response_streaming_mode: ResponseStreamingMode,

    #[serde(
        default = "default_server_settings_enable_provider_update_checks",
        deserialize_with = "decode_server_settings_enable_provider_update_checks"
    )]
    pub enable_provider_update_checks: bool,

    #[serde(
        default = "default_server_settings_continue_threads_after_server_update",
        deserialize_with = "decode_server_settings_continue_threads_after_server_update"
    )]
    pub continue_threads_after_server_update: bool,

    #[serde(
        default = "default_server_settings_enable_agent_browser_access",
        deserialize_with = "decode_server_settings_enable_agent_browser_access"
    )]
    pub enable_agent_browser_access: bool,

    #[serde(
        default = "default_server_settings_project_agent_browser_access_overrides",
        deserialize_with = "decode_server_settings_project_agent_browser_access_overrides"
    )]
    pub project_agent_browser_access_overrides: BTreeMap<ProjectId, bool>,

    #[serde(
        default = "default_server_settings_default_auto_pull",
        deserialize_with = "decode_server_settings_default_auto_pull"
    )]
    pub default_auto_pull: bool,

    #[serde(
        default = "default_server_settings_default_project_scripts",
        deserialize_with = "decode_server_settings_default_project_scripts"
    )]
    pub default_project_scripts: Vec<ProjectScript>,

    #[serde(
        default = "default_server_settings_project_script_overrides",
        deserialize_with = "decode_server_settings_project_script_overrides"
    )]
    pub project_script_overrides: BTreeMap<ProjectId, Option<Vec<ProjectScript>>>,

    #[serde(
        default = "default_server_settings_project_auto_pull_overrides",
        deserialize_with = "decode_server_settings_project_auto_pull_overrides"
    )]
    pub project_auto_pull_overrides: BTreeMap<ProjectId, bool>,

    #[serde(
        default = "default_server_settings_default_model_selection",
        deserialize_with = "decode_server_settings_default_model_selection"
    )]
    pub default_model_selection: Option<ModelSelection>,

    #[serde(
        default = "default_server_settings_default_runtime_mode",
        deserialize_with = "decode_server_settings_default_runtime_mode"
    )]
    pub default_runtime_mode: RuntimeMode,

    #[serde(
        default = "default_server_settings_project_settings_overrides",
        deserialize_with = "decode_server_settings_project_settings_overrides"
    )]
    pub project_settings_overrides: BTreeMap<ProjectId, ProjectSettingsOverrides>,

    #[serde(
        default = "default_server_settings_project_settings_folded",
        deserialize_with = "decode_server_settings_project_settings_folded"
    )]
    pub project_settings_folded: bool,

    #[serde(
        default = "default_server_settings_enable_agent_device_access",
        deserialize_with = "decode_server_settings_enable_agent_device_access"
    )]
    pub enable_agent_device_access: bool,

    #[serde(
        default = "default_server_settings_enable_device_support",
        deserialize_with = "decode_server_settings_enable_device_support"
    )]
    pub enable_device_support: bool,

    #[serde(
        default = "default_server_settings_device_onboarding_completed",
        deserialize_with = "decode_server_settings_device_onboarding_completed"
    )]
    pub device_onboarding_completed: bool,

    #[serde(
        default = "default_server_settings_device_hosts",
        deserialize_with = "decode_server_settings_device_hosts"
    )]
    pub device_hosts: SshDeviceHostConfigs,

    #[serde(
        default = "default_server_settings_sidebar_auto_settle_after_days",
        deserialize_with = "decode_server_settings_sidebar_auto_settle_after_days"
    )]
    pub sidebar_auto_settle_after_days: Option<SidebarAutoSettleAfterDays>,

    #[serde(
        default = "default_server_settings_snooze_limited_threads",
        deserialize_with = "decode_server_settings_snooze_limited_threads"
    )]
    pub snooze_limited_threads: bool,

    #[serde(
        default = "default_server_settings_auto_resume_limited_threads",
        deserialize_with = "decode_server_settings_auto_resume_limited_threads"
    )]
    pub auto_resume_limited_threads: bool,

    #[serde(
        default = "default_server_settings_sidebar_auto_settle_on_merge",
        deserialize_with = "decode_server_settings_sidebar_auto_settle_on_merge"
    )]
    pub sidebar_auto_settle_on_merge: bool,

    #[serde(
        default = "default_server_settings_background_activity",
        deserialize_with = "decode_server_settings_background_activity"
    )]
    pub background_activity: BackgroundActivitySettings,

    #[serde(
        default = "default_server_settings_automatic_git_fetch_interval",
        deserialize_with = "decode_server_settings_automatic_git_fetch_interval"
    )]
    pub automatic_git_fetch_interval: DurationMillis,

    #[serde(
        default = "default_server_settings_provider_health_refresh_interval",
        deserialize_with = "decode_server_settings_provider_health_refresh_interval"
    )]
    pub provider_health_refresh_interval: DurationMillis,

    #[serde(
        default = "default_server_settings_background_activity_profile",
        deserialize_with = "decode_server_settings_background_activity_profile"
    )]
    pub background_activity_profile: BackgroundActivityProfile,

    #[serde(
        default = "default_server_settings_default_theme",
        deserialize_with = "decode_server_settings_default_theme"
    )]
    pub default_theme: BoundedString<64>,

    #[serde(
        default = "default_server_settings_default_theme_set_at",
        deserialize_with = "decode_server_settings_default_theme_set_at"
    )]
    pub default_theme_set_at: BoundedString<64>,

    #[serde(
        default = "default_server_settings_environment_icon",
        deserialize_with = "decode_server_settings_environment_icon"
    )]
    pub environment_icon: Option<EnvironmentMachineKind>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_omitted_unknown"
    )]
    pub default_thread_env_mode: Option<ThreadEnvMode>,

    #[serde(
        default = "default_server_settings_new_worktrees_start_from_origin",
        deserialize_with = "decode_server_settings_new_worktrees_start_from_origin"
    )]
    pub new_worktrees_start_from_origin: bool,

    #[serde(
        default = "default_server_settings_worktree_submodules",
        deserialize_with = "decode_server_settings_worktree_submodules"
    )]
    pub worktree_submodules: Option<WorktreeSubmodules>,

    #[serde(
        default = "default_server_settings_add_project_base_directory",
        deserialize_with = "decode_server_settings_add_project_base_directory"
    )]
    pub add_project_base_directory: TrimmedString,

    #[serde(
        default = "default_server_settings_text_generation_model_selection",
        deserialize_with = "decode_server_settings_text_generation_model_selection"
    )]
    pub text_generation_model_selection: ModelSelection,

    #[serde(
        default = "default_server_settings_branch_naming_mode",
        deserialize_with = "decode_server_settings_branch_naming_mode"
    )]
    pub branch_naming_mode: BranchNamingMode,

    #[serde(
        default = "default_server_settings_branch_name_prefix",
        deserialize_with = "decode_server_settings_branch_name_prefix"
    )]
    pub branch_name_prefix: TrimmedString,

    #[serde(
        default = "default_server_settings_branch_name_instructions",
        deserialize_with = "decode_server_settings_branch_name_instructions"
    )]
    pub branch_name_instructions: TrimmedString,

    #[serde(
        default = "default_server_settings_remove_agent_credits_on_merge",
        deserialize_with = "decode_server_settings_remove_agent_credits_on_merge"
    )]
    pub remove_agent_credits_on_merge: bool,

    #[serde(
        default = "default_server_settings_source_control_writing_style",
        deserialize_with = "decode_server_settings_source_control_writing_style"
    )]
    pub source_control_writing_style: SourceControlWritingStyleSettings,

    #[serde(
        default = "default_server_settings_source_control_writer_model_selection",
        deserialize_with = "decode_server_settings_source_control_writer_model_selection"
    )]
    pub source_control_writer_model_selection: Option<ModelSelection>,

    #[serde(
        default = "default_server_settings_pull_request_merge_method",
        deserialize_with = "decode_server_settings_pull_request_merge_method"
    )]
    pub pull_request_merge_method: Option<PullRequestMergeMethod>,

    #[serde(
        default = "default_server_settings_providers",
        deserialize_with = "decode_server_settings_providers"
    )]
    pub providers: LegacyProviderSettings,

    #[serde(
        default = "default_server_settings_provider_instances",
        deserialize_with = "decode_server_settings_provider_instances"
    )]
    pub provider_instances: ProviderInstanceConfigMap,

    #[serde(
        default = "default_server_settings_observability",
        deserialize_with = "decode_server_settings_observability"
    )]
    pub observability: ObservabilitySettings,

    #[serde(
        default = "default_server_settings_bitbucket",
        deserialize_with = "decode_server_settings_bitbucket"
    )]
    pub bitbucket: BitbucketSettings,

    #[serde(
        default = "default_server_settings_github",
        deserialize_with = "decode_server_settings_github"
    )]
    pub github: GitHubSettings,

    #[serde(
        default = "default_server_settings_usage_limit_sources",
        deserialize_with = "decode_server_settings_usage_limit_sources"
    )]
    pub usage_limit_sources: BTreeMap<String, UsageLimitSourceConfig>,

    #[serde(
        default = "default_server_settings_cursor_keychain_usage_enabled",
        deserialize_with = "decode_server_settings_cursor_keychain_usage_enabled"
    )]
    pub cursor_keychain_usage_enabled: bool,

    #[serde(
        default = "default_server_settings_usage_price_overrides",
        deserialize_with = "decode_server_settings_usage_price_overrides"
    )]
    pub usage_price_overrides: BTreeMap<TrimmedNonEmptyString, UsageModelPriceOverride>,

    #[serde(
        default = "default_server_settings_usage_model_aliases",
        deserialize_with = "decode_server_settings_usage_model_aliases"
    )]
    pub usage_model_aliases: BTreeMap<TrimmedNonEmptyString, TrimmedNonEmptyString>,
}

fn default_server_settings_worktree_cleanup() -> Option<WorktreeCleanupPolicy> {
    serde_json::from_str(r###"null"###).expect("source-backed settings default")
}
fn decode_server_settings_worktree_cleanup<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<WorktreeCleanupPolicy>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    let value: Option<WorktreeCleanupPolicy> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_storage_cleanup() -> StorageCleanupSettings {
    serde_json::from_str(r###"{"worktreeAfterDays":null,"worktreeOnMerge":false,"worktreeOnDelete":false,"worktreeUnchanged":false,"browserArtifactsAfterDays":null,"logsAfterDays":null}"###).expect("source-backed settings default")
}
fn decode_server_settings_storage_cleanup<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<StorageCleanupSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_storage_cleanup());
    }

    let value: StorageCleanupSettings =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_worktrees_directory() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_server_settings_worktrees_directory<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_worktrees_directory());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_previous_worktrees_directories() -> Vec<TrimmedString> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_server_settings_previous_worktrees_directories<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<TrimmedString>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_previous_worktrees_directories());
    }

    let value: Vec<TrimmedString> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_response_streaming_mode() -> ResponseStreamingMode {
    serde_json::from_str(r###""paragraph""###).expect("source-backed settings default")
}
fn decode_server_settings_response_streaming_mode<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<ResponseStreamingMode, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_response_streaming_mode());
    }

    let value: ResponseStreamingMode =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_enable_provider_update_checks() -> bool {
    serde_json::from_str(r###"true"###).expect("source-backed settings default")
}
fn decode_server_settings_enable_provider_update_checks<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_enable_provider_update_checks());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_continue_threads_after_server_update() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_server_settings_continue_threads_after_server_update<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_continue_threads_after_server_update());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_enable_agent_browser_access() -> bool {
    serde_json::from_str(r###"true"###).expect("source-backed settings default")
}
fn decode_server_settings_enable_agent_browser_access<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_enable_agent_browser_access());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_project_agent_browser_access_overrides() -> BTreeMap<ProjectId, bool> {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_server_settings_project_agent_browser_access_overrides<
    'de,
    D: serde::Deserializer<'de>,
>(
    d: D,
) -> Result<BTreeMap<ProjectId, bool>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_project_agent_browser_access_overrides());
    }

    let value: BTreeMap<ProjectId, bool> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_default_auto_pull() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_server_settings_default_auto_pull<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_default_auto_pull());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_default_project_scripts() -> Vec<ProjectScript> {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_server_settings_default_project_scripts<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<ProjectScript>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_default_project_scripts());
    }

    let value: Vec<ProjectScript> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_project_script_overrides()
-> BTreeMap<ProjectId, Option<Vec<ProjectScript>>> {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_server_settings_project_script_overrides<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<ProjectId, Option<Vec<ProjectScript>>>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_project_script_overrides());
    }

    let value: BTreeMap<ProjectId, Option<Vec<ProjectScript>>> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_project_auto_pull_overrides() -> BTreeMap<ProjectId, bool> {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_server_settings_project_auto_pull_overrides<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<ProjectId, bool>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_project_auto_pull_overrides());
    }

    let value: BTreeMap<ProjectId, bool> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_default_model_selection() -> Option<ModelSelection> {
    serde_json::from_str(r###"null"###).expect("source-backed settings default")
}
fn decode_server_settings_default_model_selection<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<ModelSelection>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    let value: Option<ModelSelection> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_default_runtime_mode() -> RuntimeMode {
    serde_json::from_str(r###""full-access""###).expect("source-backed settings default")
}
fn decode_server_settings_default_runtime_mode<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<RuntimeMode, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_default_runtime_mode());
    }

    let value: RuntimeMode = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_project_settings_overrides()
-> BTreeMap<ProjectId, ProjectSettingsOverrides> {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_server_settings_project_settings_overrides<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<ProjectId, ProjectSettingsOverrides>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_project_settings_overrides());
    }

    let value: BTreeMap<ProjectId, ProjectSettingsOverrides> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_project_settings_folded() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_server_settings_project_settings_folded<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_project_settings_folded());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_enable_agent_device_access() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_server_settings_enable_agent_device_access<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_enable_agent_device_access());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_enable_device_support() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_server_settings_enable_device_support<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_enable_device_support());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_device_onboarding_completed() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_server_settings_device_onboarding_completed<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_device_onboarding_completed());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_device_hosts() -> SshDeviceHostConfigs {
    serde_json::from_str(r###"[]"###).expect("source-backed settings default")
}
fn decode_server_settings_device_hosts<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<SshDeviceHostConfigs, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_device_hosts());
    }

    let value: SshDeviceHostConfigs =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_sidebar_auto_settle_after_days() -> Option<SidebarAutoSettleAfterDays> {
    serde_json::from_str(r###"3"###).expect("source-backed settings default")
}
fn decode_server_settings_sidebar_auto_settle_after_days<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<SidebarAutoSettleAfterDays>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    let value: Option<SidebarAutoSettleAfterDays> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_snooze_limited_threads() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_server_settings_snooze_limited_threads<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_snooze_limited_threads());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_auto_resume_limited_threads() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_server_settings_auto_resume_limited_threads<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_auto_resume_limited_threads());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_sidebar_auto_settle_on_merge() -> bool {
    serde_json::from_str(r###"true"###).expect("source-backed settings default")
}
fn decode_server_settings_sidebar_auto_settle_on_merge<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_sidebar_auto_settle_on_merge());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_background_activity() -> BackgroundActivitySettings {
    serde_json::from_str(r###"{"schemaVersion":1,"profile":"balanced","overrides":{}}"###)
        .expect("source-backed settings default")
}
fn decode_server_settings_background_activity<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BackgroundActivitySettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_background_activity());
    }

    let value: BackgroundActivitySettings =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_automatic_git_fetch_interval() -> DurationMillis {
    serde_json::from_str(r###"30000"###).expect("source-backed settings default")
}
fn decode_server_settings_automatic_git_fetch_interval<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<DurationMillis, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_automatic_git_fetch_interval());
    }

    let value: DurationMillis = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_provider_health_refresh_interval() -> DurationMillis {
    serde_json::from_str(r###"300000"###).expect("source-backed settings default")
}
fn decode_server_settings_provider_health_refresh_interval<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<DurationMillis, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_provider_health_refresh_interval());
    }

    let value: DurationMillis = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_background_activity_profile() -> BackgroundActivityProfile {
    serde_json::from_str(r###""balanced""###).expect("source-backed settings default")
}
fn decode_server_settings_background_activity_profile<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BackgroundActivityProfile, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_background_activity_profile());
    }

    let value: BackgroundActivityProfile =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_default_theme() -> BoundedString<64> {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_server_settings_default_theme<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BoundedString<64>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_default_theme());
    }

    let value: BoundedString<64> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_default_theme_set_at() -> BoundedString<64> {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_server_settings_default_theme_set_at<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BoundedString<64>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_default_theme_set_at());
    }

    let value: BoundedString<64> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_environment_icon() -> Option<EnvironmentMachineKind> {
    serde_json::from_str(r###"null"###).expect("source-backed settings default")
}
fn decode_server_settings_environment_icon<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<EnvironmentMachineKind>, D::Error> {
    Ok(serde_json::from_value(serde_json::Value::deserialize(d)?).ok())
}

fn default_server_settings_new_worktrees_start_from_origin() -> bool {
    serde_json::from_str(r###"true"###).expect("source-backed settings default")
}
fn decode_server_settings_new_worktrees_start_from_origin<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_new_worktrees_start_from_origin());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_worktree_submodules() -> Option<WorktreeSubmodules> {
    serde_json::from_str(r###"null"###).expect("source-backed settings default")
}
fn decode_server_settings_worktree_submodules<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<WorktreeSubmodules>, D::Error> {
    Ok(serde_json::from_value(serde_json::Value::deserialize(d)?).ok())
}

fn default_server_settings_add_project_base_directory() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_server_settings_add_project_base_directory<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_add_project_base_directory());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_text_generation_model_selection() -> ModelSelection {
    serde_json::from_str(r###"{"instanceId":"codex","model":"gpt-6-luna","options":[{"id":"reasoningEffort","value":"low"}]}"###).expect("source-backed settings default")
}
fn decode_server_settings_text_generation_model_selection<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<ModelSelection, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_text_generation_model_selection());
    }

    let value: ModelSelection = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_branch_naming_mode() -> BranchNamingMode {
    serde_json::from_str(r###""static""###).expect("source-backed settings default")
}
fn decode_server_settings_branch_naming_mode<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BranchNamingMode, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_branch_naming_mode());
    }

    let value: BranchNamingMode =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_branch_name_prefix() -> TrimmedString {
    serde_json::from_str(r###""t3""###).expect("source-backed settings default")
}
fn decode_server_settings_branch_name_prefix<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_branch_name_prefix());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_branch_name_instructions() -> TrimmedString {
    serde_json::from_str(r###""""###).expect("source-backed settings default")
}
fn decode_server_settings_branch_name_instructions<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<TrimmedString, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_branch_name_instructions());
    }

    let value: TrimmedString = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_remove_agent_credits_on_merge() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_server_settings_remove_agent_credits_on_merge<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_remove_agent_credits_on_merge());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_source_control_writing_style() -> SourceControlWritingStyleSettings {
    serde_json::from_str(r###"{"mode":"repo_conventions","customInstructions":"","followChangeRequestTemplates":true}"###).expect("source-backed settings default")
}
fn decode_server_settings_source_control_writing_style<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<SourceControlWritingStyleSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_source_control_writing_style());
    }

    let value: SourceControlWritingStyleSettings =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_source_control_writer_model_selection() -> Option<ModelSelection> {
    serde_json::from_str(r###"null"###).expect("source-backed settings default")
}
fn decode_server_settings_source_control_writer_model_selection<
    'de,
    D: serde::Deserializer<'de>,
>(
    d: D,
) -> Result<Option<ModelSelection>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    let value: Option<ModelSelection> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_pull_request_merge_method() -> Option<PullRequestMergeMethod> {
    serde_json::from_str(r###"null"###).expect("source-backed settings default")
}
fn decode_server_settings_pull_request_merge_method<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<PullRequestMergeMethod>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    let value: Option<PullRequestMergeMethod> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_providers() -> LegacyProviderSettings {
    serde_json::from_str(r###"{"codex":{"enabled":true,"binaryPath":"codex","homePath":"","shadowHomePath":"","launchArgs":"","customModels":[]},"claudeAgent":{"enabled":true,"binaryPath":"claude","homePath":"","customModels":[],"launchArgs":"","autoCompactWindow":""},"cursor":{"enabled":false,"customModels":[]},"grok":{"enabled":false,"binaryPath":"grok","customModels":[]},"pi":{"enabled":false,"binaryPath":"pi","launchArgs":"","customModels":[]},"opencode":{"enabled":false,"binaryPath":"opencode","serverUrl":"","serverPassword":"","customModels":[]},"antigravity":{"enabled":false,"authMethod":"oauth-personal","apiKey":"","gcpProject":"","gcpLocation":"","binaryPath":"","customModels":[]}}"###).expect("source-backed settings default")
}
fn decode_server_settings_providers<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<LegacyProviderSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_providers());
    }

    let value: LegacyProviderSettings =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_provider_instances() -> ProviderInstanceConfigMap {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_server_settings_provider_instances<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<ProviderInstanceConfigMap, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_provider_instances());
    }

    let value: ProviderInstanceConfigMap =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_observability() -> ObservabilitySettings {
    serde_json::from_str(r###"{"otlpTracesUrl":"","otlpMetricsUrl":"","otlpLogsUrl":""}"###)
        .expect("source-backed settings default")
}
fn decode_server_settings_observability<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<ObservabilitySettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_observability());
    }

    let value: ObservabilitySettings =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_bitbucket() -> BitbucketSettings {
    serde_json::from_str(r###"{"email":"","accessToken":"","apiToken":""}"###)
        .expect("source-backed settings default")
}
fn decode_server_settings_bitbucket<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BitbucketSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_bitbucket());
    }

    let value: BitbucketSettings =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_github() -> GitHubSettings {
    serde_json::from_str(r###"{"hosts":{},"tokens":{}}"###).expect("source-backed settings default")
}
fn decode_server_settings_github<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<GitHubSettings, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_github());
    }

    let value: GitHubSettings = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_usage_limit_sources() -> BTreeMap<String, UsageLimitSourceConfig> {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_server_settings_usage_limit_sources<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, UsageLimitSourceConfig>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_usage_limit_sources());
    }

    let value: BTreeMap<String, UsageLimitSourceConfig> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_cursor_keychain_usage_enabled() -> bool {
    serde_json::from_str(r###"false"###).expect("source-backed settings default")
}
fn decode_server_settings_cursor_keychain_usage_enabled<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_cursor_keychain_usage_enabled());
    }

    let value: bool = serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_usage_price_overrides()
-> BTreeMap<TrimmedNonEmptyString, UsageModelPriceOverride> {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_server_settings_usage_price_overrides<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<TrimmedNonEmptyString, UsageModelPriceOverride>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_usage_price_overrides());
    }

    let value: BTreeMap<TrimmedNonEmptyString, UsageModelPriceOverride> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

fn default_server_settings_usage_model_aliases()
-> BTreeMap<TrimmedNonEmptyString, TrimmedNonEmptyString> {
    serde_json::from_str(r###"{}"###).expect("source-backed settings default")
}
fn decode_server_settings_usage_model_aliases<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<TrimmedNonEmptyString, TrimmedNonEmptyString>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;

    if value.is_null() {
        return Ok(default_server_settings_usage_model_aliases());
    }

    let value: BTreeMap<TrimmedNonEmptyString, TrimmedNonEmptyString> =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;

    Ok(value)
}

impl Default for ServerSettings {
    fn default() -> Self {
        serde_json::from_str(r###"{"worktreeCleanup":null,"storageCleanup":{"worktreeAfterDays":null,"worktreeOnMerge":false,"worktreeOnDelete":false,"worktreeUnchanged":false,"browserArtifactsAfterDays":null,"logsAfterDays":null},"worktreesDirectory":"","previousWorktreesDirectories":[],"responseStreamingMode":"paragraph","enableProviderUpdateChecks":true,"continueThreadsAfterServerUpdate":false,"enableAgentBrowserAccess":true,"projectAgentBrowserAccessOverrides":{},"defaultAutoPull":false,"defaultProjectScripts":[],"projectScriptOverrides":{},"projectAutoPullOverrides":{},"defaultModelSelection":null,"defaultRuntimeMode":"full-access","projectSettingsOverrides":{},"projectSettingsFolded":false,"enableAgentDeviceAccess":false,"enableDeviceSupport":false,"deviceOnboardingCompleted":false,"deviceHosts":[],"sidebarAutoSettleAfterDays":3,"snoozeLimitedThreads":false,"autoResumeLimitedThreads":false,"sidebarAutoSettleOnMerge":true,"backgroundActivity":{"schemaVersion":1,"profile":"balanced","overrides":{}},"automaticGitFetchInterval":30000,"providerHealthRefreshInterval":300000,"backgroundActivityProfile":"balanced","defaultTheme":"","defaultThemeSetAt":"","environmentIcon":null,"newWorktreesStartFromOrigin":true,"worktreeSubmodules":null,"addProjectBaseDirectory":"","textGenerationModelSelection":{"instanceId":"codex","model":"gpt-6-luna","options":[{"id":"reasoningEffort","value":"low"}]},"branchNamingMode":"static","branchNamePrefix":"t3","branchNameInstructions":"","removeAgentCreditsOnMerge":false,"sourceControlWritingStyle":{"mode":"repo_conventions","customInstructions":"","followChangeRequestTemplates":true},"sourceControlWriterModelSelection":null,"pullRequestMergeMethod":null,"providers":{"codex":{"enabled":true,"binaryPath":"codex","homePath":"","shadowHomePath":"","launchArgs":"","customModels":[]},"claudeAgent":{"enabled":true,"binaryPath":"claude","homePath":"","customModels":[],"launchArgs":"","autoCompactWindow":""},"cursor":{"enabled":false,"customModels":[]},"grok":{"enabled":false,"binaryPath":"grok","customModels":[]},"pi":{"enabled":false,"binaryPath":"pi","launchArgs":"","customModels":[]},"opencode":{"enabled":false,"binaryPath":"opencode","serverUrl":"","serverPassword":"","customModels":[]},"antigravity":{"enabled":false,"authMethod":"oauth-personal","apiKey":"","gcpProject":"","gcpLocation":"","binaryPath":"","customModels":[]}},"providerInstances":{},"observability":{"otlpTracesUrl":"","otlpMetricsUrl":"","otlpLogsUrl":""},"bitbucket":{"email":"","accessToken":"","apiToken":""},"github":{"hosts":{},"tokens":{}},"usageLimitSources":{},"cursorKeychainUsageEnabled":false,"usagePriceOverrides":{},"usageModelAliases":{}}"###).expect("source-backed settings default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSettingsPatch {
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
    pub binary_path: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub home_path: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub shadow_home_path: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub launch_args: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub custom_models: Option<Vec<CustomModelSetting>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSettingsPatch {
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
    pub binary_path: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub home_path: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub custom_models: Option<Vec<CustomModelSetting>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub launch_args: Option<String>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub auto_compact_window: Option<ClaudeAutoCompactWindow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorSettingsPatch {
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
    pub custom_models: Option<Vec<CustomModelSetting>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokSettingsPatch {
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
    pub binary_path: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub custom_models: Option<Vec<CustomModelSetting>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySettingsPatch {
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
    pub auth_method: Option<AntigravityAuthMethod>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub api_key: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub gcp_project: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub gcp_location: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub binary_path: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub custom_models: Option<Vec<CustomModelSetting>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiSettingsPatch {
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
    pub binary_path: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub launch_args: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub custom_models: Option<Vec<CustomModelSetting>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCodeSettingsPatch {
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
    pub binary_path: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub server_url: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub server_password: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub custom_models: Option<Vec<CustomModelSetting>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyProviderSettingsPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub codex: Option<CodexSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub claude_agent: Option<ClaudeSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cursor: Option<CursorSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub grok: Option<GrokSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pi: Option<PiSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub opencode: Option<OpenCodeSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub antigravity: Option<AntigravitySettingsPatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelectionPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub instance_id: Option<ProviderInstanceId>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub model: Option<TrimmedNonEmptyString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub options: Option<ProviderOptionSelections>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCleanupSettingsPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_after_days: Option<Option<StorageRetentionDays>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_on_merge: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_on_delete: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_unchanged: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_artifacts_after_days: Option<Option<StorageRetentionDays>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub logs_after_days: Option<Option<StorageRetentionDays>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeCleanupRulesPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_after_days: Option<Option<StorageRetentionDays>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_on_merge: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_on_delete: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_unchanged: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlWritingStyleSettingsPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub mode: Option<SourceControlWritingStyleMode>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub custom_instructions: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub follow_change_request_templates: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundActivitySettingsPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub schema_version: Option<LiteralInt<1>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub profile: Option<BackgroundActivityProfileSelection>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub base_profile: Option<BackgroundActivityProfile>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub overrides: Option<BackgroundActivityOverrides>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilitySettingsPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub otlp_traces_url: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub otlp_metrics_url: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub otlp_logs_url: Option<TrimmedString>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BitbucketSettingsPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub email: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub access_token: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub api_token: Option<TrimmedString>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubSettingsPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub hosts: Option<BTreeMap<GitHubHost, GitHubHostSettings>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub tokens: Option<BTreeMap<GitHubHost, TrimmedString>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettingsPatch {
    #[serde(flatten, skip_serializing)]
    _object_fields: crate::base::DiscardUnknownFields,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_cleanup: Option<Option<WorktreeCleanupPatch>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub storage_cleanup: Option<StorageCleanupSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktrees_directory: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub response_streaming_mode: Option<ResponseStreamingMode>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub enable_provider_update_checks: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub continue_threads_after_server_update: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub enable_agent_browser_access: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub project_agent_browser_access_overrides: Option<BTreeMap<ProjectId, Option<bool>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub default_auto_pull: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub default_project_scripts: Option<Vec<ProjectScript>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub project_script_overrides: Option<BTreeMap<ProjectId, Option<Vec<ProjectScript>>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub project_auto_pull_overrides: Option<BTreeMap<ProjectId, Option<bool>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub default_model_selection: Option<Option<ModelSelection>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub default_runtime_mode: Option<RuntimeMode>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub project_settings_overrides: Option<BTreeMap<ProjectId, Option<ProjectSettingsOverrides>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub enable_agent_device_access: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub enable_device_support: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub device_onboarding_completed: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub device_hosts: Option<SshDeviceHostConfigs>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sidebar_auto_settle_after_days: Option<Option<SidebarAutoSettleAfterDays>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snooze_limited_threads: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub auto_resume_limited_threads: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sidebar_auto_settle_on_merge: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub background_activity: Option<BackgroundActivitySettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub automatic_git_fetch_interval: Option<DurationMillis>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub provider_health_refresh_interval: Option<DurationMillis>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub background_activity_profile: Option<BackgroundActivityProfile>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub environment_icon: Option<Option<EnvironmentMachineKind>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub default_thread_env_mode: Option<Option<ThreadEnvMode>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub new_worktrees_start_from_origin: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktree_submodules: Option<Option<WorktreeSubmodules>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub add_project_base_directory: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub text_generation_model_selection: Option<ModelSelectionPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch_naming_mode: Option<BranchNamingMode>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch_name_prefix: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch_name_instructions: Option<TrimmedString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub remove_agent_credits_on_merge: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub source_control_writing_style: Option<SourceControlWritingStyleSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub source_control_writer_model_selection: Option<Option<ModelSelection>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pull_request_merge_method: Option<Option<PullRequestMergeMethod>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub providers: Option<LegacyProviderSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub provider_instances: Option<ProviderInstanceConfigMap>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub observability: Option<ObservabilitySettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub bitbucket: Option<BitbucketSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub github: Option<GitHubSettingsPatch>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_limit_sources: Option<BTreeMap<String, Option<UsageLimitSourceConfig>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cursor_keychain_usage_enabled: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_price_overrides:
        Option<BTreeMap<TrimmedNonEmptyString, Option<UsageModelPriceOverride>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_model_aliases: Option<BTreeMap<TrimmedNonEmptyString, Option<TrimmedNonEmptyString>>>,
}

pub type StorageRetentionDays = RangeInt<1, 3650>;
pub type SidebarAutoSettleAfterDays = RangeNumber<1, 90>;
pub type UsageTokenPrice = NonNegativeNumber;
pub type DurationMillis = serde_json::Number;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum WorktreeCleanupPolicy {
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "custom")]
    Custom { rules: WorktreeCleanupRules },
}
pub type WorktreeCleanup = Option<WorktreeCleanupPolicy>;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum WorktreeCleanupPatch {
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "custom")]
    Custom { rules: WorktreeCleanupRulesPatch },
}
pub fn deserialize_omitted_unknown<
    'de,
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Ok(serde_json::from_value(serde_json::Value::deserialize(d)?).ok())
}
fn compact_window(value: &str) -> Result<(), ValidationError> {
    if value.is_empty()
        || value == "1000000"
        || value.len() == 6
            && value.as_bytes()[0] != b'0'
            && value.bytes().all(|c| c.is_ascii_digit())
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "empty or an integer from 100000 to 1000000",
        })
    }
}
crate::base::string_type!(ClaudeAutoCompactWindow, compact_window);
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct GitHubHost(pub String);
impl<'de> Deserialize<'de> for GitHubHost {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(
            TrimmedNonEmptyString::deserialize(d)?
                .as_str()
                .to_lowercase(),
        ))
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProviderOptionSelections(pub Vec<ProviderOptionSelection>);
impl<'de> Deserialize<'de> for ProviderOptionSelections {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::provider::decode_provider_options(serde_json::Value::deserialize(d)?)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}
fn ssh_host_id(value: &str) -> Result<(), ValidationError> {
    if value != "local"
        && !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a unique non-local ASCII SSH host id of at most 128 characters",
        })
    }
}
fn ssh_target(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty()
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| !trim_wire_string(&c.to_string()).is_empty())
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "an SSH target without whitespace or a leading dash",
        })
    }
}
crate::base::string_type!(SshDeviceHostId, ssh_host_id);
crate::base::string_type!(SshDeviceHostTarget, ssh_target);
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshDeviceHostConfig {
    pub id: SshDeviceHostId,
    pub label: TrimmedNonEmptyString,
    pub target: SshDeviceHostTarget,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub identity_file: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub port: Option<Option<PortSchema>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(transparent)]
pub struct SshDeviceHostConfigs(pub Vec<SshDeviceHostConfig>);
impl<'de> Deserialize<'de> for SshDeviceHostConfigs {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let hosts = Vec::<SshDeviceHostConfig>::deserialize(d)?;
        let unique: std::collections::BTreeSet<_> = hosts.iter().map(|host| &host.id).collect();
        if unique.len() == hosts.len() {
            Ok(Self(hosts))
        } else {
            Err(serde::de::Error::custom("device host ids must be unique"))
        }
    }
}
impl ServerSettingsPatch {
    pub fn required_scopes(&self) -> Vec<AuthEnvironmentScope> {
        let providers = self.providers.is_some()
            || self.provider_instances.is_some()
            || self.usage_limit_sources.is_some();
        let settings = self.worktree_cleanup.is_some()
            || self.storage_cleanup.is_some()
            || self.worktrees_directory.is_some()
            || self.response_streaming_mode.is_some()
            || self.enable_provider_update_checks.is_some()
            || self.continue_threads_after_server_update.is_some()
            || self.enable_agent_browser_access.is_some()
            || self.project_agent_browser_access_overrides.is_some()
            || self.default_auto_pull.is_some()
            || self.default_project_scripts.is_some()
            || self.project_script_overrides.is_some()
            || self.project_auto_pull_overrides.is_some()
            || self.default_model_selection.is_some()
            || self.default_runtime_mode.is_some()
            || self.project_settings_overrides.is_some()
            || self.enable_agent_device_access.is_some()
            || self.enable_device_support.is_some()
            || self.device_onboarding_completed.is_some()
            || self.device_hosts.is_some()
            || self.sidebar_auto_settle_after_days.is_some()
            || self.snooze_limited_threads.is_some()
            || self.auto_resume_limited_threads.is_some()
            || self.sidebar_auto_settle_on_merge.is_some()
            || self.background_activity.is_some()
            || self.automatic_git_fetch_interval.is_some()
            || self.provider_health_refresh_interval.is_some()
            || self.background_activity_profile.is_some()
            || self.environment_icon.is_some()
            || self.default_thread_env_mode.is_some()
            || self.new_worktrees_start_from_origin.is_some()
            || self.worktree_submodules.is_some()
            || self.add_project_base_directory.is_some()
            || self.text_generation_model_selection.is_some()
            || self.branch_naming_mode.is_some()
            || self.branch_name_prefix.is_some()
            || self.branch_name_instructions.is_some()
            || self.remove_agent_credits_on_merge.is_some()
            || self.source_control_writing_style.is_some()
            || self.source_control_writer_model_selection.is_some()
            || self.pull_request_merge_method.is_some()
            || self.observability.is_some()
            || self.bitbucket.is_some()
            || self.github.is_some()
            || self.cursor_keychain_usage_enabled.is_some()
            || self.usage_price_overrides.is_some()
            || self.usage_model_aliases.is_some();
        let mut scopes = Vec::new();
        if settings || !providers {
            scopes.push(AuthEnvironmentScope::SettingsWrite);
        }
        if providers {
            scopes.push(AuthEnvironmentScope::ProvidersManage);
        }
        scopes
    }
}

pub fn provider_instance_config_enabled_flag(config: &serde_json::Value) -> Option<bool> {
    config.get("enabled").and_then(serde_json::Value::as_bool)
}
pub fn default_enabled_for_driver(driver: &ProviderDriverKind) -> bool {
    !matches!(
        driver.as_str(),
        "cursor" | "grok" | "pi" | "opencode" | "antigravity"
    )
}
pub fn resolve_provider_instance_enabled(instance: &ProviderInstanceConfig) -> bool {
    let config_enabled = instance
        .config
        .as_ref()
        .and_then(provider_instance_config_enabled_flag);
    if instance.enabled == Some(false) || config_enabled == Some(false) {
        return false;
    }
    instance
        .enabled
        .or(config_enabled)
        .unwrap_or_else(|| default_enabled_for_driver(&instance.driver))
}
