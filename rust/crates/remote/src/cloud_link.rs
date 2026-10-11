//! The T3 Connect link of this environment (`cloud/CloudLink.ts`): linking it to the relay,
//! installing and reading the link, unlinking, answering the relay's signed health and mint
//! requests, and keeping the managed tunnel registered, recovered and released. The HTTP routes,
//! server startup and shutdown all go through [`CloudLink`].

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use t3_contracts::{AuthEnvironmentScope, ExecutionEnvironmentDescriptor};
use tokio::sync::Mutex;

use crate::backoff::{Random, capped_exponential_delay, jittered};
use crate::cli_state::*;
use crate::cli_token::{CliTokenError, CliTokenManager, PersistedToken};
use crate::clock::{Clock, format_iso};
use crate::control_plane::ControlPlane;
use crate::endpoint_runtime::{EndpointStatus, ManagedEndpointRuntime, is_retryable_status_json};
use crate::env_keys::{KeyPairError, get_or_create_environment_key_pair};
use crate::jwt::*;
use crate::link_checks::*;
use crate::relay_error::RelayRequestError;
use crate::relay_url::{is_secure_relay_url, normalize_relay_issuer};
use crate::secrets::*;
use crate::types::*;

const PROOF_TTL_SECONDS: i64 = 5 * 60;
/// How long a pairing credential minted for a relay-vouched client lives.
pub const CLOUD_CONNECT_CREDENTIAL_TTL: Duration = Duration::from_secs(2 * 60);

/// Who this environment is. Implemented by the server's environment service.
pub trait EnvironmentIdentity: Send + Sync {
    fn environment_id(&self) -> String;
    fn descriptor(&self) -> ExecutionEnvironmentDescriptor;
}

/// What the server needs to issue for a client the relay vouched for
/// (`EnvironmentAuth.createPairingLink`).
#[derive(Debug, Clone)]
pub struct PairingRequest {
    pub scopes: &'static [AuthEnvironmentScope],
    /// Always `cloud-connect`.
    pub subject: &'static str,
    pub ttl: Duration,
    /// Always `T3 Connect connect`.
    pub label: &'static str,
    /// The DPoP key thumbprint the credential, and the session it becomes, is bound to.
    pub proof_key_thumbprint: String,
}

pub struct IssuedPairing {
    pub credential: String,
    pub expires_at_ms: i64,
}

#[async_trait]
pub trait PairingIssuer: Send + Sync {
    async fn create_pairing_link(&self, request: PairingRequest) -> Result<IssuedPairing, String>;
}

/// Wakes the agent-activity publisher after a link or preference change
/// (`AgentAwarenessRelay.requestCatchUp`).
pub trait AwarenessCatchUp: Send + Sync {
    fn request_catch_up(&self);
}

/// Whether this server is going down only to be replaced by an update (`cloud/updateHandoff.ts`).
/// Shutdown keeps the managed tunnel across those restarts instead of releasing it.
#[async_trait]
pub trait UpdateHandoff: Send + Sync {
    /// Reads and consumes the desktop app's restart marker; true only for a fresh one.
    async fn desktop_update_restart_pending(&self) -> bool;
    /// A pending service update with no explicit-stop marker: a replacement server is coming.
    async fn pending_update_handoff_exists(&self) -> bool;
}

/// The CLI's OAuth credential.
#[async_trait]
pub trait CliCredentials: Send + Sync {
    async fn get_existing(&self) -> Result<Option<PersistedToken>, CliTokenError>;
}

#[async_trait]
impl CliCredentials for CliTokenManager {
    async fn get_existing(&self) -> Result<Option<PersistedToken>, CliTokenError> {
        CliTokenManager::get_existing(self).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignedRequest {
    Health,
    Mint,
}

impl SignedRequest {
    fn name(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::Mint => "mint",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayConfigField {
    RelayUrl,
    RelayIssuer,
    EnvironmentCredential,
    CloudUserId,
    CloudMintPublicKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalOperation {
    RelayUrlUnconfigured,
    GenerateLinkProof,
    PersistRelayConfig,
    RegisterEndpointOrigin,
    ResolveServerOrigin,
    PersistDesiredLink,
    SignRecoveryProof,
    UnsupportedRecoveredTunnel,
    PersistRecoveredTunnel,
    ReadRelayConfig,
    RemoveRelayConfig,
    UpdateWebhookSettings,
    ReadPreferences,
    PersistPreferences,
    AnswerHealth,
    IssueCredential,
}

impl InternalOperation {
    fn message(self) -> &'static str {
        match self {
            Self::RelayUrlUnconfigured => {
                "T3CODE_RELAY_URL must be configured as a secure absolute HTTPS origin."
            }
            Self::GenerateLinkProof => "Could not generate environment link proof.",
            Self::PersistRelayConfig => "Could not persist environment relay configuration.",
            Self::RegisterEndpointOrigin => "Could not register the managed endpoint origin.",
            Self::ResolveServerOrigin => "Could not resolve the local server origin.",
            Self::PersistDesiredLink => "Could not persist desired T3 Connect link state.",
            Self::SignRecoveryProof => "Could not sign the managed tunnel recovery request.",
            Self::UnsupportedRecoveredTunnel => {
                "T3 Connect returned an unsupported managed tunnel configuration."
            }
            Self::PersistRecoveredTunnel => {
                "Could not persist the recovered managed tunnel configuration."
            }
            Self::ReadRelayConfig => "Could not read environment relay configuration.",
            Self::RemoveRelayConfig => "Could not remove environment relay configuration.",
            Self::UpdateWebhookSettings => "Could not update T3 Connect webhook settings.",
            Self::ReadPreferences => "Could not read environment cloud preferences.",
            Self::PersistPreferences => "Could not persist environment cloud preferences.",
            Self::AnswerHealth => "Could not answer cloud health request.",
            Self::IssueCredential => "Could not issue cloud connection credential.",
        }
    }
}

/// `ServerAuth*Error`: failures of the server's own auth service as the cloud routes see them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthFailure {
    LinkedCloudAccountVerification,
    LinkedCloudAccountRead,
    LinkedCloudAccountMissing,
    CloudLinkJwtSigning,
    CloudMintPublicKeyMissing,
    CloudRelayIssuerMissing,
    CloudHealthJwtSigning,
    CloudMintJwtSigning,
    /// Anything else the pairing issuer reports.
    Other(String),
}

impl AuthFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::LinkedCloudAccountVerification => "Could not verify the linked cloud account.",
            Self::LinkedCloudAccountRead => "Could not read the linked cloud account.",
            Self::LinkedCloudAccountMissing => {
                "Cloud linked user is not installed for this environment."
            }
            Self::CloudLinkJwtSigning => "Failed to sign cloud link JWT.",
            Self::CloudMintPublicKeyMissing => {
                "Cloud mint public key is not installed for this environment."
            }
            Self::CloudRelayIssuerMissing => {
                "Cloud relay issuer is not installed for this environment."
            }
            Self::CloudHealthJwtSigning => "Failed to sign cloud health JWT.",
            Self::CloudMintJwtSigning => "Failed to sign cloud mint JWT.",
            Self::Other(message) => message,
        }
    }
}

/// Every failure the link service reports (the `CloudLink*Error` classes of the original, its
/// relay failures, and the server auth failures it can surface).
#[derive(Debug, Clone, PartialEq)]
pub enum CloudLinkError {
    RelayConfigInvalid {
        field: RelayConfigField,
    },
    /// `endpoint`: a proof request that did not reach this server directly on loopback.
    /// `local`: a local origin that is not a bare http(s) origin.
    OriginInvalid {
        endpoint: bool,
    },
    NotLinked,
    AccountMismatch,
    AuthorizationMissing,
    ProofRejected(SignedRequest),
    ProofReplayed(SignedRequest),
    TunnelSuperseded,
    EndpointUnavailable {
        origin_unconfirmed: bool,
        endpoint_runtime_status: Value,
    },
    Internal {
        operation: InternalOperation,
        cause: String,
    },
    Relay(RelayRequestError),
    Auth(AuthFailure),
    CliToken(CliTokenError),
}

impl CloudLinkError {
    pub fn internal(operation: InternalOperation, cause: impl ToString) -> Self {
        Self::Internal {
            operation,
            cause: cause.to_string(),
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::RelayConfigInvalid { field } => match field {
                RelayConfigField::RelayUrl => "Relay URL must be a secure absolute HTTPS URL.",
                RelayConfigField::RelayIssuer => "Relay issuer must be a secure absolute HTTPS URL.",
                RelayConfigField::EnvironmentCredential => "Relay environment credential is required.",
                RelayConfigField::CloudUserId => "Cloud user id is required.",
                RelayConfigField::CloudMintPublicKey => {
                    "Cloud mint public key must be a valid Ed25519 public key."
                }
            }
            .to_owned(),
            Self::OriginInvalid { endpoint: true } => "Invalid managed endpoint origin.".to_owned(),
            Self::OriginInvalid { endpoint: false } => {
                "Could not resolve local environment origin.".to_owned()
            }
            Self::NotLinked => "Link this environment to T3 Connect first.".to_owned(),
            Self::AccountMismatch => "This environment is already linked to a different cloud account. Unlink it before switching accounts.".to_owned(),
            Self::AuthorizationMissing => "Run `t3 connect link` to authorize this environment.".to_owned(),
            Self::ProofRejected(request) => format!("Invalid cloud {} request.", request.name()),
            Self::ProofReplayed(request) => {
                format!("Cloud {} request was already consumed.", request.name())
            }
            Self::TunnelSuperseded => {
                "The managed tunnel configuration changed during registration.".to_owned()
            }
            Self::EndpointUnavailable { origin_unconfirmed, .. } => if *origin_unconfirmed {
                "Managed endpoint origin could not be confirmed."
            } else {
                "Managed endpoint runtime could not be started."
            }
            .to_owned(),
            Self::Internal { operation, .. } => operation.message().to_owned(),
            Self::Relay(error) => error.description.clone(),
            Self::Auth(failure) => failure.message().to_owned(),
            Self::CliToken(error) => error.to_string(),
        }
    }

    /// Whether a failed link step may succeed on retry: not when the relay or this server refused
    /// it (`shouldRetryCloudLink`).
    pub fn should_retry(&self) -> bool {
        match self {
            Self::Relay(error) => error.should_retry(),
            Self::RelayConfigInvalid { .. }
            | Self::OriginInvalid { .. }
            | Self::NotLinked
            | Self::AccountMismatch
            | Self::AuthorizationMissing
            | Self::ProofRejected(_)
            | Self::ProofReplayed(_)
            | Self::TunnelSuperseded => false,
            _ => true,
        }
    }

    pub fn is_endpoint_unavailable(&self) -> bool {
        matches!(self, Self::EndpointUnavailable { .. })
    }

    /// How the `/api/connect/*` routes answer this failure (`cloud/http.ts`).
    pub fn to_http_failure(&self) -> HttpFailure {
        let (status, tag) = match self {
            Self::RelayConfigInvalid { .. } | Self::OriginInvalid { .. } | Self::NotLinked => {
                (400, "EnvironmentHttpBadRequestError")
            }
            Self::AccountMismatch | Self::ProofReplayed(_) | Self::TunnelSuperseded => {
                (409, "EnvironmentHttpConflictError")
            }
            Self::AuthorizationMissing | Self::ProofRejected(_) => {
                (401, "EnvironmentHttpUnauthorizedError")
            }
            Self::Relay(error) => match error.rejection {
                crate::relay_error::Rejection::Unauthorized => {
                    (401, "EnvironmentHttpUnauthorizedError")
                }
                crate::relay_error::Rejection::Forbidden => (403, "EnvironmentHttpForbiddenError"),
                crate::relay_error::Rejection::Rejected => (400, "EnvironmentHttpBadRequestError"),
                crate::relay_error::Rejection::Unavailable => {
                    (500, "EnvironmentHttpInternalServerError")
                }
            },
            Self::EndpointUnavailable { .. } => (503, "EnvironmentCloudEndpointUnavailableError"),
            Self::Internal { .. } | Self::Auth(_) | Self::CliToken(_) => {
                (500, "EnvironmentHttpInternalServerError")
            }
        };
        HttpFailure {
            status,
            tag,
            message: self.message(),
            endpoint_runtime_status: match self {
                Self::EndpointUnavailable {
                    endpoint_runtime_status,
                    ..
                } => Some(endpoint_runtime_status.clone()),
                _ => None,
            },
        }
    }
}

impl std::fmt::Display for CloudLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for CloudLinkError {}

/// An HTTP error body: `{ "_tag": tag, "message": message }`, plus `endpointRuntimeStatus` for 503.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpFailure {
    pub status: u16,
    pub tag: &'static str,
    pub message: String,
    pub endpoint_runtime_status: Option<Value>,
}

impl HttpFailure {
    pub fn body(&self) -> Value {
        let mut body = json!({ "_tag": self.tag, "message": self.message });
        if let Some(status) = &self.endpoint_runtime_status {
            body["endpointRuntimeStatus"] = status.clone();
        }
        body
    }
}

impl From<RelayRequestError> for CloudLinkError {
    fn from(error: RelayRequestError) -> Self {
        Self::Relay(error)
    }
}

impl From<CliTokenError> for CloudLinkError {
    fn from(error: CliTokenError) -> Self {
        Self::CliToken(error)
    }
}

/// The outcome of telling the relay which origin the stored tunnel serves.
#[derive(Debug, Clone, PartialEq)]
pub enum ManagedTunnelRegistration {
    NotLinked,
    Superseded,
    RecoveryRequired {
        config: RuntimeConfig,
    },
    Ready {
        endpoint_runtime_status: EndpointStatus,
    },
}

/// The pieces a [`CloudLink`] works with.
pub struct CloudLinkDeps {
    pub secrets: Arc<dyn SecretStore>,
    pub environment: Arc<dyn EnvironmentIdentity>,
    pub runtime: ManagedEndpointRuntime,
    pub pairing: Arc<dyn PairingIssuer>,
    pub credentials: Arc<dyn CliCredentials>,
    pub control_plane: Arc<dyn ControlPlane>,
    pub awareness: Arc<dyn AwarenessCatchUp>,
    pub updates: Arc<dyn UpdateHandoff>,
    pub clock: Arc<dyn Clock>,
    pub random: Arc<dyn Random>,
    /// The configured relay URL (`T3CODE_RELAY_URL` or the embedded one), already normalized.
    /// Only the CLI link flow reads it; every other call targets the URL the link was made against.
    pub relay_url: Option<String>,
    /// The port this server listens on, for registering the managed tunnel's origin.
    pub local_port: Arc<dyn Fn() -> Option<u16> + Send + Sync>,
    /// Lets a relay URL be plain `http://` on a loopback address, so tests can use a relay on
    /// 127.0.0.1. The server leaves this false: real relay URLs must be secure origins.
    pub allow_loopback_http_relay: bool,
}

struct Inner {
    deps: CloudLinkDeps,
    preferences_lock: Mutex<()>,
}

#[derive(Clone)]
pub struct CloudLink {
    inner: Arc<Inner>,
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Records each one-shot marker; false when any already existed, so the request is a replay
/// (`consumeCloudReplayGuards`). Every marker is attempted even after one already exists.
pub fn consume_cloud_replay_guards(
    secrets: &dyn SecretStore,
    names: &[String],
    value: &[u8],
) -> Result<bool, SecretError> {
    let mut all_created = true;
    for name in names {
        match secrets.create(name, value) {
            Ok(()) => {}
            Err(error) if error.already_exists => all_created = false,
            Err(error) => return Err(error),
        }
    }
    Ok(all_created)
}

fn secret_to_internal(operation: InternalOperation) -> impl Fn(SecretError) -> CloudLinkError {
    move |error| CloudLinkError::internal(operation, error.message)
}

fn key_pair_to_internal(operation: InternalOperation) -> impl Fn(KeyPairError) -> CloudLinkError {
    move |error| CloudLinkError::internal(operation, error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProofClaims {
    jti: String,
    sub: String,
    iat: i64,
    exp: i64,
    environment_id: String,
    nonce: String,
    scope: Vec<String>,
    #[serde(default)]
    client_proof_key_thumbprint: Option<String>,
    #[serde(default)]
    cnf: Option<Cnf>,
}

#[derive(Deserialize)]
struct Cnf {
    jkt: String,
}

impl CloudLink {
    pub fn new(deps: CloudLinkDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                deps,
                preferences_lock: Mutex::new(()),
            }),
        }
    }

    fn deps(&self) -> &CloudLinkDeps {
        &self.inner.deps
    }

    fn secrets(&self) -> &dyn SecretStore {
        self.inner.deps.secrets.as_ref()
    }

    fn get_text(
        &self,
        name: &str,
        operation: InternalOperation,
    ) -> Result<Option<String>, CloudLinkError> {
        self.secrets()
            .get(name)
            .map(|v| v.map(|bytes| text(&bytes)))
            .map_err(secret_to_internal(operation))
    }

    fn validate_linked_cloud_user(&self, cloud_user_id: &str) -> Result<(), CloudLinkError> {
        let existing = self
            .secrets()
            .get(CLOUD_LINKED_USER_ID)
            .map_err(|_| CloudLinkError::Auth(AuthFailure::LinkedCloudAccountVerification))?;
        match existing {
            Some(existing) if text(&existing) != cloud_user_id => {
                Err(CloudLinkError::AccountMismatch)
            }
            _ => Ok(()),
        }
    }

    fn read_installed_cloud_user_id(&self) -> Result<String, CloudLinkError> {
        self.secrets()
            .get(CLOUD_LINKED_USER_ID)
            .map_err(|_| CloudLinkError::Auth(AuthFailure::LinkedCloudAccountRead))?
            .map(|bytes| text(&bytes))
            .ok_or(CloudLinkError::Auth(AuthFailure::LinkedCloudAccountMissing))
    }

    fn sign(
        &self,
        key_pair: &KeyPair,
        typ: &str,
        payload: &Value,
        failure: AuthFailure,
    ) -> Result<String, CloudLinkError> {
        sign_relay_jwt(&key_pair.private_key, typ, payload)
            .map_err(|_| CloudLinkError::Auth(failure))
    }

    // ---- link proof ------------------------------------------------------------------------

    fn make_cloud_link_proof(
        &self,
        request: &LinkProofRequest,
        request_url: &str,
    ) -> Result<String, CloudLinkError> {
        let deps = self.deps();
        let key_pair = get_or_create_environment_key_pair(self.secrets())
            .map_err(key_pair_to_internal(InternalOperation::GenerateLinkProof))?;
        if !is_supported_link_provider_kind(request)
            || !is_allowed_endpoint_origin(&request.origin, request_url)
        {
            return Err(CloudLinkError::OriginInvalid { endpoint: true });
        }
        let now = deps.clock.now_seconds();
        let descriptor = deps.environment.descriptor();
        let environment_id = descriptor.environment_id.as_str().to_owned();
        let payload = json!({
            "iss": format!("t3-env:{environment_id}"),
            "aud": normalize_relay_issuer(&request.relay_issuer),
            "sub": environment_id,
            "jti": uuid::Uuid::new_v4().to_string(),
            "iat": now,
            "exp": now + PROOF_TTL_SECONDS,
            "challenge": request.challenge,
            "descriptor": descriptor,
            "environmentId": environment_id,
            "environmentPublicKey": key_pair.public_key.trim(),
            "endpoint": request.endpoint,
            "origin": request.origin,
            "scopes": link_proof_scopes(request),
        });
        self.sign(
            &key_pair,
            RELAY_LINK_PROOF_TYP,
            &payload,
            AuthFailure::CloudLinkJwtSigning,
        )
    }

    /// Signs the proof the relay asks for to link this environment. The request must reach this
    /// server directly on the loopback origin it names (`POST /api/connect/link-proof`).
    pub fn link_proof(
        &self,
        request: &LinkProofRequest,
        http_request: &InboundRequest,
    ) -> Result<String, CloudLinkError> {
        let url = request_absolute_url(http_request);
        let Some(url) = url.filter(|_| !has_forwarded_authority_headers(http_request)) else {
            return Err(CloudLinkError::OriginInvalid { endpoint: true });
        };
        self.make_cloud_link_proof(request, &url)
    }

    // ---- relay config ----------------------------------------------------------------------

    fn acceptable_relay_url(&self, value: &str) -> bool {
        is_secure_relay_url(value)
            || (self.deps().allow_loopback_http_relay
                && url::Url::parse(value.trim()).is_ok_and(|url| {
                    url.scheme() == "http"
                        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
                }))
    }

    fn invalid_relay_config_field(
        &self,
        payload: &EnvironmentConfigRequest,
    ) -> Option<RelayConfigField> {
        if !self.acceptable_relay_url(&payload.relay_url) {
            return Some(RelayConfigField::RelayUrl);
        }
        if payload
            .relay_issuer
            .as_deref()
            .is_some_and(|issuer| !self.acceptable_relay_url(issuer))
        {
            return Some(RelayConfigField::RelayIssuer);
        }
        if payload.environment_credential.trim().is_empty() {
            return Some(RelayConfigField::EnvironmentCredential);
        }
        if payload.cloud_user_id.trim().is_empty() {
            return Some(RelayConfigField::CloudUserId);
        }
        None
    }

    /// Installs the link. With `confirmed_origin`, a managed tunnel is started and the origin it
    /// serves recorded. The caller holds the link-state lock unless `lock_held` is false.
    async fn apply_cloud_relay_config(
        &self,
        payload: &EnvironmentConfigRequest,
        lock_held: bool,
        confirmed_origin: Option<&ManagedEndpointOrigin>,
    ) -> Result<RelayConfigResult, CloudLinkError> {
        let runtime = self.deps().runtime.clone();
        if lock_held {
            self.apply_cloud_relay_config_locked(payload, confirmed_origin)
                .await
        } else {
            runtime
                .with_link_state_lock(
                    self.apply_cloud_relay_config_locked(payload, confirmed_origin),
                )
                .await
        }
    }

    async fn apply_cloud_relay_config_locked(
        &self,
        payload: &EnvironmentConfigRequest,
        confirmed_origin: Option<&ManagedEndpointOrigin>,
    ) -> Result<RelayConfigResult, CloudLinkError> {
        let persist = secret_to_internal(InternalOperation::PersistRelayConfig);
        if let Some(field) = self.invalid_relay_config_field(payload) {
            return Err(CloudLinkError::RelayConfigInvalid { field });
        }
        self.validate_linked_cloud_user(&payload.cloud_user_id)?;
        if !is_ed25519_public_key(&payload.cloud_mint_public_key) {
            return Err(CloudLinkError::RelayConfigInvalid {
                field: RelayConfigField::CloudMintPublicKey,
            });
        }
        // Reject unsupported runtimes before touching the connector so a bad payload cannot stop
        // a healthy tunnel on its way to a 503.
        if let Some(config) = payload
            .endpoint_runtime
            .as_ref()
            .filter(|c| c.provider_kind != ProviderKind::CloudflareTunnel)
        {
            return Err(CloudLinkError::EndpointUnavailable {
                origin_unconfirmed: false,
                endpoint_runtime_status: EndpointStatus::Unsupported {
                    provider_kind: config.provider_kind,
                }
                .to_json(),
            });
        }
        let runtime = &self.deps().runtime;
        runtime.apply_config(None).await;
        self.secrets()
            .remove(CLOUD_ENDPOINT_CONFIRMED_ORIGIN)
            .map_err(&persist)?;
        let secrets = self.secrets();
        secrets
            .set(RELAY_URL_SECRET, payload.relay_url.as_bytes())
            .map_err(&persist)?;
        secrets
            .set(
                RELAY_ISSUER_SECRET,
                payload
                    .relay_issuer
                    .as_deref()
                    .unwrap_or(&payload.relay_url)
                    .as_bytes(),
            )
            .map_err(&persist)?;
        secrets
            .set(CLOUD_LINKED_USER_ID, payload.cloud_user_id.as_bytes())
            .map_err(&persist)?;
        secrets
            .set(
                RELAY_ENVIRONMENT_CREDENTIAL_SECRET,
                payload.environment_credential.as_bytes(),
            )
            .map_err(&persist)?;
        secrets
            .set(
                CLOUD_MINT_PUBLIC_KEY,
                payload.cloud_mint_public_key.as_bytes(),
            )
            .map_err(&persist)?;
        self.deps().awareness.request_catch_up();
        match &payload.endpoint_runtime {
            Some(config) => {
                let json = serde_json::to_vec(config).expect("config json");
                secrets
                    .set(CLOUD_ENDPOINT_RUNTIME_CONFIG, &json)
                    .map_err(&persist)?;
            }
            None => secrets
                .remove(CLOUD_ENDPOINT_RUNTIME_CONFIG)
                .map_err(&persist)?,
        }
        let (Some(config), Some(origin)) = (&payload.endpoint_runtime, confirmed_origin) else {
            return Ok(RelayConfigResult {
                ok: true,
                endpoint_runtime_status: EndpointStatus::Disabled.to_json(),
            });
        };
        let status = runtime.apply_config(Some(config)).await;
        if !status.is_running() {
            return Err(CloudLinkError::EndpointUnavailable {
                origin_unconfirmed: false,
                endpoint_runtime_status: status.to_json(),
            });
        }
        let marker = serde_json::to_vec(&ConfirmedOrigin {
            config: config.clone(),
            origin: origin.clone(),
        })
        .expect("marker json");
        secrets
            .set(CLOUD_ENDPOINT_CONFIRMED_ORIGIN, &marker)
            .map_err(&persist)?;
        Ok(RelayConfigResult {
            ok: true,
            endpoint_runtime_status: status.to_json(),
        })
    }

    /// Installs the link the relay returned and, for a managed tunnel, confirms its origin
    /// (`POST /api/connect/relay-config`).
    pub async fn apply_relay_config(
        &self,
        payload: &EnvironmentConfigRequest,
    ) -> Result<RelayConfigResult, CloudLinkError> {
        let result = self.apply_cloud_relay_config(payload, false, None).await?;
        let is_tunnel = payload
            .endpoint_runtime
            .as_ref()
            .is_some_and(|c| c.provider_kind == ProviderKind::CloudflareTunnel);
        if !is_tunnel {
            return Ok(result);
        }
        let port = (self.deps().local_port)().ok_or_else(|| {
            CloudLinkError::internal(InternalOperation::ResolveServerOrigin, "no address")
        })?;
        let local_origin = format!("http://127.0.0.1:{port}");
        // Two immediate retries of failures that may clear; no delay between them.
        let mut attempts_left = 2;
        let registration = loop {
            match self
                .register_managed_tunnel_recovery(&local_origin, false)
                .await
            {
                Err(error)
                    if attempts_left > 0
                        && error.should_retry()
                        && !error.is_endpoint_unavailable() =>
                {
                    attempts_left -= 1;
                }
                other => break other?,
            }
        };
        match registration {
            ManagedTunnelRegistration::Superseded => return Err(CloudLinkError::TunnelSuperseded),
            ManagedTunnelRegistration::RecoveryRequired { ref config } => {
                self.deps().runtime.request_recovery(config.clone());
            }
            _ => {}
        }
        match registration {
            ManagedTunnelRegistration::Ready {
                endpoint_runtime_status,
            } => Ok(RelayConfigResult {
                ok: true,
                endpoint_runtime_status: endpoint_runtime_status.to_json(),
            }),
            _ => Err(CloudLinkError::EndpointUnavailable {
                origin_unconfirmed: true,
                endpoint_runtime_status: EndpointStatus::Disabled.to_json(),
            }),
        }
    }

    // ---- managed tunnel startup / recovery -------------------------------------------------

    fn parse_local_origin(local_origin: &str) -> Result<ParsedLocalOrigin, CloudLinkError> {
        parse_managed_endpoint_local_origin(local_origin)
            .map_err(|_| CloudLinkError::OriginInvalid { endpoint: false })
    }

    /// Applies the stored config to the runtime and records that the relay confirmed `origin`.
    /// `Ok(None)`: the stored config changed meanwhile, so this activation is stale.
    async fn activate_managed_tunnel(
        &self,
        config: &RuntimeConfig,
        config_json: &str,
        origin: &ManagedEndpointOrigin,
    ) -> Result<Option<EndpointStatus>, CloudLinkError> {
        let runtime = self.deps().runtime.clone();
        runtime
            .with_link_state_lock(async {
                let current = self
                    .secrets()
                    .get(CLOUD_ENDPOINT_RUNTIME_CONFIG)
                    .map_err(secret_to_internal(InternalOperation::PersistRelayConfig))?;
                if current.map(|bytes| text(&bytes)).as_deref() != Some(config_json) {
                    return Ok(None);
                }
                let status = self.deps().runtime.apply_config(Some(config)).await;
                if !status.is_running() {
                    return Err(CloudLinkError::EndpointUnavailable {
                        origin_unconfirmed: false,
                        endpoint_runtime_status: status.to_json(),
                    });
                }
                let marker = serde_json::to_vec(&ConfirmedOrigin {
                    config: config.clone(),
                    origin: origin.clone(),
                })
                .expect("marker json");
                self.secrets()
                    .set(CLOUD_ENDPOINT_CONFIRMED_ORIGIN, &marker)
                    .map_err(secret_to_internal(InternalOperation::PersistRelayConfig))?;
                Ok(Some(status))
            })
            .await
    }

    async fn activate_managed_tunnel_with_retry(
        &self,
        config: &RuntimeConfig,
        config_json: &str,
        origin: &ManagedEndpointOrigin,
        retry_runtime_failures: bool,
    ) -> Result<Option<EndpointStatus>, CloudLinkError> {
        let mut attempt = 0;
        loop {
            match self
                .activate_managed_tunnel(config, config_json, origin)
                .await
            {
                Err(CloudLinkError::EndpointUnavailable {
                    endpoint_runtime_status,
                    ..
                }) if retry_runtime_failures
                    && is_retryable_status_json(&endpoint_runtime_status) =>
                {
                    let delay = jittered(
                        capped_exponential_delay(attempt),
                        self.deps().random.as_ref(),
                    );
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                }
                other => return other,
            }
        }
    }

    /// Starts the stored tunnel. By default only one the relay already confirmed on this origin;
    /// startup falls back to any stored tunnel when the relay stays unreachable.
    pub async fn start_managed_tunnel_if_origin_confirmed(
        &self,
        local_origin: &str,
        require_confirmed_origin: bool,
    ) -> Result<bool, CloudLinkError> {
        let parsed = Self::parse_local_origin(local_origin)?;
        let runtime = self.deps().runtime.clone();
        runtime
            .with_link_state_lock(async {
                let read = secret_to_internal(InternalOperation::ReadRelayConfig);
                let runtime_bytes = self
                    .secrets()
                    .get(CLOUD_ENDPOINT_RUNTIME_CONFIG)
                    .map_err(&read)?;
                let marker_bytes = self
                    .secrets()
                    .get(CLOUD_ENDPOINT_CONFIRMED_ORIGIN)
                    .map_err(&read)?;
                let Some(runtime_bytes) = runtime_bytes else {
                    return Ok(false);
                };
                let Some(config) = decode_runtime_config(&text(&runtime_bytes))
                    .filter(|c| c.provider_kind == ProviderKind::CloudflareTunnel)
                else {
                    return Ok(false);
                };
                // With the marker required, only a config the relay already confirmed on this port
                // may start. Without it, startup is falling back after the relay stayed
                // unreachable: an unconfirmed origin may send traffic to a stale port, but that
                // beats no remote access at all.
                if require_confirmed_origin {
                    let marker = marker_bytes.and_then(|b| decode_confirmed_origin(&text(&b)));
                    let Some(marker) = marker else {
                        return Ok(false);
                    };
                    if !marker.config.matches(&config)
                        || marker.origin.local_http_host != parsed.origin.local_http_host
                        || marker.origin.local_http_port != parsed.origin.local_http_port
                    {
                        return Ok(false);
                    }
                }
                let status = self.deps().runtime.apply_config(Some(&config)).await;
                if !status.is_running() {
                    return Err(CloudLinkError::EndpointUnavailable {
                        origin_unconfirmed: false,
                        endpoint_runtime_status: status.to_json(),
                    });
                }
                Ok(true)
            })
            .await
    }

    /// Tells the relay which local origin the stored tunnel serves, then starts it
    /// (`registerManagedTunnelRecovery`).
    pub async fn register_managed_tunnel_recovery(
        &self,
        local_origin: &str,
        retry_runtime_failures: bool,
    ) -> Result<ManagedTunnelRegistration, CloudLinkError> {
        let read = secret_to_internal(InternalOperation::ReadRelayConfig);
        let secrets = self.secrets();
        let runtime_config = secrets.get(CLOUD_ENDPOINT_RUNTIME_CONFIG).map_err(&read)?;
        let relay_url = secrets.get(RELAY_URL_SECRET).map_err(&read)?;
        let cloud_user_id = secrets.get(CLOUD_LINKED_USER_ID).map_err(&read)?;
        let credential = secrets
            .get(RELAY_ENVIRONMENT_CREDENTIAL_SECRET)
            .map_err(&read)?;
        let (Some(runtime_config), Some(relay_url), Some(cloud_user_id), Some(credential)) =
            (runtime_config, relay_url, cloud_user_id, credential)
        else {
            return Ok(ManagedTunnelRegistration::NotLinked);
        };
        let config_json = text(&runtime_config);
        let Some(config) = decode_runtime_config(&config_json)
            .filter(|c| c.provider_kind == ProviderKind::CloudflareTunnel)
        else {
            return Ok(ManagedTunnelRegistration::NotLinked);
        };
        let parsed = Self::parse_local_origin(local_origin)?;
        let Some(tunnel_id) = config.tunnel_id.clone() else {
            return Ok(ManagedTunnelRegistration::RecoveryRequired { config });
        };
        let origin = parsed.origin;
        let environment_id = self.deps().environment.environment_id();
        let relay_url = text(&relay_url);
        let cloud_user_id = text(&cloud_user_id);
        let proof = self.make_managed_tunnel_recovery_proof(
            &environment_id,
            &cloud_user_id,
            &relay_url,
            RecoveryAction::Register {
                tunnel_id: tunnel_id.as_str(),
                origin: &origin,
            },
        )?;
        let registered = self
            .deps()
            .control_plane
            .register_tunnel_recovery(
                &relay_url,
                &text(&credential),
                &environment_id,
                &RecoveryRegistrationRequest {
                    cloud_user_id: t3_contracts::TrimmedNonEmptyString::new(&cloud_user_id)
                        .map_err(|_| CloudLinkError::NotLinked)?,
                    tunnel_id,
                    origin: origin.clone(),
                    proof: t3_contracts::TrimmedNonEmptyString::new(&proof).expect("proof"),
                },
            )
            .await?;
        if registered.status == RegistrationStatus::RecoveryRequired {
            return Ok(ManagedTunnelRegistration::RecoveryRequired { config });
        }
        match self
            .activate_managed_tunnel_with_retry(
                &config,
                &config_json,
                &origin,
                retry_runtime_failures,
            )
            .await?
        {
            None => Ok(ManagedTunnelRegistration::Superseded),
            Some(status) => Ok(ManagedTunnelRegistration::Ready {
                endpoint_runtime_status: status,
            }),
        }
    }

    fn make_managed_tunnel_recovery_proof(
        &self,
        environment_id: &str,
        cloud_user_id: &str,
        relay_url: &str,
        action: RecoveryAction<'_>,
    ) -> Result<String, CloudLinkError> {
        let sign_error =
            |cause: String| CloudLinkError::internal(InternalOperation::SignRecoveryProof, cause);
        let key_pair = get_or_create_environment_key_pair(self.secrets())
            .map_err(|e| sign_error(e.to_string()))?;
        let configured_issuer = self
            .secrets()
            .get(RELAY_ISSUER_SECRET)
            .map_err(|e| sign_error(e.message))?;
        let issued_at = self.deps().clock.now_seconds();
        let aud = normalize_relay_issuer(
            &configured_issuer
                .map(|b| text(&b))
                .unwrap_or_else(|| relay_url.to_owned()),
        );
        let mut payload = json!({
            "iss": format!("t3-env:{environment_id}"),
            "aud": aud,
            "sub": environment_id,
            "jti": uuid::Uuid::new_v4().to_string(),
            "iat": issued_at,
            "exp": issued_at + 60,
            "environmentId": environment_id,
            "cloudUserId": cloud_user_id,
        });
        match action {
            RecoveryAction::Register { tunnel_id, origin } => {
                payload["action"] = json!("register");
                payload["tunnelId"] = json!(tunnel_id);
                payload["origin"] = json!(origin);
            }
            RecoveryAction::Recover { origin } => {
                payload["action"] = json!("recover");
                payload["origin"] = json!(origin);
            }
        }
        sign_relay_jwt(
            &key_pair.private_key,
            RELAY_MANAGED_TUNNEL_RECOVERY_TYP,
            &payload,
        )
        .map_err(|e| sign_error(e.to_string()))
    }

    /// Asks the relay for a replacement tunnel, unless the stored one changed meanwhile.
    /// `Ok(false)`: nothing was replaced.
    pub async fn recover_managed_tunnel(
        &self,
        local_origin: &str,
        expected_config: Option<&RuntimeConfig>,
        retry_runtime_failures: bool,
    ) -> Result<bool, CloudLinkError> {
        let read = secret_to_internal(InternalOperation::ReadRelayConfig);
        let secrets = self.secrets();
        let runtime_config = secrets.get(CLOUD_ENDPOINT_RUNTIME_CONFIG).map_err(&read)?;
        let relay_url = secrets.get(RELAY_URL_SECRET).map_err(&read)?;
        let cloud_user_id = secrets.get(CLOUD_LINKED_USER_ID).map_err(&read)?;
        let credential = secrets
            .get(RELAY_ENVIRONMENT_CREDENTIAL_SECRET)
            .map_err(&read)?;
        let (Some(runtime_config), Some(relay_url), Some(cloud_user_id), Some(credential)) =
            (runtime_config, relay_url, cloud_user_id, credential)
        else {
            return Ok(false);
        };
        let stored_json = text(&runtime_config);
        if let Some(expected) = expected_config {
            let current = decode_runtime_config(&stored_json);
            if !current
                .as_ref()
                .is_some_and(|current| current.matches(expected))
            {
                return Ok(false);
            }
        }
        let parsed = Self::parse_local_origin(local_origin)?;
        let environment_id = self.deps().environment.environment_id();
        let relay_url = text(&relay_url);
        let cloud_user_id = text(&cloud_user_id);
        let origin = parsed.origin;
        let proof = self.make_managed_tunnel_recovery_proof(
            &environment_id,
            &cloud_user_id,
            &relay_url,
            RecoveryAction::Recover { origin: &origin },
        )?;
        let recovered = self
            .deps()
            .control_plane
            .recover_tunnel(
                &relay_url,
                &text(&credential),
                &environment_id,
                &RecoveryRequest {
                    cloud_user_id: t3_contracts::TrimmedNonEmptyString::new(&cloud_user_id)
                        .map_err(|_| CloudLinkError::NotLinked)?,
                    origin: origin.clone(),
                    proof: t3_contracts::TrimmedNonEmptyString::new(&proof).expect("proof"),
                },
            )
            .await?;
        if recovered.endpoint_runtime.provider_kind != ProviderKind::CloudflareTunnel {
            return Err(CloudLinkError::internal(
                InternalOperation::UnsupportedRecoveredTunnel,
                "unsupported",
            ));
        }
        let encoded = serde_json::to_string(&recovered.endpoint_runtime).expect("config json");
        let stored = self
            .deps()
            .runtime
            .clone()
            .with_link_state_lock(async {
                let persist = secret_to_internal(InternalOperation::PersistRecoveredTunnel);
                let current = self
                    .secrets()
                    .get(CLOUD_ENDPOINT_RUNTIME_CONFIG)
                    .map_err(&persist)?;
                if current.map(|b| text(&b)).as_deref() != Some(stored_json.as_str()) {
                    return Ok::<_, CloudLinkError>(false);
                }
                self.secrets()
                    .set(CLOUD_ENDPOINT_RUNTIME_CONFIG, encoded.as_bytes())
                    .map_err(&persist)?;
                self.secrets()
                    .remove(CLOUD_ENDPOINT_CONFIRMED_ORIGIN)
                    .map_err(&persist)?;
                Ok(true)
            })
            .await?;
        if !stored {
            return Ok(false);
        }
        let status = self
            .activate_managed_tunnel_with_retry(
                &recovered.endpoint_runtime,
                &encoded,
                &origin,
                retry_runtime_failures,
            )
            .await?;
        Ok(status.is_some())
    }

    // ---- CLI-desired link ------------------------------------------------------------------

    fn relay_url(&self) -> Result<String, CloudLinkError> {
        self.deps().relay_url.clone().ok_or_else(|| {
            CloudLinkError::internal(InternalOperation::RelayUrlUnconfigured, "unset")
        })
    }

    async fn reconcile_desired_cloud_link_with(
        &self,
        local_origin: &str,
    ) -> Result<DesiredLinkMode, CloudLinkError> {
        let parsed = Self::parse_local_origin(local_origin)?;
        let token = self
            .deps()
            .credentials
            .get_existing()
            .await?
            .ok_or(CloudLinkError::AuthorizationMissing)?;
        let mode = read_cli_desired_link_mode(self.secrets())
            .map_err(secret_to_internal(InternalOperation::PersistDesiredLink))?;
        let managed = mode != DesiredLinkMode::PublishOnly;
        let relay_url = self.relay_url()?;
        let control = &self.deps().control_plane;
        let challenge = control
            .create_link_challenge(
                &relay_url,
                &token.access_token,
                &LinkChallengeRequest {
                    notifications_enabled: true,
                    live_activities_enabled: true,
                    managed_tunnels_enabled: managed,
                },
            )
            .await?;
        let proof = self.make_cloud_link_proof(
            &LinkProofRequest {
                challenge: challenge.challenge.as_str().to_owned(),
                relay_issuer: relay_url.clone(),
                endpoint: ManagedEndpoint {
                    http_base_url: t3_contracts::TrimmedNonEmptyString::new(&parsed.http_base_url)
                        .expect("url"),
                    ws_base_url: t3_contracts::TrimmedNonEmptyString::new(&parsed.ws_base_url)
                        .expect("url"),
                    provider_kind: if managed {
                        ProviderKind::CloudflareTunnel
                    } else {
                        ProviderKind::Manual
                    },
                },
                origin: parsed.origin.clone(),
            },
            &parsed.http_base_url,
        )?;
        let link = control
            .link_environment(
                &relay_url,
                &token.access_token,
                &LinkRequest {
                    device_id: None,
                    proof: t3_contracts::TrimmedNonEmptyString::new(&proof).expect("proof"),
                    notifications_enabled: true,
                    live_activities_enabled: true,
                    managed_tunnels_enabled: managed,
                },
            )
            .await?;
        set_cli_desired_cloud_link(self.secrets(), true, mode)
            .map_err(secret_to_internal(InternalOperation::PersistDesiredLink))?;
        self.apply_cloud_relay_config(
            &EnvironmentConfigRequest {
                relay_url,
                relay_issuer: Some(link.relay_issuer.as_str().to_owned()),
                cloud_user_id: link.cloud_user_id.as_str().to_owned(),
                environment_credential: link.environment_credential.as_str().to_owned(),
                cloud_mint_public_key: link.cloud_mint_public_key.as_str().to_owned(),
                endpoint_runtime: link.endpoint_runtime,
            },
            true,
            Some(&parsed.origin),
        )
        .await?;
        // Callers decide on managed tunnel recovery from the mode this link actually used, not
        // from a value read before the relay round trip.
        Ok(mode)
    }

    /// Links this environment with the stored CLI authorization and records the CLI's wish.
    pub async fn reconcile_desired_link(
        &self,
        local_origin: &str,
    ) -> Result<DesiredLinkMode, CloudLinkError> {
        let runtime = self.deps().runtime.clone();
        runtime
            .with_link_state_lock(self.reconcile_desired_cloud_link_with(local_origin))
            .await
    }

    /// As [`Self::reconcile_desired_link`], but `None` when the CLI no longer wants a link.
    pub async fn reconcile_desired_link_if_still_desired(
        &self,
        local_origin: &str,
    ) -> Result<Option<DesiredLinkMode>, CloudLinkError> {
        let runtime = self.deps().runtime.clone();
        runtime
            .with_link_state_lock(async {
                let desired = read_cli_desired_cloud_link(self.secrets())
                    .map_err(secret_to_internal(InternalOperation::PersistDesiredLink))?;
                if !desired {
                    return Ok(None);
                }
                self.reconcile_desired_cloud_link_with(local_origin)
                    .await
                    .map(Some)
            })
            .await
    }

    // ---- shutdown --------------------------------------------------------------------------

    /// Deletes a CLI-managed tunnel when this server goes offline for good. The relay keeps the
    /// link and the hostname reservation; the next startup provisions a replacement tunnel under
    /// the same URL. Returns whether a tunnel was released.
    pub async fn release_managed_tunnel_on_shutdown(&self) -> Result<bool, CloudLinkError> {
        let read = secret_to_internal(InternalOperation::ReadRelayConfig);
        // Only a managed link stores a runtime config; publish-only links have no tunnel.
        let Some(runtime_config) = self
            .secrets()
            .get(CLOUD_ENDPOINT_RUNTIME_CONFIG)
            .map_err(&read)?
        else {
            return Ok(false);
        };
        // Only CLI-desired managed links release eagerly because this request uses CLI
        // authorization. Web/mobile links register startup recovery with their environment
        // credential, and the relay reaper removes them after a grace period.
        let desired = read_cli_desired_cloud_link(self.secrets()).map_err(&read)?;
        let mode = read_cli_desired_link_mode(self.secrets()).map_err(&read)?;
        if !desired || mode != DesiredLinkMode::Managed {
            return Ok(false);
        }
        // A shutdown that hands off to a pending update is not the environment going offline: a
        // server comes right back, and deleting the tunnel would cost it 1-2 minutes of routing
        // propagation on every update restart.
        let updates = &self.deps().updates;
        if updates.desktop_update_restart_pending().await
            || updates.pending_update_handoff_exists().await
        {
            tracing::info!("Keeping the managed tunnel across the update restart");
            return Ok(false);
        }
        let Some(token) = self.deps().credentials.get_existing().await? else {
            return Ok(false);
        };
        // The link belongs to the relay it was installed against.
        let Some(relay_url) =
            self.get_text(RELAY_URL_SECRET, InternalOperation::ReadRelayConfig)?
        else {
            return Ok(false);
        };
        let environment_id = self.deps().environment.environment_id();
        // Stop the local connector before the relay deletes the tunnel it serves.
        self.deps().runtime.apply_config(None).await;
        let response = self
            .deps()
            .control_plane
            .release_environment_tunnel(&relay_url, &token.access_token, &environment_id)
            .await?;
        // ok:false means the relay skipped deletion because a concurrent provision owns the
        // recorded tunnel now; leave the stored config alone.
        if !response.ok {
            return Ok(false);
        }
        // The connector token died with the tunnel. Drop the stored config so the next start waits
        // for the link reconcile instead of respawning with a dead token; but only while it is
        // still the config this shutdown released, since a fast restart may have stored a fresh one.
        let stored = self
            .secrets()
            .get(CLOUD_ENDPOINT_RUNTIME_CONFIG)
            .map_err(&read)?;
        if stored.as_deref() == Some(runtime_config.as_slice()) {
            self.secrets()
                .remove(CLOUD_ENDPOINT_RUNTIME_CONFIG)
                .map_err(&read)?;
            self.secrets()
                .remove(CLOUD_ENDPOINT_CONFIRMED_ORIGIN)
                .map_err(&read)?;
        }
        Ok(true)
    }

    // ---- state and preferences -------------------------------------------------------------

    fn read_cloud_link_state(&self) -> Result<LinkState, SecretError> {
        let secrets = self.secrets();
        let user = get_string(secrets, CLOUD_LINKED_USER_ID)?;
        let relay_url = get_string(secrets, RELAY_URL_SECRET)?;
        let relay_issuer = get_string(secrets, RELAY_ISSUER_SECRET)?;
        let runtime_config = secrets.get(CLOUD_ENDPOINT_RUNTIME_CONFIG)?;
        let publish = get_string(secrets, PUBLISH_AGENT_ACTIVITY_SECRET)?;
        let hold = get_string(secrets, HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET)?;
        Ok(LinkState {
            linked: user.is_some(),
            cloud_user_id: user,
            relay_url,
            relay_issuer,
            // Only stored for managed links; a publish-only link leaves it absent.
            managed_tunnel_active: Some(runtime_config.is_some()),
            publish_agent_activity: publish.as_deref() == Some("true"),
            hold_webhooks_while_offline: Some(hold.as_deref() == Some("true")),
        })
    }

    /// `GET /api/connect/link-state`
    pub fn link_state(&self) -> Result<LinkState, CloudLinkError> {
        self.read_cloud_link_state()
            .map_err(secret_to_internal(InternalOperation::ReadRelayConfig))
    }

    /// Stops the tunnel and forgets the link, including the CLI's wish to keep it
    /// (`POST /api/connect/unlink`).
    pub async fn unlink(&self) -> Result<RelayConfigResult, CloudLinkError> {
        let runtime = self.deps().runtime.clone();
        runtime
            .with_link_state_lock(async {
                let status = self.deps().runtime.apply_config(None).await;
                let remove = secret_to_internal(InternalOperation::RemoveRelayConfig);
                for name in [
                    CLOUD_LINKED_USER_ID,
                    RELAY_URL_SECRET,
                    RELAY_ISSUER_SECRET,
                    RELAY_ENVIRONMENT_CREDENTIAL_SECRET,
                    CLOUD_MINT_PUBLIC_KEY,
                    CLOUD_ENDPOINT_RUNTIME_CONFIG,
                    CLOUD_ENDPOINT_CONFIRMED_ORIGIN,
                    PUBLISH_AGENT_ACTIVITY_SECRET,
                    HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET,
                ] {
                    self.secrets().remove(name).map_err(&remove)?;
                }
                set_cli_desired_cloud_link(self.secrets(), false, DesiredLinkMode::Managed)
                    .map_err(secret_to_internal(InternalOperation::PersistDesiredLink))?;
                Ok(RelayConfigResult {
                    ok: true,
                    endpoint_runtime_status: status.to_json(),
                })
            })
            .await
    }

    async fn push_hold_webhooks_while_offline(&self, hold: bool) -> Result<(), CloudLinkError> {
        let Some(connection) = read_relay_connection(self.secrets()) else {
            return Err(CloudLinkError::NotLinked);
        };
        let environment_id = self.deps().environment.environment_id();
        let request = LinkPreferencesRequest {
            hold_webhooks_while_offline: hold,
        };
        let call = self.deps().control_plane.update_link_preferences(
            &connection.url,
            &connection.environment_credential,
            &environment_id,
            &request,
        );
        match tokio::time::timeout(Duration::from_secs(10), call).await {
            Ok(Ok(_)) => Ok(()),
            _ => Err(CloudLinkError::internal(
                InternalOperation::UpdateWebhookSettings,
                "relay",
            )),
        }
    }

    fn save_preference(&self, name: &str, value: bool) -> Result<(), CloudLinkError> {
        self.secrets()
            .set(name, value.to_string().as_bytes())
            .map_err(secret_to_internal(InternalOperation::PersistPreferences))
    }

    /// Saves both preferences, all or nothing. The activity setting is saved first. Holding
    /// webhooks while offline is decided by the relay, so the relay is told before the local copy
    /// is saved. If either step fails, the activity setting is put back, and so is the relay when
    /// only the local save failed.
    async fn save_preferences(&self, input: PreferencesRequest) -> Result<(), CloudLinkError> {
        let _guard = self.inner.preferences_lock.lock().await;
        let read = secret_to_internal(InternalOperation::ReadPreferences);
        // A failed read stops here, before anything changes, because a guessed value would be the
        // rollback target.
        let previous_activity = self
            .secrets()
            .get(PUBLISH_AGENT_ACTIVITY_SECRET)
            .map_err(&read)?;
        let previous_hold = self
            .secrets()
            .get(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET)
            .map_err(&read)?;
        self.save_preference(PUBLISH_AGENT_ACTIVITY_SECRET, input.publish_agent_activity)?;
        if let Some(next) = input.hold_webhooks_while_offline {
            let previous = previous_hold.as_deref() == Some(b"true".as_slice());
            let result = async {
                self.push_hold_webhooks_while_offline(next).await?;
                if let Err(error) = self.save_preference(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET, next) {
                    if previous != next {
                        if let Err(rollback) = self.push_hold_webhooks_while_offline(previous).await {
                            tracing::warn!(%rollback, "Could not roll back a T3 Connect preference");
                        }
                    }
                    return Err(error);
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                let restored = match previous_activity {
                    None => self.secrets().remove(PUBLISH_AGENT_ACTIVITY_SECRET),
                    Some(bytes) => self.secrets().set(PUBLISH_AGENT_ACTIVITY_SECRET, &bytes),
                };
                if let Err(rollback) = restored {
                    tracing::warn!(%rollback, "Could not roll back a T3 Connect preference");
                }
                return Err(error);
            }
        }
        self.deps().awareness.request_catch_up();
        Ok(())
    }

    /// Saves this environment's T3 Connect preferences and returns the link state
    /// (`POST /api/connect/preferences`). Runs to completion even if the caller goes away, so an
    /// update always finishes or undoes itself.
    pub async fn update_preferences(
        &self,
        input: PreferencesRequest,
    ) -> Result<LinkState, CloudLinkError> {
        let this = self.clone();
        tokio::spawn(async move { this.save_preferences(input).await })
            .await
            .map_err(|e| CloudLinkError::internal(InternalOperation::PersistPreferences, e))??;
        self.read_cloud_link_state()
            .map_err(secret_to_internal(InternalOperation::ReadPreferences))
    }

    // ---- the relay's signed requests -------------------------------------------------------

    fn relay_trust(
        &self,
        operation: InternalOperation,
    ) -> Result<(String, String), CloudLinkError> {
        let missing = |failure| CloudLinkError::Auth(failure);
        let key = get_string(self.secrets(), CLOUD_MINT_PUBLIC_KEY)
            .map_err(|_| CloudLinkError::internal(operation, "read"))?
            .ok_or_else(|| missing(AuthFailure::CloudMintPublicKeyMissing))?;
        let issuer = match get_string(self.secrets(), RELAY_ISSUER_SECRET)
            .map_err(|_| CloudLinkError::internal(operation, "read"))?
        {
            Some(issuer) => issuer,
            None => get_string(self.secrets(), RELAY_URL_SECRET)
                .map_err(|_| CloudLinkError::internal(operation, "read"))?
                .ok_or_else(|| missing(AuthFailure::CloudRelayIssuerMissing))?,
        };
        Ok((key, issuer))
    }

    fn verify_signed_request(
        &self,
        token: &str,
        typ: &str,
        kind: SignedRequest,
        public_key: &str,
        issuer: &str,
        environment_id: &str,
        linked_user: &str,
        now_seconds: i64,
        expected_scope: &str,
    ) -> Result<ProofClaims, CloudLinkError> {
        let claims = verify_relay_jwt(&VerifyOptions {
            public_key,
            token,
            typ,
            issuer: &normalize_relay_issuer(issuer),
            audience: &format!("t3-env:{environment_id}"),
            now_epoch_seconds: now_seconds,
            max_token_age_secs: None,
        })
        .ok()
        .and_then(|claims| serde_json::from_value::<ProofClaims>(Value::Object(claims)).ok())
        .ok_or(CloudLinkError::ProofRejected(kind))?;
        let thumbprint_ok = kind == SignedRequest::Health
            || match (&claims.cnf, &claims.client_proof_key_thumbprint) {
                (Some(cnf), Some(thumbprint)) => cnf.jkt == *thumbprint,
                _ => false,
            };
        if claims.environment_id != environment_id
            || claims.sub != linked_user
            || !thumbprint_ok
            || !has_bounded_cloud_proof_lifetime(claims.iat, claims.exp, now_seconds)
            || !has_exact_scope(&claims.scope, expected_scope)
        {
            return Err(CloudLinkError::ProofRejected(kind));
        }
        Ok(claims)
    }

    /// Answers the relay's signed health check once per proof (`POST /api/t3-connect/health`).
    pub fn answer_health_request(
        &self,
        request: &HealthRequest,
    ) -> Result<HealthResponse, CloudLinkError> {
        let internal =
            |cause: String| CloudLinkError::internal(InternalOperation::AnswerHealth, cause);
        let (public_key, issuer) = self.relay_trust(InternalOperation::AnswerHealth)?;
        let environment_id = self.deps().environment.environment_id();
        let linked_user = self.read_installed_cloud_user_id()?;
        let now_ms = self.deps().clock.now_ms();
        let now = now_ms.div_euclid(1_000);
        let proof = self.verify_signed_request(
            request.proof.as_str(),
            RELAY_HEALTH_REQUEST_TYP,
            SignedRequest::Health,
            &public_key,
            &issuer,
            &environment_id,
            &linked_user,
            now,
            "environment:status",
        )?;
        let consumed = consume_cloud_replay_guards(
            self.secrets(),
            &[
                format!("{CLOUD_HEALTH_JTI_PREFIX}{}", proof.jti),
                format!("{CLOUD_HEALTH_NONCE_PREFIX}{}", proof.nonce),
            ],
            format_iso(now_ms).as_bytes(),
        )
        .map_err(|e| internal(e.message))?;
        if !consumed {
            return Err(CloudLinkError::ProofReplayed(SignedRequest::Health));
        }
        let key_pair = get_or_create_environment_key_pair(self.secrets())
            .map_err(|e| internal(e.to_string()))?;
        let descriptor = self.deps().environment.descriptor();
        let checked_at = format_iso(now_ms);
        let payload = json!({
            "iss": format!("t3-env:{environment_id}"),
            "aud": normalize_relay_issuer(&issuer),
            "sub": environment_id,
            "jti": uuid::Uuid::new_v4().to_string(),
            "iat": now,
            "exp": (now_ms + PROOF_TTL_SECONDS * 1_000).div_euclid(1_000),
            "environmentId": environment_id,
            "requestNonce": proof.nonce,
            "status": "online",
            "descriptor": descriptor,
            "checkedAt": checked_at,
        });
        let response_proof = self.sign(
            &key_pair,
            RELAY_HEALTH_RESPONSE_TYP,
            &payload,
            AuthFailure::CloudHealthJwtSigning,
        )?;
        Ok(HealthResponse {
            environment_id: t3_contracts::EnvironmentId::new(&environment_id)
                .expect("environment id"),
            status: HealthStatus::Online,
            descriptor,
            checked_at: t3_contracts::TrimmedNonEmptyString::new(&checked_at).expect("iso"),
            proof: t3_contracts::TrimmedNonEmptyString::new(&response_proof).expect("proof"),
        })
    }

    /// Issues a short-lived pairing credential for a client the relay vouched for, once per proof
    /// (`POST /api/connect/mint-credential` and `/api/t3-connect/mint-credential`).
    pub async fn mint_credential(
        &self,
        request: &MintRequest,
    ) -> Result<MintResponse, CloudLinkError> {
        let internal =
            |cause: String| CloudLinkError::internal(InternalOperation::IssueCredential, cause);
        let (public_key, issuer) = self.relay_trust(InternalOperation::IssueCredential)?;
        let environment_id = self.deps().environment.environment_id();
        let linked_user = self.read_installed_cloud_user_id()?;
        let now_ms = self.deps().clock.now_ms();
        let now = now_ms.div_euclid(1_000);
        let proof = self.verify_signed_request(
            request.proof.as_str(),
            RELAY_MINT_REQUEST_TYP,
            SignedRequest::Mint,
            &public_key,
            &issuer,
            &environment_id,
            &linked_user,
            now,
            "environment:connect",
        )?;
        let consumed = consume_cloud_replay_guards(
            self.secrets(),
            &[
                format!("{CLOUD_MINT_JTI_PREFIX}{}", proof.jti),
                format!("{CLOUD_MINT_NONCE_PREFIX}{}", proof.nonce),
            ],
            format_iso(now_ms).as_bytes(),
        )
        .map_err(|e| internal(e.message))?;
        if !consumed {
            return Err(CloudLinkError::ProofReplayed(SignedRequest::Mint));
        }
        let key_pair = get_or_create_environment_key_pair(self.secrets())
            .map_err(|e| internal(e.to_string()))?;
        let thumbprint = proof
            .client_proof_key_thumbprint
            .clone()
            .unwrap_or_default();
        let issued = self
            .deps()
            .pairing
            .create_pairing_link(PairingRequest {
                scopes: t3_contracts::AUTH_STANDARD_CLIENT_SCOPES,
                subject: "cloud-connect",
                ttl: CLOUD_CONNECT_CREDENTIAL_TTL,
                label: "T3 Connect connect",
                proof_key_thumbprint: thumbprint.clone(),
            })
            .await
            .map_err(|message| CloudLinkError::Auth(AuthFailure::Other(message)))?;
        let payload = json!({
            "iss": format!("t3-env:{environment_id}"),
            "aud": normalize_relay_issuer(&issuer),
            "sub": environment_id,
            "jti": uuid::Uuid::new_v4().to_string(),
            "iat": now,
            "exp": issued.expires_at_ms.div_euclid(1_000),
            "environmentId": environment_id,
            "clientProofKeyThumbprint": thumbprint,
            "requestNonce": proof.nonce,
            "credential": issued.credential,
        });
        let response_proof = self.sign(
            &key_pair,
            RELAY_MINT_RESPONSE_TYP,
            &payload,
            AuthFailure::CloudMintJwtSigning,
        )?;
        Ok(MintResponse {
            credential: t3_contracts::TrimmedNonEmptyString::new(&issued.credential)
                .map_err(|e| internal(e.to_string()))?,
            expires_at: t3_contracts::TrimmedNonEmptyString::new(format_iso(issued.expires_at_ms))
                .expect("iso"),
            proof: t3_contracts::TrimmedNonEmptyString::new(&response_proof).expect("proof"),
        })
    }
}

enum RecoveryAction<'a> {
    Register {
        tunnel_id: &'a str,
        origin: &'a ManagedEndpointOrigin,
    },
    Recover {
        origin: &'a ManagedEndpointOrigin,
    },
}
