//! Provider setup and explicit sign-in interactions from providerSetup.ts.
use crate::{history::object_struct, *};
use serde::{Deserialize, Serialize};
pub type SetupOperationId = BoundedTrimmedString<128>;
object_struct! {pub struct ProviderSetupInput {pub instance_id:ProviderInstanceId,}}
object_struct! {pub struct CodexAuthCallbackInput {
    pub authorization_url:BoundedString<16384>,pub return_url:BoundedString<4096>,pub environment_id:EnvironmentId,pub instance_id:ProviderInstanceId,pub flow_id:SetupOperationId,
}}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum CodexAuthCallbackState {
    Ready,
    Finished {
        #[serde(rename = "callbackUrl")]
        callback_url: BoundedString<16384>,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderAuthMethodType {
    Agent,
    Terminal,
    Credentials,
}
object_struct! {pub struct ProviderAuthMethod {
    pub id:SetupOperationId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub account_email:Option<Option<TrimmedNonEmptyString>>,
    pub name:TrimmedNonEmptyString,
    #[serde(deserialize_with="deserialize_required_nullable")] pub description:Option<String>,
    #[serde(rename="type")] pub method_type:ProviderAuthMethodType,
}}
object_struct! {pub struct ProviderAuthCredentialField {pub name:SetupOperationId,pub label:TrimmedNonEmptyString,pub secret:bool,}}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ProviderAuthInteraction {
    Browser {
        id: SetupOperationId,
        url: BoundedTrimmedString<16384>,
        #[serde(rename = "requiresConsent")]
        requires_consent: bool,
        #[serde(
            default,
            rename = "acceptsCallback",
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        accepts_callback: Option<bool>,
    },
    DeviceCode {
        id: SetupOperationId,
        url: BoundedTrimmedString<16384>,
        #[serde(rename = "userCode")]
        user_code: BoundedTrimmedString<256>,
    },
    Terminal {
        id: SetupOperationId,
        output: BoundedString<16384>,
        #[serde(
            default,
            rename = "outputOffset",
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        output_offset: Option<NonNegativeInt>,
    },
    Credentials {
        id: SetupOperationId,
        fields: BoundedVec<ProviderAuthCredentialField, 16>,
    },
}
impl ProviderAuthInteraction {
    pub fn id(&self) -> &str {
        match self {
            Self::Browser { id, .. }
            | Self::DeviceCode { id, .. }
            | Self::Terminal { id, .. }
            | Self::Credentials { id, .. } => id.0.as_str(),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderAuthBrowserAction {
    Accept,
    Decline,
}
object_struct! {pub struct ProviderAuthTerminalSize {pub cols:RangeInt<1,500>,pub rows:RangeInt<1,200>,}}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProviderAuthCredentialValues(pub serde_json::Map<String, serde_json::Value>);
impl<'de> Deserialize<'de> for ProviderAuthCredentialValues {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        let values = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("expected credentials object"))?;
        let mut result = serde_json::Map::new();
        for (key, value) in values {
            let key: SetupOperationId =
                serde_json::from_value(serde_json::json!(key)).map_err(serde::de::Error::custom)?;
            let value: BoundedString<16384> =
                serde_json::from_value(value.clone()).map_err(serde::de::Error::custom)?;
            result.insert(key.0.to_string(), serde_json::Value::String(value.0));
        }
        if result.len() > 16 {
            return Err(serde::de::Error::custom("expected at most 16 credentials"));
        }
        Ok(Self(result))
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ProviderAuthResponse {
    Browser {
        action: ProviderAuthBrowserAction,
    },
    Terminal {
        data: BoundedString<4096>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        size: Option<ProviderAuthTerminalSize>,
    },
    Credentials {
        values: ProviderAuthCredentialValues,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderAuthCallbackMode {
    Server,
    Client,
}
object_struct! {pub struct ProviderAuthStartInput {
    pub instance_id:ProviderInstanceId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub method_id:Option<SetupOperationId>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub return_url:Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub callback_mode:Option<ProviderAuthCallbackMode>,
}}
object_struct! {pub struct ProviderAuthRespondInput {pub instance_id:ProviderInstanceId,pub flow_id:SetupOperationId,pub interaction_id:SetupOperationId,pub response:ProviderAuthResponse,}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderAuthPhase {
    Idle,
    Starting,
    Waiting,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderCredentialOwner {
    Provider,
    T3,
}
fn methods<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<ForwardCompatibleArray<ProviderAuthMethod>>, D::Error> {
    let methods = ForwardCompatibleArray::<ProviderAuthMethod>::deserialize(d)?;
    if methods.0.len() > 32 {
        return Err(serde::de::Error::custom(
            "expected at most 32 authentication methods",
        ));
    }
    Ok(Some(methods))
}
object_struct! {pub struct ProviderAuthState {
    pub instance_id:ProviderInstanceId,pub phase:ProviderAuthPhase,
    #[serde(deserialize_with="deserialize_required_nullable")] pub flow_id:Option<SetupOperationId>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub authorization_url:Option<String>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub expires_at:Option<IsoDateTime>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub message:Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="methods")] pub methods:Option<ForwardCompatibleArray<ProviderAuthMethod>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_forward_optional",serialize_with="serialize_forward_optional")] pub interaction:Option<Option<Option<ProviderAuthInteraction>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_forward_optional",serialize_with="serialize_forward_optional")] pub credential_owner:Option<Option<ProviderCredentialOwner>>,
}}
object_struct! {pub struct ProviderAuthCompleteInput {pub instance_id:ProviderInstanceId,pub flow_id:SetupOperationId,pub callback_url:BoundedTrimmedString<16384>,}}
object_struct! {pub struct ProviderAuthCancelInput {pub instance_id:ProviderInstanceId,pub flow_id:SetupOperationId,}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderInstallPhase {
    Idle,
    Downloading,
    Extracting,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderInstallSource {
    Managed,
    Local,
}
object_struct! {pub struct ProviderInstallState {
    pub driver:ProviderDriverKind,
    #[serde(deserialize_with="deserialize_required_nullable")] pub operation_id:Option<SetupOperationId>,
    pub phase:ProviderInstallPhase,pub downloaded_bytes:NonNegativeInt,
    #[serde(deserialize_with="deserialize_required_nullable")] pub total_bytes:Option<NonNegativeInt>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub version:Option<TrimmedNonEmptyString>,
    #[serde(deserialize_with="deserialize_required_nullable")] pub installed_version:Option<TrimmedNonEmptyString>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub executable_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub source:Option<Option<ProviderInstallSource>>,
    pub can_remove:bool,
    #[serde(deserialize_with="deserialize_required_nullable")] pub message:Option<String>,
}}
object_struct! {pub struct ProviderInstallCancelInput {pub instance_id:ProviderInstanceId,pub operation_id:SetupOperationId,}}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderSetupErrorTag {
    ProviderSetupError,
}
object_struct! {pub struct ProviderSetupError {
    #[serde(rename="_tag")] pub tag:ProviderSetupErrorTag,
    pub instance_id:ProviderInstanceId,pub operation:String,pub detail:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub cause:Option<Option<serde_json::Value>>,
}}
impl std::fmt::Display for ProviderSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}
impl std::error::Error for ProviderSetupError {}

fn client_id(value: &str) -> Result<(), ValidationError> {
    let valid = value.strip_prefix("oaiapp_").is_some_and(|tail| {
        !tail.is_empty()
            && tail
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    });
    if valid {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a ChatGPT registration client ID",
        })
    }
}
crate::base::plain_string_type!(ChatGptRegistrationClientId, client_id);
fn redirect_uri(value: &str) -> Result<(), ValidationError> {
    let port = value
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| value.strip_prefix("http://localhost:"))
        .and_then(|tail| tail.strip_suffix("/auth/callback"));
    if port.is_some_and(|port| {
        !port.is_empty()
            && port.len() <= 5
            && port.as_bytes()[0] != b'0'
            && port.bytes().all(|c| c.is_ascii_digit())
    }) {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a loopback ChatGPT redirect URI",
        })
    }
}
crate::base::plain_string_type!(ChatGptRegistrationRedirectUri, redirect_uri);
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ChatGptToken(pub String);
impl<'de> Deserialize<'de> for ChatGptToken {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = BoundedString::<16384>::deserialize(d)?.0;
        if value.is_empty() {
            Err(serde::de::Error::custom("expected non-empty token"))
        } else {
            Ok(Self(value))
        }
    }
}
object_struct! {pub struct ChatGptReconnectProfile {
    pub client_id:ChatGptRegistrationClientId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub subject:Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub email:Option<Option<String>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub redirect_uri:Option<ChatGptRegistrationRedirectUri>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub connection_label:Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub sharing_enabled:Option<bool>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub id_token_hint:Option<BoundedString<16384>>,
}}
object_struct! {pub struct ChatGptTransferredCredentials {
    pub client_id:String,pub access_token:ChatGptToken,
    #[serde(deserialize_with="deserialize_required_nullable")] pub refresh_token:Option<BoundedString<16384>>,
    pub id_token:ChatGptToken,pub issuer:String,pub expires_at:serde_json::Number,
    #[serde(deserialize_with="deserialize_required_nullable")] pub earliest_refresh_at:Option<serde_json::Number>,
    pub scopes:Vec<String>,pub subject:String,
    #[serde(deserialize_with="deserialize_required_nullable")] pub email:Option<String>,
}}
object_struct! {pub struct ChatGptTransferredProfile {pub registration:ChatGptReconnectProfile,pub credentials:ChatGptTransferredCredentials,}}
object_struct! {pub struct ChatGptReconnectProfileInput {pub instance_id:ProviderInstanceId,pub method_id:String,}}
object_struct! {pub struct ChatGptImportProfileInput {pub instance_id:ProviderInstanceId,pub profile:ChatGptTransferredProfile,}}
object_struct! {pub struct ChatGptHandoffInput {pub instance_id:ProviderInstanceId,pub environment_id:EnvironmentId,pub attempt_id:BoundedString<128>,pub return_url:BoundedString<4096>,#[serde(deserialize_with="deserialize_required_nullable")] pub profile:Option<ChatGptReconnectProfile>,}}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
pub enum ChatGptHandoffState {
    Auth { state: ProviderAuthState },
    Finished { profile: ChatGptTransferredProfile },
}
