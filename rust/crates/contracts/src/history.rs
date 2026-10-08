//! Progressive history, cold-open snapshots, and turn-detail wire contracts.
use crate::*;
use serde::{Deserialize, Serialize};
/// Effect Struct accepts JSON objects only; serde's derived Struct visitor also
/// accepts positional arrays. Guard the shape before invoking field codecs.
macro_rules! object_struct {
    ($(#[$meta:meta])* pub struct $name:ident { $($(#[$field_meta:meta])* pub $field:ident: $ty:ty,)* }) => {
        $(#[$meta])*
        #[derive(Debug,Clone,PartialEq,Serialize)]
        #[serde(rename_all="camelCase")]
        pub struct $name {$($(#[$field_meta])* pub $field:$ty,)*}
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D:serde::Deserializer<'de>>(d:D)->Result<Self,D::Error>{
                #[derive(Deserialize)] #[serde(rename_all="camelCase")]
                struct Wire {$($(#[$field_meta])* $field:$ty,)*}
                let value=serde_json::Value::deserialize(d)?;
                if !value.is_object(){return Err(serde::de::Error::custom("expected a JSON object"));}
                let wire:Wire=serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Self{$($field:wire.$field,)*})
            }
        }
    };
}
pub(crate) use object_struct;
object_struct! {pub struct ThreadDetailSnapshot {
    pub snapshot_sequence:NonNegativeInt,
    pub projection:ThreadProjection,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub history_cursor:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub has_more_history:Option<bool>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub latest_local_turn_ordinal:Option<Option<NonNegativeInt>>,
}}
object_struct! {pub struct ThreadBoundedSnapshot {
    pub snapshot_sequence:NonNegativeInt,
    pub projection:ThreadProjection,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub history_cursor:Option<TrimmedNonEmptyString>,
    pub has_more_history:bool,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub latest_local_turn_ordinal:Option<NonNegativeInt>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub payload_budget_exceeded:Option<Option<bool>>,
}}
object_struct! {pub struct ThreadHistoryPage {
    pub snapshot_sequence:NonNegativeInt,
    #[serde(deserialize_with="crate::orchestration::deserialize_projected_turn_item_array")]
    pub items:Vec<ProjectedTurnItem>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub next_cursor:Option<TrimmedNonEmptyString>,
    pub has_more_history:bool,
}}
object_struct! {pub struct GetTurnItemInput {
    pub thread_id:ThreadId,
    pub item_id:TurnItemId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub revision:Option<Option<String>>,
}}
object_struct! {pub struct GetTurnItemResult {
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub item:Option<TurnItem>,
}}
object_struct! {pub struct GetThreadProjectionInput {pub thread_id:ThreadId,}}
object_struct! {pub struct SubscribeShellInput {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub after_sequence:Option<NonNegativeInt>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub request_completion_marker:Option<bool>,
}}
object_struct! {pub struct SubscribeThreadInput {
    pub thread_id:ThreadId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub after_sequence:Option<NonNegativeInt>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub request_completion_marker:Option<bool>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub accept_bounded_snapshot:Option<bool>,
}}
/// HTTP path and opaque history query extracted from EnvironmentHttpApi's
/// threadHistoryPage endpoint schemas; no client cursor parsing is implied.
pub type EnvironmentOrchestrationThreadSnapshotParams = GetThreadProjectionInput;
object_struct! {pub struct EnvironmentOrchestrationThreadHistoryQuery {pub cursor:TrimmedNonEmptyString,}}
