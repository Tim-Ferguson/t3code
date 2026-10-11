//! What a client is served of a thread's stored projection (`ProjectionStore.readProjection`, `readCanonicalProjection`), as
//! opposed to what is stored: the order the original's queries list rows in, the rows other threads' events wrote that this thread
//! reads (a provider session that serves several threads, a child thread's provider threads), a fork's history as its source holds
//! it now, and, for the bounded snapshot (`getThreadSnapshotWindow`), only the rows the newest turn items lead to: what a rollback
//! undid, a wake's notification message and records nothing recent refers to are left out.
//!
//! Stored lists are in the order events wrote them; the original's are in the order of its queries, which clients may rely on.
use crate::persistence::{StoreError, read_projection};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashSet;

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}
/// A list of the projection, moved out of it (left empty) to be filtered and ordered in place.
fn take(projection: &mut Value, field: &str) -> Vec<Value> {
    match projection[field].take() {
        Value::Array(list) => list,
        _ => Vec::new(),
    }
}
fn text<'a>(row: &'a Value, field: &str) -> &'a str {
    row[field].as_str().unwrap_or("")
}
fn is_in(set: &HashSet<String>, value: &Value) -> bool {
    value.as_str().is_some_and(|id| set.contains(id))
}
fn collect(items: &[Value], field: &str) -> HashSet<String> {
    items.iter().filter_map(|item| item[field].as_str().map(str::to_owned)).collect()
}
fn active(status: &Value) -> bool {
    matches!(status.as_str(), Some("pending" | "starting" | "running" | "waiting"))
}
fn run_open(status: &Value) -> bool {
    matches!(status.as_str(), Some("queued" | "preparing" | "starting" | "running" | "waiting"))
}

/// The rows a query for a thread's turn items returns when it is bounded to the newest `limit` (`readCanonicalProjection` with a
/// window of rows and no anchor): items of runs a rollback undid, the queued message of a cancelled run and a superseded stop result
/// are not history; of what is, the newest `limit`, with the stop request each stop result shown needs.
fn windowed_items(projection: &Value, limit: usize) -> Vec<Value> {
    let (runs, attempts, items) = (rows(projection, "runs"), rows(projection, "attempts"), rows(projection, "turnItems"));
    let status_of = |run: &Value| runs.iter().find(|candidate| candidate["id"] == *run).and_then(|run| run["status"].as_str());
    let eligible = |item: &&Value| {
        let status = status_of(&item["runId"]);
        if status == Some("rolled_back") || (status == Some("cancelled") && item["type"] == "user_message" && item["inputIntent"] == "queued_turn") {
            return false;
        }
        !(item["type"] == "run_interrupt_result"
            && attempts.iter().any(|attempt| attempt["runId"] == item["runId"] && attempt["rootNodeId"] == item["nodeId"] && attempt["status"] == "superseded")
            && !items.iter().any(|request| request["type"] == "run_interrupt_request" && request["runId"] == item["runId"] && request["threadId"] == item["threadId"]))
    };
    let order = |item: &Value| (item["ordinal"].as_i64().unwrap_or(0), text(item, "id").to_owned());
    let mut eligible: Vec<&Value> = items.iter().filter(eligible).collect();
    eligible.sort_by_key(|item| order(item));
    let mut selected: Vec<&Value> = eligible[eligible.len().saturating_sub(limit)..].to_vec();
    let results: HashSet<&str> = selected.iter().filter(|item| item["type"] == "run_interrupt_result").filter_map(|item| item["runId"].as_str()).collect();
    selected.extend(items.iter().filter(|item| item["type"] == "run_interrupt_request" && item["runId"].as_str().is_some_and(|run| results.contains(run))));
    let mut seen = HashSet::new();
    let mut retained: Vec<Value> = selected.into_iter().filter(|item| seen.insert(text(item, "id").to_owned())).cloned().collect();
    retained.sort_by_key(|item| order(item));
    retained
}

/// The thread's projection as the original reads it. `windowed` is the bounded read (the first page of a thread, the compatibility
/// `getThreadProjection`); otherwise every row is read.
pub(crate) fn served(connection: &Connection, projection: Value, windowed: bool) -> Result<Value, StoreError> {
    let mut projection = crate::thread_fork::refreshed(projection, &|source| read_projection(connection, "thread", source).ok().flatten());
    if let Some(object) = projection.as_object_mut() {
        object.remove(crate::shell_state::KEY);
    }
    let thread_id = text(&projection["thread"], "id").to_owned();
    const WINDOW_ROWS: usize = 77;
    // The turn items, and what they lead to (`cohort*` in the original).
    let items = if windowed {
        windowed_items(&projection, WINDOW_ROWS)
    } else {
        let mut all = take(&mut projection, "turnItems");
        all.sort_by_key(|item| (item["ordinal"].as_i64().unwrap_or(0), text(item, "id").to_owned()));
        all
    };
    let (cohort_runs, cohort_threads, cohort_turns, cohort_messages, cohort_plans, cohort_checkpoints, cohort_handoffs) = (
        collect(&items, "runId"), collect(&items, "providerThreadId"), collect(&items, "providerTurnId"), collect(&items, "messageId"),
        collect(&items, "planId"), collect(&items, "checkpointId"), collect(&items, "contextHandoffId"),
    );
    let mut cohort_nodes = collect(&items, "nodeId");
    if windowed {
        cohort_nodes.extend(rows(&projection, "nodes").iter().filter(|node| active(&node["status"])).filter_map(|node| node["id"].as_str().map(str::to_owned)));
        cohort_nodes.extend(rows(&projection, "attempts").iter().filter(|attempt| active(&attempt["status"]) || is_in(&cohort_runs, &attempt["runId"])).filter_map(|attempt| attempt["rootNodeId"].as_str().map(str::to_owned)));
        cohort_nodes.extend(rows(&projection, "runs").iter().filter(|run| run_open(&run["status"]) || is_in(&cohort_runs, &run["id"])).filter_map(|run| run["rootNodeId"].as_str().map(str::to_owned)));
        cohort_nodes.extend(rows(&projection, "runtimeRequests").iter().filter(|request| matches!(request["status"].as_str(), Some("pending" | "waiting"))).filter_map(|request| request["nodeId"].as_str().map(str::to_owned)));
        cohort_nodes.extend(rows(&projection, "subagents").iter().filter(|task| active(&task["status"])).filter_map(|task| task["parentNodeId"].as_str().map(str::to_owned)));
        cohort_nodes.extend(rows(&projection, "providerTurns").iter().filter(|turn| matches!(turn["status"].as_str(), Some("starting" | "running" | "waiting"))).filter_map(|turn| turn["nodeId"].as_str().map(str::to_owned)));
        // Every ancestor of a retained node.
        loop {
            let parents: Vec<String> = rows(&projection, "nodes").iter().filter(|node| is_in(&cohort_nodes, &node["id"])).filter_map(|node| node["parentNodeId"].as_str().map(str::to_owned)).filter(|parent| !cohort_nodes.contains(parent)).collect();
            if parents.is_empty() {
                break;
            }
            cohort_nodes.extend(parents);
        }
    }
    // What the shared tables are asked, and what messages are kept for open runs, are read before the lists are taken apart.
    let (owners, held): (HashSet<String>, HashSet<String>) = if windowed {
        (cohort_nodes.clone(), cohort_threads.clone())
    } else {
        (collect(rows(&projection, "nodes"), "id"), collect(rows(&projection, "subagents"), "providerThreadId"))
    };
    let open_runs: HashSet<String> = rows(&projection, "runs").iter().filter(|run| run_open(&run["status"])).filter_map(|run| run["id"].as_str().map(str::to_owned)).collect();
    let keep = |windowed_rule: bool| !windowed || windowed_rule;

    let mut runs = take(&mut projection, "runs");
    runs.retain(|run| keep(run_open(&run["status"]) || is_in(&cohort_runs, &run["id"])));
    runs.sort_by_key(|run| run["ordinal"].as_i64().unwrap_or(0));
    let mut attempts = take(&mut projection, "attempts");
    attempts.retain(|attempt| keep(active(&attempt["status"]) || is_in(&cohort_runs, &attempt["runId"])));
    attempts.sort_by_key(|attempt| (text(attempt, "runId").to_owned(), attempt["attemptOrdinal"].as_i64().unwrap_or(0)));
    let mut nodes = take(&mut projection, "nodes");
    nodes.retain(|node| keep(is_in(&cohort_nodes, &node["id"])));
    nodes.sort_by_key(|node| (text(node, "startedAt").to_owned(), text(node, "id").to_owned()));
    let mut subagents = take(&mut projection, "subagents");
    subagents.retain(|task| keep(active(&task["status"]) || is_in(&cohort_runs, &task["runId"]) || is_in(&cohort_nodes, &task["parentNodeId"])));
    subagents.sort_by_key(|task| (text(task, "startedAt").to_owned(), text(task, "id").to_owned()));

    // Provider threads: the thread's own, and the others a node of it owns or a subagent of it holds (the cohort's, when windowed).
    let mut provider_threads = take(&mut projection, "providerThreads");
    {
        let mut statement = connection.prepare("SELECT json FROM rust_provider_threads WHERE owner_node_id IN (SELECT value FROM json_each(?1)) OR id IN (SELECT value FROM json_each(?2))")?;
        let (owners, held) = (serde_json::to_string(&owners)?, serde_json::to_string(&held)?);
        for json in statement.query_map([owners, held], |row| row.get::<_, String>(0))? {
            let found: Value = serde_json::from_str(&json?)?;
            match provider_threads.iter_mut().find(|existing| existing["id"] == found["id"]) {
                Some(existing) => *existing = found,
                None => provider_threads.push(found),
            }
        }
    }
    if windowed {
        provider_threads.retain(|thread| (thread["appThreadId"] == thread_id.as_str() && thread["status"] == "active") || is_in(&cohort_threads, &thread["id"]) || is_in(&cohort_nodes, &thread["ownerNodeId"]));
    }
    provider_threads.sort_by_key(|thread| (thread["firstRunOrdinal"].as_i64().unwrap_or(0), text(thread, "id").to_owned()));

    // Provider sessions bound to the thread, as their one row now says. Only a bound session is the thread's: the original joins its
    // session rows to the bindings, so a session the thread's own events updated but never attached is not served.
    let mut sessions: Vec<Value> = Vec::new();
    take(&mut projection, "providerSessions");
    {
        let mut statement = connection.prepare("SELECT s.json FROM rust_provider_sessions s JOIN rust_provider_session_bindings b ON b.session_id = s.id WHERE b.thread_id = ?1")?;
        for json in statement.query_map([&thread_id], |row| row.get::<_, String>(0))? {
            sessions.push(serde_json::from_str(&json?)?);
        }
    }
    if windowed {
        let leads_to = |session: &Value| provider_threads.iter().any(|thread| thread["providerSessionId"] == session["id"] && is_in(&cohort_threads, &thread["id"]));
        sessions.retain(|session| matches!(session["status"].as_str(), Some("starting" | "running" | "waiting")) || leads_to(session));
    }
    sessions.sort_by_key(|session| (text(session, "updatedAt").to_owned(), text(session, "id").to_owned()));

    let mut provider_turns = take(&mut projection, "providerTurns");
    provider_turns.retain(|turn| keep(matches!(turn["status"].as_str(), Some("starting" | "running" | "waiting")) || is_in(&cohort_turns, &turn["id"]) || is_in(&cohort_nodes, &turn["nodeId"])));
    provider_turns.sort_by_key(|turn| (text(turn, "providerThreadId").to_owned(), turn["ordinal"].as_i64().unwrap_or(0)));
    let mut requests = take(&mut projection, "runtimeRequests");
    requests.retain(|request| keep(matches!(request["status"].as_str(), Some("pending" | "waiting")) || is_in(&cohort_nodes, &request["nodeId"]) || is_in(&cohort_turns, &request["providerTurnId"])));
    requests.sort_by_key(|request| (text(request, "createdAt").to_owned(), text(request, "id").to_owned()));

    // Messages in the order of the first turn item that shows them, then by creation.
    let mut first_item: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for item in &items {
        if matches!(item["type"].as_str(), Some("user_message" | "assistant_message")) {
            if let (Some(id), Some(ordinal)) = (item["messageId"].as_str(), item["ordinal"].as_i64()) {
                let entry = first_item.entry(id.to_owned()).or_insert(ordinal);
                *entry = (*entry).min(ordinal);
            }
        }
    }
    let mut messages = take(&mut projection, "messages");
    messages.retain(|message| keep(is_in(&cohort_messages, &message["id"]) || is_in(&open_runs, &message["runId"])));
    messages.sort_by_key(|message| (first_item.get(text(message, "id")).copied().unwrap_or(i64::MAX), text(message, "createdAt").to_owned(), text(message, "id").to_owned()));

    let mut plans = take(&mut projection, "plans");
    plans.retain(|plan| keep(plan["status"] == "active" || is_in(&cohort_plans, &plan["id"])));
    plans.sort_by_key(|plan| text(plan, "id").to_owned());
    let mut scopes = take(&mut projection, "checkpointScopes");
    scopes.retain(|scope| keep(is_in(&cohort_runs, &scope["runId"]) || is_in(&cohort_nodes, &scope["nodeId"])));
    scopes.sort_by_key(|scope| (scope["ordinalWithinParent"].as_i64().unwrap_or(0), text(scope, "id").to_owned()));
    let mut checkpoints = take(&mut projection, "checkpoints");
    checkpoints.retain(|checkpoint| keep(matches!(checkpoint["status"].as_str(), Some("pending" | "capturing")) || is_in(&cohort_checkpoints, &checkpoint["id"]) || is_in(&cohort_runs, &checkpoint["runId"]) || is_in(&cohort_nodes, &checkpoint["nodeId"])));
    checkpoints.sort_by_key(|checkpoint| (text(checkpoint, "scopeId").to_owned(), checkpoint["ordinalWithinScope"].as_i64().unwrap_or(0)));
    let mut handoffs = take(&mut projection, "contextHandoffs");
    handoffs.retain(|handoff| keep(matches!(handoff["status"].as_str(), Some("pending" | "ready")) || is_in(&cohort_handoffs, &handoff["id"])));
    let mut transfers = take(&mut projection, "contextTransfers");
    transfers.retain(|transfer| keep(matches!(transfer["status"].as_str(), Some("pending" | "running" | "waiting")) || is_in(&cohort_runs, &transfer["targetRunId"]) || is_in(&cohort_handoffs, &transfer["contextHandoffId"])));

    for (field, list) in [
        ("runs", runs), ("attempts", attempts), ("nodes", nodes), ("subagents", subagents), ("providerSessions", sessions), ("providerThreads", provider_threads),
        ("providerTurns", provider_turns), ("runtimeRequests", requests), ("messages", messages), ("plans", plans), ("turnItems", items), ("checkpointScopes", scopes),
        ("checkpoints", checkpoints), ("contextHandoffs", handoffs), ("contextTransfers", transfers),
    ] {
        projection[field] = Value::Array(list);
    }
    Ok(projection)
}
