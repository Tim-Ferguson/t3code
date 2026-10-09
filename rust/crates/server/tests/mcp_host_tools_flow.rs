#![cfg(unix)]
//! The pull request, worktree and preview tools over the real MCP HTTP transport with a
//! thread-scoped credential, a real store and execution service, real git in a temporary
//! repository, and the scripted fake Codex app-server. Tests wait on persisted events, never on
//! sleeps.
mod mcp_support;
use mcp_support::*;
use serde_json::{Value, json};
use std::{path::Path, process::Command};
use t3_server::persistence::write_projection;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git").args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.com", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"]).args(args).current_dir(dir).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn thread_links(h: &Harness, thread: &str) -> Vec<Value> {
    h.view(thread)["thread"]["pullRequests"].as_array().cloned().unwrap_or_default()
}

fn edit_thread(h: &Harness, thread: &str, change: impl FnOnce(&mut Value)) {
    let mut projection = h.view(thread);
    change(&mut projection);
    h.store.transaction(|tx| write_projection(tx, "thread", thread, &projection)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_request_tools_link_list_watch_and_unwatch_through_the_thread_commands() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;
    let url = "https://github.com/Acme/Web/pull/12/files";

    // Link by URL files the pull request under its host-level identity, as the agent's own link.
    let linked = h.ok(&caller, "link_pull_request", json!({"url":url})).await;
    assert_eq!(linked, json!({"host":"github.com","repository":"acme/web","number":12,"url":url,"alreadyLinked":false}));
    let links = thread_links(&h, &parent);
    assert_eq!((links.len(), links[0]["source"].as_str(), links[0]["repository"].as_str()), (1, Some("agent"), Some("acme/web")));
    assert_eq!(h.ok(&caller, "link_pull_request", json!({"url":url})).await["alreadyLinked"], true);
    assert_eq!(thread_links(&h, &parent).len(), 1, "linking twice records nothing more");

    // A repository and number need a host; the project here has no remote to supply one.
    assert_eq!(h.text(&caller, "link_pull_request", json!({"repository":"acme/web","number":13})).await, (true, "This thread's project has no recognised remote. Pass host or url.".to_owned()));
    assert_eq!(h.text(&caller, "link_pull_request", json!({"host":"github.com"})).await, (true, "Pass either url, or both repository and number.".to_owned()));
    assert_eq!(h.text(&caller, "link_pull_request", json!({"url":"https://example.com/nothing"})).await, (true, "This is not a recognised pull request URL. Pass repository and number instead.".to_owned()));
    let second = h.ok(&caller, "link_pull_request", json!({"repository":"Acme/Web","number":13,"host":"GitHub.com"})).await;
    assert_eq!((second["url"].as_str(), second["alreadyLinked"].as_bool()), (Some("https://github.com/acme/web/pull/13"), Some(false)));

    // Listing reports what is linked; a stack shows in `chains` once snapshots chain the layers.
    edit_thread(&h, &parent, |projection| {
        let links = projection["thread"]["pullRequests"].as_array_mut().unwrap();
        links[0]["snapshot"] = json!({"state":"open","title":"Base","headBranch":"base","baseBranch":"main","isDraft":false,"updatedAt":"2026-01-01T00:00:00.000Z","syncedAt":"2026-01-01T00:00:00.000Z"});
        links[1]["snapshot"] = json!({"state":"open","title":"Top","headBranch":"top","baseBranch":"base","isDraft":true,"updatedAt":"2026-01-01T00:00:00.000Z","syncedAt":"2026-01-01T00:00:00.000Z"});
    });
    let listed = h.ok(&caller, "list_thread_pull_requests", json!({})).await;
    assert_eq!(listed["chains"], json!([{"kind":"derived","numbers":[12,13]}]));
    assert_eq!(listed["pullRequests"][1]["stack"], json!({"kind":"derived","position":2,"size":2}));
    assert_eq!((listed["pullRequests"][0]["title"].as_str(), listed["pullRequests"][1]["isDraft"].as_bool(), listed["pullRequests"][0]["watching"].as_bool()), (Some("Base"), Some(true), Some(false)));

    // Watch starts the server watching the pull request; the thread's background work shows the monitor.
    let watched = h.ok(&caller, "watch_pull_request", json!({"url":url})).await;
    assert_eq!((watched["watching"].as_bool(), watched["wasWatching"].as_bool()), (Some(true), Some(false)));
    assert!(thread_links(&h, &parent)[0]["watch"]["startedAt"].is_string());
    // The thread's background work lists the watch as a monitor once it has no run in flight.
    let sibling = h.ok(&caller, "t3_thread_launch", json!({"title":"Sibling"})).await["threadId"].as_str().unwrap().to_owned();
    h.ok(&caller, "watch_pull_request", json!({"threadId":sibling,"url":url})).await;
    let tasks = t3_server::thread::shell(&h.view(&sibling))["pendingBackgroundTasks"].to_string();
    assert!(tasks.contains("Watching pull request #12") && tasks.contains("monitor"), "{tasks}");
    h.ok(&caller, "unwatch_pull_request", json!({"threadId":sibling,"url":url})).await;
    assert_eq!(h.ok(&caller, "watch_pull_request", json!({"url":url})).await["wasWatching"], true);
    // Watching a pull request the thread has not linked links it first, as the agent's.
    let fresh = h.ok(&caller, "watch_pull_request", json!({"url":"https://github.com/acme/web/pull/14"})).await;
    assert_eq!(fresh["watching"], true);
    assert_eq!(thread_links(&h, &parent).iter().find(|link| link["number"] == 14).unwrap()["source"], "agent");
    let stopped = h.ok(&caller, "unwatch_pull_request", json!({"url":url})).await;
    assert_eq!((stopped["watching"].as_bool(), stopped["wasWatching"].as_bool()), (Some(false), Some(true)));
    assert!(thread_links(&h, &parent)[0].get("watch").is_none_or(|watch| watch.is_null()), "the pull request stays linked");

    // A merged pull request cannot be watched; a subagent never watches.
    edit_thread(&h, &parent, |projection| projection["thread"]["pullRequests"][0]["snapshot"]["state"] = json!("merged"));
    assert_eq!(h.text(&caller, "watch_pull_request", json!({"url":url})).await, (true, "The pull request is merged, so there is nothing to watch.".to_owned()));
    let child = h.running_thread("child", "full-access", "default").await;
    edit_thread(&h, &child, |projection| projection["thread"]["lineage"] = json!({"parentThreadId":"parent","relationshipToParent":"subagent","rootThreadId":"parent"}));
    let child_caller = h.caller(&child).await;
    let (failed, text) = h.text(&child_caller, "watch_pull_request", json!({"url":url})).await;
    assert!(failed && text.starts_with("This thread is a subagent, so it cannot watch pull requests."), "{text}");

    // Unlink removes the link, and says so when there was none.
    assert_eq!(h.ok(&caller, "unlink_pull_request", json!({"url":url})).await, json!({"host":"github.com","repository":"acme/web","number":12,"wasLinked":true}));
    assert_eq!(h.ok(&caller, "unlink_pull_request", json!({"url":url})).await["wasLinked"], false);
    assert!(!thread_links(&h, &parent).iter().any(|link| link["number"] == 12));

    // Another thread's pull requests are the caller's to change only within its own modes.
    let weak = h.running_thread("weak", "approval-required", "plan").await;
    let weak_caller = h.caller(&weak).await;
    let before = thread_links(&h, &parent).len();
    for (tool, arguments) in [("link_pull_request", json!({"threadId":parent,"url":url})), ("unlink_pull_request", json!({"threadId":parent,"url":"https://github.com/acme/web/pull/14"})), ("watch_pull_request", json!({"threadId":parent,"url":url})), ("unwatch_pull_request", json!({"threadId":parent,"url":url}))] {
        assert_eq!(h.text(&weak_caller, tool, arguments).await, (true, "Child runtime mode full-access is broader than parent mode approval-required.".to_owned()), "{tool}");
    }
    assert_eq!(thread_links(&h, &parent).len(), before);
    // Reading is not writing: the list of another thread's pull requests is open to the caller.
    assert_eq!(h.ok(&weak_caller, "list_thread_pull_requests", json!({"threadId":parent})).await["pullRequests"].as_array().unwrap().len(), before);
    assert_eq!(h.text(&caller, "list_thread_pull_requests", json!({"threadId":"missing"})).await, (true, "Thread missing was not found.".to_owned()));
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn worktree_tools_list_refs_report_the_binding_and_hand_a_thread_off_to_a_new_worktree() {
    let h = Harness::new().await;
    let workspace = h.store.projection("project", "project").unwrap().unwrap()["workspaceRoot"].as_str().unwrap().to_owned();
    let workspace = Path::new(&workspace);
    git(workspace, &["init", "-q"]);
    std::fs::write(workspace.join("README.md"), "hello\n").unwrap();
    git(workspace, &["add", "."]);
    git(workspace, &["commit", "-q", "-m", "init"]);
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;

    let status = h.ok(&caller, "t3_worktree_status", json!({})).await;
    assert_eq!((status["attached"].as_bool(), status["worktreePath"].clone(), status["projectWorkspaceRoot"].as_str()), (Some(false), Value::Null, workspace.to_str()));
    let refs = h.ok(&caller, "t3_worktree_list", json!({})).await;
    assert_eq!(refs["isRepo"], true);
    assert!(refs["refs"].as_array().unwrap().iter().any(|candidate| candidate["name"] == "main"), "{refs}");

    // A branch that exists is refused before anything is created, naming where it is checked out.
    let taken = h.refused(&caller, "t3_worktree_handoff", json!({"branch":"main"})).await;
    assert_eq!((taken["_tag"].as_str(), taken["code"].as_str()), (Some("WorktreeMcpFailure"), Some("invalid_request")));
    assert!(taken["message"].as_str().unwrap().starts_with("Branch 'main' already exists and is checked out at"), "{taken}");
    assert!(h.view(&parent)["thread"]["worktreePath"].is_null());

    // The handoff creates the worktree, binds the thread to it and queues the continuation, which starts inside it.
    let handed = h.ok(&caller, "t3_worktree_handoff", json!({"branch":"feature/handoff","startFromOrigin":false,"continuationPrompt":"carry on in the worktree"})).await;
    let worktree = handed["worktreePath"].as_str().unwrap().to_owned();
    assert!(Path::new(&worktree).join("README.md").is_file() && Path::new(&worktree).join(".git").exists(), "{handed}");
    assert_eq!((handed["branch"].as_str(), handed["baseRef"].as_str(), handed["startedFromOrigin"].as_bool(), handed["setupScript"]["status"].as_str(), handed["continuation"]["status"].as_str()), (Some("feature/handoff"), Some("main"), Some(false), Some("no-script"), Some("scheduled")));
    let view = h.view(&parent);
    assert_eq!((view["thread"]["worktreePath"].as_str(), view["thread"]["branch"].as_str()), (Some(worktree.as_str()), Some("feature/handoff")));
    let continuation = view["messages"].as_array().unwrap().iter().find(|message| message["text"] == "carry on in the worktree").unwrap().clone();
    assert_eq!((continuation["createdBy"].as_str(), continuation["creationSource"].as_str()), (Some("agent"), Some("mcp")));
    h.until(&parent, |view| view["runs"].as_array().unwrap().iter().find(|run| run["userMessageId"] == continuation["id"]).filter(|run| run["status"] == "completed").map(|_| ())).await;
    assert!(h.provider_requests().iter().any(|request| request["method"] == "turn/start" && request["params"]["cwd"] == worktree.as_str() && request["params"]["input"][0]["text"] == "carry on in the worktree"), "{:?}", h.provider_requests());
    // Once attached, the status says so and a second handoff is refused.
    let attached = h.ok(&caller, "t3_worktree_status", json!({})).await;
    assert_eq!((attached["attached"].as_bool(), attached["worktreePath"].as_str(), attached["branch"].as_str()), (Some(true), Some(worktree.as_str()), Some("feature/handoff")));
    // The handed-off session no longer owns a live run, so it cannot act again; a thread already attached is refused.
    assert_eq!(h.refused(&caller, "t3_worktree_handoff", json!({"branch":"feature/second"})).await["code"], "parent_not_active");
    let other = h.running_thread("other", "full-access", "default").await;
    let other_caller = h.caller(&other).await;
    edit_thread(&h, &other, |projection| projection["thread"]["worktreePath"] = json!("/elsewhere/worktree"));
    let again = h.refused(&other_caller, "t3_worktree_handoff", json!({"branch":"feature/second"})).await;
    assert_eq!((again["_tag"].as_str(), again["code"].as_str(), again["message"].as_str()), (Some("WorktreeMcpFailure"), Some("already_in_worktree"), Some(&format!("Thread 'other' is already attached to worktree '/elsewhere/worktree'.")[..])));
    // The thread's refs are now listed from its worktree.
    assert!(h.ok(&caller, "t3_worktree_list", json!({"query":"handoff"})).await["refs"].as_array().unwrap().iter().any(|candidate| candidate["name"] == "feature/handoff"));

    // A thread whose run has stopped cannot act as the caller, so cannot hand off.
    let stopped = h.running_thread("stopped", "full-access", "default").await;
    let stopped_caller = h.caller(&stopped).await;
    h.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop-stopped","threadId":stopped,"runId":h.view(&stopped)["runs"][0]["id"]}), chrono::Utc::now()).unwrap();
    h.until(&stopped, |view| (view["runs"][0]["status"] == "interrupted").then_some(())).await;
    assert_eq!(h.refused(&stopped_caller, "t3_worktree_handoff", json!({"branch":"feature/late"})).await["code"], "parent_not_active");
    assert!(h.view(&stopped)["thread"]["worktreePath"].is_null());
    let branches = Command::new("git").args(["branch", "--list"]).current_dir(workspace).output().unwrap();
    assert!(!String::from_utf8_lossy(&branches.stdout).contains("feature/late"));
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_with_no_service_here_answer_as_the_original_does_without_a_host_and_say_not_to_retry() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;
    let listed = h.rpc(&caller, "tools/list", json!({})).await["result"]["tools"].clone();
    let names: Vec<&str> = listed.as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
    for tool in ["preview_open", "preview_snapshot", "html_render", "html_preview", "t3_preview_list", "t3_queue_list", "watch_pull_request"] {
        assert!(names.contains(&tool), "the original lists {tool} to every caller");
    }
    // Without the preview capability: the original's own refusal, as the plain message the transport writes.
    let (failed, text) = h.text(&caller, "preview_click", json!({"selector":"button"})).await;
    assert!(failed && text.starts_with("MCP credential does not grant the preview capability: browser preview tools are off for this thread. Do not retry them."), "{text}");
    let listing = h.refused(&caller, "t3_preview_list", json!({})).await;
    assert_eq!((listing["_tag"].as_str(), listing["capability"].as_str()), (Some("PreviewAutomationUnavailableError"), Some("preview")));
    // With it, but no browser host to run them: the original's "no available host" result.
    let browser = h.caller_with_preview(&parent).await;
    let (failed, text) = h.text(&browser, "preview_open", json!({"url":"http://localhost:3000"})).await;
    assert!(failed && text.starts_with("No preview automation host is available for open in environment environment-1.") && text.contains("a headless server has no browser of its own. Do not retry."), "{text}");
    let (failed, text) = h.text(&browser, "preview_snapshot", json!({})).await;
    assert!(failed && text.starts_with("Preview snapshot failed: No preview automation host is available for snapshot"), "{text}");
    for (tool, operation) in [("preview_status", "status"), ("preview_wait_for", "waitFor"), ("preview_set_appearance", "setColorScheme"), ("preview_recording_stop", "recordingStop")] {
        let arguments = match tool { "preview_set_appearance" => json!({"colorScheme":"dark"}), "preview_wait_for" => json!({"text":"x"}), _ => json!({}) };
        let (failed, text) = h.text(&browser, tool, arguments).await;
        assert!(failed && text.starts_with(&format!("No preview automation host is available for {operation} ")), "{tool}: {text}");
    }
    // The tab tools see what a server with no host has: no tabs, and nothing to close.
    let tabs = h.ok(&browser, "t3_preview_list", json!({})).await;
    assert_eq!((tabs["sessions"].clone(), tabs["nextCursor"].clone()), (json!([]), Value::Null));
    assert_eq!(h.ok(&browser, "t3_preview_close", json!({"tabId":"preview-tab-1"})).await, json!({}));
    // The HTML tools need a headless browser this server does not have.
    let (failed, text) = h.text(&caller, "html_preview", json!({"html":"<p>x</p>"})).await;
    assert!(failed && text.starts_with("This T3 server cannot render an HTML preview: it has no headless browser. Do not retry."), "{text}");
    let render = h.refused(&caller, "html_render", json!({"html":"<p>x</p>","title":"x","height":300})).await;
    assert_eq!(render["code"], "orchestration_error");
    assert!(render["message"].as_str().unwrap().contains("Do not retry"), "{render}");
    h.finish().await;
}

/// A Codex tool call that names an MCP App lands with the stored document; the app then reaches its
/// own server through the thread's live provider session, its notes reach the agent on the next turn,
/// and feedback uploads through the same session. The fake Codex app-server answers each request.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_tool_call_that_names_an_app_lands_with_its_document_and_the_app_reaches_its_server() {
    let h = Harness::new().await;
    h.start_thread("app", "fixture-model", "full-access", "draw it [mcp-app]");
    let item = h.until("app", |view| view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "dynamic_tool" && item["toolName"] == "widget_server.render_widget" && item["status"] == "completed").cloned()).await;
    // The reference is on the item, beside the full result the app replays; the document is stored under its policy.
    let reference = item["output"]["t3McpApp"].clone();
    assert_eq!((reference["server"].as_str(), reference["tool"].as_str(), reference["resourceUri"].as_str(), reference["prefersBorder"].as_bool()), (Some("widget_server"), Some("render_widget"), Some("ui://widget/app.html"), Some(true)));
    assert_eq!(reference["csp"], json!({"connectDomains":["https://api.example.com"]}));
    assert_eq!(item["output"]["result"]["structuredContent"], json!({"widget":"fixture"}));
    let files = t3_server::attachment_service::AttachmentService::for_store(&h.store).unwrap();
    let stored = std::fs::read_to_string(files.attachments_dir().join(format!("{}.html", reference["attachmentId"].as_str().unwrap()))).unwrap();
    assert!(stored.starts_with("<!doctype html><meta http-equiv=\"Content-Security-Policy\"") && stored.contains("connect-src https://api.example.com;") && stored.ends_with("Fixture widget</body></html>"), "{stored}");
    assert!(h.provider_requests().iter().any(|request| request["method"] == "mcpServer/resource/read" && request["params"]["uri"] == "ui://widget/app.html"), "the document was read through Codex's own MCP client");
    h.until("app", |view| view["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed").then_some(())).await;

    // The app asks its own server: tool info, a call it is allowed, one it is not, and a resource.
    let item_id = item["id"].as_str().unwrap();
    let ask = |tag: &'static str, payload: Value| {
        let store = h.store.clone();
        async move { t3_server::mcp_apps::command(&store, tag, payload).await }
    };
    let info = ask("mcpApps.toolInfo", json!({"threadId":"app","itemId":item_id,"name":"render_widget"})).await.unwrap();
    assert_eq!((info["callable"].as_bool(), info["readOnly"].as_bool(), info["title"].as_str()), (Some(true), Some(true), Some("Render widget")));
    let hidden = ask("mcpApps.toolInfo", json!({"threadId":"app","itemId":item_id,"name":"model_only"})).await.unwrap();
    assert_eq!(hidden["callable"], false);
    let called = ask("mcpApps.callTool", json!({"threadId":"app","itemId":item_id,"name":"render_widget","arguments":{"size":3}})).await.unwrap();
    assert_eq!((called["content"][0]["text"].as_str(), called["structuredContent"]["echo"].clone(), called["_meta"]["fixture"].as_bool()), (Some("called render_widget {\"size\": 3}"), json!({"size":3}), Some(true)));
    assert!(ask("mcpApps.callTool", json!({"threadId":"app","itemId":item_id,"name":"model_only","arguments":{}})).await.is_err(), "an app may not call a tool hidden from apps");
    let read = ask("mcpApps.readResource", json!({"threadId":"app","itemId":item_id,"uri":"ui://widget/app.html"})).await.unwrap();
    assert_eq!(read["contents"][0]["mimeType"], "text/html;profile=mcp-app");

    // What the app tells the agent goes in with the next turn, as untrusted context.
    ask("mcpApps.updateModelContext", json!({"threadId":"app","itemId":item_id,"conversationThreadId":"app","content":[{"type":"text","text":"The widget shows 3 rows."}]})).await.unwrap();
    h.execution.dispatch(&json!({"type":"message.dispatch","commandId":"again","threadId":"app","messageId":"again-message","text":"and now","attachments":[],"dispatchMode":{"type":"start_immediately"}}), chrono::Utc::now()).unwrap();
    h.until("app", |view| view["runs"].as_array().unwrap().iter().filter(|run| run["status"] == "completed").count().eq(&2).then_some(())).await;
    let second = h.provider_requests().into_iter().filter(|request| request["method"] == "turn/start").last().unwrap();
    let key = format!("mcp_app_{}", item_id.replace(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '-')), "_"));
    assert_eq!(second["params"]["additionalContext"][&key], json!({"kind":"untrusted","value":"The widget shows 3 rows."}));
    // Clearing the note removes it from the next turn.
    ask("mcpApps.updateModelContext", json!({"threadId":"app","itemId":item_id,"conversationThreadId":"app","content":[{"type":"text","text":" "}]})).await.unwrap();
    assert!(t3_server::mcp_apps::model_context_for_thread(&h.store, "app").unwrap().is_empty());

    // Feedback uploads through the same live session, and says so when the thread has none.
    let feedback = ask("provider.uploadFeedback", json!({"threadId":"app","reason":"The widget was blank."})).await.unwrap();
    assert!(feedback["feedbackId"].as_str().unwrap().starts_with("fixture-feedback-"), "{feedback}");
    assert!(h.provider_requests().iter().any(|request| request["method"] == "feedback/upload" && request["params"]["reason"] == "The widget was blank." && request["params"]["classification"] == "bug"));
    assert_eq!(ask("provider.uploadFeedback", json!({"threadId":"missing"})).await.unwrap_err()["_tag"], "ProviderUploadFeedbackError");
    h.finish().await;
}
