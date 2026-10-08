//! Cooperative thread-control schemas from orchestratorMcp/threadMetadataMcp.
use crate::{history::object_struct, *};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct McpPositiveInt<const MAX: u64>(pub u64);
impl<'de, const MAX: u64> Deserialize<'de> for McpPositiveInt<MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = PositiveInt::deserialize(d)?.0;
        if value <= MAX {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom("integer exceeds limit"))
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct McpThreadStatuses(pub Vec<McpThreadStatus>);
impl<'de> Deserialize<'de> for McpThreadStatuses {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let values = Vec::<McpThreadStatus>::deserialize(d)?;
        if values.len() <= 10 {
            Ok(Self(values))
        } else {
            Err(serde::de::Error::custom("at most ten statuses"))
        }
    }
}
macro_rules! vocabulary{($name:ident{$($variant:ident=>$value:literal),+$(,)?})=>{#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]pub enum $name{$(#[serde(rename=$value)]$variant,)+}};}
vocabulary! {McpThreadStatus{Idle=>"idle",Preparing=>"preparing",Queued=>"queued",Starting=>"starting",Running=>"running",Waiting=>"waiting",Completed=>"completed",Failed=>"failed",Cancelled=>"cancelled",Interrupted=>"interrupted",RolledBack=>"rolled_back"}}
vocabulary! {McpThreadView{Messages=>"messages",Activity=>"activity"}}
vocabulary! {McpThreadSendMode{Auto=>"auto",Queue=>"queue",Steer=>"steer",Restart=>"restart"}}
vocabulary! {McpThreadDelivery{Started=>"started",Queued=>"queued",Steered=>"steered",Restarted=>"restarted"}}
vocabulary! {ThreadMetadataMcpAction{Rename=>"rename",RegenerateTitle=>"regenerate_title",LinkPullRequest=>"link_pull_request",UnlinkPullRequest=>"unlink_pull_request"}}
object_struct! {pub struct OrchestratorMcpThreadListInput {
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub project_id:Option<Option<ProjectId>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub statuses:Option<Option<McpThreadStatuses>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub title_contains:Option<Option<BoundedTrimmedString<256>>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub settled:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub snoozed:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub include_subagents:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub cursor:Option<Option<NonNegativeInt>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub limit:Option<Option<McpPositiveInt<100>>>,
}}
object_struct! {pub struct OrchestratorMcpThreadReadInput {
pub thread_id:ThreadId,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub item_id:Option<Option<TurnItemId>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub text_offset:Option<Option<NonNegativeInt>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub view:Option<Option<McpThreadView>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub after_position:Option<Option<NonNegativeInt>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub limit:Option<Option<McpPositiveInt<100>>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub run_limit:Option<Option<McpPositiveInt<50>>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub max_chars_per_item:Option<Option<McpPositiveInt<50000>>>,
}}
object_struct! {pub struct OrchestratorMcpThreadSendInput {
pub thread_id:ThreadId,
pub message:BoundedTrimmedString<120000>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub mode:Option<Option<McpThreadSendMode>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub client_request_id:Option<Option<BoundedTrimmedString<256>>>,
}}
object_struct! {pub struct OrchestratorMcpThreadWaitInput {
pub thread_id:ThreadId,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub run_id:Option<Option<RunId>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub timeout_ms:Option<Option<serde_json::Number>>,
}}
object_struct! {pub struct OrchestratorMcpThreadInterruptInput {
pub thread_id:ThreadId,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub run_id:Option<Option<RunId>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub reason:Option<Option<BoundedString<2000>>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub client_request_id:Option<Option<BoundedTrimmedString<256>>>,
}}
object_struct! {pub struct ThreadMetadataMcpPullRequest {
pub repository:TrimmedNonEmptyString,
pub number:PositiveInt,
pub url:McpHttpUrl,
}}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct McpHttpUrl(pub TrimmedNonEmptyString);
impl<'de> Deserialize<'de> for McpHttpUrl {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = TrimmedNonEmptyString::deserialize(d)?;
        let parsed = url::Url::parse(value.as_str()).map_err(serde::de::Error::custom)?;
        if matches!(parsed.scheme(), "http" | "https") {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom("expected HTTP(S) URL"))
        }
    }
}
object_struct! {pub struct ThreadMetadataMcpUpdateFields {
pub action:ThreadMetadataMcpAction,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub thread_id:Option<Option<ThreadId>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub title:Option<Option<BoundedTrimmedString<512>>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub pull_request:Option<Option<ThreadMetadataMcpPullRequest>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub client_request_id:Option<Option<BoundedTrimmedString<256>>>,
}}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ThreadMetadataMcpUpdateInput(pub ThreadMetadataMcpUpdateFields);
impl<'de> Deserialize<'de> for ThreadMetadataMcpUpdateInput {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = ThreadMetadataMcpUpdateFields::deserialize(d)?;
        let valid = match value.action {
            ThreadMetadataMcpAction::Rename => {
                value.title.as_ref().and_then(Option::as_ref).is_some()
                    && value
                        .pull_request
                        .as_ref()
                        .and_then(Option::as_ref)
                        .is_none()
            }
            ThreadMetadataMcpAction::LinkPullRequest => {
                value.title.as_ref().and_then(Option::as_ref).is_none()
                    && value
                        .pull_request
                        .as_ref()
                        .and_then(Option::as_ref)
                        .is_some()
            }
            _ => {
                value.title.as_ref().and_then(Option::as_ref).is_none()
                    && value
                        .pull_request
                        .as_ref()
                        .and_then(Option::as_ref)
                        .is_none()
            }
        };
        if valid {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "fields do not match metadata action",
            ))
        }
    }
}
object_struct! {pub struct OrchestratorMcpThreadListItem {
pub thread_id:ThreadId,
pub link:String,
pub title:String,
pub created_by:Actor,
pub creation_source:CreationSource,
pub status:McpThreadStatus,
pub provider_instance_id:ProviderInstanceId,
pub model:String,
pub runtime_mode:RuntimeMode,
pub interaction_mode:ProviderInteractionMode,
pub settled:bool,
pub snoozed:bool,
pub item_count:NonNegativeInt,
pub created_at:IsoDateTime,
pub updated_at:IsoDateTime,
#[serde(deserialize_with="deserialize_required_nullable")] pub latest_run_id:Option<RunId>,
#[serde(deserialize_with="deserialize_required_nullable")] pub linked_pull_request:Option<ThreadLinkedPullRequest>,
#[serde(deserialize_with="deserialize_required_nullable")] pub settled_at:Option<IsoDateTime>,
#[serde(deserialize_with="deserialize_required_nullable")] pub snoozed_until:Option<IsoDateTime>,
#[serde(deserialize_with="deserialize_required_nullable")] pub parent_thread_id:Option<ThreadId>,
#[serde(deserialize_with="deserialize_required_nullable")] pub relationship_to_parent:Option<ThreadRelationship>,
}}
object_struct! {pub struct OrchestratorMcpThreadListResult {
pub project_id:ProjectId,
pub threads:Vec<OrchestratorMcpThreadListItem>,
pub total:NonNegativeInt,
#[serde(deserialize_with="deserialize_required_nullable")] pub current_thread_id:Option<ThreadId>,
#[serde(deserialize_with="deserialize_required_nullable")] pub next_cursor:Option<NonNegativeInt>,
}}
object_struct! {pub struct OrchestratorMcpThreadDetail {
pub thread_id:ThreadId,
pub link:String,
pub title:String,
pub created_by:Actor,
pub creation_source:CreationSource,
pub status:McpThreadStatus,
pub provider_instance_id:ProviderInstanceId,
pub model:String,
pub runtime_mode:RuntimeMode,
pub interaction_mode:ProviderInteractionMode,
pub settled:bool,
pub snoozed:bool,
pub item_count:NonNegativeInt,
pub created_at:IsoDateTime,
pub updated_at:IsoDateTime,
pub project_id:ProjectId,
pub run_count:NonNegativeInt,
pub pending_request_count:NonNegativeInt,
pub archived:bool,
#[serde(deserialize_with="deserialize_required_nullable")] pub latest_run_id:Option<RunId>,
#[serde(deserialize_with="deserialize_required_nullable")] pub linked_pull_request:Option<ThreadLinkedPullRequest>,
#[serde(deserialize_with="deserialize_required_nullable")] pub settled_at:Option<IsoDateTime>,
#[serde(deserialize_with="deserialize_required_nullable")] pub snoozed_until:Option<IsoDateTime>,
#[serde(deserialize_with="deserialize_required_nullable")] pub parent_thread_id:Option<ThreadId>,
#[serde(deserialize_with="deserialize_required_nullable")] pub relationship_to_parent:Option<ThreadRelationship>,
#[serde(deserialize_with="deserialize_required_nullable")] pub active_run_id:Option<RunId>,
#[serde(deserialize_with="deserialize_required_nullable")] pub title_regeneration:Option<McpTitleRegeneration>,
#[serde(deserialize_with="deserialize_required_nullable")] pub branch:Option<String>,
#[serde(deserialize_with="deserialize_required_nullable")] pub worktree_path:Option<String>,
}}
object_struct! {pub struct OrchestratorMcpThreadRun {
pub run_id:RunId,
pub ordinal:PositiveInt,
pub status:RunStatus,
pub provider_instance_id:ProviderInstanceId,
pub model:String,
pub requested_at:IsoDateTime,
#[serde(deserialize_with="deserialize_required_nullable")] pub started_at:Option<IsoDateTime>,
#[serde(deserialize_with="deserialize_required_nullable")] pub completed_at:Option<IsoDateTime>,
}}
object_struct! {pub struct OrchestratorMcpThreadTimelineItem {
pub position:NonNegativeInt,
pub visibility:ProjectedTurnItemVisibility,
pub source_thread_id:ThreadId,
pub item_id:TurnItemId,
#[serde(rename="type")] pub type_:String,
pub status:TurnItemStatus,
pub text_truncated:bool,
pub updated_at:IsoDateTime,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub next_text_offset:Option<Option<NonNegativeInt>>,
#[serde(deserialize_with="deserialize_required_nullable")] pub run_id:Option<RunId>,
#[serde(deserialize_with="deserialize_required_nullable")] pub message_id:Option<MessageId>,
#[serde(deserialize_with="deserialize_required_nullable")] pub created_by:Option<Actor>,
#[serde(deserialize_with="deserialize_required_nullable")] pub creation_source:Option<CreationSource>,
#[serde(deserialize_with="deserialize_required_nullable")] pub title:Option<String>,
#[serde(deserialize_with="deserialize_required_nullable")] pub text:Option<String>,
}}
object_struct! {pub struct OrchestratorMcpThreadReadResult {
pub thread:OrchestratorMcpThreadDetail,
pub recent_runs:Vec<OrchestratorMcpThreadRun>,
pub items:Vec<OrchestratorMcpThreadTimelineItem>,
pub has_more:bool,
#[serde(deserialize_with="deserialize_required_nullable")] pub next_position:Option<NonNegativeInt>,
}}
object_struct! {pub struct OrchestratorMcpThreadSendResult {
pub thread_id:ThreadId,
pub message_id:MessageId,
pub run_id:RunId,
pub status:RunStatus,
pub delivery:McpThreadDelivery,
}}
object_struct! {pub struct OrchestratorMcpThreadWaitResult {
pub thread_id:ThreadId,
pub status:McpThreadStatus,
pub timed_out:bool,
#[serde(deserialize_with="deserialize_required_nullable")] pub run_id:Option<RunId>,
}}
vocabulary! {McpThreadInterruptStatus{Requested=>"interrupt_requested",NoActiveRun=>"no_active_run",Completed=>"completed",Failed=>"failed",Cancelled=>"cancelled",Interrupted=>"interrupted",RolledBack=>"rolled_back"}}
object_struct! {pub struct OrchestratorMcpThreadInterruptResult {
pub thread_id:ThreadId,
pub status:McpThreadInterruptStatus,
#[serde(deserialize_with="deserialize_required_nullable")] pub run_id:Option<RunId>,
}}
object_struct! {pub struct ThreadMetadataMcpUpdateResult {
pub thread_id:ThreadId,
pub action:ThreadMetadataMcpAction,
pub command_id:CommandId,
pub sequence:NonNegativeInt,
pub title:String,
pub updated_at:IsoDateTime,
#[serde(deserialize_with="deserialize_required_nullable")] pub title_regeneration:Option<McpTitleRegeneration>,
#[serde(deserialize_with="deserialize_required_nullable")] pub linked_pull_request:Option<ThreadLinkedPullRequest>,
}}

object_struct! {pub struct McpTitleRegeneration {pub request_id:TrimmedNonEmptyString,pub started_at:IsoDateTime,}}
