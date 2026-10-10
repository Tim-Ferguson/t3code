//! Bitbucket Cloud's REST answers decoded into the neutral pull request shapes (port of
//! `bitbucketPullRequestJson.ts`). Enums are read as plain strings and normalised here, so a new
//! pull request state or build status must not fail a payload. A decoder takes the raw text and
//! returns the shape or `Err(())` where the original's schema would fail; a malformed entry of a
//! page is skipped only where the original skips it.
use crate::{
    pull_request_decode::{CheckEntry, dedupe_checks},
    pull_request_gitlab_json::{Fields, R, fields, opt_bool, opt_nullable_array, opt_nullable_bool, opt_nullable_int, opt_nullable_object, opt_nullable_str, parse, req_int, req_str, trimmed},
    source_control_decode::{format_iso, parse_date},
};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use t3_contracts::{
    PullRequestActor, PullRequestCheck, PullRequestCheckStatus, PullRequestComment, PullRequestCommentKind, PullRequestCommit, PullRequestDiffSide,
    PullRequestMergeability, PullRequestReviewThread, PullRequestReviewerCandidate, PullRequestReviewerKind, PullRequestState, PullRequestThreadComment,
    TrimmedNonEmptyString, trim_wire_string,
};

// ---- schema readers -------------------------------------------------------------------------------

/// A required `TrimmedNonEmptyString`.
fn trimmed_non_empty(o: &Fields, key: &str) -> R<String> {
    let text = trim_wire_string(req_str(o, key)?);
    if text.is_empty() { Err(()) } else { Ok(text.to_string()) }
}

/// `Schema.optional(Schema.String)`.
fn opt_str<'a>(o: &'a Fields, key: &str) -> R<Option<&'a str>> {
    match o.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(()),
    }
}

/// Bitbucket stamps times as `+00:00` with microseconds. The page sorts change requests from
/// every host against each other as plain strings, so they are normalised to the same `Z` form
/// the other hosts already use. A stamp no date reads is kept as sent.
fn to_iso_utc(value: &str) -> String {
    parse_date(value).map_or_else(|| value.to_string(), |date| format_iso(&date))
}

/// `a.localeCompare(b)` over the characters a timestamp holds: ICU puts the punctuation before the
/// digits and the digits before the letters, which is not the order of their code points.
pub fn locale_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    fn rank(character: char) -> (u8, u32) {
        match character {
            '_' => (0, 0),
            '-' => (0, 1),
            ',' => (0, 2),
            ';' => (0, 3),
            ':' => (0, 4),
            '.' => (0, 5),
            '+' => (0, 6),
            digit if digit.is_ascii_digit() => (1, digit as u32),
            letter if letter.is_alphabetic() => (2, letter.to_lowercase().next().map_or(letter as u32, |lower| lower as u32)),
            other => (0, 100 + other as u32),
        }
    }
    left.chars().map(rank).cmp(right.chars().map(rank)).then_with(|| left.cmp(right))
}

// ---- users -----------------------------------------------------------------------------------------

/// `RawUserSchema`.
#[derive(Debug, Clone)]
struct RawUser {
    /// How Bitbucket addresses an account when a reviewer set is written; the handles it shows are
    /// not accepted there. Braced, and sent back exactly as it arrived.
    uuid: Option<String>,
    /// Absent on an app account, which is why `display_name` has to stand in for it.
    nickname: Option<String>,
    display_name: Option<String>,
    avatar_href: Option<String>,
}

fn raw_user(value: &Value) -> R<RawUser> {
    let o = fields(value)?;
    let avatar_href = match opt_nullable_object(o, "links")? {
        None => None,
        Some(links) => match opt_nullable_object(links, "avatar")? {
            None => None,
            Some(avatar) => opt_str(avatar, "href")?.map(str::to_string),
        },
    };
    Ok(RawUser {
        uuid: opt_nullable_str(o, "uuid")?.map(str::to_string),
        nickname: opt_nullable_str(o, "nickname")?.map(str::to_string),
        display_name: opt_nullable_str(o, "display_name")?.map(str::to_string),
        avatar_href,
    })
}

fn opt_nullable_user(o: &Fields, key: &str) -> R<Option<RawUser>> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => raw_user(value).map(Some),
    }
}

/// An app account has no nickname, so the display name is the only handle it has.
fn to_actor(raw: Option<&RawUser>) -> Option<PullRequestActor> {
    let raw = raw?;
    let login = trimmed(raw.nickname.as_deref()).or_else(|| trimmed(raw.display_name.as_deref()))?;
    Some(PullRequestActor {
        is_bot: None,
        login: TrimmedNonEmptyString::new(&login).ok()?,
        name: trimmed(raw.display_name.as_deref()),
        avatar_url: trimmed(raw.avatar_href.as_deref()),
    })
}

// ---- pull requests ---------------------------------------------------------------------------------

/// A pull request as a listing and the detail see it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BitbucketPullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    pub head_repository_name_with_owner: Option<String>,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    /// Bitbucket reports no conflict state on a pull request, so the list leaves it unknown. The
    /// detail read asks the conflicts endpoint, which does answer.
    pub mergeability: PullRequestMergeability,
    pub created_at: String,
    pub updated_at: String,
    pub body: String,
    pub review_request_logins: Vec<String>,
    pub reviewers: Vec<PullRequestActor>,
    /// The reviewers as Bitbucket addresses them, which is what writing the set back takes.
    pub reviewer_ids: Vec<String>,
    /// Approvals and change requests, which Bitbucket keeps on its participants.
    pub reviews: Vec<PullRequestComment>,
}

/// `RawBranchSchema`: required and non-empty, because the wire contract will not carry a change
/// request without a branch or a link; a row missing one is skipped rather than breaking the
/// response it travels in.
fn raw_branch(value: Option<&Value>) -> R<(String, Option<String>)> {
    let o = fields(value.ok_or(())?)?;
    let branch = fields(o.get("branch").ok_or(())?)?;
    let name = trimmed_non_empty(branch, "name")?;
    let repository = match opt_nullable_object(o, "repository")? {
        None => None,
        Some(repository) => Some(trimmed_non_empty(repository, "full_name")?),
    };
    Ok((name, repository))
}

fn to_state(state: Option<&str>) -> PullRequestState {
    match state.map(|state| trim_wire_string(state).to_uppercase()).as_deref() {
        Some("MERGED") => PullRequestState::Merged,
        Some("DECLINED" | "SUPERSEDED") => PullRequestState::Closed,
        _ => PullRequestState::Open,
    }
}

fn to_build_status(value: Option<&str>) -> PullRequestCheckStatus {
    match value.map(|value| trim_wire_string(value).to_uppercase()).as_deref() {
        Some("SUCCESSFUL") => PullRequestCheckStatus::Success,
        Some("FAILED") => PullRequestCheckStatus::Failure,
        Some("STOPPED") => PullRequestCheckStatus::Cancelled,
        Some("INPROGRESS") => PullRequestCheckStatus::Pending,
        _ => PullRequestCheckStatus::Neutral,
    }
}

/// A participant who has voted is the closest Bitbucket has to a review, so it reads as one in
/// the conversation. Participants who have only been added carry no verdict and are skipped.
fn to_reviews(id: i64, participants: &[Participant]) -> Vec<PullRequestComment> {
    participants
        .iter()
        .filter_map(|participant| {
            let author = to_actor(participant.user.as_ref())?;
            let voted_at = trimmed(participant.participated_on.as_deref())?;
            let review_state = trimmed(participant.state.as_deref()).or_else(|| (participant.approved == Some(true)).then(|| "approved".to_string()))?;
            Some(PullRequestComment {
                id: format!("{id}:{}", author.login.as_str()),
                kind: PullRequestCommentKind::Review,
                author: Some(author),
                body: String::new(),
                created_at: to_iso_utc(&voted_at),
                edited_at: None,
                url: None,
                path: None,
                review_state: Some(review_state),
                reactions: None,
            })
        })
        .collect()
}

struct Participant {
    user: Option<RawUser>,
    approved: Option<bool>,
    state: Option<String>,
    participated_on: Option<String>,
}

fn participant(value: &Value) -> R<Participant> {
    let o = fields(value)?;
    opt_nullable_str(o, "role")?;
    Ok(Participant {
        user: opt_nullable_user(o, "user")?,
        approved: opt_bool(o, "approved")?,
        state: opt_nullable_str(o, "state")?.map(str::to_string),
        participated_on: opt_nullable_str(o, "participated_on")?.map(str::to_string),
    })
}

fn pull_request(value: &Value) -> R<BitbucketPullRequest> {
    let o = fields(value)?;
    let id = req_int(o, "id")?;
    let title = req_str(o, "title")?.to_string();
    let description = opt_nullable_str(o, "description")?.map(str::to_string);
    let state = opt_nullable_str(o, "state")?;
    let draft = opt_bool(o, "draft")?;
    let author = opt_nullable_user(o, "author")?;
    let (head_branch, head_repository) = raw_branch(o.get("source"))?;
    let (base_branch, _) = raw_branch(o.get("destination"))?;
    let created_on = req_str(o, "created_on")?;
    let updated_on = req_str(o, "updated_on")?;
    let raw_reviewers = opt_nullable_array(o, "reviewers", raw_user)?;
    let participants = opt_nullable_array(o, "participants", participant)?;
    let links = fields(o.get("links").ok_or(())?)?;
    let url = trimmed_non_empty(fields(links.get("html").ok_or(())?)?, "href")?;
    // The wire contract carries a positive number only.
    let number = u64::try_from(id).ok().filter(|number| *number >= 1).ok_or(())?;
    let reviewers: Vec<PullRequestActor> = raw_reviewers.iter().filter_map(|reviewer| to_actor(Some(reviewer))).collect();
    Ok(BitbucketPullRequest {
        number,
        title,
        url,
        author: to_actor(author.as_ref()),
        head_branch,
        head_repository_name_with_owner: head_repository,
        base_branch,
        state: to_state(state),
        is_draft: draft.unwrap_or(false),
        mergeability: PullRequestMergeability::Unknown,
        created_at: to_iso_utc(created_on),
        updated_at: to_iso_utc(updated_on),
        body: description.unwrap_or_default(),
        review_request_logins: reviewers.iter().map(|reviewer| reviewer.login.as_str().to_string()).collect(),
        reviewers,
        reviewer_ids: raw_reviewers.iter().filter_map(|reviewer| trimmed(reviewer.uuid.as_deref())).collect(),
        reviews: to_reviews(id, &participants),
    })
}

// ---- pages -----------------------------------------------------------------------------------------

/// One page of a cursor-paged answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// The whole URL of the next page, which Bitbucket sends rather than an offset.
    pub next: Option<String>,
}

/// `RawPageSchema`: the entries are left undecoded for the caller to read one at a time.
fn raw_page(raw: &str) -> R<(Vec<Value>, Option<String>)> {
    let value = parse(raw)?;
    let o = fields(&value)?;
    let values = o.get("values").and_then(Value::as_array).ok_or(())?.clone();
    opt_nullable_int(o, "size")?;
    let next = opt_nullable_str(o, "next")?;
    Ok((values, trimmed(next)))
}

/// Malformed entries are skipped rather than failing the page, as on the other hosts.
pub fn decode_pull_request_page_json(raw: &str) -> R<Page<BitbucketPullRequest>> {
    let (values, next) = raw_page(raw)?;
    Ok(Page { items: values.iter().filter_map(|entry| pull_request(entry).ok()).collect(), next })
}

pub fn decode_pull_request_json(raw: &str) -> R<BitbucketPullRequest> {
    pull_request(&parse(raw)?)
}

/// The signed-in account's handle; `None` where Bitbucket names no account at all.
pub fn decode_viewer_json(raw: &str) -> R<Option<String>> {
    let value = parse(raw)?;
    let o = fields(&value)?;
    let nickname = opt_nullable_str(o, "nickname")?;
    let display_name = opt_nullable_str(o, "display_name")?;
    Ok(trimmed(nickname).or_else(|| trimmed(display_name)))
}

/// Whether the configured credentials can write to the repository, which is what merging needs.
/// Bitbucket answers `admin`, `write` or `read`, and an empty page means it named no permission at
/// all for this account: an unknown standing, which is granted rather than guessed away.
pub fn decode_repository_permission_json(raw: &str) -> R<bool> {
    let value = parse(raw)?;
    let o = fields(&value)?;
    let rows = opt_nullable_array(o, "values", |row| {
        let row = fields(row)?;
        Ok(opt_nullable_str(row, "permission")?.map(str::to_string))
    })?;
    let permission = rows.into_iter().next().flatten().and_then(|permission| trimmed(Some(&permission))).map(|permission| permission.to_lowercase());
    Ok(matches!(permission.as_deref(), None | Some("admin" | "write")))
}

/// The workspace's members, which is the nearest thing Bitbucket has to "who may review this".
/// Nothing on a repository lists the people with access to it, and a pull request can be sent to
/// anyone in the workspace. Nobody is marked requested here: who has been asked lives on the pull
/// request, and only the caller holds both.
pub fn decode_workspace_members_json(raw: &str) -> R<Page<PullRequestReviewerCandidate>> {
    let (values, next) = raw_page(raw)?;
    let items = values
        .iter()
        .filter_map(|entry| {
            let member = fields(entry).ok()?;
            let user = opt_nullable_user(member, "user").ok()?;
            let uuid = trimmed(user.as_ref()?.uuid.as_deref())?;
            let actor = to_actor(user.as_ref())?;
            Some(PullRequestReviewerCandidate {
                is_bot: None,
                login: actor.login.as_str().to_string(),
                name: actor.name,
                avatar_url: actor.avatar_url,
                id: uuid,
                kind: PullRequestReviewerKind::User,
                is_requested: false,
            })
        })
        .collect();
    Ok(Page { items, next })
}

// ---- comments --------------------------------------------------------------------------------------

/// One comment as Bitbucket sent it, kept so threads can be assembled across pages.
#[derive(Debug, Clone)]
pub struct RawComment {
    id: i64,
    raw_content: Option<String>,
    user: Option<RawUser>,
    created_on: String,
    /// The id of the comment this one answers, which may itself be a reply.
    parent: Option<i64>,
    path: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    outdated: Option<bool>,
    /// Non-null once someone has marked the thread resolved.
    resolved: bool,
    url: Option<String>,
}

fn raw_comment(value: &Value) -> R<(RawComment, bool, bool)> {
    let o = fields(value)?;
    let id = req_int(o, "id")?;
    let raw_content = match opt_nullable_object(o, "content")? {
        None => None,
        Some(content) => opt_str(content, "raw")?.map(str::to_string),
    };
    let user = opt_nullable_user(o, "user")?;
    let created_on = req_str(o, "created_on")?.to_string();
    let deleted = opt_bool(o, "deleted")?;
    let pending = opt_bool(o, "pending")?;
    let parent = match opt_nullable_object(o, "parent")? {
        None => None,
        Some(parent) => Some(req_int(parent, "id")?),
    };
    let (path, from, to, outdated) = match opt_nullable_object(o, "inline")? {
        None => (None, None, None, None),
        Some(inline) => (
            opt_nullable_str(inline, "path")?.map(str::to_string),
            opt_nullable_int(inline, "from")?,
            opt_nullable_int(inline, "to")?,
            opt_nullable_bool(inline, "outdated")?,
        ),
    };
    let resolved = !matches!(o.get("resolution"), None | Some(Value::Null));
    let url = match opt_nullable_object(o, "links")? {
        None => None,
        Some(links) => match opt_nullable_object(links, "html")? {
            None => None,
            Some(html) => opt_str(html, "href")?.map(str::to_string),
        },
    };
    Ok((RawComment { id, raw_content, user, created_on, parent, path, from, to, outdated, resolved, url }, deleted == Some(true), pending == Some(true)))
}

#[derive(Debug, Clone)]
pub struct BitbucketComments {
    pub comments: Vec<PullRequestComment>,
    /// The same comments unread, for `build_review_threads`. A reply and the remark it answers can
    /// land on different pages, and only the caller holding every page can put them together.
    pub entries: Vec<RawComment>,
    pub next: Option<String>,
}

/// Bitbucket returns one flat list, so a thread is reassembled from it: a comment pinned to a
/// line opens a thread, and every reply that leads back to it belongs in it. A reply whose parent
/// is on a page that was not read has nowhere to go, and is left out rather than shown as a thread
/// of its own; it still stands in the flat conversation, which needs no parent.
pub fn build_review_threads(comments: &[RawComment]) -> Vec<PullRequestReviewThread> {
    let by_id: HashMap<i64, &RawComment> = comments.iter().map(|comment| (comment.id, comment)).collect();
    let root_of = |comment: &'_ RawComment| -> i64 {
        let mut current = comment;
        // Bounded by the number of comments read, so a parent cycle cannot spin here.
        for _ in 0..by_id.len() {
            match current.parent.and_then(|parent| by_id.get(&parent)) {
                Some(parent) => current = parent,
                None => return current.id,
            }
        }
        current.id
    };
    let mut order: Vec<i64> = vec![];
    let mut threads: HashMap<i64, PullRequestReviewThread> = HashMap::new();
    let mut replies: HashMap<i64, Vec<&RawComment>> = HashMap::new();
    for comment in comments {
        let root_id = root_of(comment);
        let Some(root) = by_id.get(&root_id) else { continue };
        let Some(path) = trimmed(root.path.as_deref()) else { continue };
        if root.id == comment.id {
            // `to` is the line as the file stands now, `from` the line it replaced; a comment that
            // carries only `from` was written against the removed side.
            let side = if root.to.is_none() { PullRequestDiffSide::Left } else { PullRequestDiffSide::Right };
            let line = if side == PullRequestDiffSide::Left { root.from } else { root.to };
            order.push(root.id);
            threads.insert(
                root.id,
                PullRequestReviewThread {
                    id: root.id.to_string(),
                    path,
                    line: line.filter(|line| *line > 0).map(|line| line as u64),
                    side,
                    is_resolved: root.resolved,
                    is_outdated: root.outdated == Some(true),
                    comments: vec![],
                    comment_count: None,
                    next_comments_cursor: None,
                },
            );
        }
        replies.entry(root_id).or_default().push(comment);
    }
    order
        .into_iter()
        .filter_map(|id| {
            let thread = threads.remove(&id)?;
            let mut entries = replies.remove(&id).unwrap_or_default();
            entries.sort_by(|left, right| locale_cmp(&left.created_on, &right.created_on));
            let comments: Vec<PullRequestThreadComment> = entries
                .into_iter()
                .map(|comment| PullRequestThreadComment {
                    id: comment.id.to_string(),
                    author: to_actor(comment.user.as_ref()),
                    body: comment.raw_content.clone().unwrap_or_default(),
                    created_at: to_iso_utc(&comment.created_on),
                    edited_at: None,
                    url: trimmed(comment.url.as_deref()),
                    reactions: None,
                })
                .collect();
            (!comments.is_empty()).then_some(PullRequestReviewThread { comments, ..thread })
        })
        .collect()
}

/// Deleted comments and ones their author has not posted yet carry nothing to show. A comment
/// pinned to a file is a line-level review comment, which is what that kind means.
pub fn decode_comments_json(raw: &str) -> R<BitbucketComments> {
    let (values, next) = raw_page(raw)?;
    let (mut comments, mut kept) = (vec![], vec![]);
    for entry in &values {
        let Ok((comment, deleted, pending)) = raw_comment(entry) else { continue };
        if deleted || pending {
            continue;
        }
        let body = comment.raw_content.clone().unwrap_or_default();
        if trim_wire_string(&body).is_empty() {
            continue;
        }
        let path = trimmed(comment.path.as_deref());
        comments.push(PullRequestComment {
            id: comment.id.to_string(),
            kind: if path.is_none() { PullRequestCommentKind::IssueComment } else { PullRequestCommentKind::ReviewComment },
            author: to_actor(comment.user.as_ref()),
            body,
            created_at: to_iso_utc(&comment.created_on),
            edited_at: None,
            url: trimmed(comment.url.as_deref()),
            path,
            review_state: None,
            reactions: None,
        });
        kept.push(comment);
    }
    Ok(BitbucketComments { comments, entries: kept, next })
}

// ---- commits, statuses, diffstat, conflicts ---------------------------------------------------------

pub fn decode_commits_json(raw: &str) -> R<Page<PullRequestCommit>> {
    let (values, next) = raw_page(raw)?;
    let mut commits: Vec<PullRequestCommit> = values
        .iter()
        .filter_map(|entry| {
            let o = fields(entry).ok()?;
            let hash = trimmed_non_empty(o, "hash").ok()?;
            let message = opt_nullable_str(o, "message").ok()?;
            let date = opt_nullable_str(o, "date").ok()?;
            let (raw_author, linked) = match o.get("author") {
                None | Some(Value::Null) => (None, None),
                Some(author) => {
                    let author = fields(author).ok()?;
                    (opt_nullable_str(author, "raw").ok()?.map(str::to_string), opt_nullable_user(author, "user").ok()?)
                }
            };
            let committed_date = trimmed(date)?;
            let authors = match to_actor(linked.as_ref()) {
                Some(linked) => vec![linked],
                None => match trimmed(raw_author.as_deref()) {
                    None => vec![],
                    Some(raw) => vec![PullRequestActor { is_bot: None, login: TrimmedNonEmptyString::new(&raw).ok()?, name: Some(raw), avatar_url: None }],
                },
            };
            Some(PullRequestCommit {
                oid: hash,
                message_headline: message.unwrap_or("").split('\n').next().unwrap_or("").to_string(),
                committed_date: to_iso_utc(&committed_date),
                additions: None,
                deletions: None,
                authors: Some(authors),
            })
        })
        .collect();
    // Bitbucket lists a pull request's commits newest first; the timeline reads oldest first.
    commits.reverse();
    Ok(Page { items: commits, next })
}

pub fn decode_statuses_json(raw: &str) -> R<Page<PullRequestCheck>> {
    let (values, next) = raw_page(raw)?;
    let mut entries = vec![];
    for entry in &values {
        let Ok(o) = fields(entry) else { continue };
        let read = || -> R<(Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> {
            let text = |key: &str| Ok(opt_nullable_str(o, key)?.map(str::to_string));
            Ok((text("key")?, text("name")?, text("state")?, text("description")?, text("url")?))
        };
        let Ok((key, name, state, description, url)) = read() else { continue };
        let Some(name) = trimmed(name.as_deref()).or_else(|| trimmed(key.as_deref())) else { continue };
        // Bitbucket re-uses a status key when a pipeline is run again, so the same check can appear
        // twice on one page. Nothing decoded here says which copy is newer, so the later one wins,
        // which is the order Bitbucket writes an update in. The key is kept as the workflow name so
        // two different pipelines that display the same name are not folded into one.
        entries.push(CheckEntry {
            check: PullRequestCheck { name, status: to_build_status(state.as_deref()), description: trimmed(description.as_deref()), url: trimmed(url.as_deref()), required: None },
            workflow_name: trimmed(key.as_deref()),
            at: None,
        });
    }
    Ok(Page { items: dedupe_checks(entries), next })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
}

/// One entry per changed file, each carrying that file's line counts.
pub fn decode_diffstat_json(raw: &str) -> R<(DiffStat, Option<String>)> {
    let (values, next) = raw_page(raw)?;
    let mut total = DiffStat::default();
    for entry in &values {
        let Ok(o) = fields(entry) else { continue };
        let (Ok(added), Ok(removed)) = (opt_nullable_int(o, "lines_added"), opt_nullable_int(o, "lines_removed")) else { continue };
        total.additions += added.unwrap_or(0).max(0) as u64;
        total.deletions += removed.unwrap_or(0).max(0) as u64;
        total.changed_files += 1;
    }
    Ok((total, next))
}

/// The conflicts endpoint answers with one entry per conflicting path, so an empty page is the
/// only statement Bitbucket makes that a pull request merges cleanly.
pub fn decode_conflicts_json(raw: &str) -> R<PullRequestMergeability> {
    let (values, _) = raw_page(raw)?;
    Ok(if values.is_empty() { PullRequestMergeability::Mergeable } else { PullRequestMergeability::Conflicting })
}
