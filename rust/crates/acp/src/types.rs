//! Ergonomic typed core over the generated, recursively validated wire codecs.
use crate::{AcpError, Client, v1, v2};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;
pub type Metadata = BTreeMap<String, Value>;
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
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_capabilities: Option<v1::ClientCapabilities>,
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
#[derive(Debug, Clone, PartialEq, Serialize)]
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
impl Client {
    pub async fn initialize_typed(&self, request: InitializeRequest) -> Result<Value, AcpError> {
        self.initialize(serde_json::to_value(request).expect("typed ACP initialize JSON"))
            .await
    }
    pub async fn create_session_typed(
        &self,
        request: NewSessionRequest,
    ) -> Result<Value, AcpError> {
        self.call(
            crate::AgentMethod::CreateSession,
            serde_json::to_value(request).expect("typed ACP session JSON"),
        )
        .await
    }
    pub async fn prompt_typed(&self, request: PromptRequest) -> Result<PromptResponse, AcpError> {
        let value = self
            .prompt(serde_json::to_value(request).expect("typed ACP prompt JSON"))
            .await?;
        serde_json::from_value(value)
            .map_err(|e| AcpError::Transport(format!("invalid normalized prompt result: {e}")))
    }
}
