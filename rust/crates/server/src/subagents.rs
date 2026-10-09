//! Subagents in the orchestration core: the child thread a subagent runs in, how it is titled, the
//! conversation rows an adapter writes into it, what a parent's timeline shows, and how a settling
//! run ends the subagents it owns. Ports `SubagentProjection.ts` and
//! `RunExecutionService.cascadeTerminalizeRunOwnedSubagents`.
//!
//! Adapters speak to this module and nothing else about subagents: a subagent is a `subagent` node
//! of the parent run, a `Subagent` row (`subagent.updated`), a `subagent` item in the parent's
//! timeline, and a child thread (`thread.created`) with lineage `subagent` whose own provider
//! thread, turns, nodes and items hold what the child did. A child thread has no runs. Its approvals
//! and questions are asked on the top-level parent thread, never on the child.
use crate::{
    codex_runtime::commit,
    execution::{at, event},
    persistence::{Decision, Store, StoreError},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}

/// The provider runs a native subagent's conversation, so a message sent to its thread has
/// nowhere to go. A delegated child (created for the agent through MCP) can take messages.
pub(crate) fn is_provider_native_subagent_thread(thread: &Value) -> bool {
    thread["lineage"]["relationshipToParent"] == "subagent" && thread["creationSource"] == "provider"
}

pub(crate) const READ_ONLY_REASON: &str = "This subagent is run by its provider and cannot take messages. Message the parent thread instead.";

fn trimmed(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|text| !text.is_empty())
}

/// The title of a child thread: its title or prompt, clipped to 72 characters.
pub(crate) fn thread_title(parent_title: &str, title: &Value, prompt: &str, ordinal: u64) -> String {
    let Some(detail) = trimmed(title).or_else(|| Some(prompt.trim()).filter(|text| !text.is_empty())) else {
        return format!("{parent_title} subagent {ordinal}");
    };
    // The original clips by UTF-16 unit; characters differ only for text beyond the basic plane.
    let units: Vec<u16> = detail.encode_utf16().collect();
    if units.len() > 72 { format!("{}...", String::from_utf16_lossy(&units[..69])) } else { detail.to_owned() }
}

/// The app thread a subagent runs in: the parent's settings, a fresh identity and lineage.
pub(crate) fn child_thread(parent: &Value, input: &Value, now: &str) -> Value {
    let mut child = parent.clone();
    let object = child.as_object_mut().unwrap();
    object.remove("historyOrigin");
    for (key, value) in [
        ("createdBy", input["createdBy"].clone()),
        ("creationSource", input["creationSource"].clone()),
        ("id", input["childThreadId"].clone()),
        ("title", input["title"].clone()),
        ("linkedPullRequest", Value::Null),
        ("pullRequests", json!([])),
        ("providerInstanceId", input["providerInstanceId"].clone()),
        ("modelSelection", input["modelSelection"].clone()),
        ("activeProviderThreadId", input["activeProviderThreadId"].clone()),
        ("lineage", json!({"parentThreadId":parent["id"],"relationshipToParent":"subagent","rootThreadId":parent["lineage"]["rootThreadId"]})),
        ("forkedFrom", json!({"type":"node","nodeId":input["parentNodeId"]})),
        ("createdAt", json!(now)),
        ("updatedAt", json!(now)),
        ("archivedAt", Value::Null),
        ("settledOverride", Value::Null),
        ("settledAt", Value::Null),
        ("snoozedUntil", Value::Null),
        ("snoozedAt", Value::Null),
        ("lastVisitedAt", Value::Null),
        ("deletedAt", Value::Null),
    ] {
        object.insert(key.into(), value);
    }
    child
}

/// A user or assistant message of a child thread, with its transcript item.
pub(crate) fn conversation_artifacts(input: &Value, now: &str) -> (Value, Value) {
    let user = input["role"] == "user";
    let sender = (user && !input["senderThreadId"].is_null()).then(|| input["senderThreadId"].clone());
    let mut message = json!({"createdBy":"agent","creationSource":"provider","id":input["messageId"],"threadId":input["threadId"],"runId":null,"nodeId":input["rootNodeId"],"role":input["role"],"text":input["text"],"attachments":[],"streaming":false,"createdAt":now,"updatedAt":now});
    let mut item = json!({"id":input["turnItemId"],"threadId":input["threadId"],"runId":null,"nodeId":input["rootNodeId"],"providerThreadId":input["providerThreadId"],"providerTurnId":input["providerTurnId"],"nativeItemRef":input["nativeItemRef"],"parentItemId":null,"ordinal":input["ordinal"],"status":"completed","title":null,"startedAt":now,"completedAt":now,"updatedAt":now,"messageId":input["messageId"],"text":input["text"]});
    if user {
        item["createdBy"] = json!("agent");
        item["creationSource"] = json!("provider");
        item["type"] = json!("user_message");
        item["inputIntent"] = json!("turn_start");
        item["attachments"] = json!([]);
        if let Some(sender) = sender {
            message["senderThreadId"] = sender.clone();
            item["senderThreadId"] = sender;
        }
    } else {
        item["type"] = json!("assistant_message");
        item["streaming"] = json!(false);
    }
    (message, item)
}

fn instant(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str().and_then(|text| text.parse().ok())
}

/// What a finished run said, to report to whoever waits for it: its failure, else its latest
/// assistant message, else its latest assistant item, else a sentence about how it ended.
pub(crate) fn result_for_run(projection: &Value, run: &Value) -> Value {
    let of_run = |row: &&Value| row["runId"] == run["id"];
    let message = rows(projection, "messages")
        .iter()
        .filter(of_run)
        .filter(|message| message["role"] == "assistant" && trimmed(&message["text"]).is_some())
        .max_by_key(|message| instant(&message["updatedAt"]));
    let item = rows(projection, "turnItems")
        .iter()
        .filter(of_run)
        .filter(|item| item["type"] == "assistant_message" && trimmed(&item["text"]).is_some())
        .max_by_key(|item| item["ordinal"].as_u64());
    let failure = (run["status"] == "failed")
        .then(|| rows(projection, "turnItems").iter().filter(of_run).filter(|item| item["type"] == "error").max_by_key(|item| item["ordinal"].as_u64()))
        .flatten();
    let status = run["status"].as_str().unwrap_or_default();
    let text = failure
        .and_then(|item| item["failure"]["message"].as_str().map(str::to_owned))
        .or_else(|| message.and_then(|message| message["text"].as_str().map(str::to_owned)))
        .or_else(|| item.and_then(|item| item["text"].as_str().map(str::to_owned)))
        .unwrap_or_else(|| if status == "completed" { "Child task completed without an assistant result.".into() } else { format!("Child task ended with status {status}.") });
    let message_id = if failure.is_none() { message.map(|message| message["id"].clone()).or_else(|| item.map(|item| item["messageId"].clone())).unwrap_or(Value::Null) } else { Value::Null };
    let item_id = failure.map(|failure| failure["id"].clone()).or_else(|| item.map(|item| item["id"].clone())).unwrap_or(Value::Null);
    json!({"text":text,"messageId":message_id,"turnItemId":item_id})
}

pub(crate) fn terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "interrupted" | "rolled_back")
}

/// Whether started `run` ran after `other`; an unfinished run is the latest.
pub(crate) fn ran_after(run: &Value, other: &Value) -> bool {
    let end = |run: &Value| instant(&run["completedAt"]).map(|at| at.timestamp_millis());
    let ordering = match (end(run), end(other)) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => left.cmp(&right),
    };
    if ordering.is_eq() { run["ordinal"].as_u64() > other["ordinal"].as_u64() } else { ordering.is_gt() }
}

/// A finished turn can still own live children or queued completion follow-ups: whether a thread
/// is still working, waiting for children, or has its result available.
pub(crate) fn delegated_task_progress(projection: &Value) -> (&'static str, Option<Value>) {
    let monitor_runs: Vec<&Value> = rows(projection, "messages").iter().filter(|message| message["notification"]["source"]["kind"] == "monitor").map(|message| &message["runId"]).collect();
    let work: Vec<&Value> = rows(projection, "runs").iter().filter(|run| !monitor_runs.contains(&&run["id"]) && run["status"] != "rolled_back").collect();
    let active = work.iter().any(|run| !terminal(run["status"].as_str().unwrap_or_default()));
    let children = rows(projection, "subagents").iter().any(|task| crate::background_work::work_active(&task["status"]) || matches!(task["completionDelivery"]["state"].as_str(), Some("pending" | "claimed")))
        || rows(projection, "providerThreads").iter().any(|thread| thread["pendingBackgroundTasks"].as_array().is_some_and(|tasks| !tasks.is_empty()));
    let result = work
        .iter()
        .filter(|run| terminal(run["status"].as_str().unwrap_or_default()) && (!run["startedAt"].is_null() || run["ordinal"] == 1))
        .fold(None::<&&Value>, |latest, run| match latest {
            Some(current) if !ran_after(run, current) => Some(current),
            _ => Some(run),
        });
    let state = if active || result.is_none() { "working" } else if children { "waiting_for_children" } else { "result_available" };
    (state, result.map(|run| run["id"].clone()).map(|id| json!(id)))
}

/// Creates a child thread unless it exists. The receipt is keyed by the child, so an adapter that
/// reports the same subagent twice, or a replayed effect, creates it once.
pub(crate) fn ensure_child_thread(store: &Store, child: &Value) -> Result<(), StoreError> {
    let thread_id = child["id"].as_str().unwrap_or_default();
    let command_id = format!("subagent-thread:{thread_id}");
    let now = Utc::now();
    store.dispatch(
        &command_id,
        "thread",
        thread_id,
        "subagent.thread-create",
        now,
        |_| {
            let payload = t3_contracts::normalize_event_payload("thread.created", child.clone()).map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
            Ok(Decision::Accepted { events: vec![event(thread_id, &command_id, "thread.created", payload, now)], effects: vec![] })
        },
        crate::thread::reduce,
    )?;
    Ok(())
}

/// Ends what a run's settling leaves of the subagents it owns, as the original cascades it: the
/// open `Subagent` rows of the run, and their `subagent` nodes and parent-timeline items, become
/// the run's own terminal status. App-owned delegated tasks are not cascaded; their completion
/// belongs to the cohort. The events belong to the run's thread.
pub(crate) fn cascade_events(projection: &Value, run: &Value, status: &str, completed_at: &str) -> Vec<(&'static str, Value)> {
    if !matches!(status, "interrupted" | "failed" | "cancelled") {
        return vec![];
    }
    let mut events = vec![];
    for task in rows(projection, "subagents") {
        if task["runId"] != run["id"] || task["origin"] == "app_owned" || !crate::background_work::work_active(&task["status"]) {
            continue;
        }
        let mut task = task.clone();
        task["status"] = json!(status);
        task["completedAt"] = json!(completed_at);
        task["updatedAt"] = json!(completed_at);
        events.push(("subagent.updated", task));
    }
    events
}

/// The child threads a run's subagents run in, read before the run settles.
pub(crate) fn child_threads_of_run(projection: &Value, run: &Value) -> Vec<String> {
    let mut ids: Vec<String> = vec![];
    for task in rows(projection, "subagents") {
        if task["runId"] == run["id"] && task["origin"] != "app_owned" {
            if let Some(child) = task["childThreadId"].as_str() {
                if !ids.iter().any(|known| known == child) {
                    ids.push(child.to_owned());
                }
            }
        }
    }
    ids
}

/// Ends the open nodes and streaming items of child threads whose parent run settled without them.
/// Each child is its own thread, so each settles in a commit of its own, deepest children first.
pub(crate) fn cascade_children(store: &Store, child_thread_ids: &[String], status: &str, completed_at: &str) -> Result<(), StoreError> {
    if !matches!(status, "interrupted" | "failed" | "cancelled") {
        return Ok(());
    }
    for child_id in child_thread_ids {
        let Some(view) = store.projection("thread", child_id)? else { continue };
        let grandchildren: Vec<String> = rows(&view, "subagents").iter().filter(|task| task["origin"] != "app_owned").filter_map(|task| task["childThreadId"].as_str().map(str::to_owned)).collect();
        cascade_children(store, &grandchildren, status, completed_at)?;
        commit(store, child_id, |view| {
            let mut events: Vec<(&'static str, Value)> = vec![];
            for node in rows(view, "nodes").iter().filter(|node| matches!(node["status"].as_str(), Some("pending" | "running" | "waiting"))) {
                let mut node = node.clone();
                node["status"] = json!(status);
                node["completedAt"] = json!(completed_at);
                events.push(("node.updated", node));
            }
            for item in rows(view, "turnItems").iter().filter(|item| matches!(item["type"].as_str(), Some("assistant_message" | "reasoning")) && crate::background_work::work_active(&item["status"])) {
                let mut item = item.clone();
                item["status"] = json!(status);
                item["streaming"] = json!(false);
                item["completedAt"] = json!(completed_at);
                item["updatedAt"] = json!(completed_at);
                events.push(("turn-item.updated", item));
            }
            Ok(events)
        })?;
    }
    Ok(())
}

pub(crate) fn now_text() -> String {
    at(Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Titles, child threads, conversation rows, results and progress agree with the original.
    #[test]
    fn helpers_match_the_original() {
        let mut compared = 0;
        for line in include_str!("../tests/fixtures/subagent-projection.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let (name, input) = (row["name"].as_str().unwrap(), &row["input"]);
            let actual = match row["kind"].as_str().unwrap() {
                "title" => json!(thread_title(input["parentTitle"].as_str().unwrap(), &input["title"], input["prompt"].as_str().unwrap(), input["ordinal"].as_u64().unwrap())),
                "childThread" => child_thread(&input["parentThread"], input, input["now"].as_str().unwrap()),
                "artifacts" => {
                    let (message, item) = conversation_artifacts(input, "2026-10-08T00:00:00.000Z");
                    json!({"message":message,"turnItem":item})
                }
                "result" => result_for_run(input, &input["run"]),
                "progress" => {
                    let (state, result) = delegated_task_progress(input);
                    json!({"state":state,"resultRun":result.unwrap_or(Value::Null)})
                }
                other => panic!("{other}"),
            };
            assert_eq!(actual, row["output"], "{name}");
            compared += 1;
        }
        assert_eq!(compared, include_str!("../tests/fixtures/subagent-projection.jsonl").lines().count());
        assert!(compared >= 30);
    }

    #[test]
    fn a_settling_run_ends_its_open_provider_native_subagents_only() {
        let projection = json!({"subagents":[
            {"id":"a","runId":"run","origin":"provider_native","status":"running","childThreadId":"child"},
            {"id":"b","runId":"run","origin":"provider_native","status":"completed"},
            {"id":"c","runId":"run","origin":"app_owned","status":"running"},
            {"id":"d","runId":"other","origin":"provider_native","status":"running"}]});
        let run = json!({"id":"run"});
        let ended = cascade_events(&projection, &run, "interrupted", "2026-01-01T00:00:00.000Z");
        assert_eq!(ended.iter().map(|(kind, task)| (*kind, task["id"].as_str().unwrap(), task["status"].as_str().unwrap())).collect::<Vec<_>>(), vec![("subagent.updated", "a", "interrupted")]);
        assert!(cascade_events(&projection, &run, "completed", "2026-01-01T00:00:00.000Z").is_empty());
        assert_eq!(child_threads_of_run(&projection, &run), vec!["child".to_owned()]);
    }

    /// What an adapter writes for a native subagent, then the parent run's Stop: the child thread
    /// exists with lineage and takes no messages, and the interrupt ends the subagent row, its node
    /// and timeline item, and the child's open node and streaming item.
    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_a_parent_run_ends_its_subagents_and_their_child_threads() {
        use crate::{execution::ExecutionService, launch::ThreadLaunchService, project::ProjectService, provider_registry::ProviderRegistry};
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let store = Store::open(base.join("state.sqlite")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":base}), Utc::now()).unwrap();
        let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
        let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","enabled":false},"codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":base.join("codex-home")}}}})).unwrap();
        let providers = ProviderRegistry::discover(&settings, &base).await.unwrap();
        let thread_id = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":"Parent","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers);
        execution.dispatch(&json!({"type":"message.dispatch","commandId":"m1","threadId":thread_id,"messageId":"m1","text":"[hold] spawn","attachments":[],"dispatchMode":{"type":"start_immediately"}}), Utc::now()).unwrap();
        let wait = |check: Box<dyn Fn(&Value) -> bool>| {
            let store = store.clone();
            let thread_id = thread_id.clone();
            async move {
                let mut events = store.subscribe();
                tokio::time::timeout(std::time::Duration::from_secs(30), async {
                    loop {
                        let view = store.projection("thread", &thread_id).unwrap().unwrap();
                        if check(&view) {
                            return view;
                        }
                        let _ = events.recv().await;
                    }
                })
                .await
                .expect("timed out waiting for a persisted event")
            }
        };
        let view = wait(Box::new(|view| view["runs"][0]["status"] == "running")).await;
        let run = view["runs"][0].clone();
        let now = now_text();
        // The parent's side: a subagent node, its row and its timeline item.
        let provider_thread = json!("provider-thread:child");
        let parent_thread = view["thread"].clone();
        let child = child_thread(&parent_thread, &json!({"childThreadId":"thread:child","parentNodeId":"node:subagent","activeProviderThreadId":provider_thread,"providerInstanceId":"codex","modelSelection":parent_thread["modelSelection"],"title":"Child","createdBy":"agent","creationSource":"provider"}), &now);
        ensure_child_thread(&store, &child).unwrap();
        ensure_child_thread(&store, &child).unwrap();
        let native = json!({"driver":"codex","nativeId":"native-sub","strength":"strong"});
        let node = json!({"id":"node:subagent","threadId":thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"rootNodeId":run["rootNodeId"],"kind":"subagent","status":"running","countsForRun":false,"providerThreadId":run["providerThreadId"],"providerTurnId":null,"nativeItemRef":native,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":now,"completedAt":null});
        let task = json!({"id":"node:subagent","threadId":thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"origin":"provider_native","createdBy":"agent","driver":"codex","providerInstanceId":"codex","providerThreadId":provider_thread,"childThreadId":"thread:child","nativeTaskRef":native,"prompt":"Read package.json","title":null,"model":null,"status":"running","result":null,"startedAt":now,"completedAt":null,"updatedAt":now});
        let item = json!({"id":"turn-item:subagent","threadId":thread_id,"runId":run["id"],"nodeId":"node:subagent","providerThreadId":run["providerThreadId"],"providerTurnId":null,"nativeItemRef":native,"parentItemId":null,"ordinal":50,"status":"running","title":null,"startedAt":now,"completedAt":null,"updatedAt":now,"type":"subagent","subagentId":"node:subagent","origin":"provider_native","driver":"codex","providerInstanceId":"codex","childThreadId":"thread:child","prompt":"Read package.json","result":null});
        commit(&store, &thread_id, |_| Ok(vec![("node.updated", node), ("subagent.updated", task), ("turn-item.updated", item)])).unwrap();
        // The child's side: its provider thread, a running root node and a streaming answer.
        let child_node = json!({"id":"node:child-root","threadId":"thread:child","runId":null,"parentNodeId":null,"rootNodeId":"node:child-root","kind":"root_turn","status":"running","countsForRun":false,"providerThreadId":provider_thread,"providerTurnId":null,"nativeItemRef":native,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":now,"completedAt":null});
        let (_, mut answer) = conversation_artifacts(&json!({"messageId":"message:answer","turnItemId":"turn-item:answer","threadId":"thread:child","rootNodeId":"node:child-root","providerThreadId":provider_thread,"providerTurnId":null,"nativeItemRef":null,"role":"assistant","text":"partial","ordinal":1}), &now);
        answer["status"] = json!("running");
        answer["streaming"] = json!(true);
        answer["completedAt"] = Value::Null;
        commit(&store, "thread:child", |_| Ok(vec![("node.updated", child_node), ("turn-item.updated", answer)])).unwrap();

        let child_view = store.projection("thread", "thread:child").unwrap().unwrap();
        assert_eq!((child_view["thread"]["lineage"]["relationshipToParent"].as_str(), child_view["thread"]["lineage"]["parentThreadId"].as_str()), (Some("subagent"), Some(thread_id.as_str())));
        assert!(child_view["runs"].as_array().unwrap().is_empty());
        let refused = execution.dispatch(&json!({"type":"message.dispatch","commandId":"to-child","threadId":"thread:child","messageId":"to-child","text":"hello","attachments":[],"dispatchMode":{"type":"start_immediately"}}), Utc::now());
        assert!(matches!(refused, Err(StoreError::InvalidCommand(reason)) if reason == READ_ONLY_REASON));

        execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop","threadId":thread_id,"runId":run["id"]}), Utc::now()).unwrap();
        let view = wait(Box::new(|view| view["runs"][0]["status"] == "interrupted")).await;
        execution.drained(&thread_id).await;
        let view = if view["subagents"][0]["status"] == "interrupted" { view } else { wait(Box::new(|view| view["subagents"][0]["status"] == "interrupted")).await };
        assert_eq!(view["subagents"][0]["status"], "interrupted");
        assert!(view["subagents"][0]["completedAt"].is_string());
        let timeline = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "subagent").unwrap();
        assert_eq!(timeline["status"], "interrupted");
        let child_view = store.projection("thread", "thread:child").unwrap().unwrap();
        assert_eq!(child_view["nodes"][0]["status"], "interrupted");
        let answer = child_view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "assistant_message").unwrap();
        assert_eq!((answer["status"].as_str(), answer["streaming"].as_bool()), (Some("interrupted"), Some(false)));
        execution.shutdown().await;
    }
}
