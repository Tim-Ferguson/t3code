#![cfg(unix)]
//! The thread-management tools over the real MCP HTTP transport with a thread-scoped credential, a
//! real store and execution service, and the scripted fake Codex app-server. Tests wait on
//! persisted events, never on sleeps.
mod mcp_support;
use mcp_support::*;
use serde_json::{Value, json};

fn runs(h: &Harness, thread: &str) -> Vec<Value> {
    h.view(thread)["runs"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn configuring_organizing_searching_and_reading_threads_runs_the_same_services_as_the_socket() {
    let h = Harness::with_instances(vec![], vec![("FIXTURE_UNUSED", "1".to_owned())]).await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;
    let sibling = h.ok(&caller, "t3_thread_launch", json!({"title":"Sibling"})).await["threadId"].as_str().unwrap().to_owned();

    // Configuration reads any thread of the environment; the caller's own when none is named.
    let own = h.ok(&caller, "t3_thread_configuration", json!({})).await;
    assert_eq!((own["threadId"].as_str(), own["modelSelection"]["model"].as_str(), own["runtimeMode"].as_str()), (Some("parent"), Some("fixture-model"), Some("full-access")));
    assert_eq!(h.ok(&caller, "t3_thread_configuration", json!({"threadId":sibling})).await["threadId"], sibling.as_str());
    assert_eq!(h.refused(&caller, "t3_thread_configuration", json!({"threadId":"missing"})).await, json!({"_tag":"OrchestratorMcpFailure","code":"thread_not_found","message":"The thread was not found."}));
    assert_eq!(h.ok(&caller, "t3_thread_transfers", json!({})).await, json!({"transfers":[]}));

    // Configure sets the selection through the thread service; another provider is not ported.
    let configured = h.ok(&caller, "t3_thread_configure", json!({"threadId":sibling,"modelSelection":{"instanceId":"codex","model":"other-model"}})).await;
    assert!(configured["sequence"].as_u64().unwrap() > 0);
    assert_eq!(h.view(&sibling)["thread"]["modelSelection"]["model"], "other-model");
    // Another provider instance is a switch, which the thread service allows while no run is active and
    // the tool sends as the socket does: both leave the same thread and the same event.
    let other = h.ok(&caller, "t3_thread_launch", json!({"title":"Switched by the socket"})).await["threadId"].as_str().unwrap().to_owned();
    let to_claude = json!({"instanceId":"codexChild","model":"claude-x"});
    h.ok(&caller, "t3_thread_configure", json!({"threadId":sibling,"modelSelection":to_claude})).await;
    t3_server::thread::ThreadService::new(h.store.clone()).dispatch(&json!({"type":"thread.model-selection.set","commandId":"socket-switch","threadId":other,"modelSelection":to_claude}), chrono::Utc::now()).unwrap();
    for thread in [&sibling, &other] {
        let view = h.view(thread);
        assert_eq!((view["thread"]["providerInstanceId"].as_str(), view["thread"]["modelSelection"].clone()), (Some("codexChild"), to_claude.clone()), "{thread}");
    }
    let switched_events = |thread: &str| h.store.read(|connection| Ok(connection.query_row("SELECT count(*) FROM rust_application_events WHERE aggregate_id=?1 AND event_type='thread.provider-switched'", [thread], |row| row.get::<_, u64>(0))?)).unwrap();
    assert_eq!((switched_events(&sibling), switched_events(&other)), (1, 1));
    // A provider instance that is not enabled is not a target, run in flight or not.
    let unavailable = h.refused(&caller, "t3_thread_configure", json!({"modelSelection":{"instanceId":"claudeAgent","model":"claude-x"}})).await;
    assert_eq!((unavailable["code"].as_str(), unavailable["message"].as_str()), (Some("orchestration_error"), Some("The target provider instance is unavailable.")));
    assert_eq!(h.view(&parent)["thread"]["providerInstanceId"], "codex");
    // Organize: each action is the thread command of the same name, attributed in the event log.
    h.ok(&caller, "t3_thread_organize", json!({"threadId":sibling,"action":"pin"})).await;
    assert!(!h.view(&sibling)["thread"]["pinnedAt"].is_null());
    h.ok(&caller, "t3_thread_organize", json!({"threadId":sibling,"action":"unpin"})).await;
    assert!(h.view(&sibling)["thread"]["pinnedAt"].is_null());
    assert_eq!(h.refused(&caller, "t3_thread_organize", json!({"threadId":sibling,"action":"snooze"})).await, json!({"_tag":"OrchestratorMcpFailure","code":"invalid_request","message":"snooze requires snoozedUntil."}));
    h.ok(&caller, "t3_thread_organize", json!({"threadId":sibling,"action":"snooze","snoozedUntil":"2099-01-01T00:00:00.000Z"})).await;
    assert_eq!(h.view(&sibling)["thread"]["snoozedUntil"], "2099-01-01T00:00:00.000Z");
    h.ok(&caller, "t3_thread_organize", json!({"threadId":sibling,"action":"unsnooze"})).await;
    assert!(h.view(&sibling)["thread"]["snoozedUntil"].is_null());
    h.ok(&caller, "t3_thread_organize", json!({"threadId":sibling,"action":"archive"})).await;
    assert!(!h.view(&sibling)["thread"]["archivedAt"].is_null());
    h.ok(&caller, "t3_thread_organize", json!({"threadId":sibling,"action":"unarchive"})).await;
    assert!(h.view(&sibling)["thread"]["archivedAt"].is_null());

    // Search finds the finished messages of the caller's own project unless another is named.
    let found = h.ok(&caller, "t3_thread_search", json!({"query":"hold"})).await;
    assert!(found["matches"].as_array().unwrap().iter().any(|found| found["threadId"] == "parent"), "{found}");
    assert!(h.ok(&caller, "t3_thread_search", json!({"query":"hold","projectId":"elsewhere"})).await["matches"].as_array().unwrap().is_empty());

    // A caller below the target's modes changes nothing about it.
    let weak = h.running_thread("weak", "approval-required", "plan").await;
    let weak_caller = h.caller(&weak).await;
    for (tool, arguments) in [
        ("t3_thread_organize", json!({"threadId":parent,"action":"pin"})),
        ("t3_thread_configure", json!({"threadId":parent,"modelSelection":{"instanceId":"codex","model":"x"}})),
        ("t3_queue_cancel", json!({"threadId":parent,"queuedRunId":"run"})),
        ("t3_pending_request_respond", json!({"threadId":parent,"requestId":"request","answers":{"a":"b"}})),
    ] {
        assert_eq!(h.refused(&weak_caller, tool, arguments).await["code"], "runtime_mode_escalation_denied", "{tool}");
    }
    assert!(h.view(&parent)["thread"]["pinnedAt"].is_null());
    // Reading is not writing: the same caller may read the thread above its own modes.
    assert_eq!(h.ok(&weak_caller, "t3_thread_configuration", json!({"threadId":parent})).await["runtimeMode"], "full-access");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_queue_tools_list_edit_reorder_cancel_and_promote_queued_messages() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;
    let running = runs(&h, &parent)[0]["id"].as_str().unwrap().to_owned();
    for text in ["queued one", "queued two", "queued three"] {
        assert_eq!(h.ok(&caller, "t3_thread_send", json!({"message":text,"threadId":parent,"mode":"queue"})).await["delivery"], "queued");
    }
    let ids = |listed: &Value| listed["items"].as_array().unwrap().iter().map(|item| item["queuedRunId"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    let listed = h.ok(&caller, "t3_queue_list", json!({})).await;
    assert_eq!(listed["nextCursor"], Value::Null);
    let queue = ids(&listed);
    assert_eq!(listed["items"].as_array().unwrap().iter().map(|item| item["text"].as_str().unwrap()).collect::<Vec<_>>(), ["queued one", "queued two", "queued three"]);
    let page = h.ok(&caller, "t3_queue_list", json!({"limit":2})).await;
    assert_eq!((ids(&page).len(), page["nextCursor"].as_u64()), (2, Some(2)));
    let long = "x".repeat(1200);
    let read = h.ok(&caller, "t3_queue_read", json!({"queuedRunId":queue[0]})).await;
    assert_eq!((read["text"].as_str(), read["truncated"].as_bool()), (Some("queued one"), Some(false)));
    assert_eq!(h.refused(&caller, "t3_queue_read", json!({"queuedRunId":"nope"})).await, json!({"_tag":"OrchestratorMcpFailure","code":"invalid_request","message":"The queued message was not found."}));

    // Edit changes the stored message; the list cuts long text and the read does not.
    h.ok(&caller, "t3_queue_edit", json!({"queuedRunId":queue[0],"text":long})).await;
    let cut = h.ok(&caller, "t3_queue_list", json!({})).await;
    assert_eq!((cut["items"][0]["text"].as_str().unwrap().chars().count(), cut["items"][0]["truncated"].as_bool()), (1000, Some(true)));
    assert_eq!(h.ok(&caller, "t3_queue_read", json!({"queuedRunId":queue[0]})).await["truncated"], false);
    h.ok(&caller, "t3_queue_edit", json!({"queuedRunId":queue[0],"text":"queued one"})).await;

    // Reorder moves one ahead of another, or to the end with null.
    h.ok(&caller, "t3_queue_reorder", json!({"queuedRunId":queue[2],"beforeRunId":queue[0]})).await;
    assert_eq!(ids(&h.ok(&caller, "t3_queue_list", json!({})).await), [queue[2].clone(), queue[0].clone(), queue[1].clone()]);
    h.ok(&caller, "t3_queue_reorder", json!({"queuedRunId":queue[2],"beforeRunId":null})).await;
    assert_eq!(ids(&h.ok(&caller, "t3_queue_list", json!({})).await), [queue[0].clone(), queue[1].clone(), queue[2].clone()]);

    // Cancel ends the run without it having started.
    h.ok(&caller, "t3_queue_cancel", json!({"queuedRunId":queue[2]})).await;
    let cancelled = runs(&h, &parent).into_iter().find(|run| run["id"] == queue[2].as_str()).unwrap();
    assert_eq!((cancelled["status"].as_str(), cancelled["startedAt"].is_null()), (Some("cancelled"), true));
    assert_eq!(h.refused(&caller, "t3_queue_cancel", json!({"queuedRunId":queue[2]})).await["code"], "orchestration_error");

    // Promote delivers the message into the running turn as steering and ends its queued run.
    h.ok(&caller, "t3_queue_promote_to_steer", json!({"queuedRunId":queue[1],"targetRunId":running})).await;
    let projection = h.view(&parent);
    let promoted = projection["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "user_message" && item["text"] == "queued two").unwrap();
    assert_eq!((promoted["inputIntent"].as_str(), promoted["runId"].as_str()), (Some("promoted_queued_to_steer"), Some(running.as_str())));
    let ended = runs(&h, &parent).into_iter().find(|run| run["id"] == queue[1].as_str()).unwrap();
    assert_eq!(ended["status"], "cancelled");
    // The provider is asked to steer, finishes the turn, and the message left in the queue then starts.
    h.until(&parent, |view| view["runs"].as_array().unwrap().iter().find(|run| run["id"] == queue[0].as_str()).filter(|run| run["status"] != "queued").map(|_| ())).await;
    assert!(h.provider_requests().iter().any(|request| request["method"] == "turn/steer"), "{:?}", h.provider_requests());
    // A message that is no longer queued cannot be promoted again.
    assert_eq!(h.refused(&caller, "t3_queue_promote_to_steer", json!({"queuedRunId":queue[1],"targetRunId":running})).await["code"], "orchestration_error");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_user_questions_are_read_and_answered_never_approvals() {
    let h = Harness::new().await;
    h.start_thread("asker", "fixture-model", "full-access", "ask [question]");
    h.start_thread("approver", "fixture-model", "full-access", "run [approval]");
    let pending = |thread: &str, kind: &'static str| {
        let thread = thread.to_owned();
        let h = &h;
        async move { h.until(&thread, |view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["kind"] == kind && request["status"] == "pending").map(|request| request["id"].as_str().unwrap().to_owned())).await }
    };
    let question = pending("asker", "user_input").await;
    let approval = h.until("approver", |view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").map(|request| request["id"].as_str().unwrap().to_owned())).await;
    // A caller outside both threads, at the same modes as the asker, reads the question.
    let reader = h.running_thread("reader", "full-access", "default").await;
    let caller = h.caller(&reader).await;
    assert_eq!(h.ok(&caller, "t3_pending_request_list", json!({"threadId":"asker"})).await, json!({"requestIds":[question]}));
    let read = h.ok(&caller, "t3_pending_request_read", json!({"threadId":"asker","requestId":question})).await;
    assert_eq!(read["requestId"], question.as_str());
    assert_eq!(read["questions"][0]["id"], "choice");
    assert_eq!(h.ok(&caller, "t3_pending_request_list", json!({"threadId":"approver"})).await, json!({"requestIds":[]}));
    let not_a_question = json!({"_tag":"OrchestratorMcpFailure","code":"invalid_request","message":"The pending user-input request was not found."});
    assert_eq!(h.refused(&caller, "t3_pending_request_read", json!({"threadId":"approver","requestId":approval})).await, not_a_question);
    assert_eq!(h.refused(&caller, "t3_pending_request_respond", json!({"threadId":"approver","requestId":approval,"answers":{"choice":"One"}})).await, not_a_question);
    assert!(h.view("approver")["runtimeRequests"].as_array().unwrap().iter().all(|request| request["status"] == "pending"), "the approval was left alone");

    // Answering for a thread above the caller's modes is refused, and leaves the question pending.
    let weak = h.running_thread("weak", "approval-required", "plan").await;
    let weak_caller = h.caller(&weak).await;
    assert_eq!(h.refused(&weak_caller, "t3_pending_request_respond", json!({"threadId":"asker","requestId":question,"answers":{"choice":"One"}})).await["code"], "runtime_mode_escalation_denied");
    assert!(h.view("asker")["runtimeRequests"].as_array().unwrap().iter().any(|request| request["id"] == question.as_str() && request["status"] == "pending"));

    // The permitted caller answers it: the same runtime response command the UI sends.
    h.ok(&caller, "t3_pending_request_respond", json!({"threadId":"asker","requestId":question,"answers":{"choice":"One"}})).await;
    h.until("asker", |view| view["runtimeRequests"].as_array().unwrap().iter().any(|request| request["id"] == question.as_str() && request["status"] != "pending").then_some(())).await;
    assert_eq!(h.refused(&caller, "t3_pending_request_read", json!({"threadId":"asker","requestId":question})).await, not_a_question);
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forking_a_finished_thread_and_merging_back_use_the_thread_commands() {
    let h = Harness::new().await;
    h.start_thread("done", "fixture-model", "full-access", "finish this");
    let finished = h.until("done", |view| view["runs"].as_array().unwrap().iter().find(|run| run["status"] == "completed").map(|run| run["id"].as_str().unwrap().to_owned())).await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;
    let forked = h.ok(&caller, "t3_thread_fork", json!({"threadId":"done","sourcePoint":{"type":"run","runId":finished},"title":"Forked work"})).await;
    let fork = forked["targetThreadId"].as_str().unwrap().to_owned();
    assert!(fork.ends_with(":fork") && forked["sequence"].as_u64().unwrap() > 0, "{forked}");
    let view = h.view(&fork);
    assert_eq!((view["thread"]["title"].as_str(), view["thread"]["createdBy"].as_str(), view["thread"]["creationSource"].as_str(), view["thread"]["lineage"]["parentThreadId"].as_str()), (Some("Forked work"), Some("agent"), Some("mcp"), Some("done")));
    // The transfer the fork left is what the transfers tool reports for either side.
    let transfers = h.ok(&caller, "t3_thread_transfers", json!({"threadId":fork})).await;
    assert_eq!((transfers["transfers"].as_array().unwrap().len(), transfers["transfers"][0]["status"].as_str(), transfers["transfers"][0]["sourceThreadId"].as_str()), (1, Some("pending"), Some("done")));
    // A fork with no run of its own has nothing to merge back, and a thread that is not its fork takes none.
    let nothing = h.refused(&caller, "t3_thread_merge_back", json!({"sourceThreadId":fork,"targetThreadId":"done","sourcePoint":{"type":"latest_stable"}})).await;
    assert_eq!(nothing["code"], "orchestration_error");
    assert!(nothing["message"].as_str().unwrap().contains("No stable source run"), "{nothing}");
    let unrelated = h.refused(&caller, "t3_thread_merge_back", json!({"sourceThreadId":"done","targetThreadId":parent,"sourcePoint":{"type":"latest_stable"}})).await;
    assert!(unrelated["message"].as_str().unwrap().contains("is not a fork of"), "{unrelated}");
    // A fork of a source that cannot be forked (no such run) is refused with the service's reason.
    let missing = h.refused(&caller, "t3_thread_fork", json!({"threadId":"done","sourcePoint":{"type":"run","runId":"missing"}})).await;
    assert!(missing["message"].as_str().unwrap().contains("No stable source run"), "{missing}");
    // The modes gate: a caller below the source's modes cannot fork it.
    let weak = h.running_thread("weak", "approval-required", "plan").await;
    let weak_caller = h.caller(&weak).await;
    assert_eq!(h.refused(&weak_caller, "t3_thread_fork", json!({"threadId":"done","sourcePoint":{"type":"run","runId":finished}})).await["code"], "runtime_mode_escalation_denied");
    h.finish().await;
}
