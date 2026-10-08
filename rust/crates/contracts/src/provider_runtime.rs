//! Canonical provider runtime events and their complete typed payloads.
//! These transport contracts do not imply every native provider adapter is implemented.
use crate::history::object_struct;
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type ProviderRequestId = TrimmedNonEmptyString;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeSessionState {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeThreadState {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "archived")]
    Archived,
    #[serde(rename = "closed")]
    Closed,
    #[serde(rename = "compacted")]
    Compacted,
    #[serde(rename = "error")]
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeTurnState {
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "interrupted")]
    Interrupted,
    #[serde(rename = "cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimePlanStepStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "inProgress")]
    InProgress,
    #[serde(rename = "completed")]
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeItemStatus {
    #[serde(rename = "inProgress")]
    InProgress,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "declined")]
    Declined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeContentStreamKind {
    #[serde(rename = "assistant_text")]
    AssistantText,
    #[serde(rename = "reasoning_text")]
    ReasoningText,
    #[serde(rename = "reasoning_summary_text")]
    ReasoningSummaryText,
    #[serde(rename = "plan_text")]
    PlanText,
    #[serde(rename = "command_output")]
    CommandOutput,
    #[serde(rename = "file_change_output")]
    FileChangeOutput,
    #[serde(rename = "unknown")]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeSessionExitKind {
    #[serde(rename = "graceful")]
    Graceful,
    #[serde(rename = "error")]
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeErrorClass {
    #[serde(rename = "provider_error")]
    ProviderError,
    #[serde(rename = "transport_error")]
    TransportError,
    #[serde(rename = "permission_error")]
    PermissionError,
    #[serde(rename = "validation_error")]
    ValidationError,
    #[serde(rename = "unknown")]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolLifecycleItemType {
    #[serde(rename = "command_execution")]
    CommandExecution,
    #[serde(rename = "file_change")]
    FileChange,
    #[serde(rename = "mcp_tool_call")]
    McpToolCall,
    #[serde(rename = "dynamic_tool_call")]
    DynamicToolCall,
    #[serde(rename = "collab_agent_tool_call")]
    CollabAgentToolCall,
    #[serde(rename = "web_search")]
    WebSearch,
    #[serde(rename = "image_view")]
    ImageView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanonicalItemType {
    #[serde(rename = "user_message")]
    UserMessage,
    #[serde(rename = "assistant_message")]
    AssistantMessage,
    #[serde(rename = "reasoning")]
    Reasoning,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "command_execution")]
    CommandExecution,
    #[serde(rename = "file_change")]
    FileChange,
    #[serde(rename = "mcp_tool_call")]
    McpToolCall,
    #[serde(rename = "dynamic_tool_call")]
    DynamicToolCall,
    #[serde(rename = "collab_agent_tool_call")]
    CollabAgentToolCall,
    #[serde(rename = "web_search")]
    WebSearch,
    #[serde(rename = "image_view")]
    ImageView,
    #[serde(rename = "review_entered")]
    ReviewEntered,
    #[serde(rename = "review_exited")]
    ReviewExited,
    #[serde(rename = "context_compaction")]
    ContextCompaction,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "unknown")]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanonicalRequestType {
    #[serde(rename = "command_execution_approval")]
    CommandExecutionApproval,
    #[serde(rename = "file_read_approval")]
    FileReadApproval,
    #[serde(rename = "file_change_approval")]
    FileChangeApproval,
    #[serde(rename = "apply_patch_approval")]
    ApplyPatchApproval,
    #[serde(rename = "exec_command_approval")]
    ExecCommandApproval,
    #[serde(rename = "mcp_elicitation_approval")]
    McpElicitationApproval,
    #[serde(rename = "permission_approval")]
    PermissionApproval,
    #[serde(rename = "tool_user_input")]
    ToolUserInput,
    #[serde(rename = "dynamic_tool_call")]
    DynamicToolCall,
    #[serde(rename = "auth_tokens_refresh")]
    AuthTokensRefresh,
    #[serde(rename = "unknown")]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeTaskStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "waiting")]
    Waiting,
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "interrupted")]
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeTaskAgentKind {
    #[serde(rename = "agent")]
    Agent,
    #[serde(rename = "background")]
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeTaskCompletedStatus {
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "stopped")]
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeHookOutcome {
    #[serde(rename = "success")]
    Success,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserInputResponseMode {
    #[serde(rename = "message")]
    Message,
}

object_struct! {pub struct RuntimeEventRaw {
    pub source:RuntimeEventRawSource,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub method:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub message_type:Option<Option<TrimmedNonEmptyString>>,
    pub payload:serde_json::Value,
}}

object_struct! {pub struct ProviderRefs {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_turn_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_item_id:Option<Option<ProviderItemId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_request_id:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct ProviderRuntimeEventBase {
    pub event_id:EventId,
    pub provider:ProviderDriverKind,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_instance_id:Option<Option<ProviderInstanceId>>,
    pub thread_id:ThreadId,
    pub created_at:IsoDateTime,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub turn_id:Option<Option<TurnId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub item_id:Option<Option<RuntimeItemId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub request_id:Option<Option<RuntimeRequestId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_refs:Option<Option<ProviderRefs>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub raw:Option<Option<RuntimeEventRaw>>,
}}

object_struct! {pub struct SessionStartedPayload {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub message:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub resume:Option<Option<serde_json::Value>>,
}}

object_struct! {pub struct SessionConfiguredPayload {
    pub config:BTreeMap<String,serde_json::Value>,
}}

object_struct! {pub struct SessionStateChangedPayload {
    pub state:RuntimeSessionState,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub reason:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub detail:Option<Option<serde_json::Value>>,
}}

object_struct! {pub struct SessionExitedPayload {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub reason:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub recoverable:Option<Option<bool>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub exit_kind:Option<Option<RuntimeSessionExitKind>>,
}}

object_struct! {pub struct ThreadStartedPayload {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_thread_id:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct ThreadStateChangedPayload {
    pub state:RuntimeThreadState,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub before_tokens:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub after_tokens:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub detail:Option<Option<serde_json::Value>>,
}}

object_struct! {pub struct ThreadMetadataUpdatedPayload {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub name:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub metadata:Option<Option<BTreeMap<String,serde_json::Value>>>,
}}

object_struct! {pub struct ThreadTokenUsageUpdatedPayload {
    pub usage:ThreadTokenUsageSnapshot,
}}

object_struct! {pub struct ThreadRealtimeStartedPayload {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub realtime_session_id:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct ThreadRealtimeItemAddedPayload {
    pub item:serde_json::Value,
}}

object_struct! {pub struct ThreadRealtimeAudioDeltaPayload {
    pub audio:serde_json::Value,
}}

object_struct! {pub struct ThreadRealtimeErrorPayload {
    pub message:TrimmedNonEmptyString,
}}

object_struct! {pub struct ThreadRealtimeClosedPayload {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub reason:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct TurnStartedPayload {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub model:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub effort:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct TurnCompletedPayload {
    pub state:RuntimeTurnState,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub stop_reason:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub usage:Option<Option<serde_json::Value>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub model_usage:Option<Option<BTreeMap<String,serde_json::Value>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub total_cost_usd:Option<Option<serde_json::Number>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub error_message:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub token_usage:Option<Option<TurnTokenUsage>>,
}}

object_struct! {pub struct TurnAbortedPayload {
    pub reason:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub token_usage:Option<Option<TurnTokenUsage>>,
}}

object_struct! {pub struct RuntimePlanStep {
    pub step:TrimmedNonEmptyString,
    pub status:RuntimePlanStepStatus,
}}

object_struct! {pub struct TurnPlanUpdatedPayload {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub explanation:Option<Option<TrimmedNonEmptyString>>,
    pub plan:Vec<RuntimePlanStep>,
}}

object_struct! {pub struct TurnProposedDeltaPayload {
    pub delta:String,
}}

object_struct! {pub struct TurnProposedCompletedPayload {
    pub plan_markdown:TrimmedNonEmptyString,
}}

object_struct! {pub struct TurnDiffUpdatedPayload {
    pub unified_diff:String,
}}

object_struct! {pub struct ItemLifecyclePayload {
    pub item_type:CanonicalItemType,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub status:Option<Option<RuntimeItemStatus>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub detail:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_surface:Option<Option<ToolActivitySurface>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_icon:Option<Option<ToolActivityIcon>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_source:Option<Option<ToolActivitySource>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub data:Option<Option<serde_json::Value>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub parent_tool_use_id:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct ContentDeltaPayload {
    pub stream_kind:RuntimeContentStreamKind,
    pub delta:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub content_index:Option<Option<SignedSafeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub summary_index:Option<Option<SignedSafeInt>>,
}}

object_struct! {pub struct RequestOpenedPayload {
    pub request_type:CanonicalRequestType,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub detail:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub app_name:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub options:Option<Option<Vec<ProviderApprovalOption>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub args:Option<Option<serde_json::Value>>,
}}

object_struct! {pub struct RequestResolvedPayload {
    pub request_type:CanonicalRequestType,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub decision:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub resolution:Option<Option<serde_json::Value>>,
}}

object_struct! {pub struct RuntimeUserInputQuestionOption {
    pub label:TrimmedNonEmptyString,
    pub description:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub value:Option<Option<String>>,
}}

object_struct! {pub struct UserInputQuestion {
    pub id:TrimmedNonEmptyString,
    pub header:TrimmedNonEmptyString,
    pub question:TrimmedNonEmptyString,
    pub options:Vec<RuntimeUserInputQuestionOption>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub allow_custom_answer:Option<Option<bool>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub multi_select:Option<Option<bool>>,
}}

object_struct! {pub struct UserInputRequestedPayload {
    pub questions:Vec<UserInputQuestion>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub response_mode:Option<Option<UserInputResponseMode>>,
}}

object_struct! {pub struct UserInputResolvedPayload {
    pub answers:BTreeMap<String,serde_json::Value>,
}}

object_struct! {pub struct RuntimeTaskUsage {
    pub total_tokens:NonNegativeInt,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub input_tokens:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cached_input_tokens:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub output_tokens:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub reasoning_output_tokens:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_uses:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub duration_ms:Option<Option<NonNegativeInt>>,
}}

object_struct! {pub struct TaskWorkflowPhase {
    pub index:NonNegativeInt,
    pub title:TrimmedNonEmptyString,
}}

object_struct! {pub struct TaskRunHandles {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub run_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub script_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub transcript_dir:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub session_url:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct TaskStartedPayload {
    pub task_id:RuntimeTaskId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub description:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub task_type:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_kind:Option<Option<RuntimeTaskAgentKind>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub role:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub model:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub effort:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_use_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub parent_agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub workflow_name:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_index:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phase_index:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phase_title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phases:Option<Option<Vec<TaskWorkflowPhase>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub attempt:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub run_handles:Option<Option<TaskRunHandles>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub output_file:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub timeline_bypass:Option<Option<bool>>,
}}

object_struct! {pub struct TaskProgressPayload {
    pub task_id:RuntimeTaskId,
    pub description:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub summary:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub usage:Option<Option<serde_json::Value>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub typed_usage:Option<Option<RuntimeTaskUsage>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub last_tool_name:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub status:Option<Option<RuntimeTaskStatus>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub error:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub task_type:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_kind:Option<Option<RuntimeTaskAgentKind>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub role:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub model:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub effort:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_use_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub parent_agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub workflow_name:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_index:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phase_index:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phase_title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phases:Option<Option<Vec<TaskWorkflowPhase>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub attempt:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub run_handles:Option<Option<TaskRunHandles>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub output_file:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub timeline_bypass:Option<Option<bool>>,
}}

object_struct! {pub struct TaskUpdatedPayload {
    pub task_id:RuntimeTaskId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub status:Option<Option<RuntimeTaskStatus>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub description:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub error:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub ended_at:Option<Option<IsoDateTime>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub is_backgrounded:Option<Option<bool>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub task_type:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_kind:Option<Option<RuntimeTaskAgentKind>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub role:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub model:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub effort:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_use_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub parent_agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub workflow_name:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_index:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phase_index:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phase_title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phases:Option<Option<Vec<TaskWorkflowPhase>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub attempt:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub run_handles:Option<Option<TaskRunHandles>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub output_file:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub timeline_bypass:Option<Option<bool>>,
}}

object_struct! {pub struct TaskCompletedPayload {
    pub task_id:RuntimeTaskId,
    pub status:RuntimeTaskCompletedStatus,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub summary:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub usage:Option<Option<serde_json::Value>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub typed_usage:Option<Option<RuntimeTaskUsage>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub task_type:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_kind:Option<Option<RuntimeTaskAgentKind>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub role:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub model:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub effort:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_use_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub parent_agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub workflow_name:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_index:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phase_index:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phase_title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phases:Option<Option<Vec<TaskWorkflowPhase>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub attempt:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub run_handles:Option<Option<TaskRunHandles>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub output_file:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub timeline_bypass:Option<Option<bool>>,
}}

object_struct! {pub struct HookStartedPayload {
    pub hook_id:TrimmedNonEmptyString,
    pub hook_name:TrimmedNonEmptyString,
    pub hook_event:TrimmedNonEmptyString,
}}

object_struct! {pub struct HookProgressPayload {
    pub hook_id:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub output:Option<Option<String>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub stdout:Option<Option<String>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub stderr:Option<Option<String>>,
}}

object_struct! {pub struct HookCompletedPayload {
    pub hook_id:TrimmedNonEmptyString,
    pub outcome:RuntimeHookOutcome,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub output:Option<Option<String>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub stdout:Option<Option<String>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub stderr:Option<Option<String>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub exit_code:Option<Option<SignedSafeInt>>,
}}

object_struct! {pub struct ToolProgressPayload {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_use_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_name:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub summary:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub elapsed_seconds:Option<Option<serde_json::Number>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub task_id:Option<Option<RuntimeTaskId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub parent_tool_use_id:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct ToolSummaryPayload {
    pub summary:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub preceding_tool_use_ids:Option<Option<Vec<TrimmedNonEmptyString>>>,
}}

object_struct! {pub struct AuthStatusPayload {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub is_authenticating:Option<Option<bool>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub output:Option<Option<Vec<String>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub error:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct AccountUpdatedPayload {
    pub account:serde_json::Value,
}}

object_struct! {pub struct AccountRateLimitsUpdatedPayload {
    pub limits:ProviderUsageLimitsUpdate,
}}

object_struct! {pub struct McpStatusUpdatedPayload {
    pub status:serde_json::Value,
}}

object_struct! {pub struct McpOauthCompletedPayload {
    pub success:bool,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub name:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub error:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct ModelReroutedPayload {
    pub from_model:TrimmedNonEmptyString,
    pub to_model:TrimmedNonEmptyString,
    pub reason:TrimmedNonEmptyString,
}}

object_struct! {pub struct ConfigWarningPayload {
    pub summary:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub details:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub range:Option<Option<serde_json::Value>>,
}}

object_struct! {pub struct DeprecationNoticePayload {
    pub summary:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub details:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct FilesPersistedPayload {
    pub files:Vec<RuntimePersistedFile>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub failed:Option<Option<Vec<RuntimePersistedFileFailure>>>,
}}

object_struct! {pub struct ToolDeniedPayload {
    pub tool_name:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_use_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub reason:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_id:Option<Option<TrimmedNonEmptyString>>,
}}

object_struct! {pub struct RuntimeWarningPayload {
    pub message:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub detail:Option<Option<serde_json::Value>>,
}}

object_struct! {pub struct RuntimeErrorPayload {
    pub message:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub code:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub class:Option<Option<RuntimeErrorClass>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub detail:Option<Option<serde_json::Value>>,
}}

object_struct! {pub struct TaskAgentLinkage {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub task_type:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_kind:Option<Option<RuntimeTaskAgentKind>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub role:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub model:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub effort:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tool_use_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub parent_agent_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub workflow_name:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_index:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phase_index:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phase_title:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub phases:Option<Option<Vec<TaskWorkflowPhase>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub attempt:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub run_handles:Option<Option<TaskRunHandles>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub output_file:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub timeline_bypass:Option<Option<bool>>,
}}

object_struct! {pub struct RuntimePersistedFile {
    pub filename:TrimmedNonEmptyString,
    pub file_id:TrimmedNonEmptyString,
}}

object_struct! {pub struct RuntimePersistedFileFailure {
    pub filename:TrimmedNonEmptyString,
    pub error:TrimmedNonEmptyString,
}}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RuntimeEventRawSource(String);
impl RuntimeEventRawSource {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for RuntimeEventRawSource {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        if [
            "codex.app-server.notification",
            "codex.app-server.request",
            "codex.eventmsg",
            "claude.sdk.message",
            "claude.sdk.permission",
            "codex.sdk.thread-event",
            "opencode.sdk.event",
            "acp.jsonrpc",
        ]
        .contains(&value.as_str())
            || (value.starts_with("acp.") && value.ends_with(".extension") && value.len() >= 14)
        {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "unknown provider raw event source",
            ))
        }
    }
}

pub const PROVIDER_RUNTIME_EVENT_TYPES: &[&str] = &[
    "session.started",
    "session.configured",
    "session.state.changed",
    "session.exited",
    "thread.started",
    "thread.state.changed",
    "thread.metadata.updated",
    "thread.token-usage.updated",
    "thread.realtime.started",
    "thread.realtime.item-added",
    "thread.realtime.audio.delta",
    "thread.realtime.error",
    "thread.realtime.closed",
    "turn.started",
    "turn.completed",
    "turn.aborted",
    "turn.plan.updated",
    "turn.proposed.delta",
    "turn.proposed.completed",
    "turn.diff.updated",
    "item.started",
    "item.updated",
    "item.completed",
    "content.delta",
    "request.opened",
    "request.resolved",
    "user-input.requested",
    "user-input.resolved",
    "task.started",
    "task.progress",
    "task.updated",
    "task.completed",
    "hook.started",
    "hook.progress",
    "hook.completed",
    "tool.progress",
    "tool.summary",
    "auth.status",
    "account.updated",
    "account.rate-limits.updated",
    "mcp.status.updated",
    "mcp.oauth.completed",
    "model.rerouted",
    "config.warning",
    "deprecation.notice",
    "files.persisted",
    "tool.denied",
    "runtime.warning",
    "runtime.error",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeEventTag<const KIND: usize>;
impl<const KIND: usize> Serialize for RuntimeEventTag<KIND> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let tag = PROVIDER_RUNTIME_EVENT_TYPES
            .get(KIND)
            .ok_or_else(|| serde::ser::Error::custom("unknown runtime event tag"))?;
        s.serialize_str(tag)
    }
}
impl<'de, const KIND: usize> Deserialize<'de> for RuntimeEventTag<KIND> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        if PROVIDER_RUNTIME_EVENT_TYPES
            .get(KIND)
            .is_some_and(|tag| *tag == value)
        {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("incorrect runtime event tag"))
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaggedProviderRuntimeEvent<P, const KIND: usize> {
    #[serde(flatten)]
    pub base: ProviderRuntimeEventBase,
    #[serde(rename = "type")]
    pub event_type: RuntimeEventTag<KIND>,
    pub payload: P,
}
impl<P, const KIND: usize> TaggedProviderRuntimeEvent<P, KIND> {
    pub fn new(base: ProviderRuntimeEventBase, payload: P) -> Self {
        Self {
            base,
            event_type: RuntimeEventTag,
            payload,
        }
    }
}
impl<'de, P: serde::de::DeserializeOwned, const KIND: usize> Deserialize<'de>
    for TaggedProviderRuntimeEvent<P, KIND>
{
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut object = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let tag = object.remove("type").unwrap_or(serde_json::Value::Null);
        let event_type = serde_json::from_value(tag).map_err(serde::de::Error::custom)?;
        let payload =
            serde_json::from_value(object.remove("payload").unwrap_or(serde_json::Value::Null))
                .map_err(serde::de::Error::custom)?;
        let base = serde_json::from_value(serde_json::Value::Object(object))
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            base,
            event_type,
            payload,
        })
    }
}

pub type ProviderRuntimeSessionStartedEvent = TaggedProviderRuntimeEvent<SessionStartedPayload, 0>;
pub type ProviderRuntimeSessionConfiguredEvent =
    TaggedProviderRuntimeEvent<SessionConfiguredPayload, 1>;
pub type ProviderRuntimeSessionStateChangedEvent =
    TaggedProviderRuntimeEvent<SessionStateChangedPayload, 2>;
pub type ProviderRuntimeSessionExitedEvent = TaggedProviderRuntimeEvent<SessionExitedPayload, 3>;
pub type ProviderRuntimeThreadStartedEvent = TaggedProviderRuntimeEvent<ThreadStartedPayload, 4>;
pub type ProviderRuntimeThreadStateChangedEvent =
    TaggedProviderRuntimeEvent<ThreadStateChangedPayload, 5>;
pub type ProviderRuntimeThreadMetadataUpdatedEvent =
    TaggedProviderRuntimeEvent<ThreadMetadataUpdatedPayload, 6>;
pub type ProviderRuntimeThreadTokenUsageUpdatedEvent =
    TaggedProviderRuntimeEvent<ThreadTokenUsageUpdatedPayload, 7>;
pub type ProviderRuntimeThreadRealtimeStartedEvent =
    TaggedProviderRuntimeEvent<ThreadRealtimeStartedPayload, 8>;
pub type ProviderRuntimeThreadRealtimeItemAddedEvent =
    TaggedProviderRuntimeEvent<ThreadRealtimeItemAddedPayload, 9>;
pub type ProviderRuntimeThreadRealtimeAudioDeltaEvent =
    TaggedProviderRuntimeEvent<ThreadRealtimeAudioDeltaPayload, 10>;
pub type ProviderRuntimeThreadRealtimeErrorEvent =
    TaggedProviderRuntimeEvent<ThreadRealtimeErrorPayload, 11>;
pub type ProviderRuntimeThreadRealtimeClosedEvent =
    TaggedProviderRuntimeEvent<ThreadRealtimeClosedPayload, 12>;
pub type ProviderRuntimeTurnStartedEvent = TaggedProviderRuntimeEvent<TurnStartedPayload, 13>;
pub type ProviderRuntimeTurnCompletedEvent = TaggedProviderRuntimeEvent<TurnCompletedPayload, 14>;
pub type ProviderRuntimeTurnAbortedEvent = TaggedProviderRuntimeEvent<TurnAbortedPayload, 15>;
pub type ProviderRuntimeTurnPlanUpdatedEvent =
    TaggedProviderRuntimeEvent<TurnPlanUpdatedPayload, 16>;
pub type ProviderRuntimeTurnProposedDeltaEvent =
    TaggedProviderRuntimeEvent<TurnProposedDeltaPayload, 17>;
pub type ProviderRuntimeTurnProposedCompletedEvent =
    TaggedProviderRuntimeEvent<TurnProposedCompletedPayload, 18>;
pub type ProviderRuntimeTurnDiffUpdatedEvent =
    TaggedProviderRuntimeEvent<TurnDiffUpdatedPayload, 19>;
pub type ProviderRuntimeItemStartedEvent = TaggedProviderRuntimeEvent<ItemLifecyclePayload, 20>;
pub type ProviderRuntimeItemUpdatedEvent = TaggedProviderRuntimeEvent<ItemLifecyclePayload, 21>;
pub type ProviderRuntimeItemCompletedEvent = TaggedProviderRuntimeEvent<ItemLifecyclePayload, 22>;
pub type ProviderRuntimeContentDeltaEvent = TaggedProviderRuntimeEvent<ContentDeltaPayload, 23>;
pub type ProviderRuntimeRequestOpenedEvent = TaggedProviderRuntimeEvent<RequestOpenedPayload, 24>;
pub type ProviderRuntimeRequestResolvedEvent =
    TaggedProviderRuntimeEvent<RequestResolvedPayload, 25>;
pub type ProviderRuntimeUserInputRequestedEvent =
    TaggedProviderRuntimeEvent<UserInputRequestedPayload, 26>;
pub type ProviderRuntimeUserInputResolvedEvent =
    TaggedProviderRuntimeEvent<UserInputResolvedPayload, 27>;
pub type ProviderRuntimeTaskStartedEvent = TaggedProviderRuntimeEvent<TaskStartedPayload, 28>;
pub type ProviderRuntimeTaskProgressEvent = TaggedProviderRuntimeEvent<TaskProgressPayload, 29>;
pub type ProviderRuntimeTaskUpdatedEvent = TaggedProviderRuntimeEvent<TaskUpdatedPayload, 30>;
pub type ProviderRuntimeTaskCompletedEvent = TaggedProviderRuntimeEvent<TaskCompletedPayload, 31>;
pub type ProviderRuntimeHookStartedEvent = TaggedProviderRuntimeEvent<HookStartedPayload, 32>;
pub type ProviderRuntimeHookProgressEvent = TaggedProviderRuntimeEvent<HookProgressPayload, 33>;
pub type ProviderRuntimeHookCompletedEvent = TaggedProviderRuntimeEvent<HookCompletedPayload, 34>;
pub type ProviderRuntimeToolProgressEvent = TaggedProviderRuntimeEvent<ToolProgressPayload, 35>;
pub type ProviderRuntimeToolSummaryEvent = TaggedProviderRuntimeEvent<ToolSummaryPayload, 36>;
pub type ProviderRuntimeAuthStatusEvent = TaggedProviderRuntimeEvent<AuthStatusPayload, 37>;
pub type ProviderRuntimeAccountUpdatedEvent = TaggedProviderRuntimeEvent<AccountUpdatedPayload, 38>;
pub type ProviderRuntimeAccountRateLimitsUpdatedEvent =
    TaggedProviderRuntimeEvent<AccountRateLimitsUpdatedPayload, 39>;
pub type ProviderRuntimeMcpStatusUpdatedEvent =
    TaggedProviderRuntimeEvent<McpStatusUpdatedPayload, 40>;
pub type ProviderRuntimeMcpOauthCompletedEvent =
    TaggedProviderRuntimeEvent<McpOauthCompletedPayload, 41>;
pub type ProviderRuntimeModelReroutedEvent = TaggedProviderRuntimeEvent<ModelReroutedPayload, 42>;
pub type ProviderRuntimeConfigWarningEvent = TaggedProviderRuntimeEvent<ConfigWarningPayload, 43>;
pub type ProviderRuntimeDeprecationNoticeEvent =
    TaggedProviderRuntimeEvent<DeprecationNoticePayload, 44>;
pub type ProviderRuntimeFilesPersistedEvent = TaggedProviderRuntimeEvent<FilesPersistedPayload, 45>;
pub type ProviderRuntimeToolDeniedEvent = TaggedProviderRuntimeEvent<ToolDeniedPayload, 46>;
pub type ProviderRuntimeWarningEvent = TaggedProviderRuntimeEvent<RuntimeWarningPayload, 47>;
pub type ProviderRuntimeErrorEvent = TaggedProviderRuntimeEvent<RuntimeErrorPayload, 48>;
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ProviderRuntimeEventV2 {
    SessionStarted(ProviderRuntimeSessionStartedEvent),
    SessionConfigured(ProviderRuntimeSessionConfiguredEvent),
    SessionStateChanged(ProviderRuntimeSessionStateChangedEvent),
    SessionExited(ProviderRuntimeSessionExitedEvent),
    ThreadStarted(ProviderRuntimeThreadStartedEvent),
    ThreadStateChanged(ProviderRuntimeThreadStateChangedEvent),
    ThreadMetadataUpdated(ProviderRuntimeThreadMetadataUpdatedEvent),
    ThreadTokenUsageUpdated(ProviderRuntimeThreadTokenUsageUpdatedEvent),
    ThreadRealtimeStarted(ProviderRuntimeThreadRealtimeStartedEvent),
    ThreadRealtimeItemAdded(ProviderRuntimeThreadRealtimeItemAddedEvent),
    ThreadRealtimeAudioDelta(ProviderRuntimeThreadRealtimeAudioDeltaEvent),
    ThreadRealtimeError(ProviderRuntimeThreadRealtimeErrorEvent),
    ThreadRealtimeClosed(ProviderRuntimeThreadRealtimeClosedEvent),
    TurnStarted(ProviderRuntimeTurnStartedEvent),
    TurnCompleted(ProviderRuntimeTurnCompletedEvent),
    TurnAborted(ProviderRuntimeTurnAbortedEvent),
    TurnPlanUpdated(ProviderRuntimeTurnPlanUpdatedEvent),
    TurnProposedDelta(ProviderRuntimeTurnProposedDeltaEvent),
    TurnProposedCompleted(ProviderRuntimeTurnProposedCompletedEvent),
    TurnDiffUpdated(ProviderRuntimeTurnDiffUpdatedEvent),
    ItemStarted(ProviderRuntimeItemStartedEvent),
    ItemUpdated(ProviderRuntimeItemUpdatedEvent),
    ItemCompleted(ProviderRuntimeItemCompletedEvent),
    ContentDelta(ProviderRuntimeContentDeltaEvent),
    RequestOpened(ProviderRuntimeRequestOpenedEvent),
    RequestResolved(ProviderRuntimeRequestResolvedEvent),
    UserInputRequested(ProviderRuntimeUserInputRequestedEvent),
    UserInputResolved(ProviderRuntimeUserInputResolvedEvent),
    TaskStarted(ProviderRuntimeTaskStartedEvent),
    TaskProgress(ProviderRuntimeTaskProgressEvent),
    TaskUpdated(ProviderRuntimeTaskUpdatedEvent),
    TaskCompleted(ProviderRuntimeTaskCompletedEvent),
    HookStarted(ProviderRuntimeHookStartedEvent),
    HookProgress(ProviderRuntimeHookProgressEvent),
    HookCompleted(ProviderRuntimeHookCompletedEvent),
    ToolProgress(ProviderRuntimeToolProgressEvent),
    ToolSummary(ProviderRuntimeToolSummaryEvent),
    AuthStatus(ProviderRuntimeAuthStatusEvent),
    AccountUpdated(ProviderRuntimeAccountUpdatedEvent),
    AccountRateLimitsUpdated(ProviderRuntimeAccountRateLimitsUpdatedEvent),
    McpStatusUpdated(ProviderRuntimeMcpStatusUpdatedEvent),
    McpOauthCompleted(ProviderRuntimeMcpOauthCompletedEvent),
    ModelRerouted(ProviderRuntimeModelReroutedEvent),
    ConfigWarning(ProviderRuntimeConfigWarningEvent),
    DeprecationNotice(ProviderRuntimeDeprecationNoticeEvent),
    FilesPersisted(ProviderRuntimeFilesPersistedEvent),
    ToolDenied(ProviderRuntimeToolDeniedEvent),
    Warning(ProviderRuntimeWarningEvent),
    Error(ProviderRuntimeErrorEvent),
}
pub type ProviderRuntimeEvent = ProviderRuntimeEventV2;
pub type ProviderRuntimeTurnStatus = RuntimeTurnState;

impl ProviderRuntimeEventV2 {
    pub fn base(&self) -> &ProviderRuntimeEventBase {
        match self {
            Self::SessionStarted(event) => &event.base,
            Self::SessionConfigured(event) => &event.base,
            Self::SessionStateChanged(event) => &event.base,
            Self::SessionExited(event) => &event.base,
            Self::ThreadStarted(event) => &event.base,
            Self::ThreadStateChanged(event) => &event.base,
            Self::ThreadMetadataUpdated(event) => &event.base,
            Self::ThreadTokenUsageUpdated(event) => &event.base,
            Self::ThreadRealtimeStarted(event) => &event.base,
            Self::ThreadRealtimeItemAdded(event) => &event.base,
            Self::ThreadRealtimeAudioDelta(event) => &event.base,
            Self::ThreadRealtimeError(event) => &event.base,
            Self::ThreadRealtimeClosed(event) => &event.base,
            Self::TurnStarted(event) => &event.base,
            Self::TurnCompleted(event) => &event.base,
            Self::TurnAborted(event) => &event.base,
            Self::TurnPlanUpdated(event) => &event.base,
            Self::TurnProposedDelta(event) => &event.base,
            Self::TurnProposedCompleted(event) => &event.base,
            Self::TurnDiffUpdated(event) => &event.base,
            Self::ItemStarted(event) => &event.base,
            Self::ItemUpdated(event) => &event.base,
            Self::ItemCompleted(event) => &event.base,
            Self::ContentDelta(event) => &event.base,
            Self::RequestOpened(event) => &event.base,
            Self::RequestResolved(event) => &event.base,
            Self::UserInputRequested(event) => &event.base,
            Self::UserInputResolved(event) => &event.base,
            Self::TaskStarted(event) => &event.base,
            Self::TaskProgress(event) => &event.base,
            Self::TaskUpdated(event) => &event.base,
            Self::TaskCompleted(event) => &event.base,
            Self::HookStarted(event) => &event.base,
            Self::HookProgress(event) => &event.base,
            Self::HookCompleted(event) => &event.base,
            Self::ToolProgress(event) => &event.base,
            Self::ToolSummary(event) => &event.base,
            Self::AuthStatus(event) => &event.base,
            Self::AccountUpdated(event) => &event.base,
            Self::AccountRateLimitsUpdated(event) => &event.base,
            Self::McpStatusUpdated(event) => &event.base,
            Self::McpOauthCompleted(event) => &event.base,
            Self::ModelRerouted(event) => &event.base,
            Self::ConfigWarning(event) => &event.base,
            Self::DeprecationNotice(event) => &event.base,
            Self::FilesPersisted(event) => &event.base,
            Self::ToolDenied(event) => &event.base,
            Self::Warning(event) => &event.base,
            Self::Error(event) => &event.base,
        }
    }
}

pub fn is_tool_lifecycle_item_type(value: &str) -> bool {
    [
        "command_execution",
        "file_change",
        "mcp_tool_call",
        "dynamic_tool_call",
        "collab_agent_tool_call",
        "web_search",
        "image_view",
    ]
    .contains(&value)
}

impl<'de> Deserialize<'de> for ProviderRuntimeEventV2 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        let tag = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("expected a provider runtime event type"))?
            .to_owned();
        match tag.as_str() {
            "session.started" => serde_json::from_value(value)
                .map(Self::SessionStarted)
                .map_err(serde::de::Error::custom),
            "session.configured" => serde_json::from_value(value)
                .map(Self::SessionConfigured)
                .map_err(serde::de::Error::custom),
            "session.state.changed" => serde_json::from_value(value)
                .map(Self::SessionStateChanged)
                .map_err(serde::de::Error::custom),
            "session.exited" => serde_json::from_value(value)
                .map(Self::SessionExited)
                .map_err(serde::de::Error::custom),
            "thread.started" => serde_json::from_value(value)
                .map(Self::ThreadStarted)
                .map_err(serde::de::Error::custom),
            "thread.state.changed" => serde_json::from_value(value)
                .map(Self::ThreadStateChanged)
                .map_err(serde::de::Error::custom),
            "thread.metadata.updated" => serde_json::from_value(value)
                .map(Self::ThreadMetadataUpdated)
                .map_err(serde::de::Error::custom),
            "thread.token-usage.updated" => serde_json::from_value(value)
                .map(Self::ThreadTokenUsageUpdated)
                .map_err(serde::de::Error::custom),
            "thread.realtime.started" => serde_json::from_value(value)
                .map(Self::ThreadRealtimeStarted)
                .map_err(serde::de::Error::custom),
            "thread.realtime.item-added" => serde_json::from_value(value)
                .map(Self::ThreadRealtimeItemAdded)
                .map_err(serde::de::Error::custom),
            "thread.realtime.audio.delta" => serde_json::from_value(value)
                .map(Self::ThreadRealtimeAudioDelta)
                .map_err(serde::de::Error::custom),
            "thread.realtime.error" => serde_json::from_value(value)
                .map(Self::ThreadRealtimeError)
                .map_err(serde::de::Error::custom),
            "thread.realtime.closed" => serde_json::from_value(value)
                .map(Self::ThreadRealtimeClosed)
                .map_err(serde::de::Error::custom),
            "turn.started" => serde_json::from_value(value)
                .map(Self::TurnStarted)
                .map_err(serde::de::Error::custom),
            "turn.completed" => serde_json::from_value(value)
                .map(Self::TurnCompleted)
                .map_err(serde::de::Error::custom),
            "turn.aborted" => serde_json::from_value(value)
                .map(Self::TurnAborted)
                .map_err(serde::de::Error::custom),
            "turn.plan.updated" => serde_json::from_value(value)
                .map(Self::TurnPlanUpdated)
                .map_err(serde::de::Error::custom),
            "turn.proposed.delta" => serde_json::from_value(value)
                .map(Self::TurnProposedDelta)
                .map_err(serde::de::Error::custom),
            "turn.proposed.completed" => serde_json::from_value(value)
                .map(Self::TurnProposedCompleted)
                .map_err(serde::de::Error::custom),
            "turn.diff.updated" => serde_json::from_value(value)
                .map(Self::TurnDiffUpdated)
                .map_err(serde::de::Error::custom),
            "item.started" => serde_json::from_value(value)
                .map(Self::ItemStarted)
                .map_err(serde::de::Error::custom),
            "item.updated" => serde_json::from_value(value)
                .map(Self::ItemUpdated)
                .map_err(serde::de::Error::custom),
            "item.completed" => serde_json::from_value(value)
                .map(Self::ItemCompleted)
                .map_err(serde::de::Error::custom),
            "content.delta" => serde_json::from_value(value)
                .map(Self::ContentDelta)
                .map_err(serde::de::Error::custom),
            "request.opened" => serde_json::from_value(value)
                .map(Self::RequestOpened)
                .map_err(serde::de::Error::custom),
            "request.resolved" => serde_json::from_value(value)
                .map(Self::RequestResolved)
                .map_err(serde::de::Error::custom),
            "user-input.requested" => serde_json::from_value(value)
                .map(Self::UserInputRequested)
                .map_err(serde::de::Error::custom),
            "user-input.resolved" => serde_json::from_value(value)
                .map(Self::UserInputResolved)
                .map_err(serde::de::Error::custom),
            "task.started" => serde_json::from_value(value)
                .map(Self::TaskStarted)
                .map_err(serde::de::Error::custom),
            "task.progress" => serde_json::from_value(value)
                .map(Self::TaskProgress)
                .map_err(serde::de::Error::custom),
            "task.updated" => serde_json::from_value(value)
                .map(Self::TaskUpdated)
                .map_err(serde::de::Error::custom),
            "task.completed" => serde_json::from_value(value)
                .map(Self::TaskCompleted)
                .map_err(serde::de::Error::custom),
            "hook.started" => serde_json::from_value(value)
                .map(Self::HookStarted)
                .map_err(serde::de::Error::custom),
            "hook.progress" => serde_json::from_value(value)
                .map(Self::HookProgress)
                .map_err(serde::de::Error::custom),
            "hook.completed" => serde_json::from_value(value)
                .map(Self::HookCompleted)
                .map_err(serde::de::Error::custom),
            "tool.progress" => serde_json::from_value(value)
                .map(Self::ToolProgress)
                .map_err(serde::de::Error::custom),
            "tool.summary" => serde_json::from_value(value)
                .map(Self::ToolSummary)
                .map_err(serde::de::Error::custom),
            "auth.status" => serde_json::from_value(value)
                .map(Self::AuthStatus)
                .map_err(serde::de::Error::custom),
            "account.updated" => serde_json::from_value(value)
                .map(Self::AccountUpdated)
                .map_err(serde::de::Error::custom),
            "account.rate-limits.updated" => serde_json::from_value(value)
                .map(Self::AccountRateLimitsUpdated)
                .map_err(serde::de::Error::custom),
            "mcp.status.updated" => serde_json::from_value(value)
                .map(Self::McpStatusUpdated)
                .map_err(serde::de::Error::custom),
            "mcp.oauth.completed" => serde_json::from_value(value)
                .map(Self::McpOauthCompleted)
                .map_err(serde::de::Error::custom),
            "model.rerouted" => serde_json::from_value(value)
                .map(Self::ModelRerouted)
                .map_err(serde::de::Error::custom),
            "config.warning" => serde_json::from_value(value)
                .map(Self::ConfigWarning)
                .map_err(serde::de::Error::custom),
            "deprecation.notice" => serde_json::from_value(value)
                .map(Self::DeprecationNotice)
                .map_err(serde::de::Error::custom),
            "files.persisted" => serde_json::from_value(value)
                .map(Self::FilesPersisted)
                .map_err(serde::de::Error::custom),
            "tool.denied" => serde_json::from_value(value)
                .map(Self::ToolDenied)
                .map_err(serde::de::Error::custom),
            "runtime.warning" => serde_json::from_value(value)
                .map(Self::Warning)
                .map_err(serde::de::Error::custom),
            "runtime.error" => serde_json::from_value(value)
                .map(Self::Error)
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "unknown provider runtime event type",
            )),
        }
    }
}
