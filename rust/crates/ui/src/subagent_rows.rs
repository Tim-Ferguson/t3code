//! A subagent in its parent's conversation: one row per child, or one collapsible group for the
//! children a provider turn launched together. Port of the subagent row of `V2LifecycleRow.tsx`
//! and `V2SubagentGroup` (`MessagesTimeline.tsx`). A row takes plain props, so a child's progress
//! repaints that row alone; only a working child counts seconds, in a component of its own.
use crate::runtime::{self, UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use serde_json::Value;
use t3_client::subagent_display as display;

/// What the hover card says beyond the row.
#[derive(Clone, PartialEq, Default)]
pub(crate) struct Tip {
    pub title: String,
    pub model: String,
    pub workspace: Vec<(String, String)>,
    pub preview: Option<String>,
}
#[derive(Clone, PartialEq)]
pub(crate) struct Card {
    pub id: String,
    pub title: String,
    pub driver: String,
    pub status: String,
    pub result: Option<String>,
    pub progress: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub child: Option<String>,
    pub tip: Tip,
}

fn driver_mark(driver: &str) -> &'static str {
    match driver {
        "codex" => "\u{25ce}",
        "claudeAgent" => "\u{2733}",
        "cursor" => "\u{25b7}",
        "opencode" => "\u{25c7}",
        _ => "\u{25c8}",
    }
}
fn dot(status: &str) -> &'static str {
    match status {
        "pending" | "running" | "waiting" => "info",
        "completed" => "success",
        "failed" => "failed",
        _ => "idle",
    }
}

/// The cards for these subagent items of the open conversation: the live record of each child
/// (`subagents` of the projection) wins over what the item said when it was written.
pub(crate) fn cards(state: Store<UiModel>, projection: &Value, items: &[&Value]) -> Vec<Card> {
    let agents = projection["subagents"].as_array();
    let shell = state.shell();
    let shell = shell.peek();
    let typed = state.typed_config();
    let typed = typed.peek();
    let parent_id = projection["thread"]["id"].as_str().unwrap_or_default();
    let snapshot = shell.snapshot.as_ref();
    let parent = snapshot.and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == parent_id));
    items
        .iter()
        .map(|item| {
            let live = agents.and_then(|agents| agents.iter().find(|agent| agent["id"] == item["subagentId"]));
            let pick = |field: &str| live.and_then(|agent| agent[field].as_str()).or_else(|| item[field].as_str());
            let status = live.and_then(|agent| agent["status"].as_str()).or_else(|| item["status"].as_str()).unwrap_or("idle").to_owned();
            // The row is named, and opens, as the item says; the live record gives the state.
            let title = display::display_title(item["title"].as_str().unwrap_or("Subagent"));
            let child = item["childThreadId"].as_str().map(str::to_owned);
            let (result, progress) = (pick("result").map(str::to_owned), pick("progress").map(str::to_owned));
            let instance = item["providerInstanceId"].as_str().unwrap_or_default();
            let catalog: Vec<Value> = typed.as_ref().and_then(|config| config.providers.0.iter().find(|provider| provider.instance_id.as_str() == instance)).and_then(|provider| serde_json::to_value(&provider.models).ok()).and_then(|models| models.as_array().cloned()).unwrap_or_default();
            // The child's own workspace is shown only where it differs from the parent's.
            let child_thread = child.as_deref().and_then(|child| snapshot.and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == child)));
            let mut workspace = vec![];
            if let (Some(parent), Some(child_thread), Some(snapshot)) = (parent, child_thread, snapshot) {
                if child_thread.project_id != parent.project_id {
                    if let Some(project) = snapshot.projects.iter().find(|project| project.id == child_thread.project_id) {
                        workspace.push(("Project".to_owned(), project.title.to_string()));
                    }
                }
                let root = |thread: &t3_contracts::ThreadShell| thread.worktree_path.as_ref().map(|path| path.to_string()).or_else(|| snapshot.projects.iter().find(|project| project.id == thread.project_id).map(|project| project.workspace_root.to_string()));
                if let (Some(a), Some(b)) = (root(parent), root(child_thread)) {
                    if a != b {
                        let label = if child_thread.branch.is_some() { "Branch" } else if child_thread.worktree_path.is_some() { "Worktree" } else { "Workspace" };
                        let value = child_thread.branch.as_ref().map(|branch| branch.to_string()).unwrap_or_else(|| b.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(&b).to_owned());
                        workspace.push((label.to_owned(), value));
                    }
                }
            }
            let tip_title = child_thread.map(|thread| display::display_title(thread.title.as_str())).unwrap_or_else(|| title.clone());
            Card {
                id: item["id"].as_str().unwrap_or_default().to_owned(),
                title,
                driver: item["driver"].as_str().unwrap_or_default().to_owned(),
                tip: Tip { title: tip_title, model: display::model_label(pick("model"), &catalog), workspace, preview: display::detail_preview(&status, result.as_deref(), progress.as_deref()) },
                status,
                result,
                progress,
                started_at: live.and_then(|agent| agent["startedAt"].as_str()).or_else(|| item["startedAt"].as_str()).map(str::to_owned),
                completed_at: live.and_then(|agent| agent["completedAt"].as_str()).or_else(|| item["completedAt"].as_str()).map(str::to_owned),
                child,
            }
        })
        .collect()
}

#[component]
fn LiveElapsed(started_at: String) -> Element {
    let mut now = use_signal(|| chrono::Utc::now().timestamp_millis());
    use_future(move || async move {
        loop {
            runtime::delay(1000).await;
            now.set(chrono::Utc::now().timestamp_millis());
        }
    });
    let text = display::elapsed_ms("running", Some(&started_at), None, now()).map(|ms| display::format_elapsed(ms as f64 / 1000.0)).unwrap_or_default();
    rsx! {"{text}"}
}
/// Time worked: counting while the child works, frozen at its end afterwards.
#[component]
fn Elapsed(status: String, started_at: Option<String>, completed_at: Option<String>) -> Element {
    if display::is_active(&status) {
        return match started_at {
            Some(started_at) => rsx! {LiveElapsed {started_at}},
            None => rsx! {},
        };
    }
    let text = display::elapsed_ms(&status, started_at.as_deref(), completed_at.as_deref(), 0).map(|ms| display::format_elapsed(ms as f64 / 1000.0)).unwrap_or_default();
    rsx! {"{text}"}
}

#[component]
fn Avatar(driver: String, status: Option<String>) -> Element {
    rsx! {span {class:"subagent-avatar","aria-hidden":"true","{driver_mark(&driver)}"
        if let Some(status) = status {span {class:"subagent-dot {dot(&status)}"}}
    }}
}

#[component]
pub(crate) fn SubagentRow(card: Card, state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<crate::RenderProbe>() {
        probe.item.set(probe.item.get() + 1);
    }
    let label = display::status_label(&card.status);
    let detail = display::row_detail(&card.status, card.result.as_deref(), card.progress.as_deref());
    let failed = card.status == "failed";
    let settled_completed = card.status == "completed";
    let show_state = detail.is_some() && !settled_completed;
    let content = rsx! {
        Avatar {driver:card.driver.clone(),status:Some(card.status.clone())}
        span {class:"subagent-text",
            span {class:"subagent-head",
                span {class:"subagent-title","{card.title}"}
                if show_state {span {class:if failed {"subagent-state failed"} else {"subagent-state"},"{label}"}}
            }
            span {class:if failed {"subagent-detail failed"} else {"subagent-detail"},
                match &detail {Some(detail) => rsx!{span {class:"subagent-line","{detail}"}}, None => rsx!{"{label}"}}
            }
        }
        span {class:"subagent-elapsed",Elapsed {status:card.status.clone(),started_at:card.started_at.clone(),completed_at:card.completed_at.clone()}}
        if card.child.is_some() {span {class:"fold-chevron","aria-hidden":"true","\u{203a}"}}
    };
    let tip = rsx! {
        span {class:"subagent-tip",role:"tooltip",
            strong {"{card.tip.title}"}
            span {"{card.tip.model}"}
            span {class:"subagent-tip-status","{card.status.replace('_', \" \")}"}
            for (name, value) in card.tip.workspace.iter() {span {key:"{name}",span {class:"sr-only","{name}"}"{value}"}}
            if let Some(preview) = &card.tip.preview {span {class:"subagent-tip-preview","{preview}"}}
        }
    };
    match card.child.clone() {
        Some(child) => rsx! {div {class:"subagent-card","data-item-type":"subagent",
            button {class:"subagent-row",r#type:"button","aria-label":"Open {card.title}","aria-description":"{label}",onclick:move |_| runtime::select_thread(&transport, state, child.clone()),{content}}
            {tip}
        }},
        None => rsx! {div {class:"subagent-card","data-item-type":"subagent",
            div {class:"subagent-row","aria-description":"{label}",{content}}
            {tip}
        }},
    }
}

/// Children launched together by one provider turn: a header that counts them, and the rows when opened.
#[component]
pub(crate) fn SubagentGroup(id: String, cards: Vec<Card>, state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    let mut open = use_signal(|| false);
    let statuses: Vec<&str> = cards.iter().map(|card| card.status.as_str()).collect();
    let (_, active, failed) = display::group_summary(&statuses);
    let label = format!("{} {}", cards.len(), if cards.len() == 1 { "subagent" } else { "subagents" });
    let summary = display::status_summary(&statuses);
    // The group's span: from the first start to the last end, counting while any member works.
    let started = cards.iter().filter_map(|card| card.started_at.as_deref()).min().map(str::to_owned);
    let all_ended = cards.iter().all(|card| card.completed_at.is_some());
    let ended = if active || !all_ended { None } else { cards.iter().filter_map(|card| card.completed_at.as_deref()).max().map(str::to_owned) };
    let expanded = open();
    rsx! {div {class:"subagent-group","data-subagent-group":"true",
        button {class:if expanded || active {"subagent-group-head active"} else {"subagent-group-head"},r#type:"button","aria-label":"{label}","aria-description":"{summary}","aria-expanded":"{expanded}",onclick:move |_| open.set(!expanded),
            span {class:"subagent-avatars","aria-hidden":"true",
                for card in cards.iter().take(3) {Avatar {key:"{card.id}",driver:card.driver.clone(),status:if cards.len() == 1 {Some(card.status.clone())} else {None}}}
                if cards.len() > 3 {span {class:"subagent-avatar more","+{cards.len() - 3}"}}
            }
            span {class:"subagent-text",
                span {class:"subagent-title","{label}"}
                span {class:if active {"subagent-detail active"} else if failed {"subagent-detail failed"} else {"subagent-detail"},"{summary}"}
            }
            span {class:"subagent-elapsed",Elapsed {status:if active {"running".to_owned()} else {"completed".to_owned()},started_at:started,completed_at:ended}}
            span {class:"fold-chevron","aria-hidden":"true",if expanded {"\u{25be}"} else {"\u{25b8}"}}
        }
        if expanded {div {class:"subagent-members",
            for card in cards {SubagentRow {key:"{card.id}",card:card.clone(),state,transport:transport.clone()}}
        }}
    }}
}
