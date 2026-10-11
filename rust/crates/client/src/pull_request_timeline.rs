//! What the pull request panel's Summary and Timeline tabs are made of: the conversation split into the comments that are still
//! live, the ones a resolved thread or a dismissed review finished, and the bots', and the timeline of what happened in order.
//! Port of `buildPullRequestTimeline`, `groupPullRequestTimelineConversations`, `pullRequestReviewOutcome`,
//! `isPullRequestVerdictStale`, `visibleBody`, `orderPullRequestComments` (`pullRequestDetail.logic.ts`), the comment split of
//! `PullRequestSummaryTab.tsx`, and the check wording of `pullRequestPresentation.tsx`.
use serde_json::Value;

/// How many comments the Summary tab draws before "Show older".
pub const COMMENT_PAGE: usize = 10;

/// `visibleBody`: bots keep their bookkeeping in HTML comments, which a markdown renderer drops, so a body that is nothing else
/// is no body at all.
pub fn visible_body(body: &str) -> Option<String> {
    let mut stripped = String::new();
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        stripped.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    stripped.push_str(rest);
    if stripped.trim().is_empty() { None } else { Some(body.trim().to_owned()) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Approved,
    ChangesRequested,
    Dismissed,
}
impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Approved => "Approved",
            Outcome::ChangesRequested => "Changes requested",
            Outcome::Dismissed => "Review dismissed",
        }
    }
    pub fn class(self) -> &'static str {
        match self {
            Outcome::Approved => "approved",
            Outcome::ChangesRequested => "changes-requested",
            Outcome::Dismissed => "dismissed",
        }
    }
}
/// `pullRequestReviewOutcome`: hosts spell the three verdicts differently, so case and separator are ignored; anything else
/// (GitHub's `COMMENTED`) is not a verdict.
pub fn review_outcome(review_state: Option<&str>) -> Option<Outcome> {
    match review_state?.trim().to_lowercase().replace('_', "-").as_str() {
        "approved" => Some(Outcome::Approved),
        "changes-requested" => Some(Outcome::ChangesRequested),
        "dismissed" => Some(Outcome::Dismissed),
        _ => None,
    }
}
/// The label of a verdict that commits have since overtaken.
pub fn stale_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Approved => "Approved before the latest commits",
        Outcome::ChangesRequested => "Changes requested before the latest commits",
        Outcome::Dismissed => "Review dismissed",
    }
}

fn instant(at: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(at).ok().map(|at| at.timestamp_millis())
}
/// `newestPullRequestCommitAt`.
pub fn newest_commit_at(commits: &[Value]) -> Option<String> {
    commits.iter().filter_map(|commit| commit["committedDate"].as_str().and_then(|at| instant(at).map(|millis| (millis, at)))).max_by_key(|(millis, _)| *millis).map(|(_, at)| at.to_owned())
}
/// `isPullRequestVerdictStale`: a verdict given before the code it was given on.
pub fn verdict_stale(at: &str, newest_commit: Option<&str>) -> bool {
    matches!((instant(at), newest_commit.and_then(instant)), (Some(verdict), Some(commit)) if verdict < commit)
}

/// `latestPullRequestReviewOutcomes`: each reviewer's last verdict (reviewers by lowercased login, so `Octocat` and
/// `octocat` are one person), and whether commits have overtaken it. The newest verdict wins; a comment with no verdict
/// is not one.
pub fn latest_outcomes(comments: &[Value], commits: &[Value]) -> std::collections::BTreeMap<String, (Outcome, bool)> {
    let newest = newest_commit_at(commits);
    let mut latest: std::collections::BTreeMap<String, (String, Outcome)> = Default::default();
    for comment in comments {
        let Some(outcome) = review_outcome(comment["reviewState"].as_str()) else { continue };
        let who = comment["author"]["login"].as_str().map_or_else(|| format!("ghost:{}", comment["id"]), str::to_lowercase);
        let at = comment["createdAt"].as_str().unwrap_or_default().to_owned();
        if latest.get(&who).is_some_and(|(seen, _)| *seen > at) {
            continue;
        }
        latest.insert(who, (at, outcome));
    }
    latest.into_iter().map(|(who, (at, outcome))| (who, (outcome, verdict_stale(&at, newest.as_deref())))).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Opened,
    Commit,
    Comment,
    Review,
    Merged,
    Closed,
}
/// One line of the timeline, with the remark behind it for the entries that are remarks.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: String,
    pub at: String,
    pub kind: Kind,
    pub actor: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub url: Option<String>,
    pub path: Option<String>,
    pub review_state: Option<String>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    /// The comment as the host sent it; none for a commit or a lifecycle entry.
    pub comment: Option<Value>,
}

/// `buildPullRequestTimeline`: opening, commits, comments and reviews, and how it ended, newest first.
pub fn build(detail: &Value, commits: &[Value], comments: &[Value]) -> Vec<Event> {
    let mut events = vec![Event {
        id: "created".into(),
        at: detail["createdAt"].as_str().unwrap_or_default().to_owned(),
        kind: Kind::Opened,
        actor: detail["author"]["login"].as_str().map(str::to_owned),
        title: "opened this pull request".into(),
        body: None,
        url: None,
        path: None,
        review_state: None,
        additions: None,
        deletions: None,
        comment: None,
    }];
    for commit in commits {
        let oid = commit["oid"].as_str().unwrap_or_default();
        events.push(Event {
            id: oid.to_owned(),
            at: commit["committedDate"].as_str().unwrap_or_default().to_owned(),
            kind: Kind::Commit,
            actor: commit["authors"][0]["login"].as_str().map(str::to_owned),
            title: format!("Commit {}", crate::pull_request::short_oid(oid)),
            body: commit["messageHeadline"].as_str().filter(|headline| !headline.is_empty()).map(str::to_owned),
            url: None,
            path: None,
            review_state: None,
            additions: commit["additions"].as_i64(),
            deletions: commit["deletions"].as_i64(),
            comment: None,
        });
    }
    for comment in comments {
        let review = comment["kind"] == "review";
        events.push(Event {
            id: comment["id"].as_str().unwrap_or_default().to_owned(),
            at: comment["createdAt"].as_str().unwrap_or_default().to_owned(),
            kind: if review { Kind::Review } else { Kind::Comment },
            actor: comment["author"]["login"].as_str().map(str::to_owned),
            title: if review { "reviewed" } else { "commented" }.into(),
            body: visible_body(comment["body"].as_str().unwrap_or_default()),
            url: comment["url"].as_str().map(str::to_owned),
            path: comment["path"].as_str().filter(|path| !path.is_empty()).map(str::to_owned),
            review_state: comment["reviewState"].as_str().map(str::to_owned),
            additions: None,
            deletions: None,
            comment: Some(comment.clone()),
        });
    }
    // Merged wins over closed: a host stamps both on a merge, and "closed" would misstate what happened.
    let ending = |id: &str, at: &Value, kind, title: &str| Event { id: id.into(), at: at.as_str().unwrap_or_default().to_owned(), kind, actor: None, title: title.into(), body: None, url: None, path: None, review_state: None, additions: None, deletions: None, comment: None };
    if detail["mergedAt"].as_str().is_some_and(|at| !at.is_empty()) {
        events.push(ending("merged", &detail["mergedAt"], Kind::Merged, "Pull request merged"));
    } else if detail["closedAt"].as_str().is_some_and(|at| !at.is_empty()) {
        events.push(ending("closed", &detail["closedAt"], Kind::Closed, "Pull request closed"));
    }
    // The original sorts the text of the stamps; hosts return the same ISO form, so this is the same order.
    events.sort_by(|left, right| right.at.cmp(&left.at));
    events
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Event(Event),
    /// Consecutive remarks, which fold into one conversation.
    Comments(Vec<Event>),
}
/// `groupPullRequestTimelineConversations`: commits, verdicts and lifecycle entries are rows of their own and split the
/// conversations; a verdict is never folded into "9 comments".
pub fn group(events: Vec<Event>) -> Vec<Row> {
    let mut rows: Vec<Row> = vec![];
    for event in events {
        let remark = matches!(event.kind, Kind::Comment | Kind::Review) && review_outcome(event.review_state.as_deref()).is_none();
        if remark {
            if let Some(Row::Comments(last)) = rows.last_mut() {
                last.push(event);
            } else {
                rows.push(Row::Comments(vec![event]));
            }
        } else {
            rows.push(Row::Event(event));
        }
    }
    rows
}
/// The authors of a conversation, once each, in the order they first spoke.
pub fn authors(events: &[Event]) -> Vec<String> {
    let mut seen: Vec<String> = vec![];
    for login in events.iter().filter_map(|event| event.actor.clone()) {
        if !seen.contains(&login) {
            seen.push(login);
        }
    }
    seen
}

/// The comments of the Summary tab in three piles (`PullRequestSummaryTab`): live ones, the ones a resolved thread or a
/// dismissed review finished, and the bots'. A comment that lives on a review thread is judged by that thread.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Comments {
    pub active: Vec<Value>,
    pub finished: Vec<Value>,
    pub bots: Vec<Value>,
}
pub fn partition_comments(comments: &[Value], threads: &[Value]) -> Comments {
    let resolved = |comment: &Value| {
        threads.iter().any(|thread| thread["isResolved"] == true && thread["comments"].as_array().into_iter().flatten().any(|entry| entry["id"] == comment["id"]))
    };
    let mut piles = Comments::default();
    for comment in comments {
        let finished = resolved(comment) || review_outcome(comment["reviewState"].as_str()) == Some(Outcome::Dismissed);
        let bot = comment["author"]["isBot"] == true || comment["author"]["login"].as_str().is_some_and(|login| login.ends_with("[bot]"));
        if finished {
            piles.finished.push(comment.clone());
        } else if bot {
            piles.bots.push(comment.clone());
        } else {
            piles.active.push(comment.clone());
        }
    }
    piles
}
/// The thread a comment belongs to, when it lives on one.
pub fn thread_of<'a>(comment: &Value, threads: &'a [Value]) -> Option<&'a Value> {
    threads.iter().find(|thread| thread["comments"].as_array().into_iter().flatten().any(|entry| entry["id"] == comment["id"]))
}
/// `orderPullRequestComments`: the host sends them oldest first; "newest" reads them backwards.
pub fn ordered(comments: &[Value], newest_first: bool) -> Vec<Value> {
    let mut list = comments.to_vec();
    if newest_first {
        list.reverse();
    }
    list
}
/// The window of comments the Summary tab draws, by recency whatever the order it reads in, and how many it leaves out.
pub fn recent(comments: &[Value], shown: usize) -> (&[Value], usize) {
    let start = comments.len().saturating_sub(shown);
    (&comments[start..], start)
}

// ---- checks ----------------------------------------------------------------------------------------------------------------

fn is_workflow_approval(check: &Value) -> bool {
    check["status"] == "action-required" && check["url"].as_str().is_some_and(|url| {
        // `/actions/runs/<digits>` followed by the end or a slash.
        url.find("/actions/runs/").is_some_and(|at| {
            let rest = &url[at + "/actions/runs/".len()..];
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            digits > 0 && (rest.len() == digits || rest[digits..].starts_with('/'))
        })
    })
}
/// `pullRequestCheckStatusLabel`: the wire folds queued, in progress and pending into one status, and the panel calls it
/// "Running".
pub fn check_status_label(check: &Value) -> &'static str {
    if is_workflow_approval(check) {
        return "Awaiting approval";
    }
    match check["status"].as_str().unwrap_or_default() {
        "pending" => "Running",
        "action-required" => "Awaiting action",
        "success" => "Passed",
        "failure" => "Failed",
        "cancelled" => "Cancelled",
        "skipped" => "Skipped",
        "neutral" => "Neutral",
        _ => "Running",
    }
}
/// `summarizePullRequestChecks`: the line beside the tabs.
pub fn summarize_checks(checks: &[Value]) -> String {
    if checks.is_empty() {
        return "No checks reported".into();
    }
    let total = checks.len();
    let count = |statuses: &[&str]| checks.iter().filter(|check| statuses.iter().any(|status| check["status"] == *status)).count();
    let workflows = checks.iter().filter(|check| is_workflow_approval(check)).count();
    let other_action = count(&["action-required"]) - workflows;
    let (failed, pending, passed) = (count(&["failure", "cancelled"]), count(&["pending"]), count(&["success"]));
    let plural = |n: usize, one: &str, many: &str| if n == 1 { one.to_owned() } else { many.to_owned() };
    if failed > 0 {
        return format!("{failed} of {total} failing");
    }
    if workflows > 0 && other_action > 0 {
        return format!("{workflows} {} and {other_action} {} awaiting action", plural(workflows, "workflow", "workflows"), plural(other_action, "check", "checks"));
    }
    if workflows > 0 {
        return format!("{workflows} {} awaiting approval", plural(workflows, "workflow", "workflows"));
    }
    if other_action > 0 {
        return format!("{other_action} {} awaiting action", plural(other_action, "check", "checks"));
    }
    if pending > 0 {
        return format!("{pending} of {total} running");
    }
    if passed == total { "All checks passed".into() } else { format!("{passed} of {total} passing") }
}
/// The checks a popover shows first (those that need someone or are running), and whether the finished ones fold behind
/// "Show all" (`ChecksBody`).
pub fn checks_for_popover(checks: &[Value], show_all: bool) -> (Vec<&Value>, bool) {
    let [attention, running, completed] = crate::pull_request::group_checks(checks);
    let collapsible = !attention.is_empty() || !running.is_empty();
    let collapsible = collapsible && !completed.is_empty();
    let mut visible: Vec<&Value> = attention;
    visible.extend(running);
    if show_all || !collapsible {
        visible.extend(completed);
    }
    (visible, collapsible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_body_of_nothing_but_a_bots_bookkeeping_is_no_body() {
        assert_eq!(visible_body("<!-- bot: 1 -->\n"), None);
        assert_eq!(visible_body("  hello <!-- x --> "), Some("hello <!-- x -->".into()));
        assert_eq!(visible_body(""), None);
    }

    #[test]
    fn the_wires_one_pending_status_reads_as_running_and_a_workflow_waiting_for_approval_says_so() {
        let check = |status: &str, url: Option<&str>| json!({"name":"x","status":status,"url":url});
        for queued in ["pending"] {
            assert_eq!(check_status_label(&check(queued, None)), "Running");
        }
        assert_eq!(check_status_label(&check("action-required", Some("https://github.com/a/b/actions/runs/123/job/4"))), "Awaiting approval");
        assert_eq!(check_status_label(&check("action-required", Some("https://ci/x"))), "Awaiting action");
        assert_eq!(check_status_label(&check("success", None)), "Passed");
        let checks = vec![check("success", None), check("pending", None), check("pending", None)];
        assert_eq!(summarize_checks(&checks), "2 of 3 running");
        assert_eq!(summarize_checks(&[check("failure", None), check("success", None)]), "1 of 2 failing");
        assert_eq!(summarize_checks(&[check("success", None)]), "All checks passed");
    }

    #[test]
    fn the_popover_folds_finished_checks_only_while_something_else_needs_looking_at() {
        let checks = vec![json!({"name":"a","status":"success"}), json!({"name":"b","status":"failure"}), json!({"name":"c","status":"pending"})];
        let (shown, collapsible) = checks_for_popover(&checks, false);
        assert_eq!((shown.iter().map(|check| check["name"].as_str().unwrap()).collect::<Vec<_>>(), collapsible), (vec!["b", "c"], true));
        assert_eq!(checks_for_popover(&checks, true).0.len(), 3);
        let all_done = vec![json!({"name":"a","status":"success"})];
        assert_eq!(checks_for_popover(&all_done, false), (vec![&all_done[0]], false));
    }

    #[test]
    fn a_remark_is_one_comment_whether_it_sits_on_a_thread_or_not_and_a_verdict_is_a_row_of_its_own() {
        let detail = json!({"createdAt":"2026-01-01T00:00:00Z","author":{"login":"alice"},"mergedAt":"2026-01-09T00:00:00Z"});
        let commits = vec![json!({"oid":"abcdef123456","committedDate":"2026-01-03T00:00:00Z","messageHeadline":"one","authors":[{"login":"alice"}]})];
        let comments = vec![
            json!({"id":"c1","kind":"issue-comment","createdAt":"2026-01-02T00:00:00Z","author":{"login":"bob"},"body":"first","reviewState":null}),
            json!({"id":"c2","kind":"review-comment","createdAt":"2026-01-04T00:00:00Z","author":{"login":"bob"},"body":"nit","path":"a.rs","reviewState":null}),
            json!({"id":"c3","kind":"review","createdAt":"2026-01-05T00:00:00Z","author":{"login":"carol"},"body":"","reviewState":"APPROVED"}),
            json!({"id":"c4","kind":"issue-comment","createdAt":"2026-01-06T00:00:00Z","author":{"login":"dan"},"body":"later","reviewState":null}),
        ];
        let events = build(&detail, &commits, &comments);
        assert_eq!(events.iter().map(|event| event.id.as_str()).collect::<Vec<_>>(), ["merged", "c4", "c3", "c2", "abcdef123456", "c1", "created"]);
        let rows = group(events);
        let shape: Vec<String> = rows.iter().map(|row| match row { Row::Event(event) => event.id.clone(), Row::Comments(events) => format!("{} remarks", events.len()) }).collect();
        // The approval splits the conversation; the commit does too.
        assert_eq!(shape, ["merged", "1 remarks", "c3", "1 remarks", "abcdef123456", "1 remarks", "created"]);
        assert!(verdict_stale("2026-01-02T00:00:00Z", newest_commit_at(&commits).as_deref()));
        assert!(!verdict_stale("2026-01-05T00:00:00Z", newest_commit_at(&commits).as_deref()));
    }

    #[test]
    fn comments_a_resolved_thread_or_a_dismissed_review_finished_leave_the_live_list() {
        let comments = vec![
            json!({"id":"a","author":{"login":"bob"},"reviewState":null}),
            json!({"id":"b","author":{"login":"bob"},"reviewState":null}),
            json!({"id":"c","author":{"login":"ci[bot]"},"reviewState":null}),
            json!({"id":"d","author":{"login":"eve"},"reviewState":"DISMISSED"}),
        ];
        let threads = vec![json!({"id":"t","isResolved":true,"comments":[{"id":"b"}]})];
        let piles = partition_comments(&comments, &threads);
        let ids = |list: &[Value]| list.iter().map(|comment| comment["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!((ids(&piles.active), ids(&piles.finished), ids(&piles.bots)), (vec!["a".to_owned()], vec!["b".to_owned(), "d".to_owned()], vec!["c".to_owned()]));
        // The window is by recency whichever way the list reads.
        let (window, hidden) = recent(&comments, 3);
        assert_eq!((window.len(), hidden), (3, 1));
        assert_eq!(ordered(&comments, true)[0]["id"], "d");
    }

    #[test]
    fn a_reviewers_ring_is_their_last_verdict_and_dims_once_commits_overtake_it() {
        let comments = [
            json!({"id":"1","author":{"login":"Ann"},"reviewState":"CHANGES_REQUESTED","createdAt":"2026-01-01T10:00:00Z"}),
            json!({"id":"2","author":{"login":"ann"},"reviewState":"approved","createdAt":"2026-01-01T12:00:00Z"}),
            json!({"id":"3","author":{"login":"bob"},"reviewState":"COMMENTED","createdAt":"2026-01-01T12:00:00Z"}),
            json!({"id":"4","author":{"login":"cy"},"reviewState":"dismissed","createdAt":"2026-01-01T09:00:00Z"}),
        ];
        let commits = [json!({"committedDate":"2026-01-01T11:00:00Z"}), json!({"committedDate":"2026-01-01T10:30:00Z"})];
        let outcomes = latest_outcomes(&comments, &commits);
        // `Ann` and `ann` are one reviewer, and the later verdict is theirs; it came after the newest commit.
        assert_eq!(outcomes.get("ann"), Some(&(Outcome::Approved, false)));
        // A comment is not a verdict; a dismissal given before the newest commit is stale.
        assert_eq!(outcomes.get("bob"), None);
        assert_eq!(outcomes.get("cy"), Some(&(Outcome::Dismissed, true)));
    }
}
