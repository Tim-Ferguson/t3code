//! `packages/contracts/src/checkpointDiff.ts`: inputs and result of the two checkpoint diff RPCs.
use crate::base::{NonNegativeInt, ThreadId};
use serde::{Deserialize, Serialize};

/// `orchestration.getTurnDiff`. Decoding enforces the source's `fromTurnCount <= toTurnCount`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "TurnDiffWire")]
pub struct OrchestrationGetTurnDiffInput {
    pub thread_id: ThreadId,
    pub from_turn_count: NonNegativeInt,
    pub to_turn_count: NonNegativeInt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_whitespace: Option<bool>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnDiffWire {
    thread_id: ThreadId,
    from_turn_count: NonNegativeInt,
    to_turn_count: NonNegativeInt,
    #[serde(default)]
    ignore_whitespace: Option<bool>,
}
impl TryFrom<TurnDiffWire> for OrchestrationGetTurnDiffInput {
    type Error = String;
    fn try_from(wire: TurnDiffWire) -> Result<Self, String> {
        if wire.from_turn_count > wire.to_turn_count {
            return Err("fromTurnCount must be less than or equal to toTurnCount".into());
        }
        Ok(Self {
            thread_id: wire.thread_id,
            from_turn_count: wire.from_turn_count,
            to_turn_count: wire.to_turn_count,
            ignore_whitespace: wire.ignore_whitespace,
        })
    }
}

/// `orchestration.getFullThreadDiff`: the turn diff from the thread's baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestrationGetFullThreadDiffInput {
    pub thread_id: ThreadId,
    pub to_turn_count: NonNegativeInt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_whitespace: Option<bool>,
}

/// Result of both diff RPCs (`ThreadTurnDiff`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTurnDiff {
    pub thread_id: ThreadId,
    pub from_turn_count: NonNegativeInt,
    pub to_turn_count: NonNegativeInt,
    pub diff: String,
}
pub type OrchestrationGetTurnDiffResult = ThreadTurnDiff;
pub type OrchestrationGetFullThreadDiffResult = ThreadTurnDiff;
