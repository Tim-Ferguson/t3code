//! ACP registry setup contracts, shared by catalog services and every client.
use crate::{history::object_struct, *};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct AcpRegistryAgentId(pub BoundedTrimmedString<128>);
impl AcpRegistryAgentId {
    pub fn as_str(&self) -> &str {
        self.0.0.as_str()
    }
}
impl<'de> Deserialize<'de> for AcpRegistryAgentId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = BoundedTrimmedString::<128>::deserialize(d)?;
        let text = value.0.as_str();
        if text
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && text.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            })
        {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom("invalid ACP registry agent ID"))
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpRegistryDistribution {
    Binary,
    Npx,
    Uvx,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpRegistryIntegrity {
    Sha256,
    Registry,
}
fn query<'de, D: serde::Deserializer<'de>>(d: D) -> Result<TrimmedString, D::Error> {
    let value = TrimmedString::deserialize(d)?;
    if value.as_str().encode_utf16().count() <= 120 {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "ACP registry query exceeds120 UTF16 units",
        ))
    }
}
fn literal_true<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    if bool::deserialize(d)? {
        Ok(true)
    } else {
        Err(serde::de::Error::custom("expected true"))
    }
}
object_struct! {pub struct AcpRegistrySearchInput {#[serde(deserialize_with="query")] pub query:TrimmedString,}}
object_struct! {pub struct AcpRegistrySearchAgent {
    pub id:AcpRegistryAgentId,pub name:BoundedTrimmedString<160>,pub version:BoundedTrimmedString<128>,pub description:BoundedString<1024>,pub authors:BoundedVec<BoundedString<256>,16>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub license:Option<BoundedString<128>>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub website:Option<BoundedString<2048>>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub repository:Option<BoundedString<2048>>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub icon:Option<BoundedString<2048>>,
    pub distribution:AcpRegistryDistribution,pub integrity:AcpRegistryIntegrity,
}}
object_struct! {pub struct AcpRegistrySearchResult {pub agents:BoundedVec<AcpRegistrySearchAgent,20>,}}
object_struct! {pub struct AcpRegistryPrepareInput {pub agent_id:AcpRegistryAgentId,}}
object_struct! {pub struct AcpRegistryPrepareResult {pub agent_id:AcpRegistryAgentId,pub version:BoundedTrimmedString<128>,pub distribution:AcpRegistryDistribution,#[serde(deserialize_with="literal_true")] pub prepared:bool,}}
object_struct! {pub struct AcpRegistryManagedBinaryUninstallInput {pub agent_id:AcpRegistryAgentId,}}
object_struct! {pub struct AcpRegistryManagedBinaryUninstallResult {pub agent_id:AcpRegistryAgentId,pub removed:bool,}}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcpRegistryProbeAuthMethodType {
    #[serde(rename = "agent")]
    Agent,
    #[serde(rename = "env_var")]
    EnvVar,
    #[serde(rename = "terminal")]
    Terminal,
}
object_struct! {pub struct AcpRegistryProbeAuthMethod {
    pub id:BoundedTrimmedString<256>,pub name:BoundedTrimmedString<256>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub description:Option<BoundedString<1024>>,
    #[serde(rename="type")] pub kind:AcpRegistryProbeAuthMethodType,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub command:Option<BoundedString<2048>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub env_var_names:Option<BoundedVec<BoundedTrimmedString<256>,16>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub link:Option<BoundedString<2048>>,
}}
object_struct! {pub struct AcpRegistryAcceptUrlAuthInput {
    pub instance_id:ProviderInstanceId,pub elicitation_id:BoundedTrimmedString<256>,
}}
object_struct! {pub struct AcpRegistryAcceptUrlAuthResult {pub accepted:bool,}}
object_struct! {pub struct AcpRegistrySession {
    pub session_id:BoundedTrimmedString<1024>,pub cwd:BoundedTrimmedString<4096>,
    pub additional_directories:BoundedVec<BoundedTrimmedString<4096>,32>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub title:Option<BoundedString<1024>>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub updated_at:Option<BoundedString<128>>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub imported_thread_id:Option<ThreadId>,
}}
object_struct! {pub struct AcpRegistryListSessionsInput {
    pub instance_id:ProviderInstanceId,pub project_id:ProjectId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub cursor:Option<BoundedString<2048>>,
}}
object_struct! {pub struct AcpRegistryListSessionsResult {
    pub sessions:BoundedVec<AcpRegistrySession,256>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub next_cursor:Option<BoundedString<2048>>,
    pub can_load:bool,pub can_resume:bool,pub can_delete:bool,
}}
object_struct! {pub struct AcpRegistryImportSessionInput {
    pub instance_id:ProviderInstanceId,pub project_id:ProjectId,pub session_id:BoundedTrimmedString<1024>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub title:Option<Option<BoundedTrimmedString<1024>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub updated_at:Option<Option<BoundedTrimmedString<128>>>,
}}
object_struct! {pub struct AcpRegistryImportSessionResult {pub thread_id:ThreadId,pub imported:bool,}}
object_struct! {pub struct AcpRegistryDeleteSessionInput {
    pub instance_id:ProviderInstanceId,pub project_id:ProjectId,pub session_id:BoundedTrimmedString<1024>,
}}
object_struct! {pub struct AcpRegistryDeleteSessionResult {#[serde(deserialize_with="literal_true")] pub deleted:bool,}}
object_struct! {pub struct AcpRegistryConfiguredProvider {
    pub api_type:BoundedTrimmedString<64>,pub base_url:BoundedString<2048>,
}}
object_struct! {pub struct AcpRegistryConfigurableProvider {
    pub provider_id:BoundedTrimmedString<256>,pub supported:BoundedVec<BoundedTrimmedString<64>,16>,pub required:bool,
    #[serde(deserialize_with="deserialize_required_nullable")] pub current:Option<AcpRegistryConfiguredProvider>,
}}
object_struct! {pub struct AcpRegistryListProvidersInput {pub instance_id:ProviderInstanceId,pub project_id:ProjectId,}}
object_struct! {pub struct AcpRegistryListProvidersResult {pub providers:BoundedVec<AcpRegistryConfigurableProvider,64>,}}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct AcpRegistryProviderHeaders(pub serde_json::Map<String, serde_json::Value>);
impl<'de> Deserialize<'de> for AcpRegistryProviderHeaders {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        let values = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("expected provider headers object"))?;
        let mut headers = serde_json::Map::new();
        for (key, value) in values {
            let key: BoundedTrimmedString<128> =
                serde_json::from_value(serde_json::json!(key)).map_err(serde::de::Error::custom)?;
            let value: BoundedString<8192> =
                serde_json::from_value(value.clone()).map_err(serde::de::Error::custom)?;
            headers.insert(key.0.to_string(), serde_json::Value::String(value.0));
        }
        Ok(Self(headers))
    }
}
object_struct! {pub struct AcpRegistrySetProviderInput {
    pub instance_id:ProviderInstanceId,pub project_id:ProjectId,pub provider_id:BoundedTrimmedString<256>,
    pub api_type:BoundedTrimmedString<64>,pub base_url:BoundedString<2048>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub headers:Option<AcpRegistryProviderHeaders>,
}}
object_struct! {pub struct AcpRegistrySetProviderResult {#[serde(deserialize_with="literal_true")] pub configured:bool,}}
object_struct! {pub struct AcpRegistryDisableProviderInput {pub instance_id:ProviderInstanceId,pub project_id:ProjectId,pub provider_id:BoundedTrimmedString<256>,}}
object_struct! {pub struct AcpRegistryDisableProviderResult {#[serde(deserialize_with="literal_true")] pub disabled:bool,}}
object_struct! {pub struct AcpRegistryLogoutInput {pub instance_id:ProviderInstanceId,}}
object_struct! {pub struct AcpRegistryLogoutResult {#[serde(deserialize_with="literal_true")] pub logged_out:bool,}}
object_struct! {pub struct AcpRegistryProbeModel {
    pub id:BoundedTrimmedString<256>,pub name:BoundedTrimmedString<256>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub description:Option<BoundedString<1024>>,
}}
object_struct! {#[derive(Default)] pub struct AcpRegistrySessionManagement {
    pub can_list:bool,pub can_load:bool,pub can_resume:bool,pub can_logout:bool,pub can_delete:bool,pub can_configure_providers:bool,
}}
fn default_session_management<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<AcpRegistrySessionManagement, D::Error> {
    Option::<AcpRegistrySessionManagement>::deserialize(d).map(Option::unwrap_or_default)
}
object_struct! {pub struct AcpRegistryProbeResult {
    pub instance_id:ProviderInstanceId,#[serde(deserialize_with="literal_true")] pub ready:bool,
    #[serde(deserialize_with="deserialize_required_nullable")] pub icon:Option<BoundedString<2048>>,
    pub auth_methods:BoundedVec<AcpRegistryProbeAuthMethod,32>,pub models:BoundedVec<AcpRegistryProbeModel,256>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub current_model_id:Option<BoundedTrimmedString<256>>,
    pub config_options:BoundedVec<ProviderOptionDescriptor,16>,
    #[serde(default,deserialize_with="default_session_management")] pub session_management:AcpRegistrySessionManagement,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpRegistryOperationErrorReason {
    AgentNotConfigured,
    AgentNotFound,
    ArchiveInvalid,
    AuthenticationFailed,
    ChecksumMismatch,
    DownloadFailed,
    InstallFailed,
    InstanceNotFound,
    LogoutUnsupported,
    LogoutFailed,
    ProbeFailed,
    ProjectNotFound,
    RegistryUnavailable,
    RunnerUnavailable,
    SessionImportFailed,
    SessionDeleteUnsupported,
    SessionDeleteFailed,
    SessionListUnsupported,
    SessionResumeUnsupported,
    ProvidersUnsupported,
    ProvidersListFailed,
    ProviderConfigurationFailed,
    UnsupportedDistribution,
    UnsupportedPlatform,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcpRegistryOperationErrorTag {
    #[serde(rename = "AcpRegistryOperationError")]
    AcpRegistryOperationError,
}
object_struct! {pub struct AcpRegistryOperationError {
    #[serde(rename="_tag")] pub tag:AcpRegistryOperationErrorTag,
    pub reason:AcpRegistryOperationErrorReason,pub message:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub auth_methods:Option<BoundedVec<AcpRegistryProbeAuthMethod,32>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub auth_action:Option<AcpRegistryUrlAuthAction>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub cause:Option<serde_json::Value>,
}}
