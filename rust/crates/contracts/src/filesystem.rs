//! Filesystem browsing and project resources. Wire bounds are independent of
//! backend path containment and filesystem authorization checks.
use crate::history::object_struct;
use crate::*;
use serde::{Deserialize, Serialize};
object_struct! {pub struct FilesystemBrowseInput {
    pub partial_path:BoundedTrimmedString<512>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cwd:Option<Option<BoundedTrimmedString<512>>>,
}}
object_struct! {pub struct FilesystemBrowseEntry {pub name:TrimmedNonEmptyString,pub full_path:TrimmedNonEmptyString,}}
object_struct! {pub struct FilesystemBrowseResult {pub parent_path:TrimmedNonEmptyString,pub entries:Vec<FilesystemBrowseEntry>,}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemBrowseFailure {
    WindowsPathUnsupported,
    CurrentProjectRequired,
    ReadDirectoryFailed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectEntryKind {
    File,
    Directory,
}
object_struct! {pub struct ProjectSearchEntriesInput {
    pub cwd:TrimmedNonEmptyString,
    pub query:BoundedTrimmedAllowEmptyString<256>,
    pub limit:RangeInt<1,200>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub kind:Option<Option<ProjectEntryKind>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub image_only:Option<Option<bool>>,
}}
object_struct! {pub struct ProjectEntry {
    pub path:TrimmedNonEmptyString,
    pub kind:ProjectEntryKind,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub ignored:Option<Option<bool>>,
}}
object_struct! {pub struct ProjectSearchEntriesResult {pub entries:Vec<ProjectEntry>,pub truncated:bool,}}
/// Unlike filename queries, content queries and terminal writes preserve every
/// whitespace character. Nonemptiness is checked before UTF-16 wire length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct NonEmptyBoundedString<const MAX: usize>(pub String);
impl<'de, const MAX: usize> Deserialize<'de> for NonEmptyBoundedString<MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        if !value.is_empty() && value.encode_utf16().count() <= MAX {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(format!(
                "expected 1..={MAX} UTF-16 code units"
            )))
        }
    }
}
object_struct! {pub struct ProjectSearchContentsInput {
    pub cwd:TrimmedNonEmptyString,
    pub query:NonEmptyBoundedString<256>,
    pub limit:RangeInt<1,500>,
    pub case_sensitive:bool,
    pub whole_word:bool,
    pub use_regex:bool,
}}
object_struct! {pub struct ProjectContentMatchRange {pub start:NonNegativeInt,pub end:NonNegativeInt,}}
object_struct! {pub struct ProjectContentMatch {pub path:TrimmedNonEmptyString,pub line_number:PositiveInt,pub line_content:String,pub match_ranges:Vec<ProjectContentMatchRange>,}}
object_struct! {pub struct ProjectSearchContentsResult {
    pub matches:Vec<ProjectContentMatch>,pub truncated:bool,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub regex_fallback_error:Option<Option<String>>,
}}
object_struct! {pub struct ProjectListEntriesInput {
    pub cwd:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub directory_path:Option<Option<TrimmedString>>,
}}
pub type ProjectListEntriesResult = ProjectSearchEntriesResult;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectEntriesFailure {
    WorkspaceRootNotFound,
    WorkspaceRootCreateFailed,
    WorkspaceRootStatFailed,
    WorkspaceRootNotDirectory,
    SearchIndexCreateFailed,
    SearchIndexScanTimedOut,
    SearchIndexSearchFailed,
    DirectoryListFailed,
}
object_struct! {pub struct ProjectReadFileInput {pub cwd:TrimmedNonEmptyString,pub relative_path:BoundedTrimmedString<512>,}}
object_struct! {pub struct ProjectReadFileResult {pub relative_path:TrimmedNonEmptyString,pub contents:String,pub byte_length:NonNegativeInt,pub truncated:bool,}}
object_struct! {pub struct ProjectWriteFileInput {pub cwd:TrimmedNonEmptyString,pub relative_path:BoundedTrimmedString<512>,pub contents:String,}}
object_struct! {pub struct ProjectWriteFileResult {pub relative_path:TrimmedNonEmptyString,}}
object_struct! {pub struct ProjectEnsureScratchResult {pub project_id:ProjectId,}}
object_struct! {pub struct ProjectCreateNewInput {pub name:BoundedTrimmedString<200>,}}
object_struct! {pub struct ProjectCreateNewResult {
    pub project_id:ProjectId,pub workspace_root:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub commit_error:Option<TrimmedNonEmptyString>,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectFileFailure {
    WorkspacePathOutsideRoot,
    ResolvedPathOutsideRoot,
    PathNotFile,
    BinaryFile,
    OperationFailed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectFileOperation {
    RealpathWorkspaceRoot,
    RealpathTarget,
    Open,
    Stat,
    Read,
    Close,
    MakeDirectory,
    WriteFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilesystemBrowseErrorTag {
    FilesystemBrowseError,
}
object_struct! {pub struct FilesystemBrowseError {
    #[serde(rename="_tag")] pub tag:FilesystemBrowseErrorTag,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub partial_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cwd:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub failure:Option<Option<FilesystemBrowseFailure>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub parent_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub platform:Option<Option<TrimmedNonEmptyString>>,
    pub message:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cause:Option<Option<serde_json::Value>>,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectSearchEntriesErrorTag {
    ProjectSearchEntriesError,
}
object_struct! {pub struct ProjectSearchEntriesError {
    #[serde(rename="_tag")] pub tag:ProjectSearchEntriesErrorTag,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cwd:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub query_length:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub limit:Option<Option<PositiveInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub failure:Option<Option<ProjectEntriesFailure>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub normalized_cwd:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub timeout:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub detail:Option<Option<TrimmedNonEmptyString>>,
    pub message:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cause:Option<Option<serde_json::Value>>,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectSearchContentsErrorTag {
    ProjectSearchContentsError,
}
object_struct! {pub struct ProjectSearchContentsError {
    #[serde(rename="_tag")] pub tag:ProjectSearchContentsErrorTag,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cwd:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub query_length:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub limit:Option<Option<PositiveInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub failure:Option<Option<ProjectEntriesFailure>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub normalized_cwd:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub timeout:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub detail:Option<Option<TrimmedNonEmptyString>>,
    pub message:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cause:Option<Option<serde_json::Value>>,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectListEntriesErrorTag {
    ProjectListEntriesError,
}
object_struct! {pub struct ProjectListEntriesError {
    #[serde(rename="_tag")] pub tag:ProjectListEntriesErrorTag,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cwd:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub failure:Option<Option<ProjectEntriesFailure>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub normalized_cwd:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub timeout:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub detail:Option<Option<TrimmedNonEmptyString>>,
    pub message:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cause:Option<Option<serde_json::Value>>,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectReadFileErrorTag {
    ProjectReadFileError,
}
object_struct! {pub struct ProjectReadFileError {
    #[serde(rename="_tag")] pub tag:ProjectReadFileErrorTag,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cwd:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub relative_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub failure:Option<Option<ProjectFileFailure>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub resolved_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub resolved_workspace_root:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub operation:Option<Option<ProjectFileOperation>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub operation_path:Option<Option<TrimmedNonEmptyString>>,
    pub message:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cause:Option<Option<serde_json::Value>>,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectWriteFileErrorTag {
    ProjectWriteFileError,
}
object_struct! {pub struct ProjectWriteFileError {
    #[serde(rename="_tag")] pub tag:ProjectWriteFileErrorTag,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cwd:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub relative_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub failure:Option<Option<ProjectFileFailure>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub resolved_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub resolved_workspace_root:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub operation:Option<Option<ProjectFileOperation>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub operation_path:Option<Option<TrimmedNonEmptyString>>,
    pub message:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cause:Option<Option<serde_json::Value>>,
}}
