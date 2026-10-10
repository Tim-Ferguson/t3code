//! Azure DevOps answers decoded into the neutral pull request shapes (port of
//! `azureDevOpsPullRequestJson.ts`). Azure's enums are read as plain strings and normalised here,
//! so a new merge status must not fail a payload. A decoder takes the raw text and returns the
//! shape or `Err(())` where the original's schema would fail; a malformed row is skipped only
//! where the original skips it.
use crate::{
    pull_request_gitlab_json::{Fields, R, fields, int_of, opt_nullable_array, opt_nullable_bool, opt_nullable_object, opt_nullable_str, parse, trimmed},
    source_control_azure_devops::pull_request_web_url,
    workspace_entries::collate,
};
use serde::Serialize;
use serde_json::Value;
use t3_contracts::{PullRequestActor, PullRequestComment, PullRequestCommentKind, PullRequestMergeMethod, PullRequestMergeability, PullRequestState, TrimmedNonEmptyString, trim_wire_string};

/// Where a repository lives, in the terms Azure's REST routes address it by: the project and the
/// repository travel as separate route parameters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub project: String,
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AzurePullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeability: PullRequestMergeability,
    pub created_at: String,
    /// Azure records no last-touched time, so the closing time stands in where there is one.
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub body: String,
    pub review_request_logins: Vec<String>,
    pub reviewers: Vec<PullRequestActor>,
    pub location: Option<Location>,
    pub auto_merge_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_merge_method: Option<PullRequestMergeMethod>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestBatch {
    pub items: Vec<AzurePullRequest>,
    /// Zero-based positions of the decoded items in Azure's raw page.
    pub raw_indexes: Vec<usize>,
    /// Rows Azure returned, counted before decoding, so a skipped row cannot hide a next page.
    pub raw_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Iteration {
    pub id: i64,
    pub head_commit: String,
    pub merge_base_commit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    New,
    Deleted,
    Change,
    RenamePure,
    RenameChanged,
}

/// What one file did across an iteration. `old_path` differs from `path` only for a rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeEntry {
    pub path: String,
    pub old_path: String,
    pub change_kind: ChangeKind,
    pub object_id: Option<String>,
    pub original_object_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangePage {
    pub changes: Vec<ChangeEntry>,
    pub next_skip: Option<i64>,
}

/// One file's text at one commit, and whether Azure says the text is text at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemContent {
    pub contents: String,
    pub is_binary: bool,
}

// ---- schema readers -------------------------------------------------------------------------------

/// `RawIdentitySchema`: every field optional and nullable, the value itself an object.
struct Identity<'a>(&'a Fields);

fn identity(value: &Value) -> R<Identity<'_>> {
    let o = fields(value)?;
    for key in ["displayName", "uniqueName", "imageUrl"] {
        opt_nullable_str(o, key)?;
    }
    Ok(Identity(o))
}

fn opt_identity<'a>(o: &'a Fields, key: &str) -> R<Option<Identity<'a>>> {
    opt_nullable_object(o, key)?.map(|_| identity(&o[key])).transpose()
}

/// A login has to compare against `az account show`, which reports an email.
fn to_actor(raw: Option<&Identity<'_>>) -> Option<PullRequestActor> {
    let o = raw?.0;
    let get = |key: &str| trimmed(o.get(key).and_then(Value::as_str));
    let login = get("uniqueName").or_else(|| get("displayName"))?;
    Some(PullRequestActor { is_bot: None, login: TrimmedNonEmptyString::new(&login).ok()?, name: get("displayName"), avatar_url: get("imageUrl") })
}

/// `TrimmedNonEmptyString`: the trimmed text, which must not be empty.
fn trimmed_required(o: &Fields, key: &str) -> R<String> {
    match o.get(key) {
        Some(Value::String(text)) if !trim_wire_string(text).is_empty() => Ok(trim_wire_string(text).to_string()),
        _ => Err(()),
    }
}

fn normalize_ref_name(name: &str) -> String {
    let name = trim_wire_string(name);
    name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
}

fn to_state(status: Option<&str>) -> PullRequestState {
    match status.map(|status| trim_wire_string(status).to_lowercase()).as_deref() {
        Some("completed") => PullRequestState::Merged,
        Some("abandoned") => PullRequestState::Closed,
        _ => PullRequestState::Open,
    }
}

fn to_mergeability(status: Option<&str>) -> PullRequestMergeability {
    match status.map(|status| trim_wire_string(status).to_lowercase()).as_deref() {
        Some("succeeded") => PullRequestMergeability::Mergeable,
        Some("conflicts" | "failure" | "rejectedbypolicy") => PullRequestMergeability::Conflicting,
        // `queued` and `notSet` mean Azure has not finished checking.
        _ => PullRequestMergeability::Unknown,
    }
}

/// One row of the schema, or `None` when it carries too little to place the pull request: no
/// browser url, or no branch left after its prefix is dropped.
fn to_pull_request(o: &Fields) -> R<Option<AzurePullRequest>> {
    // `Schema.Int`, but a change request is numbered from one on the wire (`PositiveInt`), so a row
    // with any other id could not be carried and is not read.
    let number = match o.get("pullRequestId") {
        Some(value) => int_of(value).ok().filter(|number| *number >= 1).ok_or(())?,
        None => return Err(()),
    };
    let title = match o.get("title") {
        Some(Value::String(title)) => title.clone(),
        _ => return Err(()),
    };
    let description = opt_nullable_str(o, "description")?;
    let status = opt_nullable_str(o, "status")?;
    let is_draft = opt_nullable_bool(o, "isDraft")?;
    let auto_complete = opt_identity(o, "autoCompleteSetBy")?;
    let completion = opt_nullable_object(o, "completionOptions")?;
    let (merge_strategy, squash_merge) = match completion {
        None => (None, None),
        Some(completion) => (opt_nullable_str(completion, "mergeStrategy")?, opt_nullable_bool(completion, "squashMerge")?),
    };
    let merge_status = opt_nullable_str(o, "mergeStatus")?;
    let created_by = opt_identity(o, "createdBy")?;
    let reviewers = opt_nullable_array(o, "reviewers", identity)?;
    let (source, target, creation) = (trimmed_required(o, "sourceRefName")?, trimmed_required(o, "targetRefName")?, trimmed_required(o, "creationDate")?);
    let closed_date = opt_nullable_str(o, "closedDate")?;
    let rest_url = opt_nullable_str(o, "url")?;
    let repository = opt_nullable_object(o, "repository")?;
    let (repository_name, repository_web_url, project_name) = match repository {
        None => (None, None, None),
        Some(repository) => {
            let project = opt_nullable_object(repository, "project")?;
            (
                opt_nullable_str(repository, "name")?,
                opt_nullable_str(repository, "webUrl")?,
                match project {
                    None => None,
                    Some(project) => opt_nullable_str(project, "name")?,
                },
            )
        }
    };
    // `_links.web.href` is `Schema.optional(Schema.String)`: absent, never null.
    let web_link = match opt_nullable_object(o, "_links")? {
        None => None,
        Some(links) => match opt_nullable_object(links, "web")? {
            None => None,
            Some(web) => match web.get("href") {
                None => None,
                Some(Value::String(href)) => Some(href.as_str()),
                Some(_) => return Err(()),
            },
        },
    };
    let closed_at = trimmed(closed_date);
    let url = trimmed(Some(pull_request_web_url(number as u64, web_link, repository_web_url, rest_url, project_name, repository_name).as_str()));
    let (head_branch, base_branch) = (trimmed(Some(normalize_ref_name(&source).as_str())), trimmed(Some(normalize_ref_name(&target).as_str())));
    let (Some(url), Some(head_branch), Some(base_branch)) = (url, head_branch, base_branch) else { return Ok(None) };
    let reviewers: Vec<PullRequestActor> = reviewers.iter().filter_map(|reviewer| to_actor(Some(reviewer))).collect();
    let auto_merge_method = auto_complete.as_ref().and_then(|_| match merge_strategy.map(|strategy| trim_wire_string(strategy).to_lowercase()).as_deref() {
        Some("squash") => Some(PullRequestMergeMethod::Squash),
        Some("rebase" | "rebasemerge") => Some(PullRequestMergeMethod::Rebase),
        Some("nofastforward") => Some(PullRequestMergeMethod::Merge),
        _ => (squash_merge == Some(true)).then_some(PullRequestMergeMethod::Squash),
    });
    let location = match (trimmed(project_name), trimmed(repository_name)) {
        (Some(project), Some(repository)) => Some(Location { project, repository }),
        _ => None,
    };
    Ok(Some(AzurePullRequest {
        number: number as u64,
        title,
        url,
        author: to_actor(created_by.as_ref()),
        head_branch,
        base_branch,
        state: to_state(status),
        is_draft: is_draft.unwrap_or(false),
        mergeability: to_mergeability(merge_status),
        updated_at: closed_at.clone().unwrap_or_else(|| creation.clone()),
        created_at: creation,
        closed_at,
        body: description.unwrap_or("").to_string(),
        review_request_logins: reviewers.iter().map(|reviewer| reviewer.login.as_str().to_string()).collect(),
        reviewers,
        location,
        auto_merge_enabled: auto_complete.is_some(),
        auto_merge_method,
    }))
}

// ---- the decoders -----------------------------------------------------------------------------------

/// Malformed entries are skipped rather than failing the batch, as on the other hosts.
pub fn decode_pull_request_list_json(raw: &str) -> R<PullRequestBatch> {
    let Value::Array(entries) = parse(raw)? else { return Err(()) };
    let (mut items, mut raw_indexes) = (vec![], vec![]);
    for (index, entry) in entries.iter().enumerate() {
        if let Some(item) = fields(entry).ok().and_then(|o| to_pull_request(o).ok()).flatten() {
            items.push(item);
            raw_indexes.push(index);
        }
    }
    Ok(PullRequestBatch { items, raw_indexes, raw_count: entries.len() })
}

/// `None` carries "Azure answered, but with too little to use", which the caller reports.
pub fn decode_pull_request_json(raw: &str) -> R<Option<AzurePullRequest>> {
    to_pull_request(fields(&parse(raw)?)?)
}

/// `az account show --query user` reports the signed-in account, whose name is an email.
pub fn decode_viewer_json(raw: &str) -> R<Option<String>> {
    let value = parse(raw)?;
    let o = fields(&value)?;
    Ok(match opt_nullable_object(o, "user")? {
        None => None,
        Some(user) => trimmed(opt_nullable_str(user, "name")?),
    })
}

struct RawComment {
    id: Option<i64>,
    content: Option<String>,
    author: Option<PullRequestActor>,
    published: Option<String>,
    deleted: bool,
    system: bool,
}

fn thread_comment(value: &Value) -> R<RawComment> {
    let o = fields(value)?;
    let id = match o.get("id") {
        None | Some(Value::Null) => None,
        Some(id) => Some(int_of(id)?),
    };
    let content = opt_nullable_str(o, "content")?.map(str::to_string);
    let author = opt_identity(o, "author")?;
    let published = opt_nullable_str(o, "publishedDate")?;
    let deleted = opt_nullable_bool(o, "isDeleted")? == Some(true);
    let comment_type = opt_nullable_str(o, "commentType")?;
    Ok(RawComment {
        id,
        content,
        author: to_actor(author.as_ref()),
        published: trimmed(published),
        deleted,
        system: comment_type.is_some_and(|kind| trim_wire_string(kind).to_lowercase() == "system"),
    })
}

/// Azure keeps its conversation as threads of comments, and every one of them is a remark somebody
/// wrote: a reply under a thread is as much of the conversation as the line that opened it. A
/// thread pinned to a file is a line-level review comment. The whole collection arrives at once.
pub fn decode_threads_json(raw: &str) -> R<Vec<PullRequestComment>> {
    let value = parse(raw)?;
    let o = fields(&value)?;
    let Some(Value::Array(entries)) = o.get("value") else { return Err(()) };
    let mut comments = vec![];
    for entry in entries {
        let Ok(thread) = fields(entry) else { continue };
        let Ok(id) = thread.get("id").ok_or(()).and_then(int_of) else { continue };
        let parsed = (|| -> R<(bool, Option<String>, Vec<RawComment>)> {
            let deleted = opt_nullable_bool(thread, "isDeleted")? == Some(true);
            let path = match opt_nullable_object(thread, "threadContext")? {
                None => None,
                Some(context) => trimmed(opt_nullable_str(context, "filePath")?),
            };
            Ok((deleted, path, opt_nullable_array(thread, "comments", thread_comment)?))
        })();
        let Ok((deleted, path, thread_comments)) = parsed else { continue };
        if deleted {
            continue;
        }
        for comment in thread_comments {
            let Some(published) = comment.published else { continue };
            let content = comment.content.unwrap_or_default();
            if comment.deleted || comment.system || trim_wire_string(&content).is_empty() {
                continue;
            }
            comments.push(PullRequestComment {
                id: format!("{id}:{}", comment.id.unwrap_or(0)),
                kind: if path.is_none() { PullRequestCommentKind::IssueComment } else { PullRequestCommentKind::ReviewComment },
                author: comment.author,
                body: content,
                created_at: published,
                edited_at: None,
                url: None,
                path: path.clone(),
                review_state: None,
                reactions: None,
            });
        }
    }
    // `toSorted` is stable and `localeCompare` is the collation the original sorts with.
    comments.sort_by(|left, right| collate(&left.created_at, &right.created_at));
    Ok(comments)
}

/// The commit an `{ commitId }` wrapper names.
fn commit_of(o: &Fields, key: &str) -> R<Option<String>> {
    Ok(match opt_nullable_object(o, key)? {
        None => None,
        Some(wrapper) => trimmed(opt_nullable_str(wrapper, "commitId")?),
    })
}

/// Every push, oldest first.
pub fn decode_iterations_json(raw: &str) -> R<Vec<Iteration>> {
    let value = parse(raw)?;
    let Some(Value::Array(entries)) = fields(&value)?.get("value") else { return Err(()) };
    let mut iterations = vec![];
    for entry in entries {
        let Ok(o) = fields(entry) else { continue };
        let parsed = (|| -> R<Option<Iteration>> {
            let id = int_of(o.get("id").ok_or(())?)?;
            let (head, base) = (commit_of(o, "sourceRefCommit")?, commit_of(o, "commonRefCommit")?);
            // An iteration Azure cannot place both ends of names no range, and a patch needs both.
            Ok(head.zip(base).map(|(head_commit, merge_base_commit)| Iteration { id, head_commit, merge_base_commit }))
        })();
        if let Ok(Some(iteration)) = parsed {
            iterations.push(iteration);
        }
    }
    iterations.sort_by_key(|iteration| iteration.id);
    Ok(iterations)
}

/// Azure leads a path with a slash that every other host and every patch omits. Not trimmed: a
/// leading or trailing space is a legal part of a file's name.
fn to_repository_path(value: Option<&str>) -> Option<String> {
    let path = value?.trim_start_matches('/');
    (!path.is_empty()).then(|| path.to_string())
}

/// Azure names a change with one word or two, and a rename arrives alone or alongside the edit
/// that came with it. Anything it has added since reads as a plain change.
fn to_change_kind(raw: Option<&str>, renamed: bool) -> ChangeKind {
    let lowered = raw.unwrap_or("").to_lowercase();
    let parts: Vec<&str> = lowered.split(',').map(trim_wire_string).filter(|part| !part.is_empty()).collect();
    let has = |name: &str| parts.contains(&name);
    if has("delete") {
        ChangeKind::Deleted
    } else if renamed || has("rename") {
        if has("edit") { ChangeKind::RenameChanged } else { ChangeKind::RenamePure }
    } else if has("add") {
        ChangeKind::New
    } else {
        ChangeKind::Change
    }
}

pub fn decode_iteration_changes_json(raw: &str) -> R<ChangePage> {
    let value = parse(raw)?;
    let o = fields(&value)?;
    let Some(Value::Array(entries)) = o.get("changeEntries") else { return Err(()) };
    let next_skip = match o.get("nextSkip") {
        None | Some(Value::Null) => None,
        Some(number) => Some(number.as_f64().ok_or(())?),
    };
    let mut changes = vec![];
    for entry in entries {
        let Ok(Some(change)) = (|| -> R<Option<ChangeEntry>> {
            let o = fields(entry)?;
            let change_type = opt_nullable_str(o, "changeType")?;
            let source_server_item = opt_nullable_str(o, "sourceServerItem")?;
            let original_path = opt_nullable_str(o, "originalPath")?;
            let item = opt_nullable_object(o, "item")?;
            let (path, object_id, original_object_id, is_folder, object_type) = match item {
                None => (None, None, None, None, None),
                Some(item) => (
                    opt_nullable_str(item, "path")?,
                    opt_nullable_str(item, "objectId")?,
                    opt_nullable_str(item, "originalObjectId")?,
                    opt_nullable_bool(item, "isFolder")?,
                    opt_nullable_str(item, "gitObjectType")?,
                ),
            };
            let Some(path) = to_repository_path(path) else { return Ok(None) };
            // A review shows files, and a folder has no content to show for either side of one.
            if is_folder == Some(true) || object_type.unwrap_or("blob").to_lowercase() != "blob" {
                return Ok(None);
            }
            // Azure names where a renamed file came from in either of two places depending on the
            // route and the version, so both are read and the current path stands in otherwise.
            let old_path = to_repository_path(source_server_item).or_else(|| to_repository_path(original_path)).unwrap_or_else(|| path.clone());
            Ok(Some(ChangeEntry {
                change_kind: to_change_kind(change_type, old_path != path),
                path,
                old_path,
                object_id: trimmed(object_id),
                original_object_id: trimmed(original_object_id),
            }))
        })() else {
            continue;
        };
        changes.push(change);
    }
    Ok(ChangePage { changes, next_skip: next_skip.filter(|skip| skip.fract() == 0.0 && *skip > 0.0 && *skip <= 9_007_199_254_740_991.0).map(|skip| skip as i64) })
}

/// Azure answers an absent file with an empty body rather than an error, which reads as empty.
/// Whether the bytes are text is Azure's own call, not this decoder's to guess from content.
pub fn decode_item_content_json(raw: &str) -> R<ItemContent> {
    let value = parse(raw)?;
    let o = fields(&value)?;
    let contents = opt_nullable_str(o, "content")?.unwrap_or("").to_string();
    let is_binary = match opt_nullable_object(o, "contentMetadata")? {
        None => false,
        Some(metadata) => opt_nullable_bool(metadata, "isBinary")? == Some(true),
    };
    Ok(ItemContent { contents, is_binary })
}
