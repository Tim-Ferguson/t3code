//! Permission declarations ported from clientRpcPermissions.ts and server auth/RpcAuthorization.ts.
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CLIENT_GUARDED_RPC_SCOPES: &[(&str, AuthEnvironmentScope)] = &[
    (
        "pullRequests.runAction",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.update",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.comment",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.updateComment",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.submitReview",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.replyToThread",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.setThreadResolution",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.setReaction",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.setFilesViewed",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.requestReviewers",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.setLabels",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "sourceControl.cloneRepository",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "sourceControl.publishRepository",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "projectClone.start",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "projectClone.cancel",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "projectClone.retry",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    ("vcs.pull", AuthEnvironmentScope::SourceControlWrite),
    (
        "git.runStackedAction",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "git.preparePullRequestThread",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "vcs.createWorktree",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "vcs.removeWorktree",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    ("vcs.createRef", AuthEnvironmentScope::SourceControlWrite),
    ("vcs.switchRef", AuthEnvironmentScope::SourceControlWrite),
    ("vcs.init", AuthEnvironmentScope::SourceControlWrite),
    (
        "scheduledTasks.upsert",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "scheduledTasks.setEnabled",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "scheduledTasks.delete",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "scheduledTasks.runNow",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "scheduledTasks.rotateWebhookToken",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
];
pub const RPC_REQUIRED_SCOPES: &[(&str, AuthEnvironmentScope)] = &[
    (
        "pullRequests.runAction",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.update",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.comment",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.updateComment",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.submitReview",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.replyToThread",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.setThreadResolution",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.setReaction",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.setFilesViewed",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.requestReviewers",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "pullRequests.setLabels",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "sourceControl.cloneRepository",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "sourceControl.publishRepository",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "projectClone.start",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "projectClone.cancel",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "projectClone.retry",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    ("vcs.pull", AuthEnvironmentScope::SourceControlWrite),
    (
        "git.runStackedAction",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "git.preparePullRequestThread",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "vcs.createWorktree",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    (
        "vcs.removeWorktree",
        AuthEnvironmentScope::SourceControlWrite,
    ),
    ("vcs.createRef", AuthEnvironmentScope::SourceControlWrite),
    ("vcs.switchRef", AuthEnvironmentScope::SourceControlWrite),
    ("vcs.init", AuthEnvironmentScope::SourceControlWrite),
    (
        "scheduledTasks.upsert",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "scheduledTasks.setEnabled",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "scheduledTasks.delete",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "scheduledTasks.runNow",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "scheduledTasks.rotateWebhookToken",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "orchestration.dispatchCommand",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "orchestration.getWorkflowScript",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "orchestration.getTurnDiff",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "orchestration.getFullThreadDiff",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "orchestration.searchThreads",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "orchestration.getArchivedShellSnapshot",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "orchestration.getThreadProjection",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "orchestration.getTurnItem",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "orchestration.launchThread",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "orchestration.subscribeArchivedShell",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "orchestration.subscribeShell",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "orchestration.subscribeThread",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "projects.mutate",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    ("server.probe", AuthEnvironmentScope::OrchestrationRead),
    ("server.getConfig", AuthEnvironmentScope::OrchestrationRead),
    (
        "server.refreshProviders",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "server.updateProvider",
        AuthEnvironmentScope::ProvidersManage,
    ),
    ("provider.auth.start", AuthEnvironmentScope::ProvidersManage),
    (
        "provider.consumeResetCredit",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.auth.complete",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.chatgpt.reconnect-profile",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.chatgpt.import-profile",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.chatgpt.handoff.subscribe",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.codex.auth-callback.subscribe",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.auth.respond",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.auth.cancel",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.auth.logout",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.auth.subscribe",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.install.start",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.install.cancel",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "provider.install.subscribe",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "provider.install.remove",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "server.updateServer",
        AuthEnvironmentScope::EnvironmentMaintain,
    ),
    (
        "server.updateServerWithProgress",
        AuthEnvironmentScope::EnvironmentMaintain,
    ),
    (
        "server.commitDesktopUpdate",
        AuthEnvironmentScope::EnvironmentMaintain,
    ),
    (
        "server.upsertKeybinding",
        AuthEnvironmentScope::SettingsWrite,
    ),
    (
        "server.removeKeybinding",
        AuthEnvironmentScope::SettingsWrite,
    ),
    (
        "server.getSettings",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    ("server.updateSettings", AuthEnvironmentScope::SettingsWrite),
    (
        "server.searchAcpRegistry",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "server.prepareAcpRegistryAgent",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "server.uninstallAcpRegistryManagedBinary",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "server.acceptAcpRegistryUrlAuth",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "server.listAcpRegistrySessions",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "server.importAcpRegistrySession",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "server.deleteAcpRegistrySession",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "server.listAcpRegistryProviders",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "server.setAcpRegistryProvider",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "server.disableAcpRegistryProvider",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "server.logoutAcpRegistry",
        AuthEnvironmentScope::ProvidersManage,
    ),
    (
        "server.discoverSourceControl",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "server.getTraceDiagnostics",
        AuthEnvironmentScope::DiagnosticsRead,
    ),
    (
        "server.getProcessDiagnostics",
        AuthEnvironmentScope::DiagnosticsRead,
    ),
    (
        "server.getHostResources",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "server.getProcessResourceHistory",
        AuthEnvironmentScope::DiagnosticsRead,
    ),
    (
        "server.getResourceTelemetryHistory",
        AuthEnvironmentScope::DiagnosticsRead,
    ),
    (
        "server.retryResourceTelemetry",
        AuthEnvironmentScope::DiagnosticsRead,
    ),
    (
        "server.getUsageSummary",
        AuthEnvironmentScope::DiagnosticsRead,
    ),
    (
        "server.refreshUsageRates",
        AuthEnvironmentScope::DiagnosticsRead,
    ),
    (
        "server.signalProcess",
        AuthEnvironmentScope::EnvironmentMaintain,
    ),
    (
        "server.reportClientActivity",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "server.reportHostPowerState",
        AuthEnvironmentScope::EnvironmentMaintain,
    ),
    (
        "server.getBackgroundPolicy",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "scheduledTasks.list",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "scheduledTasks.subscribe",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "secrets.answerRequest",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "scheduledTasks.listWebhookDeliveries",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "scheduledTasks.getWebhookDelivery",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "cloud.getRelayClientStatus",
        AuthEnvironmentScope::RelayRead,
    ),
    ("cloud.installRelayClient", AuthEnvironmentScope::RelayWrite),
    ("pullRequests.list", AuthEnvironmentScope::OrchestrationRead),
    (
        "pullRequests.listStats",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.summary",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.routing",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.routingIdentity",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.stack",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.linkedThreads",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.detail",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.preview",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.checks",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.activity",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.threadComments",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.diffFileContents",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.filesViewed",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.invalidate",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.subscribeRefreshes",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.reviewerCandidates",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "pullRequests.labelCandidates",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "sourceControl.lookupRepository",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "subscribeProjectClones",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    ("projects.listEntries", AuthEnvironmentScope::FilesystemRead),
    ("projects.readFile", AuthEnvironmentScope::FilesystemRead),
    (
        "projects.searchContents",
        AuthEnvironmentScope::FilesystemRead,
    ),
    (
        "projects.searchEntries",
        AuthEnvironmentScope::FilesystemRead,
    ),
    ("projects.writeFile", AuthEnvironmentScope::FilesystemWrite),
    (
        "projects.ensureScratch",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "projects.createNew",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "shell.openInEditor",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    ("filesystem.browse", AuthEnvironmentScope::FilesystemRead),
    (
        "agentSessions.scan",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "agentSessions.import",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    ("assets.createUrl", AuthEnvironmentScope::OrchestrationRead),
    (
        "assets.persistChatAttachments",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "attachments.createUploadUrl",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "attachments.delete",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "provider.uploadFeedback",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "mcpApps.callTool",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    ("mcpApps.toolInfo", AuthEnvironmentScope::OrchestrationRead),
    (
        "mcpApps.readResource",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "mcpApps.updateModelContext",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "subscribeVcsStatus",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "subscribeWorktreeSetup",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "worktreeSetup.cancel",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    (
        "subscribeResourceTelemetry",
        AuthEnvironmentScope::DiagnosticsRead,
    ),
    ("vcs.refreshStatus", AuthEnvironmentScope::OrchestrationRead),
    (
        "git.resolvePullRequest",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    ("vcs.listRefs", AuthEnvironmentScope::OrchestrationRead),
    (
        "review.getDiffPreview",
        AuthEnvironmentScope::FilesystemRead,
    ),
    (
        "review.getDiffFileContents",
        AuthEnvironmentScope::FilesystemRead,
    ),
    ("terminal.open", AuthEnvironmentScope::TerminalOperate),
    ("terminal.attach", AuthEnvironmentScope::TerminalOperate),
    ("terminal.observe", AuthEnvironmentScope::TerminalRead),
    ("terminal.write", AuthEnvironmentScope::TerminalOperate),
    ("terminal.resize", AuthEnvironmentScope::TerminalOperate),
    ("terminal.clear", AuthEnvironmentScope::TerminalOperate),
    ("terminal.restart", AuthEnvironmentScope::TerminalOperate),
    ("terminal.close", AuthEnvironmentScope::TerminalOperate),
    (
        "subscribeTerminalEvents",
        AuthEnvironmentScope::TerminalRead,
    ),
    (
        "subscribeTerminalMetadata",
        AuthEnvironmentScope::TerminalRead,
    ),
    ("preview.open", AuthEnvironmentScope::PreviewOperate),
    ("preview.navigate", AuthEnvironmentScope::PreviewOperate),
    ("preview.resize", AuthEnvironmentScope::PreviewOperate),
    ("preview.adjust", AuthEnvironmentScope::PreviewOperate),
    ("preview.refresh", AuthEnvironmentScope::PreviewOperate),
    ("preview.close", AuthEnvironmentScope::PreviewOperate),
    ("preview.list", AuthEnvironmentScope::OrchestrationRead),
    ("preview.clearProfile", AuthEnvironmentScope::PreviewOperate),
    ("preview.reportStatus", AuthEnvironmentScope::PreviewOperate),
    (
        "subscribePreviewEvents",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "subscribeDiscoveredLocalServers",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    ("device.configure", AuthEnvironmentScope::SettingsWrite),
    ("device.testHost", AuthEnvironmentScope::SettingsWrite),
    ("device.list", AuthEnvironmentScope::OrchestrationRead),
    ("device.open", AuthEnvironmentScope::OrchestrationOperate),
    ("device.close", AuthEnvironmentScope::OrchestrationOperate),
    (
        "device.shutdown",
        AuthEnvironmentScope::OrchestrationOperate,
    ),
    ("device.detail", AuthEnvironmentScope::OrchestrationRead),
    ("device.action", AuthEnvironmentScope::OrchestrationOperate),
    (
        "subscribeDeviceState",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "subscribeServerConfig",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    (
        "subscribeServerLifecycle",
        AuthEnvironmentScope::OrchestrationRead,
    ),
    ("subscribeAuthAccess", AuthEnvironmentScope::AccessRead),
    (
        "subscribeBackgroundPolicy",
        AuthEnvironmentScope::OrchestrationRead,
    ),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitPreparePullRequestThreadMode {
    Local,
    Worktree,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitPreparePullRequestThreadInput {
    pub cwd: TrimmedNonEmptyString,
    pub reference: TrimmedNonEmptyString,
    pub mode: GitPreparePullRequestThreadMode,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_id: Option<ThreadId>,
}
/// Mirrors the source's incremental client enforcement. Server authorization
/// remains authoritative and uses every declared RPC method's required scope.
pub fn client_rpc_required_scopes(
    method: &str,
    input: Option<&Value>,
) -> Result<Vec<AuthEnvironmentScope>, serde_json::Error> {
    if method == "git.preparePullRequestThread" {
        if let Some(input) = input {
            let input: GitPreparePullRequestThreadInput = serde_json::from_value(input.clone())?;
            if input.mode == GitPreparePullRequestThreadMode::Worktree && input.thread_id.is_some()
            {
                return Ok(vec![
                    AuthEnvironmentScope::SourceControlWrite,
                    AuthEnvironmentScope::OrchestrationOperate,
                ]);
            }
        }
    }
    Ok(CLIENT_GUARDED_RPC_SCOPES
        .iter()
        .find(|(tag, _)| *tag == method)
        .map(|(_, scope)| vec![*scope])
        .unwrap_or_default())
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("RPC method {0} has no declared authorization scope.")]
pub struct UndeclaredRpcScope(pub String);
pub fn rpc_required_scope(method: &str) -> Result<AuthEnvironmentScope, UndeclaredRpcScope> {
    RPC_REQUIRED_SCOPES
        .iter()
        .find(|(tag, _)| *tag == method)
        .map(|(_, scope)| *scope)
        .ok_or_else(|| UndeclaredRpcScope(method.to_owned()))
}
