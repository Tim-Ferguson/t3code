//! Forks, merge-back and the lineage of a thread: the original's `threadRelationships.ts`,
//! `handoff.ts` and the fork/merge-back parts of `threadWorkflows.ts`, over the same shapes.
//! Replayed against the original by `tests/thread_relations_parity.rs`.
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::collections::HashSet;

/// What the relationship graph reads of a thread shell.
#[derive(Debug, Clone, PartialEq)]
pub struct Shell {
    pub id: String,
    pub title: String,
    /// The thread it was forked from (the fork's run's thread when it has one), or its owning agent.
    pub parent: Option<String>,
    pub subagent: bool,
    pub activity_status: Option<String>,
    pub status: String,
    pub created_ms: Option<i64>,
    pub project_id: String,
    pub provider_instance_id: String,
}
impl From<&t3_contracts::ThreadShell> for Shell {
    fn from(thread: &t3_contracts::ThreadShell) -> Self {
        let text = |value: Value| value.as_str().unwrap_or_default().to_owned();
        let forked = match &thread.forked_from {
            Some(t3_contracts::ThreadForkSource::Run { thread_id, .. }) => Some(thread_id.to_string()),
            _ => None,
        };
        Self {
            id: thread.id.to_string(),
            title: thread.title.clone(),
            parent: forked.or_else(|| thread.lineage.parent_thread_id.as_ref().map(ToString::to_string)),
            subagent: thread.lineage.relationship_to_parent == Some(t3_contracts::ThreadRelationship::Subagent),
            activity_status: thread.activity_run_status.flatten().and_then(|status| serde_json::to_value(status).ok()).map(text),
            status: serde_json::to_value(thread.status).map(text).unwrap_or_default(),
            created_ms: Some(thread.created_at.timestamp_millis()),
            project_id: thread.project_id.to_string(),
            provider_instance_id: thread.provider_instance_id.to_string(),
        }
    }
}
impl Shell {
    /// From the JSON the original's shells have (the parity fixtures).
    pub fn from_json(thread: &Value) -> Self {
        let forked = (thread["forkedFrom"]["type"] == "run").then(|| thread["forkedFrom"]["threadId"].as_str().map(str::to_owned)).flatten();
        Self {
            id: thread["id"].as_str().unwrap_or_default().to_owned(),
            title: thread["title"].as_str().unwrap_or_default().to_owned(),
            parent: forked.or_else(|| thread["lineage"]["parentThreadId"].as_str().map(str::to_owned)),
            subagent: thread["lineage"]["relationshipToParent"] == "subagent",
            activity_status: thread["activityRunStatus"].as_str().map(str::to_owned),
            status: thread["status"].as_str().unwrap_or_default().to_owned(),
            created_ms: thread["createdAtMs"].as_i64(),
            project_id: thread["projectId"].as_str().unwrap_or_default().to_owned(),
            provider_instance_id: thread["providerInstanceId"].as_str().unwrap_or_default().to_owned(),
        }
    }
    /// The status the thread's own row shows: its live run's, else its status.
    fn live_status(&self) -> &str {
        self.activity_status.as_deref().unwrap_or(&self.status)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Fork,
    Subagent,
    Transfer,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub source: String,
    pub target: String,
    pub kind: Kind,
    pub status: Option<String>,
}
#[derive(Debug, Clone, Default)]
pub struct Graph {
    /// Every thread the graph names; `None` for one whose shell is not known.
    pub nodes: IndexMap<String, Option<Shell>>,
    pub edges: Vec<Edge>,
}
/// One related thread, reached over `edge`.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub thread: String,
    pub edge: Edge,
}

/// `deriveThreadRelationshipGraph`. Shells come most authoritative first: the first of an id wins.
pub fn graph(shells: &[Shell], projection: Option<&Value>) -> Graph {
    let mut nodes: IndexMap<String, Option<Shell>> = IndexMap::new();
    for shell in shells {
        nodes.entry(shell.id.clone()).or_insert_with(|| Some(shell.clone()));
    }
    let known: Vec<Shell> = nodes.values().flatten().cloned().collect();
    let mut edges: IndexMap<(String, String, Kind), Edge> = IndexMap::new();
    let mut add = |nodes: &mut IndexMap<String, Option<Shell>>, edge: Edge| {
        nodes.entry(edge.source.clone()).or_insert(None);
        nodes.entry(edge.target.clone()).or_insert(None);
        edges.insert((edge.source.clone(), edge.target.clone(), edge.kind), edge);
    };
    for thread in &known {
        let Some(parent) = &thread.parent else { continue };
        add(&mut nodes, Edge { source: parent.clone(), target: thread.id.clone(), kind: if thread.subagent { Kind::Subagent } else { Kind::Fork }, status: Some(thread.live_status().to_owned()) });
    }
    if let Some(projection) = projection {
        let owner = projection["thread"]["id"].as_str().unwrap_or_default().to_owned();
        for subagent in projection["subagents"].as_array().into_iter().flatten() {
            let Some(child) = subagent["childThreadId"].as_str() else { continue };
            // The subagent record settles with the delegated task's first run, but the parent can keep
            // sending the child follow-ups: a live run on the child outranks the settled status.
            let live = known.iter().find(|thread| thread.id == child).and_then(|thread| thread.activity_status.clone());
            add(&mut nodes, Edge { source: owner.clone(), target: child.to_owned(), kind: Kind::Subagent, status: live.or_else(|| subagent["status"].as_str().map(str::to_owned)) });
        }
        for transfer in projection["contextTransfers"].as_array().into_iter().flatten() {
            let (source, target) = (transfer["sourceThreadId"].as_str().unwrap_or_default(), transfer["targetThreadId"].as_str().unwrap_or_default());
            if source == target {
                continue;
            }
            add(&mut nodes, Edge { source: source.to_owned(), target: target.to_owned(), kind: Kind::Transfer, status: transfer["status"].as_str().map(str::to_owned) });
        }
    }
    Graph { nodes, edges: edges.into_values().collect() }
}

impl Graph {
    pub fn shell(&self, id: &str) -> Option<&Shell> {
        self.nodes.get(id)?.as_ref()
    }
    /// `immediateThreadRelationships`.
    pub fn immediate(&self, thread: &str) -> Vec<Row> {
        let mut seen = HashSet::new();
        let mut rows = vec![];
        for edge in &self.edges {
            let related = if edge.source == thread { &edge.target } else if edge.target == thread { &edge.source } else { continue };
            if seen.insert(related.clone()) {
                rows.push(Row { thread: related.clone(), edge: edge.clone() });
            }
        }
        rows
    }
    /// `threadRelationshipRowStatus`: an incoming parent row shows its own activity, not the child's.
    pub fn row_status(&self, row: &Row) -> Option<String> {
        if row.edge.kind == Kind::Transfer || row.thread == row.edge.target {
            return row.edge.status.clone();
        }
        self.shell(&row.thread).map(|thread| thread.live_status().to_owned())
    }
}

/// `isParentThreadRelationship`: `edge` reaches `current` from its parent or owning agent.
pub fn is_parent(edge: &Edge, current: &str) -> bool {
    edge.kind != Kind::Transfer && edge.target == current
}

/// `orderWebThreadLineageRows`: the parent first, the merge-back target second, then newest first.
pub fn order_rows(graph: &Graph, rows: Vec<Row>, current: &str, merge_target: Option<&str>) -> Vec<Row> {
    let rank = |row: &Row| if is_parent(&row.edge, current) { 0 } else if Some(row.thread.as_str()) == merge_target { 1 } else { 2 };
    let created = |row: &Row| graph.shell(&row.thread).and_then(|thread| thread.created_ms);
    let mut rows = rows;
    rows.sort_by(|left, right| {
        rank(left).cmp(&rank(right)).then_with(|| match (created(left), created(right)) {
            (a, b) if a == b => left.thread.cmp(&right.thread),
            (None, _) => std::cmp::Ordering::Greater,
            (_, None) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => b.cmp(&a),
        })
    });
    rows
}

/// `resolveMergeBackTargetThreadId` over the projection's `thread`: a fork merges back into the
/// thread it was forked from.
pub fn merge_back_target(thread: &Value) -> Option<String> {
    if thread["lineage"]["relationshipToParent"] != "fork" {
        return None;
    }
    if thread["forkedFrom"]["type"] == "run" {
        return thread["forkedFrom"]["threadId"].as_str().map(str::to_owned);
    }
    thread["lineage"]["parentThreadId"].as_str().map(str::to_owned)
}

/// `resolveLatestMergeBackRun`: a finished provider turn stays `waiting` while its checkpoint is
/// captured, and that newest turn is still the one to merge back.
pub fn latest_merge_back_run(projection: &Value) -> Option<&Value> {
    let runs = projection["runs"].as_array()?;
    let mut latest: Option<&Value> = None;
    for run in runs {
        if matches!(run["status"].as_str(), Some("waiting" | "completed")) && latest.is_none_or(|known| run["ordinal"].as_i64() > known["ordinal"].as_i64()) {
            latest = Some(run);
        }
    }
    let latest = latest?;
    let blocked = runs.iter().any(|run| run["ordinal"].as_i64() > latest["ordinal"].as_i64() && matches!(run["status"].as_str(), Some("preparing" | "starting" | "running")));
    (!blocked).then_some(latest)
}

/// `canForkProjectedAssistantItem`: `capabilities` is the item's provider session's, absent for a
/// historical or inherited row whose session is gone (the server then forks portably).
pub fn can_fork_assistant_item(item: &Value, capabilities: Option<&Value>) -> bool {
    if item["type"] != "assistant_message" || item["runId"].is_null() || item["status"] != "completed" {
        return false;
    }
    let Some(capabilities) = capabilities else { return true };
    let natively = capabilities["threads"]["canForkThread"] == true && capabilities["threads"]["canForkFromTurn"] == true && capabilities["identity"]["nativeThreadIds"] == "strong";
    natively || capabilities["context"]["supportsFullThreadHandoff"] == true
}

/// The capabilities of the provider session that ran `item` (`resolveV2ItemSupport.providerSession`),
/// when the projection that holds the item also holds its session.
pub fn item_capabilities<'a>(projection: &'a Value, item: &Value) -> Option<&'a Value> {
    let thread = projection["providerThreads"].as_array()?.iter().find(|thread| !item["providerThreadId"].is_null() && thread["id"] == item["providerThreadId"])?;
    let session = projection["providerSessions"].as_array()?.iter().find(|session| !thread["providerSessionId"].is_null() && session["id"] == thread["providerSessionId"])?;
    Some(&session["capabilities"])
}

/// `forkThreadFromRun`: the command that forks `source` at `run` into `target`.
pub fn fork_command(source: &str, target: &str, run_id: &str, title: &str, creation_source: &str) -> Value {
    json!({"type":"thread.fork","createdBy":"user","creationSource":creation_source,"sourceThreadId":source,"targetThreadId":target,"sourcePoint":{"type":"run","runId":run_id},"title":title})
}
/// The title a fork starts with (`${activeThread.title} fork`).
pub fn fork_title(title: &str) -> String {
    format!("{title} fork")
}
/// `mergeThreadBack`.
pub fn merge_back_command(source: &str, target: &str, run_id: &str, creation_source: &str) -> Value {
    json!({"type":"thread.merge_back","createdBy":"user","creationSource":creation_source,"sourceThreadId":source,"targetThreadId":target,"sourcePoint":{"type":"run","runId":run_id}})
}

/// `threadRelationshipStatusLabel`.
pub fn status_label(status: Option<&str>) -> &'static str {
    match status {
        Some("preparing" | "starting") => "Starting",
        Some("running" | "in_progress") => "Running",
        Some("pending" | "queued") => "Queued",
        Some("waiting" | "blocked") => "Waiting",
        Some("completed") => "Done",
        Some("failed" | "error") => "Failed",
        Some("cancelled" | "interrupted") => "Stopped",
        Some("rolled_back") => "Reverted",
        Some("resolved_native") => "Resolved (native)",
        Some("resolved_portable") => "Resolved (portable)",
        Some("consumed") => "Consumed",
        Some("superseded") => "Superseded",
        Some("idle") => "Idle",
        _ => "Unknown",
    }
}

/// `relationshipLabel`: what a related thread is to the open one.
pub fn relationship_label(edge: &Edge, current: &str) -> &'static str {
    match edge.kind {
        Kind::Transfer => "Context transfer",
        Kind::Subagent => if edge.source == current { "Subagent" } else { "Parent agent" },
        Kind::Fork => if edge.source == current { "Fork" } else { "Parent thread" },
    }
}

/// The tooltip of the merge-back button.
pub fn merge_back_tooltip(has_run: bool, parent_title: Option<&str>) -> String {
    if !has_run {
        return "Complete a run in this fork before merging it back".into();
    }
    match parent_title {
        Some(title) => format!("Merge this conversation back into {title}"),
        None => "Merge this conversation back into its source".into(),
    }
}
/// The merge-back button's accessible name.
pub fn merge_back_label(parent_title: Option<&str>) -> String {
    match parent_title {
        Some(title) => format!("Merge back to {title}"),
        None => "Merge back to source conversation".into(),
    }
}

/// One side of a context handoff: a provider instance and, when known, the model.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub instance_id: String,
    pub model: Option<String>,
}
/// `resolveHandoffEndpoints`: items persisted before models were stamped carry instance ids only,
/// and the models are recovered from the thread's runs.
pub fn handoff_endpoints(item: &Value, runs: &[Value]) -> (Vec<Endpoint>, Endpoint) {
    let handoff_run = item["runId"].as_str().and_then(|id| runs.iter().find(|run| run["id"] == id));
    let to_instance = item["toProviderInstanceId"].as_str().unwrap_or_default();
    let to_model = item["toModel"].as_str().map(str::to_owned).or_else(|| handoff_run.filter(|run| run["providerInstanceId"] == to_instance).and_then(|run| run["modelSelection"]["model"].as_str().map(str::to_owned)));
    let stamped = item["fromModelSelections"].as_array().filter(|list| !list.is_empty());
    let from = match stamped {
        Some(list) => list.iter().map(|selection| Endpoint { instance_id: selection["instanceId"].as_str().unwrap_or_default().to_owned(), model: selection["model"].as_str().map(str::to_owned) }).collect(),
        None => item["fromProviderInstanceIds"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|instance| Endpoint { instance_id: instance.to_owned(), model: latest_run_model_before(runs, instance, handoff_run.and_then(|run| run["ordinal"].as_i64())) }).collect(),
    };
    (from, Endpoint { instance_id: to_instance.to_owned(), model: to_model })
}
fn latest_run_model_before(runs: &[Value], instance: &str, before: Option<i64>) -> Option<String> {
    let mut latest: Option<&Value> = None;
    for run in runs {
        if run["providerInstanceId"] != instance || before.is_some_and(|ordinal| run["ordinal"].as_i64() >= Some(ordinal)) {
            continue;
        }
        if latest.is_none_or(|known| run["ordinal"].as_i64() > known["ordinal"].as_i64()) {
            latest = Some(run);
        }
    }
    latest.and_then(|run| run["modelSelection"]["model"].as_str().map(str::to_owned))
}

/// The label of a `fork` timeline item and where its button leads (`V2LifecycleRow`).
pub fn fork_row(item: &Value) -> (&'static str, &'static str, String) {
    let from_run = item["source"]["type"] == "run";
    let related = if from_run { &item["source"]["threadId"] } else { &item["targetThreadId"] };
    (if from_run { "Forked from conversation" } else { "Conversation fork" }, if from_run { "Open source conversation" } else { "Open fork" }, related.as_str().unwrap_or_default().to_owned())
}

/// `getTriggerDisplayModelName`: the short name when there is one, without the sub-provider it
/// leads with.
pub fn model_label(model: &crate::models::ModelOption) -> String {
    let name = model.short_name.as_deref().filter(|name| !name.is_empty()).unwrap_or(&model.name);
    let Some(qualifier) = model.sub_provider.as_deref().map(str::trim).filter(|qualifier| !qualifier.is_empty()) else { return name.to_owned() };
    let pattern = format!(r"(?iu)^{}(?:\s*[.:/-]\s*|\s+)", regex::escape(qualifier));
    let Ok(pattern) = regex::Regex::new(&pattern) else { return name.to_owned() };
    let stripped = pattern.replace(name, "").trim().to_owned();
    if stripped.is_empty() { name.to_owned() } else { stripped }
}

/// The run to interrupt to stop a subagent the app itself runs (`canStop` of
/// `ThreadRelationshipsControl.tsx`), or none when it is not one, has not started, or has finished.
/// `agent` is the subagent as the parent's projection lists it; `child` is what the child thread's
/// shell says: its live activity status and start, and its active (else latest) run. A live shell
/// outranks the agent's own, settled, status: a task woken for a second run is running again.
pub fn subagent_stop_run(agent: &Value, child: Option<&ChildActivity>) -> Option<String> {
    if agent["origin"] != "app_owned" {
        return None;
    }
    let (status, started) = match child.and_then(|child| child.live.as_ref()) {
        Some((status, started)) => (if matches!(status.as_str(), "running" | "waiting") { status.as_str() } else { "pending" }, *started),
        None => (agent["status"].as_str().unwrap_or(""), agent["startedAt"].as_str().is_some_and(|at| !at.is_empty())),
    };
    if !started || !matches!(status, "pending" | "running" | "waiting") {
        return None;
    }
    child.and_then(|child| child.run.clone())
}
/// A child thread's activity, from its shell.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChildActivity {
    /// The activity run's status and whether it has started.
    pub live: Option<(String, bool)>,
    pub run: Option<String>,
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_running_app_owned_subagent_can_be_stopped_and_by_its_own_run() {
        let child = ChildActivity { live: Some(("running".into(), true)), run: Some("run-9".into()) };
        let agent = |origin: &str, status: &str, started: Option<&str>| json!({"origin":origin,"status":status,"startedAt":started});
        assert_eq!(subagent_stop_run(&agent("app_owned", "running", Some("t")), Some(&child)).as_deref(), Some("run-9"));
        // A provider's own subagent is the provider's to stop.
        assert_eq!(subagent_stop_run(&agent("provider_native", "running", Some("t")), Some(&child)), None);
        // Nothing live, and the agent itself finished or never started: nothing to stop.
        let idle = ChildActivity { live: None, run: Some("run-9".into()) };
        assert_eq!(subagent_stop_run(&agent("app_owned", "completed", Some("t")), Some(&idle)), None);
        assert_eq!(subagent_stop_run(&agent("app_owned", "running", None), Some(&idle)), None);
        assert_eq!(subagent_stop_run(&agent("app_owned", "running", Some("t")), Some(&idle)).as_deref(), Some("run-9"));
        // A task woken again is running again, whatever its settled status says.
        assert_eq!(subagent_stop_run(&agent("app_owned", "completed", Some("t")), Some(&child)).as_deref(), Some("run-9"));
        // Not started yet, or no run to name.
        assert_eq!(subagent_stop_run(&agent("app_owned", "running", Some("t")), Some(&ChildActivity { live: Some(("running".into(), false)), run: Some("r".into()) })), None);
        assert_eq!(subagent_stop_run(&agent("app_owned", "running", Some("t")), None), None);
    }
}
