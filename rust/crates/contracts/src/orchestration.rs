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
    pub forked_from: Option<Value>,
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
    pub forked_from: Option<Value>,
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
    #[serde(default)]
    pub pending_background_tasks: Vec<Value>,
    #[serde(default)]
    pub provider_instance_history: Vec<ProviderInstanceId>,
    pub item_count: u64,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSnapshot {
    #[serde(deserialize_with = "positive_schema_version")]
    pub schema_version: u64,
    pub snapshot_sequence: u64,
    pub projects: Vec<ProjectShell>,
    pub threads: Vec<ThreadShell>,
    pub archived_threads: Vec<ThreadShell>,
}
fn positive_schema_version<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let version = u64::deserialize(d)?;
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
        sequence: u64,
        project: ProjectShell,
    },
    #[serde(rename = "project.removed")]
    ProjectRemoved {
        sequence: u64,
        project_id: ProjectId,
    },
    #[serde(rename = "thread.updated")]
    ThreadUpdated {
        sequence: u64,
        location: ThreadLocation,
        thread: ThreadShell,
    },
    #[serde(rename = "thread.removed")]
    ThreadRemoved {
        sequence: u64,
        location: ThreadLocation,
        thread_id: ThreadId,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationMessage {
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
    pub attachments: Vec<Value>,
    pub streaming: bool,
    pub created_at: UtcDateTime,
    pub updated_at: UtcDateTime,
    #[serde(flatten)]
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
    pub native_request_ref: Option<Value>,
    pub kind: RuntimeRequestKind,
    pub status: RuntimeRequestStatus,
    pub response_capability: Value,
    pub created_at: UtcDateTime,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub resolved_at: Option<UtcDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub decision: Option<ProviderApprovalDecision>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub answers: Option<BTreeMap<String, Value>>,
    #[serde(flatten)]
    pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadProjection {
    pub thread: AppThread,
    pub runs: Vec<Value>,
    pub attempts: Vec<Value>,
    pub nodes: Vec<Value>,
    pub subagents: Vec<Value>,
    pub provider_sessions: Vec<Value>,
    pub provider_threads: Vec<Value>,
    pub provider_turns: Vec<Value>,
    pub runtime_requests: Vec<RuntimeRequest>,
    pub messages: Vec<ConversationMessage>,
    pub plans: Vec<Value>,
    pub turn_items: Vec<Value>,
    pub checkpoint_scopes: Vec<Value>,
    pub checkpoints: Vec<Value>,
    pub context_handoffs: Vec<Value>,
    pub context_transfers: Vec<Value>,
    pub visible_turn_items: Vec<Value>,
    pub updated_at: UtcDateTime,
    #[serde(flatten)]
    pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainEvent {
    pub id: EventId,
    pub thread_id: ThreadId,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
    pub occurred_at: UtcDateTime,
    #[serde(flatten)]
    pub extra: ExtraFields,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
pub enum ThreadStreamItem {
    #[serde(rename = "synchronized")]
    Synchronized,
    #[serde(rename = "snapshot")]
    Snapshot {
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
        snapshot_sequence: u64,
        projection: ThreadProjection,
        #[serde(default, deserialize_with = "deserialize_optional")]
        history_cursor: Option<Option<TrimmedNonEmptyString>>,
        #[serde(default, deserialize_with = "deserialize_optional")]
        has_more_history: Option<bool>,
        #[serde(default, deserialize_with = "deserialize_optional")]
        latest_local_turn_ordinal: Option<Option<u64>>,
        #[serde(default, deserialize_with = "deserialize_optional")]
        payload_budget_exceeded: Option<bool>,
    },
}

impl<'de> Deserialize<'de> for ThreadStreamItem {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(d)?;
        if raw.get("kind").and_then(Value::as_str) == Some("event") {
            let sequence: u64 = serde_json::from_value(
                raw.get("sequence")
                    .cloned()
                    .ok_or_else(|| serde::de::Error::missing_field("sequence"))?,
            )
            .map_err(serde::de::Error::custom)?;
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
                    .is_some_and(|v| {
                        !v.as_str()
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

fn validate_event_payload(event: &DomainEvent) -> Result<(), serde_json::Error> {
    match event.event_type.as_str() {
        t if t.starts_with("thread.") => {
            serde_json::from_value::<AppThread>(event.payload.clone())?;
        }
        "message.updated" => {
            serde_json::from_value::<ConversationMessage>(event.payload.clone())?;
        }
        "runtime-request.updated" => {
            serde_json::from_value::<RuntimeRequest>(event.payload.clone())?;
        }
        "provider-session.detached" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Detached {
                #[serde(rename = "providerSessionId")]
                _provider_session_id: ProviderSessionId,
                #[serde(rename = "detachedAt")]
                _detached_at: UtcDateTime,
            }
            serde_json::from_value::<Detached>(event.payload.clone())?;
        }
        // Other payload contracts still require a typed port. Their JSON is retained.
        _ => {}
    }
    Ok(())
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
