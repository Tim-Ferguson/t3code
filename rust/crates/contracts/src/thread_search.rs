//! `orchestration.searchThreads` payload and result (port of `packages/contracts/src/threadSearch.ts`).
use crate::history::object_struct;
use crate::*;
use serde::{Deserialize, Serialize};

/// Trimmed, 2 to 200 UTF-16 code units: a search cannot be broader or longer than that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ThreadSearchQuery(String);
impl ThreadSearchQuery {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for ThreadSearchQuery {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        let trimmed = trim_wire_string(&value);
        match trimmed.encode_utf16().count() {
            2..=200 => Ok(Self(trimmed.to_owned())),
            _ => Err(serde::de::Error::custom(
                "expected a trimmed string of 2 to 200 UTF-16 code units",
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrchestrationThreadSearchSource {
    User,
    Assistant,
}

object_struct! {pub struct OrchestrationSearchThreadsInput {
    pub query:ThreadSearchQuery,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub limit:Option<RangeInt<1,50>>,
}}
object_struct! {pub struct OrchestrationThreadSearchMatch {
    pub thread_id:ThreadId,
    pub project_id:ProjectId,
    pub source:OrchestrationThreadSearchSource,
    pub snippet:BoundedString<240>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub message_created_at:Option<String>,
}}
object_struct! {pub struct OrchestrationSearchThreadsResult {
    pub matches:Vec<OrchestrationThreadSearchMatch>,
}}
