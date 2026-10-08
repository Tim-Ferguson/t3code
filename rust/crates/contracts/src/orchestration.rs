use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub type ExtraFields = BTreeMap<String, Value>;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    User,
    Agent,
    System,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CreationSource {
    Web,
    Mobile,
    Mcp,
    Provider,
    Server,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Idle,
    Preparing,
    Queued,
    Starting,
    Running,
    Waiting,
    Completed,
    Interrupted,
    Failed,
    Cancelled,
    RolledBack,
}
impl ThreadStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Queued | Self::Starting | Self::Running | Self::Waiting
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadLocation {
    Active,
    Archive,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectShell {
    pub id: ProjectId,
    pub title: TrimmedNonEmptyString,
    pub workspace_root: TrimmedNonEmptyString,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub default_model_selection: Option<ModelSelection>,
    pub scripts: Vec<Value>,
    pub created_at: IsoDateTime,
    pub updated_at: IsoDateTime,
    #[serde(flatten)]
    pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadLineage {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub parent_thread_id: Option<ThreadId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub relationship_to_parent: Option<ThreadRelationship>,
    pub root_thread_id: ThreadId,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadRelationship {
    Fork,
    Subagent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppThread {
    pub created_by: Actor,
    pub creation_source: CreationSource,
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: TrimmedNonEmptyString,
    pub provider_instance_id: ProviderInstanceId,
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: ProviderInteractionMode,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub branch: Option<TrimmedNonEmptyString>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub worktree_path: Option<TrimmedNonEmptyString>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub active_provider_thread_id: Option<ProviderThreadId>,
    pub lineage: ThreadLineage,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub forked_from: Option<ThreadForkSource>,
    pub created_at: UtcDateTime,
    pub updated_at: UtcDateTime,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub archived_at: Option<UtcDateTime>,
    #[serde(default)]
    pub settled_override: Option<SettledOverride>,
    #[serde(default)]
    pub settled_at: Option<UtcDateTime>,
    #[serde(default)]
    pub last_visited_at: Option<UtcDateTime>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub deleted_at: Option<UtcDateTime>,
    #[serde(flatten)]
    pub extra: ExtraFields,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub linked_pull_request: Option<Option<ThreadLinkedPullRequest>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pull_requests: Option<Option<Vec<ThreadPullRequestLink>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch_pull_request: Option<Option<ThreadLinkedPullRequest>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub history_origin: Option<Option<ThreadHistoryOrigin>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub unsettled_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snoozed_until: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snoozed_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub limit_recovery: Option<Option<LimitRecovery>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pinned_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub auto_settle_disabled_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pin_order_key: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub active_order_key: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub title_regeneration: Option<Option<TitleRegeneration>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub rollback_request_id: Option<Option<CommandId>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub rollback_failure: Option<Option<RollbackFailure>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettledOverride {
    Settled,
    Active,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestVisibleMessageSummary {
    pub id: MessageId,
    pub role: MessageRole,
    pub text: String,
    pub updated_at: UtcDateTime,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRuntimeRequestSummary {
    pub id: RuntimeRequestId,
    pub kind: RuntimeRequestKind,
    pub created_at: UtcDateTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadShell {
    pub created_by: Actor,
    pub creation_source: CreationSource,
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub provider_instance_id: ProviderInstanceId,
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: ProviderInteractionMode,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub branch: Option<TrimmedNonEmptyString>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub worktree_path: Option<TrimmedNonEmptyString>,
    pub lineage: ThreadLineage,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub forked_from: Option<ThreadForkSource>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub active_provider_thread_id: Option<ProviderThreadId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub latest_run_id: Option<RunId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub active_run_id: Option<RunId>,
    pub status: ThreadStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub pending_runtime_request: Option<PendingRuntimeRequestSummary>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub latest_visible_message: Option<LatestVisibleMessageSummary>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub latest_user_message_at: Option<UtcDateTime>,
    pub has_actionable_proposed_plan: bool,
    #[serde(default, deserialize_with = "crate::provider::deserialize_default_vec")]
    pub pending_background_tasks: Vec<PendingBackgroundTask>,
    #[serde(default, deserialize_with = "crate::provider::deserialize_default_vec")]
    pub provider_instance_history: Vec<ProviderInstanceId>,
    #[serde(deserialize_with = "deserialize_nonnegative_u64")]
    pub item_count: u64,
    #[serde(deserialize_with = "deserialize_nonnegative_u64")]
    pub visible_item_count: u64,
    pub created_at: UtcDateTime,
    pub updated_at: UtcDateTime,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub archived_at: Option<UtcDateTime>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub settled_override: Option<SettledOverride>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub settled_at: Option<UtcDateTime>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub deleted_at: Option<UtcDateTime>,
    #[serde(flatten)]
    pub extra: ExtraFields,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub linked_pull_request: Option<Option<ThreadLinkedPullRequest>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pull_requests: Option<Option<Vec<ThreadPullRequestLink>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub branch_pull_request: Option<Option<ThreadLinkedPullRequest>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub history_origin: Option<Option<ThreadHistoryOrigin>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub unsettled_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snoozed_until: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snoozed_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub limit_recovery: Option<Option<LimitRecovery>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pinned_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub auto_settle_disabled_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pin_order_key: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub active_order_key: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub title_regeneration: Option<Option<TitleRegeneration>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub latest_run_requested_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub latest_run_started_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub latest_run_completed_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub activity_run_started_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub activity_run_status: Option<Option<ActivityRunStatus>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub last_error: Option<Option<String>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub last_error_class: Option<Option<ProviderFailureClass>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_limit_reset_at: Option<Option<IsoDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub latest_user_authored_message_at: Option<Option<UtcDateTime>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub goal: Option<Option<ProviderGoal>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub last_visited_at: Option<Option<UtcDateTime>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSnapshot {
    #[serde(deserialize_with = "positive_schema_version")]
    pub schema_version: u64,
    #[serde(deserialize_with = "deserialize_nonnegative_u64")]
    pub snapshot_sequence: u64,
    pub projects: Vec<ProjectShell>,
    pub threads: Vec<ThreadShell>,
    pub archived_threads: Vec<ThreadShell>,
}
fn positive_schema_version<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let version = PositiveInt::deserialize(d)?.0;
    if version == 0 {
        Err(serde::de::Error::custom("schemaVersion must be positive"))
    } else {
        Ok(version)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
pub enum ShellStreamItem {
    #[serde(rename = "synchronized")]
    Synchronized,
    #[serde(rename = "snapshot")]
    Snapshot {
        snapshot: ShellSnapshot,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        resolved_repository_identity_roots: Option<Vec<String>>,
    },
    #[serde(rename = "project.updated")]
    ProjectUpdated {
        #[serde(deserialize_with = "deserialize_nonnegative_u64")]
        sequence: u64,
        project: ProjectShell,
    },
    #[serde(rename = "project.removed")]
    ProjectRemoved {
        #[serde(deserialize_with = "deserialize_nonnegative_u64")]
        sequence: u64,
        project_id: ProjectId,
    },
    #[serde(rename = "thread.updated")]
    ThreadUpdated {
        #[serde(deserialize_with = "deserialize_nonnegative_u64")]
        sequence: u64,
        location: ThreadLocation,
        thread: ThreadShell,
    },
    #[serde(rename = "thread.removed")]
    ThreadRemoved {
        #[serde(deserialize_with = "deserialize_nonnegative_u64")]
        sequence: u64,
        location: ThreadLocation,
        thread_id: ThreadId,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationMessage {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub notification: Option<Option<Notification>>,
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
    pub delegated_completion: Option<Option<DelegatedCompletionMessage>>,

    pub created_by: Actor,
    pub creation_source: CreationSource,
    pub id: MessageId,
    pub thread_id: ThreadId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub run_id: Option<RunId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub node_id: Option<NodeId>,
    pub role: MessageRole,
    pub text: String,
    pub attachments: Vec<ChatAttachment>,
    pub streaming: bool,
    pub created_at: UtcDateTime,
    pub updated_at: UtcDateTime,
    #[serde(flatten, skip_serializing)]
    pub extra: ExtraFields,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeRequestKind {
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
    #[serde(rename = "dynamic_tool_call")]
    DynamicToolCall,
    #[serde(rename = "user_input")]
    UserInput,
    #[serde(rename = "auth_refresh")]
    AuthRefresh,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeRequestStatus {
    Pending,
    Resolved,
    Expired,
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRequest {
    pub id: RuntimeRequestId,
    pub node_id: NodeId,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provider_turn_id: Option<ProviderTurnId>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub native_request_ref: Option<ProviderRef>,
    pub kind: RuntimeRequestKind,
    pub status: RuntimeRequestStatus,
    pub response_capability: RuntimeResponseCapability,
    pub created_at: UtcDateTime,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub resolved_at: Option<UtcDateTime>,
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
    #[serde(flatten, skip_serializing)]
    pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadProjection {
    pub thread: AppThread,
    pub runs: Vec<Run>,
    pub attempts: Vec<RunAttempt>,
    pub nodes: Vec<ExecutionNode>,
    pub subagents: Vec<Subagent>,
    pub provider_sessions: Vec<ProviderSessionV2>,
    pub provider_threads: Vec<ProviderThread>,
    pub provider_turns: Vec<ProviderTurn>,
    pub runtime_requests: Vec<RuntimeRequest>,
    pub messages: Vec<ConversationMessage>,
    pub plans: Vec<PlanArtifact>,
    #[serde(deserialize_with = "deserialize_turn_item_array")]
    pub turn_items: Vec<TurnItem>,
    pub checkpoint_scopes: Vec<CheckpointScope>,
    pub checkpoints: Vec<Checkpoint>,
    pub context_handoffs: Vec<ContextHandoff>,
    pub context_transfers: Vec<ContextTransfer>,
    #[serde(deserialize_with = "deserialize_projected_turn_item_array")]
    pub visible_turn_items: Vec<ProjectedTurnItem>,
    pub updated_at: UtcDateTime,
    #[serde(flatten, skip_serializing)]
    pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "DomainEventWire")]
pub struct DomainEvent {
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
    pub node_id: Option<Option<NodeId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub driver: Option<Option<ProviderDriverKind>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub provider_instance_id: Option<Option<ProviderInstanceId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub raw_event_id: Option<Option<RawEventId>>,

    pub id: EventId,
    pub thread_id: ThreadId,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
    pub occurred_at: UtcDateTime,
    #[serde(flatten, skip_serializing)]
    pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
pub enum ThreadStreamItem {
    #[serde(rename = "synchronized")]
    Synchronized,
    #[serde(rename = "snapshot")]
    Snapshot {
        #[serde(deserialize_with = "deserialize_nonnegative_u64")]
        snapshot_sequence: u64,
        projection: ThreadProjection,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        history_cursor: Option<Option<TrimmedNonEmptyString>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        has_more_history: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        latest_local_turn_ordinal: Option<Option<u64>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        payload_budget_exceeded: Option<bool>,
    },
    #[serde(rename = "event")]
    Event { sequence: u64, event: DomainEvent },
    #[serde(skip_serializing)]
    UnknownEvent { sequence: u64, event_type: String },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
enum ThreadStreamWire {
    #[serde(rename = "synchronized")]
    Synchronized,
    #[serde(rename = "snapshot")]
    Snapshot {
        #[serde(deserialize_with = "deserialize_nonnegative_u64")]
        snapshot_sequence: u64,
        projection: ThreadProjection,
        #[serde(default, deserialize_with = "deserialize_optional")]
        history_cursor: Option<Option<TrimmedNonEmptyString>>,
        #[serde(default, deserialize_with = "deserialize_optional")]
        has_more_history: Option<bool>,
        #[serde(default, deserialize_with = "deserialize_optional_nonnegative_u64")]
        latest_local_turn_ordinal: Option<Option<u64>>,
        #[serde(default, deserialize_with = "deserialize_optional")]
        payload_budget_exceeded: Option<bool>,
    },
}

impl<'de> Deserialize<'de> for ThreadStreamItem {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(d)?;
        if raw.get("kind").and_then(Value::as_str) == Some("event") {
            let sequence: u64 = serde_json::from_value::<NonNegativeInt>(
                raw.get("sequence")
                    .cloned()
                    .ok_or_else(|| serde::de::Error::missing_field("sequence"))?,
            )
            .map_err(serde::de::Error::custom)?
            .0;
            let event = raw
                .get("event")
                .ok_or_else(|| serde::de::Error::missing_field("event"))?;
            let event_type = event
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| serde::de::Error::custom("an event must have a string type"))?;
            let unknown_turn_item = event_type == "turn-item.updated"
                && event
                    .get("payload")
                    .and_then(|v| v.get("type"))
                    .is_some_and(|tag| {
                        !tag.as_str()
                            .is_some_and(|t| KNOWN_TURN_ITEM_TYPES.contains(&t))
                    });
            if !KNOWN_EVENT_TYPES.contains(&event_type) || unknown_turn_item {
                return Ok(Self::UnknownEvent {
                    sequence,
                    event_type: event_type.to_owned(),
                });
            }
            let event: DomainEvent =
                serde_json::from_value(event.clone()).map_err(serde::de::Error::custom)?;
            validate_event_payload(&event).map_err(serde::de::Error::custom)?;
            return Ok(Self::Event { sequence, event });
        }
        match serde_json::from_value::<ThreadStreamWire>(raw).map_err(serde::de::Error::custom)? {
            ThreadStreamWire::Synchronized => Ok(Self::Synchronized),
            ThreadStreamWire::Snapshot {
                snapshot_sequence,
                projection,
                history_cursor,
                has_more_history,
                latest_local_turn_ordinal,
                payload_budget_exceeded,
            } => {
                // Null is a legitimate latest ordinal in the original wire schema.
                Ok(Self::Snapshot {
                    snapshot_sequence,
                    projection,
                    history_cursor,
                    has_more_history,
                    latest_local_turn_ordinal,
                    payload_budget_exceeded,
                })
            }
        }
    }
}

pub fn validate_event_payload(event: &DomainEvent) -> Result<(), serde_json::Error> {
    normalize_event_payload(&event.event_type, event.payload.clone()).map(|_| ())
}
/// Validate and canonicalize every known domain payload using its source-backed codec.
pub fn normalize_event_payload(
    event_type: &str,
    payload: Value,
) -> Result<Value, serde_json::Error> {
    fn normalize<T: serde::de::DeserializeOwned + Serialize>(
        payload: Value,
    ) -> Result<Value, serde_json::Error> {
        serde_json::to_value(serde_json::from_value::<T>(payload)?)
    }
    match event_type {
        t if t.starts_with("thread.") && KNOWN_EVENT_TYPES.contains(&t) => {
            normalize::<AppThread>(payload)
        }
        "run.created" | "run.updated" => normalize::<Run>(payload),
        "run.background-work-cancelled" => normalize::<RunBackgroundWorkCancelled>(payload),
        "run-attempt.created" | "run-attempt.updated" => normalize::<RunAttempt>(payload),
        "node.updated" => normalize::<ExecutionNode>(payload),
        "subagent.updated" => normalize::<Subagent>(payload),
        "provider-session.attached" | "provider-session.updated" => {
            normalize::<ProviderSessionV2>(payload)
        }
        "provider-session.detached" => normalize::<ProviderSessionDetached>(payload),
        "provider-thread.updated" => normalize::<ProviderThread>(payload),
        "provider-turn.updated" => normalize::<ProviderTurn>(payload),
        "runtime-request.updated" => normalize::<RuntimeRequest>(payload),
        "message.updated" => normalize::<ConversationMessage>(payload),
        "turn-item.updated" => normalize::<TurnItem>(payload),
        "plan.updated" => normalize::<PlanArtifact>(payload),
        "checkpoint-scope.created" => normalize::<CheckpointScope>(payload),
        "checkpoint.captured" => normalize::<Checkpoint>(payload),
        "checkpoint.rollback-requested" => normalize::<CheckpointRollbackRequest>(payload),
        "context-handoff.updated" => normalize::<ContextHandoff>(payload),
        "context-transfer.created" | "context-transfer.updated" => {
            normalize::<ContextTransfer>(payload)
        }
        _ => Err(<serde_json::Error as serde::de::Error>::custom(
            "unknown domain event type",
        )),
    }
}
fn deserialize_turn_item_array<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<TurnItem>, D::Error> {
    decode_forward_union_array(Value::deserialize(d)?, "type", KNOWN_TURN_ITEM_TYPES)
        .map_err(serde::de::Error::custom)
}
fn deserialize_projected_turn_item_array<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<ProjectedTurnItem>, D::Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        position: NonNegativeInt,
        visibility: ProjectedTurnItemVisibility,
        source_thread_id: ThreadId,
        source_item_id: TurnItemId,
        item: Value,
    }
    let rows = Vec::<Wire>::deserialize(d)?;
    rows.into_iter()
        .filter(|row| {
            row.item
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|kind| KNOWN_TURN_ITEM_TYPES.contains(&kind))
        })
        .map(|row| {
            Ok(ProjectedTurnItem {
                position: row.position,
                visibility: row.visibility,
                source_thread_id: row.source_thread_id,
                source_item_id: row.source_item_id,
                item: serde_json::from_value(row.item).map_err(serde::de::Error::custom)?,
            })
        })
        .collect()
}

pub const KNOWN_TURN_ITEM_TYPES: &[&str] = &[
    "notification",
    "user_message",
    "assistant_message",
    "reasoning",
    "proposed_plan",
    "todo_list",
    "user_input_request",
    "file_change",
    "command_execution",
    "file_search",
    "web_search",
    "approval_request",
    "checkpoint",
    "run_interrupt_request",
    "run_interrupt_result",
    "system_notice",
    "error",
    "compaction",
    "handoff",
    "fork",
    "thread_created",
    "secret_request",
    "subagent",
    "dynamic_tool",
];
pub const KNOWN_EVENT_TYPES: &[&str] = &[
    "thread.created",
    "thread.archived",
    "thread.unarchived",
    "thread.deleted",
    "thread.settled",
    "thread.unsettled",
    "thread.snoozed",
    "thread.unsnoozed",
    "thread.pinned",
    "thread.auto-settle-set",
    "thread.unpinned",
    "thread.pin-reordered",
    "thread.active-reordered",
    "thread.visited",
    "thread.marked-unread",
    "thread.pull-request-synced",
    "thread.metadata-updated",
    "thread.runtime-mode-updated",
    "thread.interaction-mode-updated",
    "thread.model-selection-updated",
    "thread.provider-switched",
    "run.created",
    "run.updated",
    "run.background-work-cancelled",
    "run-attempt.created",
    "run-attempt.updated",
    "node.updated",
    "subagent.updated",
    "provider-session.attached",
    "provider-session.updated",
    "provider-session.detached",
    "provider-thread.updated",
    "provider-turn.updated",
    "runtime-request.updated",
    "message.updated",
    "turn-item.updated",
    "plan.updated",
    "checkpoint-scope.created",
    "checkpoint.captured",
    "checkpoint.rollback-requested",
    "context-handoff.updated",
    "context-transfer.created",
    "context-transfer.updated",
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DomainEventWire {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    run_id: Option<Option<RunId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    node_id: Option<Option<NodeId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    driver: Option<Option<ProviderDriverKind>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    provider_instance_id: Option<Option<ProviderInstanceId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    raw_event_id: Option<Option<RawEventId>>,

    id: EventId,
    thread_id: ThreadId,
    #[serde(rename = "type")]
    event_type: String,
    payload: Value,
    occurred_at: UtcDateTime,
    #[serde(flatten)]
    extra: ExtraFields,
}
impl TryFrom<DomainEventWire> for DomainEvent {
    type Error = serde_json::Error;
    fn try_from(wire: DomainEventWire) -> Result<Self, Self::Error> {
        let payload = normalize_event_payload(&wire.event_type, wire.payload)?;
        Ok(Self {
            id: wire.id,
            thread_id: wire.thread_id,
            event_type: wire.event_type,
            payload,
            occurred_at: wire.occurred_at,
            run_id: wire.run_id,
            node_id: wire.node_id,
            driver: wire.driver,
            provider_instance_id: wire.provider_instance_id,
            raw_event_id: wire.raw_event_id,
            extra: wire.extra,
        })
    }
}
