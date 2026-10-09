//! JSON shapes of `packages/contracts/src/git.ts`, `vcs.ts` and `review.ts` that the driver
//! produces or consumes. Results derive `Serialize` and inputs `Deserialize`; field names and
//! optionality follow the contracts exactly, and `tests/wire_parity.rs` holds them to JSON
//! encoded by the original schemas. Input limits the schemas enforce (query length, `limit`
//! range, non-blank strings) are the RPC layer's to check.
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeMap};

use crate::{
    core::GitCommandError,
    status::{BranchChanges, GitRemoteStatusDetails, GitStatusDetails, WorkingTree},
};

/// `Schema.DateTimeUtc` encodes like `Date.prototype.toISOString`.
pub mod iso_millis {
    use super::*;

    pub fn format(value: &DateTime<Utc>) -> String {
        value.to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    pub fn serialize<S: Serializer>(
        value: &DateTime<Utc>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format(value))
    }

    pub fn parse<E: serde::de::Error>(text: &str) -> Result<DateTime<Utc>, E> {
        DateTime::parse_from_rfc3339(text)
            .map(|time| time.with_timezone(&Utc))
            .map_err(E::custom)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DateTime<Utc>, D::Error> {
        parse(&String::deserialize(deserializer)?)
    }
}

/// `Schema.Option` in the JSON codec the RPC layer uses.
fn serialize_option<S: Serializer, T: Serialize>(
    value: Option<&T>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    match value {
        Some(value) => {
            map.serialize_entry("_tag", "Some")?;
            map.serialize_entry("value", value)?;
        }
        None => map.serialize_entry("_tag", "None")?,
    }
    map.end()
}

#[derive(Deserialize)]
#[serde(tag = "_tag")]
enum TaggedOption {
    None,
    Some { value: String },
}

fn deserialize_option<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(match TaggedOption::deserialize(deserializer)? {
        TaggedOption::None => None,
        TaggedOption::Some { value } => Some(value),
    })
}

pub mod effect_option {
    use super::*;

    pub fn serialize<S: Serializer>(
        value: &Option<String>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serialize_option(value.as_ref(), serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<String>, D::Error> {
        deserialize_option(deserializer)
    }
}

pub mod effect_option_datetime {
    use super::*;

    pub fn serialize<S: Serializer>(
        value: &Option<DateTime<Utc>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serialize_option(value.as_ref().map(iso_millis::format).as_ref(), serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<DateTime<Utc>>, D::Error> {
        deserialize_option(deserializer)?
            .map(|text| iso_millis::parse(&text))
            .transpose()
    }
}

pub(crate) fn ignore_classifier<S: Serializer>(
    native: &bool,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(if *native {
        "native"
    } else {
        "git-compatible-fallback"
    })
}

pub(crate) fn deserialize_ignore_classifier<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    match String::deserialize(deserializer)?.as_str() {
        "native" => Ok(true),
        "git-compatible-fallback" => Ok(false),
        other => Err(serde::de::Error::custom(format!(
            "unknown ignoreClassifier {other}"
        ))),
    }
}

/// `GitCommandError` as the RPC layer encodes it. The underlying OS error (`cause`) is not
/// included.
impl Serialize for GitCommandError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("_tag", "GitCommandError")?;
        map.serialize_entry("operation", &self.operation)?;
        map.serialize_entry("command", &self.command)?;
        map.serialize_entry("cwd", &self.cwd)?;
        if let Some(value) = self.argument_count {
            map.serialize_entry("argumentCount", &value)?;
        }
        if let Some(value) = self.exit_code {
            map.serialize_entry("exitCode", &value)?;
        }
        if let Some(value) = self.stdout_length {
            map.serialize_entry("stdoutLength", &value)?;
        }
        if let Some(value) = self.stderr_length {
            map.serialize_entry("stderrLength", &value)?;
        }
        if let Some(value) = self.output_length {
            map.serialize_entry("outputLength", &value)?;
        }
        if let Some(reason) = self.reason {
            map.serialize_entry("reason", reason.as_str())?;
        }
        map.serialize_entry("detail", &self.detail)?;
        map.end()
    }
}

// ---------------------------------------------------------------------------------------------
// git.ts
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsRef {
    pub name: String,
    pub is_remote: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_name: Option<String>,
    pub current: bool,
    pub is_default: bool,
    pub worktree_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    All,
    Local,
    Remote,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsListRefsInput {
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_matching_remote_refs: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_kind: Option<RefKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsListRefsResult {
    pub refs: Vec<VcsRef>,
    pub is_repo: bool,
    pub has_primary_remote: bool,
    pub next_cursor: Option<usize>,
    pub total_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullStatus {
    Pulled,
    SkippedUpToDate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsPullResult {
    pub status: PullStatus,
    pub ref_name: String,
    pub upstream_ref: Option<String>,
}

/// `VcsStatusLocalResult`. `source_control_provider` is the server's to fill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsStatusLocalResult {
    pub is_repo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_control_provider: Option<serde_json::Value>,
    pub has_primary_remote: bool,
    pub is_default_ref: bool,
    pub ref_name: Option<String>,
    pub has_working_tree_changes: bool,
    pub working_tree: WorkingTree,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_changes: Option<BranchChanges>,
}

impl From<&GitStatusDetails> for VcsStatusLocalResult {
    fn from(details: &GitStatusDetails) -> Self {
        Self {
            is_repo: details.is_repo,
            source_control_provider: None,
            has_primary_remote: details.has_origin_remote,
            is_default_ref: details.is_default_branch,
            ref_name: details.branch.clone(),
            has_working_tree_changes: details.has_working_tree_changes,
            working_tree: details.working_tree.clone(),
            branch_changes: details.branch_changes.clone(),
        }
    }
}

/// `VcsStatusRemoteResult`. `pr` is the server's to fill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsStatusRemoteResult {
    pub has_upstream: bool,
    pub ahead_count: u64,
    pub behind_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead_of_default_count: Option<u64>,
    pub pr: Option<serde_json::Value>,
}

impl From<&GitRemoteStatusDetails> for VcsStatusRemoteResult {
    fn from(details: &GitRemoteStatusDetails) -> Self {
        Self {
            has_upstream: details.has_upstream,
            ahead_count: details.ahead_count,
            behind_count: details.behind_count,
            ahead_of_default_count: Some(details.ahead_of_default_count),
            pr: None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// review.ts
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewDiffSourceKind {
    WorkingTree,
    BranchRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffFileRequest {
    pub path: String,
    pub previous_path: Option<String>,
    pub source_kind: ReviewDiffSourceKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffPreviewInput {
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_whitespace: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<ReviewDiffFileRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffFileStat {
    pub path: String,
    pub previous_path: Option<String>,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffPreviewSource {
    pub id: String,
    pub kind: ReviewDiffSourceKind,
    pub title: String,
    pub base_ref: Option<String>,
    pub head_ref: Option<String>,
    pub diff: String,
    pub diff_hash: String,
    pub truncated: bool,
    /// Absent when the totals are incomplete (too many untracked files to index).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<ReviewDiffFileStat>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffPreviewResult {
    pub cwd: String,
    #[serde(with = "iso_millis")]
    pub generated_at: DateTime<Utc>,
    pub sources: Vec<ReviewDiffPreviewSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewDiffChangeType {
    Change,
    RenamePure,
    RenameChanged,
    New,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffFileContentsInput {
    pub cwd: String,
    pub source_kind: ReviewDiffSourceKind,
    pub change_type: ReviewDiffChangeType,
    pub base_ref: Option<String>,
    #[serde(default)]
    pub head_ref: Option<String>,
    pub old_path: String,
    pub new_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffFileContentsResult {
    pub old_contents: String,
    pub new_contents: String,
}
