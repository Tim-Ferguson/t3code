//! `packages/contracts/src/worktreeSetup.ts`: live progress of a thread's worktree setup and
//! the two RPCs that read and cancel it. The server keeps a snapshot in memory only.
use crate::base::{NonNegativeInt, ThreadId, TrimmedNonEmptyString};
use serde::{Deserialize, Serialize};

/// Producers clamp free text to these before publishing, so encoding never fails.
pub const WORKTREE_SETUP_DETAIL_MAX_LENGTH: usize = 200;
pub const WORKTREE_SETUP_TAIL_LINE_MAX_LENGTH: usize = 400;
pub const WORKTREE_SETUP_ERROR_MAX_LENGTH: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeSetupStageId {
    #[serde(rename = "fetch")]
    Fetch,
    #[serde(rename = "checkout")]
    Checkout,
    #[serde(rename = "submodules")]
    Submodules,
    #[serde(rename = "setup-script")]
    SetupScript,
    #[serde(rename = "agent")]
    Agent,
}
/// `WORKTREE_SETUP_STAGE_ORDER`.
pub const WORKTREE_SETUP_STAGE_ORDER: [WorktreeSetupStageId; 5] = [
    WorktreeSetupStageId::Fetch,
    WorktreeSetupStageId::Checkout,
    WorktreeSetupStageId::Submodules,
    WorktreeSetupStageId::SetupScript,
    WorktreeSetupStageId::Agent,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeSetupStageStatus {
    Pending,
    Running,
    Done,
    Skipped,
    Warning,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSetupStage {
    pub id: WorktreeSetupStageId,
    pub status: WorktreeSetupStageStatus,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    /// Only the checkout stage reports a real percentage.
    pub percent: Option<u8>,
    pub detail: Option<String>,
    /// Last few output lines of the setup script, ANSI stripped, newest last.
    pub tail: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeSetupPhase {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSetupScript {
    pub name: TrimmedNonEmptyString,
    pub command: TrimmedNonEmptyString,
    pub terminal_id: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSetupSnapshot {
    pub thread_id: ThreadId,
    pub phase: WorktreeSetupPhase,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub branch: Option<TrimmedNonEmptyString>,
    pub base_ref: Option<TrimmedNonEmptyString>,
    pub worktree_path: Option<TrimmedNonEmptyString>,
    pub setup_script: Option<WorktreeSetupScript>,
    pub stages: Vec<WorktreeSetupStage>,
    pub error: Option<String>,
    pub sequence: NonNegativeInt,
}

/// `subscribeWorktreeSetup` and `worktreeSetup.cancel` inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSetupSubscribeInput {
    pub thread_id: ThreadId,
}
pub type WorktreeSetupCancelInput = WorktreeSetupSubscribeInput;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSetupCancelResult {
    pub cancelled: bool,
}
