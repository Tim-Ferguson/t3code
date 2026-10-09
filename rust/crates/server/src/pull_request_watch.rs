//! What a watched pull request means for the agent that watches it: the port of
//! `orchestration-v2/pullRequestWatch.ts`. `evaluate` compares what the host shows with what the
//! watch last told the agent, and `message` writes the wake. Both are pure; `PullRequestWatcher`
//! (`pull_request_watcher.rs`) reads the hosts and records the outcome.
use chrono::DateTime;
use serde_json::{Value, json};

/// Wakes in a row that bring only comments. Check, conflict, or push news resets the count, so this
/// only stops a chatty bot looping an agent that is replying to it.
pub const WAKE_LIMIT: u64 = 10;
const LISTED_ITEMS: usize = 10;
const SNIPPET_LENGTH: usize = 200;

/// One thing the agent has not been told yet.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    ChecksFailed(Vec<Value>),
    ChecksPassed { count: usize, required: bool },
    Remarks(Vec<Value>),
    Conflicting,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// What the agent has not been told yet. Empty means no wake.
    pub changes: Vec<Change>,
    /// The watch to record, whether or not anything is reported.
    pub next: Value,
    /// This report spends the last wake before the limit, so watching stops after it.
    pub exhausted: bool,
}

impl Change {
    pub fn to_json(&self) -> Value {
        match self {
            Change::ChecksFailed(failed) => json!({"kind":"checks-failed","failed":failed}),
            Change::ChecksPassed { count, required } => json!({"kind":"checks-passed","count":count,"required":required}),
            Change::Remarks(remarks) => json!({"kind":"remarks","remarks":remarks}),
            Change::Conflicting => json!({"kind":"conflicting"}),
        }
    }
    fn kind(&self) -> &'static str {
        match self {
            Change::ChecksFailed(_) => "checks-failed",
            Change::ChecksPassed { .. } => "checks-passed",
            Change::Remarks(_) => "remarks",
            Change::Conflicting => "conflicting",
        }
    }
}

// "action-required" is a finished check that needs someone, so the agent hears about it.
fn is_failed(check: &Value) -> bool {
    matches!(check["status"].as_str(), Some("failure" | "cancelled" | "action-required"))
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(|item| item.as_str().map(str::to_owned)).collect()
}

/// `Date.parse` of an ISO time, in milliseconds; NaN when it is not one.
fn parse(value: &Value) -> f64 {
    value.as_str().and_then(|text| DateTime::parse_from_rfc3339(text).ok()).map_or(f64::NAN, |at| at.timestamp_millis() as f64)
}

/// `remark.editedAt ?? remark.createdAt`: an edit counts as new activity.
fn active_at(remark: &Value) -> &Value {
    match &remark["editedAt"] {
        Value::Null => &remark["createdAt"],
        edited => edited,
    }
}

/// Compares a watched pull request with what its agent was last told. Each check is reported as soon
/// as it fails, so a check that never finishes (an advisory review bot) cannot hold the news back.
/// "Passed" is reported once the checks the base branch requires all passed, or all checks where the
/// host marks none required. Remarks count when someone other than the agent's own account wrote
/// them, so its own replies never wake it. `remarks` is None when the conversation could not be read;
/// remarks then wait for a later pass.
pub fn evaluate(watch: &Value, detail: &Value, remarks: Option<&[Value]>) -> Report {
    let mut changes = Vec::new();
    let head_sha = detail["headSha"].clone();
    let head_sha = if head_sha.is_null() { Value::Null } else { head_sha };
    let head_moved = head_sha != watch["headSha"];

    // An empty list keeps the last state: a host can answer with one when its check read fails.
    let mut failed_checks = if head_moved { vec![] } else { strings(&watch["failedChecks"]) };
    let mut passed = if head_moved { false } else { watch["passed"] == true };
    let mut passed_checks = if head_moved { vec![] } else { strings(&watch["passedChecks"]) };
    let checks: Vec<&Value> = detail["checks"].as_array().into_iter().flatten().collect();
    if !checks.is_empty() {
        let failed: Vec<&Value> = checks.iter().copied().filter(|check| is_failed(check)).collect();
        let newly_failed: Vec<Value> = failed.iter().filter(|check| !failed_checks.iter().any(|name| check["name"] == *name)).map(|check| (*check).clone()).collect();
        if !newly_failed.is_empty() {
            changes.push(Change::ChecksFailed(newly_failed));
        }
        // A check that runs again leaves the list, so a rerun that fails again is reported.
        failed_checks = failed.iter().map(|check| check["name"].as_str().unwrap_or_default().to_owned()).collect();

        let required: Vec<&Value> = checks.iter().copied().filter(|check| check["required"] == true).collect();
        let gate = if required.is_empty() { &checks } else { &required };
        let passed_now = gate.iter().all(|check| check["status"] != "pending" && !is_failed(check));
        let gate_names: Vec<String> = gate.iter().map(|check| check["name"].as_str().unwrap_or_default().to_owned()).collect();
        // A watch saved before passedChecks existed takes the current names, so it does not wake.
        let told = if passed && passed_checks.is_empty() { &gate_names } else { &passed_checks };
        // A required job created and finished between two passes is never seen pending. Without
        // required checks, any check counts, and advisory bots keep adding passed ones: no wake.
        let gate_grew = !required.is_empty() && gate_names.iter().any(|name| !told.contains(name));
        if passed_now && (!passed || gate_grew) {
            changes.push(Change::ChecksPassed { count: gate.len(), required: !required.is_empty() });
        }
        passed = passed_now;
        passed_checks = if passed_now { gate_names } else { vec![] };
    }

    // `(detail.viewer ?? detail.author?.login)?.toLowerCase()`
    let own: Option<String> = match &detail["viewer"] {
        Value::Null => detail["author"]["login"].as_str().map(str::to_lowercase),
        viewer => viewer.as_str().map(str::to_lowercase),
    };
    let through = parse(&watch["remarksThrough"]);
    let told_ids = strings(&watch["remarkIds"]);
    // GitHub times are per second, so remarks at the boundary time are told apart by ID.
    let fresh: Vec<&Value> = remarks
        .unwrap_or_default()
        .iter()
        .filter(|remark| {
            let at = parse(active_at(remark));
            let author = remark["author"]["login"].as_str().map(str::to_lowercase);
            (at > through || (at == through && !told_ids.iter().any(|id| remark["id"] == *id))) && author != own
        })
        .collect();
    if !fresh.is_empty() {
        changes.push(Change::Remarks(fresh.iter().map(|remark| (*remark).clone()).collect()));
    }
    let latest = fresh.iter().map(|remark| parse(active_at(remark))).fold(through, |best, at| if best.is_nan() || at.is_nan() { f64::NAN } else { best.max(at) });
    let at_latest: Vec<&Value> = fresh.iter().copied().filter(|remark| parse(active_at(remark)) == latest).collect();
    let unmoved = latest == through;
    let remarks_through = match at_latest.first() {
        Some(newest) if !unmoved => active_at(newest).clone(),
        _ => watch["remarksThrough"].clone(),
    };
    let remark_ids: Vec<Value> = (if unmoved { told_ids.iter().map(|id| json!(id)).collect() } else { vec![] }).into_iter().chain(at_latest.iter().map(|remark| remark["id"].clone())).collect();

    if detail["mergeability"] == "conflicting" && watch["conflicting"] != true {
        changes.push(Change::Conflicting);
    }
    // "unknown" is GitHub still computing after a push; only a clean answer clears a conflict.
    let conflicting = if detail["mergeability"] == "unknown" { watch["conflicting"] == true } else { detail["mergeability"] == "conflicting" };

    let comments_only = !changes.is_empty() && changes.iter().all(|change| change.kind() == "remarks");
    let progress = head_moved || (!changes.is_empty() && !comments_only);
    let wakes = if progress { 0 } else { watch["wakes"].as_u64().unwrap_or(0) } + u64::from(comments_only);
    Report {
        changes,
        next: json!({"startedAt":watch["startedAt"],"headSha":head_sha,"failedChecks":failed_checks,"passed":passed,"passedChecks":passed_checks,"remarksThrough":remarks_through,"remarkIds":remark_ids,"conflicting":conflicting,"wakes":wakes}),
        exhausted: comments_only && wakes >= WAKE_LIMIT,
    }
}

/// JavaScript's `\s`: what `String.prototype.trim` strips.
fn is_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn snippet(body: &str) -> String {
    static COMMENT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?s)<!--.*?-->").unwrap());
    let text = COMMENT.replace_all(body, " ");
    let mut collapsed = String::new();
    let mut in_space = false;
    for c in text.chars() {
        if is_space(c) {
            in_space = true;
        } else {
            if in_space {
                collapsed.push(' ');
            }
            in_space = false;
            collapsed.push(c);
        }
    }
    // A run of space at the start or end of the text is trimmed, not collapsed to one.
    let text = collapsed.trim_start_matches(' ').to_owned();
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= SNIPPET_LENGTH { text } else { format!("{}...", String::from_utf16_lossy(&units[..SNIPPET_LENGTH - 3])) }
}

fn listed(items: &[Value], line: impl Fn(&Value) -> String) -> Vec<String> {
    let mut lines: Vec<String> = items.iter().take(LISTED_ITEMS).map(line).collect();
    if items.len() > LISTED_ITEMS {
        lines.push(format!("  - and {} more", items.len() - LISTED_ITEMS));
    }
    lines
}

fn change_lines(change: &Change, base_branch: &str, commit: &str) -> Vec<String> {
    match change {
        Change::ChecksFailed(failed) => std::iter::once(format!("- Checks failed{commit}:"))
            .chain(listed(failed, |check| {
                let status = check["status"].as_str().unwrap_or_default();
                let url = check["url"].as_str().filter(|url| !url.is_empty()).map_or(String::new(), |url| format!(" {url}"));
                format!("  - {}{}{url}", check["name"].as_str().unwrap_or_default(), if status == "failure" { String::new() } else { format!(" ({status})") })
            }))
            .collect(),
        Change::ChecksPassed { count, required } => vec![format!("- All {count} {}{} passed{commit}.", if *required { "required " } else { "" }, if *count == 1 { "check" } else { "checks" })],
        Change::Remarks(remarks) => std::iter::once(format!("- {} new {}:", remarks.len(), if remarks.len() == 1 { "comment" } else { "comments" }))
            .chain(listed(remarks, |remark| {
                let place = remark["path"].as_str().map_or(String::new(), |path| format!(" on {path}"));
                let body = snippet(remark["body"].as_str().unwrap_or_default());
                let said = if body.is_empty() { remark["reviewState"].as_str().unwrap_or("reviewed").to_owned() } else { format!("\"{body}\"") };
                let url = remark["url"].as_str().filter(|url| !url.is_empty()).map_or(String::new(), |url| format!(" {url}"));
                format!("  - {}{place}: {said}{url}", remark["author"]["login"].as_str().unwrap_or("someone"))
            }))
            .collect(),
        Change::Conflicting => vec![format!("- The branch now conflicts with {base_branch}.")],
    }
}

fn summary_of(kind: &str) -> &'static str {
    match kind {
        "checks-failed" => "checks failed",
        "checks-passed" => "checks passed",
        "remarks" => "new comments",
        _ => "merge conflict",
    }
}

/// The wake the agent reads and the timeline notification the user sees.
pub fn message(number: u64, url: &str, base_branch: &str, head_sha: &Value, report: &Report) -> (String, Value) {
    let commit = head_sha.as_str().map_or(String::new(), |sha| format!(" on {}", sha.chars().take(7).collect::<String>()));
    let closing = if report.exhausted {
        format!("T3 Code stopped watching after {WAKE_LIMIT} comment-only updates in a row. Call watch_pull_request to watch it again.")
    } else {
        "Look into each item and act on it as your task requires. T3 Code keeps watching and wakes you on the next change, so end your turn when you are done. When you hand the work back to the user, call unwatch_pull_request first so the thread returns to their inbox.".to_owned()
    };
    let text = std::iter::once(format!("Update on pull request #{number} ({url}), which T3 Code is watching for you:"))
        .chain(report.changes.iter().flat_map(|change| change_lines(change, base_branch, &commit)))
        .chain([String::new(), closing])
        .collect::<Vec<_>>()
        .join("\n");
    let failed = report.changes.iter().any(|change| matches!(change, Change::ChecksFailed(_) | Change::Conflicting));
    let mut summary: Vec<&str> = report.changes.iter().map(|change| summary_of(change.kind())).collect();
    if report.exhausted {
        summary.push("stopped watching");
    }
    let outcome = if failed { "failed" } else if report.changes.iter().all(|change| change.kind() == "checks-passed") { "completed" } else { "updated" };
    (text, json!({"source":{"kind":"monitor"},"outcome":outcome,"summary":format!("#{number}: {}", summary.join(", "))}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original's own decisions over observation pairs `tools/generate_pull_request_watch_fixtures.mjs`
    /// derives: what is new, the watch to record, whether the limit is reached, and the wake text.
    #[test]
    fn evaluation_and_wake_match_the_original() {
        let mut count = 0;
        for line in include_str!("../tests/fixtures/pull-request-watch.jsonl").lines().filter(|line| !line.is_empty()) {
            let row: Value = serde_json::from_str(line).unwrap();
            let remarks: Option<Vec<Value>> = row["remarks"].as_array().cloned();
            let report = evaluate(&row["watch"], &row["detail"], remarks.as_deref());
            assert_eq!(report.changes.iter().map(Change::to_json).collect::<Vec<_>>(), row["report"]["changes"].as_array().cloned().unwrap(), "changes: {row}");
            assert_eq!(report.next, row["report"]["next"], "next: {row}");
            assert_eq!(report.exhausted, row["report"]["exhausted"], "exhausted: {row}");
            if let Some(expected) = row.get("message") {
                let (text, notification) = message(17, "https://github.com/o/r/pull/17", row["baseBranch"].as_str().unwrap(), &report.next["headSha"], &report);
                assert_eq!(text, expected["text"].as_str().unwrap(), "text: {row}");
                assert_eq!(notification, expected["notification"], "notification: {row}");
            }
            count += 1;
        }
        assert!(count > 1000, "{count} cases");
    }
}
