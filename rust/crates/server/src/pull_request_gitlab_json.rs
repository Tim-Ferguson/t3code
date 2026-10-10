//! GitLab's REST and GraphQL answers decoded into the neutral pull request shapes (port of
//! `gitLabMergeRequestJson.ts`). GitLab's enums are read as plain strings and normalised here, so
//! a release that adds a pipeline status must not fail the payload. A decoder takes the raw text
//! and returns the shape or `Err(())` where the original's schema would fail: a malformed row is
//! skipped only where the original skips it.
use crate::pull_request_decode::quote_git_patch_path;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use t3_contracts::{
    PullRequestActor, PullRequestCheck, PullRequestCheckStatus, PullRequestComment, PullRequestCommentKind, PullRequestCommit,
    PullRequestDiffSide, PullRequestLabel, PullRequestMergeCapabilities, PullRequestMergeMethod, PullRequestMergeability,
    PullRequestReaction, PullRequestReactionContent, PullRequestReviewThread, PullRequestReviewerCandidate,
    PullRequestReviewerKind, PullRequestState, PullRequestThreadComment, TrimmedNonEmptyString, trim_wire_string,
};

type R<T> = Result<T, ()>;
type Fields = Map<String, Value>;

const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

// ---- schema readers -----------------------------------------------------------------------------

fn parse(raw: &str) -> R<Value> {
    serde_json::from_str(raw).map_err(|_| ())
}

fn fields(value: &Value) -> R<&Fields> {
    value.as_object().ok_or(())
}

/// `Schema.Int`: a number with no fractional part (`1.0` reads as `1` in JavaScript).
fn int_of(value: &Value) -> R<i64> {
    let number = value.as_f64().ok_or(())?;
    if number.fract() == 0.0 && number.abs() <= MAX_SAFE { Ok(number as i64) } else { Err(()) }
}

fn req_str<'a>(o: &'a Fields, key: &str) -> R<&'a str> {
    o.get(key).and_then(Value::as_str).ok_or(())
}

fn req_int(o: &Fields, key: &str) -> R<i64> {
    int_of(o.get(key).ok_or(())?)
}

/// `Schema.optional(Schema.NullOr(Schema.String))`.
fn opt_nullable_str<'a>(o: &'a Fields, key: &str) -> R<Option<&'a str>> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(()),
    }
}

fn opt_bool(o: &Fields, key: &str) -> R<Option<bool>> {
    match o.get(key) {
        None => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(()),
    }
}

fn opt_nullable_bool(o: &Fields, key: &str) -> R<Option<bool>> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(()),
    }
}

fn opt_int(o: &Fields, key: &str) -> R<Option<i64>> {
    match o.get(key) {
        None => Ok(None),
        Some(value) => int_of(value).map(Some),
    }
}

fn opt_nullable_int(o: &Fields, key: &str) -> R<Option<i64>> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => int_of(value).map(Some),
    }
}

/// `Schema.optional(Schema.NullOr(Struct))`.
fn opt_nullable_object<'a>(o: &'a Fields, key: &str) -> R<Option<&'a Fields>> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => fields(value).map(Some),
    }
}

/// `Schema.optional(Schema.NullOr(Schema.Array(item)))`.
fn opt_nullable_array<'a, T>(o: &'a Fields, key: &str, item: impl Fn(&'a Value) -> R<T>) -> R<Vec<T>> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(entries)) => entries.iter().map(item).collect(),
        Some(_) => Err(()),
    }
}

fn trimmed(value: Option<&str>) -> Option<String> {
    let text = trim_wire_string(value.unwrap_or(""));
    (!text.is_empty()).then(|| text.to_string())
}

// ---- users ---------------------------------------------------------------------------------------

/// `RawUserSchema`: the id rides alongside the handle because GitLab writes reviewers as ids.
struct RawUser {
    id: Option<i64>,
    username: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

fn raw_user(value: &Value) -> R<RawUser> {
    let o = fields(value)?;
    Ok(RawUser {
        id: opt_int(o, "id")?,
        username: req_str(o, "username")?.to_string(),
        name: opt_nullable_str(o, "name")?.map(str::to_string),
        avatar_url: opt_nullable_str(o, "avatar_url")?.map(str::to_string),
    })
}

fn opt_nullable_user(o: &Fields, key: &str) -> R<Option<RawUser>> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => raw_user(value).map(Some),
    }
}

fn to_actor(raw: Option<&RawUser>) -> Option<PullRequestActor> {
    let raw = raw?;
    let login = trimmed(Some(&raw.username))?;
    Some(PullRequestActor {
        is_bot: None,
        login: TrimmedNonEmptyString::new(&login).ok()?,
        name: trimmed(raw.name.as_deref()),
        avatar_url: trimmed(raw.avatar_url.as_deref()),
    })
}

// ---- merge requests -----------------------------------------------------------------------------

/// What a listing carries of a merge request. GitLab reports no added or removed lines, so both
/// stay zero and the surface omits the stat.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeRequestListItem {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeability: PullRequestMergeability,
    pub additions: u64,
    pub deletions: u64,
    pub created_at: String,
    pub updated_at: String,
    pub review_request_logins: Vec<String>,
    pub labels: Vec<PullRequestLabel>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeRequestDetail {
    #[serde(flatten)]
    pub item: MergeRequestListItem,
    pub body: String,
    pub changed_files: u64,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    pub reviewers: Vec<PullRequestActor>,
    pub checks: Vec<PullRequestCheck>,
    /// False only where GitLab said so; an answer without the field leaves merging permitted.
    pub viewer_can_merge: bool,
    /// The reviewers as GitLab addresses them, which is what writing the set back takes.
    pub reviewer_ids: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_merge_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_merge_method: Option<PullRequestMergeMethod>,
    /// Absent where GitLab did not count, which is not the same as nothing behind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diverged_commits: Option<i64>,
}

struct RawMergeRequest {
    iid: i64,
    title: String,
    web_url: String,
    description: Option<String>,
    author: Option<RawUser>,
    source_branch: String,
    target_branch: String,
    state: Option<String>,
    draft: Option<bool>,
    work_in_progress: Option<bool>,
    merge_status: Option<String>,
    has_conflicts: Option<bool>,
    created_at: String,
    updated_at: String,
    merged_at: Option<String>,
    closed_at: Option<String>,
    reviewers: Vec<RawUser>,
    labels: Vec<String>,
    changes_count: Option<String>,
    pipeline: Option<(Option<String>, Option<String>, Option<String>)>,
    can_merge: Option<bool>,
    merge_when_pipeline_succeeds: Option<bool>,
    auto_merge_enabled: Option<bool>,
    squash_on_merge: Option<bool>,
    diverged_commits_count: Option<i64>,
}

fn raw_merge_request(value: &Value) -> R<RawMergeRequest> {
    let o = fields(value)?;
    let pipeline = match opt_nullable_object(o, "head_pipeline")? {
        None => None,
        Some(pipeline) => Some((
            opt_nullable_str(pipeline, "status")?.map(str::to_string),
            opt_nullable_str(pipeline, "web_url")?.map(str::to_string),
            opt_nullable_str(pipeline, "source")?.map(str::to_string),
        )),
    };
    let can_merge = match opt_nullable_object(o, "user")? {
        None => None,
        Some(user) => opt_bool(user, "can_merge")?,
    };
    // `squash` is read for the shape's sake: only `squash_on_merge` decides the stored strategy.
    opt_nullable_bool(o, "squash")?;
    Ok(RawMergeRequest {
        iid: req_int(o, "iid")?,
        title: req_str(o, "title")?.to_string(),
        web_url: req_str(o, "web_url")?.to_string(),
        description: opt_nullable_str(o, "description")?.map(str::to_string),
        author: opt_nullable_user(o, "author")?,
        source_branch: req_str(o, "source_branch")?.to_string(),
        target_branch: req_str(o, "target_branch")?.to_string(),
        state: opt_nullable_str(o, "state")?.map(str::to_string),
        draft: opt_bool(o, "draft")?,
        work_in_progress: opt_bool(o, "work_in_progress")?,
        merge_status: opt_nullable_str(o, "merge_status")?.map(str::to_string),
        has_conflicts: opt_nullable_bool(o, "has_conflicts")?,
        created_at: req_str(o, "created_at")?.to_string(),
        updated_at: req_str(o, "updated_at")?.to_string(),
        merged_at: opt_nullable_str(o, "merged_at")?.map(str::to_string),
        closed_at: opt_nullable_str(o, "closed_at")?.map(str::to_string),
        reviewers: opt_nullable_array(o, "reviewers", raw_user)?,
        labels: opt_nullable_array(o, "labels", |label| label.as_str().map(str::to_string).ok_or(()))?,
        changes_count: opt_nullable_str(o, "changes_count")?.map(str::to_string),
        pipeline,
        can_merge,
        merge_when_pipeline_succeeds: opt_nullable_bool(o, "merge_when_pipeline_succeeds")?,
        auto_merge_enabled: opt_nullable_bool(o, "auto_merge_enabled")?,
        squash_on_merge: opt_nullable_bool(o, "squash_on_merge")?,
        diverged_commits_count: opt_nullable_int(o, "diverged_commits_count")?,
    })
}

fn to_state(raw: &RawMergeRequest) -> PullRequestState {
    if trimmed(raw.merged_at.as_deref()).is_some() {
        return PullRequestState::Merged;
    }
    // `locked` is an open merge request whose discussion is locked.
    match raw.state.as_deref().map(|state| state.trim().to_lowercase()).as_deref() {
        Some("merged") => PullRequestState::Merged,
        Some("closed") => PullRequestState::Closed,
        _ => PullRequestState::Open,
    }
}

fn to_mergeability(raw: &RawMergeRequest) -> PullRequestMergeability {
    if raw.has_conflicts == Some(true) {
        return PullRequestMergeability::Conflicting;
    }
    // `unchecked` and `checking` mean GitLab has not finished the merge check yet.
    match raw.merge_status.as_deref().map(|status| status.trim().to_lowercase()).as_deref() {
        Some("can_be_merged") => PullRequestMergeability::Mergeable,
        Some("cannot_be_merged") => PullRequestMergeability::Conflicting,
        _ => PullRequestMergeability::Unknown,
    }
}

/// "3" for a counted change set, "1000+" once GitLab gives up counting.
fn to_changed_files(value: Option<&str>) -> u64 {
    let text = value.unwrap_or("").trim();
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u64>().ok().filter(|parsed| *parsed > 0).unwrap_or(0)
}

fn to_pipeline_status(value: Option<&str>) -> PullRequestCheckStatus {
    match value.map(|status| status.trim().to_lowercase()).as_deref() {
        Some("success") => PullRequestCheckStatus::Success,
        Some("failed") => PullRequestCheckStatus::Failure,
        Some("canceled" | "cancelling") => PullRequestCheckStatus::Cancelled,
        Some("skipped") => PullRequestCheckStatus::Skipped,
        // A pipeline waiting on a person is not progress, and it is not a failure either.
        Some("manual" | "scheduled") => PullRequestCheckStatus::Neutral,
        _ => PullRequestCheckStatus::Pending,
    }
}

/// GitLab has no per-job check list on a merge request, so its pipeline is the one check.
fn to_checks(raw: &RawMergeRequest) -> Vec<PullRequestCheck> {
    let Some((status, web_url, source)) = &raw.pipeline else { return vec![] };
    vec![PullRequestCheck { name: "Pipeline".into(), status: to_pipeline_status(status.as_deref()), description: trimmed(source.as_deref()), url: trimmed(web_url.as_deref()), required: None }]
}

fn to_list_item(raw: &RawMergeRequest) -> R<MergeRequestListItem> {
    Ok(MergeRequestListItem {
        number: u64::try_from(raw.iid).ok().filter(|number| *number > 0).ok_or(())?,
        title: raw.title.clone(),
        url: raw.web_url.clone(),
        author: to_actor(raw.author.as_ref()),
        head_branch: raw.source_branch.clone(),
        base_branch: raw.target_branch.clone(),
        state: to_state(raw),
        is_draft: raw.draft.or(raw.work_in_progress).unwrap_or(false),
        mergeability: to_mergeability(raw),
        additions: 0,
        deletions: 0,
        created_at: raw.created_at.clone(),
        updated_at: raw.updated_at.clone(),
        review_request_logins: raw.reviewers.iter().filter_map(|reviewer| trimmed(Some(&reviewer.username))).collect(),
        // GitLab returns label names only, so there is no colour to carry.
        labels: raw.labels.iter().filter_map(|label| trimmed(Some(label))).map(|name| PullRequestLabel { name, color: None }).collect(),
    })
}

fn to_detail(raw: &RawMergeRequest) -> R<MergeRequestDetail> {
    let auto_merge = if raw.merge_when_pipeline_succeeds.is_none() && raw.auto_merge_enabled.is_none() {
        None
    } else {
        Some(raw.merge_when_pipeline_succeeds == Some(true) || raw.auto_merge_enabled == Some(true))
    };
    Ok(MergeRequestDetail {
        item: to_list_item(raw)?,
        body: raw.description.clone().unwrap_or_default(),
        changed_files: to_changed_files(raw.changes_count.as_deref()),
        merged_at: trimmed(raw.merged_at.as_deref()),
        closed_at: trimmed(raw.closed_at.as_deref()),
        // Built from the reviewers themselves rather than their logins, so the avatars survive.
        reviewers: raw.reviewers.iter().filter_map(|reviewer| to_actor(Some(reviewer))).collect(),
        checks: to_checks(raw),
        viewer_can_merge: raw.can_merge != Some(false),
        reviewer_ids: raw.reviewers.iter().filter_map(|reviewer| reviewer.id).collect(),
        auto_merge_enabled: auto_merge,
        auto_merge_method: (auto_merge == Some(true) && raw.squash_on_merge == Some(true)).then_some(PullRequestMergeMethod::Squash),
        diverged_commits: raw.diverged_commits_count,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeRequestListBatch {
    pub items: Vec<MergeRequestListItem>,
    /// Zero-based positions of the decoded items in GitLab's raw page.
    pub raw_indexes: Vec<usize>,
    /// Rows GitLab returned, counted before decoding, so a skipped row cannot hide a next page.
    pub raw_count: usize,
}

/// Malformed entries are skipped rather than failing the batch: one unexpected merge request must
/// not blank the whole list.
pub fn decode_merge_request_list_json(raw: &str) -> R<MergeRequestListBatch> {
    let Value::Array(entries) = parse(raw)? else { return Err(()) };
    let (mut items, mut raw_indexes) = (vec![], vec![]);
    for (index, entry) in entries.iter().enumerate() {
        if let Ok(item) = raw_merge_request(entry).and_then(|raw| to_list_item(&raw)) {
            items.push(item);
            raw_indexes.push(index);
        }
    }
    Ok(MergeRequestListBatch { items, raw_indexes, raw_count: entries.len() })
}

pub fn decode_merge_request_detail_json(raw: &str) -> R<MergeRequestDetail> {
    to_detail(&raw_merge_request(&parse(raw)?)?)
}

pub fn decode_viewer_json(raw: &str) -> R<Option<String>> {
    let value = parse(raw)?;
    Ok(trimmed(opt_nullable_str(fields(&value)?, "username")?))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectUsers {
    pub candidates: Vec<PullRequestReviewerCandidate>,
    pub raw_count: usize,
}

/// The people with access to the project (`GET /projects/:id/users`). A malformed row is skipped.
/// Nobody is marked requested here: who has been asked lives on the merge request.
pub fn decode_project_users_json(raw: &str) -> R<ProjectUsers> {
    let Value::Array(entries) = parse(raw)? else { return Err(()) };
    let mut candidates = vec![];
    for entry in &entries {
        let Ok(user) = raw_user(entry) else { continue };
        let Some(id) = user.id else { continue };
        let Some(actor) = to_actor(Some(&user)) else { continue };
        candidates.push(PullRequestReviewerCandidate {
            is_bot: None,
            login: actor.login.as_str().to_string(),
            name: actor.name,
            avatar_url: actor.avatar_url,
            id: id.to_string(),
            kind: PullRequestReviewerKind::User,
            is_requested: false,
        });
    }
    Ok(ProjectUsers { candidates, raw_count: entries.len() })
}

/// GitLab settles the strategy per project: `merge_method` picks one of merge commit, semi-linear
/// or fast-forward, and squashing is a separate switch. An unrecognised setting offers nothing.
pub fn decode_project_merge_capabilities_json(raw: &str) -> R<PullRequestMergeCapabilities> {
    let value = parse(raw)?;
    let o = fields(&value)?;
    let method = opt_nullable_str(o, "merge_method")?.map(|method| method.trim().to_lowercase());
    let squash = opt_nullable_str(o, "squash_option")?.map(|option| option.trim().to_lowercase());
    Ok(PullRequestMergeCapabilities {
        merge: method.as_deref() == Some("merge"),
        // Both semi-linear and fast-forward histories are reached by rebasing onto the target.
        rebase: matches!(method.as_deref(), Some("rebase_merge" | "ff")),
        squash: matches!(squash.as_deref(), Some("always" | "default_on" | "default_off")),
    })
}

// ---- conversation -------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffRefs {
    pub base_sha: String,
    pub head_sha: String,
    pub start_sha: String,
}

pub fn decode_diff_refs_json(raw: &str) -> R<Option<DiffRefs>> {
    let value = parse(raw)?;
    match opt_nullable_object(fields(&value)?, "diff_refs")? {
        None => Ok(None),
        Some(refs) => Ok(Some(DiffRefs {
            base_sha: req_str(refs, "base_sha")?.to_string(),
            head_sha: req_str(refs, "head_sha")?.to_string(),
            start_sha: req_str(refs, "start_sha")?.to_string(),
        })),
    }
}

/// The positioned discussions only: GitLab returns the whole conversation here, including the
/// plain notes the timeline already shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Discussions {
    pub threads: Vec<PullRequestReviewThread>,
    pub raw_count: usize,
}

struct DiscussionNote {
    id: i64,
    body: Option<String>,
    author: Option<RawUser>,
    created_at: String,
    system: Option<bool>,
    resolved: Option<bool>,
    position: Option<DiscussionPosition>,
}

struct DiscussionPosition {
    position_type: Option<String>,
    new_path: Option<String>,
    old_path: Option<String>,
    new_line: Option<i64>,
    old_line: Option<i64>,
}

fn discussion_note(value: &Value) -> R<DiscussionNote> {
    let o = fields(value)?;
    opt_bool(o, "resolvable")?;
    let position = match opt_nullable_object(o, "position")? {
        None => None,
        Some(p) => Some(DiscussionPosition {
            position_type: opt_nullable_str(p, "position_type")?.map(str::to_string),
            new_path: opt_nullable_str(p, "new_path")?.map(str::to_string),
            old_path: opt_nullable_str(p, "old_path")?.map(str::to_string),
            new_line: opt_nullable_int(p, "new_line")?,
            old_line: opt_nullable_int(p, "old_line")?,
        }),
    };
    Ok(DiscussionNote {
        id: req_int(o, "id")?,
        body: opt_nullable_str(o, "body")?.map(str::to_string),
        author: opt_nullable_user(o, "author")?,
        created_at: req_str(o, "created_at")?.to_string(),
        system: opt_bool(o, "system")?,
        resolved: opt_nullable_bool(o, "resolved")?,
        position,
    })
}

pub fn decode_discussions_json(raw: &str) -> R<Discussions> {
    let Value::Array(entries) = parse(raw)? else { return Err(()) };
    let mut threads = vec![];
    for entry in &entries {
        let Ok((id, notes)) = (|| {
            let o = fields(entry)?;
            let id = req_str(o, "id")?.to_string();
            let notes = opt_nullable_array(o, "notes", discussion_note)?;
            Ok::<_, ()>((id, notes))
        })() else {
            continue;
        };
        let notes: Vec<&DiscussionNote> = notes.iter().filter(|note| note.system != Some(true)).collect();
        let Some(root) = notes.first() else { continue };
        let Some(position) = &root.position else { continue };
        if position.position_type.as_deref() != Some("text") {
            continue;
        }
        // A comment on an added or context line carries `new_line`; one on a removed line carries
        // only `old_line`, and belongs against the file as it was.
        let left = position.new_line.is_none();
        let path = trimmed(if left { position.old_path.as_deref() } else { position.new_path.as_deref() });
        let line = if left { position.old_line } else { position.new_line };
        let Some(path) = path else { continue };
        threads.push(PullRequestReviewThread {
            id,
            path,
            line: line.filter(|line| *line > 0).map(|line| line as u64),
            side: if left { PullRequestDiffSide::Left } else { PullRequestDiffSide::Right },
            is_resolved: root.resolved == Some(true),
            // GitLab reports no equivalent of a line that has since moved.
            is_outdated: false,
            comments: notes
                .iter()
                .map(|note| PullRequestThreadComment {
                    id: note.id.to_string(),
                    author: to_actor(note.author.as_ref()),
                    body: note.body.clone().unwrap_or_default(),
                    created_at: note.created_at.clone(),
                    edited_at: None,
                    url: None,
                    reactions: None,
                })
                .collect(),
            comment_count: None,
            next_comments_cursor: None,
        });
    }
    Ok(Discussions { threads, raw_count: entries.len() })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Notes {
    pub comments: Vec<PullRequestComment>,
    pub raw_count: usize,
}

/// Comments only: system notes are GitLab's own activity feed, and a `DiffNote` is the root of a
/// line-level discussion. The raw count comes back alongside because dropping notes hides whether
/// the page was full.
pub fn decode_notes_json(raw: &str) -> R<Notes> {
    let Value::Array(entries) = parse(raw)? else { return Err(()) };
    let mut comments = vec![];
    for entry in &entries {
        let Ok(note) = (|| {
            let o = fields(entry)?;
            let position = match opt_nullable_object(o, "position")? {
                None => (None, None),
                Some(p) => (opt_nullable_str(p, "new_path")?.map(str::to_string), opt_nullable_str(p, "old_path")?.map(str::to_string)),
            };
            Ok::<_, ()>((
                req_int(o, "id")?,
                opt_nullable_str(o, "body")?.map(str::to_string),
                opt_nullable_user(o, "author")?,
                req_str(o, "created_at")?.to_string(),
                opt_bool(o, "system")?,
                opt_nullable_str(o, "type")?.map(str::to_string),
                position,
            ))
        })() else {
            continue;
        };
        let (id, body, author, created_at, system, kind, (new_path, old_path)) = note;
        if system == Some(true) {
            continue;
        }
        let body = body.unwrap_or_default();
        if body.trim().is_empty() {
            continue;
        }
        comments.push(PullRequestComment {
            id: id.to_string(),
            kind: if kind.as_deref().map(str::trim) == Some("DiffNote") { PullRequestCommentKind::ReviewComment } else { PullRequestCommentKind::IssueComment },
            author: to_actor(author.as_ref()),
            body,
            created_at,
            edited_at: None,
            url: None,
            path: trimmed(new_path.as_deref()).or_else(|| trimmed(old_path.as_deref())),
            review_state: None,
            reactions: None,
        });
    }
    Ok(Notes { comments, raw_count: entries.len() })
}

/// The commits oldest first: GitLab lists a merge request's commits newest first.
pub fn decode_commits_json(raw: &str) -> R<Vec<PullRequestCommit>> {
    let Value::Array(entries) = parse(raw)? else { return Err(()) };
    let mut commits = vec![];
    for entry in &entries {
        let Ok(commit) = (|| {
            let o = fields(entry)?;
            let id = trimmed(Some(req_str(o, "id")?)).ok_or(())?;
            opt_array_of_strings(o, "parent_ids")?;
            let stats = match opt_nullable_object(o, "stats")? {
                None => None,
                Some(stats) => Some((opt_int(stats, "additions")?, opt_int(stats, "deletions")?)),
            };
            Ok::<_, ()>((
                id,
                opt_nullable_str(o, "title")?.map(str::to_string),
                opt_nullable_str(o, "committed_date")?.map(str::to_string),
                opt_nullable_str(o, "created_at")?.map(str::to_string),
                opt_nullable_str(o, "author_name")?.map(str::to_string),
                opt_nullable_str(o, "author_email")?.map(str::to_string),
                stats,
            ))
        })() else {
            continue;
        };
        let (oid, title, committed, created, author_name, author_email, stats) = commit;
        let Some(committed_date) = trimmed(committed.as_deref()).or_else(|| trimmed(created.as_deref())) else { continue };
        let login = trimmed(author_name.as_deref()).or_else(|| trimmed(author_email.as_deref()));
        commits.push(PullRequestCommit {
            oid,
            message_headline: title.unwrap_or_default(),
            committed_date,
            additions: stats.map(|(additions, _)| additions.unwrap_or(0).max(0) as u64),
            deletions: stats.map(|(_, deletions)| deletions.unwrap_or(0).max(0) as u64),
            authors: Some(match login.and_then(|login| TrimmedNonEmptyString::new(&login).ok()) {
                None => vec![],
                Some(login) => vec![PullRequestActor { is_bot: None, login, name: trimmed(author_name.as_deref()), avatar_url: None }],
            }),
        });
    }
    commits.reverse();
    Ok(commits)
}

fn opt_array_of_strings(o: &Fields, key: &str) -> R<Vec<String>> {
    match o.get(key) {
        None => Ok(vec![]),
        Some(Value::Array(entries)) => entries.iter().map(|entry| entry.as_str().map(str::to_string).ok_or(())).collect(),
        Some(_) => Err(()),
    }
}

/// The exact comparison GitLab uses for a commit-scoped diff.
pub fn decode_commit_diff_refs_json(raw: &str) -> R<Option<DiffRefs>> {
    let value = parse(raw)?;
    let o = fields(&value)?;
    let id = trimmed(Some(req_str(o, "id")?)).ok_or(())?;
    opt_nullable_str(o, "title")?;
    opt_nullable_str(o, "committed_date")?;
    opt_nullable_str(o, "created_at")?;
    opt_nullable_str(o, "author_name")?;
    opt_nullable_str(o, "author_email")?;
    if let Some(stats) = opt_nullable_object(o, "stats")? {
        opt_int(stats, "additions")?;
        opt_int(stats, "deletions")?;
    }
    let parents = opt_array_of_strings(o, "parent_ids")?;
    let base = trimmed(parents.first().map(String::as_str));
    Ok(base.map(|base| DiffRefs { base_sha: base.clone(), head_sha: id, start_sha: base }))
}

// ---- patches -------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeRequestPatch {
    pub patch: String,
    /// At least one file's hunks were withheld by GitLab as too large to inline.
    pub truncated: bool,
    pub raw_count: usize,
}

/// GitLab returns hunks per file with no `diff --git` header, so the unified patch every diff
/// viewer expects is assembled here. One page; walking pages is the caller's job.
pub fn decode_merge_request_diffs_json(raw: &str) -> R<MergeRequestPatch> {
    let Value::Array(entries) = parse(raw)? else { return Err(()) };
    let mut sections = vec![];
    let mut truncated = false;
    for entry in &entries {
        let Ok(file) = (|| {
            let o = fields(entry)?;
            Ok::<_, ()>((
                req_str(o, "old_path")?.to_string(),
                req_str(o, "new_path")?.to_string(),
                opt_nullable_str(o, "a_mode")?.map(str::to_string),
                opt_nullable_str(o, "b_mode")?.map(str::to_string),
                opt_bool(o, "new_file")? == Some(true),
                opt_bool(o, "renamed_file")? == Some(true),
                opt_bool(o, "deleted_file")? == Some(true),
                opt_nullable_str(o, "diff")?.map(str::to_string).unwrap_or_default(),
                opt_nullable_bool(o, "too_large")? == Some(true),
                opt_nullable_bool(o, "collapsed")? == Some(true),
            ))
        })() else {
            continue;
        };
        let (old_path, new_path, a_mode, b_mode, new_file, renamed, deleted, hunks, too_large, collapsed) = file;
        if hunks.is_empty() {
            // A file GitLab declined to inline still belongs in the file list, header only.
            truncated = truncated || too_large || collapsed;
        }
        let from = if new_file { "/dev/null".to_string() } else { quote_git_patch_path(&format!("a/{old_path}")) };
        let to = if deleted { "/dev/null".to_string() } else { quote_git_patch_path(&format!("b/{new_path}")) };
        let mut header = vec![format!("diff --git {} {}", quote_git_patch_path(&format!("a/{old_path}")), quote_git_patch_path(&format!("b/{new_path}")))];
        if new_file {
            header.push(format!("new file mode {}", b_mode.as_deref().unwrap_or("100644")));
        }
        if deleted {
            header.push(format!("deleted file mode {}", a_mode.as_deref().unwrap_or("100644")));
        }
        if renamed {
            header.push(format!("rename from {}", quote_git_patch_path(&old_path)));
            header.push(format!("rename to {}", quote_git_patch_path(&new_path)));
        }
        header.push(format!("--- {from}"));
        header.push(format!("+++ {to}"));
        let header = header.join("\n");
        sections.push(if hunks.is_empty() { header } else { format!("{header}\n{}", ends_with_newline(&hunks)) });
    }
    Ok(MergeRequestPatch { patch: sections.join("\n"), truncated, raw_count: entries.len() })
}

/// `text.replace(/\n?$/, "\n")`: exactly the text, ending in a newline.
pub fn ends_with_newline(text: &str) -> String {
    if text.ends_with('\n') { text.to_string() } else { format!("{text}\n") }
}

// ---- reactions ----------------------------------------------------------------------------------

/// GitLab's award names for the eight reactions the contract carries.
pub fn award_name(content: PullRequestReactionContent) -> &'static str {
    use PullRequestReactionContent::*;
    match content {
        ThumbsUp => "thumbsup",
        ThumbsDown => "thumbsdown",
        Laugh => "laughing",
        Hooray => "tada",
        Confused => "confused",
        Heart => "heart",
        Rocket => "rocket",
        Eyes => "eyes",
    }
}

fn content_of_award(name: &str) -> Option<PullRequestReactionContent> {
    use PullRequestReactionContent::*;
    [ThumbsUp, ThumbsDown, Laugh, Hooray, Confused, Heart, Rocket, Eyes].into_iter().find(|content| award_name(*content) == name)
}

/// Awards on the merge request and on every note of it, in one read. `currentUser` rides along
/// because GitLab names who awarded but never says whether that is the reader.
pub const AWARD_EMOJI_GRAPHQL_QUERY: &str = "query($fullPath: ID!, $iid: String!, $cursor: String) {
  currentUser { username }
  project(fullPath: $fullPath) {
    mergeRequest(iid: $iid) {
      awardEmoji { nodes { name user { username } } }
      notes(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { id awardEmoji { nodes { name user { username } } } }
      }
    }
  }
}";

/// What the given paths are at one revision, as blob ids; asked for by path because GitLab charges
/// this query by how many paths it is given.
pub const REPOSITORY_BLOBS_GRAPHQL_QUERY: &str = "query($fullPath: ID!, $ref: String!, $paths: [String!]!) {
  project(fullPath: $fullPath) {
    repository {
      blobs(ref: $ref, paths: $paths) {
        nodes { path oid }
      }
    }
  }
}";

/// `awardEmoji { nodes }`: optional, nullable, with nullable nodes.
fn award_nodes<'a>(o: &'a Fields, key: &str) -> R<Vec<Option<(Option<&'a str>, Option<&'a str>)>>> {
    let Some(holder) = opt_nullable_object(o, key)? else { return Ok(vec![]) };
    match holder.get("nodes") {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(nodes)) => nodes
            .iter()
            .map(|node| match node {
                Value::Null => Ok(None),
                node => {
                    let node = fields(node)?;
                    let username = match opt_nullable_object(node, "user")? {
                        None => None,
                        Some(user) => opt_nullable_str(user, "username")?,
                    };
                    Ok(Some((opt_nullable_str(node, "name")?, username)))
                }
            })
            .collect(),
        Some(_) => Err(()),
    }
}

/// The awards on one subject, grouped the way a reaction pill is drawn. The viewer's own username
/// is left out of `actors` (the page says "You") but `count` still counts them.
fn to_reactions(nodes: &[Option<(Option<&str>, Option<&str>)>], viewer: Option<&str>) -> Vec<PullRequestReaction> {
    let viewer = viewer.map(str::to_lowercase);
    let mut groups: Vec<(PullRequestReactionContent, u64, Vec<String>, bool)> = vec![];
    for node in nodes.iter().flatten() {
        // An award outside the eight is left out rather than shown under a name nobody can take back.
        let Some(content) = trimmed(node.0).and_then(|name| content_of_award(&name.to_lowercase())) else { continue };
        let Some(username) = trimmed(node.1) else { continue };
        let index = match groups.iter().position(|group| group.0 == content) {
            Some(index) => index,
            None => {
                groups.push((content, 0, vec![], false));
                groups.len() - 1
            }
        };
        let group = &mut groups[index];
        group.1 += 1;
        if viewer.as_deref() == Some(username.to_lowercase().as_str()) {
            group.3 = true;
        } else {
            group.2.push(username);
        }
    }
    groups.into_iter().filter(|group| group.1 > 0).map(|(content, count, actors, viewer_has_reacted)| PullRequestReaction { content, count, actors, viewer_has_reacted }).collect()
}

/// `gid://gitlab/DiffNote/42` is note 42, the id the REST conversation carries.
fn note_id_of(gid: Option<&str>) -> Option<String> {
    let id = trimmed(gid)?;
    let last = id.split('/').next_back()?;
    (!last.is_empty() && last.bytes().all(|byte| byte.is_ascii_digit())).then(|| last.to_string())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AwardEmojiPage {
    /// The merge request's own awards: the ones on its description.
    pub reactions: Vec<PullRequestReaction>,
    pub reactions_by_note_id: BTreeMap<String, Vec<PullRequestReaction>>,
    pub next_cursor: Option<String>,
}

pub fn decode_award_emoji_json(raw: &str) -> R<AwardEmojiPage> {
    let value = parse(raw)?;
    let data = fields(value.get("data").ok_or(())?)?;
    let viewer = match opt_nullable_object(data, "currentUser")? {
        None => None,
        Some(user) => trimmed(opt_nullable_str(user, "username")?),
    };
    let project = match data.get("project").ok_or(())? {
        Value::Null => None,
        project => Some(fields(project)?),
    };
    let merge_request = match project {
        None => None,
        Some(project) => match project.get("mergeRequest").ok_or(())? {
            Value::Null => None,
            merge_request => Some(fields(merge_request)?),
        },
    };
    let mut reactions_by_note_id = BTreeMap::new();
    let (mut reactions, mut next_cursor) = (vec![], None);
    if let Some(merge_request) = merge_request {
        let own = award_nodes(merge_request, "awardEmoji")?;
        reactions = to_reactions(&own, viewer.as_deref());
        if let Some(notes) = opt_nullable_object(merge_request, "notes")? {
            let info = opt_object_required_free(notes, "pageInfo")?;
            let nodes = match notes.get("nodes").ok_or(())? {
                Value::Array(nodes) => nodes,
                _ => return Err(()),
            };
            for node in nodes {
                let node = match node {
                    Value::Null => continue,
                    node => fields(node)?,
                };
                let id = opt_nullable_str(node, "id")?;
                let awards = award_nodes(node, "awardEmoji")?;
                let Some(id) = note_id_of(id) else { continue };
                let reactions = to_reactions(&awards, viewer.as_deref());
                if !reactions.is_empty() {
                    reactions_by_note_id.insert(id, reactions);
                }
            }
            if let Some(info) = info {
                let has_next = opt_bool(info, "hasNextPage")?;
                let end = opt_nullable_str(info, "endCursor")?;
                if has_next == Some(true) {
                    next_cursor = trimmed(end);
                }
            }
        }
    }
    Ok(AwardEmojiPage { reactions, reactions_by_note_id, next_cursor })
}

/// `Schema.optional(Struct)` (not nullable).
fn opt_object_required_free<'a>(o: &'a Fields, key: &str) -> R<Option<&'a Fields>> {
    match o.get(key) {
        None => Ok(None),
        Some(value) => fields(value).map(Some),
    }
}

/// The reader's own award of one name on a subject, which is what taking a reaction back is
/// addressed by: GitLab deletes an award by its id.
pub fn decode_own_award_id_json(raw: &str, content: PullRequestReactionContent, viewer: &str) -> R<Option<i64>> {
    let Value::Array(entries) = parse(raw)? else { return Err(()) };
    let name = award_name(content);
    for entry in &entries {
        let Ok((id, award, username)) = (|| {
            let o = fields(entry)?;
            let username = match opt_nullable_object(o, "user")? {
                None => None,
                Some(user) => opt_nullable_str(user, "username")?.map(str::to_string),
            };
            Ok::<_, ()>((req_int(o, "id")?, opt_nullable_str(o, "name")?.map(str::to_string), username))
        })() else {
            continue;
        };
        if trimmed(award.as_deref()).map(|award| award.to_lowercase()).as_deref() != Some(name) {
            continue;
        }
        if trimmed(username.as_deref()).as_deref() != Some(viewer) {
            continue;
        }
        return Ok(Some(id));
    }
    Ok(None)
}

/// Blob ids by path, or `None` where GitLab did not answer the query at all (a project the token
/// cannot see): that must be told apart from an empty answer, which would report every cleared
/// file as changed again.
pub fn decode_repository_blobs_json(raw: &str) -> R<Option<BTreeMap<String, String>>> {
    let value = parse(raw)?;
    let data = fields(value.get("data").ok_or(())?)?;
    let project = match data.get("project").ok_or(())? {
        Value::Null => return Ok(None),
        project => fields(project)?,
    };
    let Some(repository) = opt_nullable_object(project, "repository")? else { return Ok(None) };
    let Some(blobs) = opt_nullable_object(repository, "blobs")? else { return Ok(None) };
    let nodes = match blobs.get("nodes") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(nodes)) => nodes,
        Some(_) => return Err(()),
    };
    let mut found = BTreeMap::new();
    for node in nodes {
        let node = match node {
            Value::Null => continue,
            node => fields(node)?,
        };
        // Not trimmed, unlike everything else read out of this payload: a leading or trailing
        // space is a legal part of a file's name.
        let path = opt_nullable_str(node, "path")?;
        let oid = trimmed(opt_nullable_str(node, "oid")?);
        match (path, oid) {
            (Some(path), Some(oid)) if !path.is_empty() => {
                found.insert(path.to_string(), oid);
            }
            _ => {}
        }
    }
    Ok(Some(found))
}
