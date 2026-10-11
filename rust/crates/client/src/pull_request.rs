//! The rules behind the pull request panel: what the checks add up to, which merge methods a
//! repository allows, which control the header offers for a state, which actions a reader may
//! take, the question asked before an action that cannot be undone, and the words for what
//! happened. Port of `pullRequestDetail.logic.ts` and `usePullRequestActions.ts`; the tests
//! replay what the original answers. Everything reads the wire JSON, so a field the server adds
//! costs nothing here.
use serde_json::{Value, json};

pub const SUMMARY: &str = "pullRequests.summary";
pub const DETAIL: &str = "pullRequests.detail";
pub const CHECKS: &str = "pullRequests.checks";
pub const ACTIVITY: &str = "pullRequests.activity";
pub const RUN_ACTION: &str = "pullRequests.runAction";
pub const COMMENT: &str = "pullRequests.comment";
pub const LIST: &str = "pullRequests.list";
pub const LIST_STATS: &str = "pullRequests.listStats";
pub const LINKED_THREADS: &str = "pullRequests.linkedThreads";
pub const UPDATE: &str = "pullRequests.update";
pub const UPDATE_COMMENT: &str = "pullRequests.updateComment";
pub const SET_REACTION: &str = "pullRequests.setReaction";
pub const SUBSCRIBE_REFRESHES: &str = "pullRequests.subscribeRefreshes";
pub const INVALIDATE: &str = "pullRequests.invalidate";
pub const DIFF_FILE_CONTENTS: &str = "pullRequests.diffFileContents";
pub const FILES_VIEWED: &str = "pullRequests.filesViewed";
pub const SET_FILES_VIEWED: &str = "pullRequests.setFilesViewed";
/// The route that carries a pull request's patch, which travels over HTTP where it compresses.
pub const DIFF_PATH: &str = "/api/pull-requests/diff";

/// What the panel says for a host that cannot be read as a source of pull requests. It is a state,
/// not a failure: nothing is wrong, this host has no pull request support here.
pub const NOT_AVAILABLE_FOR_HOST: &str = "Pull requests are not available for this host.";

/// What the panel says of a pull request the host does not know (`PullRequestDetailPanel.tsx`).
pub const NOT_FOUND: &str = "Pull request not found. It may be an issue rather than a pull request, or this account can't see it.";

/// The words for a failed pull request call whose error carries a tag the generic reader would
/// not word: an unreadable host. `None` leaves the failure to the generic reader.
pub fn failure_message(cause: &Value) -> Option<String> {
    let errors: Vec<&Value> = match cause.as_array() {
        Some(causes) => causes.iter().map(|cause| &cause["error"]).collect(),
        None => vec![cause],
    };
    for error in errors {
        // The host answered that there is no such pull request: an issue number, or one this account cannot see.
        if error["_tag"] == "PullRequestOperationError" && error["reason"] == "not-found" {
            return Some(NOT_FOUND.to_owned());
        }
        if error["_tag"] != "PullRequestUnavailableError" {
            continue;
        }
        let provider = error["provider"].clone();
        return Some(match error["reason"].as_str() {
            Some("provider-unsupported") | None => NOT_AVAILABLE_FOR_HOST.to_owned(),
            Some(reason) => {
                let requirement = serde_json::from_value(provider).ok().zip(serde_json::from_value(json!(reason)).ok()).and_then(|(provider, reason)| t3_contracts::pull_request_provider_requirement(provider, reason));
                requirement.map_or_else(|| "This host has no working credentials.".to_owned(), str::to_owned)
            }
        });
    }
    None
}

/// The key of a pull request: `{projectId, host?, repository, number}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub project_id: String,
    pub host: Option<String>,
    pub repository: String,
    pub number: u64,
}
impl Reference {
    /// The reference a thread's link names (`pullRequests[]`, or the legacy `linkedPullRequest`).
    pub fn from_link(project_id: &str, link: &Value) -> Option<Self> {
        Some(Self { project_id: project_id.to_owned(), host: link["host"].as_str().map(str::to_owned), repository: link["repository"].as_str()?.to_owned(), number: link["number"].as_u64()? })
    }
    /// The request body, with what each call adds.
    pub fn payload(&self, extra: Value) -> Value {
        let mut payload = json!({"projectId":self.project_id,"repository":self.repository,"number":self.number});
        if let Some(host) = &self.host {
            payload["host"] = json!(host);
        }
        if let (Some(target), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        payload
    }
    /// The reference a pull request URL names, for a branch whose status reports one: host,
    /// `owner/repository` and number, for hosts whose paths read `/owner/repo/pull/N`.
    pub fn from_url(project_id: &str, url: &str, number: u64) -> Option<Self> {
        let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
        let (host, path) = rest.split_once('/')?;
        let segments: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();
        let at = segments.iter().position(|segment| matches!(*segment, "pull" | "pulls" | "merge_requests" | "pullrequests" | "pull-requests"))?;
        let repository = segments[..at].iter().filter(|segment| **segment != "-").copied().collect::<Vec<_>>().join("/");
        (!repository.is_empty()).then(|| Self { project_id: project_id.to_owned(), host: Some(host.to_owned()), repository, number })
    }
    pub fn key(&self) -> String {
        json!([self.project_id, self.host, self.repository, self.number]).to_string()
    }
}

/// The body of a file's contents read: the kind of change the diff shows and both paths.
pub fn contents_payload(reference: &Reference, file: &crate::diff::FileDiff) -> Value {
    let new_path = file.path();
    let old_path = file.old_path.as_deref().unwrap_or(new_path);
    let change = match file.status {
        crate::diff::FileStatus::Added => "new",
        crate::diff::FileStatus::Deleted => "deleted",
        crate::diff::FileStatus::Renamed if file.hunks.is_empty() => "rename-pure",
        crate::diff::FileStatus::Renamed => "rename-changed",
        _ => "change",
    };
    reference.payload(json!({"changeType":change,"oldPath":old_path,"newPath":new_path}))
}

/// Which files the reader has marked viewed, from a `filesViewed` answer. `dismissed` (the host
/// withdrew the mark because the file changed) reads as not viewed, as in the original.
pub fn viewed_paths(answer: &Value) -> std::collections::BTreeSet<String> {
    answer["files"].as_array().into_iter().flatten().filter(|file| file["state"] == "viewed").filter_map(|file| file["path"].as_str().map(str::to_owned)).collect()
}

/// Who keeps the reader's viewed marks: the host itself, this environment (a host with no record of its
/// own), or nobody, in which case no checkbox is offered. `capabilities.viewedFiles`.
pub fn viewed_store(detail: &Value) -> Option<&str> {
    detail["capabilities"]["viewedFiles"].as_str().filter(|store| matches!(*store, "host" | "environment"))
}
/// What the viewed count says beside the number of files: "2 / 5 viewed", and on a host with no record of
/// its own the marks are named as this app's.
pub fn viewed_summary(viewed: usize, files: usize, store: &str) -> String {
    format!("{viewed} / {files} {}", if store == "environment" { "viewed in T3 Code" } else { "viewed" })
}
pub const VIEWED_ENVIRONMENT_NOTE: &str = "This host keeps no shared record of which files you have read, so these ticks are kept by this environment. They follow you between the apps connected to it, but the host's own web UI will not show them.";

/// The host's own name for the link out (`openOnHostLabel`): "Open on GitLab".
pub fn open_on_host_label(provider: Option<&str>) -> String {
    match provider {
        Some("github") => "Open on GitHub",
        Some("gitlab") => "Open on GitLab",
        Some("forgejo") => "Open on Forgejo",
        Some("bitbucket") => "Open on Bitbucket",
        Some("azure-devops") => "Open on Azure DevOps",
        _ => "Open on host",
    }
    .to_owned()
}
/// The host's name for a row (`getSourceControlPresentationForKind(...).providerName`).
pub fn host_name(provider: Option<&str>) -> &'static str {
    match provider {
        Some("github") => "GitHub",
        Some("gitlab") => "GitLab",
        Some("forgejo") => "Forgejo",
        Some("bitbucket") => "Bitbucket",
        Some("azure-devops") => "Azure DevOps",
        _ => "",
    }
}
/// A change in size is drawn only when the host counted one: zero in both is how a host that does not
/// count says nothing, and "+0 -0" would be a claim.
pub fn has_size(additions: u64, deletions: u64) -> bool {
    additions + deletions > 0
}
/// What a row says of its checks, when the host summarises them (absent for hosts that do not).
pub fn checks_state_label(state: Option<&str>) -> Option<&'static str> {
    match state? {
        "passing" => Some("Checks passing"),
        "failing" => Some("Checks failing"),
        "pending" => Some("Checks running"),
        _ => None,
    }
}
/// What a row says of its review, when the host summarises it (GitHub alone does).
pub fn review_decision_label(decision: Option<&str>) -> Option<&'static str> {
    match decision? {
        "approved" => Some("Approved"),
        "changes-requested" => Some("Changes requested"),
        "review-required" => Some("Review required"),
        _ => None,
    }
}

/// The body of a `setFilesViewed` for one file.
pub fn set_viewed_payload(reference: &Reference, path: &str, viewed: bool) -> Value {
    reference.payload(json!({"files":[{"path":path,"viewed":viewed}]}))
}

/// The body of a patch read: the first page, or the page a cursor names.
pub fn diff_payload(reference: &Reference, cursor: Option<&str>) -> Value {
    reference.payload(match cursor {
        Some(cursor) => json!({"cursor":cursor}),
        None => json!({}),
    })
}

fn status_of(check: &Value) -> &str {
    check["status"].as_str().unwrap_or_default()
}

/// The checks as one word. Failing outranks running.
pub fn classify_checks(checks: &[Value]) -> &'static str {
    if checks.is_empty() {
        "none"
    } else if checks.iter().any(|check| matches!(status_of(check), "failure" | "cancelled")) {
        "failing"
    } else if checks.iter().any(|check| matches!(status_of(check), "pending" | "action-required")) {
        "pending"
    } else {
        "passing"
    }
}

/// The checks with every live facet: "7 of 16 running · 1 failed".
pub fn describe_checks(checks: &[Value]) -> String {
    if checks.is_empty() {
        return "No checks reported".into();
    }
    let count = |statuses: &[&str]| checks.iter().filter(|check| statuses.contains(&status_of(check))).count();
    let (failed, pending, action, passed, total) = (count(&["failure", "cancelled"]), count(&["pending"]), count(&["action-required"]), count(&["success"]), checks.len());
    let mut parts: Vec<String> = Vec::new();
    if pending > 0 {
        parts.push(format!("{pending} of {total} running"));
    }
    if action > 0 {
        parts.push(format!("{action} of {total} awaiting action"));
    }
    if failed > 0 {
        parts.push(if parts.is_empty() { format!("{failed} of {total} failing") } else { format!("{failed} failed") });
    }
    if parts.is_empty() {
        return if passed == total { "All checks passed".into() } else { format!("{passed} of {total} passing") };
    }
    parts.join(" \u{b7} ")
}

/// Checks that want attention, checks running, and checks done.
pub fn group_checks(checks: &[Value]) -> [Vec<&Value>; 3] {
    let pick = |statuses: &[&str]| checks.iter().filter(|check| statuses.contains(&status_of(check))).collect::<Vec<_>>();
    [pick(&["failure", "cancelled", "action-required"]), pick(&["pending"]), pick(&["success", "skipped", "neutral"])]
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(|item| item.as_str().map(str::to_owned)).collect()
}

/// The merge methods the host supports and the repository allows.
pub fn allowed_merge_methods(detail: &Value) -> Vec<String> {
    strings(&detail["capabilities"]["mergeMethods"]).into_iter().filter(|method| detail["mergeCapabilities"][method.as_str()] == true).collect()
}

/// The reader's preference where the repository allows it, else the first allowed method.
pub fn resolve_merge_method(allowed: &[String], preferred: &str) -> String {
    if allowed.iter().any(|method| method == preferred) { preferred.to_owned() } else { allowed.first().cloned().unwrap_or_else(|| "merge".into()) }
}

pub fn merge_method_label(method: &str) -> &'static str {
    match method {
        "squash" => "Squash and merge",
        "rebase" => "Rebase and merge",
        _ => "Merge",
    }
}

/// Whether this host can do the action at all, and whether this account may.
pub fn can_perform(detail: &Value, action: &str) -> bool {
    strings(&detail["capabilities"]["actions"]).iter().any(|candidate| candidate == action) && strings(&detail["viewerPermissions"]["actions"]).iter().any(|candidate| candidate == action)
}

pub fn conflicting(detail: &Value) -> bool {
    detail["state"] == "open" && detail["mergeability"] == "conflicting"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryInput<'a> {
    pub state: &'a str,
    pub is_draft: bool,
    pub mergeability: &'a str,
    pub checks_state: Option<&'a str>,
    pub auto_merge_enabled: Option<bool>,
    pub has_merge_method: bool,
    pub can_merge: bool,
    pub can_mark_ready: bool,
    pub can_enable_auto_merge: bool,
}

/// The one merge-area state shown in the header, terminal and deferred states included.
pub fn primary_control(input: &PrimaryInput<'_>) -> Option<&'static str> {
    if input.state == "merged" {
        return Some("merged");
    }
    if input.state == "closed" {
        return Some("closed");
    }
    if input.mergeability == "conflicting" {
        return Some("resolve");
    }
    if input.is_draft {
        return input.can_mark_ready.then_some("ready");
    }
    if input.auto_merge_enabled == Some(true) {
        return Some("auto-merge-armed");
    }
    if !input.has_merge_method {
        return None;
    }
    if input.auto_merge_enabled == Some(false) && input.checks_state.is_some_and(|state| state != "passing") && input.can_enable_auto_merge {
        return Some("enable-auto-merge");
    }
    input.can_merge.then_some("merge")
}

/// The control for a loaded detail, from the detail's own fields.
pub fn primary_for(detail: &Value) -> Option<&'static str> {
    primary_for_gated(detail, true)
}
/// As `primary_for`, with merging a single pull request switched off (`canMergeSinglePullRequest`): where a host acts on
/// stacks, a pull request is merged on its own only once the stack lookup has found it in none.
pub fn primary_for_gated(detail: &Value, single_merge: bool) -> Option<&'static str> {
    let checks = detail["checks"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    primary_control(&PrimaryInput {
        state: detail["state"].as_str().unwrap_or("open"),
        is_draft: detail["isDraft"] == true,
        mergeability: detail["mergeability"].as_str().unwrap_or("unknown"),
        checks_state: if checks.is_empty() { None } else { Some(classify_checks(checks)) },
        auto_merge_enabled: detail["autoMergeEnabled"].as_bool(),
        has_merge_method: !allowed_merge_methods(detail).is_empty(),
        can_merge: single_merge && can_perform(detail, "merge"),
        can_mark_ready: can_perform(detail, "ready"),
        can_enable_auto_merge: single_merge && can_perform(detail, "enable-auto-merge"),
    })
}

/// A branch that is behind its base but cleanly mergeable: how far, and the methods this reader
/// may update it with (empty where it is news and not an offer).
pub fn base_freshness(detail: &Value) -> Option<(Option<u64>, Vec<String>)> {
    if detail["state"] != "open" || detail["baseComparison"] != "behind" || detail["mergeability"] != "mergeable" {
        return None;
    }
    let allowed = strings(&detail["viewerPermissions"]["updateMethods"]);
    Some((detail["behindBy"].as_u64(), strings(&detail["capabilities"]["updateMethods"]).into_iter().filter(|method| allowed.contains(method)).collect()))
}

/// What a confirmation asks, and what its button says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    pub title: &'static str,
    pub description: String,
    pub button: String,
    pub destructive: bool,
}

/// The question asked before an action, in the original's words; `None` for the actions that run
/// at once (reopen, ready, draft, disable auto-merge, update branch).
pub fn confirmation(action: &str, number: u64, method: &str, workflows: u64) -> Option<Confirmation> {
    let label = merge_method_label(method);
    Some(match action {
        "merge" => Confirmation { title: "Merge pull request?", description: format!("This merges #{number} using {method}."), button: label.to_owned(), destructive: false },
        "enable-auto-merge" => Confirmation {
            title: "Enable auto-merge?",
            description: format!("This merges #{number} using {method} as soon as the host considers it ready, which may be immediately."),
            button: "Enable auto-merge".into(),
            destructive: false,
        },
        "revert" => Confirmation { title: "Revert these changes?", description: format!("This opens a new pull request that reverses the changes merged by #{number}."), button: "Create revert PR".into(), destructive: false },
        "approve-workflows" => Confirmation {
            title: "Approve workflows to run?",
            description: format!("This allows {workflows} {} from #{number} to run. Review the code and workflow changes first.", if workflows == 1 { "workflow" } else { "workflows" }),
            button: "Approve and run".into(),
            destructive: false,
        },
        "close" => Confirmation { title: "Close pull request?", description: format!("This closes #{number} without merging it."), button: "Close".into(), destructive: true },
        _ => return None,
    })
}

pub fn success_label(action: &str) -> &'static str {
    match action {
        "merge" => "Merge requested",
        "ready" => "Marked ready for review",
        "draft" => "Converted to draft",
        "close" => "Pull request closed",
        "reopen" => "Pull request reopened",
        "update-branch" => "Branch updated with the base branch",
        "enable-auto-merge" => "Auto-merge enabled",
        "disable-auto-merge" => "Auto-merge disabled",
        "revert" => "Revert pull request opened",
        "approve-workflows" => "Workflows approved",
        _ => "Done",
    }
}

/// Said as the thing that did not happen.
pub fn failure_label(action: &str) -> &'static str {
    match action {
        "merge" => "Could not merge this pull request",
        "ready" => "Could not mark this ready for review",
        "draft" => "Could not convert this to a draft",
        "close" => "Could not close this pull request",
        "reopen" => "Could not reopen this pull request",
        "update-branch" => "Could not update this branch",
        "enable-auto-merge" => "Could not enable auto-merge",
        "disable-auto-merge" => "Could not disable auto-merge",
        "revert" => "Could not open a revert pull request",
        "approve-workflows" => "Could not approve workflows",
        _ => "The action failed",
    }
}

/// What to try, for the times the host says only that it refused.
pub fn failure_hint(action: &str) -> &'static str {
    match action {
        "merge" => "The host refused the merge. Check that you have write access, that the checks it requires have passed, and that the branch is not conflicting.",
        "ready" | "draft" | "disable-auto-merge" => "The host refused it. Check that you have write access to this repository.",
        "close" => "The host refused it. Check that you have write access, or that you opened it.",
        "reopen" => "The host refused it. Check that you have write access, and that the branch still exists.",
        "update-branch" => "The host refused it. Check that you have write access, and that the base branch has not diverged in a way the host cannot merge.",
        "enable-auto-merge" => "The host refused it. Check that auto-merge is enabled for this repository and that you have write access.",
        "revert" => "The host refused it. Check that you have write access and that this pull request was merged on the host.",
        "approve-workflows" => "The host refused it. Check that you have Actions write access and that these workflow runs are still awaiting approval.",
        _ => "The host refused it.",
    }
}

/// `readableFailure`: the host's own sentence when it said something, else the hint.
pub fn readable_failure(raw: &str, hint: &str) -> String {
    let detail = strip_operation_prefix(raw).trim().to_owned();
    let lower = detail.to_lowercase();
    let noise = detail.is_empty() || tool_noise(&lower);
    if noise {
        return hint.to_owned();
    }
    let units: Vec<u16> = detail.encode_utf16().collect();
    if units.len() <= 320 { detail } else { format!("{}\u{2026}", String::from_utf16_lossy(&units[..319])) }
}

/// Sentences that report only that a tool exited: true, and no help at all.
fn tool_noise(lower: &str) -> bool {
    let mut rest = lower;
    for host in ["github", "gitlab", "bitbucket", "azure devops"] {
        if let Some(after) = rest.strip_prefix(host) {
            rest = after;
            break;
        }
    }
    rest = rest.trim_start();
    for word in ["cli", "api"] {
        if let Some(after) = rest.strip_prefix(word) {
            rest = after;
            break;
        }
    }
    rest = rest.trim_start();
    if let Some(after) = rest.strip_prefix("command") {
        rest = after.trim_start();
    }
    if rest == "failed" || rest == "failed." {
        return true;
    }
    let exited = lower.strip_prefix("exited with ").or_else(|| lower.strip_prefix("exit with "));
    if let Some(after) = exited.and_then(|after| after.strip_prefix("code ").or_else(|| after.strip_prefix("status "))) {
        let digits = after.strip_suffix('.').unwrap_or(after);
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    lower == "unknown error" || lower == "unknown error."
}

/// The sentence of a failed operation, without the words that name it: `PullRequestOperationError.detail`.
pub fn operation_detail(raw: &str) -> &str {
    strip_operation_prefix(raw)
}

/// `^Pull request operation \w+ failed:\s*`, case-insensitively.
fn strip_operation_prefix(raw: &str) -> &str {
    let lower = raw.to_lowercase();
    let Some(rest) = lower.strip_prefix("pull request operation ") else { return raw };
    let word: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    let after = &rest[word.len()..];
    if word.is_empty() || !after.starts_with(" failed:") {
        return raw;
    }
    let consumed = lower.len() - after.len() + " failed:".len();
    raw.get(consumed..).map_or(raw, str::trim_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Vec<Value> {
        serde_json::from_str(include_str!("../tests/fixtures/pull_request_logic.json")).unwrap()
    }
    fn checks_of(statuses: &Value) -> Vec<Value> {
        statuses.as_array().unwrap().iter().map(|status| json!({"name":status,"status":status,"description":null,"url":null})).collect()
    }

    #[test]
    fn checks_add_up_as_the_original_adds_them() {
        let mut seen = 0;
        for row in fixtures().iter().filter(|row| row["type"] == "checks") {
            let checks = checks_of(&row["statuses"]);
            assert_eq!(classify_checks(&checks), row["state"], "{row}");
            assert_eq!(describe_checks(&checks), row["text"].as_str().unwrap(), "{row}");
            let groups = group_checks(&checks);
            assert_eq!(json!([groups[0].len(), groups[1].len(), groups[2].len()]), row["groups"], "{row}");
            seen += 1;
        }
        assert!(seen > 10);
    }

    #[test]
    fn the_header_control_follows_the_original_for_every_state() {
        let mut seen = 0;
        for row in fixtures().iter().filter(|row| row["type"] == "primary") {
            let input = &row["input"];
            let answer = primary_control(&PrimaryInput {
                state: input["state"].as_str().unwrap(),
                is_draft: input["isDraft"].as_bool().unwrap(),
                mergeability: input["mergeability"].as_str().unwrap(),
                checks_state: input["checksState"].as_str(),
                auto_merge_enabled: input["autoMergeEnabled"].as_bool(),
                has_merge_method: input["hasMergeMethod"].as_bool().unwrap(),
                can_merge: input["canMerge"].as_bool().unwrap(),
                can_mark_ready: input["canMarkReady"].as_bool().unwrap(),
                can_enable_auto_merge: input["canEnableAutoMerge"].as_bool().unwrap(),
            });
            assert_eq!(answer, row["answer"].as_str(), "{input}");
            seen += 1;
        }
        assert!(seen > 2000);
    }

    #[test]
    fn merge_methods_and_the_selected_one_follow_the_original() {
        for row in fixtures().iter().filter(|row| row["type"] == "methods") {
            let detail = json!({"capabilities":{"mergeMethods":row["methods"]},"mergeCapabilities":row["allowed"]});
            let list = allowed_merge_methods(&detail);
            assert_eq!(json!(list), row["list"], "{row}");
            let picks: Vec<String> = ["merge", "squash", "rebase"].iter().map(|preferred| resolve_merge_method(&list, preferred)).collect();
            assert_eq!(json!(picks), row["picks"], "{row}");
        }
    }

    #[test]
    fn a_stale_branch_is_news_or_an_offer_as_the_original_decides() {
        for row in fixtures().iter().filter(|row| row["type"] == "freshness") {
            let input = &row["input"];
            let detail = json!({"state":input["state"],"mergeability":input["mergeability"],"baseComparison":input.get("baseComparison"),"behindBy":input.get("behindBy"),"capabilities":input["capabilities"],"viewerPermissions":input["viewerPermissions"]});
            let answer = base_freshness(&detail).map(|(behind, methods)| json!({"behindBy":behind,"methods":methods}));
            assert_eq!(answer.unwrap_or(Value::Null), row["answer"], "{input}");
        }
    }

    #[test]
    fn a_failure_is_the_hosts_sentence_or_the_hint() {
        for row in fixtures().iter().filter(|row| row["type"] == "failure") {
            assert_eq!(readable_failure(row["message"].as_str().unwrap(), "HINT"), row["answer"].as_str().unwrap(), "{row}");
        }
    }

    #[test]
    fn the_actions_that_cannot_be_undone_ask_first_in_the_originals_words() {
        let merge = confirmation("merge", 7, "squash", 0).unwrap();
        assert_eq!((merge.title, merge.description.as_str(), merge.button.as_str()), ("Merge pull request?", "This merges #7 using squash.", "Squash and merge"));
        let auto = confirmation("enable-auto-merge", 7, "merge", 0).unwrap();
        assert_eq!(auto.description, "This merges #7 using merge as soon as the host considers it ready, which may be immediately.");
        assert_eq!(confirmation("revert", 7, "merge", 0).unwrap().description, "This opens a new pull request that reverses the changes merged by #7.");
        assert_eq!(confirmation("approve-workflows", 7, "merge", 1).unwrap().description, "This allows 1 workflow from #7 to run. Review the code and workflow changes first.");
        assert!(confirmation("approve-workflows", 7, "merge", 3).unwrap().description.contains("3 workflows"));
        let close = confirmation("close", 7, "merge", 0).unwrap();
        assert_eq!((close.title, close.description.as_str(), close.button.as_str(), close.destructive), ("Close pull request?", "This closes #7 without merging it.", "Close", true));
        for action in ["reopen", "ready", "draft", "disable-auto-merge", "update-branch"] {
            assert!(confirmation(action, 7, "merge", 0).is_none(), "{action} runs at once");
        }
    }

    #[test]
    fn an_action_needs_both_the_host_and_the_reader() {
        let detail = json!({"capabilities":{"actions":["merge","close"]},"viewerPermissions":{"actions":["close","reopen"]}});
        assert!(can_perform(&detail, "close") && !can_perform(&detail, "merge") && !can_perform(&detail, "reopen"));
    }

    #[test]
    fn a_status_url_names_the_pull_request() {
        let reference = Reference::from_url("p", "https://github.com/acme/web/pull/7", 7).unwrap();
        assert_eq!((reference.host.as_deref(), reference.repository.as_str(), reference.number), (Some("github.com"), "acme/web", 7));
        assert_eq!(Reference::from_url("p", "https://gitlab.com/group/sub/project/-/merge_requests/3", 3).unwrap().repository, "group/sub/project");
        assert!(Reference::from_url("p", "not a url", 1).is_none());
    }

    /// Every host's unreadable state names its own fix: Bitbucket has no command line, so it points at the
    /// settings page where its tokens are kept and never at another host's `auth login`.
    #[test]
    fn an_unreadable_host_names_its_own_remedy() {
        let message = |provider: &str, reason: &str| failure_message(&json!([{"_tag":"Fail","error":{"_tag":"PullRequestUnavailableError","reason":reason,"provider":provider}}])).unwrap();
        for reason in ["cli-unauthenticated", "cli-missing"] {
            let text = message("bitbucket", reason);
            assert!(text.contains("Settings") && text.contains("Source Control") && !text.contains("auth login") && !text.contains("glab") && !text.contains("`gh`"), "{reason}: {text}");
        }
        assert!(message("gitlab", "cli-unauthenticated").contains("glab auth login"));
        assert!(message("azure-devops", "cli-unauthenticated").contains("az login"));
        assert!(message("forgejo", "cli-unauthenticated").contains("fj"));
    }

    #[test]
    fn an_unsupported_host_is_a_state_and_other_failures_are_left_to_the_generic_reader() {
        let unsupported = json!([{"_tag":"Fail","error":{"_tag":"PullRequestUnavailableError","reason":"provider-unsupported"}}]);
        assert_eq!(failure_message(&unsupported).as_deref(), Some(NOT_AVAILABLE_FOR_HOST));
        let missing = json!([{"_tag":"Fail","error":{"_tag":"PullRequestUnavailableError","reason":"cli-unauthenticated","provider":"github"}}]);
        assert!(failure_message(&missing).unwrap().contains("gh auth login"));
        assert_eq!(failure_message(&json!([{"_tag":"Fail","error":{"_tag":"PullRequestOperationError","detail":"x"}}])), None);
        // Not found is the one operation failure with words of its own; the wire carries it as a reason.
        assert_eq!(failure_message(&json!([{"_tag":"Fail","error":{"_tag":"PullRequestOperationError","detail":"x","reason":"not-found"}}])).as_deref(), Some(NOT_FOUND));
    }

    #[test]
    fn a_reference_carries_the_key_the_server_wants() {
        let reference = Reference::from_link("p", &json!({"host":"github.com","repository":"acme/web","number":7,"url":"u"})).unwrap();
        assert_eq!(reference.payload(json!({"allowStale":false})), json!({"projectId":"p","host":"github.com","repository":"acme/web","number":7,"allowStale":false}));
        let legacy = Reference::from_link("p", &json!({"repository":"acme/web","number":7})).unwrap();
        assert_eq!(legacy.payload(json!({})), json!({"projectId":"p","repository":"acme/web","number":7}));
        assert_ne!(reference.key(), legacy.key());
    }
}

/// The input of `pullRequests.list` for a state, continuing from the cursors a page returned.
pub fn list_input(state: &str, cursors: &std::collections::BTreeMap<String, String>) -> Value {
    let mut input = json!({"state":state,"limit":50});
    if !cursors.is_empty() {
        input["cursors"] = json!(cursors);
    }
    input
}
/// The pull requests of a page as `listStats` wants them named.
pub fn stats_input(entries: &[Value]) -> Value {
    json!({"refs":entries.iter().map(|entry| json!({"projectId":entry["projectId"],"host":entry["host"],"repository":entry["repository"],"number":entry["number"]})).collect::<Vec<_>>()})
}
/// What a row says of its state.
pub fn list_state_label(entry: &Value) -> &'static str {
    match (entry["state"].as_str(), entry["isDraft"] == true) {
        (Some("merged"), _) => "Merged",
        (Some("closed"), _) => "Closed",
        (_, true) => "Draft",
        _ => "Open",
    }
}

#[cfg(test)]
mod host_tests {
    use super::*;

    #[test]
    fn a_host_names_itself_and_says_who_keeps_the_viewed_marks() {
        assert_eq!(open_on_host_label(Some("gitlab")), "Open on GitLab");
        assert_eq!(open_on_host_label(Some("azure-devops")), "Open on Azure DevOps");
        assert_eq!(open_on_host_label(Some("unknown")), "Open on host");
        assert_eq!(open_on_host_label(None), "Open on host");
        assert_eq!((host_name(Some("github")), host_name(Some("bitbucket")), host_name(Some("unknown"))), ("GitHub", "Bitbucket", ""));
        let detail = |store: Value| json!({"capabilities":{"viewedFiles":store}});
        assert_eq!((viewed_store(&detail(json!("host"))), viewed_store(&detail(json!("environment"))), viewed_store(&json!({"capabilities":{}}))), (Some("host"), Some("environment"), None));
        assert_eq!(viewed_summary(2, 5, "host"), "2 / 5 viewed");
        assert_eq!(viewed_summary(0, 5, "environment"), "0 / 5 viewed in T3 Code");
    }

    #[test]
    fn a_size_or_a_summary_the_host_did_not_give_is_left_out_rather_than_zero() {
        assert!(!has_size(0, 0) && has_size(1, 0) && has_size(0, 4));
        assert_eq!((checks_state_label(None), checks_state_label(Some("failing")), checks_state_label(Some("pending"))), (None, Some("Checks failing"), Some("Checks running")));
        assert_eq!((review_decision_label(None), review_decision_label(Some("changes-requested"))), (None, Some("Changes requested")));
    }
}

/// What the head of a conversation entry says the author did: "commented", "reviewed", "approved",
/// "requested changes", or, for a comment on a line, where.
pub fn comment_verb(kind: &str, review_state: Option<&str>, path: Option<&str>) -> String {
    match kind {
        "review" => match review_state.map(str::to_ascii_lowercase).as_deref() {
            Some("approved") => "approved".into(),
            Some("changes_requested") | Some("changes-requested") => "requested changes".into(),
            Some("dismissed") => "review dismissed".into(),
            _ => "reviewed".into(),
        },
        "review-comment" => match path.filter(|path| !path.is_empty()) {
            Some(path) => format!("commented on {path}"),
            None => "commented".into(),
        },
        _ => "commented".into(),
    }
}

/// A commit id as the panel shows it: the first seven characters.
pub fn short_oid(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

#[cfg(test)]
mod conversation_tests {
    use super::*;

    #[test]
    fn an_entry_says_what_its_author_did() {
        assert_eq!(comment_verb("issue-comment", None, None), "commented");
        assert_eq!(comment_verb("review-comment", None, Some("src/a.ts")), "commented on src/a.ts");
        assert_eq!(comment_verb("review-comment", None, None), "commented");
        assert_eq!((comment_verb("review", Some("APPROVED"), None), comment_verb("review", Some("CHANGES_REQUESTED"), None), comment_verb("review", Some("COMMENTED"), None), comment_verb("review", None, None)), ("approved".to_owned(), "requested changes".to_owned(), "reviewed".to_owned(), "reviewed".to_owned()));
        assert_eq!((short_oid("e4d3c2b1a0"), short_oid("abc")), ("e4d3c2b", "abc"));
    }
}
