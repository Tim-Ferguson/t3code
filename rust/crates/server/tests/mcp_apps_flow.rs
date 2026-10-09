//! MCP Apps and `provider.uploadFeedback` over a real socket. The provider session is a fake the
//! test registers for a provider session id, so no provider runs; the app is the stored tool item
//! a real adapter would have left. Document helpers replay the original over generated inputs.
mod rpc_support;
use futures_util::future::BoxFuture;
use rpc_support::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use t3_server::{
    mcp_apps::{content_security_policy, inject_csp, model_context_for_thread, read_csp, read_permissions, snapshot_app, tool_callable_by_app},
    provider_live::{LiveSession, McpAppsHost, register},
};

#[test]
fn documents_and_visibility_agree_with_the_original() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/mcp-app-documents.json")).unwrap();
    for row in fixture["csp"].as_array().unwrap() {
        let read = read_csp(&row["input"]);
        assert_eq!(read.clone().unwrap_or(Value::Null), row["read"], "{row}");
        assert_eq!(content_security_policy(read.as_ref()), row["policy"].as_str().unwrap(), "{row}");
    }
    for row in fixture["injection"].as_array().unwrap() {
        assert_eq!(inject_csp(row["html"].as_str().unwrap(), read_csp(&row["csp"]).as_ref()), row["injected"].as_str().unwrap(), "{row}");
    }
    for row in fixture["permissions"].as_array().unwrap() {
        assert_eq!(read_permissions(&row["input"]).unwrap_or(Value::Null), row["read"], "{row}");
    }
    for row in fixture["callable"].as_array().unwrap() {
        assert_eq!(tool_callable_by_app(&row["meta"]), row["callable"], "{row}");
    }
}

#[test]
fn a_snapshot_stores_the_document_under_its_policy_and_refuses_what_is_not_an_app() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    let contents = |mime: &str, text: &str| vec![json!({"uri":"ui://srv/app","mimeType":mime,"text":text,"_meta":{"ui":{"csp":{"connectDomains":["https://api.example.com","bogus"]},"permissions":{"camera":{}},"prefersBorder":true}}})];
    let reference = snapshot_app(dir, "Thread One", "srv", "show", "ui://srv/app", &contents("text/html; profile=mcp-app", "<!doctype html><p>hi")).unwrap();
    assert_eq!(reference["server"], "srv");
    assert_eq!(reference["csp"], json!({"connectDomains":["https://api.example.com"]}));
    assert_eq!((reference["permissions"].clone(), reference["prefersBorder"].clone()), (json!({"camera":{}}), json!(true)));
    let id = reference["attachmentId"].as_str().unwrap();
    assert!(id.starts_with("thread-one-") && id.ends_with("-html"), "{id}");
    let stored = std::fs::read_to_string(dir.join(format!("{id}.html"))).unwrap();
    assert!(stored.starts_with("<!doctype html><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; "));
    assert!(stored.contains("connect-src https://api.example.com;") && stored.ends_with("<p>hi"));
    // A blob is base64; another mime type, another scheme, a blank document and an oversized one are not apps.
    let blob = vec![json!({"mimeType":"text/html;profile=mcp-app","blob":"PHA+YmxvYjwvcD4="})];
    let from_blob = snapshot_app(dir, "t", "srv", "show", "ui://srv/app", &blob).unwrap();
    assert!(std::fs::read_to_string(dir.join(format!("{}.html", from_blob["attachmentId"].as_str().unwrap()))).unwrap().ends_with("<p>blob</p>"));
    assert!(snapshot_app(dir, "t", "srv", "show", "ui://srv/app", &contents("text/html", "<p>")).is_none());
    assert!(snapshot_app(dir, "t", "srv", "show", "https://srv/app", &contents("text/html;profile=mcp-app", "<p>")).is_none());
    assert!(snapshot_app(dir, "t", "srv", "show", "ui://srv/app", &contents("text/html;profile=mcp-app", "  ")).is_none());
    assert!(snapshot_app(dir, "t", "srv", "show", "ui://srv/app", &contents("text/html;profile=mcp-app", &"x".repeat(5 * 1024 * 1024 + 1))).is_none());
    assert!(snapshot_app(dir, "t", "srv", "show", "ui://other/app", &contents("text/html;profile=mcp-app", "<p>")).is_none(), "a content that names another uri is not this resource");
    let leftover = std::fs::read_dir(dir).unwrap().count();
    assert_eq!(leftover, 2, "only documents that were kept are on disk");
}

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Value>>,
}
impl McpAppsHost for Fake {
    fn list_tools(&self, _: Value, server: String) -> BoxFuture<'static, Result<Vec<Value>, String>> {
        Box::pin(async move {
            if server == "broken" {
                return Err("server unreachable".into());
            }
            Ok(vec![
                json!({"name":"refresh","annotations":{"readOnlyHint":true,"title":"Refresh"},"_meta":{"ui":{"visibility":["model","app"]}}}),
                json!({"name":"secret","_meta":{"ui":{"visibility":["model"]}}}),
                json!({"name":"plain"}),
            ])
        })
    }
    fn call_tool(&self, thread: Value, server: String, tool: String, arguments: Value) -> BoxFuture<'static, Result<Value, String>> {
        self.calls.lock().unwrap().push(json!({"thread":thread["id"],"server":server,"tool":tool,"arguments":arguments}));
        Box::pin(async move { if tool == "plain" && arguments["fail"] == true { Err("tool exploded".into()) } else { Ok(json!({"content":[{"type":"text","text":"done"}],"isError":false})) } })
    }
    fn read_resource(&self, _: Value, _: String, uri: String) -> BoxFuture<'static, Result<Value, String>> {
        Box::pin(async move { Ok(json!({"contents":[{"uri":uri,"text":"body"}]})) })
    }
}

fn launch(h: &Harness, thread: &str) {
    t3_server::launch::ThreadLaunchService::new(h.store.clone())
        .launch(json!({"commandId":format!("launch-{thread}"),"threadId":thread,"projectId":"project","title":thread,"modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), chrono::Utc::now())
        .unwrap();
}
fn edit(h: &Harness, thread: &str, change: impl FnOnce(&mut Value)) {
    let mut projection = h.store.projection("thread", thread).unwrap().unwrap();
    change(&mut projection);
    h.store.transaction(|transaction| t3_server::persistence::write_projection(transaction, "thread", thread, &projection)).unwrap();
}
fn app_item(id: &str, tool_name: &str, server: &str, tool: &str, provider_thread: Value) -> Value {
    json!({"id":id,"threadId":"thread","runId":"run-1","type":"dynamic_tool","toolName":tool_name,"providerThreadId":provider_thread,"output":{"structuredContent":{"t3McpApp":{"attachmentId":"a-html","server":server,"tool":tool,"resourceUri":"ui://x/app"}}}})
}

#[test]
fn apps_reach_only_their_own_server_through_a_live_session() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness().await;
        std::fs::create_dir_all(h.base.join("work")).unwrap();
        let mut operator = h.connect(&h.operator).await;
        let mut reader = h.connect(&h.reader).await;
        success(&rpc(&mut operator, 1, "projects.mutate", json!({"type":"project.create","commandId":"p","projectId":"project","title":"P","workspaceRoot":h.base.join("work")})).await);
        launch(&h, "thread");
        launch(&h, "fork");
        launch(&h, "stranger");
        let mut rolled = app_item("app-rolled", "srv.show", "srv", "show", json!("pt-live"));
        rolled["runId"] = json!("run-2");
        let mut text = app_item("text", "srv.show", "srv", "show", json!("pt-live"));
        text["type"] = json!("command_execution");
        let items = vec![
            app_item("app", "srv.show", "srv", "show", json!("pt-live")),
            app_item("app-gone", "srv.show", "srv", "show", json!("pt-gone")),
            app_item("app-plain", "srv.show", "srv", "show", json!("pt-plain")),
            app_item("app-none", "srv.show", "srv", "show", json!("pt-none")),
            app_item("app-broken", "broken.show", "broken", "show", json!("pt-live")),
            app_item("app-forged", "other.tool", "srv", "show", json!("pt-live")),
            app_item("app-orphan", "srv.show", "srv", "show", Value::Null),
            rolled,
            text,
        ];
        edit(&h, "thread", |thread| {
            thread["providerThreads"] = json!([{"id":"pt-live","providerSessionId":"session-live"},{"id":"pt-gone","providerSessionId":"session-gone"},{"id":"pt-plain","providerSessionId":"session-plain"},{"id":"pt-none","providerSessionId":null}]);
            thread["thread"]["activeProviderThreadId"] = json!("pt-live");
            thread["runs"] = json!([{"id":"run-1","threadId":"thread","status":"completed"},{"id":"run-2","threadId":"thread","status":"rolled_back"}]);
            thread["turnItems"] = Value::Array(items);
        });
        edit(&h, "fork", |thread| thread["thread"]["lineage"] = json!({"parentThreadId":"thread","relationshipToParent":"fork","rootThreadId":"thread"}));
        edit(&h, "stranger", |thread| thread["thread"]["lineage"] = json!({"parentThreadId":"thread","relationshipToParent":"handoff","rootThreadId":"thread"}));
        let fake = Arc::new(Fake::default());
        let _live = register("session-live", LiveSession { driver: "codex".into(), upload_feedback: None, mcp_apps: Some(fake.clone()) });
        let _plain = register("session-plain", LiveSession { driver: "claudeAgent".into(), upload_feedback: None, mcp_apps: None });
        let call = |thread: &str, item: &str, name: &str| json!({"threadId":thread,"itemId":item,"name":name,"arguments":{"x":1}});

        // Resolving the app.
        for (item, reason) in [("nobody", "not-an-app"), ("text", "not-an-app"), ("app-forged", "not-an-app"), ("app-orphan", "not-an-app"), ("app-gone", "session-stopped"), ("app-none", "session-stopped"), ("app-plain", "provider-unsupported")] {
            let exit = rpc(&mut operator, 2, "mcpApps.toolInfo", json!({"threadId":"thread","itemId":item,"name":"refresh"})).await;
            assert_eq!((failure(&exit)["_tag"].as_str(), failure(&exit)["reason"].as_str()), (Some("McpAppRequestError"), Some(reason)), "{item}");
        }
        let nowhere = rpc(&mut operator, 3, "mcpApps.toolInfo", json!({"threadId":"nowhere","itemId":"app","name":"refresh"})).await;
        assert_eq!(failure(&nowhere)["reason"], "not-an-app");

        // toolInfo: what a client needs to decide whether to ask.
        let info = success(&rpc(&mut reader, 4, "mcpApps.toolInfo", json!({"threadId":"thread","itemId":"app","name":"refresh"})).await).clone();
        assert_eq!((info["callable"].clone(), info["readOnly"].clone(), info["title"].clone(), info["tool"]["name"].clone()), (json!(true), json!(true), json!("Refresh"), json!("refresh")));
        let hidden = success(&rpc(&mut reader, 5, "mcpApps.toolInfo", json!({"threadId":"thread","itemId":"app","name":"secret"})).await).clone();
        assert_eq!((hidden["callable"].clone(), hidden["readOnly"].clone()), (json!(false), json!(false)));
        let unknown = success(&rpc(&mut reader, 6, "mcpApps.toolInfo", json!({"threadId":"thread","itemId":"app","name":"missing"})).await).clone();
        assert_eq!(unknown, json!({"callable":false,"readOnly":false}));
        let failed = rpc(&mut reader, 7, "mcpApps.toolInfo", json!({"threadId":"thread","itemId":"app-broken","name":"refresh"})).await;
        assert_eq!((failure(&failed)["reason"].as_str(), failure(&failed)["cause"]["message"].as_str()), (Some("request-failed"), Some("server unreachable")));

        // callTool: operate scope, a tool the app may call, the app's own server.
        let denied = rpc(&mut reader, 8, "mcpApps.callTool", call("thread", "app", "refresh")).await;
        assert_eq!(failure(&denied)["_tag"], "EnvironmentAuthorizationError");
        let done = success(&rpc(&mut operator, 9, "mcpApps.callTool", call("thread", "app", "refresh")).await).clone();
        assert_eq!(done["content"][0]["text"], "done");
        assert_eq!(fake.calls.lock().unwrap()[0], json!({"thread":"pt-live","server":"srv","tool":"refresh","arguments":{"x":1}}));
        for name in ["secret", "missing"] {
            assert_eq!(failure(&rpc(&mut operator, 10, "mcpApps.callTool", call("thread", "app", name)).await)["reason"], "tool-not-callable", "{name}");
        }
        let exploded = rpc(&mut operator, 11, "mcpApps.callTool", json!({"threadId":"thread","itemId":"app","name":"plain","arguments":{"fail":true}})).await;
        assert_eq!((failure(&exploded)["reason"].as_str(), failure(&exploded)["cause"]["message"].as_str()), (Some("request-failed"), Some("tool exploded")));
        assert!(!defect(&rpc(&mut operator, 12, "mcpApps.callTool", json!({"threadId":"thread","itemId":"app","name":"plain","arguments":"nope"})).await).is_null());
        assert_eq!(fake.calls.lock().unwrap().len(), 2, "a refused call never reaches the server");

        // readResource.
        let resource = success(&rpc(&mut reader, 13, "mcpApps.readResource", json!({"threadId":"thread","itemId":"app","uri":"ui://srv/app"})).await).clone();
        assert_eq!(resource["contents"][0]["uri"], "ui://srv/app");

        // updateModelContext: operate scope, the app's own thread or a fork of it, text and structured content.
        let context = |conversation: &str, extra: Value| { let mut payload = json!({"threadId":"thread","itemId":"app","conversationThreadId":conversation}); payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone()); payload };
        assert_eq!(failure(&rpc(&mut reader, 14, "mcpApps.updateModelContext", context("thread", json!({}))).await)["_tag"], "EnvironmentAuthorizationError");
        success(&rpc(&mut operator, 15, "mcpApps.updateModelContext", context("fork", json!({"content":[{"type":"text","text":"line one"},{"type":"text","text":"line two"}],"structuredContent":{"b":1}}))).await);
        let entries = model_context_for_thread(&h.store, "fork").unwrap();
        assert_eq!(entries, vec![json!({"itemId":"app","server":"srv","tool":"show","text":"line one\nline two\n{\"b\":1}"})]);
        assert_eq!(model_context_for_thread(&h.store, "thread").unwrap(), Vec::<Value>::new(), "context belongs to the conversation on screen");
        for (conversation, extra, reason) in [("stranger", json!({}), "not-an-app"), ("nowhere", json!({}), "request-failed"), ("fork", json!({"content":[{"type":"image"}]}), "unsupported-content"), ("fork", json!({"content":["text"]}), "unsupported-content"), ("fork", json!({"content":[{"type":"text","text":"x".repeat(16 * 1024 + 1)}]}), "request-failed")] {
            let exit = rpc(&mut operator, 16, "mcpApps.updateModelContext", context(conversation, extra.clone())).await;
            assert_eq!(failure(&exit)["reason"], reason, "{conversation} {extra}");
        }
        let too_big = rpc(&mut operator, 17, "mcpApps.updateModelContext", context("fork", json!({"content":[{"type":"text","text":"x".repeat(16 * 1024 + 1)}]}))).await;
        assert_eq!(failure(&too_big)["cause"]["message"], "Model context is too large.");
        assert_eq!(model_context_for_thread(&h.store, "fork").unwrap().len(), 1, "a refused update changes nothing");
        // Blank text clears; an update replaces the previous one.
        success(&rpc(&mut operator, 18, "mcpApps.updateModelContext", context("fork", json!({"content":[{"type":"text","text":"replaced"}]}))).await);
        assert_eq!(model_context_for_thread(&h.store, "fork").unwrap()[0]["text"], "replaced");
        // A rolled-back run's app stops informing the agent.
        success(&rpc(&mut operator, 19, "mcpApps.updateModelContext", json!({"threadId":"thread","itemId":"app-rolled","conversationThreadId":"thread","content":[{"type":"text","text":"gone soon"}]})).await);
        assert_eq!(model_context_for_thread(&h.store, "thread").unwrap().len(), 0);
        success(&rpc(&mut operator, 20, "mcpApps.updateModelContext", context("fork", json!({"content":[]}))).await);
        assert_eq!(model_context_for_thread(&h.store, "fork").unwrap().len(), 0);

        // provider.uploadFeedback: the thread's active provider session, if it is live and supports it.
        let uploads = Arc::new(Mutex::new(Vec::<(Value, Option<String>)>::new()));
        let recorder = uploads.clone();
        let error = |exit: &Value| (failure(exit)["_tag"].as_str().map(str::to_owned), failure(exit)["cause"]["message"].as_str().map(str::to_owned));
        assert_eq!(failure(&rpc(&mut reader, 21, "provider.uploadFeedback", json!({"threadId":"thread"})).await)["_tag"], "EnvironmentAuthorizationError");
        edit(&h, "thread", |thread| thread["thread"]["activeProviderThreadId"] = json!("pt-gone"));
        assert_eq!(error(&rpc(&mut operator, 22, "provider.uploadFeedback", json!({"threadId":"thread"})).await).1.as_deref(), Some("The provider session is no longer running. Send a message first."));
        edit(&h, "thread", |thread| thread["thread"]["activeProviderThreadId"] = json!("pt-plain"));
        assert_eq!(error(&rpc(&mut operator, 23, "provider.uploadFeedback", json!({"threadId":"thread"})).await).1.as_deref(), Some("Provider 'claudeAgent' does not support feedback uploads."));
        launch(&h, "fresh");
        assert_eq!(error(&rpc(&mut operator, 24, "provider.uploadFeedback", json!({"threadId":"fresh"})).await).1.as_deref(), Some("No provider session has run in this thread yet."));
        assert_eq!(error(&rpc(&mut operator, 25, "provider.uploadFeedback", json!({"threadId":"nowhere"})).await).0.as_deref(), Some("ProviderUploadFeedbackError"));
        let _uploader = register("session-live", LiveSession { driver: "codex".into(), upload_feedback: Some(Arc::new(move |thread, reason| { recorder.lock().unwrap().push((thread["id"].clone(), reason)); Box::pin(async { Ok("feedback-1".to_owned()) }) })), mcp_apps: None });
        edit(&h, "thread", |thread| thread["thread"]["activeProviderThreadId"] = json!("pt-live"));
        let sent = success(&rpc(&mut operator, 26, "provider.uploadFeedback", json!({"threadId":"thread","reason":"it broke"})).await).clone();
        assert_eq!(sent, json!({"feedbackId":"feedback-1"}));
        assert_eq!(uploads.lock().unwrap()[0], (json!("pt-live"), Some("it broke".to_owned())));
        assert!(!defect(&rpc(&mut operator, 27, "provider.uploadFeedback", json!({"threadId":"thread","reason":"  "})).await).is_null());
        h.finish().await;
    }));
}
