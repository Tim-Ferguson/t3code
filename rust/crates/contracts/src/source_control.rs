//! `packages/contracts/src/sourceControl.ts`: the providers, change requests, repository
//! lookups, discovery result and the two error shapes of the source-control methods.
//!
//! `Schema.Option` fields travel as `{"_tag":"Some","value":..}` / `{"_tag":"None"}`; the
//! `effect_option` helpers read and write that form.
use crate::base::TrimmedNonEmptyString;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceControlProviderKind {
    Github,
    Gitlab,
    Forgejo,
    AzureDevops,
    Bitbucket,
    Unknown,
}

impl SourceControlProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Forgejo => "forgejo",
            Self::AzureDevops => "azure-devops",
            Self::Bitbucket => "bitbucket",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for SourceControlProviderKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlProviderInfo {
    pub kind: SourceControlProviderKind,
    pub name: String,
    pub base_url: String,
}

/// Serde for `Schema.Option(String)` as the JSON codec writes it.
pub mod effect_option {
    use super::*;

    pub fn serialize<S: Serializer>(
        value: &Option<String>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => json!({"_tag":"Some","value":value}),
            None => json!({"_tag":"None"}),
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<String>, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value["_tag"].as_str() {
            Some("None") => Ok(None),
            Some("Some") => value["value"]
                .as_str()
                .map(|text| Some(text.to_string()))
                .ok_or_else(|| serde::de::Error::custom("Some needs a string value")),
            _ => Err(serde::de::Error::custom("expected an Option")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeRequestState {
    Open,
    Closed,
    Merged,
}

/// `ChangeRequest`. `updated_at` is an ISO-8601 timestamp with milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeRequest {
    pub provider: SourceControlProviderKind,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub state: ChangeRequestState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_draft: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<Option<String>>,
    #[serde(with = "effect_option")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_cross_repository: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_repository_name_with_owner: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_repository_owner_login: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlRepositoryCloneUrls {
    pub name_with_owner: String,
    pub url: String,
    pub ssh_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceControlRepositoryVisibility {
    Private,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceControlCloneProtocol {
    Auto,
    Ssh,
    Https,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlRepositoryInfo {
    pub provider: SourceControlProviderKind,
    pub name_with_owner: String,
    pub url: String,
    pub ssh_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlRepositoryLookupInput {
    pub provider: SourceControlProviderKind,
    pub repository: TrimmedNonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<TrimmedNonEmptyString>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlCloneRepositoryInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<SourceControlProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<TrimmedNonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<TrimmedNonEmptyString>,
    pub destination_path: TrimmedNonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<SourceControlCloneProtocol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlCloneRepositoryResult {
    pub cwd: String,
    pub remote_url: String,
    pub repository: Option<SourceControlRepositoryInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlPublishRepositoryInput {
    pub cwd: TrimmedNonEmptyString,
    pub provider: SourceControlProviderKind,
    pub repository: TrimmedNonEmptyString,
    pub visibility: SourceControlRepositoryVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_name: Option<TrimmedNonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<SourceControlCloneProtocol>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceControlPublishStatus {
    Pushed,
    RemoteAdded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlPublishRepositoryResult {
    pub repository: SourceControlRepositoryInfo,
    pub remote_name: String,
    pub remote_url: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_branch: Option<String>,
    pub status: SourceControlPublishStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceControlDiscoveryStatus {
    Available,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceControlProviderAuthStatus {
    Authenticated,
    Unauthenticated,
    Unknown,
}

/// One login a provider CLI holds. Only GitHub reports these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlProviderAccount {
    pub host: String,
    pub account: String,
    pub active: bool,
    pub authenticated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Set when the login comes from a token variable such as `GH_TOKEN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_variable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlProviderAuth {
    pub status: SourceControlProviderAuthStatus,
    #[serde(with = "effect_option")]
    pub account: Option<String>,
    #[serde(with = "effect_option")]
    pub host: Option<String>,
    #[serde(with = "effect_option")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounts: Option<Vec<SourceControlProviderAccount>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsDriverKindWire {
    Git,
    Jj,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsDiscoveryItem {
    pub kind: VcsDriverKindWire,
    pub implemented: bool,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    pub status: SourceControlDiscoveryStatus,
    #[serde(with = "effect_option")]
    pub version: Option<String>,
    pub install_hint: String,
    #[serde(with = "effect_option")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlProviderDiscoveryItem {
    pub kind: SourceControlProviderKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    pub status: SourceControlDiscoveryStatus,
    #[serde(with = "effect_option")]
    pub version: Option<String>,
    pub install_hint: String,
    #[serde(with = "effect_option")]
    pub detail: Option<String>,
    pub auth: SourceControlProviderAuth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlDiscoveryResult {
    pub version_control_systems: Vec<VcsDiscoveryItem>,
    pub source_control_providers: Vec<SourceControlProviderDiscoveryItem>,
}

/// How a `Schema.Defect` cause reaches the wire: the error's name and message, and the same
/// for the error that caused it. Causes that are not plain errors (schema decode failures)
/// carry no nested cause here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorCause {
    pub name: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<ErrorCause>>,
}

impl ErrorCause {
    pub fn new(name: &str, message: impl Into<String>) -> Self {
        Self { name: name.into(), message: message.into(), cause: None }
    }

    pub fn caused_by(mut self, cause: ErrorCause) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    pub fn with_nested(mut self, cause: Option<ErrorCause>) -> Self {
        self.cause = cause.map(Box::new);
        self
    }
}

/// `SourceControlProviderError`. `cause` is kept as its wire form; the typed host error that
/// produced it stays on the server side of the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceControlProviderError {
    pub provider: SourceControlProviderKind,
    pub operation: String,
    pub cwd: String,
    pub command: Option<String>,
    pub repository: Option<String>,
    pub reference: Option<String>,
    pub detail: String,
    pub cause: Option<ErrorCause>,
}

impl SourceControlProviderError {
    pub fn new(
        provider: SourceControlProviderKind,
        operation: &str,
        cwd: &str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            operation: operation.into(),
            cwd: cwd.into(),
            command: None,
            repository: None,
            reference: None,
            detail: detail.into(),
            cause: None,
        }
    }

    pub fn with_command(mut self, command: Option<&str>) -> Self {
        self.command = command.map(str::to_string);
        self
    }

    pub fn with_repository(mut self, repository: &str) -> Self {
        self.repository = Some(repository.to_string());
        self
    }

    pub fn with_reference(mut self, reference: &str) -> Self {
        self.reference = Some(reference.to_string());
        self
    }

    pub fn with_cause(mut self, name: &str, message: impl Into<String>) -> Self {
        self.cause = Some(ErrorCause::new(name, message));
        self
    }

    pub fn with_cause_chain(mut self, cause: Option<ErrorCause>) -> Self {
        self.cause = cause;
        self
    }

    /// This error as the cause of another.
    pub fn as_cause(&self) -> ErrorCause {
        ErrorCause {
            name: "SourceControlProviderError".into(),
            message: self.message(),
            cause: self.cause.clone().map(Box::new),
        }
    }

    pub fn message(&self) -> String {
        format!(
            "Source control provider {} failed in {}: {}",
            self.provider, self.operation, self.detail
        )
    }

    pub fn wire(&self) -> Value {
        let mut wire = json!({
            "_tag":"SourceControlProviderError",
            "provider":self.provider,
            "operation":self.operation,
            "cwd":self.cwd,
            "detail":self.detail,
        });
        let object = wire.as_object_mut().expect("object");
        for (key, value) in [
            ("command", &self.command),
            ("repository", &self.repository),
            ("reference", &self.reference),
        ] {
            if let Some(value) = value {
                object.insert(key.into(), json!(value));
            }
        }
        if let Some(cause) = &self.cause {
            object.insert("cause".into(), json!(cause));
        }
        wire
    }
}

impl std::fmt::Display for SourceControlProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for SourceControlProviderError {}

/// `SourceControlRepositoryError`, the error of the `sourceControl.*` methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceControlRepositoryError {
    pub provider: SourceControlProviderKind,
    pub operation: String,
    pub detail: String,
    pub cause: Option<ErrorCause>,
}

impl SourceControlRepositoryError {
    pub fn new(
        provider: SourceControlProviderKind,
        operation: &str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            operation: operation.into(),
            detail: detail.into(),
            cause: None,
        }
    }

    pub fn with_cause(mut self, name: &str, message: impl Into<String>) -> Self {
        self.cause = Some(ErrorCause::new(name, message));
        self
    }

    pub fn with_cause_chain(mut self, cause: ErrorCause) -> Self {
        self.cause = Some(cause);
        self
    }

    pub fn message(&self) -> String {
        format!(
            "Source control repository operation {} failed for {}: {}",
            self.operation, self.provider, self.detail
        )
    }

    pub fn wire(&self) -> Value {
        let mut wire = json!({
            "_tag":"SourceControlRepositoryError",
            "provider":self.provider,
            "operation":self.operation,
            "detail":self.detail,
        });
        if let Some(cause) = &self.cause {
            wire.as_object_mut()
                .expect("object")
                .insert("cause".into(), json!(cause));
        }
        wire
    }
}

impl std::fmt::Display for SourceControlRepositoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for SourceControlRepositoryError {}
