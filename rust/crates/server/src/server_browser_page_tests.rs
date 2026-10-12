//! A page over the protocol (apps/server/src/preview/ServerBrowserPage.test.ts): what attaching sets up, how the page's events become its state,
//! how navigation waits, how a target is named, and what evaluation and waiting do. The browser is played by `server_browser_double`.
use super::*;
use crate::{cdp_fake::FakeBrowser, server_browser_double::Double, test_support::HANG_BOUND};

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, future).await.expect("the awaited event happened")
}

pub(crate) struct Rig {
    pub page: Page,
    pub events: mpsc::UnboundedReceiver<PageEvent>,
    pub browser: FakeBrowser,
    pub double: Double,
    pub connection: CdpConnection,
}

pub(crate) async fn rig() -> Rig {
    let double = Double::new();
    let browser = double.browser();
    let (reader, writer) = browser.pipe();
    let connection = CdpConnection::over_pipe(reader, writer);
    let created = connection.send("Target.createTarget", json!({"url": "about:blank"}), None).await.unwrap();
    let (page, events) = within(Page::attach(&connection, created["targetId"].as_str().unwrap(), None, true)).await.unwrap();
    Rig { page, events, browser, double, connection }
}

#[test]
fn a_net_error_is_named_and_coded() {
    assert_eq!(parse_net_error("net::ERR_CONNECTION_REFUSED at http://x/"), ("ERR_CONNECTION_REFUSED".to_owned(), -102));
    assert_eq!(parse_net_error("net::ERR_NAME_NOT_RESOLVED"), ("ERR_NAME_NOT_RESOLVED".to_owned(), -105));
    // A name this table does not know is the generic failure, and text with no name stays as it is.
    assert_eq!(parse_net_error("net::ERR_SOMETHING_NEW"), ("ERR_SOMETHING_NEW".to_owned(), -2));
    assert_eq!(parse_net_error("it broke"), ("it broke".to_owned(), -2));
}

#[test]
fn a_failure_is_typed_by_what_it_says() {
    for (message, tag) in [
        ("locator.click: Unexpected token \"=\" while parsing selector \"a=\"", "PreviewAutomationInvalidSelectorError"),
        ("strict mode violation: locator resolved to 2 elements", "PreviewAutomationInvalidSelectorError"),
        ("Unknown engine \"foo\" while parsing selector foo=bar", "PreviewAutomationInvalidSelectorError"),
        ("Node is not an <input> element", "PreviewAutomationTargetNotEditableError"),
        ("Element is not editable", "PreviewAutomationTargetNotEditableError"),
        ("Something else\nwith a second line", "PreviewAutomationExecutionError"),
    ] {
        let error = OpError::classify(message);
        assert_eq!((error.tag.as_str(), error.message.as_str()), (tag, message.lines().next().unwrap()), "{message}");
    }
    let stale = OpError::stale_ref();
    assert_eq!((stale.tag.as_str(), stale.detail), ("PreviewAutomationInvalidSelectorError", Some(json!({"staleRef": true}))));
}

#[test]
fn the_helper_script_marks_its_failures_so_the_server_reads_their_kind() {
    let details = |description: &str| json!({"text": "Uncaught", "exception": {"description": description}});
    let invalid = exception_error(&details("Error: T3:invalid:strict mode violation: locator resolved to 3 elements\n    at query"));
    assert_eq!((invalid.tag.as_str(), invalid.message.as_str()), ("PreviewAutomationInvalidSelectorError", "strict mode violation: locator resolved to 3 elements"));
    assert_eq!(exception_error(&details("Error: T3:stale:This element ref is stale.")).detail, Some(json!({"staleRef": true})));
    assert_eq!(exception_error(&details("ReferenceError: x is not defined\n    at <anonymous>")).tag, "PreviewAutomationExecutionError");
}

#[tokio::test]
async fn attaching_sets_the_page_up_for_what_the_server_reads_from_it() {
    let rig = rig().await;
    let methods: Vec<String> = rig.browser.commands().into_iter().map(|command| command.method).collect();
    assert_eq!(
        methods,
        [
            "Target.createTarget",
            "Target.attachToTarget",
            "Page.enable",
            "Runtime.enable",
            "Network.enable",
            "Page.setLifecycleEventsEnabled",
            "Page.setInterceptFileChooserDialog",
            "Input.setInterceptDrags",
            "Emulation.setFocusEmulationEnabled",
            "Page.addScriptToEvaluateOnNewDocument",
            "Runtime.addBinding",
            "Page.addScriptToEvaluateOnNewDocument",
        ]
    );
    let scripts = rig.browser.commands_named("Page.addScriptToEvaluateOnNewDocument");
    // The helper script carries this process's key, and the clipboard script the binding the page reports through.
    assert!(scripts[0].params["source"].as_str().unwrap().contains(&format!("\"{}\"", runtime_key())) && !scripts[0].params["source"].as_str().unwrap().contains("__KEY__"));
    assert!(scripts[1].params["source"].as_str().unwrap().contains(CLIPBOARD_BINDING) && !scripts[1].params["source"].as_str().unwrap().contains("__BINDING__"));
    assert_eq!(rig.browser.commands_named("Runtime.addBinding")[0].params, json!({"name": CLIPBOARD_BINDING}));
    assert_eq!((rig.page.url(), rig.page.viewport(), rig.page.is_closed()), ("about:blank".to_owned(), None, false));
}

#[tokio::test]
async fn navigation_waits_for_the_milestone_it_was_asked_for() {
    let mut rig = rig().await;
    rig.page.goto("http://localhost:3000/a#top", Readiness::Load, Duration::from_secs(5)).await.unwrap();
    assert_eq!(rig.page.url(), "http://localhost:3000/a#top");
    let mut kinds = Vec::new();
    for _ in 0..4 {
        kinds.push(match within(rig.events.recv()).await.unwrap() {
            PageEvent::Request { main: true, .. } => "request",
            PageEvent::Response { status: 200, .. } => "response",
            PageEvent::Navigated => "navigated",
            PageEvent::Load => "load",
            other => panic!("{other:?}"),
        });
    }
    assert_eq!(kinds, ["request", "response", "navigated", "load"]);
    // A navigation the browser refuses outright is its error, named.
    rig.double.navigation_error("nowhere.test", "net::ERR_NAME_NOT_RESOLVED");
    let refused = rig.page.goto("http://nowhere.test/", Readiness::Load, Duration::from_secs(5)).await.unwrap_err();
    assert!(refused.message.contains("net::ERR_NAME_NOT_RESOLVED"), "{refused}");
    // The history can be walked, the page reloaded, and the position read.
    rig.double.history(json!({"currentIndex": 1, "entries": [{"id": 4}, {"id": 5}]}));
    assert_eq!(rig.page.history_position().await, Some((1, 2)));
    assert!(rig.page.go_history(-1, Duration::from_secs(5)).await.unwrap());
    assert!(!rig.page.go_history(-2, Duration::from_secs(5)).await.unwrap());
    rig.page.reload(true, Duration::from_secs(5)).await.unwrap();
    assert_eq!(rig.browser.commands_named("Page.navigateToHistoryEntry")[0].params, json!({"entryId": 4}));
}

#[tokio::test(start_paused = true)]
async fn a_navigation_that_never_commits_times_out() {
    let rig = rig().await;
    rig.double.hold_navigation(true);
    let started = tokio::time::Instant::now();
    let slow = rig.page.goto("http://localhost:3000/", Readiness::Load, Duration::from_secs(7)).await.unwrap_err();
    assert_eq!(slow.tag, "PreviewAutomationTimeoutError");
    assert_eq!(started.elapsed(), Duration::from_secs(7));
}

#[tokio::test]
async fn the_pages_events_become_its_state() {
    let mut rig = rig().await;
    let session = rig.page.session_id().to_owned();
    let push = |method: &str, params: Value| rig.browser.push(json!({"method": method, "sessionId": session, "params": params}));
    push("Page.frameAttached", json!({"frameId": "F-child", "parentFrameId": "T-1"}));
    push("Network.requestWillBeSent", json!({"requestId": "r1", "loaderId": "L1", "frameId": "T-1", "type": "Document", "request": {"url": "http://x/", "method": "POST"}}));
    push("Network.loadingFailed", json!({"requestId": "r1", "errorText": "net::ERR_CONNECTION_REFUSED", "canceled": false}));
    push("Network.requestWillBeSent", json!({"requestId": "r2", "loaderId": "L1", "frameId": "F-child", "type": "Script", "request": {"url": "http://x/a.js", "method": "GET"}}));
    push("Network.loadingFailed", json!({"requestId": "r2", "errorText": "", "canceled": true}));
    push("Runtime.consoleAPICalled", json!({"type": "log", "args": [{"type": "string", "value": "hello"}, {"type": "number", "value": 3, "description": "3"}]}));
    push("Page.javascriptDialogOpening", json!({"type": "prompt", "message": "Name?", "defaultPrompt": "me", "url": "http://x/"}));
    push("Runtime.bindingCalled", json!({"name": CLIPBOARD_BINDING, "payload": "copied text"}));
    push("Runtime.bindingCalled", json!({"name": "other", "payload": "ignored"}));
    push("Page.fileChooserOpened", json!({"frameId": "T-1", "mode": "selectMultiple", "backendNodeId": 12}));
    push("Inspector.targetCrashed", json!({}));
    let mut got = Vec::new();
    for _ in 0..9 {
        got.push(within(rig.events.recv()).await.unwrap());
    }
    assert_eq!(
        got,
        [
            PageEvent::Request { url: "http://x/".into(), main: true },
            PageEvent::RequestFailed { url: "http://x/".into(), method: "POST".into(), error_text: "net::ERR_CONNECTION_REFUSED".into(), main: true },
            PageEvent::Request { url: "http://x/a.js".into(), main: false },
            // A canceled request is an abort, not a failure with a reason.
            PageEvent::RequestFailed { url: "http://x/a.js".into(), method: "GET".into(), error_text: "net::ERR_ABORTED".into(), main: false },
            PageEvent::Console { level: "log".into(), text: "hello 3".into() },
            PageEvent::Dialog { kind: "prompt".into(), message: "Name?".into(), default_value: "me".into() },
            PageEvent::Clipboard("copied text".into()),
            PageEvent::FileChooser { multiple: true, backend_node_id: 12 },
            PageEvent::Crashed,
        ]
    );
    assert!(rig.page.owns_frame("T-1") && rig.page.owns_frame("F-child") && !rig.page.owns_frame("elsewhere"));
}

#[tokio::test]
async fn a_page_closes_when_its_target_goes_or_the_browser_does() {
    let mut rig = rig().await;
    rig.page.close().await;
    assert_eq!(within(rig.events.recv()).await, Some(PageEvent::Closed));
    assert!(rig.page.is_closed());
    within(rig.page.closed()).await;
    // The browser going away closes every page it had.
    let other = rig.connection.send("Target.createTarget", json!({"url": "about:blank"}), None).await.unwrap();
    let (page, mut events) = within(Page::attach(&rig.connection, other["targetId"].as_str().unwrap(), None, false)).await.unwrap();
    rig.browser.hang_up();
    assert_eq!(within(events.recv()).await, Some(PageEvent::Closed));
    assert!(page.is_closed());
}

#[tokio::test]
async fn a_target_is_named_as_the_page_issued_it() {
    let rig = rig().await;
    // A selector with no ref is passed on as written, including one whose quoted text mentions refs.
    for selector in ["button.save", "text=\"aria-ref=e1\"", "[data-x=\"aria-ref=e1\"] >> nth=0", r#"div[title='>> aria-ref = e3']"#, "role=button[name='x']"] {
        assert_eq!(rig.page.target_selector(selector), Ok(selector.to_owned()), "{selector}");
    }
    // A ref the page did not issue in its latest snapshot is refused, in a chain as well as alone.
    for selector in ["aria-ref=t3-nope-e1", "  aria-ref = e1 ", "div >> aria-ref=e1", "*aria-ref=e2"] {
        assert_eq!(rig.page.target_selector(selector), Err(OpError::stale_ref()), "{selector}");
    }
    // One it issued is the page's own.
    let generation = rig.page.ref_generation();
    rig.page.0.shared.lock().unwrap().refs.refs.insert(format!("t3-{generation}-e5"), "e5".into());
    assert_eq!(rig.page.target_selector(&format!("aria-ref=t3-{generation}-e5")), Ok("aria-ref=e5".to_owned()));
    // A navigation revokes it.
    rig.page.goto("http://localhost/", Readiness::Load, Duration::from_secs(5)).await.unwrap();
    assert_eq!(rig.page.target_selector(&format!("aria-ref=t3-{generation}-e5")), Err(OpError::stale_ref()));
    assert_ne!(rig.page.ref_generation(), generation);
}

#[tokio::test]
async fn evaluation_returns_what_the_page_gives_up_to_the_limit() {
    let rig = rig().await;
    rig.double.on_evaluate("small", json!({"a": 1}));
    rig.double.on_evaluate("exactly", json!("x".repeat(MAX_EVALUATION_BYTES - 2)));
    rig.double.on_evaluate("large", json!("x".repeat(MAX_EVALUATION_BYTES - 1)));
    rig.double.on_evaluate("throws", json!({"__exception": "TypeError: nope\n    at <anonymous>:1:1"}));
    assert_eq!(rig.page.evaluate("small", true, true).await.unwrap(), json!({"a": 1}));
    assert_eq!(rig.page.evaluate("exactly", true, true).await.unwrap().as_str().map(str::len), Some(MAX_EVALUATION_BYTES - 2));
    let too_large = rig.page.evaluate("large", true, true).await.unwrap_err();
    assert_eq!((too_large.tag.as_str(), too_large.detail), ("PreviewAutomationResultTooLargeError", Some(json!({"maximumBytes": MAX_EVALUATION_BYTES}))));
    assert_eq!(rig.page.evaluate("throws", true, true).await.unwrap_err().message, "TypeError: nope\n    at <anonymous>:1:1");
    // A value that is not there (undefined) is null.
    assert_eq!(rig.page.evaluate("nothing", false, false).await.unwrap(), Value::Null);
    let sent = rig.browser.commands_named("Runtime.evaluate");
    assert_eq!((sent[4].params["awaitPromise"].clone(), sent[4].params["returnByValue"].clone()), (json!(false), json!(false)));
}

#[tokio::test(start_paused = true)]
async fn waiting_polls_until_every_condition_holds_or_the_time_is_up() {
    let rig = rig().await;
    rig.double.on_evaluate("includes(\"Ready\")", json!(true));
    // The text is on the page and the URL is not yet what is asked for.
    let started = tokio::time::Instant::now();
    let timed_out = rig.page.wait_for(None, Some("Ready"), Some("/done"), 1_000).await.unwrap_err();
    assert_eq!((timed_out.tag.as_str(), timed_out.message.as_str()), ("PreviewAutomationTimeoutError", "Waited 1000ms without every condition holding."));
    assert_eq!(started.elapsed(), Duration::from_millis(1_000));
    rig.page.goto("http://localhost/done", Readiness::Load, Duration::from_secs(5)).await.unwrap();
    rig.page.wait_for(None, Some("Ready"), Some("/done"), 1_000).await.unwrap();
    // A selector must match an element; the helper script counts them.
    rig.double.on_evaluate(".count(", json!(2));
    rig.page.wait_for(Some("li"), None, None, 1_000).await.unwrap();
    let counted = rig.browser.commands_named("Runtime.evaluate").into_iter().filter(|command| command.params["expression"].as_str().unwrap().contains(".count(")).count();
    assert_eq!(counted, 1);
    // A ref that was not issued is refused before the page is asked.
    assert_eq!(rig.page.wait_for(Some("aria-ref=t3-x-e1"), None, None, 1_000).await.unwrap_err(), OpError::stale_ref());
}

#[tokio::test]
async fn the_viewport_follows_what_was_set_and_a_scaled_capture_clips_to_it() {
    let rig = rig().await;
    rig.page.set_viewport(900, 700).await.unwrap();
    assert_eq!(rig.page.viewport(), Some((900, 700)));
    let set = rig.browser.commands_named("Emulation.setDeviceMetricsOverride");
    assert_eq!(set[0].params, json!({"width": 900, "height": 700, "deviceScaleFactor": 2.0, "mobile": false}));
    rig.page.set_layout(450, 350, 4.0).await.unwrap();
    assert_eq!(rig.page.viewport(), Some((900, 700)));
    rig.page.emulate_color_scheme(Some("dark")).await.unwrap();
    rig.page.emulate_color_scheme(None).await.unwrap();
    let media = rig.browser.commands_named("Emulation.setEmulatedMedia");
    assert_eq!((media[0].params["features"].clone(), media[1].params["features"].clone()), (json!([{"name": "prefers-color-scheme", "value": "dark"}]), json!([{"name": "prefers-color-scheme", "value": ""}])));
    assert_eq!(rig.page.capture_viewport(None, "png", None, 1.0).await.unwrap(), "ZnJhbWU=");
    assert_eq!(rig.page.capture_viewport(None, "jpeg", Some(70), 0.5).await.unwrap(), "ZnJhbWU=");
    let shots = rig.browser.commands_named("Page.captureScreenshot");
    assert_eq!(shots[0].params, json!({"format": "png"}));
    assert_eq!(shots[1].params, json!({"format": "jpeg", "quality": 70, "clip": {"x": 0, "y": 0, "width": 900, "height": 700, "scale": 0.5}}));
}
