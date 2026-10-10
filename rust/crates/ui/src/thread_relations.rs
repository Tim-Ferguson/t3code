//! Forking a conversation, merging a fork back, and the lineage between threads: the original's
//! `AssistantForkButton`, `onForkFromRun`, `ThreadRelationshipsPanel` and the `fork` and `handoff`
//! rows of `V2LifecycleRow`. The original asks no confirmation for either write, and neither does
//! this: a fork is a new thread beside the old one, and a merge only queues the fork's work for
//! the parent's next message. Each write is sent once per click and the button waits for the
//! server's answer.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::thread_relations as relations;
use t3_contracts::AuthEnvironmentScope;

/// The fork or merge in flight, if any: one at a time, so a second click cannot send a second write.
#[derive(Debug, Clone, PartialEq)]
pub struct Busy {
    pub destination: String,
    pub thread: String,
}
/// A fork the server accepted, waiting for its thread to reach this client before it opens.
#[derive(Debug, Clone, PartialEq)]
pub struct Opening {
    pub destination: String,
    pub from_thread: String,
    pub target: String,
}
#[derive(Debug, Clone, Default)]
pub struct Model {
    pub fork: Option<Busy>,
    pub merge: Option<Busy>,
    pub opening: Option<Opening>,
    /// The queued run being sent into the running turn, so one click sends one steer.
    pub steering: Option<String>,
}

/// How long an accepted fork may take to reach this client before the reader is told.
const REACH_MS: u64 = 5_000;

/// The answer to a command, or the words to show in its place.
fn failure(message: String, fallback: &str) -> String {
    if message.trim().is_empty() { fallback.to_owned() } else { message }
}

fn destination_of(state: Store<UiModel>) -> Option<String> {
    state.peek().destination.as_ref().map(ToString::to_string)
}

/// Forks the open thread at `run_id` of `source_thread` (an inherited answer's own thread).
pub(crate) fn fork(transport: &TransportHandle, state: Store<UiModel>, source_thread: String, run_id: String) {
    let Some(destination) = destination_of(state) else { return };
    let (open, title) = {
        let model = state.peek();
        let Some(open) = model.active_thread.clone() else { return };
        let title = model.shell.snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == open)).map(|thread| thread.title.clone());
        let Some(title) = title else { return };
        (open, title)
    };
    if state.relations().peek().fork.is_some() {
        return;
    }
    let target = uuid::Uuid::new_v4().to_string();
    let mut payload = relations::fork_command(&source_thread, &target, &run_id, &relations::fork_title(&title), runtime::creation_source());
    payload["commandId"] = json!(uuid::Uuid::new_v4().to_string());
    let owner = runtime::response_owner(transport, state);
    state.relations().write().fork = Some(Busy { destination: destination.clone(), thread: open.clone() });
    let transport = transport.clone();
    // The write outlives the answer's button: leaving the thread must not strand its busy state.
    dioxus::dioxus_core::spawn_forever(async move {
        let result = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", payload).await;
        state.relations().write().fork = None;
        let current = runtime::response_owner(&transport, state);
        if owner.is_none() || owner != current {
            return;
        }
        match result {
            Err(message) => notice::push(state, Level::Error, "Could not fork", failure(message, "Failed to fork this response.")),
            Ok(_) => {
                state.relations().write().opening = Some(Opening { destination: destination.clone(), from_thread: open, target: target.clone() });
                runtime::delay(REACH_MS).await;
                let waiting = state.relations().peek().opening.as_ref().is_some_and(|opening| opening.target == target);
                if waiting {
                    state.relations().write().opening = None;
                    notice::push(state, Level::Warning, "Fork created", "The fork was created, but its thread data did not reach this client. Reconnect and try opening it from the sidebar.");
                }
            }
        }
    });
}

/// Merges the open fork's `run_id` back into `target`, the thread it came from.
pub(crate) fn merge_back(transport: &TransportHandle, state: Store<UiModel>, source_thread: String, target: String, run_id: String) {
    let Some(destination) = destination_of(state) else { return };
    if state.relations().peek().merge.is_some() {
        return;
    }
    let mut payload = relations::merge_back_command(&source_thread, &target, &run_id, runtime::creation_source());
    payload["commandId"] = json!(uuid::Uuid::new_v4().to_string());
    let owner = runtime::response_owner(transport, state);
    state.relations().write().merge = Some(Busy { destination, thread: source_thread.clone() });
    let transport = transport.clone();
    dioxus::dioxus_core::spawn_forever(async move {
        let result = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", payload).await;
        state.relations().write().merge = None;
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        match result {
            Err(message) => notice::push(state, Level::Error, "Could not merge back", failure(message, "Failed to merge this conversation back.")),
            Ok(_) => runtime::select_thread(&transport, state, target),
        }
    });
}

/// Opens an accepted fork once its thread is known here, if the reader is still where they forked.
#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle) -> Element {
    use_effect(move || {
        let Some(opening) = state.relations().read().opening.clone() else { return };
        let here = state.active_thread().peek().clone().as_deref() == Some(opening.from_thread.as_str()) && destination_of(state).as_deref() == Some(opening.destination.as_str());
        if !here {
            state.relations().write().opening = None;
            return;
        }
        let known = state.shell().read().snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|thread| thread.id.as_str() == opening.target));
        if known {
            state.relations().write().opening = None;
            runtime::select_thread(&transport, state, opening.target);
        }
    });
    rsx! {}
}

/// The fork button of a settled answer. Enabled while connected with the operate grant and no
/// fork is in flight.
#[component]
pub(crate) fn ForkButton(state: Store<UiModel>, transport: TransportHandle, source_thread: String, run_id: String) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<crate::RenderProbe>() {
        probe.fork_button.set(probe.fork_button.get() + 1);
    }
    let operable = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationOperate));
    let busy = use_memo(move || state.relations().read().fork.is_some());
    rsx! {
        button { class: "icon-button fork-button", r#type: "button", "aria-label": "Fork from this response", title: "Fork from this response", "aria-busy": "{busy()}", disabled: !operable() || busy(),
            onclick: move |_| fork(&transport, state, source_thread.clone(), run_id.clone()),
            "\u{2442}" }
    }
}

// ---- the lineage panel --------------------------------------------------------------------------

/// A page of the lineage list: a busy thread can accumulate dozens of forks and agents.
const FIRST_ROWS: usize = 6;
const MORE_ROWS: usize = 12;

/// What a child thread's shell says it is doing, for stopping the subagent that runs it.
fn child_activity(state: Store<UiModel>, child: &str) -> Option<relations::ChildActivity> {
    let shell = state.shell();
    let shell = shell.read();
    let thread = shell.snapshot.as_ref()?.threads.iter().find(|thread| thread.id.as_str() == child)?;
    let live = thread.activity_run_status.flatten().map(|status| (serde_json::to_value(status).ok().and_then(|status| status.as_str().map(str::to_owned)).unwrap_or_default(), thread.activity_run_started_at.flatten().is_some()));
    Some(relations::ChildActivity { live, run: thread.active_run_id.as_ref().or(thread.latest_run_id.as_ref()).map(ToString::to_string) })
}

/// Interrupts a subagent's run, holding its queue (`run.interrupt`, as the original's stop does). One
/// at a time; the button waits for the answer and a refusal is told.
fn stop_subagent(transport: &TransportHandle, state: Store<UiModel>, mut stopping: Signal<Option<String>>, child: String, run: String) {
    if stopping.peek().is_some() {
        return;
    }
    stopping.set(Some(child.clone()));
    let owner = runtime::response_owner(transport, state);
    let transport = transport.clone();
    dioxus::dioxus_core::spawn_forever(async move {
        let payload = json!({"type":"run.interrupt","commandId":uuid::Uuid::new_v4().to_string(),"threadId":child,"runId":run,"holdQueue":true});
        let result = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", payload).await;
        stopping.set(None);
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        if let Err(message) = result {
            notice::push(state, Level::Error, "Could not stop subagent", message);
        }
    });
}

fn shells(state: Store<UiModel>) -> Vec<relations::Shell> {
    state.shell().read().snapshot.as_ref().map(|snapshot| snapshot.threads.iter().filter(|thread| thread.deleted_at.is_none()).map(relations::Shell::from).collect()).unwrap_or_default()
}

/// Whether the open thread has anything related, read narrowly so streaming does not repaint it.
fn has_related(state: Store<UiModel>, thread_id: &str) -> bool {
    let shell = state.shell();
    let snapshot = shell.read();
    let Some(snapshot) = snapshot.snapshot.as_ref() else { return false };
    let own_parent = snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id).is_some_and(|thread| thread.lineage.parent_thread_id.is_some() || matches!(thread.forked_from, Some(t3_contracts::ThreadForkSource::Run { .. })));
    if own_parent || snapshot.threads.iter().any(|thread| thread.deleted_at.is_none() && (thread.lineage.parent_thread_id.as_ref().is_some_and(|parent| parent.as_str() == thread_id) || matches!(&thread.forked_from, Some(t3_contracts::ThreadForkSource::Run { thread_id: from, .. }) if from.as_str() == thread_id))) {
        return true;
    }
    let thread = state.thread();
    let domain = thread.read();
    domain.projection.as_ref().filter(|projection| projection["thread"]["id"] == thread_id).is_some_and(|projection| projection["subagents"].as_array().is_some_and(|list| list.iter().any(|agent| !agent["childThreadId"].is_null() || agent["status"] == "running")) || projection["contextTransfers"].as_array().is_some_and(|list| list.iter().any(|transfer| transfer["sourceThreadId"] != transfer["targetThreadId"])))
}

/// The header button that opens the lineage panel, present only for a thread that has relatives.
#[component]
pub(crate) fn HeaderButton(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<crate::RenderProbe>() {
        probe.lineage_button.set(probe.lineage_button.get() + 1);
    }
    // The panel belongs to the thread it was opened for: moving to another thread closes it.
    let mut opened = use_signal(|| None::<String>);
    let related = use_memo(use_reactive((&thread_id,), move |(thread_id,)| has_related(state, &thread_id)));
    let open = opened.read().as_deref() == Some(thread_id.as_str());
    if !related() {
        return rsx! {};
    }
    rsx! {
        div { class: "lineage-anchor", onkeydown: move |event| { if event.key() == Key::Escape && open { event.stop_propagation(); opened.set(None); } },
            button { class: "icon-button", r#type: "button", "aria-label": "Thread lineage", title: "Forks and related threads", "aria-haspopup": "dialog", "aria-expanded": "{open}",
                onclick: { let thread_id = thread_id.clone(); move |_| opened.set(if open { None } else { Some(thread_id.clone()) }) },
                "\u{2442} Lineage" }
            if open { Panel { key: "{thread_id}", state, transport, thread_id: thread_id.clone(), close: move |_| opened.set(None) } }
        }
    }
}

#[component]
fn Panel(state: Store<UiModel>, transport: TransportHandle, thread_id: String, close: EventHandler<()>) -> Element {
    let mut shown = use_signal(|| FIRST_ROWS);
    let mut previous_open = use_signal(|| false);
    let stopping = use_signal(|| None::<String>);
    let all = shells(state);
    let projection = { let thread = state.thread(); let domain = thread.read(); domain.projection.clone().filter(|projection| projection["thread"]["id"] == thread_id.as_str()) };
    let graph = relations::graph(&all, projection.as_ref());
    let merge_target = projection.as_ref().and_then(|projection| relations::merge_back_target(&projection["thread"]));
    let ordered = relations::order_rows(&graph, graph.immediate(&thread_id), &thread_id, merge_target.as_deref());
    let latest = projection.as_ref().and_then(relations::latest_merge_back_run).and_then(|run| run["id"].as_str().map(str::to_owned));
    let operable = connected_allows(state, AuthEnvironmentScope::OrchestrationOperate);
    let merging = state.relations().read().merge.is_some();
    let parent_title = merge_target.as_deref().and_then(|id| graph.shell(id)).map(|shell| shell.title.clone());
    // Subagents are listed apart: the running ones with the rest, the finished ones behind a heading.
    let finished = |status: Option<&str>| matches!(status, Some("completed" | "failed" | "error" | "cancelled" | "interrupted" | "idle"));
    let (mut related, mut active, mut done) = (vec![], vec![], vec![]);
    for row in ordered {
        if row.edge.kind != relations::Kind::Subagent || relations::is_parent(&row.edge, &thread_id) {
            related.push(row);
        } else if finished(row.edge.status.as_deref()) {
            done.push(row);
        } else {
            active.push(row);
        }
    }
    let running = active.iter().filter(|row| row.edge.status.as_deref() == Some("running")).count();
    let heading = if running > 0 { format!("Lineage \u{b7} {running} running") } else { "Lineage".to_owned() };
    let visible: Vec<relations::Row> = related.into_iter().chain(active).collect();
    let hidden = visible.len().saturating_sub(shown());
    let line = |row: &relations::Row| {
        let status = graph.row_status(row);
        let missing = graph.shell(&row.thread).is_none();
        let title = graph.shell(&row.thread).map_or_else(|| row.thread.clone(), |shell| shell.title.clone());
        let label = relations::relationship_label(&row.edge, &thread_id);
        let word = relations::status_label(status.as_deref());
        let parent = relations::is_parent(&row.edge, &thread_id);
        let glyph = if parent { "\u{21b0}" } else if row.edge.kind == relations::Kind::Subagent { "\u{25c8}" } else if row.edge.kind == relations::Kind::Transfer { "\u{21c4}" } else { "\u{2442}" };
        let is_merge_target = Some(row.thread.as_str()) == merge_target.as_deref();
        (row.thread.clone(), title, label, word, glyph, missing, is_merge_target)
    };
    let render = |row: &relations::Row| {
        let (id, title, label, word, glyph, missing, is_merge_target) = line(row);
        let hint = if missing { "This related thread is unavailable".to_owned() } else { format!("Open {} in this chat", label.to_lowercase()) };
        let aria = format!("{title} {word}");
        let open_id = id.clone();
        let (merge_title, merge_label) = (relations::merge_back_tooltip(latest.is_some(), parent_title.as_deref()), relations::merge_back_label(parent_title.as_deref()));
        let (merge_source, merge_run, merge_target_id) = (thread_id.clone(), latest.clone(), id.clone());
        // A subagent this app runs, still going: the button that stops it.
        let stop_run = (row.edge.kind == relations::Kind::Subagent && !relations::is_parent(&row.edge, &thread_id))
            .then(|| projection.as_ref().and_then(|projection| projection["subagents"].as_array()).and_then(|agents| agents.iter().find(|agent| agent["childThreadId"] == id.as_str())))
            .flatten()
            .and_then(|agent| relations::subagent_stop_run(agent, child_activity(state, &id).as_ref()));
        let (stop_child, transport_stop) = (id.clone(), transport.clone());
        let stop_label = format!("Stop subagent {title}");
        let waiting = stopping.read().is_some();
        let stopping_this = stopping.read().as_deref() == Some(id.as_str());
        let transport_open = transport.clone();
        let transport_merge = transport.clone();
        rsx! {
            li { key: "{id}", class: "lineage-row", "data-relationship": "{label}",
                button { class: "lineage-open", r#type: "button", "aria-label": "{aria}", title: "{hint}", disabled: missing,
                    onclick: move |_| { close.call(()); runtime::select_thread(&transport_open, state, open_id.clone()); },
                    span { class: "lineage-glyph", "aria-hidden": "true", "{glyph}" }
                    span { class: "lineage-title", "{title}" }
                    span { class: "muted lineage-status", "{word}" } }
                if let Some(run) = stop_run {
                    button { class: "icon-button lineage-stop", r#type: "button", "aria-label": "{stop_label}", title: "Stop subagent", "aria-busy": "{stopping_this}", disabled: waiting || !operable,
                        onclick: move |_| stop_subagent(&transport_stop, state, stopping, stop_child.clone(), run.clone()),
                        if stopping_this { "Stopping\u{2026}" } else { "\u{25a0}" } }
                }
                if is_merge_target {
                    button { class: "icon-button lineage-merge", r#type: "button", "aria-label": "{merge_label}", title: "{merge_title}", "aria-busy": "{merging}", disabled: latest.is_none() || merging || !operable,
                        onclick: move |_| { if let Some(run) = merge_run.clone() { merge_back(&transport_merge, state, merge_source.clone(), merge_target_id.clone(), run); } },
                        "\u{21e1} Merge back" }
                }
            }
        }
    };
    rsx! {
        div { class: "lineage-panel", role: "dialog", "aria-label": "Thread lineage",
            h2 { "{heading}" }
            ul { class: "lineage-list", "aria-label": "Related threads",
                for row in visible.iter().take(shown()) { {render(row)} }
            }
            if hidden > 0 { button { r#type: "button", class: "work-more", onclick: move |_| shown.set(shown() + MORE_ROWS), "Show {hidden.min(MORE_ROWS)} more" } }
            if !done.is_empty() {
                button { r#type: "button", class: "lineage-previous", "aria-expanded": "{previous_open()}", onclick: move |_| previous_open.set(!previous_open()),
                    if previous_open() { "\u{25be} Previous agents" } else { "\u{25b8} Previous agents ({done.len()})" } }
                if previous_open() { ul { class: "lineage-list", "aria-label": "Previous agents", for row in done.iter() { {render(row)} } } }
            }
        }
    }
}

// ---- timeline rows ------------------------------------------------------------------------------

/// "Forked from conversation", with the way back to the source (or on to the fork).
#[component]
pub(crate) fn ForkRow(item: Value, state: Store<UiModel>, transport: TransportHandle, visibility: String) -> Element {
    let (label, action, related) = relations::fork_row(&item);
    rsx! { div { class: "system-divider fork-divider", role: "separator", "aria-label": "{label}", "data-item-type": "fork", "data-visibility": "{visibility}",
        span { class: "divider-line" }
        span { class: "divider-label", "\u{2442} {label}" }
        button { class: "link-button", r#type: "button", "aria-label": "{action}", onclick: move |_| runtime::select_thread(&transport, state, related.clone()), "{action}" }
        span { class: "divider-line" }
    } }
}

/// The name of one side of a handoff: the model, else the provider, as the original labels it.
fn endpoint(state: Store<UiModel>, endpoint: &relations::Endpoint) -> (String, String) {
    let field = state.typed_config();
    let config = field.read();
    let provider = config.as_ref().and_then(|config| config.providers.0.iter().find(|provider| provider.instance_id.as_str() == endpoint.instance_id));
    let name = provider.map_or_else(|| endpoint.instance_id.clone(), |provider| provider.display_name.as_ref().and_then(Option::as_ref).map_or_else(|| endpoint.instance_id.clone(), ToString::to_string));
    let model = endpoint.model.as_deref().map(str::trim).filter(|model| !model.is_empty());
    let label = model.map(|model| {
        let known = provider.and_then(|provider| provider.models.iter().find(|candidate| candidate.slug.as_str() == model));
        known.map_or_else(|| model.to_owned(), |known| {
            let option = t3_client::models::ModelOption::from(known);
            relations::model_label(&option)
        })
    });
    let label = label.unwrap_or_else(|| name.clone());
    (label.clone(), format!("{name} \u{b7} {label}"))
}

/// "Context handoff" between the providers a conversation moved across.
#[component]
pub(crate) fn HandoffRow(item: Value, runs: Vec<Value>, state: Store<UiModel>, visibility: String) -> Element {
    let (from, to) = relations::handoff_endpoints(&item, &runs);
    let failed = item["status"] == "failed";
    let from: Vec<(String, String)> = from.iter().map(|side| endpoint(state, side)).collect();
    let to = endpoint(state, &to);
    let title = item["title"].as_str().map(str::trim).filter(|title| !title.is_empty()).map(str::to_owned);
    let summary = item["summary"].as_str().map(str::to_owned);
    rsx! { div { class: if failed { "system-divider handoff-divider danger" } else { "system-divider handoff-divider" }, role: "separator", "aria-label": "Context handoff", "data-item-type": "handoff", "data-visibility": "{visibility}",
        span { class: "divider-line" }
        span { class: "divider-label", "\u{21c4} Context handoff" }
        span { class: "handoff-endpoints",
            for (index, (label, tooltip)) in from.iter().enumerate() { span { key: "{index}", class: "handoff-endpoint", tabindex: "0", title: "{tooltip}", "{label}" } if index + 1 < from.len() { span { "aria-hidden": "true", "," } } }
            if !from.is_empty() { span { "aria-hidden": "true", "\u{2192}" } }
            span { class: "handoff-endpoint", tabindex: "0", title: "{to.1}", "{to.0}" } }
        span { class: "divider-line" }
        if let Some(title) = title { p { class: "divider-detail muted", "{title}" } }
        if let Some(summary) = summary { details { class: "handoff-summary", summary { "What the provider was told" } pre { "{summary}" } } }
    } }
}

/// A thread an agent created for the work, summarised under the turn that made it: its title and a
/// way in (`V2LifecycleRow`'s resource summary).
#[component]
pub(crate) fn CreatedThreadCard(item: Value, state: Store<UiModel>, transport: TransportHandle) -> Element {
    let title = item["title"].as_str().map(str::trim).filter(|title| !title.is_empty()).unwrap_or("Created thread").to_owned();
    let target = item["targetThreadId"].as_str().unwrap_or_default().to_owned();
    let label = format!("Open {}", if title == "Created thread" { "created thread" } else { title.as_str() });
    rsx! { div { class: "resource-card thread-created-card", "data-item-type": "thread_created",
        span { class: "resource-card-icon", "aria-hidden": "true", "\u{1f4ac}" }
        span { class: "resource-card-title", "{title}" }
        button { r#type: "button", "aria-label": "{label}", disabled: target.is_empty(), onclick: move |_| runtime::select_thread(&transport, state, target.clone()), "Open chat" }
    } }
}
