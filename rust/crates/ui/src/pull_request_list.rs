//! The pull requests of the environment's projects, from their hosts: a page of rows that open the
//! thread filed under one, or start a thread from it. Port of the list of `PullRequestListPage`.
//!
//! The list is read when the connection can read it (the page may open before it does, from an address
//! or while offline) and when asked for; nothing polls. A page of the host's answer is a signal of
//! this page, so reading it repaints the page and nothing else. After a lost connection the rows stay,
//! marked old, and nothing can be started from them; the connection returning reads the list again.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use t3_client::pull_request as pr;
use t3_contracts::AuthEnvironmentScope;

#[derive(Debug, Clone, PartialEq, Default)]
struct Listed {
    entries: Vec<Value>,
    errors: Vec<String>,
    cursors: BTreeMap<String, String>,
    truncated: bool,
    /// Diff sizes by `repository#number`, read after the page.
    stats: BTreeMap<String, (u64, u64)>,
    loaded: bool,
    stale: bool,
}

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let mut filter = use_signal(|| "open".to_owned());
    let mut listed = use_signal(Listed::default);
    let mut reads = use_signal(|| 0u64);
    // Until the first read answers the page says it is reading; the read clears this when it ends.
    let mut pending = use_signal(|| true);
    let mut failure = use_signal(|| None::<String>);
    let mut more = use_signal(|| false);
    // A memo, not a copy of the flag: the read below restarts when the connection that can read changes,
    // and a closure that captured the flag at mount would keep the answer it had before connecting.
    let connection = use_memo(move || (connected_allows(state, AuthEnvironmentScope::OrchestrationRead), state.destination().read().clone()));
    let readable = connection.read().0;
    let writable = connected_allows(state, AuthEnvironmentScope::SourceControlWrite) && !listed.read().stale;

    // The first page of the chosen state: when the connection can read, when the state changes, when asked.
    // A newer read drops the one in flight, and a lost connection only marks the rows old.
    {
        let transport = transport.clone();
        use_resource(move || {
            let (transport, (readable, _), chosen, _) = (transport.clone(), connection(), filter(), reads());
            async move {
                if !readable {
                    pending.set(false);
                    if listed.peek().loaded && !listed.peek().stale {
                        listed.write().stale = true;
                    }
                    return;
                }
                let owner = runtime::response_owner(&transport, state);
                pending.set(true);
                failure.set(None);
                let answer = runtime::request_value_reported(transport.clone(), state, pr::LIST, pr::list_input(&chosen, &BTreeMap::new())).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                match answer {
                    Ok(value) => {
                        let entries = value["entries"].as_array().cloned().unwrap_or_default();
                        let stats_answer = if entries.is_empty() { None } else { runtime::request_value_reported(transport.clone(), state, pr::LIST_STATS, pr::stats_input(&entries)).await.ok() };
                        if owner != runtime::response_owner(&transport, state) {
                            return;
                        }
                        *listed.write() = page(&value, entries, stats_answer, Vec::new());
                    }
                    Err(message) => failure.set(Some(message)),
                }
                pending.set(false);
            }
        });
    }
    let load_more = {
        let transport = transport.clone();
        move |_| {
            if *more.peek() || *pending.peek() || !readable {
                return;
            }
            more.set(true);
            let (transport, cursors, chosen) = (transport.clone(), listed.peek().cursors.clone(), filter.peek().clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, pr::LIST, pr::list_input(&chosen, &cursors)).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) && *filter.peek() == chosen {
                    match answer {
                        Ok(value) => {
                            let entries = value["entries"].as_array().cloned().unwrap_or_default();
                            let stats_answer = if entries.is_empty() { None } else { runtime::request_value_reported(transport.clone(), state, pr::LIST_STATS, pr::stats_input(&entries)).await.ok() };
                            let before = listed.peek().clone();
                            let combined: Vec<Value> = before.entries.iter().cloned().chain(entries).collect();
                            let mut next = page(&value, combined, stats_answer, before.errors);
                            next.stats.extend(before.stats);
                            *listed.write() = next;
                        }
                        Err(message) => failure.set(Some(message)),
                    }
                }
                more.set(false);
            });
        }
    };
    let current = listed.read().clone();
    // The pull request panel opened from this page (a target with no thread), so its row can say so.
    let open_here = state.pr_panel().read().as_ref().filter(|target| target.thread_id.is_empty()).map(|target| target.reference.clone());
    // Leaving the page takes its panel with it: a target with no thread belongs to no conversation.
    use_drop(move || {
        if state.pr_panel().peek().as_ref().is_some_and(|target| target.thread_id.is_empty()) {
            state.pr_panel().set(None);
        }
    });
    let projects: BTreeMap<String, String> = state.shell().read().snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| (project.id.to_string(), project.workspace_root.to_string())).collect()).unwrap_or_default();
    rsx! { section { class: "settings-page pr-list-page",
        h1 { "Pull requests" }
        div { class: "provider-toolbar",
            label { r#for: "pr-list-state", class: "sr-only", "State" }
            select { id: "pr-list-state", "aria-label": "Pull request state", value: "{filter}", disabled: !readable, onchange: move |event| filter.set(event.value()),
                for (value, label) in [("open", "Open"), ("closed", "Closed"), ("merged", "Merged"), ("all", "All")] { option { key: "{value}", value, selected: *filter.read() == value, "{label}" } }
            }
            button { "aria-label": "Refresh pull requests", disabled: !readable || *pending.read(), onclick: move |_| { let next = *reads.peek() + 1; reads.set(next) }, if *pending.read() { "Loading pull requests\u{2026}" } else { "Refresh" } }
            button { onclick: move |_| state.view().set(View::Chat), "Back to chat" }
        }
        if !readable && !current.loaded { crate::empty_state::EmptyState { title: "Not connected", description: "Connect to an environment to list its pull requests." } }
        if current.stale { p { class: "diff-banner", role: "status", "Offline. This is the last list read." } }
        if let Some(message) = failure.read().clone().filter(|_| current.entries.is_empty()) {
            crate::empty_state::EmptyState { title: "Could not load pull requests", description: message,
                button { r#type: "button", "aria-label": "Retry loading pull requests", disabled: !readable || *pending.read(), onclick: move |_| { let next = *reads.peek() + 1; reads.set(next) }, "Retry" } }
        } else if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
        // Projects the host could not be asked about say why, one line each: no host for the remote,
        // not signed in, or the host's own refusal.
        if !current.errors.is_empty() {
            section { class: "pr-unavailable", "aria-label": "Projects without pull requests",
                h2 { "Not available for some projects" }
                ul { for error in current.errors.iter() { li { key: "{error}", "{error}" } } }
            }
        }
        if readable && !current.loaded && failure.read().is_none() { p { class: "muted", role: "status", "Loading pull requests\u{2026}" } }
        if current.loaded && current.entries.is_empty() && !*pending.read() && failure.read().is_none() {
            if projects.is_empty() {
                crate::empty_state::EmptyState { title: "No projects in this workspace", description: "Add a project, and the pull requests from its repository appear here.",
                    if let Some(add) = try_consume_context::<crate::AddProject>() { button { class: "primary", r#type: "button", onclick: move |_| { let mut open = add.0; open.set(true); }, "Add project" } } }
            } else {
                crate::empty_state::EmptyState {
                    title: if *filter.read() == "open" { "No pull requests" } else { "Nothing under this filter" },
                    description: if *filter.read() == "open" { "Pull requests from every project in this workspace appear here." } else { "Choose another state to see more." },
                    button { r#type: "button", "aria-label": "Check pull requests again", disabled: !readable || *pending.read(), onclick: move |_| { let next = *reads.peek() + 1; reads.set(next) }, "Check again" } }
            }
        }
        ol { class: "pr-list",
            for entry in current.entries.iter().cloned() {
                { let key = format!("{}#{}", entry["repository"].as_str().unwrap_or_default(), entry["number"]); let (additions, deletions) = current.stats.get(&key).copied().unwrap_or((entry["additions"].as_u64().unwrap_or(0), entry["deletions"].as_u64().unwrap_or(0)));
                  let project = entry["projectId"].as_str().unwrap_or_default().to_owned(); let url = entry["url"].as_str().unwrap_or_default().to_owned(); let cwd = projects.get(&project).cloned();
                  let reference = t3_client::pull_request::Reference { project_id: project.clone(), host: entry["host"].as_str().map(str::to_owned), repository: entry["repository"].as_str().unwrap_or_default().to_owned(), number: entry["number"].as_u64().unwrap_or(0) };
                  let selected = open_here.as_ref().is_some_and(|open| *open == reference);
                  rsx! { Row { key: "{entry[\"projectId\"]}:{key}", state, transport: transport.clone(), entry: entry.clone(), additions, deletions, can_start: writable && cwd.is_some(), selected,
                      on_open: { let reference = reference.clone(); move |_| crate::pull_request_panel::open(state, crate::pull_request_panel::Target { thread_id: String::new(), reference: reference.clone() }) },
                      on_start: { let (project, url) = (project.clone(), url.clone()); move |_| if let Some(cwd) = cwd.clone() { crate::pull_request_thread::open(state, &project, &cwd, &url) } } } } }
            }
        }
        if current.truncated && !current.cursors.is_empty() { button { "aria-label": "Load more pull requests", disabled: !readable || *more.read(), onclick: load_more, if *more.read() { "Loading\u{2026}" } else { "Load more" } } }
    } }
}

/// A page of the host's answer as the list holds it.
fn page(value: &Value, entries: Vec<Value>, stats: Option<Value>, mut errors: Vec<String>) -> Listed {
    errors.extend(value["errors"].as_array().into_iter().flatten().map(|error| format!("{}: {}", error["projectTitle"].as_str().unwrap_or_default(), error["message"].as_str().unwrap_or_default())));
    Listed {
        entries,
        errors,
        cursors: value["nextCursors"].as_object().map(|cursors| cursors.iter().filter_map(|(key, cursor)| Some((key.clone(), cursor.as_str()?.to_owned()))).collect()).unwrap_or_default(),
        truncated: value["truncated"] == true,
        stats: stats.and_then(|stats| stats["stats"].as_array().cloned()).unwrap_or_default().iter().filter_map(|stat| Some((format!("{}#{}", stat["repository"].as_str()?, stat["number"]), (stat["additions"].as_u64()?, stat["deletions"].as_u64()?)))).collect(),
        loaded: true,
        stale: false,
    }
}

/// One pull request: what it is, what it is filed under, and what can be started from it. The threads
/// filed under it are read when asked, not for every row.
#[component]
fn Row(state: Store<UiModel>, transport: TransportHandle, entry: Value, additions: u64, deletions: u64, can_start: bool, selected: bool, on_open: EventHandler<()>, on_start: EventHandler<()>) -> Element {
    let mut threads = use_signal(|| None::<Vec<Value>>);
    let mut asking = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let number = entry["number"].as_u64().unwrap_or(0);
    let ask = {
        let (transport, entry) = (transport.clone(), entry.clone());
        move |_| {
            if *asking.peek() {
                return;
            }
            asking.set(true);
            failure.set(None);
            let (transport, input) = (transport.clone(), json!({"projectId":entry["projectId"],"host":entry["host"],"repository":entry["repository"],"number":entry["number"]}));
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, pr::LINKED_THREADS, input).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match answer {
                        Ok(value) => threads.set(Some(value["threads"].as_array().cloned().unwrap_or_default())),
                        Err(message) => failure.set(Some(message)),
                    }
                }
                asking.set(false);
            });
        }
    };
    let author = entry["author"]["login"].as_str().unwrap_or_default().to_owned();
    let labels: Vec<String> = entry["labels"].as_array().into_iter().flatten().filter_map(|label| label["name"].as_str().map(str::to_owned)).collect();
    rsx! { li { class: "pr-list-row",
        div { class: "pr-list-main",
            // The title opens the pull request beside the list, as selecting a row does in the original.
            button { class: "pr-list-title", r#type: "button", "aria-label": "Open pull request #{number}", "aria-pressed": "{selected}", onclick: move |_| on_open.call(()), strong { { entry["title"].as_str().unwrap_or_default().to_owned() } } }
            span { class: "muted", " #{number} \u{b7} {pr::host_name(entry[\"provider\"].as_str())} {entry[\"repository\"].as_str().unwrap_or_default()} \u{b7} {entry[\"projectTitle\"].as_str().unwrap_or_default()}" }
            span { class: "pr-state", "data-state": "{pr::list_state_label(&entry)}", "{pr::list_state_label(&entry)}" }
            if !author.is_empty() { span { class: "muted", "{author}" } }
            if pr::has_size(additions, deletions) { span { class: "muted", "+{additions} \u{2212}{deletions}" } }
            // What the host summarises, and nothing it does not: GitHub alone reports a review decision, and a
            // host with no check rollup (or a change with no checks) has no checks state.
            if let Some(text) = pr::checks_state_label(entry["checksState"].as_str()) { span { class: "muted", "data-checks": "{entry[\"checksState\"].as_str().unwrap_or_default()}", "{text}" } }
            if let Some(text) = pr::review_decision_label(entry["reviewDecision"].as_str()) { span { class: "muted", "{text}" } }
            for label in labels { span { key: "{label}", class: "pr-label", "{label}" } }
        }
        div { class: "pr-list-actions",
            button { "aria-label": "Linked threads of pull request #{number}", disabled: *asking.read(), onclick: ask, if *asking.read() { "Looking\u{2026}" } else { "Linked threads" } }
            button { "aria-label": "Start a thread from pull request #{number}", disabled: !can_start, onclick: move |_| on_start.call(()), "Start thread" }
        }
        if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
        if let Some(found) = threads.read().clone() {
            if found.is_empty() { p { class: "muted", "No thread is filed under this pull request." } }
            ul { for thread in found { { let id = thread["id"].as_str().unwrap_or_default().to_owned(); let title = thread["title"].as_str().unwrap_or("Untitled thread").to_owned(); let transport = transport.clone();
                rsx! { li { key: "{id}", button { "aria-label": "Open thread {id}", onclick: move |_| runtime::select_thread(&transport, state, id.clone()), "{title}" } } } } } }
        }
    } }
}
