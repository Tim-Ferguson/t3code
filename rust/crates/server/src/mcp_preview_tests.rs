//! The browser tools end to end through `McpControlTools::call`, against a host the test plays: what the host is asked, what the agent gets
//! back, and what the agent is told when something goes wrong. Time is paused.
use crate::{
    mcp_control::McpControlTools,
    mcp_invocation::{InvocationScope, McpCapability},
    persistence::Store,
    preview_automation_broker::{HostConnection, PreviewAutomationBroker},
    test_support::HANG_BOUND,
};
use serde_json::{Value, json};
use std::sync::Arc;
use t3_contracts::{Present, PreviewAutomationRequest, PreviewAutomationStreamEvent};

struct Env {
    tools: McpControlTools,
    scope: InvocationScope,
    broker: Arc<PreviewAutomationBroker>,
    state_dir: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

fn env() -> Env {
    let directory = tempfile::tempdir().unwrap();
    let (store, mut scope) = crate::mcp_invocation::tests::fixture_with(Store::open(directory.path().join("rust-state.sqlite")).unwrap());
    scope.capabilities.insert(McpCapability::Preview);
    // The caller owns a live run, so the tools that act as it are let through.
    let mut projection = store.projection("thread", "thread:1").unwrap().unwrap();
    projection["runs"] = json!([{"id":"run:1","status":"running"}]);
    store.transaction(|tx| crate::persistence::write_projection(tx, "thread", "thread:1", &projection)).unwrap();
    let broker = store.extension(PreviewAutomationBroker::default);
    let state_dir = directory.path().to_path_buf();
    Env { tools: McpControlTools { store, execution: None, clock: Arc::new(chrono::Utc::now), services: Default::default() }, scope, broker, state_dir, _directory: directory }
}

fn host(env: &Env) -> HostConnection {
    env.broker.connect(serde_json::from_value(json!({"clientId":"server-browser","environmentId":"environment-1","supportedOperations":serde_json::to_value(t3_contracts::PREVIEW_AUTOMATION_SERVER_OPERATIONS).unwrap()})).unwrap(), true)
}

/// Answers the next requests with `replies` in order, returning what was asked.
async fn serve(env: &Env, connection: &mut HostConnection, replies: Vec<Value>) -> Vec<PreviewAutomationRequest> {
    let mut asked = Vec::new();
    for reply in replies {
        let request = loop {
            match tokio::time::timeout(HANG_BOUND, connection.next()).await.expect("an event").expect("an open stream") {
                PreviewAutomationStreamEvent::Request(event) => break event.request,
                PreviewAutomationStreamEvent::Connected(_) => {}
            }
        };
        let mut response = json!({"clientId":"server-browser","connectionId":connection.connection_id(),"requestId":request.request_id});
        if reply.get("ok").is_some() {
            response.as_object_mut().unwrap().extend(reply.as_object().unwrap().clone());
        } else {
            response["ok"] = json!(true);
            response["result"] = reply;
        }
        env.broker.respond(serde_json::from_value(response).unwrap());
        asked.push(request);
    }
    asked
}

fn page_status(url: &str) -> Value {
    json!({"available":true,"visible":true,"tabId":"tab_1","url":url,"title":"Page","loading":false})
}
fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

#[tokio::test(start_paused = true)]
async fn status_goes_to_the_host_and_comes_back_cut_to_its_schema() {
    let env = env();
    let mut host = host(&env);
    let call = env.tools.call(&env.scope, "preview_status", json!({}));
    let mut reply = page_status("https://example.test/");
    reply["somethingTheSchemaDoesNotName"] = json!(1);
    let (result, asked) = tokio::join!(call, serve(&env, &mut host, vec![reply]));
    let result = result.unwrap();
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"], page_status("https://example.test/"));
    assert_eq!((asked[0].operation.as_str(), &asked[0].input), ("status", &json!({})));
}

#[tokio::test(start_paused = true)]
async fn open_folds_show_into_open_defaults_reuse_and_routes_by_tab() {
    let env = env();
    let mut host = host(&env);
    let call = env.tools.call(&env.scope, "preview_open", json!({"url":"localhost:5173","show":true,"tabId":"tab_7"}));
    let (result, asked) = tokio::join!(call, serve(&env, &mut host, vec![page_status("http://localhost:5173/")]));
    assert_eq!(result.unwrap()["isError"], false);
    assert_eq!(asked[0].input, json!({"url":"localhost:5173","show":true,"open":true,"reuseExistingTab":true}));
    assert_eq!(asked[0].tab_id_explicit, Some(Some(true)));
    // Said nothing about showing: that stays unsaid, since the user's own setting decides.
    let call = env.tools.call(&env.scope, "preview_open", json!({"reuseExistingTab":false}));
    let (result, asked) = tokio::join!(call, serve(&env, &mut host, vec![page_status("about:blank")]));
    assert_eq!(result.unwrap()["isError"], false);
    assert_eq!(asked[0].input, json!({"reuseExistingTab":false}));
}

#[tokio::test(start_paused = true)]
async fn an_action_answers_with_the_sites_icon_when_the_page_is_on_the_web() {
    let env = env();
    let mut host = host(&env);
    let call = env.tools.call(&env.scope, "preview_click", json!({"locator":"role=button[name='Go']","tabId":"tab_2","timeoutMs":2000,"button":"right"}));
    let (result, asked) = tokio::join!(call, serve(&env, &mut host, vec![json!({"clicked":true,"leak":"not in the schema"}), page_status("https://example.test/page")]));
    let result = result.unwrap();
    assert_eq!(result["structuredContent"], json!({"toolIcon":{"_tag":"website","pageUrl":"https://example.test/page"}}));
    // The tab is routed by, not sent as input; the tool's own timeout is the call's budget; the follow-up read is short and for the tab acted on.
    assert_eq!(asked[0].input, json!({"locator":"role=button[name='Go']","timeoutMs":2000,"button":"right"}));
    assert_eq!((asked[0].timeout_ms.0, asked[0].tab_id.present().map(|t| t.as_str().to_owned())), (2000, Some("tab_2".to_owned())));
    assert_eq!((asked[1].operation.as_str(), asked[1].timeout_ms.0), ("status", 500));

    // A page that is not on the web shows no icon, and a status read that fails costs the agent nothing.
    let call = env.tools.call(&env.scope, "preview_press", json!({"key":"Enter"}));
    let (result, _) = tokio::join!(call, serve(&env, &mut host, vec![json!({}), page_status("about:blank")]));
    assert_eq!(result.unwrap()["structuredContent"], json!({}));
    let call = env.tools.call(&env.scope, "preview_scroll", json!({"deltaY":100}));
    let failing = json!({"ok":false,"error":{"_tag":"PreviewAutomationTabNotFoundError","message":"gone"}});
    let (result, _) = tokio::join!(call, serve(&env, &mut host, vec![json!({}), failing]));
    assert_eq!(result.unwrap()["structuredContent"], json!({}));
}

#[tokio::test(start_paused = true)]
async fn evaluate_wraps_whatever_the_page_returned() {
    let env = env();
    let mut host = host(&env);
    for (answer, expected) in [(json!([1, 2]), json!([1, 2])), (json!("text"), json!("text")), (json!(null), Value::Null), (json!({"a": 1}), json!({"a": 1}))] {
        let call = env.tools.call(&env.scope, "preview_evaluate", json!({"expression":"document.title"}));
        let (result, asked) = tokio::join!(call, serve(&env, &mut host, vec![answer, page_status("https://example.test/")]));
        let result = result.unwrap();
        assert_eq!(result["structuredContent"]["value"], expected);
        assert_eq!(result["structuredContent"]["toolIcon"]["pageUrl"], "https://example.test/");
        assert_eq!(asked[0].input, json!({"expression":"document.title"}));
    }
}

#[tokio::test(start_paused = true)]
async fn a_failure_reaches_the_agent_as_the_text_the_original_computes() {
    let env = env();
    // No host at all.
    let result = env.tools.call(&env.scope, "preview_click", json!({"selector":"b"})).await.unwrap();
    assert_eq!(result["isError"], true);
    assert!(text(&result).starts_with("No preview automation host is available for click in environment environment-1."), "{}", text(&result));
    // A host that fails.
    let mut host = host(&env);
    let call = env.tools.call(&env.scope, "preview_type", json!({"text":"x","locator":"abcd"}));
    let failing = json!({"ok":false,"error":{"_tag":"PreviewAutomationTargetNotEditableError","message":"no","detail":{"selectorKind":"locator","selectorLength":4}}});
    let (result, _) = tokio::join!(call, serve(&env, &mut host, vec![failing]));
    assert_eq!(result.unwrap()["content"][0]["text"], "Preview automation type requires an editable locator (4 characters).");
    // A credential without the capability.
    let mut scope = env.scope.clone();
    scope.capabilities.shift_remove(&McpCapability::Preview);
    let result = env.tools.call(&scope, "preview_status", json!({})).await.unwrap();
    assert!(text(&result).contains("Do not retry them."));
}

#[tokio::test(start_paused = true)]
async fn a_host_answer_that_is_not_what_the_tool_promises_is_a_failure_not_a_forgery() {
    let env = env();
    let mut host = host(&env);
    let call = env.tools.call(&env.scope, "preview_status", json!({}));
    let (result, _) = tokio::join!(call, serve(&env, &mut host, vec![json!({"available":"yes"})]));
    assert_eq!(result.unwrap()["isError"], true);
}

fn snapshot(text_chars: usize, elements: usize) -> Value {
    json!({
        "url": "https://Example.test/a", "title": "A page", "loading": false, "visibleText": "t".repeat(text_chars),
        "interactiveElements": (0..elements).map(|i| json!({"tag":"button","role":"button","name":format!("Button {i} {}", "n".repeat(300)),"selector":format!("#b{i}"),"x":1,"y":2,"width":3,"height":4})).collect::<Vec<_>>(),
        "accessibilityTree": "- generic [ref=e1]",
        "consoleEntries": (0..60).map(|i| json!({"level":"log","text":format!("line {i} {}", "c".repeat(700)),"timestamp":"2026-10-11T00:00:00.000Z"})).collect::<Vec<_>>(),
        "networkEntries": [], "actionTimeline": [],
        "screenshot": {"mimeType":"image/png","data":"iVBORw0KGgo=","width":10,"height":20},
    })
}

#[tokio::test(start_paused = true)]
async fn a_snapshot_is_bounded_and_its_image_goes_out_as_an_image() {
    let env = env();
    let mut host = host(&env);
    let call = env.tools.call(&env.scope, "preview_snapshot", json!({}));
    let (result, asked) = tokio::join!(call, serve(&env, &mut host, vec![snapshot(30_000, 20)]));
    let result = result.unwrap();
    assert_eq!(result["isError"], false);
    assert_eq!(asked[0].input, json!({}));
    let content = result["content"].as_array().unwrap();
    assert_eq!(content[0]["text"], json!({"url":"https://Example.test/a"}).to_string());
    assert!(content[1]["text"].as_str().unwrap().len() <= crate::mcp_preview::MAX_SNAPSHOT_TEXT_BYTES, "the metadata text fits its ceiling");
    assert!(content[2]["text"].as_str().unwrap().starts_with("Snapshot text was bounded. Omitted: "));
    assert_eq!((content[3]["type"].as_str(), content[3]["data"].as_str(), content[3]["mimeType"].as_str()), (Some("image"), Some("iVBORw0KGgo="), Some("image/png")));
    let structured = &result["structuredContent"];
    assert_eq!(structured["screenshot"], json!({"mimeType":"image/png","width":10,"height":20}));
    assert_eq!(structured["accessibilityTree"], "- generic [ref=e1]");
    let omitted: Vec<&str> = structured["omitted"].as_array().unwrap().iter().map(|note| note.as_str().unwrap()).collect();
    assert!(omitted.contains(&"20 older console entries"), "{omitted:?}");
    assert!(omitted.contains(&"element names longer than 200 characters"), "{omitted:?}");
    assert!(omitted.contains(&"console entries text after 500 characters"), "{omitted:?}");
    assert!(omitted.iter().any(|note| note.starts_with("visibleText after ")), "{omitted:?}");
    assert!(omitted.contains(&"30 of 40 consoleEntries"), "{omitted:?}");
    assert!(omitted.contains(&"visibleText after 8000 characters (use preview_evaluate for more)"), "{omitted:?}");
    // The kept log entries are the newest, with their text cut.
    let kept = structured["consoleEntries"].as_array().unwrap();
    assert!(kept.len() == 10 && kept.last().unwrap()["text"].as_str().unwrap().starts_with("line 59 "));
    assert!(kept[0]["text"].as_str().unwrap().ends_with('…'));
    // The structured content is the bounded metadata of the text block, which the model reads in its place.
    assert_eq!(structured.to_string().len(), content[1]["text"].as_str().unwrap().len() + format!(",\"omitted\":{}", structured["omitted"]).len());
}

#[tokio::test(start_paused = true)]
async fn a_snapshot_with_too_many_elements_sheds_them_last_and_says_so() {
    let env = env();
    let mut host = host(&env);
    let call = env.tools.call(&env.scope, "preview_snapshot", json!({"includeImage":false}));
    let (result, _) = tokio::join!(call, serve(&env, &mut host, vec![snapshot(30_000, 300)]));
    let structured = &result.unwrap()["structuredContent"];
    let omitted: Vec<&str> = structured["omitted"].as_array().unwrap().iter().map(|note| note.as_str().unwrap()).collect();
    assert!(omitted.iter().any(|note| note.ends_with(" of 300 interactiveElements")), "{omitted:?}");
    // The logs and the page text went before any locator did.
    assert_eq!((structured["consoleEntries"].as_array().unwrap().len(), structured["visibleText"].as_str().unwrap()), (0, "…"));
    assert!(!structured["interactiveElements"].as_array().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_small_snapshot_is_whole_and_can_leave_its_image_out() {
    let env = env();
    let mut host = host(&env);
    let mut small = snapshot(10, 0);
    small["interactiveElements"] = json!([]);
    small["consoleEntries"] = json!([]);
    let call = env.tools.call(&env.scope, "preview_snapshot", json!({"includeImage":false,"tabId":"tab_3"}));
    let (result, asked) = tokio::join!(call, serve(&env, &mut host, vec![small]));
    let result = result.unwrap();
    assert_eq!(result["content"].as_array().unwrap().len(), 2);
    assert!(result["structuredContent"].get("omitted").is_none());
    assert_eq!(result["structuredContent"]["visibleText"], "tttttttttt");
    // `includeImage` and `save` are for the tool: the host is asked for a complete snapshot of the tab.
    assert_eq!((&asked[0].input, asked[0].tab_id.present().map(|t| t.as_str().to_owned())), (&json!({}), Some("tab_3".to_owned())));
}

#[tokio::test(start_paused = true)]
async fn saving_a_snapshot_writes_the_png_under_the_browser_artifacts_and_names_it() {
    let env = env();
    let mut host = host(&env);
    let call = env.tools.call(&env.scope, "preview_snapshot", json!({"save":true,"includeImage":false}));
    let (result, _) = tokio::join!(call, serve(&env, &mut host, vec![snapshot(10, 0)]));
    let result = result.unwrap();
    // Only the url and the path come back.
    let saved = &result["structuredContent"];
    assert_eq!(saved.as_object().unwrap().len(), 2);
    let path = std::path::PathBuf::from(saved["screenshotPath"].as_str().unwrap());
    assert_eq!(path.parent().unwrap().canonicalize().unwrap(), env.state_dir.canonicalize().unwrap().join("browser-artifacts"));
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("browser-screenshot-example-test-") && name.ends_with(".png"), "{name}");
    assert_eq!(std::fs::read(&path).unwrap(), [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    assert_eq!(result["content"].as_array().unwrap().len(), 1);

    // With the image too, the path rides in the metadata.
    let call = env.tools.call(&env.scope, "preview_snapshot", json!({"save":true}));
    let (result, _) = tokio::join!(call, serve(&env, &mut host, vec![snapshot(10, 0)]));
    let result = result.unwrap();
    assert!(result["structuredContent"]["screenshotPath"].as_str().is_some() && result["content"].as_array().unwrap().last().unwrap()["type"] == "image");
}

#[tokio::test(start_paused = true)]
async fn a_snapshot_failure_names_the_operation_and_carries_the_text_for_errors_the_server_wrote() {
    let env = env();
    let none = env.tools.call(&env.scope, "preview_snapshot", json!({})).await.unwrap();
    assert_eq!(none["isError"], true);
    assert_eq!(none["structuredContent"]["error"]["_tag"], "PreviewAutomationNoAvailableHostError");
    assert_eq!(none["structuredContent"]["error"]["operation"], "snapshot");
    assert!(text(&none).starts_with("Preview snapshot failed: No preview automation host is available for snapshot"));
    // A refusal from the access gate is not an automation error: only its tag is shown.
    let mut scope = env.scope.clone();
    scope.thread = None;
    scope.client = Some(crate::mcp_invocation::ClientCaller { session_id: "c".into(), label: "C".into(), access: t3_contracts::AuthMcpClientAccess::Auto });
    let outside = env.tools.call(&scope, "preview_snapshot", json!({})).await.unwrap();
    assert_eq!(text(&outside), "Preview snapshot failed: OrchestratorMcpFailure.");
    assert!(outside["structuredContent"]["error"].get("message").is_none());
    // A snapshot that does not fit its schema.
    let mut host = host(&env);
    let call = env.tools.call(&env.scope, "preview_snapshot", json!({}));
    let (broken, _) = tokio::join!(call, serve(&env, &mut host, vec![json!({"url":"x"})]));
    assert_eq!(text(&broken.unwrap()), "Preview snapshot failed: AiError.");
}

#[test]
fn bounding_keeps_what_fits_and_sheds_in_the_originals_order() {
    let metadata = |value: Value| value.as_object().unwrap().clone();
    // Nothing to shed.
    let (value, text, omitted) = crate::mcp_preview::bound_snapshot_metadata(&metadata(json!({"url":"u","title":"t","visibleText":"hello","interactiveElements":[],"consoleEntries":[],"networkEntries":[],"actionTimeline":[],"accessibilityTree":"tree"})));
    assert!(omitted.is_empty());
    assert_eq!(serde_json::to_string(&value).unwrap(), text);
    assert_eq!(value["accessibilityTree"], "tree");
    // An object tree is dropped, with a note; the url and the title are cut at 2048 characters.
    let long = "x".repeat(3000);
    let (value, _, omitted) = crate::mcp_preview::bound_snapshot_metadata(&metadata(json!({"url":long,"title":long,"visibleText":"","interactiveElements":[],"consoleEntries":[],"networkEntries":[],"actionTimeline":[],"accessibilityTree":{"role":"root"}})));
    assert!(value.get("accessibilityTree").is_none());
    assert_eq!(omitted, ["accessibilityTree (use interactiveElements locators or preview_evaluate)", "url or title after 2048 characters"]);
    assert_eq!(value["url"].as_str().unwrap().chars().count(), 2049);
    // Text is cut by UTF-16 units: an emoji counts twice.
    assert_eq!(crate::mcp_preview::cut_text("😀😀😀", 4), "😀😀…");
    assert_eq!(crate::mcp_preview::cut_text("😀😀😀", 5), "😀😀…");
    assert_eq!(crate::mcp_preview::cut_text("abc", 3), "abc");
}

#[tokio::test(start_paused = true)]
async fn stopping_a_recording_claims_the_pending_upload_for_the_thread() {
    let env = env();
    let mut host = host(&env);
    let attachments = env.state_dir.join("attachments");
    std::fs::create_dir_all(&attachments).unwrap();
    let uuid = "00000000-0000-4000-8000-0000000000aa";
    let pending = attachments.join(format!("pending-{uuid}-webm.webm"));
    std::fs::write(&pending, b"12345").unwrap();
    let artifact = |size: u64, uploaded: Value| {
        let mut artifact = json!({"id":"rec","tabId":"tab_1","path":"/desktop/rec.webm","mimeType":"video/webm","sizeBytes":size,"createdAt":"2026-10-11T00:00:00.000Z"});
        if !uploaded.is_null() {
            artifact["uploadedAttachmentId"] = uploaded;
        }
        artifact
    };
    let id = format!("pending-{uuid}-webm");

    // A size that is not the file's size is refused and the file stays where it was.
    let call = env.tools.call(&env.scope, "preview_recording_stop", json!({}));
    let (result, asked) = tokio::join!(call, serve(&env, &mut host, vec![artifact(6, json!(id)), page_status("https://example.test/")]));
    assert_eq!(text(&result.unwrap()), "Preview recording could not be saved to the agent environment. The saved copy remains on the desktop.");
    assert!(pending.exists());
    assert_eq!((asked[0].timeout_ms.0, asked[0].input.clone()), (120_000, json!({"transferToEnvironment":true})));

    // A host that did not upload (an older desktop) is told so.
    let call = env.tools.call(&env.scope, "preview_recording_stop", json!({}));
    let (result, _) = tokio::join!(call, serve(&env, &mut host, vec![artifact(5, Value::Null), page_status("https://example.test/")]));
    assert_eq!(text(&result.unwrap()), "Update the desktop app to transfer recordings. The recording remains on the desktop.");

    // An upload that belongs to no pending name, or that names another thread's file, is not claimed.
    for bad in ["thread-x-00000000-0000-4000-8000-0000000000aa-webm", "../etc/passwd"] {
        let call = env.tools.call(&env.scope, "preview_recording_stop", json!({}));
        let (result, _) = tokio::join!(call, serve(&env, &mut host, vec![artifact(5, json!(bad)), page_status("https://example.test/")]));
        assert_eq!(result.unwrap()["isError"], true, "{bad}");
    }
    assert!(pending.exists());

    // The right size claims it: renamed for the thread, and the artifact points at the new path.
    let call = env.tools.call(&env.scope, "preview_recording_stop", json!({"tabId":"tab_1"}));
    let (result, _) = tokio::join!(call, serve(&env, &mut host, vec![artifact(5, json!(id)), page_status("https://example.test/")]));
    let result = result.unwrap();
    assert_eq!(result["isError"], false, "{result}");
    let claimed = &result["structuredContent"];
    let expected = attachments.canonicalize().unwrap().join(format!("thread-1-{uuid}-webm.webm"));
    assert!(claimed["id"].as_str().unwrap().ends_with(&format!("-{uuid}-webm")));
    assert_eq!(claimed["path"], expected.to_str().unwrap());
    assert!(claimed.get("uploadedAttachmentId").is_none());
    assert_eq!(claimed["toolIcon"]["pageUrl"], "https://example.test/");
    assert!(!pending.exists() && expected.exists());
    // The same completed upload returned again (an overlapping stop) is found already claimed.
    let call = env.tools.call(&env.scope, "preview_recording_stop", json!({}));
    let (again, _) = tokio::join!(call, serve(&env, &mut host, vec![artifact(5, json!(id)), page_status("https://example.test/")]));
    assert_eq!(again.unwrap()["structuredContent"]["path"], expected.to_str().unwrap());
}

#[tokio::test(start_paused = true)]
async fn the_thread_lists_its_tabs_a_page_at_a_time_and_closes_them() {
    let env = env();
    let manager = env.tools.store.extension(crate::preview_manager::PreviewManager::default);
    let open = |value: Value| manager.open(serde_json::from_value(value).unwrap()).unwrap();
    let first = open(json!({"threadId":"thread:1"}));
    open(json!({"threadId":"thread:1"}));
    open(json!({"threadId":"thread:1"}));
    open(json!({"threadId":"other-thread"}));
    let page = env.tools.call(&env.scope, "t3_preview_list", json!({"limit":2})).await.unwrap();
    assert_eq!(page["isError"], false);
    let listed = &page["structuredContent"];
    assert_eq!((listed["sessions"].as_array().unwrap().len(), listed["nextCursor"].as_u64()), (2, Some(2)));
    assert_eq!(listed["serverEpoch"], manager.server_epoch());
    let rest = env.tools.call(&env.scope, "t3_preview_list", json!({"cursor":2,"limit":2})).await.unwrap();
    assert_eq!((rest["structuredContent"]["sessions"].as_array().unwrap().len(), rest["structuredContent"]["nextCursor"].is_null()), (1, true));

    // A desktop tab is closed in the sessions; a tab that is not there is not an error.
    let closed = env.tools.call(&env.scope, "t3_preview_close", json!({"tabId":first.tab_id.as_str()})).await.unwrap();
    assert_eq!(closed["structuredContent"], json!({}));
    assert_eq!(manager.list(serde_json::from_value(json!({"threadId":"thread:1"})).unwrap()).sessions.len(), 2);
    assert_eq!(env.tools.call(&env.scope, "t3_preview_close", json!({"tabId":"nope"})).await.unwrap()["isError"], false);
    // Another thread's tab is not this thread's to close.
    let foreign = manager.list(serde_json::from_value(json!({"threadId":"other-thread"})).unwrap()).sessions[0].tab_id.clone();
    env.tools.call(&env.scope, "t3_preview_close", json!({"tabId":foreign.as_str()})).await.unwrap();
    assert_eq!(manager.list(serde_json::from_value(json!({"threadId":"other-thread"})).unwrap()).sessions.len(), 1);

    // A tab of the server's browser is closed through that browser.
    let server_tab = open(json!({"threadId":"thread:1","runtime":"server"}));
    let mut host = host(&env);
    let call = env.tools.call(&env.scope, "t3_preview_close", json!({"tabId":server_tab.tab_id.as_str()}));
    let (result, asked) = tokio::join!(call, serve(&env, &mut host, vec![json!({})]));
    assert_eq!(result.unwrap()["structuredContent"], json!({}));
    assert_eq!(asked[0].operation.as_str(), "close");
    assert_eq!(asked[0].tab_id.present().map(|t| t.as_str().to_owned()), Some(server_tab.tab_id.as_str().to_owned()));
    // With no browser to close it, the failure is the generic one.
    drop(host);
    let result = env.tools.call(&env.scope, "t3_preview_close", json!({"tabId":server_tab.tab_id.as_str()})).await.unwrap();
    assert_eq!((result["isError"].as_bool(), serde_json::from_str::<Value>(text(&result)).unwrap()), (Some(true), json!({"_tag":"OrchestratorMcpFailure","code":"orchestration_error","message":"The operation could not be completed."})));
}
