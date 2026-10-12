//! The server browser against a scripted Chrome (apps/server/src/preview/ServerBrowser.test.ts, which mocks Playwright the same way): which
//! tabs exist for whom, what the agent's operations send to the page, who controls a tab, and what happens to the files and dialogs it opens.
//! The manager, the broker and the viewers are real; only the browser is played.
use super::*;
use crate::{
    mcp_invocation::InvocationScope,
    preview_automation_broker::Invocation,
    server_browser_contexts::{BrowserExecutable, ContextsOptions},
    server_browser_double::{Double, DoubleLauncher},
    server_browser_viewer::{ServerBrowserApi, ViewerOutput, ViewerStream},
    test_support::HANG_BOUND,
};
use async_trait::async_trait;
use t3_contracts::PreviewTabId;

struct Fixed;
#[async_trait]
impl BrowserExecutable for Fixed {
    async fn executable(&self) -> Result<PathBuf, crate::preview_browser_install::PreviewBrowserError> {
        Ok(PathBuf::from("/fake/chrome-headless-shell"))
    }
}

struct Fixture {
    browser: ServerBrowser,
    manager: Arc<PreviewManager>,
    broker: Arc<PreviewAutomationBroker>,
    double: Double,
    launcher: Arc<DoubleLauncher>,
    directory: tempfile::TempDir,
}

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, future).await.expect("the awaited event happened")
}

async fn fixture() -> Fixture {
    fixture_with(DoubleLauncher::new(Double::new())).await
}

async fn fixture_with(launcher: DoubleLauncher) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let double = launcher.double.clone();
    let launcher = Arc::new(launcher);
    let contexts = ServerBrowserContexts::new(ContextsOptions {
        profiles_dir: directory.path().join("server-browser").join("profiles"),
        downloads_dir: directory.path().join("server-browser").join("downloads"),
        executable: Arc::new(Fixed),
        launcher: launcher.clone(),
        no_sandbox: false,
        setup_command: "sudo t3 browser setup".into(),
        scratch_root: directory.path().to_owned(),
    });
    let manager = Arc::new(PreviewManager::default());
    let broker = Arc::new(PreviewAutomationBroker::default());
    let browser = ServerBrowser::start(ServerBrowserOptions {
        contexts,
        manager: manager.clone(),
        broker: broker.clone(),
        environment_id: "env-1".into(),
        downloads_dir: directory.path().join("server-browser").join("downloads"),
        attachments_dir: directory.path().join("attachments"),
        setup_command: "sudo t3 browser setup".into(),
        sandbox_blocked: false,
    });
    Fixture { browser, manager, broker, double, launcher, directory }
}

fn scope(session: &str) -> InvocationScope {
    serde_json::from_value(json!({
        "environmentId": "env-1", "requestNamespace": session, "issuedAt": 1, "capabilities": ["preview"],
        "thread": {"threadId": "thread-1", "providerSessionId": session, "providerInstanceId": "codex"}
    }))
    .unwrap()
}

/// An agent's tool call, through the broker to the browser registered as its host.
async fn call(fixture: &Fixture, session: &str, operation: PreviewAutomationOperation, input: Value) -> Result<Value, Value> {
    call_tab(fixture, session, operation, input, None).await
}

async fn call_tab(fixture: &Fixture, session: &str, operation: PreviewAutomationOperation, input: Value, tab: Option<&str>) -> Result<Value, Value> {
    let scope = scope(session);
    let mut invocation = Invocation::new(&scope, operation, input);
    invocation.tab_id = tab.map(|tab| PreviewTabId::new(tab).unwrap());
    within(fixture.broker.invoke(invocation)).await.map(|invoked| invoked.result).map_err(|error| serde_json::to_value(error).unwrap())
}

fn only_tab(fixture: &Fixture) -> Arc<Tab> {
    let tabs = fixture.browser.0.tab_list();
    assert_eq!(tabs.len(), 1);
    tabs[0].clone()
}

/// Waits for the tab to act on the next event the browser sends.
async fn after_event(tab: &Tab, send: impl FnOnce()) {
    let mut handled = tab.handled.subscribe();
    let seen = *handled.borrow_and_update();
    send();
    within(handled.wait_for(|count| *count > seen)).await.unwrap();
}

fn page_event(session: &str, method: &str, params: Value) -> Value {
    json!({"method": method, "sessionId": session, "params": params})
}

/// Plays the page's own answers to the helper script: `method` returns `value`.
fn script_runtime(double: &Double, method: &str, value: Value) {
    double.on_evaluate(&format!(".{method}("), value);
}

#[test]
fn the_browser_is_off_unless_the_switch_says_so() {
    let env = |value: Option<&'static str>| move |name: &str| (name == SWITCH).then(|| value.map(str::to_owned)).flatten();
    assert_eq!(SWITCH, "T3CODE_RUST_SERVER_BROWSER");
    for off in [None, Some(""), Some("0"), Some("false"), Some("yes"), Some("on")] {
        assert!(!switch_on(env(off)), "{off:?}");
    }
    for on in [Some("1"), Some("true"), Some(" 1 ")] {
        assert!(switch_on(env(on)), "{on:?}");
    }
    assert!(!switch_on(|_| None));
}

#[tokio::test]
async fn with_the_switch_off_nothing_starts_and_nothing_is_served() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::persistence::Store::open(directory.path().join("state.sqlite")).unwrap();
    assert!(start_for_store(&store, "env-1", false).await.is_none());
    assert!(crate::server_browser_http::installed(&store).is_none());
    // No process was started and no directory made for one.
    assert!(!directory.path().join("server-browser").exists());
    shutdown_for_store(&store).await;
}

#[tokio::test]
async fn an_agent_opens_a_tab_in_a_context_of_its_own_and_it_loads_and_reports() {
    let fixture = fixture().await;
    let mut events = fixture.manager.subscribe();
    fixture.double.on_evaluate("document.title", json!("Example"));
    let status = call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({"url": "example.com/a b"})).await.unwrap();
    assert_eq!((status["available"].clone(), status["url"].clone(), status["title"].clone(), status["loading"].clone()), (json!(true), json!("https://example.com/a%20b"), json!("Example"), json!(false)));
    assert_eq!(status["control"], json!({"owner": "agent", "ownedByCaller": true, "generation": 0}));
    assert_eq!(status["viewportSetting"], json!({"_tag": "fill"}));
    assert_eq!(status["viewport"], json!({"width": 1280, "height": 800}));
    assert_eq!(status["tabs"][0]["tabId"], status["tabId"]);
    // The agent's tab lives in a context of its own in the shared browser, never in a profile's.
    assert!(fixture.double.context_of("T-1").is_some());
    let launches = fixture.launcher.launches();
    assert_eq!(launches.len(), 1);
    assert!(launches[0].profile_directory.starts_with(fixture.directory.path()));
    assert!(!launches[0].profile_directory.starts_with(fixture.directory.path().join("server-browser").join("profiles")));
    // The manager's session follows the page: opened, loading, then loaded with the title.
    let mut kinds = Vec::new();
    loop {
        let event = serde_json::to_value(within(events.recv()).await.unwrap()).unwrap();
        kinds.push(event["type"].as_str().unwrap().to_owned());
        if event["snapshot"]["navStatus"]["_tag"] == "Success" {
            assert_eq!(event["snapshot"]["navStatus"], json!({"_tag": "Success", "url": "https://example.com/a%20b", "title": "Example"}));
            // The broker names a provider session by its environment and its own id.
            assert_eq!(event["snapshot"]["automationOwner"], "env-1\u{0}agent-a");
            break;
        }
    }
    assert_eq!(kinds[0], "opened");
    // The page was navigated once and given the pinned set-up of a tab.
    let commands = fixture.launcher.browser(0).commands();
    assert_eq!(commands.iter().filter(|command| command.method == "Page.navigate").count(), 1);
    assert!(commands.iter().any(|command| command.method == "Emulation.setDeviceMetricsOverride" && command.params["width"] == 1280 && command.params["deviceScaleFactor"] == 2.0));
}

#[tokio::test]
async fn a_persons_tab_uses_its_profile_on_disk() {
    let fixture = fixture().await;
    let snapshot = fixture.manager.open(serde_json::from_value(json!({"threadId": "thread-1", "runtime": "server", "profileId": "work", "url": "http://localhost:3000"})).unwrap()).unwrap();
    let tab = within(fixture.browser.0.find_tab("thread-1", snapshot.tab_id.as_str())).await.unwrap();
    // Its own browser, started in the profile's directory, whose default context has no protocol id.
    assert_eq!(fixture.launcher.launches()[0].profile_directory, fixture.directory.path().join("server-browser").join("profiles").join("work"));
    assert_eq!(tab.context.context_id(), None);
    assert_eq!((tab.profile_id.as_deref(), tab.control.agent_id()), (Some("work"), None));
}

#[tokio::test]
async fn an_agent_cannot_use_another_agents_tab() {
    let fixture = fixture().await;
    let status = call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    let tab_id = status["tabId"].as_str().unwrap().to_owned();
    let refused = call_tab(&fixture, "agent-b", PreviewAutomationOperation::Navigate, json!({"url": "example.com"}), Some(&tab_id)).await.unwrap_err();
    assert_eq!(refused["_tag"], "PreviewAutomationControlInterruptedError");
    assert_eq!(refused["reason"], "agentMismatch");
    // The refused call changed nothing in the page.
    assert!(fixture.launcher.browser(0).commands_named("Page.navigate").is_empty());
}

#[tokio::test]
async fn a_navigation_goes_where_the_tool_says_with_the_readiness_it_asks() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    let navigated = |input: Value| call(&fixture, "agent-a", PreviewAutomationOperation::Navigate, input);
    navigated(json!({"target": {"kind": "environment-port", "port": 5173, "path": "settings?tab=account"}})).await.unwrap();
    navigated(json!({"target": {"kind": "environment-port", "port": 8443, "protocol": "https"}, "readiness": "domContentLoaded"})).await.unwrap();
    navigated(json!({"url": "localhost:3000/x", "readiness": "none"})).await.unwrap();
    // A page that cannot be reached says why, in the browser's words.
    fixture.double.navigation_error("refused.test", "net::ERR_CONNECTION_REFUSED");
    let failed = navigated(json!({"url": "http://refused.test/"})).await.unwrap_err();
    assert_eq!(failed["_tag"], "PreviewAutomationExecutionError");
    let urls: Vec<String> = fixture.launcher.browser(0).commands_named("Page.navigate").iter().map(|command| command.params["url"].as_str().unwrap().to_owned()).collect();
    assert_eq!(urls, ["http://localhost:5173/settings?tab=account", "https://localhost:8443", "http://localhost:3000/x", "http://refused.test/"]);
}

/// TODO(original: a URL is checked for its scheme only): what is accepted today, written down so a change of policy is a visible change.
#[tokio::test]
async fn the_only_check_on_a_url_is_its_scheme_as_the_original_has_it() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    for refused in ["file:///etc/passwd", "ftp://example.com/", "chrome://version", "javascript:alert(1)", "data:text/html,<p>x</p>", "about:blank"] {
        let error = call(&fixture, "agent-a", PreviewAutomationOperation::Navigate, json!({"url": refused})).await.unwrap_err();
        assert_eq!(error["_tag"], "PreviewAutomationExecutionError", "{refused}");
    }
    assert!(fixture.launcher.browser(0).commands_named("Page.navigate").is_empty());
    // Loopback, private-network, link-local and cloud-metadata addresses are all taken as given.
    for accepted in ["http://127.0.0.1:8080/", "http://[::1]:8080/", "http://192.168.1.10/admin", "http://10.0.0.5/", "http://169.254.169.254/latest/meta-data/", "http://metadata.google.internal/"] {
        call(&fixture, "agent-a", PreviewAutomationOperation::Navigate, json!({"url": accepted})).await.unwrap();
    }
    assert_eq!(fixture.launcher.browser(0).commands_named("Page.navigate").len(), 6);
}

#[tokio::test]
async fn a_click_waits_for_its_target_and_acts_at_its_center() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    script_runtime(&fixture.double, "probe", json!({"status": "ok", "x": 40.5, "y": 20}));
    call(&fixture, "agent-a", PreviewAutomationOperation::Click, json!({"locator": "role=button[name='Save']", "button": "right", "clickCount": 2})).await.unwrap();
    let mouse: Vec<(String, String, u64)> = fixture
        .launcher
        .browser(0)
        .commands_named("Input.dispatchMouseEvent")
        .iter()
        .map(|command| (command.params["type"].as_str().unwrap().to_owned(), command.params["button"].as_str().unwrap().to_owned(), command.params["clickCount"].as_u64().unwrap()))
        .collect();
    let pair = |button: &str, count| [("mousePressed".to_owned(), button.to_owned(), count), ("mouseReleased".to_owned(), button.to_owned(), count)];
    let expected: Vec<_> = [("mouseMoved".to_owned(), "none".to_owned(), 0)].into_iter().chain(pair("right", 1)).chain(pair("right", 2)).collect();
    assert_eq!(mouse, expected);
    assert!(fixture.launcher.browser(0).commands_named("Input.dispatchMouseEvent").iter().all(|command| command.params["x"] == 40.5 && command.params["y"] == 20.0));
    // The action is in the tab's timeline.
    let tab = only_tab(&fixture);
    let timeline = tab.state().timeline.clone();
    assert_eq!((timeline[0]["action"].clone(), timeline[0]["status"].clone()), (json!("click"), json!("succeeded")));
}

#[tokio::test]
async fn a_click_that_opens_a_dialog_returns_and_the_dialog_is_resolved_explicitly() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    script_runtime(&fixture.double, "probe", json!({"status": "ok", "x": 1, "y": 2}));
    // The page's click handler opens a dialog, which holds the browser's reply to the release of the button.
    let tab = only_tab(&fixture);
    // The press and release of the button are the second and third mouse commands; the release is the one that never returns.
    let held = fixture.double.hold("Input.dispatchMouseEvent", 2);
    let browser = fixture.launcher.browser(0);
    let session = tab.page.session_id().to_owned();
    let clicking = call(&fixture, "agent-a", PreviewAutomationOperation::Click, json!({"selector": "#delete"}));
    let opening = async {
        held.notified().await;
        after_event(&tab, || browser.push(page_event(&session, "Page.javascriptDialogOpening", json!({"type": "confirm", "message": "Delete?", "defaultPrompt": "", "url": "http://x/"})))).await;
    };
    let (clicked, ()) = within(async { tokio::join!(clicking, opening) }).await;
    clicked.unwrap();
    let status = call(&fixture, "agent-a", PreviewAutomationOperation::Status, json!({})).await.unwrap();
    assert_eq!(status["dialog"], json!({"type": "confirm", "message": "Delete?", "defaultValue": ""}));
    // Nothing else runs under a pending dialog, and the page is not evaluated.
    let blocked = call(&fixture, "agent-a", PreviewAutomationOperation::Evaluate, json!({"expression": "1+1"})).await.unwrap_err();
    assert_eq!((blocked["_tag"].clone(), blocked["reason"].clone()), (json!("PreviewAutomationControlInterruptedError"), json!("dialogPending")));
    assert!(fixture.launcher.browser(0).commands_named("Runtime.evaluate").iter().all(|command| !command.params["expression"].as_str().unwrap().contains("1+1")));
    let resolved = call(&fixture, "agent-a", PreviewAutomationOperation::Dialog, json!({"accept": true, "promptText": "yes"})).await.unwrap();
    assert!(resolved["dialog"].is_null());
    let handled = fixture.launcher.browser(0).commands_named("Page.handleJavaScriptDialog");
    assert_eq!(handled[0].params, json!({"accept": true, "promptText": "yes"}));
    fixture.double.release(&browser, "Input.dispatchMouseEvent");
}

#[tokio::test]
async fn evaluate_runs_what_the_agent_wrote_and_returns_what_it_gets_up_to_the_limit() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    // TODO(original: the expression is not inspected): any script runs in the page, and the answer is cut at 64 KB only.
    fixture.double.on_evaluate("document.cookie", json!("session=abc"));
    let value = call(&fixture, "agent-a", PreviewAutomationOperation::Evaluate, json!({"expression": "document.cookie"})).await.unwrap();
    assert_eq!(value, json!("session=abc"));
    fixture.double.on_evaluate("big()", json!("x".repeat(70_000)));
    let too_large = call(&fixture, "agent-a", PreviewAutomationOperation::Evaluate, json!({"expression": "big()"})).await.unwrap_err();
    assert_eq!(too_large["_tag"], "PreviewAutomationResultTooLargeError");
    fixture.double.on_evaluate("boom()", json!({"__exception": "Error: boom\n    at <anonymous>:1:1"}));
    let failed = call(&fixture, "agent-a", PreviewAutomationOperation::Evaluate, json!({"expression": "boom()"})).await.unwrap_err();
    assert_eq!(failed["_tag"], "PreviewAutomationExecutionError");
}

#[tokio::test]
async fn a_snapshot_hands_out_refs_that_the_next_snapshot_and_navigation_revoke() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({"url": "http://localhost:3000"})).await.unwrap();
    script_runtime(
        &fixture.double,
        "snapshot",
        json!({"url": "http://localhost:3000/", "title": "App", "loading": false, "visibleText": "Hello", "tree": "- button \"Save\" [ref=e1]\n- link \"Home\" [ref=e2]"}),
    );
    let snapshot = call(&fixture, "agent-a", PreviewAutomationOperation::Snapshot, json!({})).await.unwrap();
    let tree = snapshot["accessibilityTree"].as_str().unwrap().to_owned();
    let reference = regex::Regex::new(r"\[ref=(t3-[^\]]+-e1)\]").unwrap().captures(&tree).unwrap()[1].to_owned();
    assert_eq!((snapshot["title"].clone(), snapshot["visibleText"].clone(), snapshot["screenshot"]["mimeType"].clone()), (json!("App"), json!("Hello"), json!("image/png")));
    assert_eq!(snapshot["screenshot"]["data"], "ZnJhbWU=");
    script_runtime(&fixture.double, "probe", json!({"status": "ok", "x": 1, "y": 1}));
    call(&fixture, "agent-a", PreviewAutomationOperation::Click, json!({"locator": format!("aria-ref={reference}")})).await.unwrap();
    // The page is asked for its own name for the element, not the one the agent saw.
    let asked: Vec<String> = fixture.launcher.browser(0).commands_named("Runtime.evaluate").iter().map(|command| command.params["expression"].as_str().unwrap().to_owned()).filter(|expression| expression.contains(".probe(")).collect();
    assert!(asked[0].contains(r#""aria-ref=e1""#), "{}", asked[0]);
    // A navigation revokes every ref, and the stale one is refused before the page is asked anything.
    call(&fixture, "agent-a", PreviewAutomationOperation::Navigate, json!({"url": "http://localhost:3000/next"})).await.unwrap();
    let stale = call(&fixture, "agent-a", PreviewAutomationOperation::Click, json!({"locator": format!("aria-ref={reference}")})).await.unwrap_err();
    assert_eq!(stale["_tag"], "PreviewAutomationInvalidSelectorError");
    assert_eq!(stale["staleRef"], true);
}

#[tokio::test]
async fn a_tab_belongs_to_one_agent_and_the_tab_limit_stops_it_until_idle_tabs_close() {
    let fixture = fixture().await;
    for _ in 0..AGENT_TAB_LIMIT {
        call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({"reuseExistingTab": false})).await.unwrap();
    }
    let refused = call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({"reuseExistingTab": false})).await.unwrap_err();
    assert_eq!((refused["_tag"].clone(), refused["reason"].clone()), (json!("PreviewAutomationControlInterruptedError"), json!("tabLimit")));
    // Another agent still has room.
    call(&fixture, "agent-b", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    assert_eq!(fixture.browser.tab_count(), AGENT_TAB_LIMIT + 1);
}

#[tokio::test(start_paused = true)]
async fn agent_tabs_nobody_watches_close_when_the_agent_stops_using_them() {
    let fixture = fixture().await;
    let status = call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    let mut events = fixture.manager.subscribe();
    assert_eq!(fixture.browser.tab_count(), 1);
    tokio::time::advance(AGENT_TAB_IDLE + IDLE_SWEEP_INTERVAL * 2).await;
    // The sweep closes the tab and its session.
    loop {
        let event = serde_json::to_value(within(events.recv()).await.unwrap()).unwrap();
        if event["type"] == "closed" {
            assert_eq!(event["tabId"], status["tabId"]);
            break;
        }
    }
    assert_eq!(fixture.browser.tab_count(), 0);
    assert_eq!(fixture.manager.list(serde_json::from_value(json!({"threadId": "thread-1"})).unwrap()).sessions.len(), 0);
    // A new open after the close is a new tab.
    let again = call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    assert_ne!(again["tabId"], status["tabId"]);
}

#[tokio::test]
async fn closing_a_tab_ends_its_page_and_its_session_and_a_session_closed_elsewhere_ends_its_tab() {
    let fixture = fixture().await;
    let status = call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    let tab = only_tab(&fixture);
    call(&fixture, "agent-a", PreviewAutomationOperation::Close, json!({})).await.unwrap();
    assert_eq!(fixture.browser.tab_count(), 0);
    tab.ended().await;
    assert!(tab.page.is_closed());
    assert_eq!(fixture.launcher.browser(0).commands_named("Target.closeTarget").len(), 1);
    assert!(fixture.manager.session("thread-1", status["tabId"].as_str().unwrap()).is_none());
    // A session the manager closes, such as one a person closed from the panel, takes its tab with it.
    let person = fixture.manager.open(serde_json::from_value(json!({"threadId": "thread-1", "runtime": "server"})).unwrap()).unwrap();
    let person_tab = within(fixture.browser.0.find_tab("thread-1", person.tab_id.as_str())).await.unwrap();
    let mut mirrored = fixture.browser.0.mirrored.subscribe();
    let seen = *mirrored.borrow_and_update();
    fixture.manager.close(serde_json::from_value(json!({"threadId": "thread-1", "tabId": person.tab_id.as_str()})).unwrap());
    within(mirrored.wait_for(|count| *count > seen)).await.unwrap();
    person_tab.ended().await;
    assert!(person_tab.closing.load(Ordering::SeqCst) && fixture.browser.tab_count() == 0);
}

#[tokio::test]
async fn a_popup_becomes_the_agents_own_tab_and_keeps_its_opener() {
    let fixture = fixture().await;
    let opener = call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    let mut browser_events = fixture.browser.0.browser_handled.subscribe();
    let seen = *browser_events.borrow_and_update();
    fixture.launcher.browser(0).push(json!({"method": "Target.targetCreated", "params": {"targetInfo": {"targetId": "T-9", "type": "page", "openerId": "T-1", "url": "https://login.example.com/"}}}));
    within(browser_events.wait_for(|count| *count > seen)).await.unwrap();
    // The popup is a session of the manager's now; waiting for its tab is waiting for the one opening both share.
    let sessions = serde_json::to_value(&fixture.manager.list(serde_json::from_value(json!({"threadId": "thread-1"})).unwrap()).sessions).unwrap();
    let popup_id = sessions.as_array().unwrap().iter().find(|session| session["tabId"] != opener["tabId"]).unwrap()["tabId"].as_str().unwrap().to_owned();
    within(fixture.browser.0.find_tab("thread-1", &popup_id)).await.unwrap();
    let status = call(&fixture, "agent-a", PreviewAutomationOperation::Status, json!({})).await.unwrap();
    assert_eq!(status["tabs"].as_array().unwrap().len(), 2);
    let popup = status["tabs"].as_array().unwrap().iter().find(|tab| tab["tabId"] != opener["tabId"]).unwrap().clone();
    assert_eq!(popup["openerTabId"], opener["tabId"]);
    // The popup attached to the page the browser opened, in the opener's context and owned by the opener's agent.
    let tabs = fixture.browser.0.tab_list();
    let popup_tab = tabs.iter().find(|tab| tab.page.target_id() == "T-9").unwrap();
    assert!(Arc::ptr_eq(&popup_tab.context, &tabs.iter().find(|tab| tab.page.target_id() == "T-1").unwrap().context));
    assert_eq!(popup_tab.control.agent_id(), Some("env-1\u{0}agent-a"));
    // Popups from a page that is not a tab's are not adopted.
    fixture.launcher.browser(0).push(json!({"method": "Target.targetCreated", "params": {"targetInfo": {"targetId": "T-10", "type": "page", "openerId": "T-404", "url": "https://x/"}}}));
    fixture.launcher.browser(0).push(json!({"method": "Target.targetCreated", "params": {"targetInfo": {"targetId": "T-11", "type": "service_worker", "openerId": "T-1", "url": "https://x/"}}}));
    assert_eq!(fixture.browser.tab_count(), 2);
}

#[tokio::test]
async fn a_page_download_is_kept_offered_to_the_controller_and_listed_for_the_agent() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    let tab = only_tab(&fixture);
    let viewer = within(fixture.browser.0.attach_viewer("thread-1", &tab.tab_id, 800, 600, 70, true)).await.unwrap();
    // The agent owns the tab, so the viewer takes control before a download is offered to it.
    viewer.input(json!({"type": "takeControl"})).await;
    let incoming = fixture.directory.path().join("server-browser").join("downloads").join("incoming");
    std::fs::create_dir_all(&incoming).unwrap();
    std::fs::write(incoming.join("guid-1"), b"hello").unwrap();
    let browser = fixture.launcher.browser(0);
    browser.push(json!({"method": "Browser.downloadWillBegin", "params": {"frameId": "T-1", "guid": "guid-1", "url": "http://localhost/report.csv", "suggestedFilename": "report.csv"}}));
    browser.push(json!({"method": "Browser.downloadProgress", "params": {"guid": "guid-1", "state": "completed", "totalBytes": 5, "receivedBytes": 5}}));
    let offered = within(async {
        loop {
            if let ViewerOutput::Download { id, file_name, size_bytes } = viewer.next().await.unwrap() {
                break (id, file_name, size_bytes);
            }
        }
    })
    .await;
    assert_eq!((offered.1.as_str(), offered.2), ("report.csv", 5));
    let (path, name) = fixture.browser.open_download("thread-1", &tab.tab_id, &offered.0).unwrap();
    assert_eq!((std::fs::read(&path).unwrap(), name.as_str()), (b"hello".to_vec(), "report.csv"));
    assert!(path.starts_with(fixture.directory.path().join("server-browser").join("downloads")) && !incoming.join("guid-1").exists());
    // Another tab's download cannot be asked for through this one.
    assert!(fixture.browser.open_download("thread-1", "other", &offered.0).is_none());
    assert!(fixture.browser.open_download("thread-1", &tab.tab_id, "nope").is_none());
    viewer.input(json!({"type": "releaseControl"})).await;
    let status = call(&fixture, "agent-a", PreviewAutomationOperation::Status, json!({})).await.unwrap();
    assert_eq!(status["downloads"][0]["fileName"], "report.csv");
    assert_eq!(status["downloads"][0]["sizeBytes"], 5);
    // The files go with the tab.
    let directory = path.parent().unwrap().to_owned();
    viewer.close().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Close, json!({})).await.unwrap();
    tab.ended().await;
    assert!(!directory.exists());
}

#[tokio::test]
async fn a_file_picker_goes_to_the_agent_by_path_and_to_a_viewer_by_upload() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    let tab = only_tab(&fixture);
    let (browser, session) = (fixture.launcher.browser(0), tab.page.session_id().to_owned());
    fixture.double.on_evaluate("getAttribute", json!("image/*"));
    let opened = |multiple: bool, node: i64| page_event(&session, "Page.fileChooserOpened", json!({"frameId": "T-1", "mode": if multiple { "selectMultiple" } else { "selectSingle" }, "backendNodeId": node}));
    // Without a picker open, there is nothing to answer.
    let none = call(&fixture, "agent-a", PreviewAutomationOperation::Upload, json!({"paths": ["/tmp/a.png"]})).await.unwrap_err();
    assert_eq!(none["_tag"], "PreviewAutomationExecutionError");
    after_event(&tab, || browser.push(opened(false, 7))).await;
    let status = call(&fixture, "agent-a", PreviewAutomationOperation::Status, json!({})).await.unwrap();
    assert_eq!(status["fileChooser"], json!({"multiple": false, "accept": ""}));
    // Paths must be absolute, and a single-file picker takes one.
    let relative = call(&fixture, "agent-a", PreviewAutomationOperation::Upload, json!({"paths": ["a.png"]})).await.unwrap_err();
    assert!(relative["reason"].as_str().unwrap_or_default().contains("absolute") || relative["_tag"] == "PreviewAutomationExecutionError");
    let two = call(&fixture, "agent-a", PreviewAutomationOperation::Upload, json!({"paths": ["/tmp/a.png", "/tmp/b.png"]})).await.unwrap_err();
    assert_eq!(two["_tag"], "PreviewAutomationExecutionError");
    // TODO(original: any readable file can be given to the page): an absolute path anywhere on the machine is passed to the browser as is.
    call(&fixture, "agent-a", PreviewAutomationOperation::Upload, json!({"paths": ["/etc/hostname"]})).await.unwrap();
    let set = browser.commands_named("DOM.setFileInputFiles");
    assert_eq!(set[0].params, json!({"files": ["/etc/hostname"], "backendNodeId": 7}));
    // The picker is closed once answered; a viewer can answer the next with bytes it uploads.
    let status = call(&fixture, "agent-a", PreviewAutomationOperation::Status, json!({})).await.unwrap();
    assert!(status["fileChooser"].is_null());
    after_event(&tab, || browser.push(opened(true, 8))).await;
    let chooser = tab.state().file_chooser.clone().unwrap();
    let uploaded = vec![UploadedFile { name: "../../etc/evil.txt".into(), mime_type: "text/plain".into(), bytes: b"data".to_vec() }, UploadedFile { name: "b.txt".into(), mime_type: "text/plain".into(), bytes: b"b".to_vec() }];
    assert!(!fixture.browser.answer_file_chooser("thread-1", &tab.tab_id, "stale", vec![]).await);
    assert!(fixture.browser.answer_file_chooser("thread-1", &tab.tab_id, &chooser.id, uploaded).await);
    let given = browser.commands_named("DOM.setFileInputFiles");
    let files: Vec<String> = given[1].params["files"].as_array().unwrap().iter().map(|file| file.as_str().unwrap().to_owned()).collect();
    // The name the person's browser sent is reduced to its last component, inside a directory made for this upload.
    assert!(files[0].ends_with("/0/evil.txt") && files[1].ends_with("/1/b.txt"), "{files:?}");
    assert!(files[0].starts_with(fixture.directory.path().join("server-browser").join("downloads").join("uploads").to_str().unwrap()));
    // And the picker is gone: nothing more to answer.
    assert!(!fixture.browser.answer_file_chooser("thread-1", &tab.tab_id, &chooser.id, vec![]).await);
}

#[tokio::test]
async fn a_recording_is_encoded_in_a_scratch_page_and_stored_as_a_pending_attachment() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    fixture.double.on_evaluate("__t3Recorder.stop()", json!({"mimeType": "video/webm;codecs=vp9", "count": 2, "bytes": 5}));
    fixture.double.on_evaluate("__t3Recorder.chunk(0)", json!("AQID"));
    fixture.double.on_evaluate("__t3Recorder.chunk(1)", json!("BAU="));
    let started = call(&fixture, "agent-a", PreviewAutomationOperation::RecordingStart, json!({})).await.unwrap();
    assert_eq!(started["recording"], true);
    // A second start reports the one that is running.
    let again = call(&fixture, "agent-a", PreviewAutomationOperation::RecordingStart, json!({})).await.unwrap();
    assert_eq!(again["startedAt"], started["startedAt"]);
    let stopped = call(&fixture, "agent-a", PreviewAutomationOperation::RecordingStop, json!({})).await.unwrap();
    assert_eq!((stopped["mimeType"].clone(), stopped["sizeBytes"].clone()), (json!("video/webm"), json!(5)));
    let path = PathBuf::from(stopped["path"].as_str().unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), [1, 2, 3, 4, 5]);
    assert!(path.starts_with(fixture.directory.path().join("attachments")));
    assert!(stopped["id"].as_str().unwrap().starts_with("pending-"));
    // Nothing is recording now.
    // The broker has no typed error for it, so the agent reads it as the failure it is.
    let none = call(&fixture, "agent-a", PreviewAutomationOperation::RecordingStop, json!({})).await.unwrap_err();
    assert_eq!(none["_tag"], "PreviewAutomationExecutionError");
}

#[tokio::test]
async fn a_resize_and_an_appearance_change_reach_the_page_and_the_manager() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    let resized = call(&fixture, "agent-a", PreviewAutomationOperation::Resize, json!({"mode": "preset", "preset": "iphone-12-pro", "orientation": "landscape"})).await.unwrap();
    assert_eq!(resized["setting"], json!({"_tag": "preset", "width": 844, "height": 390, "presetId": "iphone-12-pro"}));
    assert_eq!(resized["viewport"], json!({"width": 844, "height": 390}));
    let dark = call(&fixture, "agent-a", PreviewAutomationOperation::SetColorScheme, json!({"colorScheme": "dark"})).await.unwrap();
    assert_eq!(dark["colorScheme"], "dark");
    let browser = fixture.launcher.browser(0);
    let overrides = browser.commands_named("Emulation.setDeviceMetricsOverride");
    assert_eq!((overrides.last().unwrap().params["width"].clone(), overrides.last().unwrap().params["height"].clone()), (json!(844), json!(390)));
    assert_eq!(browser.commands_named("Emulation.setEmulatedMedia").last().unwrap().params["features"], json!([{"name": "prefers-color-scheme", "value": "dark"}]));
    // The manager's session carries both, for every other client.
    let tab = only_tab(&fixture);
    let session = fixture.manager.session("thread-1", &tab.tab_id).unwrap();
    let wire = serde_json::to_value(&session).unwrap();
    assert_eq!((wire["viewport"]["presetId"].clone(), wire["colorScheme"].clone()), (json!("iphone-12-pro"), json!("dark")));
    // Another client's change to the session is applied to the page.
    let before = browser.commands_named("Emulation.setDeviceMetricsOverride").len();
    let reloading = fixture.double.hold("Page.reload", 0);
    fixture.manager.resize(serde_json::from_value(json!({"threadId": "thread-1", "tabId": tab.tab_id, "viewport": {"_tag": "freeform", "width": 600, "height": 500}})).unwrap()).unwrap();
    fixture.manager.adjust(serde_json::from_value(json!({"threadId": "thread-1", "tabId": tab.tab_id, "zoomFactor": 1.5, "hardReload": true})).unwrap()).unwrap();
    // The reload is the last thing the tab does with the second change, so the settings before it have reached the page.
    within(reloading.notified()).await;
    assert!(browser.commands_named("Emulation.setDeviceMetricsOverride").len() > before);
    assert_eq!(browser.commands_named("Page.reload")[0].params, json!({"ignoreCache": true}));
    // At 1.5 zoom the page lays out in fewer CSS pixels and draws each one larger.
    let last = browser.commands_named("Emulation.setDeviceMetricsOverride").last().unwrap().params.clone();
    assert_eq!((last["width"].clone(), last["height"].clone(), last["deviceScaleFactor"].clone()), (json!(400), json!(333), json!(3.0)));
    fixture.double.release(&browser, "Page.reload");
}

#[tokio::test]
async fn clearing_a_profile_through_the_browser_closes_its_tabs_and_deletes_its_storage() {
    let fixture = fixture().await;
    let snapshot = fixture.manager.open(serde_json::from_value(json!({"threadId": "thread-1", "runtime": "server", "profileId": "work"})).unwrap()).unwrap();
    let tab = within(fixture.browser.0.find_tab("thread-1", snapshot.tab_id.as_str())).await.unwrap();
    let directory = fixture.launcher.launches()[0].profile_directory.clone();
    std::fs::write(directory.join("Cookies"), "x").unwrap();
    let mut events = fixture.manager.subscribe();
    ServerBrowserApi::clear_profile(&fixture.browser, "work").await.unwrap();
    assert!(!directory.exists());
    // The tab ended with its context, and its session with it.
    loop {
        let event = serde_json::to_value(within(events.recv()).await.unwrap()).unwrap();
        if event["type"] == "closed" {
            break;
        }
    }
    assert!(tab.closing.load(Ordering::SeqCst) && fixture.browser.tab_count() == 0);
    // The session ended with its storage.
    assert!(fixture.manager.session("thread-1", snapshot.tab_id.as_str()).is_none());
}

#[tokio::test]
async fn a_viewer_gets_the_tabs_state_a_still_and_pointer_moves_and_can_take_over() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    let tab = only_tab(&fixture);
    let reader = within(fixture.browser.0.attach_viewer("thread-1", &tab.tab_id, 800, 600, 70, false)).await.unwrap();
    let mut seen = Vec::new();
    // The viewer is told who controls the tab and how large the page is, then given a still of an idle page.
    let still = within(async {
        loop {
            match reader.next().await.unwrap() {
                ViewerOutput::Frame { data, ack } => {
                    ack.release();
                    break data;
                }
                other => seen.push(other.to_json().unwrap()),
            }
        }
    })
    .await;
    assert_eq!(still, b"frame");
    assert!(seen.contains(&json!({"type": "control", "canOperate": false, "controller": "agent", "generation": 0, "dialog": null})), "{seen:?}");
    assert!(seen.contains(&json!({"type": "viewport", "width": 1280, "height": 800})), "{seen:?}");
    // The screencast was asked for at the viewer's size and quality.
    let started = fixture.launcher.browser(0).commands_named("Page.startScreencast");
    assert_eq!(started[0].params, json!({"format": "jpeg", "quality": 70, "maxWidth": 800.0, "maxHeight": 600.0}));
    // A read-only viewer cannot take over or type: its input never reaches the page.
    reader.input(json!({"type": "takeControl"})).await;
    reader.input(json!({"type": "key", "action": "down", "key": "a", "text": "a"})).await;
    assert!(tab.control.controller().is_none());
    assert!(fixture.launcher.browser(0).commands_named("Input.dispatchKeyEvent").is_empty());
    // One that may operate takes over from the agent, and its input reaches the page.
    let operator = within(fixture.browser.0.attach_viewer("thread-1", &tab.tab_id, 800, 600, 70, true)).await.unwrap();
    operator.input(json!({"type": "takeControl"})).await;
    assert!(tab.control.controller().is_some());
    let agent_blocked = call(&fixture, "agent-a", PreviewAutomationOperation::Navigate, json!({"url": "example.com"})).await.unwrap_err();
    assert_eq!(agent_blocked["reason"], "humanControl");
    operator.input(json!({"type": "key", "action": "down", "key": "a", "code": "KeyA", "text": "a", "modifiers": 0})).await;
    operator.input(json!({"type": "mouse", "action": "down", "x": 10, "y": 20, "button": "left", "buttons": 1})).await;
    let browser = fixture.launcher.browser(0);
    assert_eq!(browser.commands_named("Input.dispatchKeyEvent")[0].params["text"], "a");
    assert_eq!(browser.commands_named("Input.dispatchMouseEvent")[0].params["type"], "mousePressed");
    // Letting go while a key and a button are down releases them in the page.
    operator.close().await;
    assert_eq!(browser.commands_named("Input.dispatchKeyEvent").last().unwrap().params["type"], "keyUp");
    assert_eq!(browser.commands_named("Input.dispatchMouseEvent").last().unwrap().params["type"], "mouseReleased");
    assert!(tab.control.controller().is_none());
    // The agent drives again.
    call(&fixture, "agent-a", PreviewAutomationOperation::Navigate, json!({"url": "example.com"})).await.unwrap();
    reader.close().await;
    assert!(tab.viewers().is_empty());
}

#[tokio::test]
async fn a_navigation_a_viewer_asks_for_is_checked_like_an_agents_and_history_is_the_pages() {
    let fixture = fixture().await;
    let snapshot = fixture.manager.open(serde_json::from_value(json!({"threadId": "thread-1", "runtime": "server"})).unwrap()).unwrap();
    let viewer = within(fixture.browser.0.attach_viewer("thread-1", snapshot.tab_id.as_str(), 800, 600, 70, true)).await.unwrap();
    let browser = fixture.launcher.browser(0);
    // No agent owns this tab, so the viewer that attached first controls it.
    viewer.input(json!({"type": "navigate", "url": "file:///etc/passwd"})).await;
    viewer.input(json!({"type": "navigate", "url": "localhost:4000"})).await;
    let urls: Vec<String> = browser.commands_named("Page.navigate").iter().map(|command| command.params["url"].as_str().unwrap().to_owned()).collect();
    assert_eq!(urls, ["http://localhost:4000/"]);
    fixture.double.history(json!({"currentIndex": 1, "entries": [{"id": 11}, {"id": 12}]}));
    viewer.input(json!({"type": "history", "delta": -1})).await;
    viewer.input(json!({"type": "reload", "ignoreCache": true})).await;
    assert_eq!(browser.commands_named("Page.reload")[0].params, json!({"ignoreCache": true}));
    assert_eq!(browser.commands_named("Page.navigateToHistoryEntry")[0].params, json!({"entryId": 11}));
    viewer.close().await;
}

#[tokio::test]
async fn a_viewer_that_stops_reading_loses_frames_but_never_the_state_it_cannot_miss() {
    let fixture = fixture().await;
    call(&fixture, "agent-a", PreviewAutomationOperation::Open, json!({})).await.unwrap();
    let tab = only_tab(&fixture);
    let handle = within(fixture.browser.0.attach_viewer("thread-1", &tab.tab_id, 800, 600, 70, false)).await.unwrap();
    // Read what the attach sent, up to the still.
    within(async {
        loop {
            if let ViewerOutput::Frame { ack, .. } = handle.next().await.unwrap() {
                ack.release();
                break;
            }
        }
    })
    .await;
    let viewer = tab.viewers()[0].clone();
    let (released, mut acks) = mpsc::unbounded_channel::<()>();
    let frame = |released: &mpsc::UnboundedSender<()>| {
        let released = released.clone();
        ViewerOutput::Frame {
            data: vec![1],
            ack: crate::server_browser_viewer::Ack::of(Box::pin(async move {
                let _ = released.send(());
            })),
        }
    };
    // Past the queue's bound, a frame is dropped, but dropping it still releases Chromium to send the next.
    for _ in 0..crate::server_browser_viewer::VIEWER_OUTPUT_LIMIT + 6 {
        viewer.push(frame(&released), &tab);
    }
    for _ in 0..6 {
        within(acks.recv()).await.unwrap();
    }
    assert_eq!(viewer.queued(), crate::server_browser_viewer::VIEWER_OUTPUT_LIMIT);
    // State the viewer cannot miss replaces the whole backlog, and the frames it replaces are released as well.
    viewer.push(ViewerOutput::Control { can_operate: false, controller: "agent", generation: 9, dialog: None }, &tab);
    for _ in 0..crate::server_browser_viewer::VIEWER_OUTPUT_LIMIT {
        within(acks.recv()).await.unwrap();
    }
    assert_eq!(viewer.queued(), 1);
    // Output that is neither state nor a frame is simply lost while the viewer is behind.
    for _ in 0..crate::server_browser_viewer::VIEWER_OUTPUT_LIMIT {
        viewer.push(ViewerOutput::Viewport { width: 1, height: 1 }, &tab);
    }
    viewer.push(ViewerOutput::Pointer { phase: "move", x: 0.0, y: 0.0, sequence: 1 }, &tab);
    assert_eq!(viewer.queued(), crate::server_browser_viewer::VIEWER_OUTPUT_LIMIT);
    viewer.push(ViewerOutput::Gone, &tab);
    assert_eq!(viewer.queued(), 1);
    assert!(matches!(within(handle.next()).await, Some(ViewerOutput::Gone)));
    handle.close().await;
}
