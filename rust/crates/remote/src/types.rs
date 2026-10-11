//! Wire types of the T3 Connect relay API and the environment's `/api/connect/*` routes
//! (`packages/contracts/src/relay.ts`, `environmentHttp.ts`). Field names and ordering match the
//! originals; strings the original decodes as `TrimmedNonEmptyString` use that type here.
//! TODO(consolidation): move into `t3-contracts` when the server is wired.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use t3_contracts::{EnvironmentId, ExecutionEnvironmentDescriptor, TrimmedNonEmptyString};

/// A `TrimmedNonEmptyString`: trimmed like JavaScript, `None` when nothing is left.
pub fn trimmed(value: &str) -> Option<String> {
    let value = t3_contracts::trim_wire_string(value);
    (!value.is_empty()).then(|| value.to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "manual")]
    Manual,
    #[serde(rename = "cloudflare_tunnel")]
    CloudflareTunnel,
    #[serde(rename = "t3_relay")]
    T3Relay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedEndpoint {
    pub http_base_url: TrimmedNonEmptyString,
    pub ws_base_url: TrimmedNonEmptyString,
    pub provider_kind: ProviderKind,
}

/// The loopback origin a managed tunnel forwards to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedEndpointOrigin {
    pub local_http_host: TrimmedNonEmptyString,
    #[serde(deserialize_with = "port")]
    pub local_http_port: u16,
}

fn port<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    let value = u32::deserialize(d)?;
    u16::try_from(value)
        .ok()
        .filter(|port| *port >= 1)
        .ok_or_else(|| serde::de::Error::custom("expected a port from 1 to 65535"))
}

impl ManagedEndpointOrigin {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            local_http_host: TrimmedNonEmptyString::new(host).expect("host"),
            local_http_port: port,
        }
    }
}

/// What the connector needs to run. Stored as JSON in `cloud-endpoint-runtime-config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub provider_kind: ProviderKind,
    pub connector_token: TrimmedNonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_id: Option<TrimmedNonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_name: Option<TrimmedNonEmptyString>,
}

impl RuntimeConfig {
    pub fn cloudflare(token: &str, tunnel_id: Option<&str>) -> Self {
        Self {
            provider_kind: ProviderKind::CloudflareTunnel,
            connector_token: TrimmedNonEmptyString::new(token).expect("token"),
            tunnel_id: tunnel_id.map(|id| TrimmedNonEmptyString::new(id).expect("id")),
            tunnel_name: None,
        }
    }

    /// The original compares provider, token, tunnel id and name (`managedEndpointRuntimeConfigsMatch`).
    pub fn matches(&self, other: &Self) -> bool {
        self == other
    }
}

/// `cloud-endpoint-confirmed-origin`: the config the relay last confirmed, with the origin it serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmedOrigin {
    pub config: RuntimeConfig,
    pub origin: ManagedEndpointOrigin,
}

pub fn decode_runtime_config(text: &str) -> Option<RuntimeConfig> {
    serde_json::from_str(text).ok()
}

pub fn decode_confirmed_origin(text: &str) -> Option<ConfirmedOrigin> {
    serde_json::from_str(text).ok()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkProofRequest {
    pub challenge: String,
    pub relay_issuer: String,
    pub endpoint: ManagedEndpoint,
    pub origin: ManagedEndpointOrigin,
}

/// Body of `POST /api/connect/relay-config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentConfigRequest {
    pub relay_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_issuer: Option<String>,
    pub cloud_user_id: String,
    pub environment_credential: String,
    pub cloud_mint_public_key: String,
    pub endpoint_runtime: Option<RuntimeConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkChallengeRequest {
    pub notifications_enabled: bool,
    pub live_activities_enabled: bool,
    pub managed_tunnels_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkChallengeResponse {
    pub challenge: TrimmedNonEmptyString,
    pub expires_at: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<TrimmedNonEmptyString>,
    pub proof: TrimmedNonEmptyString,
    pub notifications_enabled: bool,
    pub live_activities_enabled: bool,
    pub managed_tunnels_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkResponse {
    pub ok: bool,
    pub cloud_user_id: TrimmedNonEmptyString,
    pub environment_id: EnvironmentId,
    pub endpoint: ManagedEndpoint,
    pub endpoint_runtime: Option<RuntimeConfig>,
    pub relay_issuer: TrimmedNonEmptyString,
    pub environment_credential: TrimmedNonEmptyString,
    pub cloud_mint_public_key: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryRegistrationRequest {
    pub cloud_user_id: TrimmedNonEmptyString,
    pub tunnel_id: TrimmedNonEmptyString,
    pub origin: ManagedEndpointOrigin,
    pub proof: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationStatus {
    Ready,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRegistrationResponse {
    pub status: RegistrationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryRequest {
    pub cloud_user_id: TrimmedNonEmptyString,
    pub origin: ManagedEndpointOrigin,
    pub proof: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryResponse {
    pub endpoint: ManagedEndpoint,
    pub endpoint_runtime: RuntimeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkResponse {
    pub ok: bool,
}

/// Body of `POST /v1/environments/:id/link-preferences` and its answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkPreferencesRequest {
    pub hold_webhooks_while_offline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeHeldHooksResponse {
    /// True when the relay was holding requests and has started delivering them.
    pub pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthRequest {
    pub proof: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub environment_id: EnvironmentId,
    pub status: HealthStatus,
    pub descriptor: ExecutionEnvironmentDescriptor,
    pub checked_at: TrimmedNonEmptyString,
    pub proof: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthStatus {
    #[serde(rename = "online")]
    Online,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintRequest {
    pub proof: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MintResponse {
    pub credential: TrimmedNonEmptyString,
    pub expires_at: TrimmedNonEmptyString,
    pub proof: TrimmedNonEmptyString,
}

/// `EnvironmentCloudLinkStateResult`: `GET /api/connect/link-state` and the answer of preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkState {
    pub linked: bool,
    pub cloud_user_id: Option<String>,
    pub relay_url: Option<String>,
    pub relay_issuer: Option<String>,
    /// False for a publish-only link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_tunnel_active: Option<bool>,
    pub publish_agent_activity: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_webhooks_while_offline: Option<bool>,
}

/// `EnvironmentCloudPreferencesRequest`: omit `holdWebhooksWhileOffline` to leave it unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreferencesRequest {
    pub publish_agent_activity: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_webhooks_while_offline: Option<bool>,
}

/// `EnvironmentCloudRelayConfigResult`: `endpointRuntimeStatus` is the runtime's status as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayConfigResult {
    pub ok: bool,
    pub endpoint_runtime_status: Value,
}

/// The agent-activity state a thread publishes (`RelayAgentActivityState`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentActivityState {
    pub environment_id: EnvironmentId,
    pub thread_id: t3_contracts::ThreadId,
    pub project_title: TrimmedNonEmptyString,
    pub thread_title: TrimmedNonEmptyString,
    pub phase: AgentAwarenessPhase,
    pub headline: TrimmedNonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<TrimmedNonEmptyString>,
    pub model_title: TrimmedNonEmptyString,
    pub updated_at: TrimmedNonEmptyString,
    pub deep_link: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAwarenessPhase {
    Starting,
    Running,
    WaitingForApproval,
    WaitingForInput,
    Completed,
    Failed,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentActivityPublishRequest {
    pub state: Option<AgentActivityState>,
    pub proof: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryResult {
    pub device_id: TrimmedNonEmptyString,
    pub kind: String,
    pub ok: bool,
    #[serde(default)]
    pub queued: Option<bool>,
    pub apns_status: Option<f64>,
    pub apns_reason: Option<String>,
    pub apns_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishResponse {
    pub ok: bool,
    pub deliveries: Vec<DeliveryResult>,
}
