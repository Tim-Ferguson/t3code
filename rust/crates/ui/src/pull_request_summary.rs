//! The pull request panel's Summary and Timeline tabs and what sits above them. Port of `PullRequestSummaryTab.tsx`,
//! `PullRequestTimelineTab.tsx` and the merge row and checks line of `PullRequestDetailPanel.tsx`.
//!
//! Summary holds who is asked to review and the labels, the description, the checks as the host listed them, and the comments
//! (newest first unless asked otherwise, bots' and finished ones folded away). Timeline holds what happened in order: the
//! opening, commits, conversations, verdicts, and how it ended. Inline review threads belong to the Code tab, at their lines.
use crate::{
    markdown::Markdown,
    pull_request_panel::Read,
    runtime::{TransportHandle, UiModel},
};
use dioxus::prelude::*;
use serde_json::Value;
use std::rc::Rc;
use t3_client::{
    pull_request::{self as pr, Reference},
    pull_request_timeline as logic,
};

fn when(at: &str) -> String {
    crate::pull_request_review::when(at)
}

/// What a screen reader is told while the stack lookup is out.
pub(crate) const CHECKING_STACK: &str = "Checking whether this pull request is in a stack";

// ---- the merge row ---------------------------------------------------------------------------------------------------------

/// What the host and the reader allow doing with the pull request, above the tabs so it stays in view on each. While the host's
/// stack lookup has not said whether the pull request may be merged on its own (`settled`), a line for a screen reader says so.
#[component]
pub(crate) fn Actions(value: Rc<Value>, show_writes: bool, why: Option<String>, writable: bool, busy: Option<String>, preferred: String, on_method: Callback<String>, on_action: Callback<(String, Option<String>)>, #[props(default = true)] single_merge: bool, #[props(default = true)] settled: bool) -> Element {
    let allowed = pr::allowed_merge_methods(&value);
    let method = pr::resolve_merge_method(&allowed, &preferred);
    let control = pr::primary_for_gated(&value, single_merge);
    let freshness = pr::base_freshness(&value);
    let pr_state = value["state"].as_str().unwrap_or("open").to_owned();
    let auto = value["autoMergeEnabled"] == true;
    let can = |action: &str| writable && pr::can_perform(&value, action);
    let idle = busy.is_none();
    let title = why.clone().unwrap_or_default();
    rsx! {
        div { class: "pr-merge", role: "group", "aria-label": "Merge",
            match control {
                Some("merged") => rsx! { p { class: "pr-merge-state", role: "status", "This pull request was merged." } },
                Some("closed") => rsx! { p { class: "pr-merge-state", role: "status", "This pull request is closed." } },
                Some("resolve") => rsx! { p { class: "pr-merge-state", role: "status", "This branch has conflicts that must be resolved." } },
                Some("auto-merge-armed") => rsx! { p { class: "pr-merge-state", role: "status", "Auto-merge is on. It merges as soon as the host considers this ready." } },
                Some("ready") => rsx! { p { class: "pr-merge-state", role: "status", "This pull request is a draft." } },
                _ => rsx! {},
            }
            if pr_state == "open" && show_writes {
                if matches!(control, Some("merge") | Some("enable-auto-merge")) && allowed.len() > 1 {
                    label { r#for: "pr-merge-method", class: "sr-only", "Merge method" }
                    select { id: "pr-merge-method", "aria-label": "Merge method", value: "{method}", onchange: move |event| on_method.call(event.value()),
                        for candidate in allowed.iter() { option { key: "{candidate}", value: "{candidate}", selected: *candidate == method, "{pr::merge_method_label(candidate)}" } }
                    }
                }
                if control == Some("merge") {
                    button { class: "primary", "aria-label": "{pr::merge_method_label(&method)}", title: "{title}", disabled: !can("merge") || !idle,
                        onclick: { let method = method.clone(); move |_| on_action.call(("merge".into(), Some(method.clone()))) }, "{pr::merge_method_label(&method)}" }
                }
                if control == Some("enable-auto-merge") {
                    button { "aria-label": "Enable auto-merge", title: "{title}", disabled: !can("enable-auto-merge") || !idle,
                        onclick: { let method = method.clone(); move |_| on_action.call(("enable-auto-merge".into(), Some(method.clone()))) }, "Enable auto-merge" }
                }
                if control == Some("ready") {
                    button { class: "primary", "aria-label": "Ready for review", title: "{title}", disabled: !can("ready") || !idle, onclick: move |_| on_action.call(("ready".into(), None)), "Ready for review" }
                }
                if auto && single_merge && pr::can_perform(&value, "disable-auto-merge") {
                    button { "aria-label": "Disable auto-merge", title: "{title}", disabled: !can("disable-auto-merge") || !idle, onclick: move |_| on_action.call(("disable-auto-merge".into(), None)), "Disable auto-merge" }
                }
                if value["isDraft"] != true && pr::can_perform(&value, "draft") {
                    button { "aria-label": "Convert to draft", title: "{title}", disabled: !can("draft") || !idle, onclick: move |_| on_action.call(("draft".into(), None)), "Convert to draft" }
                }
                if let Some((behind, methods)) = freshness.clone() {
                    {
                        let how = behind.map_or_else(|| "behind".to_owned(), |behind| format!("{behind} commits behind"));
                        rsx! { p { class: "pr-merge-state", role: "status", "This branch is {how} its base branch." } }
                    }
                    if let Some(update) = methods.first().cloned() {
                        button { "aria-label": "Update branch", title: "{title}", disabled: !writable || !idle, onclick: move |_| on_action.call(("update-branch".into(), Some(update.clone()))), "Update branch" }
                    }
                }
                if pr::can_perform(&value, "close") {
                    button { class: "destructive", "aria-label": "Close pull request", title: "{title}", disabled: !can("close") || !idle, onclick: move |_| on_action.call(("close".into(), None)), "Close pull request" }
                }
            }
            if pr_state == "closed" && show_writes && pr::can_perform(&value, "reopen") {
                button { "aria-label": "Reopen pull request", title: "{title}", disabled: !can("reopen") || !idle, onclick: move |_| on_action.call(("reopen".into(), None)), "Reopen pull request" }
            }
            if pr_state == "merged" && show_writes && pr::can_perform(&value, "revert") {
                button { "aria-label": "Revert", title: "{title}", disabled: !can("revert") || !idle, onclick: move |_| on_action.call(("revert".into(), None)), "Revert" }
            }
            if let Some(action) = busy.clone() { span { class: "branch-busy", role: "status", "Working on {action}\u{2026}" } }
            if !settled { span { class: "sr-only", role: "status", "aria-label": "{CHECKING_STACK}", "{CHECKING_STACK}" } }
        }
    }
}

// ---- beside the tabs -------------------------------------------------------------------------------------------------------

/// The checks as one line beside the tabs, and the list behind it; or, where workflows wait for someone to let them run,
/// the button that does (`showsApproveWorkflows`).
#[component]
pub(crate) fn ChecksLine(state: Store<UiModel>, value: Rc<Value>, writable: bool, busy: Option<String>, why: Option<String>, stale: bool, on_action: Callback<(String, Option<String>)>) -> Element {
    let mut open = use_signal(|| false);
    let mut all = use_signal(|| false);
    let Some(checks) = value["checks"].as_array().cloned().filter(|_| crate::pull_request_links::checks_supported(state)) else { return rsx! {} };
    let approvals = value["workflowApprovalsRequired"].as_u64().unwrap_or(0);
    if approvals > 0 && !stale && pr::can_perform(&value, "approve-workflows") {
        let working = busy.as_deref() == Some("approve-workflows");
        let label = if working { "Approving..." } else { "Approve workflows to run" };
        return rsx! { span { class: "pr-tabs-aside",
            button { "aria-label": "{label}", title: why.clone().unwrap_or_default(), disabled: !writable || busy.is_some(), onclick: move |_| on_action.call(("approve-workflows".into(), None)), "{label}" }
        } };
    }
    let summary = if stale { pr::checks_state_label(Some(pr::classify_checks(&checks))).map(str::to_owned).unwrap_or_else(|| "No checks reported".into()) } else { logic::summarize_checks(&checks) };
    let (visible, collapsible) = logic::checks_for_popover(&checks, all());
    rsx! {
        span { class: "pr-tabs-aside",
            button { class: "pr-checks-toggle", r#type: "button", "aria-label": "Checks: {summary}", "aria-expanded": "{open()}", onclick: move |_| { let now = *open.peek(); open.set(!now) },
                span { class: "pr-checks-dot", "data-state": "{pr::classify_checks(&checks)}", "aria-hidden": "true" }
                "{summary}"
            }
            if open() {
                div { class: "pr-checks-popover", role: "dialog", "aria-label": "Checks",
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
                    if stale { p { class: "muted", "Check details are out of date. Refresh the pull request to update them." } }
                    else if checks.is_empty() { p { class: "muted", "No checks reported" } }
                    else {
                        ul {
                            for (index, check) in visible.iter().enumerate() { CheckRow { key: "{index}:{check[\"name\"].as_str().unwrap_or_default()}", check: (*check).clone() } }
                        }
                        if collapsible { button { r#type: "button", "aria-expanded": "{all()}", onclick: move |_| { let now = *all.peek(); all.set(!now) }, if all() { "Show less" } else { "Show all" } } }
                    }
                }
            }
        }
    }
}

/// One check: its state's dot, name, the word for its state, and a way to its page.
#[component]
fn CheckRow(check: Value) -> Element {
    let name = check["name"].as_str().unwrap_or_default().to_owned();
    let status = check["status"].as_str().unwrap_or_default().to_owned();
    let url = check["url"].as_str().filter(|url| url.starts_with("https://") || url.starts_with("http://")).map(str::to_owned);
    let description = check["description"].as_str().filter(|text| !text.is_empty()).unwrap_or(name.as_str()).to_owned();
    rsx! {
        li { class: "pr-check", "data-status": "{status}",
            span { class: "pr-check-dot", "aria-hidden": "true" }
            span { class: "pr-check-name", title: "{description}", "{name}" }
            span { class: "pr-check-status muted", "{logic::check_status_label(&check)}" }
            if let Some(url) = url { a { href: "{url}", target: "_blank", rel: "noopener noreferrer", "Details" } }
        }
    }
}

/// The comment count, activity counts and the order the timeline reads in.
#[component]
pub(crate) fn TimelineAside(activity: Read<Rc<Value>>, newest_first: bool, on_order: Callback<()>) -> Element {
    let (comments, commits) = match &activity {
        Read::Ready(value) => (Some(value["comments"].as_array().map_or(0, Vec::len)), Some(value["commits"].as_array().map_or(0, Vec::len))),
        _ => (None, None),
    };
    let count = |value: Option<usize>, one: &str, many: &str| match (&activity, value) {
        (Read::Failed(_), _) => (format!("{many} unavailable"), "\u{2014}".to_owned()),
        (_, None) => (many.to_owned(), "\u{2026}".to_owned()),
        (_, Some(n)) => (format!("{n} {}", if n == 1 { one } else { many }), n.to_string()),
    };
    // The host's own count, which can exceed what was read.
    let comment_count = match &activity {
        Read::Ready(value) => Some(value["commentCount"].as_u64().map_or(comments.unwrap_or(0), |n| n as usize)),
        _ => None,
    };
    let (comment_label, comment_text) = count(comment_count, "comment", "comments");
    let (commit_label, commit_text) = count(commits, "commit", "commits");
    let approvals = match &activity {
        Read::Ready(value) if value["commentsTruncated"] != true => {
            let outcomes = logic::latest_outcomes(&value["comments"].as_array().cloned().unwrap_or_default(), &value["commits"].as_array().cloned().unwrap_or_default());
            outcomes.values().filter(|(outcome, stale)| *outcome == logic::Outcome::Approved && !stale).count()
        }
        _ => 0,
    };
    rsx! {
        span { class: "pr-tabs-aside pr-timeline-aside",
            span { class: "muted", "aria-label": "{comment_label}", "\u{1f4ac} {comment_text}" }
            span { class: "muted", "aria-label": "{commit_label}", "\u{25cf} {commit_text}" }
            if approvals > 0 { span { class: "pr-outcome approved", "\u{2713} {approvals}" span { class: "sr-only", if approvals == 1 { " approval" } else { " approvals" } } } }
            button { r#type: "button", "aria-label": if newest_first { "Show oldest activity first" } else { "Show newest activity first" }, onclick: move |_| on_order.call(()),
                if newest_first { "Newest first" } else { "Oldest first" } }
        }
    }
}

fn outcome_name(outcome: logic::Outcome) -> &'static str {
    match outcome {
        logic::Outcome::Approved => "approved",
        logic::Outcome::ChangesRequested => "changes-requested",
        logic::Outcome::Dismissed => "dismissed",
    }
}
fn outcome_label(outcome: logic::Outcome) -> &'static str {
    match outcome {
        logic::Outcome::Approved => "Approved",
        logic::Outcome::ChangesRequested => "Changes requested",
        logic::Outcome::Dismissed => "Review dismissed",
    }
}

// ---- Summary ---------------------------------------------------------------------------------------------------------------

/// Who is asked to review and what labels it wears, the description, the checks as listed, and the comments.
#[component]
pub(crate) fn Summary(state: Store<UiModel>, transport: TransportHandle, reference: Reference, value: Rc<Value>, activity: Read<Rc<Value>>, writable: bool, on_changed: Callback<()>, on_retry: Callback<()>) -> Element {
    let author = value["author"]["login"].as_str().unwrap_or_default().to_owned();
    let (additions, deletions, files) = (value["additions"].as_u64().unwrap_or(0), value["deletions"].as_u64().unwrap_or(0), value["changedFiles"].as_u64().unwrap_or(0));
    let body = value["body"].as_str().unwrap_or_default().trim().to_owned();
    let checks: Vec<Value> = value["checks"].as_array().cloned().unwrap_or_default();
    // A reviewer's ring is their last verdict, when the whole conversation was read.
    let verdicts = match &activity {
        Read::Ready(activity) if activity["commentsTruncated"] != true => logic::latest_outcomes(&activity["comments"].as_array().cloned().unwrap_or_default(), &activity["commits"].as_array().cloned().unwrap_or_default()),
        _ => Default::default(),
    };
    rsx! {
        section { class: "pr-overview", "aria-label": "Summary",
            p { class: "pr-meta muted",
                if !author.is_empty() { "{author}" }
                // A size is drawn only when the host counted one; zero is how a host that does not says nothing.
                if pr::has_size(additions, deletions) { if !author.is_empty() { " \u{b7} " } "+{additions} \u{2212}{deletions}" if files > 0 { " in {files} files" } }
                else if files > 0 { if !author.is_empty() { " \u{b7} " } "{files} files" }
            }
            section { class: "pr-people", "aria-label": "Reviewers and labels",
                // The row is always there; the menu only where the host can say who could be asked (Azure DevOps takes
                // a reviewer but lists nobody, so there is nothing to open).
                p { class: "pr-meta", "Reviewers: " if value["reviewers"].as_array().is_none_or(Vec::is_empty) { span { class: "muted", "None" } } else { for reviewer in value["reviewers"].as_array().cloned().unwrap_or_default() {
                    { let login = reviewer["login"].as_str().unwrap_or_default().to_owned(); let verdict = verdicts.get(&login.to_lowercase()).copied(); let said = verdict.map(|(outcome, stale)| if stale { logic::stale_label(outcome) } else { outcome_label(outcome) }); rsx! { span { key: "{login}", class: "pr-reviewer", "data-outcome": verdict.map(|(outcome, _)| outcome_name(outcome)), "data-stale": verdict.map(|(_, stale)| stale.to_string()), title: said, "{login}" if let Some(said) = said { span { class: "sr-only", " ({said})" } } } } } } } }
                if value["capabilities"]["reviewers"]["request"] == true && value["capabilities"]["reviewers"]["listCandidates"] == true {
                    crate::pull_request_menus::Picker { state, transport: transport.clone(), reference: reference.clone(), kind: crate::pull_request_menus::Kind::Reviewers, allowed: value["viewerPermissions"]["requestReviewers"] == true, writable, on_changed }
                }
                if value["labels"].as_array().is_some_and(|labels| !labels.is_empty()) || value["capabilities"]["labels"] == true {
                    p { class: "pr-meta", "Labels: " if value["labels"].as_array().is_none_or(Vec::is_empty) { span { class: "muted", "None" } } else { for label in value["labels"].as_array().cloned().unwrap_or_default() { span { key: "{label[\"name\"].as_str().unwrap_or_default()}", class: "pr-label", "{label[\"name\"].as_str().unwrap_or_default()}" } } } }
                    if value["capabilities"]["labels"] == true { crate::pull_request_menus::Picker { state, transport: transport.clone(), reference: reference.clone(), kind: crate::pull_request_menus::Kind::Labels, allowed: value["viewerPermissions"]["labels"] != false, writable, on_changed } }
                }
            }
            section { class: "pr-description", "aria-label": "Description",
                h3 { "Description" }
                if !matches!(crate::pull_request_edit::editing().and_then(|editing| editing.edit.read().clone()), Some(crate::pull_request_edit::Edit::Body(_))) {
                    if body.is_empty() { p { class: "muted", "No description provided." } } else { Markdown { text: body.clone() } }
                }
                crate::pull_request_edit::DescriptionEdit { body: value["body"].as_str().unwrap_or_default().to_owned(), can_edit: t3_client::pull_request_edit::can_edit_change_request(&value), writable }
                crate::pull_request_edit::Reactions { state, transport: transport.clone(), reference: reference.clone(), reactions: value["reactions"].as_array().cloned().unwrap_or_default(), subject: None, can_react: value["capabilities"]["reactions"] == true, writable, on_changed }
            }
            // A host that reports no checks at all (the answer is null) shows no section: nothing is not "no checks".
            if value["checks"].is_array() && crate::pull_request_links::checks_supported(state) {
                details { class: "pr-checks", "aria-label": "Checks",
                    summary { "Checks" }
                    if checks.is_empty() { p { class: "muted", "No checks reported." } }
                    ul {
                        for (index, check) in checks.iter().enumerate() { CheckRow { key: "{index}:{check[\"name\"].as_str().unwrap_or_default()}:{check[\"url\"].as_str().unwrap_or_default()}", check: check.clone() } }
                    }
                }
            }
            Comments { state, transport, reference, detail: value, activity, writable, on_changed, on_retry }
        }
    }
}

/// The comments of the conversation, newest first unless asked otherwise.
#[component]
fn Comments(state: Store<UiModel>, transport: TransportHandle, reference: Reference, detail: Rc<Value>, activity: Read<Rc<Value>>, writable: bool, on_changed: Callback<()>, on_retry: Callback<()>) -> Element {
    let mut newest_first = use_signal(|| true);
    let mut shown = use_signal(|| logic::COMMENT_PAGE);
    let mut shown_bots = use_signal(|| logic::COMMENT_PAGE);
    let heading = match &activity {
        Read::Ready(value) => format!("Comments ({})", value["commentCount"].as_u64().unwrap_or_else(|| value["comments"].as_array().map_or(0, |comments| comments.len() as u64))),
        _ => "Comments".to_owned(),
    };
    rsx! {
        section { class: "pr-conversation", "aria-label": "Comments",
            header { class: "pr-section-head",
                h3 { "{heading}" }
                button { r#type: "button", "aria-label": if newest_first() { "Show oldest comments first" } else { "Show newest comments first" }, onclick: move |_| { let now = *newest_first.peek(); newest_first.set(!now) },
                    if newest_first() { "Newest first" } else { "Oldest first" } }
            }
            match activity {
                Read::Loading => rsx! { p { class: "diff-message", role: "status", "Loading conversation\u{2026}" } },
                Read::Failed(message) => rsx! { ActivityUnavailable { message, on_retry } },
                Read::Ready(activity) => {
                    let comments = activity["comments"].as_array().cloned().unwrap_or_default();
                    let threads = activity["reviewThreads"].as_array().cloned().unwrap_or_default();
                    let piles = logic::partition_comments(&comments, &threads);
                    let (window, hidden) = logic::recent(&piles.active, shown());
                    let visible = logic::ordered(window, newest_first());
                    let (bot_window, hidden_bots) = logic::recent(&piles.bots, shown_bots());
                    let bot_visible = logic::ordered(bot_window, newest_first());
                    let plural = |n: usize| if n == 1 { "" } else { "s" };
                    let bots_label = format!("{} bot comment{}", piles.bots.len(), plural(piles.bots.len()));
                    let finished_label = format!("{} resolved or dismissed comment{}", piles.finished.len(), plural(piles.finished.len()));
                    let older_bots_label = format!("Show {} older bot comment{} ({hidden_bots} hidden)", hidden_bots.min(logic::COMMENT_PAGE), plural(hidden_bots.min(logic::COMMENT_PAGE)));
                    let older = (hidden > 0).then(|| {
                        let step = hidden.min(logic::COMMENT_PAGE);
                        let older_label = format!("Show {step} older comment{} ({hidden} hidden)", if step == 1 { "" } else { "s" });
                        rsx! { button { r#type: "button", "aria-label": "Show older comments", onclick: move |_| shown.set(shown() + logic::COMMENT_PAGE), "{older_label}" } }
                    });
                    rsx! {
                        if activity["commentsTruncated"] == true {
                            p { class: "muted pr-truncated", "This conversation is longer than this page reads in one go. The most recent {comments.len()} are here; open it on the host to read the rest." }
                        }
                        if comments.is_empty() { p { class: "muted", "No comments yet." } }
                        else {
                            div { class: "pr-comment-list",
                                if !newest_first() { {older.clone()} }
                                ol { class: "pr-comments",
                                    for item in visible.iter() { CommentCard { key: "{item[\"id\"].as_str().unwrap_or_default()}", state, transport: transport.clone(), reference: reference.clone(), detail: detail.clone(), comment: item.clone(), threads: threads.clone(), writable, on_changed } }
                                }
                                if newest_first() { {older} }
                                if shown() > logic::COMMENT_PAGE { button { r#type: "button", onclick: move |_| shown.set(logic::COMMENT_PAGE), "Show only {logic::COMMENT_PAGE} recent comments" } }
                                if !piles.bots.is_empty() {
                                    details { class: "pr-comment-group",
                                        summary { "{bots_label}" }
                                        ol { class: "pr-comments",
                                            for item in bot_visible.iter() { CommentCard { key: "{item[\"id\"].as_str().unwrap_or_default()}", state, transport: transport.clone(), reference: reference.clone(), detail: detail.clone(), comment: item.clone(), threads: threads.clone(), writable, on_changed } }
                                        }
                                        if hidden_bots > 0 { button { r#type: "button", onclick: move |_| shown_bots.set(shown_bots() + logic::COMMENT_PAGE), "{older_bots_label}" } }
                                        if shown_bots() > logic::COMMENT_PAGE { button { r#type: "button", onclick: move |_| shown_bots.set(logic::COMMENT_PAGE), "Show only {logic::COMMENT_PAGE} recent bot comments" } }
                                    }
                                }
                                if !piles.finished.is_empty() {
                                    details { class: "pr-comment-group",
                                        summary { "{finished_label}" }
                                        ol { class: "pr-comments",
                                            for item in logic::ordered(&piles.finished, newest_first()).iter() {
                                                {
                                                    let thread = logic::thread_of(item, &threads);
                                                    let label = if thread.is_some_and(|thread| thread["isResolved"] == true) { "Resolved" } else { "Review dismissed" };
                                                    rsx! { li { key: "{item[\"id\"].as_str().unwrap_or_default()}", class: "pr-comment finished",
                                                        div { class: "pr-comment-head", strong { "{item[\"author\"][\"login\"].as_str().unwrap_or(\"ghost\")}" } span { class: "muted", "{label}" } span { class: "muted", title: "{item[\"createdAt\"].as_str().unwrap_or_default()}", "{when(item[\"createdAt\"].as_str().unwrap_or_default())}" } }
                                                        if let Some(body) = logic::visible_body(item["body"].as_str().unwrap_or_default()) { details { summary { "Show" } Markdown { text: body } } }
                                                    } }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// "Could not load pull request activity", with the way to ask again.
#[component]
pub(crate) fn ActivityUnavailable(message: String, on_retry: Callback<()>) -> Element {
    rsx! { div { class: "diff-failed", role: "alert",
        p { class: "pr-unavailable-title", "Could not load pull request activity" }
        p { class: "diff-error", "{message}" }
        button { r#type: "button", "aria-label": "Retry loading pull request activity", onclick: move |_| on_retry.call(()), "Retry" }
    } }
}

/// One remark: who, what they did, when; the verdict it carries; where it was left; its words, reactions and the pencil.
#[component]
fn CommentCard(state: Store<UiModel>, transport: TransportHandle, reference: Reference, detail: Rc<Value>, comment: Value, threads: Vec<Value>, writable: bool, on_changed: Callback<()>) -> Element {
    let id = comment["id"].as_str().unwrap_or_default().to_owned();
    let author = comment["author"]["login"].as_str().unwrap_or("ghost").to_owned();
    let at = comment["createdAt"].as_str().unwrap_or_default().to_owned();
    let kind = comment["kind"].as_str().unwrap_or_default().to_owned();
    let outcome = logic::review_outcome(comment["reviewState"].as_str());
    let verb = pr::comment_verb(&kind, comment["reviewState"].as_str(), None);
    let body = comment["body"].as_str().unwrap_or_default().to_owned();
    let visible = logic::visible_body(&body);
    let can_edit = t3_client::pull_request_edit::can_edit_comment(&detail, &comment);
    let reactions = comment["reactions"].as_array().cloned().unwrap_or_default();
    // Where an inline remark was left: its thread's line, which the bare comment has lost.
    let place = comment["path"].as_str().filter(|path| !path.is_empty()).map(|path| match logic::thread_of(&comment, &threads).and_then(|thread| thread["line"].as_u64()) {
        Some(line) => format!("{path}:{line}"),
        None => path.to_owned(),
    });
    rsx! {
        li { class: "pr-comment",
            div { class: "pr-comment-head", strong { "{author}" } span { class: "muted", "{verb}" }
                if let Some(outcome) = outcome { span { class: "pr-outcome {outcome.class()}", "{outcome.label()}" } }
                span { class: "muted", title: "{at}", "{when(&at)}" }
                if let Some(url) = comment["url"].as_str().filter(|url| url.starts_with("https://") || url.starts_with("http://")) { a { class: "pr-open-host", href: "{url}", target: "_blank", rel: "noopener noreferrer", "aria-label": "Open activity on host", title: "Open activity on host", "\u{2197}" } }
            }
            if let Some(place) = place { p { class: "pr-comment-place muted", "{place}" } }
            if visible.is_some() || can_edit {
                crate::pull_request_edit::CommentEdit { id: id.clone(), kind: kind.clone(), body: body.clone(), can_edit, writable,
                    if let Some(shown) = visible.clone() { Markdown { text: shown } } }
            }
            crate::pull_request_edit::Reactions { state, transport: transport.clone(), reference: reference.clone(), reactions, subject: Some(id.clone()), can_react: detail["capabilities"]["reactions"] == true, writable, on_changed }
        }
    }
}

// ---- Timeline --------------------------------------------------------------------------------------------------------------

/// What happened, in order, with the conversations folded into rows of their own between the commits and verdicts.
#[component]
pub(crate) fn Timeline(state: Store<UiModel>, transport: TransportHandle, reference: Reference, detail: Rc<Value>, activity: Read<Rc<Value>>, newest_first: bool, writable: bool, on_changed: Callback<()>, on_retry: Callback<()>) -> Element {
    let activity = match activity {
        Read::Loading => return rsx! { section { class: "pr-timeline", "aria-label": "Timeline", p { class: "diff-message", role: "status", "Loading timeline\u{2026}" } } },
        Read::Failed(message) => return rsx! { section { class: "pr-timeline", "aria-label": "Timeline", ActivityUnavailable { message, on_retry } } },
        Read::Ready(activity) => activity,
    };
    let comments = activity["comments"].as_array().cloned().unwrap_or_default();
    let commits = activity["commits"].as_array().cloned().unwrap_or_default();
    let threads = activity["reviewThreads"].as_array().cloned().unwrap_or_default();
    let newest_commit = logic::newest_commit_at(&commits);
    let mut events = logic::build(&detail, &commits, &comments);
    if !newest_first {
        events.reverse();
    }
    let rows = logic::group(events);
    rsx! {
        section { class: "pr-timeline", "aria-label": "Timeline",
            ol { class: "pr-timeline-list",
                for row in rows.into_iter() {
                    match row {
                        logic::Row::Comments(events) => rsx! { Conversation { key: "comments:{events[0].id}", state, transport: transport.clone(), reference: reference.clone(), detail: detail.clone(), events, threads: threads.clone(), writable, on_changed } },
                        logic::Row::Event(event) => match event.kind {
                            logic::Kind::Commit => rsx! { li { key: "{event.id}", class: "pr-timeline-row commit",
                                code { "{pr::short_oid(&event.id)}" } span { class: "pr-commit-title", "{event.body.clone().unwrap_or_else(|| \"Untitled commit\".to_owned())}" }
                                if let (Some(added), Some(removed)) = (event.additions, event.deletions) { span { class: "muted", "+{added} \u{2212}{removed}" } }
                                span { class: "muted", title: "{event.at}", "{when(&event.at)}" }
                            } },
                            logic::Kind::Opened | logic::Kind::Merged | logic::Kind::Closed => {
                                let label = match event.kind { logic::Kind::Opened => "Pull request opened", logic::Kind::Merged => "Pull request merged", _ => "Pull request closed" };
                                rsx! { li { key: "{event.id}", class: "pr-timeline-row lifecycle",
                                    if let Some(actor) = event.actor.clone() { strong { "{actor}" } }
                                    span { "{label}" }
                                    span { class: "muted", title: "{event.at}", "{when(&event.at)}" }
                                } }
                            }
                            _ => {
                                let outcome = logic::review_outcome(event.review_state.as_deref());
                                let stale = logic::verdict_stale(&event.at, newest_commit.as_deref());
                                rsx! { Verdict { key: "{event.id}", state, transport: transport.clone(), reference: reference.clone(), detail: detail.clone(), event, outcome, stale, writable, on_changed } }
                            }
                        },
                    }
                }
            }
        }
    }
}

/// A verdict as a row of its own: it wears the verdict's word, dimmed when commits have overtaken it.
#[component]
fn Verdict(state: Store<UiModel>, transport: TransportHandle, reference: Reference, detail: Rc<Value>, event: logic::Event, outcome: Option<logic::Outcome>, stale: bool, writable: bool, on_changed: Callback<()>) -> Element {
    let author = event.actor.clone().unwrap_or_else(|| "ghost".into());
    let word = outcome.map(|outcome| outcome.label().to_lowercase()).unwrap_or_default();
    let tooltip = outcome.map(|outcome| if stale { logic::stale_label(outcome) } else { outcome.label() }).unwrap_or_default();
    let class = if stale { "pr-outcome stale".to_owned() } else { format!("pr-outcome {}", outcome.map_or("", |outcome| outcome.class())) };
    rsx! { li { class: "pr-timeline-row pr-verdict-row",
        div { class: "pr-comment-head", strong { "{author}" } span { class: "{class}", title: "{tooltip}", "{word}" if stale { span { class: "sr-only", ", before the latest commits" } } }
            span { class: "muted", title: "{event.at}", "{when(&event.at)}" }
            if let Some(path) = event.path.clone() { span { class: "muted", "{path}" } }
            if let Some(url) = event.url.clone().filter(|url| url.starts_with("https://") || url.starts_with("http://")) { a { class: "pr-open-host", href: "{url}", target: "_blank", rel: "noopener noreferrer", "aria-label": "Open activity on host", title: "Open activity on host", "\u{2197}" } }
        }
        if let Some(comment) = event.comment.clone() {
            crate::pull_request_edit::Reactions { state, transport: transport.clone(), reference: reference.clone(), reactions: comment["reactions"].as_array().cloned().unwrap_or_default(), subject: Some(event.id.clone()), can_react: detail["capabilities"]["reactions"] == true, writable, on_changed }
        }
        // An approval usually carries no words. When it does they are the review, so they stay in view.
        if let Some(body) = event.body.clone() { Markdown { text: body } }
    } }
}

/// Consecutive remarks, folded into one conversation that opens on a press.
#[component]
fn Conversation(state: Store<UiModel>, transport: TransportHandle, reference: Reference, detail: Rc<Value>, events: Vec<logic::Event>, threads: Vec<Value>, writable: bool, on_changed: Callback<()>) -> Element {
    let mut open = use_signal(|| false);
    let authors = logic::authors(&events);
    let first_at = events.first().map(|event| event.at.clone()).unwrap_or_default();
    let count_label = format!("{} {}", events.len(), if events.len() == 1 { "comment" } else { "comments" });
    let authors_label = format!("{} {}", authors.len(), if authors.len() == 1 { "author" } else { "authors" });
    rsx! { li { class: "pr-timeline-row pr-conversation-row",
        button { class: "pr-conversation-toggle", r#type: "button", "aria-expanded": "{open()}", onclick: move |_| { let now = *open.peek(); open.set(!now) },
            strong { "{count_label}" }
            span { class: "muted", "{authors_label} \u{b7} {when(&first_at)}" }
        }
        if open() {
            ol { class: "pr-comments",
                for event in events.iter() {
                    if let Some(comment) = event.comment.clone() { CommentCard { key: "{event.id}", state, transport: transport.clone(), reference: reference.clone(), detail: detail.clone(), comment, threads: threads.clone(), writable, on_changed } }
                }
            }
        }
    } }
}
