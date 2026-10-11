//! The pull request panel: the header and the merge row, three tabs (Summary, Timeline, Code, as the original names them) and
//! the composer for a comment or a review. Summary and Timeline are `pull_request_summary`, Code is `pull_request_files`, the
//! composer and the review threads are `pull_request_review`. Port of `PullRequestDetailPanel.tsx`.
//!
//! Reads are held only while the panel is mounted: the detail, the checks and the activity, and
//! the server's refresh stream, whose notice (never a timer) is what makes the panel read again.
//! After the connection is lost everything shown is marked as old and every write is disabled.
//! Every write is guarded against a second click until the server answers, and the ones that
//! cannot be undone ask first, in the original's words.
use crate::{
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

/// Opens the panel on a pull request, as a tab of its thread's panel. A target with no thread belongs to the pull requests
/// page, whose panel stands alone beside the list.
pub(crate) fn open(state: Store<UiModel>, target: Target) {
    if target.thread_id.is_empty() {
        state.pr_panel().set(Some(target));
        return;
    }
    let thread_id = target.thread_id;
    let reference = target.reference;
    crate::right_panel::open_pull_request(state, &thread_id, t3_client::right_panel::PullRequestTarget { environment_id: None, project_id: reference.project_id, host: reference.host, repository: reference.repository, number: reference.number, url: None });
}
/// Closes the pull request in view: the page's panel, or the thread's shown tab.
pub(crate) fn close(state: Store<UiModel>) {
    if state.pr_panel().peek().is_some() && *state.view().peek() == runtime::View::PullRequests {
        state.pr_panel().set(None);
    } else {
        crate::right_panel::close_shown_pull_request(state);
    }
}

/// The pull requests page's panel, beside its list.
#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let target = use_memo(move || state.pr_panel().read().clone().filter(|target| target.thread_id.is_empty()));
    match target() {
        Some(target) => rsx! { Panel { key: "{target.reference.key()}", state, transport, thread_id: target.thread_id, reference: target.reference } },
        None => rsx! {},
    }
}

/// What a read has told the panel.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum Read<T> {
    #[default]
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Summary,
    Timeline,
    Code,
}

#[component]
pub(crate) fn Panel(state: Store<UiModel>, transport: TransportHandle, thread_id: String, reference: Reference) -> Element {
    let mut detail = use_signal(Read::<Rc<Value>>::default);
    let mut activity = use_signal(Read::<Rc<Value>>::default);
    let mut tab = use_signal(|| Tab::Summary);
    let mut timeline_newest_first = use_signal(|| true);
    let mut refreshing = use_signal(|| false);
    let mut refresh_count = use_signal(|| 0u32);
    let mut acting = use_signal(|| None::<String>);
    let mut confirm = use_signal(|| None::<String>);
    let mut preferred = use_signal(|| "merge".to_owned());
    // Whether what is shown was read on a connection that is still up.
    let mut fresh = use_signal(|| false);
    // The host's own stack of this pull request, where it keeps one (read with the detail, and again at each refresh).
    let mut stack = use_signal(crate::pull_request_stack::Held::default);
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
                            // Its tab in the panel's bar wears the state it was last read in.
                            let status = (value["state"].as_str().unwrap_or("open").to_owned(), value["isDraft"] == true);
                            let key = crate::right_panel::status_key(&reference.project_id, &reference.repository, reference.number);
                            if state.pr_states().peek().get(&key) != Some(&status) {
                                state.pr_states().write().insert(key, status);
                            }
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

    // Whether this pull request can be in a stack, and the host acts on stacks: the original asks once the detail has landed, so a
    // pull request nobody can read costs one request rather than two.
    let stack_wanted = use_memo(move || matches!(&*detail.read(), Read::Ready(value) if value["capabilities"]["stacks"] == true) && crate::thread_pull_requests_panel::supported(state));
    let supports_stack_actions = use_memo(move || {
        stack_wanted()
            && matches!(&*detail.read(), Read::Ready(value) if value["capabilities"]["stackActions"] == true)
            && state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.pull_request_stack_actions == Some(true))
    });
    {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_resource(move || {
            let (transport, mut reference, wanted, readable, _attempt) = (transport.clone(), reference.clone(), stack_wanted(), can_read(), refresh_count());
            async move {
                if !wanted || !readable {
                    return;
                }
                // The host of the address the detail names, for a reference that was opened without one.
                if reference.host.is_none() {
                    reference.host = match &*detail.peek() {
                        Read::Ready(value) => value["url"].as_str().and_then(|url| url::Url::parse(url).ok()).and_then(|url| url.host_str().map(str::to_owned)),
                        _ => None,
                    };
                }
                stack.write().pending = true;
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, crate::pull_request_stack::STACK, reference.payload(json!({}))).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                let mut held = stack.write();
                held.pending = false;
                match answer {
                    Ok(value) => {
                        held.data = if value.is_null() { None } else { Some(Rc::new(value)) };
                        held.succeeded = true;
                        held.error = None;
                    }
                    // A failed refresh keeps the layers that were read; the menu says they may be old.
                    Err(message) => {
                        held.succeeded = false;
                        held.error = Some(message);
                    }
                }
            }
        });
    }

    // The stack the threads saved, under the host of the address the detail names where the reference has none.
    let saved_stack = use_memo({
        let reference = reference.clone();
        move || {
            let mut reference = reference.clone();
            if reference.host.is_none() {
                reference.host = match &*detail.read() {
                    Read::Ready(value) => value["url"].as_str().and_then(|url| url::Url::parse(url).ok()).and_then(|url| url.host_str().map(str::to_owned)),
                    _ => None,
                };
            }
            if !stack_wanted() {
                return None;
            }
            crate::pull_request_stack::saved(state, &reference)
        }
    });

    // The conversation (the Summary's comments, the Timeline, the Code tab's threads) is read once the panel is open.
    {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_resource(move || {
            let (transport, reference, wanted, _attempt) = (transport.clone(), reference.clone(), can_read(), refresh_count());
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
    // The button goes around the server's cache rather than through it: it is the answer for a reader who can see that what is
    // shown is behind. The invalidation goes first so the reads that follow miss the cache; if it fails they still run.
    let refresh_from_host = {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_callback(move |_: ()| {
            if *refreshing.peek() {
                return;
            }
            refreshing.set(true);
            let (transport, reference) = (transport.clone(), reference.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let _ = runtime::request_value_reported(transport.clone(), state, pr::INVALIDATE, json!({"reference":reference.payload(json!({}))})).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    let next = *refresh_count.peek() + 1;
                    refresh_count.set(next);
                }
                refreshing.set(false);
            });
        })
    };
    let retry_activity = use_callback(move |_: ()| {
        activity.set(Read::Loading);
        let next = *refresh_count.peek() + 1;
        refresh_count.set(next);
    });
    let write_ok = use_memo(move || can_write() && fresh());
    // What the layers show: the host's last read, or until it succeeds what the threads saved. A read that succeeded with nothing
    // is an absence the saved layers must not override. A saved stack never carries head SHAs, so no action can be asked from it
    // (`fresh` is false until a read has succeeded).
    let mut held = stack.read().clone();
    if !held.succeeded && held.data.is_none() {
        held.data = saved_stack();
    }
    let stack_view = t3_client::pull_request_stack::view(held.data.is_some(), held.succeeded, held.pending, held.error.as_deref());
    let single_merge = t3_client::pull_request_stack::single_merge_allowed(supports_stack_actions(), held.data.is_some(), held.pending || !held.succeeded && held.error.is_none(), held.error.is_some());
    let _editing = crate::pull_request_edit::provide(state, transport.clone(), reference.clone(), write_ok, Callback::new(move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); }));
    let detail_now = detail.read().clone();
    let busy = acting.read().clone();
    let writable = can_write() && fresh();
    let write_reason = if !can_write() && connected() { Some("This connection cannot change source control.".to_owned()) } else if !writable { Some("Offline. This is the last pull request read.".to_owned()) } else { None };
    let stale = !fresh() && matches!(detail_now, Read::Ready(_));
    let github_url = reference.host.as_deref().filter(|host| host.eq_ignore_ascii_case("github.com")).map(|host| format!("https://{host}/{}/pull/{}", reference.repository, reference.number));
    rsx! {
        aside { class: "diff-panel pr-panel", "aria-label": "Pull request panel", tabindex: "-1",
            onkeydown: move |event| { if event.key() == Key::Escape { if confirm.peek().is_some() { confirm.set(None); } else { close(state); } } },
            match detail_now {
                Read::Loading => rsx! { header { class: "diff-header", div { class: "diff-header-main", strong { "#{reference.number}" } button { class: "icon-button diff-close", "aria-label": "Close pull request panel", onclick: move |_| close(state), "\u{2715}" } } }
                    p { class: "diff-message", role: "status", if can_read() { "Loading pull request\u{2026}" } else { "Reconnect to load this pull request." } } },
                Read::Failed(message) => rsx! { header { class: "diff-header", div { class: "diff-header-main", strong { "#{reference.number}" } button { class: "icon-button diff-close", "aria-label": "Close pull request panel", onclick: move |_| close(state), "\u{2715}" } } }
                    if message == pr::NOT_AVAILABLE_FOR_HOST { p { class: "diff-message", role: "status", "{message}" } }
                    else { div { class: "diff-failed", role: "alert",
                        if message == pr::NOT_FOUND {
                            p { class: "pr-unavailable-title", "Pull request #{reference.number} not found" }
                            p { class: "diff-error", "It may be an issue rather than a pull request, or this account can't see it." }
                        } else {
                            p { class: "pr-unavailable-title", "Could not load pull request" }
                            p { class: "diff-error", "{message}" }
                        }
                        div { class: "pr-failed-actions",
                            button { "aria-label": "Retry loading pull request", onclick: move |_| { detail.set(Read::Loading); let next = *refresh_count.peek() + 1; refresh_count.set(next); }, "Retry" }
                            // The one link out that can be made without the host's answer: GitHub's address is a function of the request.
                            if let Some(url) = github_url.clone() { a { class: "pr-link", href: "{url}", target: "_blank", rel: "noopener noreferrer", "aria-label": "Open pull request #{reference.number} on GitHub", "Open on GitHub" } }
                        } } } },
                Read::Ready(value) => rsx! {
                    Header { state, transport: transport.clone(), thread_id: thread_id.clone(), value: value.clone(), activity: activity.read().clone(), reference: reference.clone(), writable, refreshing: refreshing(), on_refresh: refresh_from_host, on_close: move |_| close(state),
                        stack: held.clone(), stack_view: stack_view.clone(), stack_wanted: stack_wanted(), supports_stack_actions: supports_stack_actions(),
                        can_merge_stack: stack_view.fresh && supports_stack_actions() && writable && pr::can_perform(&value, "merge") && !pr::allowed_merge_methods(&value).is_empty(),
                        can_rebase_stack: stack_view.fresh && supports_stack_actions() && writable && value["viewerPermissions"]["stackRebase"] == true,
                        merge_method: pr::resolve_merge_method(&pr::allowed_merge_methods(&value), &preferred()),
                        on_stack_changed: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); } }
                    if stale { p { class: "diff-banner", role: "status", if connected() { "Could not refresh. This is the last pull request read." } else { "Offline. This is the last pull request read." } } }
                    crate::pull_request_summary::Actions { value: value.clone(), show_writes: true, why: write_reason.clone(), writable, busy: busy.clone(), preferred: preferred(), single_merge, settled: !supports_stack_actions() || held.succeeded || held.error.is_some(), on_method: move |method: String| preferred.set(method),
                        on_action: move |(action, method): (String, Option<String>)| {
                            if pr::confirmation(&action, reference.number, method.as_deref().unwrap_or("merge"), 0).is_some() { confirm.set(Some(format!("{action}|{}", method.unwrap_or_default()))); } else { run.call((action, method)); }
                        } }
                    nav { class: "diff-tools pr-tabs", "aria-label": "Pull request tabs",
                        div { class: "pr-tab-buttons", role: "tablist", "aria-label": "Pull request sections",
                            button { role: "tab", "aria-label": "Summary", "aria-selected": "{tab() == Tab::Summary}", "aria-pressed": "{tab() == Tab::Summary}", onclick: move |_| tab.set(Tab::Summary), "Summary" }
                            button { role: "tab", "aria-label": "Timeline", "aria-selected": "{tab() == Tab::Timeline}", "aria-pressed": "{tab() == Tab::Timeline}", onclick: move |_| tab.set(Tab::Timeline), "Timeline" }
                            // A host that cannot show a patch has no code to read.
                            if value["capabilities"]["diff"] != false {
                                button { role: "tab", "aria-label": "Code", "aria-selected": "{tab() == Tab::Code}", "aria-pressed": "{tab() == Tab::Code}", onclick: move |_| tab.set(Tab::Code), "Code" }
                            }
                        }
                        if tab() == Tab::Summary {
                            crate::pull_request_summary::ChecksLine { state, value: value.clone(), writable, busy: busy.clone(), why: write_reason.clone(), stale: false,
                                on_action: move |(action, method): (String, Option<String>)| {
                                    if pr::confirmation(&action, reference.number, method.as_deref().unwrap_or("merge"), 0).is_some() { confirm.set(Some(format!("{action}|{}", method.unwrap_or_default()))); } else { run.call((action, method)); }
                                } }
                        } else if tab() == Tab::Timeline {
                            crate::pull_request_summary::TimelineAside { activity: activity.read().clone(), newest_first: timeline_newest_first(), on_order: move |_| { let now = *timeline_newest_first.peek(); timeline_newest_first.set(!now) } }
                        }
                    }
                    div { class: "diff-content pr-content",
                        if tab() == Tab::Summary {
                            crate::pull_request_summary::Summary { state, transport: transport.clone(), reference: reference.clone(), value: value.clone(), activity: activity.read().clone(), writable, on_changed: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); }, on_retry: retry_activity }
                        } else if tab() == Tab::Timeline {
                            crate::pull_request_summary::Timeline { state, transport: transport.clone(), reference: reference.clone(), detail: value.clone(), activity: activity.read().clone(), newest_first: timeline_newest_first(), writable, on_changed: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); }, on_retry: retry_activity }
                        } else {
                            crate::pull_request_files::Files { state, transport: transport.clone(), reference: reference.clone(), revision: refresh_count(), readable: can_read(), writable, why: write_reason.clone(),
                                viewed_store: pr::viewed_store(&value).map(str::to_owned), threads: threads_of(&activity.read()), can_inline: can_inline(&value), can_reply: can_reply(&value), can_resolve: can_resolve(&value), on_changed: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); } }
                        }
                    }
                    crate::pull_request_review::Composer { state, transport: transport.clone(), reference: reference.clone(), detail: value.clone(), writable, busy: busy.clone(), why: write_reason.clone(),
                        on_commented: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); },
                        on_action: move |(action, method): (String, Option<String>)| run.call((action, method)),
                        on_review_sent: move |_| { let next = *refresh_count.peek() + 1; refresh_count.set(next); } }
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
/// Whether the host takes a comment on a line and this account may make one: the Code tab offers the button only then.
fn can_inline(detail: &Value) -> bool {
    detail["capabilities"]["review"]["inlineComment"] == true && detail["viewerPermissions"]["comment"] == true
}
/// Whether the host takes comments and this account may make one.
#[cfg(test)]
fn can_comment(detail: &Value) -> bool {
    detail["capabilities"]["comment"] == true && detail["viewerPermissions"]["comment"] == true
}
/// The verdicts this reader may give (tests of what each host offers read them; the composer works them out itself): the host offers them and the account may use them.
#[cfg(test)]
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
fn Header(state: Store<UiModel>, transport: TransportHandle, thread_id: String, value: Rc<Value>, activity: Read<Rc<Value>>, reference: Reference, writable: bool, refreshing: bool, on_refresh: Callback<()>, on_close: Callback<()>,
    stack: crate::pull_request_stack::Held, stack_view: t3_client::pull_request_stack::View, stack_wanted: bool, supports_stack_actions: bool, can_merge_stack: bool, can_rebase_stack: bool, merge_method: String, on_stack_changed: Callback<()>) -> Element {
    let title = value["title"].as_str().unwrap_or_default().to_owned();
    let url = value["url"].as_str().filter(|url| url.starts_with("https://") || url.starts_with("http://")).map(str::to_owned);
    let label = state_label(&value);
    let host_label = pr::open_on_host_label(value["provider"].as_str());
    let (menu_url, menu_provider) = (url.clone(), value["provider"].as_str().map(str::to_owned));
    rsx! {
        header { class: "diff-header",
            div { class: "diff-header-main",
                strong { class: "pr-number", oncontextmenu: { let (url, provider) = (menu_url.clone(), menu_provider.clone()); move |event| { event.prevent_default(); event.stop_propagation(); let at = event.client_coordinates(); if let Some(url) = url.clone() { let provider = provider.clone(); spawn(async move { crate::link_menus::pull_request_number(state, url, provider, (at.x, at.y)).await }); } } }, "#{reference.number}" }
                span { class: "pr-state", "data-state": "{label}", "{label}" }
                if stack_wanted {
                    // The host's stack, read with the pull request: its layers, and what can be done to all of them.
                    if let Some(data) = stack.data.clone() {
                        crate::pull_request_stack::Menu { state, transport: transport.clone(), thread_id: thread_id.clone(), reference: reference.clone(), stack: data, notice: stack_view.notice.map(str::to_owned),
                            retry: stack.error.is_some().then(|| on_stack_changed), can_merge: can_merge_stack, can_rebase: can_rebase_stack, merge_method: merge_method.clone(), on_acted: on_stack_changed }
                    } else if supports_stack_actions && stack.error.is_some() {
                        button { r#type: "button", "aria-label": "Retry stack lookup", onclick: move |_| on_stack_changed.call(()), "Retry stack lookup" }
                    }
                } else if let (Some(stack_number), Some(position), Some(size)) = (value["stack"]["number"].as_u64(), value["stack"]["position"].as_u64(), value["stack"]["size"].as_u64()) {
                    crate::pull_request_stack::Chip { state, transport: transport.clone(), thread_id: thread_id.clone(), reference: reference.clone(), number: stack_number, position, size }
                }
                if let Some(url) = url { a { class: "pr-link", href: "{url}", target: "_blank", rel: "noopener noreferrer", "aria-label": "Open pull request #{reference.number} on host", title: "{host_label}", "{host_label}" } }
                if let Some(url) = value["url"].as_str().map(str::to_owned) { crate::pull_request_links::LinkControl { state, transport: transport.clone(), thread_id: thread_id.clone(), reference: reference.clone(), url } }
                crate::pull_request_links::WatchControl { state, transport: transport.clone(), thread_id: thread_id.clone(), reference: reference.clone(), open: matches!(label, "Open" | "Draft") }
                button { class: "icon-button", r#type: "button", "aria-label": if refreshing { "Refreshing pull request" } else { "Refresh pull request" }, title: "Ask the host again", "aria-busy": "{refreshing}", disabled: refreshing, onclick: move |_| on_refresh.call(()), "\u{21bb}" }
                if thread_id.is_empty() { crate::pull_request_handoff::CheckoutMenu { state, transport: transport.clone(), project_id: reference.project_id.clone(), value: value.clone() } }
                crate::pull_request_handoff::Menu { state, transport: transport.clone(), project_id: reference.project_id.clone(), thread_id: thread_id.clone(), value: value.clone(), activity }
                button { class: "icon-button diff-close", "aria-label": "Close pull request panel", title: "Close the pull request panel", onclick: move |_| on_close.call(()), "\u{2715}" }
            }
            crate::pull_request_edit::TitleLine { title, can_edit: t3_client::pull_request_edit::can_edit_change_request(&value), writable }
            p { class: "pr-branches muted", "{value[\"headBranch\"].as_str().unwrap_or_default()} \u{2192} {value[\"baseBranch\"].as_str().unwrap_or_default()}" }
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
        // The environment these checks come from advertises them (`pullRequestChecks`), as the real server does.
        let state = use_store(|| {
            let directory = tempfile::tempdir().unwrap();
            let mut model = UiModel::default();
            model.typed_config = Some(serde_json::from_value(crate::runtime::transport_tests::api_fixture(&directory, "checks-env").config.unwrap()).unwrap());
            model
        });
        let reference = Reference { project_id: "p".into(), host: None, repository: "acme/web".into(), number: 7 };
        let on_action = move |call: (String, Option<String>)| calls.borrow_mut().push(call);
        rsx! {
            crate::pull_request_summary::Actions { value: props.value.clone(), show_writes: props.show_writes, why: None, writable: props.writable, busy: props.busy.clone(), preferred: "merge".to_owned(), on_method: |_| {}, on_action }
            crate::pull_request_summary::ChecksLine { state, value: props.value.clone(), writable: props.writable, busy: props.busy.clone(), why: None, stale: false, on_action: |_| {} }
            crate::pull_request_summary::Summary { state, transport: TransportHandle::default(), reference, value: props.value.clone(), activity: Read::Loading, writable: props.show_writes && props.writable, on_changed: |_| {}, on_retry: |_| {} }
        }
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
        // Checks: the sentence beside the tabs, and the list as the host gave it with the original's word for each state.
        assert!(text.contains("1 of 2 failing") && text.contains("build") && text.contains("Passed") && text.contains("lint") && text.contains("Failed"), "{text}");
        assert!(!text.contains("Needs attention") && !text.contains("Pending"), "{text}");
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
    fn checks_in_flight_read_running_and_a_workflow_waiting_for_approval_has_its_button_beside_the_tabs() {
        let mut value = detail(&["approve-workflows"]);
        value["checks"] = json!([
            {"name":"queued job","status":"pending","description":null,"url":null},
            {"name":"started job","status":"pending","description":null,"url":null},
            {"name":"fork workflow","status":"action-required","description":null,"url":"https://github.com/acme/web/actions/runs/99"},
            {"name":"build","status":"success","description":null,"url":null}]);
        value["workflowApprovalsRequired"] = json!(1);
        let (mut dom, calls) = mount(value.clone(), true, true, None);
        let text = rendered_text(&dom);
        assert_eq!(text.matches("Running").count(), 2, "the host's queued and in-progress runs are one status on the wire: {text}");
        assert!(text.contains("Awaiting approval") && text.contains("Passed") && !text.contains("Pending") && !text.contains("pending"), "{text}");
        // Where workflows wait, the line beside the tabs is the button that lets them run.
        assert!(control(&dom, "Approve workflows to run").is_some() && control(&dom, "Checks*").is_none());
        click_control(&mut dom, "Approve workflows to run");
        assert_eq!(calls.borrow().len(), 0, "the harness wires the checks line to nothing, the panel asks first");
        // Without that, the line says how the checks stand, and its list puts what needs a person before what is done.
        value["workflowApprovalsRequired"] = json!(0);
        value["checks"][2]["status"] = json!("failure");
        let (mut dom, _) = mount(value, true, true, None);
        assert!(control(&dom, "Checks: 1 of 4 failing").is_some());
        click_control(&mut dom, "Checks: 1 of 4 failing");
        let text = rendered_text(&dom);
        let (failed, queued, passed) = (text.find("fork workflow").unwrap(), text.find("queued job").unwrap(), text.find("build").unwrap());
        assert!(failed < queued && queued < passed, "attention, then running, then done: {text}");
    }

    #[test]
    fn without_a_permission_the_button_is_absent_and_without_the_connection_every_write_is() {
        let (dom, _) = mount(detail(&["close"]), true, true, None);
        assert!(control(&dom, "Merge").is_none(), "an action the reader may not take is not offered");
        assert!(control(&dom, "Close pull request").is_some());
        let (dom, _) = mount(detail(&["merge", "close", "draft"]), false, false, None);
        for label in ["Merge", "Close pull request", "Convert to draft", "Update branch", "Approve workflows to run"] {
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
        for absent in ["Convert to draft", "Ready for review", "Reopen pull request", "Revert", "Update branch", "Enable auto-merge", "Approve workflows to run"] {
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
