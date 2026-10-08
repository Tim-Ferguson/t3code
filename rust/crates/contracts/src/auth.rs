use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AuthEnvironmentScope {
    #[serde(rename = "orchestration:read")]
    OrchestrationRead,
    #[serde(rename = "orchestration:operate")]
    OrchestrationOperate,
    #[serde(rename = "settings:write")]
    SettingsWrite,
    #[serde(rename = "providers:manage")]
    ProvidersManage,
    #[serde(rename = "environment:maintain")]
    EnvironmentMaintain,
    #[serde(rename = "preview:operate")]
    PreviewOperate,
    #[serde(rename = "diagnostics:read")]
    DiagnosticsRead,
    #[serde(rename = "terminal:read")]
    TerminalRead,
    #[serde(rename = "terminal:operate")]
    TerminalOperate,
    #[serde(rename = "source-control:write")]
    SourceControlWrite,
    #[serde(rename = "filesystem:read")]
    FilesystemRead,
    #[serde(rename = "filesystem:write")]
    FilesystemWrite,
    #[serde(rename = "review:write")]
    ReviewWrite,
    #[serde(rename = "access:read")]
    AccessRead,
    #[serde(rename = "access:write")]
    AccessWrite,
    #[serde(rename = "relay:read")]
    RelayRead,
    #[serde(rename = "relay:write")]
    RelayWrite,
}
use AuthEnvironmentScope::*;
impl AuthEnvironmentScope {
    pub fn legacy_parent(self) -> Option<Self> {
        match self {
            FilesystemRead | DiagnosticsRead => Some(OrchestrationRead),
            SettingsWrite | ProvidersManage | EnvironmentMaintain | PreviewOperate
            | SourceControlWrite | FilesystemWrite => Some(OrchestrationOperate),
            TerminalRead => Some(TerminalOperate),
            _ => None,
        }
    }
    pub fn is_legacy(self) -> bool {
        matches!(
            self,
            OrchestrationRead
                | OrchestrationOperate
                | TerminalOperate
                | ReviewWrite
                | AccessRead
                | AccessWrite
                | RelayRead
                | RelayWrite
        )
    }
    pub fn is_grantable(self) -> bool {
        self != ReviewWrite
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthScopeResponse {
    pub scopes: Vec<AuthEnvironmentScope>,
    pub permissions: Vec<AuthEnvironmentScope>,
}
pub fn auth_scope_response(permissions: &[AuthEnvironmentScope]) -> AuthScopeResponse {
    AuthScopeResponse {
        scopes: permissions
            .iter()
            .copied()
            .filter(|s| s.is_legacy())
            .collect(),
        permissions: permissions.to_vec(),
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthScopeRequiredResponse {
    pub required_scope: AuthEnvironmentScope,
    pub required_permission: AuthEnvironmentScope,
}
pub fn auth_scope_required_response(scope: AuthEnvironmentScope) -> AuthScopeRequiredResponse {
    AuthScopeRequiredResponse {
        required_scope: scope.legacy_parent().unwrap_or(scope),
        required_permission: scope,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionGrantInput {
    pub authenticated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<AuthEnvironmentScope>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_permissions"
    )]
    pub permissions: Option<Vec<AuthEnvironmentScope>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SessionGrantAuth>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionGrantAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_update_scope: Option<String>,
}
pub(crate) fn deserialize_permissions<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<AuthEnvironmentScope>>, D::Error> {
    // Unknown array values are discarded individually, preserving an explicit []
    // rather than accidentally restoring old broad-scope authorization.
    let values = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(Some(
        values
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect(),
    ))
}
pub fn session_grants_scope(session: &SessionGrantInput, scope: AuthEnvironmentScope) -> bool {
    if !session.authenticated {
        return false;
    }
    if let Some(permissions) = &session.permissions {
        return permissions.contains(&scope);
    }
    let scopes = session.scopes.as_deref().unwrap_or_default();
    if scopes.contains(&scope) {
        return true;
    }
    if session
        .auth
        .as_ref()
        .is_some_and(|a| a.server_update_scope.is_some())
    {
        return false;
    }
    scope
        .legacy_parent()
        .is_some_and(|parent| scopes.contains(&parent))
}
pub fn session_has_legacy_permissions(session: &SessionGrantInput) -> bool {
    session.authenticated
        && session.permissions.as_ref().is_some_and(|p| {
            p.iter().all(|s| s.is_legacy())
                && p.iter().any(|s| {
                    matches!(
                        s,
                        OrchestrationRead | OrchestrationOperate | TerminalOperate
                    )
                })
        })
}

// Typed authentication JSON contracts, matching packages/contracts/src/auth.ts.
use crate::{
    AuthSessionId, ClientSurface, LiteralBool, LiteralInt, TrimmedNonEmptyString, UtcDateTime,
    deserialize_optional, deserialize_required_nullable,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerAuthPolicy {
    #[serde(rename = "desktop-managed-local")]
    DesktopManagedLocal,
    #[serde(rename = "loopback-browser")]
    LoopbackBrowser,
    #[serde(rename = "remote-reachable")]
    RemoteReachable,
    #[serde(rename = "unsafe-no-auth")]
    UnsafeNoAuth,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerAuthBootstrapMethod {
    #[serde(rename = "desktop-bootstrap")]
    DesktopBootstrap,
    #[serde(rename = "one-time-token")]
    OneTimeToken,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerAuthSessionMethod {
    #[serde(rename = "browser-session-cookie")]
    BrowserSessionCookie,
    #[serde(rename = "bearer-access-token")]
    BearerAccessToken,
    #[serde(rename = "dpop-access-token")]
    DpopAccessToken,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerUpdateScope {
    #[serde(rename = "environment:maintain")]
    EnvironmentMaintain,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientWebDeployment {
    #[serde(rename = "hosted")]
    Hosted,
    #[serde(rename = "server")]
    Server,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthClientMetadataDeviceType {
    #[serde(rename = "desktop")]
    Desktop,
    #[serde(rename = "mobile")]
    Mobile,
    #[serde(rename = "tablet")]
    Tablet,
    #[serde(rename = "bot")]
    Bot,
    #[serde(rename = "unknown")]
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMcpClientAccess {
    #[serde(rename = "read-only")]
    ReadOnly,
    #[serde(rename = "approval-required")]
    ApprovalRequired,
    #[serde(rename = "auto-accept-edits")]
    AutoAcceptEdits,
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "full-access")]
    FullAccess,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthTokenExchangeGrantType {
    #[serde(rename = "urn:ietf:params:oauth:grant-type:token-exchange")]
    TokenExchange,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthAccessTokenType {
    #[serde(rename = "urn:ietf:params:oauth:token-type:access_token")]
    AccessToken,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthEnvironmentBootstrapTokenType {
    #[serde(rename = "urn:t3:params:oauth:token-type:environment-bootstrap")]
    EnvironmentBootstrap,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthTokenType {
    #[serde(rename = "Bearer")]
    Bearer,
    #[serde(rename = "DPoP")]
    DPoP,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerAuthDescriptor {
    pub policy: ServerAuthPolicy,
    pub bootstrap_methods: Vec<ServerAuthBootstrapMethod>,
    pub session_methods: Vec<ServerAuthSessionMethod>,
    pub session_cookie_name: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub server_update_scope: Option<ServerUpdateScope>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthBrowserSessionRequest {
    pub credential: TrimmedNonEmptyString,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthBrowserSessionResult {
    pub authenticated: LiteralBool<true>,
    pub scopes: Vec<AuthEnvironmentScope>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_permissions"
    )]
    pub permissions: Option<Vec<AuthEnvironmentScope>>,
    pub session_method: ServerAuthSessionMethod,
    pub expires_at: UtcDateTime,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthClientPresentationMetadata {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub label: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub device_type: Option<AuthClientMetadataDeviceType>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub os: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub os_major_version: Option<crate::SafeInt>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub device_model: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub surface: Option<ClientSurface>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub web_deployment: Option<ClientWebDeployment>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub app_version: Option<TrimmedNonEmptyString>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthTokenExchangeRequest {
    pub grant_type: AuthTokenExchangeGrantType,
    pub subject_token: TrimmedNonEmptyString,
    pub subject_token_type: AuthEnvironmentBootstrapTokenType,
    pub requested_token_type: AuthAccessTokenType,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub scope: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub client_label: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub client_device_type: Option<AuthClientMetadataDeviceType>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub client_os: Option<TrimmedNonEmptyString>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthAccessTokenResult {
    pub access_token: TrimmedNonEmptyString,
    pub issued_token_type: AuthAccessTokenType,
    pub token_type: AuthTokenType,
    pub expires_in: serde_json::Number,
    pub scope: TrimmedNonEmptyString,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthWebSocketTicketResult {
    pub ticket: TrimmedNonEmptyString,
    pub expires_at: UtcDateTime,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthPairingCredentialResult {
    pub id: TrimmedNonEmptyString,
    pub credential: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub label: Option<TrimmedNonEmptyString>,
    pub expires_at: UtcDateTime,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthPairingLink {
    pub id: TrimmedNonEmptyString,
    pub scopes: Vec<AuthEnvironmentScope>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_permissions"
    )]
    pub permissions: Option<Vec<AuthEnvironmentScope>>,
    pub subject: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub label: Option<TrimmedNonEmptyString>,
    pub created_at: UtcDateTime,
    pub expires_at: UtcDateTime,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthClientMetadata {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub label: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub ip_address: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub user_agent: Option<TrimmedNonEmptyString>,
    pub device_type: AuthClientMetadataDeviceType,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub os: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser: Option<TrimmedNonEmptyString>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthClientSession {
    pub session_id: AuthSessionId,
    pub subject: TrimmedNonEmptyString,
    pub scopes: Vec<AuthEnvironmentScope>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_permissions"
    )]
    pub permissions: Option<Vec<AuthEnvironmentScope>>,
    pub method: ServerAuthSessionMethod,
    pub client: AuthClientMetadata,
    pub issued_at: UtcDateTime,
    pub expires_at: UtcDateTime,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub last_connected_at: Option<UtcDateTime>,
    pub connected: bool,
    pub current: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthAccessSnapshot {
    pub pairing_links: Vec<AuthPairingLink>,
    pub client_sessions: Vec<AuthClientSession>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthPairingLinkRemoved {
    pub id: TrimmedNonEmptyString,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthClientRemoved {
    pub session_id: AuthSessionId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthRevokePairingLinkInput {
    pub id: TrimmedNonEmptyString,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthRevokeClientSessionInput {
    pub session_id: AuthSessionId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSessionState {
    pub authenticated: bool,
    pub auth: ServerAuthDescriptor,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub scopes: Option<Vec<AuthEnvironmentScope>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_permissions"
    )]
    pub permissions: Option<Vec<AuthEnvironmentScope>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub session_method: Option<ServerAuthSessionMethod>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub expires_at: Option<UtcDateTime>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeaderBearerMethod {
    #[serde(rename = "header")]
    Header,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OAuthCodeResponseType {
    #[serde(rename = "code")]
    Code,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OAuthAuthorizationCodeGrantType {
    #[serde(rename = "authorization_code")]
    AuthorizationCode,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OAuthS256ChallengeMethod {
    #[serde(rename = "S256")]
    S256,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OAuthNoneAuthenticationMethod {
    #[serde(rename = "none")]
    None,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BearerTokenType {
    #[serde(rename = "Bearer")]
    Bearer,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthMcpProtectedResourceMetadata {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub scopes_supported: Vec<AuthEnvironmentScope>,
    pub bearer_methods_supported: Vec<HeaderBearerMethod>,
    pub resource_name: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthMcpAuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
    pub response_types_supported: Vec<OAuthCodeResponseType>,
    pub grant_types_supported: Vec<OAuthAuthorizationCodeGrantType>,
    pub code_challenge_methods_supported: Vec<OAuthS256ChallengeMethod>,
    pub token_endpoint_auth_methods_supported: Vec<OAuthNoneAuthenticationMethod>,
    pub scopes_supported: Vec<AuthEnvironmentScope>,
    pub authorization_response_iss_parameter_supported: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthMcpClientRegistration {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub client_name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub redirect_uris: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub token_endpoint_auth_method: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthMcpRegisteredClient {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<OAuthAuthorizationCodeGrantType>,
    pub response_types: Vec<OAuthCodeResponseType>,
    pub token_endpoint_auth_method: OAuthNoneAuthenticationMethod,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthMcpAuthorizationRequest {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub response_type: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub client_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub redirect_uri: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub code_challenge: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub code_challenge_method: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub state: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub resource: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMcpApprovalDetails {
    pub client_name: String,
    pub redirect_host: String,
    pub environment_host: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub csrf_token: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub one_click_access: Option<Vec<AuthMcpClientAccess>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMcpApprovalRedirect {
    pub redirect_to: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMcpApprovalDecisionRequest {
    pub authorization: AuthMcpAuthorizationRequest,
    pub decision: AuthMcpApprovalDecision,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthMcpTokenRequest {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub grant_type: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub code: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub redirect_uri: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub client_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub code_verifier: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub resource: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthMcpTokenResult {
    pub access_token: String,
    pub token_type: BearerTokenType,
    pub expires_in: serde_json::Number,
    pub scope: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct AuthGrantScope(AuthEnvironmentScope);
impl AuthGrantScope {
    pub fn new(scope: AuthEnvironmentScope) -> Result<Self, crate::ValidationError> {
        if scope.is_grantable() {
            Ok(Self(scope))
        } else {
            Err(crate::ValidationError {
                expected: "a grantable environment scope",
            })
        }
    }
    pub fn scope(self) -> AuthEnvironmentScope {
        self.0
    }
}
impl<'de> Deserialize<'de> for AuthGrantScope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(AuthEnvironmentScope::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthCreatePairingCredentialInput {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub label: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub scopes: Option<Vec<AuthGrantScope>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum AuthMcpApprovalDecision {
    #[serde(rename = "deny")]
    Deny,
    #[serde(rename = "pairing-code")]
    PairingCode {
        access: AuthMcpClientAccess,
        code: TrimmedNonEmptyString,
    },
    #[serde(rename = "browser-session")]
    BrowserSession {
        access: AuthMcpClientAccess,
        csrf_token: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthAccessStreamEvent {
    #[serde(rename = "snapshot")]
    Snapshot {
        version: LiteralInt<1>,
        revision: serde_json::Number,
        payload: AuthAccessSnapshot,
    },
    #[serde(rename = "pairingLinkUpserted")]
    PairingLinkUpserted {
        version: LiteralInt<1>,
        revision: serde_json::Number,
        payload: AuthPairingLink,
    },
    #[serde(rename = "pairingLinkRemoved")]
    PairingLinkRemoved {
        version: LiteralInt<1>,
        revision: serde_json::Number,
        payload: AuthPairingLinkRemoved,
    },
    #[serde(rename = "clientUpserted")]
    ClientUpserted {
        version: LiteralInt<1>,
        revision: serde_json::Number,
        payload: AuthClientSession,
    },
    #[serde(rename = "clientRemoved")]
    ClientRemoved {
        version: LiteralInt<1>,
        revision: serde_json::Number,
        payload: AuthClientRemoved,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum AuthError {
    AuthAccessStreamError {
        message: String,
    },
    EnvironmentAuthorizationError {
        message: String,
        required_scope: AuthEnvironmentScope,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        required_permission: Option<String>,
    },
    AuthMcpApprovalError {
        message: String,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMcpRegistrationErrorCode {
    InvalidClientMetadata,
    InvalidRedirectUri,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthMcpRegistrationError {
    pub error: AuthMcpRegistrationErrorCode,
    pub error_description: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMcpTokenErrorCode {
    InvalidRequest,
    InvalidClient,
    InvalidGrant,
    UnsupportedGrantType,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthMcpTokenError {
    pub error: AuthMcpTokenErrorCode,
    pub error_description: String,
}

pub const AUTH_STANDARD_CLIENT_SCOPES: &[AuthEnvironmentScope] = &[
    OrchestrationRead,
    OrchestrationOperate,
    SettingsWrite,
    ProvidersManage,
    EnvironmentMaintain,
    PreviewOperate,
    DiagnosticsRead,
    TerminalRead,
    TerminalOperate,
    SourceControlWrite,
    FilesystemRead,
    FilesystemWrite,
    RelayRead,
];
pub const AUTH_ADMINISTRATIVE_SCOPES: &[AuthEnvironmentScope] = &[
    OrchestrationRead,
    OrchestrationOperate,
    SettingsWrite,
    ProvidersManage,
    EnvironmentMaintain,
    PreviewOperate,
    DiagnosticsRead,
    TerminalRead,
    TerminalOperate,
    SourceControlWrite,
    FilesystemRead,
    FilesystemWrite,
    RelayRead,
    AccessRead,
    AccessWrite,
    RelayWrite,
];
impl From<&AuthSessionState> for SessionGrantInput {
    fn from(state: &AuthSessionState) -> Self {
        Self {
            authenticated: state.authenticated,
            scopes: state.scopes.clone(),
            permissions: state.permissions.clone(),
            auth: Some(SessionGrantAuth {
                server_update_scope: state
                    .auth
                    .server_update_scope
                    .map(|_| "environment:maintain".to_owned()),
            }),
        }
    }
}
impl AuthSessionState {
    pub fn grants_scope(&self, scope: AuthEnvironmentScope) -> bool {
        session_grants_scope(&self.into(), scope)
    }
}
