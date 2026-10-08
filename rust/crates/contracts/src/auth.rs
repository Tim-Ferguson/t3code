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
fn deserialize_permissions<'de, D: serde::Deserializer<'de>>(
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
