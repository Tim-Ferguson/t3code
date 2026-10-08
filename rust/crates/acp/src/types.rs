//! Ergonomic typed core over the generated, recursively validated wire codecs.
use crate::{AcpError, Client, v1, v2};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
fn decode_protocol_version<'de, D: Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    let value = serde_json::Number::deserialize(d)?;
    let number = value
        .as_f64()
        .ok_or_else(|| serde::de::Error::custom("Expected protocol version"))?;
    if number.fract() == 0.0 && (0.0..=65535.0).contains(&number) {
        Ok(number as u16)
    } else {
        Err(serde::de::Error::custom("Expected uint16 protocol version"))
    }
}
pub type Metadata = serde_json::Map<String, Value>;
/// Optional nullable fields retain the distinction between omission and null.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Optional<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}
impl<T> Optional<T> {
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}
impl<T: Serialize> Serialize for Optional<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Value(v) => v.serialize(s),
            Self::Missing | Self::Null => s.serialize_none(),
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Optional<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(d)? {
            Some(v) => Self::Value(v),
            None => Self::Null,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub title: Optional<String>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_capabilities: Option<ClientCapabilities>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub client_info: Optional<Implementation>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}
impl Default for InitializeRequest {
    fn default() -> Self {
        Self {
            protocol_version: 2,
            client_capabilities: None,
            client_info: Optional::Missing,
            meta: Optional::Missing,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    pub mcp_servers: Vec<McpServer>,
    #[serde(rename = "_meta", skip_serializing_if = "Optional::is_missing")]
    pub meta: Optional<Metadata>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum McpServer {
    LegacyStdio {
        name: String,
        command: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        env: Option<Vec<v2::EnvVariable>>,
        #[serde(rename = "_meta", skip_serializing_if = "Optional::is_missing")]
        meta: Optional<Metadata>,
    },
    V2(v2::McpServer),
}
impl<'de> Deserialize<'de> for McpServer {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        if value.get("type").is_some() {
            return serde_json::from_value(value)
                .map(Self::V2)
                .map_err(serde::de::Error::custom);
        }
        #[derive(Deserialize)]
        struct Legacy {
            name: String,
            command: String,
            args: Option<Vec<String>>,
            env: Option<Vec<v2::EnvVariable>>,
            #[serde(rename = "_meta", default)]
            meta: Optional<Metadata>,
        }
        let Legacy {
            name,
            command,
            args,
            env,
            meta,
        } = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self::LegacyStdio {
            name,
            command,
            args,
            env,
            meta,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub session_id: String,
    pub prompt: Vec<v2::ContentBlock>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub usage: Optional<v2::Usage>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FsClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_text_file: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_text_file: Option<bool>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientAuthCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientSessionCapabilities {
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub list: Optional<Metadata>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<FsClientCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub session: Optional<ClientSessionCapabilities>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub plan: Optional<Metadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ClientAuthCapabilities>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub elicitation: Optional<v2::ElicitationCapabilities>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub nes: Optional<v1::ClientNesCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_encodings: Option<Vec<v1::PositionEncodingKind>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded_context: Option<bool>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<bool>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionCapabilities {
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub list: Optional<Metadata>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub fork: Optional<Metadata>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub resume: Optional<Metadata>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub close: Optional<Metadata>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub delete: Optional<Metadata>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub additional_directories: Optional<Metadata>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentAuthCapabilities {
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub logout: Optional<Metadata>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_session: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_capabilities: Option<PromptCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_capabilities: Option<McpCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_capabilities: Option<AgentSessionCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AgentAuthCapabilities>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub providers: Optional<Metadata>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub nes: Optional<v1::NesCapabilities>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub position_encoding: Optional<v1::PositionEncodingKind>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    #[serde(deserialize_with = "decode_protocol_version")]
    pub protocol_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_capabilities: Option<AgentCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_methods: Option<Vec<AuthMethod>>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub agent_info: Optional<Implementation>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub model_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub description: Optional<String>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionModelState {
    pub current_model_id: String,
    pub available_models: Vec<ModelInfo>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionMode {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub description: Optional<String>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionModeState {
    pub current_mode_id: String,
    pub available_modes: Vec<SessionMode>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectOption {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub description: Optional<String>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectGroup {
    pub group_id: String,
    pub name: String,
    pub options: Vec<SessionConfigSelectOption>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOptionInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub description: Optional<String>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub category: Optional<String>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub models: Optional<SessionModelState>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub modes: Optional<SessionModeState>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub config_options: Optional<Vec<SessionConfigOption>>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionResponse {
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub models: Optional<SessionModelState>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub modes: Optional<SessionModeState>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub config_options: Optional<Vec<SessionConfigOption>>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    pub session_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<McpServer>>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionRequest {
    pub session_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<McpServer>>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub replay_from: Optional<v2::ReplayFrom>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ForkSessionRequest {
    pub session_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<McpServer>>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModelRequest {
    pub session_id: String,
    pub model_id: String,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionResponse {
    pub config_options: Vec<SessionConfigOption>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MetadataResponse {
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacySessionConfigSelectGroup {
    pub group: String,
    pub name: String,
    pub options: Vec<SessionConfigSelectOption>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SessionConfigSelectOptions {
    Values(Vec<SessionConfigSelectOption>),
    Groups(Vec<SessionConfigSelectGroup>),
    LegacyGroups(Vec<LegacySessionConfigSelectGroup>),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionConfigOption {
    Select {
        #[serde(flatten)]
        info: SessionConfigOptionInfo,
        #[serde(rename = "currentValue")]
        current_value: String,
        options: SessionConfigSelectOptions,
    },
    Boolean {
        #[serde(flatten)]
        info: SessionConfigOptionInfo,
        #[serde(rename = "currentValue")]
        current_value: bool,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethodInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub description: Optional<String>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthEnvironmentVariable {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AuthMethod {
    Typed(TypedAuthMethod),
    /// ACP v1's original untagged authentication method is preserved verbatim.
    Legacy(AuthMethodInfo),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TypedAuthMethod {
    Agent {
        #[serde(flatten)]
        info: AuthMethodInfo,
    },
    Terminal {
        #[serde(flatten)]
        info: AuthMethodInfo,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<std::collections::BTreeMap<String, String>>,
    },
    EnvVar {
        #[serde(flatten)]
        info: AuthMethodInfo,
        vars: Vec<AuthEnvironmentVariable>,
        #[serde(default, skip_serializing_if = "Optional::is_missing")]
        link: Optional<String>,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SessionConfigValue {
    Id(String),
    Boolean(bool),
}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionRequest {
    pub session_id: String,
    pub config_id: String,
    pub value: SessionConfigValue,
    #[serde(rename = "_meta", default)]
    pub meta: Optional<Metadata>,
}
impl Serialize for SetSessionConfigOptionRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("sessionId", &self.session_id)?;
        map.serialize_entry("configId", &self.config_id)?;
        map.serialize_entry("value", &self.value)?;
        if matches!(self.value, SessionConfigValue::Boolean(_)) {
            map.serialize_entry("type", "boolean")?;
        }
        if !self.meta.is_missing() {
            map.serialize_entry("_meta", &self.meta)?;
        }
        map.end()
    }
}
pub type ForkSessionResponse = NewSessionResponse;
pub type ResumeSessionResponse = LoadSessionResponse;
pub type SetSessionModelResponse = MetadataResponse;
pub type AuthenticateRequest = v2::LoginAuthRequest;
pub type AuthenticateResponse = v2::LoginAuthResponse;
pub type LogoutRequest = v2::LogoutAuthRequest;
pub type LogoutResponse = v2::LogoutAuthResponse;

/// Both ACP diff generations and normalized future content remain representable.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallContent {
    V2(v2::ToolCallContent),
    V1(v1::ToolCallContent),
}
impl<'de> Deserialize<'de> for ToolCallContent {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        if value["type"] == "diff" && value.get("changes").is_none() {
            serde_json::from_value(value)
                .map(Self::V1)
                .map_err(serde::de::Error::custom)
        } else {
            serde_json::from_value(value)
                .map(Self::V2)
                .map_err(serde::de::Error::custom)
        }
    }
}
impl Serialize for ToolCallContent {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::V1(v) => v.serialize(s),
            Self::V2(v) => v.serialize(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub name: Optional<String>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub title: Optional<String>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub kind: Optional<v2::ToolKind>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub status: Optional<v2::ToolCallStatus>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub content: Optional<Vec<ToolCallContent>>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub locations: Optional<Vec<v2::ToolCallLocation>>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub raw_input: Optional<Value>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub raw_output: Optional<Value>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub description: Optional<String>,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub subject: Optional<v2::RequestPermissionSubject>,
    pub tool_call: ToolCallUpdate,
    pub options: Vec<v2::PermissionOption>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageChunkUpdate {
    pub content: v2::ContentBlock,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub message_id: Optional<String>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageUpdate {
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub content: Optional<Vec<v2::ContentBlock>>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Optional::is_missing")]
    pub input: Optional<v2::AvailableCommandInput>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Optional::is_missing"
    )]
    pub meta: Optional<Metadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    Running {
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Optional::is_missing"
        )]
        meta: Optional<Metadata>,
    },
    RequiresAction {
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Optional::is_missing"
        )]
        meta: Optional<Metadata>,
    },
    Idle {
        #[serde(
            rename = "stopReason",
            default,
            skip_serializing_if = "Optional::is_missing"
        )]
        stop_reason: Optional<String>,
        #[serde(default, skip_serializing_if = "Optional::is_missing")]
        usage: Optional<v2::Usage>,
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Optional::is_missing"
        )]
        meta: Optional<Metadata>,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum CompatibleSessionUpdate {
    UserMessageChunk(MessageChunkUpdate),
    AgentMessageChunk(MessageChunkUpdate),
    AgentThoughtChunk(MessageChunkUpdate),
    UserMessage(MessageUpdate),
    AgentMessage(MessageUpdate),
    AgentThought(MessageUpdate),
    ToolCall(ToolCallUpdate),
    ToolCallUpdate(ToolCallUpdate),
    ToolCallContentChunk {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: ToolCallContent,
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Optional::is_missing"
        )]
        meta: Optional<Metadata>,
    },
    StateUpdate {
        #[serde(flatten)]
        state: SessionState,
    },
    AvailableCommandsUpdate {
        #[serde(rename = "availableCommands")]
        available_commands: Vec<AvailableCommand>,
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Optional::is_missing"
        )]
        meta: Optional<Metadata>,
    },
    ConfigOptionUpdate {
        #[serde(rename = "configOptions")]
        config_options: Vec<SessionConfigOption>,
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Optional::is_missing"
        )]
        meta: Optional<Metadata>,
    },
    CompactionUpdate {
        #[serde(rename = "compactionId")]
        compaction_id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Optional::is_missing")]
        summary: Optional<Vec<v2::ContentBlock>>,
        #[serde(default, skip_serializing_if = "Optional::is_missing")]
        error: Optional<String>,
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Optional::is_missing"
        )]
        meta: Optional<Metadata>,
    },
    CompactionSummaryChunk {
        #[serde(rename = "compactionId")]
        compaction_id: String,
        content: v2::ContentBlock,
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Optional::is_missing"
        )]
        meta: Optional<Metadata>,
    },
    Plan {
        entries: Vec<v2::PlanEntry>,
    },
    CurrentModeUpdate {
        #[serde(rename = "currentModeId")]
        current_mode_id: String,
    },
    #[serde(rename = "_t3_unknown")]
    Unknown {
        #[serde(rename = "originalSessionUpdate")]
        original_session_update: String,
        raw: Value,
    },
}
/// Discriminator dispatch avoids cloning a large tool or image update into each
/// union candidate. Known v2-only rows retain their recursively checked codec.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionUpdate {
    Compatible(CompatibleSessionUpdate),
    TerminalUpdate(v2::TerminalUpdate),
    TerminalOutputChunk(v2::TerminalOutputChunk),
    PlanUpdate(v2::PlanUpdate),
    PlanRemoved(v2::PlanRemoved),
    SessionInfoUpdate(v2::SessionInfoUpdate),
    UsageUpdate(v2::UsageUpdate),
}
impl<'de> Deserialize<'de> for SessionUpdate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let parsed = match value["sessionUpdate"].as_str() {
            Some("terminal_update") => serde_json::from_value(value).map(Self::TerminalUpdate),
            Some("terminal_output_chunk") => {
                serde_json::from_value(value).map(Self::TerminalOutputChunk)
            }
            Some("plan_update") => serde_json::from_value(value).map(Self::PlanUpdate),
            Some("plan_removed") => serde_json::from_value(value).map(Self::PlanRemoved),
            Some("session_info_update") => {
                serde_json::from_value(value).map(Self::SessionInfoUpdate)
            }
            Some("usage_update") => serde_json::from_value(value).map(Self::UsageUpdate),
            _ => serde_json::from_value(value).map(Self::Compatible),
        };
        parsed.map_err(serde::de::Error::custom)
    }
}
fn serialize_update<S: Serializer>(
    tag: &str,
    value: &Value,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let object = value
        .as_object()
        .expect("validated session update is an object");
    let mut map = serializer.serialize_map(Some(object.len() + 1))?;
    map.serialize_entry("sessionUpdate", tag)?;
    for (key, value) in object {
        if key != "sessionUpdate" {
            map.serialize_entry(key, value)?;
        }
    }
    map.end()
}
impl Serialize for SessionUpdate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Compatible(v) => v.serialize(s),
            Self::TerminalUpdate(v) => serialize_update("terminal_update", v.as_value(), s),
            Self::TerminalOutputChunk(v) => {
                serialize_update("terminal_output_chunk", v.as_value(), s)
            }
            Self::PlanUpdate(v) => serialize_update("plan_update", v.as_value(), s),
            Self::PlanRemoved(v) => serialize_update("plan_removed", v.as_value(), s),
            Self::SessionInfoUpdate(v) => serialize_update("session_info_update", v.as_value(), s),
            Self::UsageUpdate(v) => serialize_update("usage_update", v.as_value(), s),
        }
    }
}

fn encode_request<T: Serialize>(request: T) -> Result<Value, AcpError> {
    serde_json::to_value(request)
        .map_err(|error| AcpError::Transport(format!("Cannot encode typed ACP request: {error}")))
}
fn decode_result<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, AcpError> {
    serde_json::from_value(value)
        .map_err(|error| AcpError::Transport(format!("Invalid normalized ACP result: {error}")))
}
impl Client {
    pub async fn initialize_typed(
        &self,
        request: InitializeRequest,
    ) -> Result<InitializeResponse, AcpError> {
        decode_result(self.initialize(encode_request(request)?).await?)
    }
    pub async fn prompt_typed(&self, request: PromptRequest) -> Result<PromptResponse, AcpError> {
        decode_result(self.prompt(encode_request(request)?).await?)
    }
    pub async fn cancel_typed(
        &self,
        request: v2::CancelSessionNotification,
    ) -> Result<(), AcpError> {
        self.cancel(request.into_value()).await
    }
}
macro_rules! client_methods {
    ($(($name:ident,$method:ident,$request:ty,$response:ty)),*$(,)?) => {
        impl Client {
            $(pub async fn $name(&self, request: $request) -> Result<$response,AcpError> {
                decode_result(self.call(crate::AgentMethod::$method, encode_request(request)?).await?)
            })*
        }
    };
}
client_methods! {
    (authenticate_typed,Authenticate,AuthenticateRequest,AuthenticateResponse),
    (logout_typed,Logout,LogoutRequest,LogoutResponse),
    (create_session_typed,CreateSession,NewSessionRequest,NewSessionResponse),
    (load_session_typed,LoadSession,LoadSessionRequest,LoadSessionResponse),
    (resume_session_typed,ResumeSession,ResumeSessionRequest,ResumeSessionResponse),
    (fork_session_typed,ForkSession,ForkSessionRequest,ForkSessionResponse),
    (list_sessions_typed,ListSessions,v2::ListSessionsRequest,v2::ListSessionsResponse),
    (close_session_typed,CloseSession,v2::CloseSessionRequest,v2::CloseSessionResponse),
    (delete_session_typed,DeleteSession,v2::DeleteSessionRequest,v2::DeleteSessionResponse),
    (list_providers_typed,ListProviders,v2::ListProvidersRequest,v2::ListProvidersResponse),
    (set_provider_typed,SetProvider,v2::SetProviderRequest,v2::SetProviderResponse),
    (disable_provider_typed,DisableProvider,v2::DisableProviderRequest,v2::DisableProviderResponse),
    (set_session_model_typed,SetSessionModel,SetSessionModelRequest,SetSessionModelResponse),
    (set_session_mode_typed,SetSessionMode,v1::SetSessionModeRequest,v1::SetSessionModeResponse),
    (set_session_config_option_typed,SetSessionConfigOption,SetSessionConfigOptionRequest,SetSessionConfigOptionResponse),
}

impl Client {
    fn handle_typed_request<Q, R>(
        &self,
        method: &str,
        handler: std::sync::Arc<
            dyn Fn(
                    Q,
                    crate::RequestContext,
                ) -> futures_util::future::BoxFuture<'static, Result<R, AcpError>>
                + Send
                + Sync,
        >,
    ) where
        Q: serde::de::DeserializeOwned + Send + 'static,
        R: Serialize + Send + 'static,
    {
        let name = method.to_owned();
        let events = self.handler_events();
        self.handle_request(
            method,
            std::sync::Arc::new(move |value, context| {
                let handler = handler.clone();
                let name = name.clone();
                let events = events.clone();
                Box::pin(async move {
                    let request = serde_json::from_value(value)
                        .map_err(|_| crate::RpcError::invalid_params())?;
                    let request_id = context.request_id.clone();
                    let result = handler(request, context).await.map_err(|cause| {
                        let error =
                            crate::errors::RequestError::from_core_handler_error(cause, &name);
                        let protocol = error.to_protocol_error();
                        let _ = events.send(crate::ClientEvent::RequestHandlerFailed {
                            request_id,
                            error: error.into(),
                        });
                        protocol
                    })?;
                    serde_json::to_value(result).map_err(|_| crate::RpcError::internal())
                })
            }),
        );
    }
    async fn handle_typed_notification<Q>(
        &self,
        method: &str,
        handler: std::sync::Arc<
            dyn Fn(Q) -> futures_util::future::BoxFuture<'static, Result<(), AcpError>>
                + Send
                + Sync,
        >,
    ) where
        Q: serde::de::DeserializeOwned + Send + 'static,
    {
        self.handle_notification(
            method,
            std::sync::Arc::new(move |value| {
                let handler = handler.clone();
                Box::pin(async move { handler(decode_result(value)?).await })
            }),
        )
        .await;
    }
}
macro_rules! client_handlers {
    ($(($name:ident,$wire:literal,$request:ty,$response:ty)),*$(,)?) => {
        impl Client { $(pub fn $name(&self, handler: std::sync::Arc<dyn Fn($request,crate::RequestContext) -> futures_util::future::BoxFuture<'static,Result<$response,AcpError>> + Send + Sync>) {
            self.handle_typed_request($wire,handler);
        })* }
    };
}
client_handlers! {
    (handle_request_permission,"session/request_permission",RequestPermissionRequest,v2::RequestPermissionResponse),
    (handle_elicitation,"elicitation/create",v2::CreateElicitationRequest,v2::CreateElicitationResponse),
    (handle_mcp_connect,"mcp/connect",v2::ConnectMcpRequest,v2::ConnectMcpResponse),
    (handle_mcp_message,"mcp/message",v2::MessageMcpRequest,v2::MessageMcpResponse),
    (handle_mcp_disconnect,"mcp/disconnect",v2::DisconnectMcpRequest,v2::DisconnectMcpResponse),
    (handle_read_text_file,"fs/read_text_file",v1::ReadTextFileRequest,v1::ReadTextFileResponse),
    (handle_write_text_file,"fs/write_text_file",v1::WriteTextFileRequest,v1::WriteTextFileResponse),
    (handle_create_terminal,"terminal/create",v1::CreateTerminalRequest,v1::CreateTerminalResponse),
    (handle_terminal_output,"terminal/output",v1::TerminalOutputRequest,v1::TerminalOutputResponse),
    (handle_terminal_wait_for_exit,"terminal/wait_for_exit",v1::WaitForTerminalExitRequest,v1::WaitForTerminalExitResponse),
    (handle_terminal_kill,"terminal/kill",v1::KillTerminalRequest,v1::KillTerminalResponse),
    (handle_terminal_release,"terminal/release",v1::ReleaseTerminalRequest,v1::ReleaseTerminalResponse),
}
macro_rules! client_notifications {
    ($(($name:ident,$wire:literal,$request:ty)),*$(,)?) => {
        impl Client { $(pub async fn $name(&self, handler: std::sync::Arc<dyn Fn($request) -> futures_util::future::BoxFuture<'static,Result<(),AcpError>> + Send + Sync>) {
            self.handle_typed_notification($wire,handler).await;
        })* }
    };
}
client_notifications! {
    (handle_session_update,"session/update",SessionNotification),
    (handle_elicitation_complete,"elicitation/complete",v2::CompleteElicitationNotification),
    (handle_mcp_notification,"mcp/message",v2::MessageMcpNotification),
}
