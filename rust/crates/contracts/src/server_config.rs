//! Environment and provider discovery contracts.
use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionEnvironmentPlatformOs {
    #[serde(rename = "darwin")]
    Darwin,
    #[serde(rename = "linux")]
    Linux,
    #[serde(rename = "windows")]
    Windows,
    #[serde(rename = "unknown")]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionEnvironmentPlatformArch {
    #[serde(rename = "arm64")]
    Arm64,
    #[serde(rename = "x64")]
    X64,
    #[serde(rename = "other")]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvironmentMachineKind {
    #[serde(rename = "server")]
    Server,
    #[serde(rename = "cloud")]
    Cloud,
    #[serde(rename = "linux")]
    Linux,
    #[serde(rename = "desktop")]
    Desktop,
    #[serde(rename = "laptop")]
    Laptop,
    #[serde(rename = "mac-mini")]
    MacMini,
    #[serde(rename = "mac-studio")]
    MacStudio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadEnvMode {
    #[serde(rename = "local")]
    Local,
    #[serde(rename = "worktree")]
    Worktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeSubmodules {
    #[serde(rename = "recursive")]
    Recursive,
    #[serde(rename = "top-level")]
    TopLevel,
    #[serde(rename = "none")]
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerSelfUpdateMethod {
    #[serde(rename = "boot-service")]
    BootService,
    #[serde(rename = "respawn")]
    Respawn,
    #[serde(rename = "desktop-app")]
    DesktopApp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerSelfUpdateCapability {
    #[serde(rename = "boot-service")]
    BootService,
    #[serde(rename = "respawn")]
    Respawn,
    #[serde(rename = "desktop-managed")]
    DesktopManaged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionEnvironmentPlatform {
    pub os: ExecutionEnvironmentPlatformOs,

    pub arch: ExecutionEnvironmentPlatformArch,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_forward_optional",
        serialize_with = "serialize_forward_optional"
    )]
    pub machine: Option<Option<EnvironmentMachineKind>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileAttachmentCapability {
    pub max_upload_bytes: PositiveInt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionEnvironmentCapabilities {
    #[serde(
        default,
        deserialize_with = "crate::provider::deserialize_default_bool"
    )]
    pub repository_identity: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub connection_probe: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub attachment_uploads: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub question_attachments: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pull_requests: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pull_request_checks: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub inline_message_context: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub required_worktree_bootstrap: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_settlement: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_auto_settlement: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub storage_cleanup: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub project_worktree_cleanup: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub worktrees_directory: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_restart_continuation: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub project_settings_overrides: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_snooze: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub environment_themes: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_limit_sources: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_price_overrides: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_model_aliases: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_pinning: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_pin_reorder: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_active_reorder: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_auto_settle_opt_out: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_title_regeneration: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_visited_tracking: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_pull_request_linking: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub server_resolved_command_context: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_pull_requests: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_pull_request_watch: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pull_request_stack_actions: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub server_self_update_progress: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub server_update_thread_continuation: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub agent_activity_publishing: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub project_clone_tracking: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub environment_icon: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub desktop_app_update: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub server_browser: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub file_attachments: Option<FileAttachmentCapability>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub server_self_update: Option<ServerSelfUpdateCapability>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_forward_optional",
        serialize_with = "serialize_forward_optional"
    )]
    pub server_installation: Option<Option<ServerInstallation>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionEnvironmentDescriptor {
    pub environment_id: EnvironmentId,

    pub label: TrimmedNonEmptyString,

    pub platform: ExecutionEnvironmentPlatform,

    pub server_version: TrimmedNonEmptyString,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub orchestration_protocol_version: Option<SafeInt>,

    pub capabilities: ExecutionEnvironmentCapabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryIdentityLocator {
    pub source: GitRemoteSource,

    pub remote_name: TrimmedNonEmptyString,

    pub remote_url: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitRemoteSource {
    #[serde(rename = "git-remote")]
    GitRemote,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryOrigin {
    pub canonical_key: TrimmedNonEmptyString,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub display_name: Option<TrimmedNonEmptyString>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryIdentity {
    pub canonical_key: TrimmedNonEmptyString,

    pub locator: RepositoryIdentityLocator,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub web_url: Option<TrimmedNonEmptyString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub root_path: Option<TrimmedNonEmptyString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub display_name: Option<TrimmedNonEmptyString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub provider: Option<TrimmedNonEmptyString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub owner: Option<TrimmedNonEmptyString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub name: Option<TrimmedNonEmptyString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub origin: Option<RepositoryOrigin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedProjectRef {
    pub environment_id: EnvironmentId,

    pub project_id: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedThreadRef {
    pub environment_id: EnvironmentId,

    pub thread_id: ThreadId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProviderState {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "warning")]
    Warning,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "disabled")]
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProviderAuthStatus {
    #[serde(rename = "authenticated")]
    Authenticated,
    #[serde(rename = "unauthenticated")]
    Unauthenticated,
    #[serde(rename = "unknown")]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProviderAvailability {
    #[serde(rename = "available")]
    Available,
    #[serde(rename = "unavailable")]
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProviderModelBadge {
    #[serde(rename = "new")]
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProviderCompatibilityStatus {
    #[serde(rename = "unknown")]
    Unknown,
    #[serde(rename = "supported")]
    Supported,
    #[serde(rename = "graceful")]
    Graceful,
    #[serde(rename = "unsupported")]
    Unsupported,
    #[serde(rename = "broken")]
    Broken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProviderVersionAdvisoryStatus {
    #[serde(rename = "unknown")]
    Unknown,
    #[serde(rename = "current")]
    Current,
    #[serde(rename = "behind_latest")]
    BehindLatest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProviderUpdateStatus {
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "queued")]
    Queued,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "succeeded")]
    Succeeded,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "unchanged")]
    Unchanged,
}

crate::history::object_struct! { pub struct AcpRegistryUrlAuthAction {
    pub elicitation_id: BoundedTrimmedString<256>,

    pub url: BoundedString<2048>,

    pub message: BoundedString<1024>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub created_at: Option<BoundedString<128>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub expires_at: Option<BoundedString<128>>,
}
 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderAuth {
    pub status: ServerProviderAuthStatus,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub r#type: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub label: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub email: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub action: Option<Option<AcpRegistryUrlAuthAction>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub can_logout: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub subscription_sharing: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub profile_id: Option<Option<TrimmedNonEmptyString>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderModel {
    pub slug: TrimmedNonEmptyString,

    pub name: TrimmedNonEmptyString,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub short_name: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sub_provider: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub aliases: Option<Option<Vec<TrimmedNonEmptyString>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub badge: Option<Option<ServerProviderModelBadge>>,

    pub is_custom: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub is_default: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub is_legacy: Option<Option<bool>>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub capabilities: Option<ModelCapabilities>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderSlashCommandInput {
    pub hint: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderSlashCommand {
    pub name: TrimmedNonEmptyString,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub description: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub input: Option<Option<ServerProviderSlashCommandInput>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderSkill {
    pub name: TrimmedNonEmptyString,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub description: Option<Option<TrimmedNonEmptyString>>,

    pub path: TrimmedNonEmptyString,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub scope: Option<Option<TrimmedNonEmptyString>>,

    pub enabled: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub display_name: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub short_description: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub user_invocation_only: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub user_invocable: Option<Option<bool>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderWorkspaceSnapshot {
    pub cwd: TrimmedNonEmptyString,

    pub checked_at: IsoDateTime,

    pub slash_commands: Vec<ServerProviderSlashCommand>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub slash_commands_pending: Option<Option<bool>>,

    pub skills: Vec<ServerProviderSkill>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderContinuation {
    pub group_key: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderCompatibilityAdvisory {
    pub status: ServerProviderCompatibilityStatus,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub latest_version_status: Option<ServerProviderCompatibilityStatus>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub message: Option<TrimmedNonEmptyString>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub recommended_version: Option<TrimmedNonEmptyString>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub recommended_range: Option<TrimmedNonEmptyString>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderVersionAdvisory {
    pub status: ServerProviderVersionAdvisoryStatus,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub current_version: Option<TrimmedNonEmptyString>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub latest_version: Option<TrimmedNonEmptyString>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub update_command: Option<TrimmedNonEmptyString>,

    #[serde(
        default,
        deserialize_with = "crate::provider::deserialize_default_bool"
    )]
    pub can_update: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub can_install_version: Option<bool>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub checked_at: Option<IsoDateTime>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub message: Option<TrimmedNonEmptyString>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderUpdateState {
    pub status: ServerProviderUpdateStatus,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub started_at: Option<IsoDateTime>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub finished_at: Option<IsoDateTime>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub message: Option<TrimmedNonEmptyString>,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub output: Option<BoundedString<10000>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderSetup {
    pub can_authenticate: bool,

    pub can_install: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub documentation_url: Option<BoundedTrimmedString<2048>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderNativeSessions {
    pub can_list: bool,

    pub can_load: bool,

    pub can_resume: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub can_delete: Option<Option<bool>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderRuntimePaths {
    pub home_path: TrimmedNonEmptyString,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub shadow_home_path: Option<TrimmedNonEmptyString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProviderUsageWindowKind {
    #[serde(rename = "session")]
    Session,
    #[serde(rename = "weekly")]
    Weekly,
    #[serde(rename = "monthly")]
    Monthly,
    #[serde(rename = "other")]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageLimitsUnavailableReason {
    #[serde(rename = "unsupported")]
    Unsupported,
    #[serde(rename = "probeFailed")]
    Probefailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderUsageWindow {
    pub id: TrimmedNonEmptyString,

    pub kind: ServerProviderUsageWindowKind,

    pub label: TrimmedNonEmptyString,

    pub used_percent: UsagePercent,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub resets_at: Option<Option<IsoDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub window_duration_mins: Option<Option<NonNegativeInt>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderResetCredits {
    pub available_count: NonNegativeInt,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub next_expires_at: Option<Option<IsoDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub next_credit_id: Option<Option<TrimmedNonEmptyString>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderExternalUsage {
    pub label: TrimmedNonEmptyString,

    pub url: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageUnavailable {
    pub reason: UsageLimitsUnavailableReason,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub message: Option<Option<TrimmedNonEmptyString>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProviderUsageLimits {
    pub checked_at: IsoDateTime,

    pub windows: ForwardCompatibleArray<ServerProviderUsageWindow>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub credential_fingerprint: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reset_credits: Option<Option<ServerProviderResetCredits>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub external_usage: Option<Option<ProviderExternalUsage>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub unavailable: Option<Option<ProviderUsageUnavailable>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageLimitsUpdate {
    pub windows: Vec<ServerProviderUsageWindow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProvider {
    pub instance_id: ProviderInstanceId,

    pub driver: ProviderDriverKind,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub display_name: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub accent_color: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub badge_label: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub icon_url: Option<Option<BoundedTrimmedString<2048>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub continuation: Option<Option<ServerProviderContinuation>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub show_interaction_mode_toggle: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reports_context_window: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub requires_new_thread_for_model_change: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub supports_conversation_rollback: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub supports_text_generation: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub configurable_providers: Option<Option<bool>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub supported_runtime_modes: Option<Option<ForwardCompatibleArray<RuntimeMode>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub setup: Option<Option<ServerProviderSetup>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub native_sessions: Option<Option<ServerProviderNativeSessions>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub runtime_paths: Option<ServerProviderRuntimePaths>,

    pub enabled: bool,

    pub installed: bool,

    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub version: Option<TrimmedNonEmptyString>,

    pub status: ServerProviderState,

    pub auth: ServerProviderAuth,

    pub checked_at: IsoDateTime,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub message: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub availability: Option<Option<ServerProviderAvailability>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub unavailable_reason: Option<Option<TrimmedNonEmptyString>>,

    pub models: Vec<ServerProviderModel>,

    #[serde(default, deserialize_with = "crate::provider::deserialize_default_vec")]
    pub slash_commands: Vec<ServerProviderSlashCommand>,

    #[serde(default, deserialize_with = "crate::provider::deserialize_default_vec")]
    pub skills: Vec<ServerProviderSkill>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub workspace_snapshots: Option<Vec<ServerProviderWorkspaceSnapshot>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_limits: Option<Option<ServerProviderUsageLimits>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub version_advisory: Option<ServerProviderVersionAdvisory>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub compatibility_advisory: Option<ServerProviderCompatibilityAdvisory>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub update_state: Option<ServerProviderUpdateState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerObservability {
    pub logs_directory_path: TrimmedNonEmptyString,

    pub local_tracing_enabled: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub otlp_traces_url: Option<Option<TrimmedNonEmptyString>>,

    pub otlp_traces_enabled: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub otlp_metrics_url: Option<Option<TrimmedNonEmptyString>>,

    pub otlp_metrics_enabled: bool,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub otlp_logs_url: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        deserialize_with = "crate::provider::deserialize_default_bool"
    )]
    pub otlp_logs_enabled: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ServerInstallation {
    #[serde(rename = "npx")]
    Npx,
    #[serde(rename = "pnpm-dlx")]
    PnpmDlx,
    #[serde(rename = "bunx")]
    Bunx,
    #[serde(rename = "npm-global")]
    NpmGlobal { prefix: TrimmedNonEmptyString },
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct UsagePercent(pub serde_json::Number);
impl<'de> Deserialize<'de> for UsagePercent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let number = serde_json::Number::deserialize(d)?;
        if number.as_f64().is_some_and(|n| (0.0..=100.0).contains(&n)) {
            Ok(Self(number))
        } else {
            Err(serde::de::Error::custom(
                "expected usage between zero and 100 percent",
            ))
        }
    }
}
impl ServerProvider {
    pub fn is_available(&self) -> bool {
        self.availability.flatten() != Some(ServerProviderAvailability::Unavailable)
    }
    pub fn is_text_generation_capable(&self) -> bool {
        self.supports_text_generation.flatten() != Some(false)
    }
}
pub const PROVIDER_WORKSPACE_SNAPSHOT_TTL_MS: i64 = 300_000;
pub fn is_provider_workspace_snapshot_current(
    snapshot: &ServerProviderWorkspaceSnapshot,
    now_ms: i64,
) -> bool {
    serde_json::from_value::<UtcDateTime>(serde_json::Value::String(snapshot.checked_at.clone()))
        .is_ok_and(|time| now_ms - time.timestamp_millis() < PROVIDER_WORKSPACE_SNAPSHOT_TTL_MS)
}
impl RepositoryIdentity {
    pub fn grouping_key(&self) -> &str {
        self.origin
            .as_ref()
            .map_or(self.canonical_key.as_str(), |origin| {
                origin.canonical_key.as_str()
            })
    }
    pub fn grouping_display_name(&self) -> Option<&str> {
        self.origin
            .as_ref()
            .map(|origin| {
                origin
                    .display_name
                    .as_ref()
                    .unwrap_or(&origin.canonical_key)
                    .as_str()
            })
            .or_else(|| {
                self.display_name
                    .as_ref()
                    .map(TrimmedNonEmptyString::as_str)
            })
    }
}
