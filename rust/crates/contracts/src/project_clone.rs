//! `packages/contracts/src/projectClone.ts`: the tracked repository clone behind a freshly added
//! project, its progress snapshots, and the `projectClone.*` inputs and results.
use crate::{
    IsoDateTime, ProjectId, SourceControlCloneProtocol, SourceControlProviderKind, SourceControlRepositoryInfo,
    TrimmedNonEmptyString,
};
use serde::{Deserialize, Serialize};

/// Producers clamp free text to these before publishing so encoding never fails.
pub const PROJECT_CLONE_DETAIL_MAX_LENGTH: usize = 200;
pub const PROJECT_CLONE_ERROR_MAX_LENGTH: usize = 1000;

/// Follows git's own clone phases as they appear on stderr, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectCloneStage {
    Connecting,
    Counting,
    Receiving,
    Resolving,
    Checkout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectClonePhase {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCloneSnapshot {
    pub project_id: ProjectId,
    pub remote_url: TrimmedNonEmptyString,
    pub destination_path: TrimmedNonEmptyString,
    pub repository: Option<SourceControlRepositoryInfo>,
    pub phase: ProjectClonePhase,
    pub stage: ProjectCloneStage,
    /// Percent of the current stage, parsed from git's progress lines (0 to 100).
    pub percent: Option<u8>,
    /// Trailing text from the progress line, typically transfer size and rate.
    pub detail: Option<String>,
    /// Human readable reason when phase is failed.
    pub error: Option<String>,
    pub started_at: IsoDateTime,
    pub ended_at: Option<IsoDateTime>,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCloneStartInput {
    pub project_id: ProjectId,
    pub title: TrimmedNonEmptyString,
    pub created_at: IsoDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<SourceControlProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<TrimmedNonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<TrimmedNonEmptyString>,
    pub destination_path: TrimmedNonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<SourceControlCloneProtocol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCloneStartResult {
    pub project_id: ProjectId,
    pub cwd: TrimmedNonEmptyString,
    pub remote_url: TrimmedNonEmptyString,
    pub repository: Option<SourceControlRepositoryInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCloneActionInput {
    pub project_id: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCloneActionResult {
    pub applied: bool,
}
