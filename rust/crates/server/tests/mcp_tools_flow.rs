#![cfg(unix)]
//! The orchestrator tools over the real MCP HTTP transport with a thread-scoped credential, a real
//! store and execution service, and the scripted fake Codex app-server
//! (`fixtures/codex-provider.py`, an absolute path inside the test's temp dir). Tests wait on
//! persisted events, never on sleeps.
mod mcp_support;
use chrono::Utc;
use mcp_support::*;
use serde_json::{Value, json};
use std::sync::Arc;
use t3_server::secret_requests::SecretRequests;

#[tokio::test(flavor = "multi_thread")]
async fn the_catalog_lists_every_original_tool_and_a_tool_without_a_service_says_so() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;
    let listed = h.rpc(&caller, "tools/list", json!({})).await["result"]["tools"].clone();
    let names: Vec<&str> = listed.as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 76, "the original registers 80 tools; the four device tools are the device service's");
    for handled in ["t3_thread_launch", "t3_thread_send", "t3_thread_update", "schedule_task", "request_secret", "run_scheduled_task_now"] {
        assert!(names.contains(&handled), "{handled}");
    }
    let refused = h.refused(&caller, "t3_queue_list", json!({"threadId":parent})).await;
    assert_eq!(refused, json!({"_tag":"OrchestratorMcpFailure","code":"orchestration_error","message":"The operation could not be completed."}));
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn launching_messaging_and_renaming_threads_runs_the_same_services_as_the_ui() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;

    // Launch: a new top-level thread whose first turn reaches the provider, attributed to the agent.
    let launched = h.ok(&caller, "t3_thread_launch", json!({"title":"Child work","message":"[hold] child task","spoofed":true})).await;
    let child = launched["threadId"].as_str().unwrap().to_owned();
    assert_eq!(launched["projectId"], "project");
    assert_eq!(launched["modelSelection"]["model"], "fixture-model");
    assert!(launched["link"].as_str().unwrap().starts_with("[Child work](t3-thread://v1/environment-1/"), "{launched}");
    h.until(&child, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "running")).then_some(())).await;
    let view = h.view(&child);
    assert_eq!(view["thread"]["createdBy"], "agent");
    assert_eq!(view["thread"]["creationSource"], "mcp");
    assert_eq!(view["thread"]["runtimeMode"], "full-access");
    assert_eq!(view["messages"][0]["senderThreadId"], "parent");
    assert!(h.turn_texts("turn/start").iter().any(|text| text.contains("child task")), "{:?}", h.turn_texts("turn/start"));

    // Send: steers the running turn, which the fake finishes.
    let sent = h.ok(&caller, "t3_thread_send", json!({"threadId":child,"message":"also this","clientRequestId":"steer-1"})).await;
    assert_eq!(sent["delivery"], "steered");
    h.until(&child, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().all(|run| run["status"] == "completed")).then_some(())).await;
    let message = h.view(&child)["messages"].as_array().unwrap().iter().find(|message| message["text"] == "also this").unwrap().clone();
    assert_eq!((message["createdBy"].as_str(), message["creationSource"].as_str(), message["senderThreadId"].as_str()), (Some("agent"), Some("mcp"), Some("parent")));
    assert!(h.provider_requests().iter().any(|request| request["method"] == "turn/steer"));
    // The same request id finds the same message.
    let again = h.ok(&caller, "t3_thread_send", json!({"threadId":child,"message":"also this","clientRequestId":"steer-1"})).await;
    assert_eq!(again["messageId"], sent["messageId"]);

    // Send to an idle thread starts a turn.
    let started = h.ok(&caller, "t3_thread_send", json!({"threadId":child,"message":"second turn"})).await;
    assert_eq!(started["delivery"], "started");
    h.until(&child, |view| (view["runs"].as_array().unwrap().len() == 2 && view["runs"][1]["status"] == "completed").then_some(())).await;
    assert!(h.turn_texts("turn/start").iter().any(|text| text.contains("second turn")));

    // Update: rename, and read it back.
    let renamed = h.ok(&caller, "t3_thread_update", json!({"threadId":child,"action":"rename","title":"Renamed child","clientRequestId":"rename-1"})).await;
    assert_eq!(renamed["title"], "Renamed child");
    assert_eq!(h.view(&child)["thread"]["title"], "Renamed child");

    // Refusals: a thread above the caller's modes, and a caller that stopped.
    let weak = h.running_thread("weak", "approval-required", "plan").await;
    let weak_caller = h.caller(&weak).await;
    let before = h.threads().len();
    let escalation = h.refused(&weak_caller, "t3_thread_launch", json!({"title":"Too much","runtimeMode":"full-access","message":"x"})).await;
    assert_eq!(escalation["code"], "runtime_mode_escalation_denied");
    let interaction = h.refused(&weak_caller, "t3_thread_launch", json!({"title":"Too much","interactionMode":"default","message":"x"})).await;
    assert_eq!(interaction["code"], "interaction_mode_escalation_denied");
    assert_eq!(h.threads().len(), before, "a refused launch creates nothing");
    let above = h.refused(&weak_caller, "t3_thread_send", json!({"threadId":parent,"message":"nope"})).await;
    assert_eq!(above["code"], "runtime_mode_escalation_denied");
    let renamed_above = h.refused(&weak_caller, "t3_thread_update", json!({"threadId":parent,"action":"rename","title":"Nope"})).await;
    assert_eq!(renamed_above["code"], "runtime_mode_escalation_denied");
    let invalid = h.rpc(&caller, "tools/call", json!({"name":"t3_thread_launch","arguments":{"title":"  "}})).await;
    assert!(invalid["error"]["message"].as_str().unwrap().contains("Expected a non-blank string"), "{invalid}");

    let stopped = h.running_thread("stopped", "full-access", "default").await;
    let stopped_caller = h.caller(&stopped).await;
    h.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop-it","threadId":stopped,"runId":h.view(&stopped)["runs"][0]["id"]}), Utc::now()).unwrap();
    h.until(&stopped, |view| (view["runs"][0]["status"] == "interrupted").then_some(())).await;
    let inactive = h.refused(&stopped_caller, "t3_thread_launch", json!({"title":"Late","message":"x"})).await;
    assert_eq!(inactive["code"], "parent_not_active");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduled_task_tools_create_list_update_run_and_delete_through_the_scheduler() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;

    let created = h.ok(&caller, "schedule_task", json!({"prompt":"Check the build\nthen report","schedule":{"type":"interval","everyMs":3_600_000},"clientRequestId":"daily"})).await;
    let id = created["scheduledTaskId"].as_str().unwrap().to_owned();
    assert_eq!((created["title"].as_str(), created["boundThreadId"].as_str(), created["enabled"].as_bool()), (Some("Check the build"), Some("parent"), Some(true)));
    // The same request id is the same task.
    let retried = h.ok(&caller, "schedule_task", json!({"prompt":"Check the build\nthen report","schedule":{"type":"interval","everyMs":3_600_000},"clientRequestId":"daily"})).await;
    assert_eq!(retried["scheduledTaskId"], created["scheduledTaskId"]);
    let listed = h.ok(&caller, "list_scheduled_tasks", json!({})).await;
    assert_eq!(listed["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(listed["tasks"][0]["scheduledTaskId"], id.as_str());

    let updated = h.ok(&caller, "update_scheduled_task", json!({"scheduledTaskId":id,"prompt":"Check everything","enabled":false})).await;
    assert_eq!((updated["prompt"].as_str(), updated["enabled"].as_bool()), (Some("Check everything"), Some(false)));
    let unbound = h.ok(&caller, "update_scheduled_task", json!({"scheduledTaskId":id,"bindToCurrentThread":false,"enabled":true})).await;
    assert_eq!(unbound["boundThreadId"], Value::Null);
    let stored = serde_json::to_value(t3_server::scheduled_tasks::ScheduledTasks::of(&h.store).unwrap().list().await.unwrap().tasks).unwrap();
    assert_eq!(stored[0]["workspaceStrategy"]["type"], "worktree");
    h.ok(&caller, "update_scheduled_task", json!({"scheduledTaskId":id,"bindToCurrentThread":true})).await;

    // Running it now posts the prompt into the bound thread, behind the turn it is running.
    let ran = h.ok(&caller, "run_scheduled_task_now", json!({"taskId":id})).await;
    assert_eq!((ran["taskId"].as_str(), ran["threadId"].as_str(), ran["runCount"].as_u64()), (Some(id.as_str()), Some("parent"), Some(1)));
    let queued = h.until("parent", |view| view["messages"].as_array().unwrap().iter().find(|message| message["text"] == "Check everything").cloned()).await;
    assert_eq!((queued["createdBy"].as_str(), queued["creationSource"].as_str()), (Some("agent"), Some("mcp")));
    assert_eq!(h.view("parent")["runs"][1]["status"], "queued");

    // The caller may not widen what a task runs at, and running one needs full access.
    let weak = h.running_thread("weak", "approval-required", "default").await;
    let weak_caller = h.caller(&weak).await;
    assert_eq!(h.refused(&weak_caller, "update_scheduled_task", json!({"scheduledTaskId":id,"enabled":false})).await["code"], "runtime_mode_escalation_denied");
    assert_eq!(h.refused(&weak_caller, "delete_scheduled_task", json!({"scheduledTaskId":id})).await["code"], "runtime_mode_escalation_denied");
    assert_eq!(h.refused(&weak_caller, "run_scheduled_task_now", json!({"taskId":id})).await["code"], "capability_denied");
    assert_eq!(h.refused(&caller, "run_scheduled_task_now", json!({"taskId":"missing"})).await["code"], "invalid_request");
    assert_eq!(h.refused(&caller, "update_scheduled_task", json!({"scheduledTaskId":"missing"})).await["code"], "task_not_found");
    let schedule_for_nothing = h.refused(&caller, "schedule_task", json!({"projectId":"nowhere","prompt":"x","schedule":{"type":"interval","everyMs":3_600_000}})).await;
    assert_eq!(schedule_for_nothing["code"], "invalid_request");

    let deleted = h.ok(&caller, "delete_scheduled_task", json!({"scheduledTaskId":id})).await;
    assert_eq!(deleted, json!({"scheduledTaskId":id,"deleted":true}));
    assert!(h.ok(&caller, "list_scheduled_tasks", json!({})).await["tasks"].as_array().unwrap().is_empty());
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_requested_secret_reaches_only_the_tool_that_takes_its_ref() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = Arc::new(h.caller(&parent).await);
    let secrets = SecretRequests::for_store(&h.store).unwrap();
    let value = "ghp_correct-horse-battery-staple";

    // The request appears as a waiting card carrying the question and no value.
    let h = Arc::new(h);
    let waiting = tokio::spawn({
        let (h, caller) = (h.clone(), caller.clone());
        async move { h.ok(&caller, "request_secret", json!({"label":"GitHub webhook secret","reason":"Signs deliveries","placeholder":"Paste it","clientRequestId":"ask-1"})).await }
    });
    let card = h.until(&parent, |view| view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "secret_request").cloned()).await;
    assert_eq!((card["secretStatus"].as_str(), card["status"].as_str(), card["label"].as_str(), card["placeholder"].as_str()), (Some("pending"), Some("waiting"), Some("GitHub webhook secret"), Some("Paste it")));
    // A retry of the same request finds the same card.
    let retry = tokio::spawn({
        let (h, caller) = (h.clone(), caller.clone());
        async move { h.rpc(&caller, "tools/call", json!({"name":"request_secret","arguments":{"label":"GitHub webhook secret","reason":"Signs deliveries","clientRequestId":"ask-1"}})).await }
    });
    // The user answers through secrets.answerRequest's service.
    secrets.answer(&parent, card["id"].as_str().unwrap(), &json!({"type":"save","secret":value})).unwrap();
    let answered = waiting.await.unwrap();
    assert_eq!(answered["status"], "saved");
    let reference = answered["secretRef"].as_str().unwrap().to_owned();
    assert!(!answered.to_string().contains(value));
    let retried = retry.await.unwrap();
    assert!(!retried.to_string().contains(value));
    let cards = h.view(&parent)["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "secret_request").count();
    assert_eq!(cards, 1);
    assert_eq!(h.view(&parent)["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "secret_request").unwrap()["secretStatus"], "saved");

    // The value is in none of the stored events or projections, nor in what the tool call returned.
    assert!(!h.stored_text().contains(value));
    // schedule_task takes the ref and consumes it: one use.
    let task = h.ok(&caller, "schedule_task", json!({"prompt":"Handle push","schedule":{"type":"webhook","signature":{"header":"x-hub-signature-256","encoding":"hex","prefix":"sha256=","secretRef":reference}}})).await;
    assert_eq!(task["webhookSignature"], "set");
    assert!(!task.to_string().contains(value));
    let second = h.refused(&caller, "schedule_task", json!({"prompt":"Handle push again","schedule":{"type":"webhook","signature":{"header":"x-hub-signature-256","encoding":"hex","prefix":"sha256=","secretRef":reference}}})).await;
    assert_eq!(second["code"], "orchestration_error");
    assert!(second["message"].as_str().unwrap().contains("secretRef"), "{second}");
    assert_eq!(secrets.saved_ref(&parent, card["id"].as_str().unwrap()), None, "the stored value is gone after its one use");
    assert!(!h.stored_text().contains(value));

    // Declining and a timeout end the call without a ref.
    let declining = tokio::spawn({
        let (h, caller) = (h.clone(), caller.clone());
        async move { h.ok(&caller, "request_secret", json!({"label":"Another","reason":"Second ask","clientRequestId":"ask-2"})).await }
    });
    let second_card = h.until(&parent, |view| view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "secret_request" && item["label"] == "Another").cloned()).await;
    secrets.answer(&parent, second_card["id"].as_str().unwrap(), &json!({"type":"decline"})).unwrap();
    assert_eq!(declining.await.unwrap(), json!({"status":"declined"}));
    let timed_out = h.ok(&caller, "request_secret", json!({"label":"Slow","reason":"Nobody answers","timeoutMs":1000,"clientRequestId":"ask-3"})).await;
    assert_eq!(timed_out, json!({"status":"timed_out"}));
    let closed = h.view(&parent)["turnItems"].as_array().unwrap().iter().find(|item| item["label"] == "Slow").unwrap().clone();
    assert_eq!((closed["secretStatus"].as_str(), closed["status"].as_str()), (Some("cancelled"), Some("cancelled")));
    let blank = h.rpc(&caller, "tools/call", json!({"name":"request_secret","arguments":{"label":" ","reason":"x"}})).await;
    assert!(blank["error"]["message"].as_str().unwrap().contains("Expected a non-blank string"), "{blank}");

    // A run that ended cannot be answered, so its call ends cancelled.
    let ending = tokio::spawn({
        let (h, caller) = (h.clone(), caller.clone());
        async move { h.ok(&caller, "request_secret", json!({"label":"Doomed","reason":"The run stops","clientRequestId":"ask-4"})).await }
    });
    h.until(&parent, |view| view["turnItems"].as_array().unwrap().iter().find(|item| item["label"] == "Doomed" && item["secretStatus"] == "pending").cloned()).await;
    h.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop-parent","threadId":parent,"runId":h.view(&parent)["runs"][0]["id"]}), Utc::now()).unwrap();
    assert_eq!(ending.await.unwrap(), json!({"status":"cancelled"}));
    let late = secrets.answer(&parent, h.view(&parent)["turnItems"].as_array().unwrap().iter().find(|item| item["label"] == "Doomed").unwrap()["id"].as_str().unwrap(), &json!({"type":"save","secret":"late"}));
    assert!(late.is_err());
    Arc::try_unwrap(h).ok().unwrap().finish().await;
}

fn subagent(view: &Value) -> Value {
    view["subagents"].as_array().unwrap().iter().find(|task| task["origin"] == "app_owned").cloned().unwrap_or(Value::Null)
}
fn wake_messages(view: &Value) -> Vec<Value> {
    view["messages"].as_array().unwrap().iter().filter(|message| message["delegatedCompletion"].is_object()).cloned().collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_task_runs_in_a_child_thread_and_its_completion_wakes_the_parent() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;

    let spawned = h.ok(&caller, "delegate_task", json!({"task":"[hold] summarize the repo","role":"research","title":"Repo summary","clientRequestId":"round-1"})).await;
    let task_id = spawned["taskId"].as_str().unwrap().to_owned();
    let child = spawned["childThreadId"].as_str().unwrap().to_owned();
    assert_eq!((spawned["status"].as_str(), spawned["workState"].as_str(), spawned["providerInstanceId"].as_str()), (Some("running"), Some("working"), Some("codex")));
    // The same request finds the same task.
    assert_eq!(h.ok(&caller, "delegate_task", json!({"task":"[hold] summarize the repo","role":"research","title":"Repo summary","clientRequestId":"round-1"})).await["taskId"], task_id.as_str());

    // Persisted: the parent's task row, node and timeline item; the child thread, its message, run and spawn transfer.
    let view = h.view(&parent);
    let task = subagent(&view);
    assert_eq!((task["id"].as_str(), task["status"].as_str(), task["completionWake"].as_str(), task["childThreadId"].as_str()), (Some(task_id.as_str()), Some("running"), Some("always"), Some(child.as_str())));
    assert!(view["nodes"].as_array().unwrap().iter().any(|node| node["id"] == task_id.as_str() && node["kind"] == "subagent"));
    assert!(view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "subagent" && item["subagentId"] == task_id.as_str() && item["status"] == "running"));
    h.until(&child, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "running")).then_some(())).await;
    let child_view = h.view(&child);
    assert_eq!((child_view["thread"]["lineage"]["relationshipToParent"].as_str(), child_view["thread"]["createdBy"].as_str(), child_view["thread"]["creationSource"].as_str(), child_view["thread"]["runtimeMode"].as_str()), (Some("subagent"), Some("agent"), Some("mcp"), Some("full-access")));
    assert_eq!(child_view["messages"][0]["text"], "Act as the research sub-agent for this task.\n\n[hold] summarize the repo");
    assert_eq!(child_view["messages"][0]["senderThreadId"], "parent");
    assert_eq!(child_view["contextTransfers"][0]["type"], "subagent_spawn");
    assert!(h.turn_texts("turn/start").iter().any(|text| text.contains("summarize the repo")));
    // The child sees only the task, not the parent's conversation.
    assert!(!h.turn_texts("turn/start").iter().any(|text| text.contains("summarize the repo") && text.contains("first")));

    // While running, task_status says so and a child's thread is a normal target for t3_thread_send.
    let running = h.ok(&caller, "task_status", json!({"taskId":task_id})).await;
    assert_eq!((running["status"].as_str(), running["workState"].as_str(), running["summary"].is_null()), (Some("running"), Some("working"), true));

    // The child finishes (its turn is steered to a close): the result reaches the parent and wakes it.
    h.ok(&caller, "t3_thread_send", json!({"threadId":child,"message":"wrap up"})).await;
    let finished = h.until(&parent, |view| (subagent(view)["status"] == "completed").then(|| subagent(view))).await;
    assert!(finished["result"].as_str().unwrap().contains("summarize the repo"), "{finished}");
    assert_eq!(finished["completionDelivery"]["state"], "claimed");
    let parent_view = h.view(&parent);
    assert!(parent_view["contextTransfers"].as_array().unwrap().iter().any(|transfer| transfer["type"] == "subagent_result" && transfer["sourceThreadId"] == child.as_str()));
    assert!(parent_view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "subagent" && item["status"] == "completed"));
    // The wake waits behind the parent's active turn, and says which task finished.
    let wakes = wake_messages(&h.view(&parent));
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0]["text"], format!("Delegated task {task_id} reached a terminal state. Use task_status with taskId {task_id} to read the result."));
    assert_eq!((wakes[0]["createdBy"].as_str(), wakes[0]["creationSource"].as_str()), (Some("agent"), Some("server")));
    let wake_run = h.view(&parent)["runs"].as_array().unwrap().iter().find(|run| run["userMessageId"] == wakes[0]["id"]).unwrap().clone();
    assert_eq!(wake_run["status"], "queued");

    // The parent's turn ends: the wake starts, reaches the provider, and delivers the task.
    h.ok(&caller, "t3_thread_send", json!({"threadId":parent,"message":"finish the turn","mode":"steer"})).await;
    let delivered = h.until(&parent, |view| (subagent(view)["completionDelivery"]["state"] == "delivered").then(|| view.clone())).await;
    assert!(h.turn_texts("turn/start").iter().any(|text| text.contains(&format!("Delegated task {task_id} reached a terminal state"))), "{:?}", h.turn_texts("turn/start"));
    let cohort = delivered["runs"][0]["delegatedCompletion"].clone();
    assert_eq!((cohort["disposition"].as_str(), cohort["delivery"].is_null(), cohort["nextGeneration"].as_u64()), (Some("open"), true, Some(2)));

    // A later turn reads the terminal result, which acknowledges it. Once no turn is live, the tool refuses.
    let refused = h.refused(&caller, "task_status", json!({"taskId":task_id})).await;
    assert_eq!(refused["code"], "parent_not_active");
    h.execution.dispatch(&json!({"type":"message.dispatch","commandId":"next-turn","threadId":parent,"messageId":"next-turn","text":"[hold] read the result","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}}), Utc::now()).unwrap();
    h.until(&parent, |view| (view["runs"].as_array().unwrap().last().unwrap()["status"] == "running").then_some(())).await;
    let done = h.ok(&caller, "task_status", json!({"taskId":task_id})).await;
    assert_eq!((done["status"].as_str(), done["workState"].as_str(), done["childRunId"].is_null()), (Some("completed"), Some("result_available"), false));
    assert!(done["summary"].as_str().unwrap().contains("summarize the repo"));
    assert!(done["resultContextTransferId"].is_string());
    assert_eq!(subagent(&h.view(&parent))["completionDelivery"]["state"], "acknowledged");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_a_result_cancels_its_queued_wake_and_a_failed_child_is_reported() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;

    // A child that ends interrupted reports its status, with the reason as its summary.
    let spawned = h.ok(&caller, "delegate_task", json!({"task":"[hold] doomed work"})).await;
    let (task_id, child) = (spawned["taskId"].as_str().unwrap().to_owned(), spawned["childThreadId"].as_str().unwrap().to_owned());
    h.until(&child, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "running")).then_some(())).await;
    h.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop-child","threadId":child,"runId":h.view(&child)["runs"][0]["id"]}), Utc::now()).unwrap();
    h.until(&parent, |view| (subagent(view)["status"] == "interrupted").then_some(())).await;
    let wakes = wake_messages(&h.view(&parent));
    assert_eq!(wakes.len(), 1);
    assert_eq!(h.view(&parent)["runs"][1]["status"], "queued");
    let status = h.ok(&caller, "task_status", json!({"taskId":task_id})).await;
    assert_eq!((status["status"].as_str(), status["workState"].as_str(), status["latestTerminalStatus"].as_str()), (Some("interrupted"), Some("result_available"), Some("interrupted")));
    // Reading it acknowledged the result: the wake nobody needs is cancelled and the cohort has no delivery.
    let after = h.view(&parent);
    assert_eq!(after["runs"][1]["status"], "cancelled");
    assert_eq!(after["runs"][0]["delegatedCompletion"]["delivery"], Value::Null);
    assert_eq!(subagent(&after)["completionDelivery"]["state"], "acknowledged");

    // Refusals: another parent's task, an unknown task, and children above the parent's modes.
    let other = h.running_thread("other", "full-access", "default").await;
    let other_caller = h.caller(&other).await;
    assert_eq!(h.refused(&other_caller, "task_status", json!({"taskId":task_id})).await["code"], "task_not_found");
    assert_eq!(h.refused(&caller, "task_status", json!({"taskId":"missing"})).await["code"], "task_not_found");
    assert_eq!(h.refused(&other_caller, "task_cancel", json!({"taskId":task_id})).await["code"], "task_not_found");
    let weak = h.running_thread("weak", "approval-required", "plan").await;
    let weak_caller = h.caller(&weak).await;
    let before = h.threads().len();
    assert_eq!(h.refused(&weak_caller, "delegate_task", json!({"task":"x","runtimeMode":"full-access"})).await["code"], "runtime_mode_escalation_denied");
    assert_eq!(h.refused(&weak_caller, "delegate_task", json!({"task":"x","interactionMode":"default"})).await["code"], "interaction_mode_escalation_denied");
    assert_eq!(h.threads().len(), before, "a refused delegation creates nothing");
    let unknown = h.refused(&caller, "delegate_task", json!({"task":"x","target":{"providerInstanceId":"nowhere"}})).await;
    assert_eq!(unknown["code"], "provider_unavailable");
    let model = h.refused(&caller, "delegate_task", json!({"task":"x","target":{"model":"not-a-model"}})).await;
    assert!(model["code"] == "model_unavailable" || model["code"] == "provider_unavailable", "{model}");
    // A child cannot be started from a turn that is over.
    h.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop-weak","threadId":weak,"runId":h.view(&weak)["runs"][0]["id"]}), Utc::now()).unwrap();
    h.until(&weak, |view| (view["runs"][0]["status"] == "interrupted").then_some(())).await;
    assert_eq!(h.refused(&weak_caller, "delegate_task", json!({"task":"x"})).await["code"], "parent_not_active");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_task_stops_its_child_and_stopping_the_parent_drops_the_wake_it_was_owed() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;

    let spawned = h.ok(&caller, "delegate_task", json!({"task":"[hold] long work"})).await;
    let (task_id, child) = (spawned["taskId"].as_str().unwrap().to_owned(), spawned["childThreadId"].as_str().unwrap().to_owned());
    h.until(&child, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "running")).then_some(())).await;
    let cancelled = h.ok(&caller, "task_cancel", json!({"taskId":task_id,"reason":"not needed","clientRequestId":"cancel-1"})).await;
    assert_eq!(cancelled, json!({"taskId":task_id,"status":"cancel_requested"}));
    h.until(&child, |view| (view["runs"][0]["status"] == "interrupted").then_some(())).await;
    assert!(h.provider_requests().iter().any(|request| request["method"] == "turn/interrupt"));
    // The cancelled task owes the parent no wake.
    let done = h.until(&parent, |view| (subagent(view)["status"] == "interrupted").then(|| subagent(view))).await;
    assert_eq!(done["completionDelivery"]["state"], "disposed");
    assert!(wake_messages(&h.view(&parent)).is_empty());
    // Cancelling a terminal task answers with its status and a retry is the same answer.
    let again = h.ok(&caller, "task_cancel", json!({"taskId":task_id,"clientRequestId":"cancel-1"})).await;
    assert_eq!(again, json!({"taskId":task_id,"status":"interrupted"}));

    // Stopping the parent with a task outstanding ends the cohort: the finished child is never delivered.
    let second = h.ok(&caller, "delegate_task", json!({"task":"[hold] more work"})).await;
    let (second_id, second_child) = (second["taskId"].as_str().unwrap().to_owned(), second["childThreadId"].as_str().unwrap().to_owned());
    h.until(&second_child, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "running")).then_some(())).await;
    h.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop-parent","threadId":parent,"runId":h.view(&parent)["runs"][0]["id"],"holdQueue":true}), Utc::now()).unwrap();
    let stopped = h.until(&parent, |view| (view["runs"][0]["status"] == "interrupted").then(|| view.clone())).await;
    assert_eq!(stopped["runs"][0]["delegatedCompletion"]["disposition"], "stopped");
    // The child still finishes and its result is kept, but nothing wakes the parent.
    h.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop-second","threadId":second_child,"runId":h.view(&second_child)["runs"][0]["id"]}), Utc::now()).unwrap();
    let finished = h.until(&parent, |view| view["subagents"].as_array().unwrap().iter().find(|task| task["id"] == second_id.as_str() && task["status"] == "interrupted").cloned()).await;
    assert_eq!(finished["completionDelivery"]["state"], "disposed");
    assert!(wake_messages(&h.view(&parent)).is_empty());
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_delegation_returns_the_result_or_hands_delivery_back_when_it_times_out() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = Arc::new(h.caller(&parent).await);
    let h = Arc::new(h);

    // A short wait times out without cancelling the child, and upgrades the task to wake the parent.
    let timed_out = h.ok(&caller, "delegate_task", json!({"task":"[hold] slow work","mode":"wait","timeoutMs":1,"clientRequestId":"slow"})).await;
    assert_eq!((timed_out["waitTimedOut"].as_bool(), timed_out["status"].as_str()), (Some(true), Some("running")));
    let task = h.view(&parent)["subagents"][0].clone();
    assert_eq!(task["completionWake"], "always");

    // A wait that sees the child finish returns its result without a wake being queued for it.
    let waiting = tokio::spawn({
        let (h, caller) = (h.clone(), caller.clone());
        async move { h.ok(&caller, "delegate_task", json!({"task":"[hold] quick work","mode":"wait","timeoutMs":60000,"clientRequestId":"quick"})).await }
    });
    let child = h.until(&parent, |view| view["subagents"].as_array().unwrap().iter().find(|task| task["prompt"] == "[hold] quick work").map(|task| task["childThreadId"].as_str().unwrap().to_owned())).await;
    h.until(&child, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "running")).then_some(())).await;
    h.ok(&caller, "t3_thread_send", json!({"threadId":child,"message":"finish"})).await;
    let result = waiting.await.unwrap();
    assert_eq!((result["status"].as_str(), result["waitTimedOut"].as_bool(), result["workState"].as_str()), (Some("completed"), Some(false), Some("result_available")));
    assert!(result["summary"].as_str().unwrap().contains("quick work"));
    let quick = h.view(&parent)["subagents"].as_array().unwrap().iter().find(|task| task["prompt"] == "[hold] quick work").unwrap().clone();
    assert_eq!((quick["completionWake"].as_str(), quick["completionDelivery"]["state"].as_str()), (Some("settled_only"), Some("acknowledged")));
    assert!(wake_messages(&h.view(&parent)).is_empty());
    Arc::try_unwrap(h).ok().unwrap().finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn create_threads_makes_ordinary_threads_in_the_callers_project_and_lists_providers() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;

    let capabilities = h.ok(&caller, "orchestrator_capabilities", json!({})).await;
    assert_eq!((capabilities["parentThreadId"].as_str(), capabilities["inheritedProviderInstanceId"].as_str(), capabilities["runtimeMode"].as_str()), (Some("parent"), Some("codex"), Some("full-access")));
    let codex = capabilities["providers"].as_array().unwrap().iter().find(|provider| provider["providerInstanceId"] == "codex").unwrap();
    assert_eq!((codex["canRunChildTask"].as_bool(), codex["models"][0]["id"].as_str()), (Some(true), Some("fixture-model")));
    let claude = capabilities["providers"].as_array().unwrap().iter().find(|provider| provider["providerInstanceId"] == "claudeAgent").unwrap();
    assert_eq!(claude["canRunChildTask"], false);
    assert!(claude["constraints"].as_array().unwrap().iter().any(|constraint| constraint == "Provider instance is disabled."), "{claude}");

    let created = h.ok(&caller, "create_threads", json!({"threads":[{"prompt":"[hold] first job","title":"First"},{"title":"Idle one"}],"clientRequestId":"batch-1"})).await;
    let threads = created["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!((threads[0]["title"].as_str(), threads[0]["createdBy"].as_str(), threads[0]["creationSource"].as_str(), threads[0]["model"].as_str()), (Some("First"), Some("agent"), Some("mcp"), Some("fixture-model")));
    assert_eq!((threads[1]["status"].as_str(), threads[1]["runId"].is_null()), (Some("idle"), true));
    let first = threads[0]["threadId"].as_str().unwrap().to_owned();
    h.until(&first, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "running")).then_some(())).await;
    assert!(h.turn_texts("turn/start").iter().any(|text| text.contains("first job")));
    let view = h.view(&first);
    assert_eq!((view["thread"]["projectId"].as_str(), view["thread"]["lineage"]["relationshipToParent"].is_null(), view["messages"][0]["senderThreadId"].as_str()), (Some("project"), true, Some("parent")));
    // Each is recorded in the parent's timeline, once.
    let cards = |view: &Value| view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "thread_created").count();
    assert_eq!(cards(&h.view(&parent)), 2);
    let retried = h.ok(&caller, "create_threads", json!({"threads":[{"prompt":"[hold] first job","title":"First"},{"title":"Idle one"}],"clientRequestId":"batch-1"})).await;
    assert_eq!(retried["threads"][0]["threadId"], threads[0]["threadId"]);
    assert_eq!(cards(&h.view(&parent)), 2);

    let weak = h.running_thread("weak", "approval-required", "default").await;
    let weak_caller = h.caller(&weak).await;
    let before = h.threads().len();
    assert_eq!(h.refused(&weak_caller, "create_threads", json!({"threads":[{"runtimeMode":"full-access"}]})).await["code"], "runtime_mode_escalation_denied");
    assert_eq!(h.threads().len(), before);
    assert_eq!(h.rpc(&caller, "tools/call", json!({"name":"create_threads","arguments":{"threads":[]}})).await["error"]["_tag"], "InvalidParams");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn project_reads_list_and_read_the_environments_projects() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "approval-required", "plan").await;
    let caller = h.caller(&parent).await;
    let listed = h.ok(&caller, "t3_project_list", json!({"limit":1})).await;
    assert_eq!((listed["projects"].as_array().unwrap().len(), listed["projects"][0]["id"].as_str(), listed["nextCursor"].is_null()), (1, Some("project"), true));
    let read = h.ok(&caller, "t3_project_read", json!({"projectId":"project"})).await;
    assert_eq!((read["id"].as_str(), read["title"].as_str()), (Some("project"), Some("Project")));
    assert_eq!(h.refused(&caller, "t3_project_read", json!({"projectId":"nowhere"})).await["message"], "The project was not found.");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_a_delegated_childs_result_acknowledges_its_delivery_but_a_truncated_read_does_not() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;
    let spawned = h.ok(&caller, "delegate_task", json!({"task":"[hold] write a long answer"})).await;
    let child = spawned["childThreadId"].as_str().unwrap().to_owned();
    h.until(&child, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "running")).then_some(())).await;
    h.ok(&caller, "t3_thread_send", json!({"threadId":child,"message":"finish"})).await;
    h.until(&parent, |view| (subagent(view)["completionDelivery"]["state"] == "claimed" && subagent(view)["status"] == "completed").then_some(())).await;
    // A read too short to hold the result does not acknowledge it.
    h.ok(&caller, "t3_thread_read", json!({"threadId":child,"maxCharsPerItem":5})).await;
    assert_eq!(subagent(&h.view(&parent))["completionDelivery"]["state"], "claimed");
    assert_eq!(wake_messages(&h.view(&parent)).len(), 1);
    // A read of the whole result does, and the wake nobody needs any more is cancelled.
    h.ok(&caller, "t3_thread_read", json!({"threadId":child})).await;
    let after = h.view(&parent);
    assert_eq!(subagent(&after)["completionDelivery"]["state"], "acknowledged");
    assert_eq!(after["runs"][1]["status"], "cancelled");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_codex_session_offers_mcp_apps_and_feedback_upload_until_it_ends() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let view = h.view(&parent);
    let session = view["providerSessions"][0]["id"].as_str().unwrap().to_owned();
    let provider_thread = view["providerThreads"][0].clone();
    let live = t3_server::provider_live::get(&session).expect("the running session is registered");
    assert_eq!(live.driver, "codex");
    let (host, upload) = (live.mcp_apps.expect("Codex offers MCP Apps"), live.upload_feedback.expect("Codex offers feedback upload"));
    // Each request reaches Codex as the original sends it, naming the native thread. The scripted fake
    // knows none of these methods and answers that, which comes back as the operation's failure.
    let native = provider_thread["nativeThreadRef"]["nativeId"].as_str().unwrap().to_owned();
    let error = host.read_resource(provider_thread.clone(), "docs".into(), "ui://docs/view".into()).await.unwrap_err();
    assert!(error.starts_with("Codex MCP resource read failed"), "{error}");
    assert!(host.call_tool(provider_thread.clone(), "docs".into(), "open".into(), json!({"page":1})).await.is_err());
    assert!(host.list_tools(provider_thread.clone(), "docs".into()).await.is_err());
    assert!(upload(provider_thread.clone(), Some("it broke".into())).await.is_err());
    let requests = h.provider_requests();
    let sent = |method: &str| requests.iter().find(|request| request["method"] == method).unwrap_or_else(|| panic!("{method} was not sent: {requests:?}"))["params"].clone();
    assert_eq!(sent("mcpServer/resource/read"), json!({"threadId":native,"server":"docs","uri":"ui://docs/view"}));
    assert_eq!(sent("mcpServer/tool/call"), json!({"threadId":native,"server":"docs","tool":"open","arguments":{"page":1}}));
    assert_eq!(sent("mcpServerStatus/list"), json!({"threadId":native,"serverName":"docs","detail":"toolsAndAuthOnly"}));
    assert_eq!(sent("feedback/upload"), json!({"classification":"bug","includeLogs":true,"reason":"it broke","threadId":native}));
    // Initialization declared MCP Apps support.
    assert_eq!(sent("initialize")["capabilities"]["extensions"], json!({"io.modelcontextprotocol/ui":{"mimeTypes":["text/html;profile=mcp-app"]}}));
    // Once the session ends it is gone: an app view never starts one.
    h.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop","threadId":parent,"runId":view["runs"][0]["id"]}), Utc::now()).unwrap();
    h.execution.shutdown().await;
    assert!(t3_server::provider_live::get(&session).is_none());
    h.http_only_finish().await;
}
