//! Typed execution entities and provider-facing command boundaries.
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    pub supports_multiple_provider_threads_per_session: bool,
    pub supports_model_switch_in_session: bool,
    pub supports_provider_switching_via_handoff: bool,
    pub supports_runtime_mode_switch_in_session: bool,
    pub pending_requests_survive_restart: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCapabilities {
    pub can_create_empty_thread: bool,
    pub can_read_thread_snapshot: bool,
    pub can_rollback_thread: bool,
    pub can_fork_thread: bool,
    pub can_fork_from_turn: bool,
    pub can_fork_from_subagent_thread: bool,
    pub exposes_native_thread_id: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCapabilities {
    pub exposes_native_turn_id: bool,
    pub emits_turn_started: bool,
    pub emits_turn_completed: bool,
    pub supports_interrupt: bool,
    pub supports_active_steering: bool,
    pub supports_steering_by_interrupt_restart: bool,
    pub supports_queued_messages: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub active_steering_interrupts_tools: Option<Option<bool>>,
    pub terminal_status_quality: NativeRefStrength,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamingCapabilities {
    pub streams_assistant_text: bool,
    pub streams_reasoning: bool,
    pub streams_tool_output: bool,
    pub streams_plan_text: bool,
    pub emits_message_completed: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCapabilities {
    pub exposes_tool_item_ids: bool,
    pub emits_tool_started: bool,
    pub emits_tool_completed: bool,
    pub emits_tool_output: bool,
    pub supports_mcp_tools: bool,
    pub supports_dynamic_tool_callbacks: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalCapabilities {
    pub supports_command_approval: bool,
    pub supports_file_read_approval: bool,
    pub supports_file_change_approval: bool,
    pub supports_apply_patch_approval: bool,
    pub approvals_have_native_request_ids: bool,
    pub approval_callbacks_are_live_only: bool,
    pub approvals_can_originate_from_subagents: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanningCapabilities {
    pub emits_plan_updated: bool,
    pub emits_todo_list: bool,
    pub emits_proposed_plan: bool,
    pub supports_structured_questions: bool,
    pub plan_deltas_have_item_ids: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentCapabilities {
    pub supports_subagents: bool,
    pub exposes_subagent_thread_ids: bool,
    pub emits_subagent_lifecycle: bool,
    pub can_wait_for_subagents: bool,
    pub can_close_subagents: bool,
    pub can_fork_subagent_thread: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCapabilities {
    pub accepts_system_context: bool,
    pub accepts_developer_context: bool,
    pub accepts_synthetic_user_context: bool,
    pub can_generate_summaries: bool,
    pub can_consume_handoff_summaries: bool,
    pub supports_delta_handoff: bool,
    pub supports_full_thread_handoff: bool,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub max_recommended_handoff_chars: Option<PositiveInt>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointCapabilities {
    pub app_can_checkpoint_filesystem: bool,
    pub supports_nested_checkpoint_scopes: bool,
    pub provider_can_rollback_conversation: bool,
    pub provider_rollback_returns_snapshot: bool,
    pub provider_can_read_conversation_snapshot: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityCapabilities {
    pub native_thread_ids: NativeRefStrength,
    pub native_turn_ids: NativeRefStrength,
    pub native_item_ids: NativeRefStrength,
    pub native_request_ids: NativeRefStrength,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimePolicyEnforcement {
    #[serde(rename = "native")]
    Native,
    #[serde(rename = "client-boundary")]
    ClientBoundary,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePolicyCapabilities {
    pub enforcement: RuntimePolicyEnforcement,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    pub sessions: SessionCapabilities,
    pub threads: ThreadCapabilities,
    pub turns: TurnCapabilities,
    pub streaming: StreamingCapabilities,
    pub tools: ToolCapabilities,
    pub approvals: ApprovalCapabilities,
    pub planning: PlanningCapabilities,
    pub subagents: SubagentCapabilities,
    pub context: ContextCapabilities,
    pub checkpointing: CheckpointCapabilities,
    pub identity: IdentityCapabilities,
    #[serde(default, deserialize_with = "deserialize_runtime_policy_default")]
    pub runtime_policy: RuntimePolicyCapabilities,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    #[serde(rename = "preparing")]
    Preparing,
    #[serde(rename = "queued")]
    Queued,
    #[serde(rename = "starting")]
    Starting,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "waiting")]
    Waiting,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "interrupted")]
    Interrupted,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "rolled_back")]
    RolledBack,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelegatedCompletionTaskDeliveryState {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "claimed")]
    Claimed,
    #[serde(rename = "acknowledged")]
    Acknowledged,
    #[serde(rename = "delivered")]
    Delivered,
    #[serde(rename = "disposed")]
    Disposed,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegatedCompletionTaskDelivery {
    pub state: DelegatedCompletionTaskDeliveryState,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub observed_by_run_id: Option<RunId>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegatedCompletionDelivery {
    pub generation: PositiveInt,
    pub message_id: MessageId,
    pub task_ids: Vec<NodeId>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelegatedCompletionDisposition {
    #[serde(rename = "open")]
    Open,
    #[serde(rename = "stopped")]
    Stopped,
    #[serde(rename = "disposed")]
    Disposed,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegatedCompletionCohort {
    pub disposition: DelegatedCompletionDisposition,
    pub next_generation: PositiveInt,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub delivery: Option<DelegatedCompletionDelivery>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartCancelledBackgroundWorkKind {
    #[serde(rename = "subagent")]
    Subagent,
    #[serde(rename = "shell")]
    Shell,
    #[serde(rename = "monitor")]
    Monitor,
    #[serde(rename = "task")]
    Task,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartCancelledBackgroundWork {
    pub kind: RestartCancelledBackgroundWorkKind,
    pub label: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub id: Option<Option<TrimmedNonEmptyString>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunBackgroundWorkCancelled {
    pub run_id: RunId,
    pub restart_cancelled_background_work: Vec<RestartCancelledBackgroundWork>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePlanRef {
    pub thread_id: ThreadId,
    pub plan_id: PlanId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRoot {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch: Option<Option<TrimmedNonEmptyString>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceExistingWorktree {
    pub worktree_path: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch: Option<Option<TrimmedNonEmptyString>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceWorktree {
    pub base_ref: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub start_from_origin: Option<Option<bool>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ThreadLaunchWorkspaceStrategy {
    #[serde(rename = "root")]
    Root(WorkspaceRoot),
    #[serde(rename = "existing_worktree")]
    ExistingWorktree(WorkspaceExistingWorktree),
    #[serde(rename = "worktree")]
    Worktree(WorkspaceWorktree),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: RunId,
    pub thread_id: ThreadId,
    pub ordinal: PositiveInt,
    pub provider_instance_id: ProviderInstanceId,
    pub model_selection: ModelSelection,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_thread_id: Option<ProviderThreadId>,
    pub user_message_id: MessageId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub root_node_id: Option<NodeId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub active_attempt_id: Option<RunAttemptId>,
    pub status: RunStatus,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub queue_position: Option<Option<PositiveInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub queue_held: Option<Option<bool>>,
    pub requested_at: UtcDateTime,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub started_at: Option<UtcDateTime>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub completed_at: Option<UtcDateTime>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub checkpoint_id: Option<CheckpointId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub context_handoff_id: Option<ContextHandoffId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub restart_continuation_of_run_id: Option<Option<RunId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub work_started_at: Option<Option<UtcDateTime>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub restart_cancelled_background_work: Option<Option<Vec<RestartCancelledBackgroundWork>>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub source_plan_ref: Option<Option<SourcePlanRef>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub delegated_completion: Option<Option<DelegatedCompletionCohort>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub workspace_preparation: Option<Option<ThreadLaunchWorkspaceStrategy>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunAttemptReason {
    #[serde(rename = "initial")]
    Initial,
    #[serde(rename = "steering_restart")]
    SteeringRestart,
    #[serde(rename = "retry")]
    Retry,
    #[serde(rename = "provider_recovery")]
    ProviderRecovery,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunAttemptStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "interrupted")]
    Interrupted,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "superseded")]
    Superseded,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAttempt {
    pub id: RunAttemptId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub native_thread_id: Option<Option<String>>,
    pub run_id: RunId,
    pub attempt_ordinal: PositiveInt,
    pub root_node_id: NodeId,
    pub provider_instance_id: ProviderInstanceId,
    pub provider_thread_id: ProviderThreadId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_turn_id: Option<ProviderTurnId>,
    pub reason: RunAttemptReason,
    pub status: RunAttemptStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub started_at: Option<UtcDateTime>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub completed_at: Option<UtcDateTime>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionNodeKind {
    #[serde(rename = "root_turn")]
    RootTurn,
    #[serde(rename = "assistant_message")]
    AssistantMessage,
    #[serde(rename = "reasoning")]
    Reasoning,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "todo_list")]
    TodoList,
    #[serde(rename = "tool_call")]
    ToolCall,
    #[serde(rename = "approval_request")]
    ApprovalRequest,
    #[serde(rename = "user_input_request")]
    UserInputRequest,
    #[serde(rename = "subagent")]
    Subagent,
    #[serde(rename = "hook")]
    Hook,
    #[serde(rename = "system")]
    System,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionNodeStatus {
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
    #[serde(rename = "interrupted")]
    Interrupted,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "rolled_back")]
    RolledBack,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionNode {
    pub id: NodeId,
    pub thread_id: ThreadId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub run_id: Option<RunId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub parent_node_id: Option<NodeId>,
    pub root_node_id: NodeId,
    pub kind: ExecutionNodeKind,
    pub status: ExecutionNodeStatus,
    pub counts_for_run: bool,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_thread_id: Option<ProviderThreadId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_turn_id: Option<ProviderTurnId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub native_item_ref: Option<ProviderRef>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub runtime_request_id: Option<RuntimeRequestId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub checkpoint_scope_id: Option<CheckpointScopeId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub started_at: Option<UtcDateTime>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub completed_at: Option<UtcDateTime>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubagentOrigin {
    #[serde(rename = "provider_native")]
    ProviderNative,
    #[serde(rename = "app_owned")]
    AppOwned,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionWake {
    #[serde(rename = "always")]
    Always,
    #[serde(rename = "settled_only")]
    SettledOnly,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubagentStatus {
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
pub struct Subagent {
    pub id: NodeId,
    pub thread_id: ThreadId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub run_id: Option<RunId>,
    pub parent_node_id: NodeId,
    pub origin: SubagentOrigin,
    pub created_by: Actor,
    pub driver: ProviderDriverKind,
    pub provider_instance_id: ProviderInstanceId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_thread_id: Option<ProviderThreadId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub child_thread_id: Option<ThreadId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub native_task_ref: Option<ProviderRef>,
    pub prompt: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub title: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub model: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub completion_wake: Option<Option<CompletionWake>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub completion_delivery: Option<Option<DelegatedCompletionTaskDelivery>>,
    pub status: SubagentStatus,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub progress: Option<Option<String>>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub result: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub started_at: Option<UtcDateTime>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub completed_at: Option<UtcDateTime>,
    pub updated_at: UtcDateTime,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointScopeKind {
    #[serde(rename = "root_run")]
    RootRun,
    #[serde(rename = "subagent")]
    Subagent,
    #[serde(rename = "tool")]
    Tool,
    #[serde(rename = "provider_thread")]
    ProviderThread,
    #[serde(rename = "manual")]
    Manual,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointScope {
    pub id: CheckpointScopeId,
    pub thread_id: ThreadId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub run_id: Option<RunId>,
    pub node_id: NodeId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub parent_scope_id: Option<CheckpointScopeId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_thread_id: Option<ProviderThreadId>,
    pub kind: CheckpointScopeKind,
    pub ordinal_within_parent: NonNegativeInt,
    pub advances_app_run_count: bool,
    pub cwd: TrimmedNonEmptyString,
    pub created_at: UtcDateTime,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderSessionV2Status {
    #[serde(rename = "starting")]
    Starting,
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "waiting")]
    Waiting,
    #[serde(rename = "stopped")]
    Stopped,
    #[serde(rename = "error")]
    Error,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSessionV2 {
    pub id: ProviderSessionId,
    pub driver: ProviderDriverKind,
    pub provider_instance_id: ProviderInstanceId,
    pub status: ProviderSessionV2Status,
    pub cwd: TrimmedNonEmptyString,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub model: Option<TrimmedNonEmptyString>,
    pub capabilities: ProviderCapabilities,
    pub created_at: UtcDateTime,
    pub updated_at: UtcDateTime,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub last_error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSessionDetached {
    pub provider_session_id: ProviderSessionId,
    pub detached_at: UtcDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reason: Option<Option<String>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsageCost {
    pub amount: serde_json::Number,
    pub currency: BoundedTrimmedString<32>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsageSnapshot {
    pub used_tokens: NonNegativeInt,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub total_processed_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub input_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cached_input_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub output_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reasoning_output_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub last_used_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub last_input_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub last_cached_input_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub last_output_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub last_reasoning_output_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub tool_uses: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub duration_ms: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub max_tokens: Option<Option<PositiveInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub compacts_automatically: Option<Option<bool>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub auto_compact_threshold: Option<Option<PositiveInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cost: Option<Option<ThreadTokenUsageCost>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderThreadFork {
    pub provider_thread_id: ProviderThreadId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub provider_turn_id: Option<Option<ProviderTurnId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub checkpoint_id: Option<Option<CheckpointId>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderThreadStatus {
    #[serde(rename = "not_loaded")]
    NotLoaded,
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "archived")]
    Archived,
    #[serde(rename = "closed")]
    Closed,
    #[serde(rename = "error")]
    Error,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderThread {
    pub id: ProviderThreadId,
    pub driver: ProviderDriverKind,
    pub provider_instance_id: ProviderInstanceId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_session_id: Option<ProviderSessionId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub app_thread_id: Option<ThreadId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub owner_node_id: Option<NodeId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub native_thread_ref: Option<ProviderRef>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub native_conversation_head_ref: Option<ProviderRef>,
    pub status: ProviderThreadStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub first_run_ordinal: Option<PositiveInt>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub last_run_ordinal: Option<PositiveInt>,
    pub handoff_ids: Vec<ContextHandoffId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub forked_from: Option<ProviderThreadFork>,
    #[serde(default, deserialize_with = "crate::provider::deserialize_default_vec")]
    pub pending_background_tasks: Vec<PendingBackgroundTask>,
    #[serde(default)]
    pub context_usage: Option<ThreadTokenUsageSnapshot>,
    #[serde(default)]
    pub native_metadata: Option<ProviderThreadNativeMetadata>,
    #[serde(default)]
    pub goal: Option<ProviderGoal>,
    pub created_at: UtcDateTime,
    pub updated_at: UtcDateTime,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTurnTokenUsage {
    pub used_tokens: NonNegativeInt,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub max_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub input_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cached_input_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub output_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reasoning_output_tokens: Option<Option<NonNegativeInt>>,
    pub updated_at: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MainAgentUsageScope {
    #[serde(rename = "main_agent")]
    MainAgent,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IncompleteUsageStatus {
    #[serde(rename = "partial")]
    Partial,
    #[serde(rename = "unavailable")]
    Unavailable,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteTurnTokenUsage {
    pub usage_scope: MainAgentUsageScope,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cached_input_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cache_creation_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reasoning_tokens: Option<Option<NonNegativeInt>>,
    pub has_subagents: bool,
    pub input_tokens: NonNegativeInt,
    pub output_tokens: NonNegativeInt,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncompleteTurnTokenUsage {
    pub usage_scope: MainAgentUsageScope,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cached_input_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cache_creation_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reasoning_tokens: Option<Option<NonNegativeInt>>,
    pub has_subagents: bool,
    pub usage_status: IncompleteUsageStatus,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub input_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub output_tokens: Option<Option<NonNegativeInt>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompleteUsageStatus {
    #[serde(rename = "complete")]
    Complete,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteTurnTokenUsageWire {
    pub usage_status: CompleteUsageStatus,
    pub usage_scope: MainAgentUsageScope,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cached_input_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub cache_creation_tokens: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reasoning_tokens: Option<Option<NonNegativeInt>>,
    pub has_subagents: bool,
    pub input_tokens: NonNegativeInt,
    pub output_tokens: NonNegativeInt,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TurnTokenUsage {
    Complete(CompleteTurnTokenUsageWire),
    Incomplete(IncompleteTurnTokenUsage),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderTurnStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "interrupted")]
    Interrupted,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTurn {
    pub id: ProviderTurnId,
    pub provider_thread_id: ProviderThreadId,
    pub node_id: NodeId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub run_attempt_id: Option<RunAttemptId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub native_turn_ref: Option<ProviderRef>,
    pub ordinal: PositiveInt,
    pub status: ProviderTurnStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub started_at: Option<UtcDateTime>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub completed_at: Option<UtcDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub token_usage: Option<Option<ProviderTurnTokenUsage>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub turn_token_usage: Option<Option<TurnTokenUsage>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeResponseLive {
    pub provider_session_id: ProviderSessionId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeResponseNotResumable {
    pub reason: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RuntimeResponseCapability {
    #[serde(rename = "live")]
    Live(RuntimeResponseLive),
    #[serde(rename = "message")]
    Message,
    #[serde(rename = "not_resumable")]
    NotResumable(RuntimeResponseNotResumable),
}
pub type ProviderUserInputAnswers = BTreeMap<String, serde_json::Value>;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserInputAttachment {
    Image(ChatImageAttachment),
    File(ChatFileAttachment),
}
pub type UserInputAttachments = BTreeMap<String, BoundedVec<UserInputAttachment, 100>>;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderFailure {
    pub class: ProviderFailureClass,
    pub message: BoundedTrimmedString<4096>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub code: Option<BoundedTrimmedString<128>>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub retryable: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reset_at: Option<Option<IsoDateTime>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRetry {
    pub attempt: PositiveInt,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub max_attempts: Option<PositiveInt>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub retry_delay_ms: Option<NonNegativeInt>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitialMessage {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub message_id: Option<Option<MessageId>>,
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
pub struct ThreadLaunchInput {
    pub command_id: CommandId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub creation_source: Option<Option<CreationSource>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_id: Option<Option<ThreadId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reuse_existing_thread: Option<Option<bool>>,
    pub project_id: ProjectId,
    pub title: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub generate_title: Option<Option<bool>>,
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: ProviderInteractionMode,
    pub workspace_strategy: ThreadLaunchWorkspaceStrategy,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub initial_message: Option<Option<InitialMessage>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadLaunchResult {
    pub thread_id: ThreadId,
    pub projection: ThreadProjection,
    pub resumed: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegatedCompletionMessage {
    pub parent_run_id: RunId,
    pub generation: PositiveInt,
    pub task_ids: Vec<NodeId>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryIntent {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "restart")]
    Restart,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchDeferStart {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub workspace_strategy: Option<Option<ThreadLaunchWorkspaceStrategy>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchTargetRun {
    pub target_run_id: RunId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DispatchMode {
    #[serde(rename = "defer_start")]
    DeferStart(DispatchDeferStart),
    #[serde(rename = "steer_active")]
    SteerActive(DispatchTargetRun),
    #[serde(rename = "restart_active")]
    RestartActive(DispatchTargetRun),
    #[serde(rename = "queue_after_active")]
    QueueAfterActive,
    #[serde(rename = "start_immediately")]
    StartImmediately,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDispatchCommand {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub notification: Option<Option<Notification>>,
    pub created_by: Actor,
    pub creation_source: CreationSource,
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
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub message_id: MessageId,
    pub text: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub context: Option<Option<OrchestrationMessageContext>>,
    pub attachments: Vec<ChatAttachment>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub title_seed: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub model_selection: Option<Option<ModelSelection>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub source_plan_ref: Option<Option<SourcePlanRef>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub restart_continuation_of_run_id: Option<Option<RunId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_limit_continuation_of_run_id: Option<Option<RunId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub manual_continuation_of_run_id: Option<Option<RunId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_limit_recovery_request_id: Option<Option<CommandId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub delivery_intent: Option<Option<DeliveryIntent>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub delegated_completion: Option<Option<DelegatedCompletionMessage>>,
    pub dispatch_mode: DispatchMode,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunIdentityCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub run_id: RunId,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreparationPhase {
    #[serde(rename = "worktree")]
    Worktree,
    #[serde(rename = "setup")]
    Setup,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedRunProgressCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub phase: PreparationPhase,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedRunFailCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub failure: ProviderFailure,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInterruptCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reason: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub hold_queue: Option<Option<bool>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationDeliveryAcceptCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub message_id: MessageId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedMessagePromoteCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub queued_run_id: RunId,
    pub target_run_id: RunId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedRunReorderCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub before_run_id: Option<RunId>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedRunEditCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub text: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub context: Option<Option<OrchestrationMessageContext>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub attachments: Option<Option<Vec<ChatAttachment>>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRequestRespondCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub request_id: RuntimeRequestId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub decision: Option<Option<ProviderApprovalDecision>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub answers: Option<Option<ProviderUserInputAnswers>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub attachments_by_question_id: Option<Option<UserInputAttachments>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputDismissCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub request_id: RuntimeRequestId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProviderCommand {
    #[serde(rename = "message.dispatch")]
    MessageDispatch(MessageDispatchCommand),
    #[serde(rename = "prepared-run.release")]
    PreparedRunRelease(RunIdentityCommand),
    #[serde(rename = "prepared-run.retry")]
    PreparedRunRetry(RunIdentityCommand),
    #[serde(rename = "prepared-run.progress")]
    PreparedRunProgress(PreparedRunProgressCommand),
    #[serde(rename = "prepared-run.fail")]
    PreparedRunFail(PreparedRunFailCommand),
    #[serde(rename = "notification.delivery.accept")]
    NotificationDeliveryAccept(NotificationDeliveryAcceptCommand),
    #[serde(rename = "run.interrupt")]
    RunInterrupt(RunInterruptCommand),
    #[serde(rename = "queued-message.promote-to-steer")]
    QueuedMessagePromoteToSteer(QueuedMessagePromoteCommand),
    #[serde(rename = "queue.resume")]
    QueueResume(ThreadIdentityCommand),
    #[serde(rename = "queued-run.reorder")]
    QueuedRunReorder(QueuedRunReorderCommand),
    #[serde(rename = "queued-run.cancel")]
    QueuedRunCancel(RunIdentityCommand),
    #[serde(rename = "queued-run.edit")]
    QueuedRunEdit(QueuedRunEditCommand),
    #[serde(rename = "runtime-request.respond")]
    RuntimeRequestRespond(RuntimeRequestRespondCommand),
    #[serde(rename = "thread.user-input.dismiss")]
    UserInputDismiss(UserInputDismissCommand),
}

impl Default for RuntimePolicyCapabilities {
    fn default() -> Self {
        Self {
            enforcement: RuntimePolicyEnforcement::ClientBoundary,
        }
    }
}
fn deserialize_runtime_policy_default<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<RuntimePolicyCapabilities, D::Error> {
    Ok(Option::<RuntimePolicyCapabilities>::deserialize(d)?.unwrap_or_default())
}
impl Run {
    pub fn work_started_at(&self) -> &UtcDateTime {
        self.work_started_at
            .as_ref()
            .and_then(Option::as_ref)
            .or(self.started_at.as_ref())
            .unwrap_or(&self.requested_at)
    }
}
impl ExecutionNodeStatus {
    pub fn is_work_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Waiting)
    }
}
pub fn latest_provider_turn_for_attempt<'a>(
    turns: &'a [ProviderTurn],
    attempt: Option<&RunAttemptId>,
) -> Option<&'a ProviderTurn> {
    let attempt = attempt?;
    turns
        .iter()
        .filter(|turn| turn.run_attempt_id.as_ref() == Some(attempt))
        .max_by_key(|turn| turn.ordinal.0)
}

#[derive(Debug, Clone, PartialEq)]
pub enum NotificationSource {
    DelegatedTask {
        task_ids: Vec<NodeId>,
        child_thread_id: Option<Option<ThreadId>>,
    },
    Subagent {
        child_thread_id: Option<Option<ThreadId>>,
    },
    Command,
    Monitor,
    BackgroundTask,
}
impl<'de> Deserialize<'de> for NotificationSource {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(d)?;
        let object = value
            .as_object()
            .ok_or_else(|| D::Error::custom("Expected notification source object"))?;
        let kind = object.get("kind");
        let optional_child = || -> Result<Option<Option<ThreadId>>, D::Error> {
            object
                .get("childThreadId")
                .map(|v| serde_json::from_value(v.clone()).map_err(D::Error::custom))
                .transpose()
        };
        match kind.and_then(serde_json::Value::as_str) {
            Some("delegated_task") => Ok(Self::DelegatedTask {
                task_ids: serde_json::from_value(
                    object
                        .get("taskIds")
                        .cloned()
                        .ok_or_else(|| D::Error::custom("Missing taskIds"))?,
                )
                .map_err(D::Error::custom)?,
                child_thread_id: optional_child()?,
            }),
            Some("subagent") => Ok(Self::Subagent {
                child_thread_id: optional_child()?,
            }),
            Some("background_task")
                if object.get("work").and_then(serde_json::Value::as_str) == Some("subagent") =>
            {
                match optional_child() {
                    Ok(child_thread_id) => Ok(Self::Subagent { child_thread_id }),
                    Err(_) => Ok(Self::BackgroundTask),
                }
            }
            Some("command" | "background_command") => Ok(Self::Command),
            Some("monitor") => Ok(Self::Monitor),
            Some(_) | None if kind.is_none_or(|v| v.is_null() || v.is_string()) => {
                Ok(Self::BackgroundTask)
            }
            _ => Err(D::Error::custom("Expected string notification source kind")),
        }
    }
}
impl Serialize for NotificationSource {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut o = serde_json::Map::new();
        match self {
            Self::DelegatedTask {
                task_ids,
                child_thread_id,
            } => {
                o.insert("kind".into(), "delegated_task".into());
                o.insert(
                    "taskIds".into(),
                    serde_json::to_value(task_ids).map_err(serde::ser::Error::custom)?,
                );
                if let Some(id) = child_thread_id {
                    o.insert(
                        "childThreadId".into(),
                        serde_json::to_value(id).map_err(serde::ser::Error::custom)?,
                    );
                }
            }
            Self::Subagent { child_thread_id } => {
                o.insert("kind".into(), "background_task".into());
                o.insert("work".into(), "subagent".into());
                if let Some(Some(id)) = child_thread_id {
                    o.insert(
                        "childThreadId".into(),
                        serde_json::to_value(id).map_err(serde::ser::Error::custom)?,
                    );
                }
            }
            Self::Command => {
                o.insert("kind".into(), "background_command".into());
            }
            Self::Monitor => {
                o.insert("kind".into(), "monitor".into());
            }
            Self::BackgroundTask => {
                o.insert("kind".into(), "background_task".into());
            }
        }
        o.serialize(s)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationOutcome {
    Completed,
    Failed,
    Cancelled,
    Updated,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub source: NotificationSource,
    pub outcome: NotificationOutcome,
    pub summary: TrimmedNonEmptyString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub detail: Option<Option<String>>,
}

/// Service view of Effect optional fields: explicit JSON null decodes to
/// undefined for Schema.optional, while wire re-encoding still retains null.
fn remove_optional_nulls(value: &mut serde_json::Value, fields: &[&str]) {
    if let Some(object) = value.as_object_mut() {
        for field in fields {
            if object.get(*field).is_some_and(serde_json::Value::is_null) {
                object.remove(*field);
            }
        }
    }
}
fn normalize_workspace_service(value: &mut serde_json::Value) {
    remove_optional_nulls(value, &["branch", "startFromOrigin"]);
}
impl ThreadLaunchInput {
    pub fn service_payload(&self) -> Result<serde_json::Value, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        remove_optional_nulls(
            &mut value,
            &[
                "creationSource",
                "threadId",
                "reuseExistingThread",
                "generateTitle",
                "initialMessage",
            ],
        );
        if let Some(strategy) = value.get_mut("workspaceStrategy") {
            normalize_workspace_service(strategy);
        }
        if let Some(message) = value.get_mut("initialMessage") {
            remove_optional_nulls(message, &["messageId", "context"]);
        }
        Ok(value)
    }
}
impl ProviderCommand {
    pub fn service_payload(&self) -> Result<serde_json::Value, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        let optional: &[&str] = match self {
            Self::MessageDispatch(_) => &[
                "notification",
                "scheduledTaskId",
                "senderThreadId",
                "context",
                "titleSeed",
                "modelSelection",
                "sourcePlanRef",
                "restartContinuationOfRunId",
                "usageLimitContinuationOfRunId",
                "manualContinuationOfRunId",
                "usageLimitRecoveryRequestId",
                "deliveryIntent",
                "delegatedCompletion",
            ],
            Self::RunInterrupt(_) => &["reason", "holdQueue"],
            Self::QueuedRunEdit(_) => &["context", "attachments"],
            Self::RuntimeRequestRespond(_) => &["decision", "answers", "attachmentsByQuestionId"],
            _ => &[],
        };
        remove_optional_nulls(&mut value, optional);
        if let Some(mode) = value.get_mut("dispatchMode") {
            remove_optional_nulls(mode, &["workspaceStrategy"]);
            if let Some(strategy) = mode.get_mut("workspaceStrategy") {
                normalize_workspace_service(strategy);
            }
        }
        if let Some(notification) = value.get_mut("notification") {
            remove_optional_nulls(notification, &["detail"]);
        }
        Ok(value)
    }
    pub fn command_id(&self) -> &CommandId {
        match self {
            Self::MessageDispatch(c) => &c.command_id,
            Self::PreparedRunRelease(c) | Self::PreparedRunRetry(c) | Self::QueuedRunCancel(c) => {
                &c.command_id
            }
            Self::PreparedRunProgress(c) => &c.command_id,
            Self::PreparedRunFail(c) => &c.command_id,
            Self::NotificationDeliveryAccept(c) => &c.command_id,
            Self::RunInterrupt(c) => &c.command_id,
            Self::QueuedMessagePromoteToSteer(c) => &c.command_id,
            Self::QueueResume(c) => &c.command_id,
            Self::QueuedRunReorder(c) => &c.command_id,
            Self::QueuedRunEdit(c) => &c.command_id,
            Self::RuntimeRequestRespond(c) => &c.command_id,
            Self::UserInputDismiss(c) => &c.command_id,
        }
    }
    pub fn thread_id(&self) -> &ThreadId {
        match self {
            Self::MessageDispatch(c) => &c.thread_id,
            Self::PreparedRunRelease(c) | Self::PreparedRunRetry(c) | Self::QueuedRunCancel(c) => {
                &c.thread_id
            }
            Self::PreparedRunProgress(c) => &c.thread_id,
            Self::PreparedRunFail(c) => &c.thread_id,
            Self::NotificationDeliveryAccept(c) => &c.thread_id,
            Self::RunInterrupt(c) => &c.thread_id,
            Self::QueuedMessagePromoteToSteer(c) => &c.thread_id,
            Self::QueueResume(c) => &c.thread_id,
            Self::QueuedRunReorder(c) => &c.thread_id,
            Self::QueuedRunEdit(c) => &c.thread_id,
            Self::RuntimeRequestRespond(c) => &c.thread_id,
            Self::UserInputDismiss(c) => &c.thread_id,
        }
    }
}
