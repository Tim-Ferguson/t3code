//! Thread lifecycle support and command payloads.
use crate::*;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeRefStrength {
    #[serde(rename = "strong")]
    Strong,
    #[serde(rename = "weak")]
    Weak,
    #[serde(rename = "none")]
    None,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrongNativeRef {
    #[serde(rename = "strong")]
    Strong,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRef {
    pub driver: ProviderDriverKind,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub native_id: Option<TrimmedNonEmptyString>,
    pub strength: NativeRefStrength,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub fingerprint: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub ordinal: Option<Option<NonNegativeInt>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedNativeThreadRef {
    pub driver: ProviderDriverKind,
    pub native_id: TrimmedNonEmptyString,
    pub strength: StrongNativeRef,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderThreadNativeMetadata {
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
    pub title: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub updated_at: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub item_identity_version: Option<Option<LiteralInt<2>>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedNativeThread {
    pub r#ref: ImportedNativeThreadRef,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub metadata: Option<Option<ProviderThreadNativeMetadata>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadHistoryOrigin {
    #[serde(rename = "native")]
    Native,
    #[serde(rename = "v1_import")]
    V1Import,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullRequestState {
    #[serde(rename = "open")]
    Open,
    #[serde(rename = "closed")]
    Closed,
    #[serde(rename = "merged")]
    Merged,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullRequestReviewDecision {
    #[serde(rename = "approved")]
    Approved,
    #[serde(rename = "changes-requested")]
    ChangesRequested,
    #[serde(rename = "review-required")]
    ReviewRequired,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullRequestChecksState {
    #[serde(rename = "passing")]
    Passing,
    #[serde(rename = "failing")]
    Failing,
    #[serde(rename = "pending")]
    Pending,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullRequestMergeability {
    #[serde(rename = "mergeable")]
    Mergeable,
    #[serde(rename = "conflicting")]
    Conflicting,
    #[serde(rename = "unknown")]
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadPullRequestLinkSource {
    #[serde(rename = "manual")]
    Manual,
    #[serde(rename = "created")]
    Created,
    #[serde(rename = "agent")]
    Agent,
    #[serde(rename = "stack")]
    Stack,
    #[serde(rename = "stack-dismissed")]
    StackDismissed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadPullRequestStackKind {
    #[serde(rename = "native")]
    Native,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestActor {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub is_bot: Option<Option<bool>>,
    pub login: TrimmedNonEmptyString,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub name: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub avatar_url: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadLinkedPullRequest {
    pub project_id: ProjectId,
    pub repository: TrimmedNonEmptyString,
    pub number: PositiveInt,
    pub url: TrimmedNonEmptyString,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPullRequestSnapshot {
    pub state: PullRequestState,
    pub title: TrimmedNonEmptyString,
    pub head_branch: TrimmedNonEmptyString,
    pub base_branch: TrimmedNonEmptyString,
    pub is_draft: bool,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub updated_at: Option<IsoDateTime>,
    pub synced_at: IsoDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub closed_at: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub merged_at: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub author: Option<Option<PullRequestActor>>,
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
    pub changed_files: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub review_decision: Option<Option<PullRequestReviewDecision>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub checks_state: Option<Option<PullRequestChecksState>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub mergeability: Option<Option<PullRequestMergeability>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPullRequestStackLayer {
    pub number: PositiveInt,
    pub head_branch: TrimmedNonEmptyString,
    pub state: PullRequestState,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPullRequestStack {
    pub kind: ThreadPullRequestStackKind,
    pub id: TrimmedNonEmptyString,
    pub number: PositiveInt,
    pub url: TrimmedNonEmptyString,
    pub base: TrimmedNonEmptyString,
    pub layers: Vec<ThreadPullRequestStackLayer>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPullRequestWatch {
    pub started_at: IsoDateTime,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub head_sha: Option<TrimmedNonEmptyString>,
    pub failed_checks: Vec<TrimmedNonEmptyString>,
    pub passed: bool,
    #[serde(default, deserialize_with = "crate::provider::deserialize_default_vec")]
    pub passed_checks: Vec<TrimmedNonEmptyString>,
    pub remarks_through: IsoDateTime,
    pub remark_ids: Vec<TrimmedNonEmptyString>,
    pub conflicting: bool,
    pub wakes: NonNegativeInt,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPullRequestLink {
    pub host: TrimmedNonEmptyString,
    pub repository: TrimmedNonEmptyString,
    pub number: PositiveInt,
    pub url: TrimmedNonEmptyString,
    pub source: ThreadPullRequestLinkSource,
    pub linked_at: IsoDateTime,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub snapshot: Option<ThreadPullRequestSnapshot>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub stack: Option<ThreadPullRequestStack>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub watch: Option<Option<ThreadPullRequestWatch>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitRecovery {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub request_id: Option<Option<CommandId>>,
    pub run_id: RunId,
    pub reset_at: IsoDateTime,
    pub auto_resume: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snooze: Option<Option<bool>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitRecoveryUpdateWire {
    pub run_id: RunId,
    pub reset_at: IsoDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub auto_resume: Option<Option<bool>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snooze: Option<Option<bool>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleRegeneration {
    pub request_id: CommandId,
    pub started_at: UtcDateTime,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackFailure {
    pub request_id: CommandId,
    pub message: TrimmedNonEmptyString,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCreateCommand {
    pub created_by: Actor,
    pub creation_source: CreationSource,
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub project_id: ProjectId,
    pub title: TrimmedNonEmptyString,
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: ProviderInteractionMode,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub branch: Option<TrimmedNonEmptyString>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub worktree_path: Option<TrimmedNonEmptyString>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub imported_native_thread: Option<Option<ImportedNativeThread>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadMetadataUpdateCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub title: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub regenerate_title: Option<Option<bool>>,
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
    pub worktree_path: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub expected_worktree_path: Option<Option<TrimmedNonEmptyString>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub expected_empty: Option<Option<bool>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub limit_recovery: Option<Option<LimitRecoveryUpdate>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub linked_pull_request: Option<Option<ThreadLinkedPullRequest>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadIdentityCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSettleCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub settled_at: Option<Option<UtcDateTime>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadAutoSettleCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub snapshot_at: UtcDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub settled_at: Option<Option<UtcDateTime>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserReason {
    #[serde(rename = "user")]
    User,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUserReasonCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub reason: UserReason,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSnoozeCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub snoozed_until: IsoDateTime,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadAutoSettleSetCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub enabled: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPinCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub order_key: Option<Option<TrimmedNonEmptyString>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadOrderCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub order_key: TrimmedNonEmptyString,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadVisitCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub visited_at: IsoDateTime,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRuntimeModeCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub runtime_mode: RuntimeMode,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadInteractionModeCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub interaction_mode: ProviderInteractionMode,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadModelSelectionCommand {
    pub command_id: CommandId,
    pub thread_id: ThreadId,
    pub model_selection: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "LimitRecoveryUpdateWire", into = "LimitRecoveryUpdateWire")]
pub struct LimitRecoveryUpdate {
    pub run_id: RunId,
    pub reset_at: IsoDateTime,
    pub auto_resume: Option<Option<bool>>,
    pub snooze: Option<Option<bool>>,
}
impl TryFrom<LimitRecoveryUpdateWire> for LimitRecoveryUpdate {
    type Error = ValidationError;
    fn try_from(value: LimitRecoveryUpdateWire) -> Result<Self, Self::Error> {
        if value.auto_resume.flatten().is_none() && value.snooze.flatten().is_none() {
            return Err(ValidationError {
                expected: "a recovery update with autoResume or snooze",
            });
        }
        Ok(Self {
            run_id: value.run_id,
            reset_at: value.reset_at,
            auto_resume: value.auto_resume,
            snooze: value.snooze,
        })
    }
}
impl From<LimitRecoveryUpdate> for LimitRecoveryUpdateWire {
    fn from(v: LimitRecoveryUpdate) -> Self {
        Self {
            run_id: v.run_id,
            reset_at: v.reset_at,
            auto_resume: v.auto_resume,
            snooze: v.snooze,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ThreadForkSource {
    #[serde(rename = "run", rename_all = "camelCase")]
    Run { thread_id: ThreadId, run_id: RunId },
    #[serde(rename = "node", rename_all = "camelCase")]
    Node { node_id: NodeId },
    #[serde(rename = "provider_thread", rename_all = "camelCase")]
    ProviderThread {
        provider_thread_id: ProviderThreadId,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        provider_turn_id: Option<Option<ProviderTurnId>>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ThreadCommand {
    #[serde(rename = "thread.create")]
    Create(ThreadCreateCommand),
    #[serde(rename = "thread.archive")]
    Archive(ThreadIdentityCommand),
    #[serde(rename = "thread.unarchive")]
    Unarchive(ThreadIdentityCommand),
    #[serde(rename = "thread.delete")]
    Delete(ThreadIdentityCommand),
    #[serde(rename = "thread.settle")]
    Settle(ThreadSettleCommand),
    #[serde(rename = "thread.auto-settle")]
    AutoSettle(ThreadAutoSettleCommand),
    #[serde(rename = "thread.unsettle")]
    Unsettle(ThreadUserReasonCommand),
    #[serde(rename = "thread.snooze")]
    Snooze(ThreadSnoozeCommand),
    #[serde(rename = "thread.unsnooze")]
    Unsnooze(ThreadUserReasonCommand),
    #[serde(rename = "thread.auto-settle.set")]
    AutoSettleSet(ThreadAutoSettleSetCommand),
    #[serde(rename = "thread.pin")]
    Pin(ThreadPinCommand),
    #[serde(rename = "thread.unpin")]
    Unpin(ThreadIdentityCommand),
    #[serde(rename = "thread.pin.reorder")]
    PinReorder(ThreadOrderCommand),
    #[serde(rename = "thread.active.reorder")]
    ActiveReorder(ThreadOrderCommand),
    #[serde(rename = "thread.visit")]
    Visit(ThreadVisitCommand),
    #[serde(rename = "thread.mark-unread")]
    MarkUnread(ThreadIdentityCommand),
    #[serde(rename = "thread.metadata.update")]
    MetadataUpdate(ThreadMetadataUpdateCommand),
    #[serde(rename = "thread.runtime-mode.set")]
    RuntimeModeSet(ThreadRuntimeModeCommand),
    #[serde(rename = "thread.interaction-mode.set")]
    InteractionModeSet(ThreadInteractionModeCommand),
    #[serde(rename = "thread.model-selection.set")]
    ModelSelectionSet(ThreadModelSelectionCommand),
}
impl ThreadCommand {
    /// Convert transport nulls representing undefined scalar options to the
    /// source's persisted command representation. Nullable mutation fields
    /// remain null so an explicit clear cannot turn into an omitted edit.
    pub fn service_payload(&self) -> Result<serde_json::Value, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        let scalar_optional: &[&str] = match self {
            Self::Create(_) => &["importedNativeThread"],
            Self::MetadataUpdate(_) => &["title", "regenerateTitle", "expectedEmpty"],
            Self::Settle(_) | Self::AutoSettle(_) => &["settledAt"],
            Self::Pin(_) => &["orderKey"],
            _ => &[],
        };
        fn remove_null(value: &mut serde_json::Value, fields: &[&str]) {
            if let Some(object) = value.as_object_mut() {
                for field in fields {
                    if object.get(*field).is_some_and(serde_json::Value::is_null) {
                        object.remove(*field);
                    }
                }
            }
        }
        remove_null(&mut value, scalar_optional);
        if let Some(recovery) = value.get_mut("limitRecovery") {
            remove_null(recovery, &["autoResume", "snooze"]);
        }
        if let Some(metadata) = value
            .get_mut("importedNativeThread")
            .and_then(|v| v.get_mut("metadata"))
        {
            remove_null(metadata, &["modelSelection", "itemIdentityVersion"]);
        }
        Ok(value)
    }
    pub fn command_id(&self) -> &CommandId {
        match self {
            Self::Create(c) => &c.command_id,
            Self::Archive(c)
            | Self::Unarchive(c)
            | Self::Delete(c)
            | Self::Unpin(c)
            | Self::MarkUnread(c) => &c.command_id,
            Self::Settle(c) => &c.command_id,
            Self::AutoSettle(c) => &c.command_id,
            Self::Unsettle(c) | Self::Unsnooze(c) => &c.command_id,
            Self::Snooze(c) => &c.command_id,
            Self::AutoSettleSet(c) => &c.command_id,
            Self::Pin(c) => &c.command_id,
            Self::PinReorder(c) | Self::ActiveReorder(c) => &c.command_id,
            Self::Visit(c) => &c.command_id,
            Self::MetadataUpdate(c) => &c.command_id,
            Self::RuntimeModeSet(c) => &c.command_id,
            Self::InteractionModeSet(c) => &c.command_id,
            Self::ModelSelectionSet(c) => &c.command_id,
        }
    }
    pub fn thread_id(&self) -> &ThreadId {
        match self {
            Self::Create(c) => &c.thread_id,
            Self::Archive(c)
            | Self::Unarchive(c)
            | Self::Delete(c)
            | Self::Unpin(c)
            | Self::MarkUnread(c) => &c.thread_id,
            Self::Settle(c) => &c.thread_id,
            Self::AutoSettle(c) => &c.thread_id,
            Self::Unsettle(c) | Self::Unsnooze(c) => &c.thread_id,
            Self::Snooze(c) => &c.thread_id,
            Self::AutoSettleSet(c) => &c.thread_id,
            Self::Pin(c) => &c.thread_id,
            Self::PinReorder(c) | Self::ActiveReorder(c) => &c.thread_id,
            Self::Visit(c) => &c.thread_id,
            Self::MetadataUpdate(c) => &c.thread_id,
            Self::RuntimeModeSet(c) => &c.thread_id,
            Self::InteractionModeSet(c) => &c.thread_id,
            Self::ModelSelectionSet(c) => &c.thread_id,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderGoalStatus {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
    #[serde(rename = "blocked")]
    Blocked,
    #[serde(rename = "usage_limited")]
    UsageLimited,
    #[serde(rename = "budget_limited")]
    BudgetLimited,
    #[serde(rename = "complete")]
    Complete,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderGoal {
    pub objective: TrimmedNonEmptyString,
    pub status: ProviderGoalStatus,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub tokens_used: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub token_budget: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub time_used_seconds: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub checks: Option<Option<NonNegativeInt>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub last_check: Option<Option<String>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderFailureClass {
    #[serde(rename = "usage_limit")]
    UsageLimit,
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
pub enum ActivityRunStatus {
    #[serde(rename = "preparing")]
    Preparing,
    #[serde(rename = "starting")]
    Starting,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "waiting")]
    Waiting,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
pub enum PendingBackgroundTask {
    #[serde(rename = "subagent")]
    Subagent {
        task_id: TrimmedNonEmptyString,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<Option<TrimmedNonEmptyString>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        child_thread_id: Option<Option<ThreadId>>,
    },
    #[serde(rename = "command")]
    Command {
        task_id: TrimmedNonEmptyString,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<Option<TrimmedNonEmptyString>>,
    },
    #[serde(rename = "monitor")]
    Monitor {
        task_id: TrimmedNonEmptyString,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<Option<TrimmedNonEmptyString>>,
    },
    #[serde(rename = "background_task")]
    BackgroundTask {
        task_id: TrimmedNonEmptyString,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<Option<TrimmedNonEmptyString>>,
    },
}
impl<'de> Deserialize<'de> for PendingBackgroundTask {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(d)?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Fields {
            task_id: TrimmedNonEmptyString,
            #[serde(default, deserialize_with = "deserialize_optional")]
            kind: Option<Option<String>>,
            #[serde(default, deserialize_with = "deserialize_optional")]
            description: Option<Option<TrimmedNonEmptyString>>,
        }
        let fields: Fields =
            serde_json::from_value(raw.clone()).map_err(serde::de::Error::custom)?;
        let Fields {
            task_id,
            kind,
            description,
        } = fields;
        Ok(match kind.flatten().as_deref() {
            Some("subagent") => {
                let child_thread_id = raw
                    .get("childThreadId")
                    .map(|v| serde_json::from_value::<Option<ThreadId>>(v.clone()))
                    .transpose()
                    .map_err(serde::de::Error::custom)?;
                Self::Subagent {
                    task_id,
                    description,
                    child_thread_id,
                }
            }
            Some("command") => Self::Command {
                task_id,
                description,
            },
            Some("monitor") => Self::Monitor {
                task_id,
                description,
            },
            Some("background_task") => Self::BackgroundTask {
                task_id,
                description,
            },
            _ => Self::BackgroundTask {
                task_id,
                description: description.flatten().map(Some),
            },
        })
    }
}

/// Source points accepted by thread.fork and thread.merge_back commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ThreadForkSourcePoint {
    #[serde(rename = "latest_stable")]
    LatestStable,
    #[serde(rename = "run", rename_all = "camelCase")]
    Run { run_id: RunId },
    #[serde(rename = "checkpoint", rename_all = "camelCase")]
    Checkpoint { checkpoint_id: CheckpointId },
}
