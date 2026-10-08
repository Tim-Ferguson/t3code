//! Durable timeline items, plans, checkpoints and context transfers.
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanStepStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "completed")]
    Completed,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub id: TrimmedNonEmptyString,
    pub text: TrimmedNonEmptyString,
    pub status: PlanStepStatus,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub duration_anchor_at: Option<Option<IsoDateTime>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub duration_ms: Option<Option<NonNegativeInt>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestionOption {
    pub label: TrimmedNonEmptyString,
    pub description: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub value: Option<Option<String>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestionV2 {
    pub id: TrimmedNonEmptyString,
    pub header: TrimmedNonEmptyString,
    pub question: TrimmedNonEmptyString,
    pub options: Vec<UserInputQuestionOption>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub multi_select: Option<Option<bool>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub allow_custom_answer: Option<Option<bool>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub required: Option<Option<bool>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanArtifactStatus {
    #[serde(rename = "draft")]
    Draft,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "superseded")]
    Superseded,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanArtifactBase {
    pub id: PlanId,
    pub thread_id: ThreadId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub run_id: Option<RunId>,
    pub node_id: NodeId,
    pub status: PlanArtifactStatus,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub detail_in_turn_item: Option<Option<LiteralBool<true>>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanMarkdown {
    pub markdown: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTodoList {
    pub steps: Vec<PlanStep>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub explanation: Option<Option<String>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum PlanArtifactKind {
    #[serde(rename = "proposed_plan")]
    ProposedPlan(PlanMarkdown),
    #[serde(rename = "todo_list")]
    TodoList(PlanTodoList),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanArtifact {
    #[serde(flatten)]
    pub base: PlanArtifactBase,
    #[serde(flatten)]
    pub kind: PlanArtifactKind,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointFileSummary {
    pub path: TrimmedNonEmptyString,
    pub kind: TrimmedNonEmptyString,
    pub additions: NonNegativeInt,
    pub deletions: NonNegativeInt,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointStatus {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "missing")]
    Missing,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "stale")]
    Stale,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub thread_id: ThreadId,
    pub scope_id: CheckpointScopeId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub run_id: Option<RunId>,
    pub node_id: NodeId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub parent_checkpoint_id: Option<CheckpointId>,
    pub ordinal_within_scope: NonNegativeInt,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub app_run_ordinal: Option<PositiveInt>,
    pub r#ref: CheckpointRef,
    pub status: CheckpointStatus,
    pub files: Vec<CheckpointFileSummary>,
    pub captured_at: UtcDateTime,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointRollbackRequest {
    pub scope_id: CheckpointScopeId,
    pub checkpoint_id: CheckpointId,
    pub requested_at: UtcDateTime,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolActivitySurface {
    #[serde(rename = "browser")]
    Browser,
    #[serde(rename = "computer")]
    Computer,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolActivitySourceKind {
    #[serde(rename = "browser")]
    Browser,
    #[serde(rename = "computer")]
    Computer,
    #[serde(rename = "integration")]
    Integration,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolNativeAppId {
    pub app_id: ToolActivityAppId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolNativeAppDisplayName {
    pub display_name: BoundedTrimmedString<160>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum ToolActivityNativeAppReference {
    #[serde(rename = "app-id")]
    AppId(ToolNativeAppId),
    #[serde(rename = "display-name")]
    DisplayName(ToolNativeAppDisplayName),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolWebsiteIcon {
    pub page_url: BoundedTrimmedString<4096>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub favicon_url: Option<Option<BoundedTrimmedString<4096>>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub favicon_url_dark: Option<Option<BoundedTrimmedString<4096>>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolNativeAppIcon {
    pub app: ToolActivityNativeAppReference,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolThemedLogoIcon {
    pub logo_url: BoundedTrimmedString<4096>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub logo_url_dark: Option<Option<BoundedTrimmedString<4096>>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum ToolActivityIcon {
    #[serde(rename = "website")]
    Website(ToolWebsiteIcon),
    #[serde(rename = "native-app")]
    NativeApp(ToolNativeAppIcon),
    #[serde(rename = "themed-logo")]
    ThemedLogo(ToolThemedLogoIcon),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolActivitySource {
    pub key: BoundedTrimmedString<512>,
    pub name: BoundedTrimmedString<160>,
    pub kind: ToolActivitySourceKind,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub icon: Option<Option<ToolActivityIcon>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnItemStatus {
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "waiting")]
    Waiting,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "interrupted")]
    Interrupted,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnItemBase {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub tool_non_execution_kind: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub tool_surface: Option<Option<ToolActivitySurface>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub tool_icon: Option<Option<ToolActivityIcon>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub tool_source: Option<Option<ToolActivitySource>>,
    pub id: TurnItemId,
    pub thread_id: ThreadId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub run_id: Option<RunId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub node_id: Option<NodeId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_thread_id: Option<ProviderThreadId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_turn_id: Option<ProviderTurnId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub native_item_ref: Option<ProviderRef>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub parent_item_id: Option<TurnItemId>,
    pub ordinal: NonNegativeInt,
    pub status: TurnItemStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub title: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub started_at: Option<UtcDateTime>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub completed_at: Option<UtcDateTime>,
    pub updated_at: UtcDateTime,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserMessageInputIntent {
    #[serde(rename = "turn_start")]
    TurnStart,
    #[serde(rename = "queued_turn")]
    QueuedTurn,
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "promoted_queued_to_steer")]
    PromotedQueuedToSteer,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessageItem {
    pub created_by: Actor,
    pub creation_source: CreationSource,
    pub message_id: MessageId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub scheduled_task_id: Option<Option<ScheduledTaskId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sender_thread_id: Option<Option<ThreadId>>,
    pub input_intent: UserMessageInputIntent,
    pub text: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub context: Option<Option<OrchestrationMessageContext>>,
    pub attachments: Vec<ChatAttachment>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessageItem {
    pub message_id: MessageId,
    pub text: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub attachments: Option<Option<Vec<ChatAttachment>>>,
    pub streaming: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningItem {
    pub text: String,
    pub streaming: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedPlanItem {
    pub plan_id: PlanId,
    pub markdown: String,
    pub streaming: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoListItem {
    pub plan_id: PlanId,
    pub steps: Vec<PlanStep>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub explanation: Option<Option<String>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageResponseMode {
    #[serde(rename = "message")]
    Message,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputAttachmentAnswerPayload {
    pub request_id: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub question_text_by_id: Option<Option<BTreeMap<String, String>>>,
    pub answers: ProviderUserInputAnswers,
    pub attachments_by_question_id: UserInputAttachments,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputRequestItem {
    pub request_id: RuntimeRequestId,
    pub questions: Vec<UserInputQuestionV2>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub question_answer: Option<Option<UserInputAttachmentAnswerPayload>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub response_mode: Option<Option<MessageResponseMode>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeDetail {
    pub operation: TrimmedNonEmptyString,
    pub path: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub old_path: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub file_type: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub mime_type: Option<Option<TrimmedNonEmptyString>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeItem {
    pub file_name: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub additions: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub deletions: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub diff_str: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub old_str: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub new_str: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub changes: Option<Option<Vec<FileChangeDetail>>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionItem {
    pub input: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub output: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub output_omitted: Option<Option<bool>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub output_indicates_failure: Option<Option<bool>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub exit_code: Option<Option<SafeInt>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchResult {
    pub file_name: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub line: Option<Option<PositiveInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub column: Option<Option<PositiveInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub preview: Option<Option<String>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchItem {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pattern: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub results: Option<Option<Vec<FileSearchResult>>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResult {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub title: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub url: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snippet: Option<Option<String>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchItem {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub patterns: Option<Option<Vec<String>>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub results: Option<Option<Vec<WebSearchResult>>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderRequestKind {
    #[serde(rename = "command")]
    Command,
    #[serde(rename = "file-read")]
    FileRead,
    #[serde(rename = "file-change")]
    FileChange,
    #[serde(rename = "mcp-elicitation")]
    McpElicitation,
    #[serde(rename = "permission")]
    Permission,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderApprovalOption {
    pub decision: ProviderApprovalDecision,
    pub label: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub warning: Option<Option<TrimmedNonEmptyString>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequestItem {
    pub request_id: RuntimeRequestId,
    pub request_kind: ProviderRequestKind,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub prompt: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub app_name: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub options: Option<Option<Vec<ProviderApprovalOption>>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointItem {
    pub checkpoint_id: CheckpointId,
    pub scope_id: CheckpointScopeId,
    pub files: Vec<CheckpointFileSummary>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageNoticeItem {
    pub message: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorItem {
    pub failure: ProviderFailure,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub retry: Option<Option<ProviderRetry>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionItem {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub driver: Option<ProviderDriverKind>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub summary: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub before_token_count: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub after_token_count: Option<Option<NonNegativeInt>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextHandoffStrategy {
    #[serde(rename = "delta_since_target_last_seen")]
    DeltaSinceTargetLastSeen,
    #[serde(rename = "fork_delta_summary")]
    ForkDeltaSummary,
    #[serde(rename = "full_thread_summary")]
    FullThreadSummary,
    #[serde(rename = "checkpoint_summary")]
    CheckpointSummary,
    #[serde(rename = "manual_context")]
    ManualContext,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffItem {
    pub context_handoff_id: ContextHandoffId,
    pub from_provider_thread_ids: Vec<ProviderThreadId>,
    pub to_provider_thread_id: ProviderThreadId,
    pub from_provider_instance_ids: Vec<ProviderInstanceId>,
    pub to_provider_instance_id: ProviderInstanceId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub from_model_selections: Option<Option<Vec<ModelSelection>>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub to_model: Option<Option<String>>,
    pub strategy: ContextHandoffStrategy,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub summary: Option<Option<String>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkItem {
    pub source: ThreadForkSource,
    pub target_thread_id: ThreadId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub provider_thread_id: Option<Option<ProviderThreadId>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCreatedItem {
    pub target_thread_id: ThreadId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub target_run_id: Option<RunId>,
    pub target_provider_instance_id: ProviderInstanceId,
    pub target_model: TrimmedNonEmptyString,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretRequestStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "saved")]
    Saved,
    #[serde(rename = "declined")]
    Declined,
    #[serde(rename = "cancelled")]
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretRequestItem {
    pub label: TrimmedNonEmptyString,
    pub reason: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub placeholder: Option<Option<String>>,
    pub secret_status: SecretRequestStatus,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentItem {
    pub subagent_id: NodeId,
    pub origin: SubagentOrigin,
    pub driver: ProviderDriverKind,
    pub provider_instance_id: ProviderInstanceId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub child_thread_id: Option<ThreadId>,
    pub prompt: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub progress: Option<Option<String>>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub result: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolItem {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub tool_name: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub viewed_image_path: Option<Option<TrimmedNonEmptyString>>,
    pub input: serde_json::Value,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub output: Option<serde_json::Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub output_omitted: Option<Option<bool>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TurnItemKind {
    #[serde(rename = "notification")]
    Notification(Notification),
    #[serde(rename = "user_message")]
    UserMessage(UserMessageItem),
    #[serde(rename = "assistant_message")]
    AssistantMessage(AssistantMessageItem),
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningItem),
    #[serde(rename = "proposed_plan")]
    ProposedPlan(ProposedPlanItem),
    #[serde(rename = "todo_list")]
    TodoList(TodoListItem),
    #[serde(rename = "user_input_request")]
    UserInputRequest(UserInputRequestItem),
    #[serde(rename = "file_change")]
    FileChange(FileChangeItem),
    #[serde(rename = "command_execution")]
    CommandExecution(CommandExecutionItem),
    #[serde(rename = "file_search")]
    FileSearch(FileSearchItem),
    #[serde(rename = "web_search")]
    WebSearch(WebSearchItem),
    #[serde(rename = "approval_request")]
    ApprovalRequest(ApprovalRequestItem),
    #[serde(rename = "checkpoint")]
    Checkpoint(CheckpointItem),
    #[serde(rename = "run_interrupt_request")]
    RunInterruptRequest(MessageNoticeItem),
    #[serde(rename = "run_interrupt_result")]
    RunInterruptResult(MessageNoticeItem),
    #[serde(rename = "system_notice")]
    SystemNotice(MessageNoticeItem),
    #[serde(rename = "error")]
    Error(ErrorItem),
    #[serde(rename = "compaction")]
    Compaction(CompactionItem),
    #[serde(rename = "handoff")]
    Handoff(HandoffItem),
    #[serde(rename = "fork")]
    Fork(ForkItem),
    #[serde(rename = "thread_created")]
    ThreadCreated(ThreadCreatedItem),
    #[serde(rename = "secret_request")]
    SecretRequest(SecretRequestItem),
    #[serde(rename = "subagent")]
    Subagent(SubagentItem),
    #[serde(rename = "dynamic_tool")]
    DynamicTool(DynamicToolItem),
}
#[derive(Debug, Clone, PartialEq)]
pub struct TurnItem {
    pub base: TurnItemBase,
    pub kind: TurnItemKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectedTurnItemVisibility {
    #[serde(rename = "local")]
    Local,
    #[serde(rename = "inherited")]
    Inherited,
    #[serde(rename = "synthetic")]
    Synthetic,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedTurnItem {
    pub position: NonNegativeInt,
    pub visibility: ProjectedTurnItemVisibility,
    pub source_thread_id: ThreadId,
    pub source_item_id: TurnItemId,
    pub item: TurnItem,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoricalMessage {
    pub role: HistoricalMessageRole,
    pub text: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub run_status: Option<Option<String>>,
    pub thread_id: ThreadId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub run_id: Option<RunId>,
    pub item_id: TurnItemId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_thread_id: Option<ProviderThreadId>,
    pub status: String,
    pub kind: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoricalMessageRole {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoveredRunOrdinals {
    pub from: PositiveInt,
    pub to: PositiveInt,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextHandoffHistory {
    pub messages: Vec<HistoricalMessage>,
    pub coverage: String,
    pub omitted_items: NonNegativeInt,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub omitted_item_ids: Option<Option<Vec<TurnItemId>>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextHandoffDeliveryStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "injected")]
    Injected,
    #[serde(rename = "inline")]
    Inline,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextHandoffDelivery {
    pub native_thread_id: String,
    pub status: ContextHandoffDeliveryStatus,
    pub item_ids: Vec<TurnItemId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub omitted_item_ids: Option<Option<Vec<TurnItemId>>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextHandoffStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "superseded")]
    Superseded,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextHandoff {
    pub id: ContextHandoffId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub transfer_id: Option<Option<ContextTransferId>>,
    pub thread_id: ThreadId,
    pub target_run_id: RunId,
    pub from_provider_thread_ids: Vec<ProviderThreadId>,
    pub to_provider_thread_id: ProviderThreadId,
    pub covered_run_ordinals: CoveredRunOrdinals,
    pub strategy: ContextHandoffStrategy,
    pub status: ContextHandoffStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub summary_message_id: Option<MessageId>,
    pub summary_text: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub history: Option<Option<ContextHandoffHistory>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub delivery: Option<Option<ContextHandoffDelivery>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub detail_in_turn_item: Option<Option<LiteralBool<true>>>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub created_by_provider_instance_id: Option<ProviderInstanceId>,
    pub created_at: UtcDateTime,
    pub updated_at: UtcDateTime,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextTransferType {
    #[serde(rename = "fork")]
    Fork,
    #[serde(rename = "provider_handoff")]
    ProviderHandoff,
    #[serde(rename = "merge_back")]
    MergeBack,
    #[serde(rename = "subagent_spawn")]
    SubagentSpawn,
    #[serde(rename = "subagent_result")]
    SubagentResult,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSourcePoint {
    pub thread_id: ThreadId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub run_id: Option<Option<RunId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub checkpoint_id: Option<Option<CheckpointId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub turn_item_id: Option<Option<TurnItemId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub provider_thread_ref: Option<Option<ProviderRef>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub provider_turn_ref: Option<Option<ProviderRef>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeForkResolution {
    pub provider_thread_ref: ProviderRef,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextHandoffResolution {
    pub context_handoff_id: ContextHandoffId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "strategy")]
pub enum ContextTransferResolution {
    #[serde(rename = "native_fork")]
    NativeFork(NativeForkResolution),
    #[serde(rename = "portable_context")]
    PortableContext(ContextHandoffResolution),
    #[serde(rename = "delta_context")]
    DeltaContext(ContextHandoffResolution),
    #[serde(rename = "fork_delta_context")]
    ForkDeltaContext(ContextHandoffResolution),
    #[serde(rename = "checkpoint_context")]
    CheckpointContext(ContextHandoffResolution),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextTransferStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "resolved_native")]
    ResolvedNative,
    #[serde(rename = "resolved_portable")]
    ResolvedPortable,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "consumed")]
    Consumed,
    #[serde(rename = "superseded")]
    Superseded,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextTransfer {
    pub id: ContextTransferId,
    #[serde(rename = "type")]
    pub transfer_type: ContextTransferType,
    pub source_thread_id: ThreadId,
    pub target_thread_id: ThreadId,
    pub source_point: ContextSourcePoint,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub base_point: Option<ContextSourcePoint>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub source_provider_instance_id: Option<ProviderInstanceId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub target_provider_instance_id: Option<ProviderInstanceId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub target_run_id: Option<RunId>,
    pub status: ContextTransferStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub resolution: Option<ContextTransferResolution>,
    pub created_by: Actor,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub error: Option<String>,
    pub created_at: UtcDateTime,
    pub updated_at: UtcDateTime,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub consumed_at: Option<UtcDateTime>,
}

fn tool_app_id(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty()
        && value.encode_utf16().count() <= 512
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "an app id of at most 512 ASCII letters, digits, dots, underscores or dashes",
        })
    }
}
crate::base::string_type!(ToolActivityAppId, tool_app_id);

impl<'de> Deserialize<'de> for TurnItem {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        let kind: TurnItemKind =
            serde_json::from_value(value.clone()).map_err(serde::de::Error::custom)?;
        let mut common = value;
        if matches!(kind, TurnItemKind::Fork(_)) {
            common
                .as_object_mut()
                .ok_or_else(|| <D::Error as serde::de::Error>::custom("Expected turn item object"))?
                .entry("providerThreadId")
                .or_insert(serde_json::Value::Null);
        }
        let base = serde_json::from_value(common).map_err(serde::de::Error::custom)?;
        Ok(Self { base, kind })
    }
}
impl Serialize for TurnItem {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut common = serde_json::to_value(&self.base).map_err(serde::ser::Error::custom)?;
        let object = common.as_object_mut().unwrap();
        if matches!(self.kind, TurnItemKind::Fork(_)) {
            object.remove("providerThreadId");
        }
        let kind = serde_json::to_value(&self.kind).map_err(serde::ser::Error::custom)?;
        object.extend(kind.as_object().unwrap().clone());
        common.serialize(s)
    }
}
