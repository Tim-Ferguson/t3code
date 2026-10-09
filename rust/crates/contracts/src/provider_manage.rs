//! Provider refresh and one-click update wire types from rpc.ts and server.ts.
use crate::{history::object_struct, *};
use serde::{Deserialize, Serialize};
object_struct! {pub struct ServerRefreshProvidersInput {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub instance_id:Option<ProviderInstanceId>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub cwd:Option<TrimmedNonEmptyString>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub fresh:Option<bool>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub refresh_models:Option<bool>,
}}
object_struct! {pub struct ServerProviderUpdateInput {
    pub provider:ProviderDriverKind,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub target_version:Option<TrimmedNonEmptyString>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")] pub instance_id:Option<ProviderInstanceId>,
}}
object_struct! {pub struct ServerProviderUpdatedPayload {pub providers:Vec<ServerProvider>,}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProviderUpdateErrorTag {
    ServerProviderUpdateError,
}
object_struct! {pub struct ServerProviderUpdateError {
    #[serde(rename="_tag")] pub tag:ServerProviderUpdateErrorTag,
    pub provider:ProviderDriverKind,pub reason:TrimmedNonEmptyString,
}}
impl std::fmt::Display for ServerProviderUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Provider update failed for {}: {}", self.provider, self.reason)
    }
}
impl std::error::Error for ServerProviderUpdateError {}
