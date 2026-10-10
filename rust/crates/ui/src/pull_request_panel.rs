//! The pull request panel: the header and overview (status, merge box, reviewers and labels,
//! checks, description), the files changed (`pull_request_files`), and the conversation with its
//! comment form, review threads and review form (`pull_request_review`). Port of the tabs of
//! `PullRequestDetailPanel.tsx`. Not ported: the thread header link, starting a thread from a pull request, and the
//! pull request list.
//!
//! Reads are held only while the panel is mounted: the detail, the checks and the activity, and
//! the server's refresh stream, whose notice (never a timer) is what makes the panel read again.
//! After the connection is lost everything shown is marked as old and every write is disabled.
//! Every write is guarded against a second click until the server answers, and the ones that
//! cannot be undone ask first, in the original's words.
use crate::{
    markdown::Markdown,
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::rc::Rc;
use t3_client::{
    connection::ConnectionStatus,
    pull_request::{self as pr, Reference},
};
use t3_contracts::AuthEnvironmentScope;

/// Which pull request the panel shows, and for which thread. A target for another thread never
/// opens this thread's panel.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub thread_id: String,
    pub reference: Reference,
}

/// Opens the panel on a pull request; the diff panel gives way, since they share the margin.
pub(crate) fn open(state: Store<UiModel>, target: Target) {
    state.diff().write().open = false;
    state.pr_panel().set(Some(target));
}
pub(crate) fn close(state: Store<UiModel>) {
    state.pr_panel().set(None);
}

#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    let target = use_memo(move || state.pr_panel().read().clone().filter(|target| target.thread_id == thread_id));
    match target() {
        Some(target) => rsx! { Panel { key: "{target.reference.key()}", state, transport, thread_id: target.thread_id, reference: target.reference } },
        None => rsx! {},
    }
}

/// What a read has told the panel.
#[derive(Debug, Clone, PartialEq, Default)]
enum Read<T> {
    #[default]
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Conversation,
    Files,
}

/// Comments shown before "Show more": a long conversation draws a page at a time.
const COMMENTS_PER_PAGE: usize = 40;

#[component]
fn Panel(state: Store<UiModel>, transport: TransportHandle, thread_id: String, reference: Reference) -> Element {
    let mut detail = use_signal(Read::<Rc<Value>>::default);
    let mut activity = use_signal(Read::<Rc<Value>>::default);
    let mut tab = use_signal(|| Tab::Overview);
    let mut refresh_count = use_signal(|| 0u32);
    let mut acting = use_signal(|| None::<String>);
    let mut confirm = use_signal(|| None::<String>);
    let mut preferred = use_signal(|| "merge".to_owned());
    let mut comment = use_signal(String::new);
    let mut shown_comments = use_signal(|| COMMENTS_PER_PAGE);
    // Whether what is shown was read on a connection that is still up.
    let mut fresh = use_signal(|| false);
    let connected = use_memo(move || *state.status().read() == ConnectionStatus::Connected);
    let can_write = use_memo(move || {
        let destination = state.destination().read().clone();
        *state.status().read() == ConnectionStatus::Connected && destination.is_some_and(|destination| state.environments().read().allows(&destination, AuthEnvironmentScope::SourceControlWrite))
    });
    let can_read = use_memo(move || {
        let destination = state.destination().read().clone();
        *state.status().read() == ConnectionStatus::Connected && destination.is_some_and(|destination| state.environments().read().allows(&destination, AuthEnvironmentScope::OrchestrationRead))
    });

    // The server pushes a revision when something it holds about pull requests may have changed;
    // each notice is a reason to read again.
    {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_resource(move || {
            let (transport, reference, readable) = (transport.clone(), reference.clone(), can_read());
            async move {
                if !readable {
                    fresh.set(false);
                    return;
                }
                let _ = &reference;
                if let Ok(mut stream) = runtime::request_stream(&transport, state, pr::SUBSCRIBE_REFRESHES, json!({})) {
                    while let Some(result) = stream.next().await {
                        if result.is_err() {
                            break;
                        }
                        let next = *refresh_count.peek() + 1;
                        refresh_count.set(next);
                    }
                }
                fresh.set(false);
            }
        });
    }

    // The detail with its checks, read now and again at each notice or request. A late answer for
    // another connection is dropped; an answer for a read that was replaced is dropped too.
    {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_resource(move || {
            let (transport, reference, readable, _attempt) = (transport.clone(), reference.clone(), can_read(), refresh_count());
            async move {
                if !readable {
                    return;
                }
                let owner = runtime::response_owner(&transport, state);
                let (detail_answer, checks_answer) = futures_util::future::join(
                    runtime::request_value_reported(transport.clone(), state, pr::DETAIL, reference.payload(json!({"allowStale":false}))),
                    runtime::request_value_reported(transport.clone(), state, pr::CHECKS, reference.payload(json!({"allowStale":false}))),
                )
                .await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                match detail_answer {
                    Ok(mut value) => {
                        // The checks have their own, shorter cache: they are the part that moves.
                        if let Ok(checks) = checks_answer {
                            value["checks"] = checks["checks"].clone();
                        }
                        if detail.peek().clone() != Read::Ready(Rc::new(value.clone())) {
                            detail.set(Read::Ready(Rc::new(value)));
                        }
                        fresh.set(true);
                    }
                    Err(message) => {
                        // A failed refresh leaves what was read, marked old; a first failure is shown.
                        if !matches!(&*detail.peek(), Read::Ready(_)) {
                            detail.set(Read::Failed(message));
                        }
                        fresh.set(false);
                    }
                }
            }
        });
    }

    // The conversation is read only while its tab is open.
    {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_resource(move || {
            let (transport, reference, wanted, _attempt) = (transport.clone(), reference.clone(), matches!(tab(), Tab::Conversation | Tab::Files) && can_read(), refresh_count());
            async move {
                if !wanted {
                    return;
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, pr::ACTIVITY, reference.payload(json!({"allowStale":false}))).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                match answer {
                    Ok(value) => activity.set(Read::Ready(Rc::new(value))),
                    Err(message) => {
                        if !matches!(&*activity.peek(), Read::Ready(_)) {
                            activity.set(Read::Failed(message));
                        }
                    }
                }
            }
        });
    }

    // One write at a time; the control is enabled again by the server's answer.
    let run = {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_callback(move |(action, method): (String, Option<String>)| {
            if acting.peek().is_some() || !*can_write.peek() {
                return;
            }
            acting.set(Some(action.clone()));
            let (transport, reference) = (transport.clone(), reference.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let mut extra = json!({"action":action});
                if let Some(method) = method {
                    extra[if action == "update-branch" { "updateMethod" } else { "mergeMethod" }] = json!(method);
                }
                let answer = runtime::request_value_reported(transport.clone(), state, pr::RUN_ACTION, reference.payload(extra)).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    acting.set(None);
                    return;
                }
                match answer {
                    Ok(_) => {
                        notice::push(state, Level::Success, pr::success_label(&action), "");
                        let next = *refresh_count.peek() + 1;
                        refresh_count.set(next);
                    }
                    Err(message) => notice::push(state, Level::Error, pr::failure_label(&action), pr::readable_failure(&message, pr::failure_hint(&action))),
                }
                acting.set(None);
            });
        })
    };
    let send_comment = {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_callback(move |_: ()| {
            let body = comment.peek().trim().to_owned();
            if body.is_empty() || acting.peek().is_some() || !*can_write.peek() {
                return;
            }
            acting.set(Some("comment".into()));
            let (transport, reference) = (transport.clone(), reference.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, pr::COMMENT, reference.payload(json!({"body":body}))).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    acting.set(None);
                    return;
                }
                match answer {
                    Ok(_) => {
                        comment.set(String::new());
                        let next = *refresh_count.peek() + 1;
                        refresh_count.set(next);
                    }
                    Err(message) => notice::push(state, Level::Error, "Could not post your comment", pr::readable_failure(&message, "The host refused it. Check that you may comment on this pull request.")),
                }
                acting.set(None);
            });
        })
    };

    let write_ok = use_memo(move || can_write() && fresh());
    let _editing = crate::pull_request_edit::provide(state, transport.clone(), reference.clone(), write_ok, Callback::new(move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); }));
    let detail_now = detail.read().clone();
    let busy = acting.read().clone();
    let writable = can_write() && fresh();
    let write_reason = if !can_write() && connected() { Some("This connection cannot change source control.".to_owned()) } else if !writable { Some("Offline. This is the last pull request read.".to_owned()) } else { None };
    let stale = !fresh() && matches!(detail_now, Read::Ready(_));
    rsx! {
        aside { class: "diff-panel pr-panel", "aria-label": "Pull request panel", tabindex: "-1",
            onkeydown: move |event| { if event.key() == Key::Escape { if confirm.peek().is_some() { confirm.set(None); } else { close(state); } } },
            match detail_now {
                Read::Loading => rsx! { header { class: "diff-header", div { class: "diff-header-main", strong { "#{reference.number}" } button { class: "icon-button diff-close", "aria-label": "Close pull request panel", onclick: move |_| close(state), "\u{2715}" } } }
                    p { class: "diff-message", role: "status", if can_read() { "Loading pull request\u{2026}" } else { "Reconnect to load this pull request." } } },
                Read::Failed(message) => rsx! { header { class: "diff-header", div { class: "diff-header-main", strong { "#{reference.number}" } button { class: "icon-button diff-close", "aria-label": "Close pull request panel", onclick: move |_| close(state), "\u{2715}" } } }
                    if message == pr::NOT_AVAILABLE_FOR_HOST { p { class: "diff-message", role: "status", "{message}" } }
                    else { div { class: "diff-failed", role: "alert", p { class: "diff-error", "{message}" } button { "aria-label": "Retry loading pull request", onclick: move |_| { detail.set(Read::Loading); let next = *refresh_count.peek() + 1; refresh_count.set(next); }, "Retry" } } } },
                Read::Ready(value) => rsx! {
                    Header { state, transport: transport.clone(), thread_id: thread_id.clone(), value: value.clone(), reference: reference.clone(), writable, on_close: move |_| close(state) }
                    if stale { p { class: "diff-banner", role: "status", if connected() { "Could not refresh. This is the last pull request read." } else { "Offline. This is the last pull request read." } } }
                    div { class: "diff-tools", role: "tablist", "aria-label": "Pull request sections",
                        button { role: "tab", "aria-label": "Overview", "aria-selected": "{tab() == Tab::Overview}", "aria-pressed": "{tab() == Tab::Overview}", onclick: move |_| tab.set(Tab::Overview), "Overview" }
                        // A host that cannot show a patch has no files to list.
                        if value["capabilities"]["diff"] != false {
                            button { role: "tab", "aria-label": "Files changed", "aria-selected": "{tab() == Tab::Files}", "aria-pressed": "{tab() == Tab::Files}", onclick: move |_| tab.set(Tab::Files), "Files changed" }
                        }
                        button { role: "tab", "aria-label": "Pull request conversation", "aria-selected": "{tab() == Tab::Conversation}", "aria-pressed": "{tab() == Tab::Conversation}", onclick: move |_| tab.set(Tab::Conversation), "Conversation" }
                    }
                    div { class: "diff-content pr-content",
                        if tab() == Tab::Overview {
                            Overview { state, transport: transport.clone(), reference: reference.clone(), on_changed: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); }, value: value.clone(), number: reference.number, show_writes: true, why: write_reason.clone(), writable, busy: busy.clone(), preferred: preferred(), on_method: move |method: String| preferred.set(method),
                                on_action: move |(action, method): (String, Option<String>)| {
                                    if pr::confirmation(&action, reference.number, method.as_deref().unwrap_or("merge"), 0).is_some() { confirm.set(Some(format!("{action}|{}", method.unwrap_or_default()))); } else { run.call((action, method)); }
                                } }
                        } else if tab() == Tab::Files {
                            crate::pull_request_files::Files { state, transport: transport.clone(), reference: reference.clone(), revision: refresh_count(), readable: can_read(), writable, why: write_reason.clone(),
                                viewed_store: pr::viewed_store(&value).map(str::to_owned), threads: threads_of(&activity.read()), verdicts: verdicts_of(&value), can_reply: can_reply(&value), can_resolve: can_resolve(&value), on_changed: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); } }
                        } else {
                            crate::pull_request_review::Threads { state, transport: transport.clone(), reference: reference.clone(), threads: threads_of(&activity.read()), can_reply: can_reply(&value), can_resolve: can_resolve(&value), writable, why: write_reason.clone(), on_changed: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); } }
                            crate::pull_request_review::ReviewForm { state, transport: transport.clone(), reference: reference.clone(), verdicts: verdicts_of(&value), writable, why: write_reason.clone(), on_done: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); } }
                            Conversation { state, transport: transport.clone(), reference: reference.clone(), detail: value.clone(), on_changed: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); }, activity: activity.read().clone(), writable, busy: busy.clone(), comment: comment(), shown: shown_comments(), on_more: move |_| shown_comments.set(shown_comments() + COMMENTS_PER_PAGE),
                                on_input: move |text: String| comment.set(text), on_send: move |_| send_comment.call(()) }
                        }
                    }
                    if let Some(chosen) = confirm.read().clone() {
                        {
                            let (action, method) = chosen.split_once('|').map(|(a, m)| (a.to_owned(), m.to_owned())).unwrap_or((chosen.clone(), String::new()));
                            let workflows = value["workflowApprovalsRequired"].as_u64().unwrap_or(0);
                            let method = if method.is_empty() { None } else { Some(method) };
                            match pr::confirmation(&action, reference.number, method.as_deref().unwrap_or("merge"), workflows) {
                                Some(copy) => rsx! {
                                    div { class: "modal-backdrop",
                                        div { class: "dialog pr-confirm", role: "alertdialog", "aria-modal": "true", "aria-labelledby": "pr-confirm-heading", "aria-describedby": "pr-confirm-description", tabindex: "-1",
                                            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                                            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); confirm.set(None); } },
                                            h2 { id: "pr-confirm-heading", "{copy.title}" }
                                            p { id: "pr-confirm-description", "{copy.description}" }
                                            div { class: "dialog-actions",
                                                button { r#type: "button", "aria-label": "Cancel", onclick: move |_| confirm.set(None), "Cancel" }
                                                button { class: if copy.destructive { "destructive" } else { "primary" }, r#type: "button", "aria-label": "Confirm {copy.button}", disabled: busy.is_some() || !writable,
                                                    onclick: move |_| { confirm.set(None); run.call((action.clone(), method.clone())); }, "{copy.button}" }
                                            }
                                        }
                                    }
                                },
                                None => rsx! {},
                            }
                        }
                    }
                },
            }
        }
    }
}

/// The review threads an activity read carried, if it has been read.
fn threads_of(activity: &Read<Rc<Value>>) -> Vec<Value> {
    match activity {
        Read::Ready(value) => value["reviewThreads"].as_array().cloned().unwrap_or_default(),
        _ => Vec::new(),
    }
}
/// Whether the host takes replies in a review thread and this account may comment: the original's
/// `hostReview.reply && viewer.comment`.
fn can_reply(detail: &Value) -> bool {
    detail["capabilities"]["review"]["reply"] == true && detail["viewerPermissions"]["comment"] == true
}
/// Whether the host resolves threads and this account may: `hostReview.resolve && viewer.resolve`.
fn can_resolve(detail: &Value) -> bool {
    detail["capabilities"]["review"]["resolve"] == true && detail["viewerPermissions"]["resolve"] == true
}
/// Whether the host takes comments and this account may make one.
fn can_comment(detail: &Value) -> bool {
    detail["capabilities"]["comment"] == true && detail["viewerPermissions"]["comment"] == true
}
/// The verdicts this reader may give: the host offers them and the account may use them.
fn verdicts_of(detail: &Value) -> Vec<String> {
    let may: Vec<&str> = detail["viewerPermissions"]["verdicts"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    detail["capabilities"]["review"]["verdicts"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|verdict| may.contains(verdict)).map(str::to_owned).collect()
}

fn state_label(value: &Value) -> &'static str {
    match (value["state"].as_str(), value["isDraft"] == true) {
        (Some("merged"), _) => "Merged",
        (Some("closed"), _) => "Closed",
        (_, true) => "Draft",
        _ => "Open",
    }
}

#[component]
fn Header(state: Store<UiModel>, transport: TransportHandle, thread_id: String, value: Rc<Value>, reference: Reference, writable: bool, on_close: Callback<()>) -> Element {
    let title = value["title"].as_str().unwrap_or_default().to_owned();
    let url = value["url"].as_str().filter(|url| url.starts_with("https://") || url.starts_with("http://")).map(str::to_owned);
    let label = state_label(&value);
    let host_label = pr::open_on_host_label(value["provider"].as_str());
    rsx! {
        header { class: "diff-header",
            div { class: "diff-header-main",
                strong { class: "pr-number", "#{reference.number}" }
                span { class: "pr-state", "data-state": "{label}", "{label}" }
                if let Some(url) = url { a { class: "pr-link", href: "{url}", target: "_blank", rel: "noopener noreferrer", "aria-label": "Open pull request #{reference.number} on host", title: "{host_label}", "{host_label}" } }
                if let Some(url) = value["url"].as_str().map(str::to_owned) { crate::pull_request_links::LinkControl { state, transport: transport.clone(), thread_id: thread_id.clone(), reference: reference.clone(), url } }
                crate::pull_request_links::WatchControl { state, transport: transport.clone(), thread_id: thread_id.clone(), reference: reference.clone(), open: matches!(label, "Open" | "Draft") }
                button { class: "icon-button diff-close", "aria-label": "Close pull request panel", title: "Close the pull request panel", onclick: move |_| on_close.call(()), "\u{2715}" }
            }
            crate::pull_request_edit::TitleLine { title, can_edit: t3_client::pull_request_edit::can_edit_change_request(&value), writable }
            p { class: "pr-branches muted", "{value[\"headBranch\"].as_str().unwrap_or_default()} \u{2192} {value[\"baseBranch\"].as_str().unwrap_or_default()}" }
        }
    }
}

#[component]
fn Overview(state: Store<UiModel>, transport: TransportHandle, reference: Reference, on_changed: Callback<()>, value: Rc<Value>, number: u64, show_writes: bool, why: Option<String>, writable: bool, busy: Option<String>, preferred: String, on_method: Callback<String>, on_action: Callback<(String, Option<String>)>) -> Element {
    let checks: Vec<Value> = value["checks"].as_array().cloned().unwrap_or_default();
    let summary = pr::describe_checks(&checks);
    let [attention, running, done] = pr::group_checks(&checks);
    let allowed = pr::allowed_merge_methods(&value);
    let method = pr::resolve_merge_method(&allowed, &preferred);
    let control = pr::primary_for(&value);
    let freshness = pr::base_freshness(&value);
    let approvals = value["workflowApprovalsRequired"].as_u64().unwrap_or(0);
    let pr_state = value["state"].as_str().unwrap_or("open").to_owned();
    let auto = value["autoMergeEnabled"] == true;
    let body = value["body"].as_str().unwrap_or_default().trim().to_owned();
    let author = value["author"]["login"].as_str().unwrap_or_default().to_owned();
    let (additions, deletions, files) = (value["additions"].as_u64().unwrap_or(0), value["deletions"].as_u64().unwrap_or(0), value["changedFiles"].as_u64().unwrap_or(0));
    let can = |action: &str| writable && pr::can_perform(&value, action);
    let idle = busy.is_none();
    // Why the controls are off, where the connection (not the host) is the reason.
    let title = why.clone().unwrap_or_default();
    let none_reason = |action: &str| if !pr::can_perform(&value, action) { Some("Not available for this pull request.") } else { None };
    let _ = none_reason;
    rsx! {
        section { class: "pr-overview",
            p { class: "pr-meta muted",
                if !author.is_empty() { "{author}" }
                // A size is drawn only when the host counted one; zero is how a host that does not says nothing.
                if pr::has_size(additions, deletions) { if !author.is_empty() { " \u{b7} " } "+{additions} \u{2212}{deletions}" if files > 0 { " in {files} files" } }
                else if files > 0 { if !author.is_empty() { " \u{b7} " } "{files} files" }
            }
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
                    if auto && pr::can_perform(&value, "disable-auto-merge") {
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
                    if approvals > 0 && pr::can_perform(&value, "approve-workflows") {
                        button { "aria-label": "Approve workflows", title: "{title}", disabled: !can("approve-workflows") || !idle, onclick: move |_| on_action.call(("approve-workflows".into(), None)), "Approve workflows ({approvals})" }
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
            }
            section { class: "pr-people", "aria-label": "Reviewers and labels",
                // The row is always there; the menu only where the host can say who could be asked (Azure DevOps takes
                // a reviewer but lists nobody, so there is nothing to open).
                p { class: "pr-meta", "Reviewers: " if value["reviewers"].as_array().is_none_or(Vec::is_empty) { span { class: "muted", "None" } } else { for reviewer in value["reviewers"].as_array().cloned().unwrap_or_default() { span { key: "{reviewer[\"login\"].as_str().unwrap_or_default()}", class: "pr-reviewer", "{reviewer[\"login\"].as_str().unwrap_or_default()} " } } } }
                if value["capabilities"]["reviewers"]["request"] == true && value["capabilities"]["reviewers"]["listCandidates"] == true {
                    crate::pull_request_menus::Picker { state, transport: transport.clone(), reference: reference.clone(), kind: crate::pull_request_menus::Kind::Reviewers, allowed: value["viewerPermissions"]["requestReviewers"] == true, writable: show_writes && writable, on_changed }
                }
                if value["labels"].as_array().is_some_and(|labels| !labels.is_empty()) || value["capabilities"]["labels"] == true {
                    p { class: "pr-meta", "Labels: " if value["labels"].as_array().is_none_or(Vec::is_empty) { span { class: "muted", "None" } } else { for label in value["labels"].as_array().cloned().unwrap_or_default() { span { key: "{label[\"name\"].as_str().unwrap_or_default()}", class: "pr-label", "{label[\"name\"].as_str().unwrap_or_default()} " } } } }
                    if value["capabilities"]["labels"] == true { crate::pull_request_menus::Picker { state, transport: transport.clone(), reference: reference.clone(), kind: crate::pull_request_menus::Kind::Labels, allowed: value["viewerPermissions"]["labels"] != false, writable: show_writes && writable, on_changed } }
                }
            }
            // A host that reports no checks at all (the answer is null) shows no section: nothing is not "no checks".
            if value["checks"].is_array() { section { class: "pr-checks", "aria-label": "Checks",
                h3 { "Checks" }
                p { class: "pr-checks-summary", role: "status", "{summary}" }
                for (heading, group) in [("Needs attention", attention), ("Running", running), ("Completed", done)] {
                    if !group.is_empty() {
                        div { key: "{heading}", class: "pr-check-group",
                            h4 { "{heading}" }
                            ul {
                                for check in group {
                                    {
                                        let name = check["name"].as_str().unwrap_or_default().to_owned();
                                        let status = check["status"].as_str().unwrap_or_default().to_owned();
                                        let url = check["url"].as_str().filter(|url| url.starts_with("https://") || url.starts_with("http://")).map(str::to_owned);
                                        let text = check["description"].as_str().filter(|text| !text.is_empty()).map(str::to_owned);
                                        rsx! {
                                            li { key: "{name}", class: "pr-check", "data-status": "{status}",
                                                span { class: "pr-check-name", "{name}" }
                                                span { class: "pr-check-status muted", "{status}" }
                                                if let Some(url) = url { a { href: "{url}", target: "_blank", rel: "noopener noreferrer", "Details" } }
                                                if let Some(text) = text { span { class: "muted", "{text}" } }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } }
            section { class: "pr-description", "aria-label": "Description",
                h3 { "Description" }
                if !matches!(crate::pull_request_edit::editing().and_then(|editing| editing.edit.read().clone()), Some(crate::pull_request_edit::Edit::Body(_))) {
                    if body.is_empty() { p { class: "muted", "No description provided." } } else { Markdown { text: body.clone() } }
                }
                crate::pull_request_edit::DescriptionEdit { body: value["body"].as_str().unwrap_or_default().to_owned(), can_edit: t3_client::pull_request_edit::can_edit_change_request(&value), writable }
                crate::pull_request_edit::Reactions { state, transport: transport.clone(), reference: reference.clone(), reactions: value["reactions"].as_array().cloned().unwrap_or_default(), subject: None, can_react: value["capabilities"]["reactions"] == true, writable, on_changed }
            }
        }
    }
}

#[component]
fn Conversation(state: Store<UiModel>, transport: TransportHandle, reference: Reference, detail: Rc<Value>, on_changed: Callback<()>, activity: Read<Rc<Value>>, writable: bool, busy: Option<String>, comment: String, shown: usize, on_more: Callback<()>, on_input: Callback<String>, on_send: Callback<()>) -> Element {
    rsx! {
        section { class: "pr-conversation", "aria-label": "Conversation",
            match activity {
                Read::Loading => rsx! { p { class: "diff-message", role: "status", "Loading conversation\u{2026}" } },
                Read::Failed(message) => rsx! { p { class: "diff-error", role: "alert", "{message}" } },
                Read::Ready(value) => {
                    let comments = value["comments"].as_array().cloned().unwrap_or_default();
                    let commits = value["commits"].as_array().cloned().unwrap_or_default();
                    let total = comments.len();
                    rsx! {
                        if !commits.is_empty() {
                            details { class: "pr-commits", open: true,
                                summary { "{commits.len()} " if commits.len() == 1 { "commit" } else { "commits" } }
                                ol { class: "pr-commit-list", "aria-label": "Commits",
                                    for commit in commits.iter() {
                                        li { key: "{commit[\"oid\"].as_str().unwrap_or_default()}", class: "pr-commit",
                                            code { "{pr::short_oid(commit[\"oid\"].as_str().unwrap_or_default())}" }
                                            span { class: "pr-commit-title", "{commit[\"messageHeadline\"].as_str().unwrap_or_default()}" }
                                            if commit["additions"].is_u64() || commit["deletions"].is_u64() { span { class: "muted", "+{commit[\"additions\"].as_u64().unwrap_or(0)} \u{2212}{commit[\"deletions\"].as_u64().unwrap_or(0)}" } }
                                            span { class: "muted", title: "{commit[\"committedDate\"].as_str().unwrap_or_default()}", "{crate::pull_request_review::when(commit[\"committedDate\"].as_str().unwrap_or_default())}" }
                                        }
                                    }
                                }
                            }
                        }
                        if comments.is_empty() { p { class: "muted", "No comments yet." } }
                        ol { class: "pr-comments",
                            for item in comments.into_iter().take(shown) {
                                {
                                    let id = item["id"].as_str().unwrap_or_default().to_owned();
                                    let author = item["author"]["login"].as_str().unwrap_or("ghost").to_owned();
                                    let at = item["createdAt"].as_str().unwrap_or_default().to_owned();
                                    let kind = item["kind"].as_str().unwrap_or_default().to_owned();
                                    let verb = pr::comment_verb(&kind, item["reviewState"].as_str(), item["path"].as_str());
                                    let body = item["body"].as_str().unwrap_or_default().to_owned();
                                    let can_edit = t3_client::pull_request_edit::can_edit_comment(&detail, &item);
                                    let reactions = item["reactions"].as_array().cloned().unwrap_or_default();
                                    rsx! {
                                        li { key: "{id}", class: "pr-comment",
                                            div { class: "pr-comment-head", strong { "{author}" } span { class: "muted", "{verb}" } span { class: "muted", title: "{at}", "{crate::pull_request_review::when(&at)}" } }
                                            crate::pull_request_edit::CommentEdit { id: id.clone(), kind: kind.clone(), body: body.clone(), can_edit, writable,
                                                if !body.trim().is_empty() { Markdown { text: body.clone() } } }
                                            crate::pull_request_edit::Reactions { state, transport: transport.clone(), reference: reference.clone(), reactions, subject: Some(id.clone()), can_react: detail["capabilities"]["reactions"] == true, writable, on_changed }
                                        }
                                    }
                                }
                            }
                        }
                        if shown < total { button { "aria-label": "Show more comments", onclick: move |_| on_more.call(()), "Show {(total - shown).min(COMMENTS_PER_PAGE)} more comments ({total - shown} not shown)" } }
                        if value["commentsTruncated"] == true { p { class: "muted", "Only the newest comments are shown. Open the pull request on the host to read the rest." } }
                    }
                }
            }
            if can_comment(&detail) {
                form { class: "pr-comment-form", onsubmit: move |event| { event.prevent_default(); on_send.call(()); },
                    label { r#for: "pr-comment", class: "sr-only", "Add a comment" }
                    textarea { id: "pr-comment", "aria-label": "Add a comment", placeholder: "Leave a comment", value: "{comment}", disabled: !writable, oninput: move |event| on_input.call(event.value()) }
                    button { class: "primary", r#type: "button", "aria-label": "Comment", disabled: !writable || busy.is_some() || comment.trim().is_empty(), onclick: move |_| on_send.call(()), "Comment" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transport_tests::{click_control, control, control_attribute, rendered_text};
    use std::cell::RefCell;

    #[derive(Clone)]
    struct Props {
        value: Rc<Value>,
        show_writes: bool,
        writable: bool,
        busy: Option<String>,
        calls: Rc<RefCell<Vec<(String, Option<String>)>>>,
    }
    impl PartialEq for Props {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value && self.show_writes == other.show_writes && self.writable == other.writable && self.busy == other.busy
        }
    }
    fn harness(props: Props) -> Element {
        let calls = props.calls.clone();
        rsx! { Overview { state: use_store(UiModel::default), transport: TransportHandle::default(), reference: Reference { project_id: "p".into(), host: None, repository: "acme/web".into(), number: 7 }, on_changed: |_| {}, value: props.value.clone(), number: 7, show_writes: props.show_writes, why: None, writable: props.writable, busy: props.busy.clone(), preferred: "merge".to_owned(), on_method: |_| {}, on_action: move |call: (String, Option<String>)| calls.borrow_mut().push(call) } }
    }
    fn detail(actions: &[&str]) -> Value {
        json!({"state":"open","isDraft":false,"mergeability":"mergeable","title":"Fix it","body":"Please **merge**.","additions":3,"deletions":1,"changedFiles":2,
            "capabilities":{"actions":["merge","close","reopen","ready","draft","enable-auto-merge","disable-auto-merge","revert","approve-workflows","update-branch"],"mergeMethods":["merge","squash"],"updateMethods":["merge","rebase"]},
            "mergeCapabilities":{"merge":true,"squash":true,"rebase":false},
            "viewerPermissions":{"actions":actions,"updateMethods":["rebase"],"comment":true},
            "checks":[{"name":"build","status":"success","description":null,"url":null},{"name":"lint","status":"failure","description":"2 errors","url":"https://ci/2"}],
            "baseComparison":"behind","behindBy":2,"autoMergeEnabled":false,"workflowApprovalsRequired":0})
    }
    fn mount(value: Value, show_writes: bool, writable: bool, busy: Option<&str>) -> (VirtualDom, Rc<RefCell<Vec<(String, Option<String>)>>>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut dom = VirtualDom::new_with_props(harness, Props { value: Rc::new(value), show_writes, writable, busy: busy.map(str::to_owned), calls: calls.clone() });
        dom.rebuild_in_place();
        (dom, calls)
    }

    #[test]
    fn the_overview_offers_only_what_the_host_and_the_reader_allow_and_reports_the_checks() {
        let (mut dom, calls) = mount(detail(&["merge", "close", "draft"]), true, true, None);
        let text = rendered_text(&dom);
        // Checks: the sentence, the failing one first with its reason.
        assert!(text.contains("1 of 2 failing") && text.contains("Needs attention") && text.contains("2 errors") && text.contains("Completed"), "{text}");
        assert!(text.contains("Please") && text.contains("+3") && text.contains("2 files"));
        // Offered: merge (a method is allowed), draft, close. Not offered: reopen, revert, ready.
        assert!(control(&dom, "Merge").is_some() && control(&dom, "Convert to draft").is_some() && control(&dom, "Close pull request").is_some());
        assert!(control(&dom, "Reopen pull request").is_none() && control(&dom, "Ready for review").is_none() && control(&dom, "Revert").is_none());
        // Update branch needs the host's method and the reader's permission to agree on one.
        assert!(control(&dom, "Update branch").is_some() && rendered_text(&dom).contains("2 commits behind"));
        click_control(&mut dom, "Merge");
        assert_eq!(calls.borrow().as_slice(), [("merge".to_owned(), Some("merge".to_owned()))]);
        click_control(&mut dom, "Close pull request");
        assert_eq!(calls.borrow()[1], ("close".to_owned(), None));
    }

    #[test]
    fn without_a_permission_the_button_is_absent_and_without_the_connection_every_write_is() {
        let (dom, _) = mount(detail(&["close"]), true, true, None);
        assert!(control(&dom, "Merge").is_none(), "an action the reader may not take is not offered");
        assert!(control(&dom, "Close pull request").is_some());
        let (dom, _) = mount(detail(&["merge", "close", "draft"]), false, false, None);
        for label in ["Merge", "Close pull request", "Convert to draft", "Update branch", "Approve workflows"] {
            assert!(control(&dom, label).is_none(), "a read-only connection sees no {label}");
        }
        assert!(rendered_text(&dom).contains("1 of 2 failing"), "it still reads");
    }

    #[test]
    fn a_write_in_flight_and_an_old_connection_both_disable_the_buttons() {
        let (dom, _) = mount(detail(&["merge", "close"]), true, true, Some("merge"));
        assert_eq!(control_attribute(&dom, "Merge", "disabled").unwrap().1.as_deref(), Some("true"));
        assert_eq!(control_attribute(&dom, "Close pull request", "disabled").unwrap().1.as_deref(), Some("true"));
        assert!(rendered_text(&dom).contains("Working on merge"));
        let (dom, _) = mount(detail(&["merge"]), true, false, None);
        assert_eq!(control_attribute(&dom, "Merge", "disabled").unwrap().1.as_deref(), Some("true"), "offline: last read, nothing sent");
    }

    /// What each host's server sends for the pieces the panel is capability-driven by.
    fn host_detail(provider: &str, capabilities: Value, permissions: Value, extra: Value) -> Value {
        let mut value = json!({"provider":provider,"state":"open","isDraft":false,"mergeability":"mergeable","title":"Fix it","body":"Body text","labels":[],"reviewers":[{"login":"bob"}],
            "capabilities":capabilities,"mergeCapabilities":{"merge":true,"squash":true,"rebase":true},"viewerPermissions":permissions});
        value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        value
    }
    fn mounted_text(value: Value) -> String {
        let (dom, _) = mount(value, true, true, None);
        rendered_text(&dom)
    }
    fn mounted_threads(can_reply: bool, can_resolve: bool) -> VirtualDom {
        #[derive(Clone, PartialEq)]
        struct Threads(bool, bool);
        fn view(props: Threads) -> Element {
            let thread = json!({"id":"t1","path":"a.ts","line":4,"isResolved":false,"isOutdated":false,"comments":[{"id":"c1","author":{"login":"bob"},"createdAt":"now","body":"Rename this"}]});
            rsx! { crate::pull_request_review::Threads { state: use_store(UiModel::default), transport: TransportHandle::default(), reference: Reference { project_id: "p".into(), host: None, repository: "acme/web".into(), number: 7 }, threads: vec![thread], can_reply: props.0, can_resolve: props.1, writable: true, why: None, on_changed: |_| {} } }
        }
        let mut dom = VirtualDom::new_with_props(view, Threads(can_reply, can_resolve));
        dom.rebuild_in_place();
        dom
    }

    /// Bitbucket: merge and close only; the verdicts include a request for changes; the viewed marks are the
    /// environment's; the host counts no lines, summarises no review and gives no base comparison.
    #[test]
    fn a_bitbucket_pull_request_offers_merge_and_close_and_nothing_the_host_cannot_do() {
        let capabilities = json!({"diff":true,"comment":true,"actions":["merge","close"],"mergeMethods":["merge","squash"],"search":false,"viewedFiles":"environment",
            "review":{"inlineComment":true,"reply":true,"resolve":true,"verdicts":["comment","approve","request-changes"]},"reviewers":{"request":true,"listCandidates":true},"edit":{"changeRequest":true,"comment":true},"labels":false});
        let permissions = json!({"actions":["merge","close"],"comment":true,"resolve":true,"verdicts":["comment","approve","request-changes"],"requestReviewers":true});
        let detail = host_detail("bitbucket", capabilities, permissions, json!({"checks":[{"name":"Build","status":"success","description":"ok","url":null}],"reviewers":[{"id":"{bob-uuid}","login":"bob"}]}));
        let text = mounted_text(detail.clone());
        let (dom, _) = mount(detail.clone(), true, true, None);
        assert!(control(&dom, "Merge").is_some() && control(&dom, "Close pull request").is_some(), "{text}");
        for absent in ["Convert to draft", "Ready for review", "Reopen pull request", "Revert", "Update branch", "Enable auto-merge", "Approve workflows"] {
            assert!(control(&dom, absent).is_none(), "Bitbucket has no {absent}");
        }
        assert!(!text.contains("commits behind") && !text.contains("+0") && !text.contains("0 files") && control(&dom, "Change labels").is_none() && !text.contains("Labels:"), "{text}");
        assert!(text.contains("Reviewers: ") && control(&dom, "Request a review").is_some() && text.contains("Build"), "{text}");
        assert_eq!(verdicts_of(&detail), ["comment", "approve", "request-changes"]);
        assert!(can_comment(&detail) && can_reply(&detail) && can_resolve(&detail) && pr::viewed_store(&detail) == Some("environment"));
    }

    /// Azure DevOps: no comment composer, reactions, labels, verdicts, replies or resolution, no list of people to
    /// ask for a review, no checks (the answer is null: no heading, not an empty one), and no counts it did not give.
    #[test]
    fn an_azure_devops_pull_request_shows_no_composer_checks_counts_or_reviewer_menu() {
        let capabilities = json!({"diff":true,"comment":false,"actions":["merge","close","reopen","ready","draft"],"mergeMethods":["merge","squash","rebase"],"search":false,
            "review":{"inlineComment":false,"reply":false,"resolve":false,"verdicts":[]},"reviewers":{"request":true,"listCandidates":false},"edit":{"changeRequest":true,"comment":false},"reactions":false,"labels":false,"stacks":false});
        let permissions = json!({"actions":["merge","close","draft"],"comment":true,"resolve":false,"verdicts":[],"requestReviewers":true});
        let detail = host_detail("azure-devops", capabilities, permissions, json!({"checks":null,"reviewers":[{"login":"bob@acme.io"}]}));
        let (dom, _) = mount(detail.clone(), true, true, None);
        let text = rendered_text(&dom);
        assert!(!text.contains("Checks") && !text.contains("No checks reported") && !text.contains("+0") && !text.contains("0 files"), "{text}");
        assert!(text.contains("Reviewers: ") && text.contains("bob@acme.io") && control(&dom, "Request a review").is_none(), "no list to pick from: {text}");
        assert!(control(&dom, "Add a reaction").is_none() && control(&dom, "Change labels").is_none() && !text.contains("Labels:"));
        // The title and description are editable; a comment is not.
        assert!(!can_comment(&detail) && !can_reply(&detail) && !can_resolve(&detail) && verdicts_of(&detail).is_empty());
        assert!(t3_client::pull_request_edit::can_edit_change_request(&detail) && !detail["capabilities"]["edit"]["comment"].as_bool().unwrap());
        let threads = rendered_text(&mounted_threads(can_reply(&detail), can_resolve(&detail)));
        assert!(threads.contains("Rename this"));
        let dom = mounted_threads(false, false);
        assert!(control(&dom, "Reply to thread on a.ts").is_none() && control(&dom, "Resolve thread on a.ts").is_none());
    }

    /// Forgejo and Gitea: labels, reactions, inline review and all three merge methods; merge, close, reopen and
    /// update-branch only; no reply, no resolution.
    #[test]
    fn a_forgejo_pull_request_offers_its_merge_methods_labels_and_update_but_no_reply_or_resolution() {
        let capabilities = json!({"diff":true,"comment":true,"actions":["merge","close","reopen","update-branch"],"mergeMethods":["merge","squash","rebase"],"updateMethods":["merge","rebase"],"search":false,"reactions":true,
            "review":{"inlineComment":true,"reply":false,"resolve":false,"verdicts":["comment","approve","request-changes"]},"reviewers":{"request":true,"listCandidates":true},"edit":{"changeRequest":true,"comment":true},"labels":true});
        let permissions = json!({"actions":["merge","close","update-branch"],"comment":true,"resolve":true,"verdicts":["comment","approve","request-changes"],"requestReviewers":true,"updateMethods":["merge","rebase"],"labels":true});
        let detail = host_detail("forgejo", capabilities, permissions, json!({"checks":[],"labels":[{"name":"bug"}],"baseComparison":"behind","behindBy":2,"reactions":[]}));
        let (mut dom, calls) = mount(detail.clone(), true, true, None);
        let text = rendered_text(&dom);
        assert!(control(&dom, "Merge method").is_some(), "three methods are on offer: {text}");
        assert!(control(&dom, "Update branch").is_some() && text.contains("2 commits behind") && control(&dom, "Change labels").is_some() && text.contains("bug"), "{text}");
        assert!(control(&dom, "Convert to draft").is_none() && control(&dom, "Revert").is_none() && control(&dom, "Enable auto-merge").is_none());
        click_control(&mut dom, "Update branch");
        assert_eq!(calls.borrow().as_slice(), [("update-branch".to_owned(), Some("merge".to_owned()))]);
        assert!(!can_reply(&detail) && !can_resolve(&detail) && can_comment(&detail) && verdicts_of(&detail).contains(&"request-changes".to_owned()));
    }

    #[test]
    fn merged_closed_and_conflicting_pull_requests_say_so_and_offer_the_way_back() {
        let mut merged = detail(&["revert", "close"]);
        merged["state"] = json!("merged");
        let (dom, _) = mount(merged, true, true, None);
        assert!(rendered_text(&dom).contains("This pull request was merged.") && control(&dom, "Revert").is_some() && control(&dom, "Merge").is_none());
        let mut closed = detail(&["reopen"]);
        closed["state"] = json!("closed");
        let (dom, _) = mount(closed, true, true, None);
        assert!(rendered_text(&dom).contains("This pull request is closed.") && control(&dom, "Reopen pull request").is_some());
        let mut conflicting = detail(&["merge"]);
        conflicting["mergeability"] = json!("conflicting");
        let (dom, _) = mount(conflicting, true, true, None);
        assert!(rendered_text(&dom).contains("This branch has conflicts that must be resolved.") && control(&dom, "Merge").is_none());
        let mut draft = detail(&["ready"]);
        draft["isDraft"] = json!(true);
        let (dom, _) = mount(draft, true, true, None);
        assert!(control(&dom, "Ready for review").is_some() && control(&dom, "Merge").is_none());
    }
}
