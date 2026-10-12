//! The broker against hosts the tests play: which host a call goes to, how an answer finds its call, what happens when a host is late, replaced
//! or gone, and how a host's failure reaches the agent. Time is paused, so a timeout costs nothing and nothing waits on a real clock.
use crate::mcp_invocation::InvocationScope;
use crate::preview_automation_broker::{HostConnection, Invocation, PreviewAutomationBroker};
use crate::test_support::HANG_BOUND;
use serde_json::{Value, json};
use t3_contracts::{PreviewAutomationOperation, PreviewAutomationRequest, PreviewAutomationStreamEvent, PreviewTabId, Present};

fn scope(session: &str, environment: &str, thread: &str) -> InvocationScope {
    serde_json::from_value(json!({
        "environmentId": environment, "requestNamespace": session, "issuedAt": 1, "capabilities": ["preview"],
        "thread": {"threadId": thread, "providerSessionId": session, "providerInstanceId": "codex"}
    }))
    .unwrap()
}
fn default_scope() -> InvocationScope {
    scope("session-1", "env-1", "thread-1")
}
fn host_in(client: &str, environment: &str, operations: Option<Value>) -> t3_contracts::PreviewAutomationHost {
    let mut host = json!({"clientId": client, "environmentId": environment});
    if let Some(operations) = operations {
        host["supportedOperations"] = operations;
    }
    serde_json::from_value(host).unwrap()
}
fn host(client: &str) -> t3_contracts::PreviewAutomationHost {
    host_in(client, "env-1", Some(all_operations()))
}
fn all_operations() -> Value {
    serde_json::to_value(t3_contracts::PREVIEW_AUTOMATION_SERVER_OPERATIONS).unwrap()
}
fn scope_for(session: &str) -> InvocationScope {
    scope(session, "env-1", "thread-1")
}
fn tab(id: &str) -> PreviewTabId {
    PreviewTabId::new(id).unwrap()
}

/// The next request a host is asked to run, after the `connected` announcement.
async fn request(connection: &mut HostConnection) -> PreviewAutomationRequest {
    loop {
        match tokio::time::timeout(HANG_BOUND, connection.next()).await.expect("an event").expect("the stream is open") {
            PreviewAutomationStreamEvent::Request(event) => return event.request,
            PreviewAutomationStreamEvent::Connected(_) => {}
        }
    }
}
fn answer(connection: &HostConnection, client: &str, request: &PreviewAutomationRequest, outcome: Value) -> t3_contracts::PreviewAutomationResponse {
    let mut response = json!({"clientId": client, "connectionId": connection.connection_id(), "requestId": request.request_id});
    response.as_object_mut().unwrap().extend(outcome.as_object().unwrap().clone());
    serde_json::from_value(response).unwrap()
}
fn ok(result: Value) -> Value {
    json!({"ok": true, "result": result})
}
fn failed(tag: &str, message: &str, detail: Option<Value>) -> Value {
    let mut error = json!({"_tag": tag, "message": message});
    if let Some(detail) = detail {
        error["detail"] = detail;
    }
    json!({"ok": false, "error": error})
}
async fn invoke(broker: &PreviewAutomationBroker, scope: &InvocationScope, operation: PreviewAutomationOperation, input: Value) -> Result<Value, Value> {
    broker.invoke(Invocation::new(scope, operation, input)).await.map(|invoked| invoked.result).map_err(|error| serde_json::to_value(error).unwrap())
}

#[tokio::test(start_paused = true)]
async fn a_host_is_announced_and_its_answer_finds_its_call() {
    let broker = PreviewAutomationBroker::default();
    let mut connection = broker.connect(host("server-browser"), true);
    let first = tokio::time::timeout(HANG_BOUND, connection.next()).await.unwrap().unwrap();
    let PreviewAutomationStreamEvent::Connected(connected) = first else { panic!("the first event announces the connection") };
    assert_eq!(connected.connection_id.as_str(), connection.connection_id());

    let scope = default_scope();
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Click, json!({"selector": "button"}));
    let host = async {
        let asked = request(&mut connection).await;
        assert_eq!(asked.operation, PreviewAutomationOperation::Click);
        assert_eq!(asked.input, json!({"selector": "button"}));
        assert_eq!((asked.request_id.as_str(), asked.thread_id.as_str()), ("preview-0", "thread-1"));
        assert_eq!(asked.timeout_ms.0, 15_000);
        assert_eq!(asked.tab_id_explicit, Some(Some(false)));
        assert_eq!(asked.agent_session_id, Some(Some("env-1\u{0}session-1".to_owned())));
        broker.respond(answer(&connection, "server-browser", &asked, ok(json!({"clicked": true}))));
    };
    let (result, ()) = tokio::join!(call, host);
    assert_eq!(result.unwrap(), json!({"clicked": true}));
}

#[tokio::test(start_paused = true)]
async fn with_no_host_the_call_fails_as_the_original_does() {
    let broker = PreviewAutomationBroker::default();
    let error = invoke(&broker, &default_scope(), PreviewAutomationOperation::Status, json!({})).await.unwrap_err();
    assert_eq!(
        error,
        json!({"_tag":"PreviewAutomationNoAvailableHostError","operation":"status","environmentId":"env-1","threadId":"thread-1","providerSessionId":"session-1","providerInstanceId":"codex"})
    );
    let typed: t3_contracts::PreviewAutomationError = serde_json::from_value(error).unwrap();
    assert!(typed.message().starts_with("No preview automation host is available for status in environment env-1."));
}

#[tokio::test(start_paused = true)]
async fn a_focus_report_alone_makes_no_host() {
    let broker = PreviewAutomationBroker::default();
    broker.focus_host(serde_json::from_value(json!({"clientId": "ghost", "environmentId": "env-1", "connectionId": "c", "focused": true})).unwrap());
    let error = invoke(&broker, &default_scope(), PreviewAutomationOperation::Status, json!({})).await.unwrap_err();
    assert_eq!(error["_tag"], "PreviewAutomationNoAvailableHostError");
}

#[tokio::test(start_paused = true)]
async fn a_session_stays_on_its_host_until_that_host_disconnects() {
    let broker = PreviewAutomationBroker::default();
    let mut first = broker.connect(host("desktop-a"), false);
    let scope = default_scope();
    // The first call pins the session to the only host there is.
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let serve = async {
        let asked = request(&mut first).await;
        broker.respond(answer(&first, "desktop-a", &asked, ok(json!({}))));
    };
    tokio::join!(call, serve).0.unwrap();

    // A focused newcomer does not take the session over.
    let mut second = broker.connect(host("desktop-b"), false);
    broker.focus_host(serde_json::from_value(json!({"clientId": "desktop-b", "environmentId": "env-1", "connectionId": second.connection_id(), "focused": true})).unwrap());
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let serve = async {
        let asked = request(&mut first).await;
        broker.respond(answer(&first, "desktop-a", &asked, ok(json!({"on": "a"}))));
    };
    assert_eq!(tokio::join!(call, serve).0.unwrap(), json!({"on": "a"}));

    // Once the pinned host is gone the session fails over to the host that is left.
    drop(first);
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let serve = async {
        let asked = request(&mut second).await;
        broker.respond(answer(&second, "desktop-b", &asked, ok(json!({"on": "b"}))));
    };
    assert_eq!(tokio::join!(call, serve).0.unwrap(), json!({"on": "b"}));
}

#[tokio::test(start_paused = true)]
async fn a_session_never_reaches_a_host_of_another_environment() {
    let broker = PreviewAutomationBroker::default();
    let _elsewhere = broker.connect(host_in("desktop", "env-2", Some(all_operations())), false);
    let error = invoke(&broker, &default_scope(), PreviewAutomationOperation::Status, json!({})).await.unwrap_err();
    assert_eq!(error["_tag"], "PreviewAutomationNoAvailableHostError");
}

#[tokio::test(start_paused = true)]
async fn a_host_that_did_not_advertise_an_operation_is_not_given_it() {
    let broker = PreviewAutomationBroker::default();
    // No list of operations means the set that predates negotiation: no resize. The capable host registered first, so recency does not decide.
    let mut capable = broker.connect(host("capable"), false);
    let _legacy = broker.connect(host_in("legacy", "env-1", None), false);
    let scope = default_scope();
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Resize, json!({"mode": "fill"}));
    let serve = async {
        let asked = request(&mut capable).await;
        assert_eq!(asked.operation, PreviewAutomationOperation::Resize);
        broker.respond(answer(&capable, "capable", &asked, ok(json!({}))));
    };
    tokio::join!(call, serve).0.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_live_legacy_assignment_is_not_moved_to_another_host_for_resize() {
    let broker = PreviewAutomationBroker::default();
    let mut legacy = broker.connect(host_in("legacy", "env-1", None), false);
    let scope = default_scope();
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let serve = async {
        let asked = request(&mut legacy).await;
        broker.respond(answer(&legacy, "legacy", &asked, ok(json!({}))));
    };
    tokio::join!(call, serve).0.unwrap();
    let _capable = broker.connect(host("capable"), false);
    let error = invoke(&broker, &scope, PreviewAutomationOperation::Resize, json!({"mode": "fill"})).await.unwrap_err();
    assert_eq!(error["_tag"], "PreviewAutomationNoAvailableHostError");
}

#[tokio::test(start_paused = true)]
async fn new_work_goes_to_the_preferred_host_then_the_tab_owner_then_the_focused_host() {
    let broker = PreviewAutomationBroker::default();
    let mut plain = broker.connect(host("desktop"), false);
    let mut preferred = broker.connect(host("server-browser"), true);
    // A preferred host beats a newer, focused one.
    broker.focus_host(serde_json::from_value(json!({"clientId": "desktop", "environmentId": "env-1", "connectionId": plain.connection_id(), "focused": true})).unwrap());
    let scope = default_scope();
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let serve = async {
        let asked = request(&mut preferred).await;
        broker.respond(answer(&preferred, "server-browser", &asked, ok(json!({}))));
    };
    tokio::join!(call, serve).0.unwrap();

    // A session that names a tab goes to the host that holds it, preferred or not.
    broker.focus_host(
        serde_json::from_value(json!({
            "clientId": "desktop", "environmentId": "env-1", "connectionId": plain.connection_id(), "focused": true,
            "liveTabs": [{"threadId": "thread-1", "tabId": "tab_9"}]
        }))
        .unwrap(),
    );
    let owner_scope = scope_for("session-owner");
    let mut owned = Invocation::new(&owner_scope, PreviewAutomationOperation::Status, json!({}));
    owned.tab_id = Some(tab("tab_9"));
    let call = broker.invoke(owned);
    let serve = async {
        let asked = request(&mut plain).await;
        assert_eq!((asked.tab_id.present().map(|tab| tab.as_str()), asked.tab_id_explicit), (Some("tab_9"), Some(Some(true))));
        broker.respond(answer(&plain, "desktop", &asked, ok(json!({}))));
    };
    tokio::join!(call, serve).0.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_first_open_on_the_environments_own_browser_gets_ten_minutes_and_a_late_host_is_cut_off() {
    let broker = PreviewAutomationBroker::default();
    let mut preferred = broker.connect(host("server-browser"), true);
    let scope = default_scope();
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Open, json!({"url": "example.com"}));
    let serve = async {
        let asked = request(&mut preferred).await;
        assert_eq!(asked.timeout_ms.0, 660_000);
        // Not answered: the call times out after the budget.
    };
    let (result, ()) = tokio::join!(call, serve);
    let error = result.unwrap_err();
    assert_eq!(error["_tag"], "PreviewAutomationTimeoutError");
    assert_eq!(error["timeoutMs"], 660_000);
    // The host that sat on a request is evicted: its stream completes, so a responsive host can register again.
    assert!(tokio::time::timeout(HANG_BOUND, preferred.next()).await.unwrap().is_none());
    let again = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({})).await.unwrap_err();
    assert_eq!(again["_tag"], "PreviewAutomationNoAvailableHostError");
}

#[tokio::test(start_paused = true)]
async fn a_slow_background_read_does_not_cut_the_host_off() {
    let broker = PreviewAutomationBroker::default();
    let mut connection = broker.connect(host("desktop"), false);
    let scope = default_scope();
    let mut read = Invocation::new(&scope, PreviewAutomationOperation::Status, json!({}));
    read.timeout_ms = Some(500);
    read.update_current_tab = false;
    let call = broker.invoke(read);
    let serve = async {
        let _unanswered = request(&mut connection).await;
    };
    let (result, ()) = tokio::join!(call, serve);
    assert_eq!(serde_json::to_value(result.unwrap_err()).unwrap()["_tag"], "PreviewAutomationTimeoutError");
    // The host is still there for the next call.
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let serve = async {
        let asked = request(&mut connection).await;
        broker.respond(answer(&connection, "desktop", &asked, ok(json!({"alive": true}))));
    };
    assert_eq!(tokio::join!(call, serve).0.unwrap(), json!({"alive": true}));
}

#[tokio::test(start_paused = true)]
async fn a_replacement_registration_fails_the_calls_of_the_one_it_replaces_and_ends_its_stream() {
    let broker = PreviewAutomationBroker::default();
    let mut old = broker.connect(host("desktop"), false);
    let scope = default_scope();
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let replace = async {
        let _seen = request(&mut old).await;
        let replacement = broker.connect(host("desktop"), false);
        assert_ne!(replacement.connection_id(), old.connection_id());
        replacement
    };
    let (result, mut replacement) = tokio::join!(call, replace);
    assert_eq!(result.unwrap_err()["_tag"], "PreviewAutomationClientDisconnectedError");
    assert!(old.next().await.is_none(), "the replaced stream ends");
    // The replacement announces itself before any request, and the old stream finalizing does not take its place.
    drop(old);
    let PreviewAutomationStreamEvent::Connected(_) = replacement.next().await.unwrap() else { panic!("announced first") };
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let serve = async {
        let asked = request(&mut replacement).await;
        broker.respond(answer(&replacement, "desktop", &asked, ok(json!({"on": "replacement"}))));
    };
    assert_eq!(tokio::join!(call, serve).0.unwrap(), json!({"on": "replacement"}));
}

#[tokio::test(start_paused = true)]
async fn only_the_connection_that_received_a_request_can_answer_it() {
    let broker = PreviewAutomationBroker::default();
    let mut connection = broker.connect(host("desktop"), false);
    let scope = default_scope();
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let serve = async {
        let asked = request(&mut connection).await;
        // Another client, another connection of the same client, and an unknown request id: all ignored.
        let mut stranger = answer(&connection, "someone-else", &asked, ok(json!({"forged": true})));
        broker.respond(stranger.clone());
        stranger = answer(&connection, "desktop", &asked, ok(json!({"forged": true})));
        let mut wrong_connection = serde_json::to_value(&stranger).unwrap();
        wrong_connection["connectionId"] = json!("another-connection");
        broker.respond(serde_json::from_value(wrong_connection).unwrap());
        let mut unknown = serde_json::to_value(&stranger).unwrap();
        unknown["requestId"] = json!("preview-99");
        broker.respond(serde_json::from_value(unknown).unwrap());
        broker.respond(answer(&connection, "desktop", &asked, ok(json!({"genuine": true}))));
        // A second answer to the same call changes nothing.
        broker.respond(answer(&connection, "desktop", &asked, ok(json!({"twice": true}))));
    };
    assert_eq!(tokio::join!(call, serve).0.unwrap(), json!({"genuine": true}));
}

#[tokio::test(start_paused = true)]
async fn requests_buffered_for_a_host_that_is_evicted_are_never_delivered() {
    let broker = PreviewAutomationBroker::default();
    let mut connection = broker.connect(host("desktop"), false);
    let scope = default_scope();
    // The host never reads: the first call times out and evicts it while a second call's request is still waiting in its queue.
    let second_scope = scope_for("session-2");
    let first = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let second = invoke(&broker, &second_scope, PreviewAutomationOperation::Status, json!({}));
    let (first, second) = tokio::join!(first, second);
    // Both budgets end together: whichever call is woken first times out and evicts the host, and the other, queued behind the same
    // connection, fails rather than waiting for a host that will never read it.
    let mut tags = [first.unwrap_err(), second.unwrap_err()].map(|error| error["_tag"].as_str().unwrap().to_owned());
    tags.sort();
    assert_eq!(tags, ["PreviewAutomationClientDisconnectedError", "PreviewAutomationTimeoutError"]);
    assert!(connection.next().await.is_none(), "an evicted host's stream ends without handing over what was queued");
}

#[tokio::test(start_paused = true)]
async fn the_session_follows_the_tab_a_result_names() {
    let broker = PreviewAutomationBroker::default();
    let mut connection = broker.connect(host("desktop"), false);
    let scope = default_scope();
    async fn call_and_answer(broker: &PreviewAutomationBroker, connection: &mut HostConnection, scope: &InvocationScope, operation: PreviewAutomationOperation, tab: Option<&str>, result: Value) -> PreviewAutomationRequest {
        let mut invocation = Invocation::new(scope, operation, json!({}));
        invocation.tab_id = tab.map(PreviewTabId::new).map(Result::unwrap);
        let call = broker.invoke(invocation);
        let serve = async {
            let asked = request(connection).await;
            broker.respond(answer(connection, "desktop", &asked, ok(result)));
            asked
        };
        let (invoked, asked) = tokio::join!(call, serve);
        invoked.unwrap();
        asked
    }
    // An open reports the tab it made; the next call, naming none, is sent to it.
    let opened = call_and_answer(&broker, &mut connection, &scope, PreviewAutomationOperation::Open, None, json!({"tabId": "tab_1"})).await;
    assert!(opened.tab_id.present().is_none());
    let next = call_and_answer(&broker, &mut connection, &scope, PreviewAutomationOperation::Click, None, json!({})).await;
    assert_eq!((next.tab_id.present().map(|t| t.as_str()), next.tab_id_explicit), (Some("tab_1"), Some(Some(false))));
    // A call that names a tab makes it current when the answer names none.
    call_and_answer(&broker, &mut connection, &scope, PreviewAutomationOperation::Click, Some("tab_2"), json!({"ok": true})).await;
    let next = call_and_answer(&broker, &mut connection, &scope, PreviewAutomationOperation::Click, None, json!({})).await;
    assert_eq!(next.tab_id.present().map(|t| t.as_str()), Some("tab_2"));
    // A stop that reports no tab clears it: the session has none now.
    call_and_answer(&broker, &mut connection, &scope, PreviewAutomationOperation::RecordingStop, None, json!({"tabId": null})).await;
    let next = call_and_answer(&broker, &mut connection, &scope, PreviewAutomationOperation::Click, None, json!({})).await;
    assert!(next.tab_id.present().is_none());
    // A tab id that is not one (too long) teaches nothing.
    call_and_answer(&broker, &mut connection, &scope, PreviewAutomationOperation::Click, None, json!({"tabId": "x".repeat(200)})).await;
    let next = call_and_answer(&broker, &mut connection, &scope, PreviewAutomationOperation::Click, None, json!({})).await;
    assert!(next.tab_id.present().is_none());
}

#[tokio::test(start_paused = true)]
async fn a_background_read_does_not_change_the_current_tab() {
    let broker = PreviewAutomationBroker::default();
    let mut connection = broker.connect(host("desktop"), false);
    let scope = default_scope();
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Open, json!({}));
    let serve = async {
        let asked = request(&mut connection).await;
        broker.respond(answer(&connection, "desktop", &asked, ok(json!({"tabId": "tab_1"}))));
    };
    tokio::join!(call, serve).0.unwrap();
    let mut read = Invocation::new(&scope, PreviewAutomationOperation::Status, json!({}));
    read.tab_id = Some(tab("tab_other"));
    read.update_current_tab = false;
    let call = broker.invoke(read);
    let serve = async {
        let asked = request(&mut connection).await;
        broker.respond(answer(&connection, "desktop", &asked, ok(json!({"tabId": "tab_other"}))));
    };
    tokio::join!(call, serve).0.unwrap();
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Click, json!({}));
    let serve = async {
        let asked = request(&mut connection).await;
        assert_eq!(asked.tab_id.present().map(|t| t.as_str()), Some("tab_1"));
        broker.respond(answer(&connection, "desktop", &asked, ok(json!({}))));
    };
    tokio::join!(call, serve).0.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_hosts_failure_reaches_the_agent_as_the_typed_error_it_names() {
    let broker = PreviewAutomationBroker::default();
    let mut connection = broker.connect(host("desktop"), false);
    let scope = default_scope();
    let long = "e".repeat(600);
    let cases: Vec<(Value, PreviewAutomationOperation, Value, Box<dyn Fn(&Value)>)> = vec![
        (failed("PreviewAutomationTabNotFoundError", "no tab", None), PreviewAutomationOperation::Click, json!({}), Box::new(|e| assert_eq!(e["_tag"], "PreviewAutomationTabNotFoundError"))),
        (
            failed("PreviewAutomationInvalidSelectorError", "bad", Some(json!({"staleRef": true}))),
            PreviewAutomationOperation::Click,
            json!({"locator": "aria-ref=e5"}),
            Box::new(|e| {
                // The call's own selector is what the error says it was about.
                assert_eq!((e["staleRef"].as_bool(), e["selectorKind"].as_str(), e["selectorLength"].as_u64()), (Some(true), Some("locator"), Some(11)));
                assert_eq!((e["remoteTag"].as_str(), e["remoteMessageLength"].as_u64(), e["remoteDetailKind"].as_str()), (Some("PreviewAutomationInvalidSelectorError"), Some(3), Some("object")));
            }),
        ),
        (failed("PreviewAutomationInvalidSelectorError", "bad", Some(json!({"staleRef": "yes"}))), PreviewAutomationOperation::Click, json!({}), Box::new(|e| assert!(e.get("staleRef").is_none()))),
        (
            failed("PreviewAutomationTargetNotEditableError", "no", Some(json!({"selectorKind": "focused-element", "selectorLength": 0}))),
            PreviewAutomationOperation::Type,
            json!({"locator": "abcd"}),
            Box::new(|e| assert_eq!((e["selectorKind"].as_str(), e["selectorLength"].as_u64()), (Some("focused-element"), Some(0)))),
        ),
        (
            failed("PreviewAutomationTargetNotEditableError", "no", Some(json!({"selectorKind": "bogus", "selectorLength": -1}))),
            PreviewAutomationOperation::Type,
            json!({"selector": "div > p"}),
            Box::new(|e| assert_eq!((e["selectorKind"].as_str(), e["selectorLength"].as_u64()), (Some("selector"), Some(7)))),
        ),
        (failed("PreviewAutomationResultTooLargeError", "big", Some(json!({"maximumBytes": 65536}))), PreviewAutomationOperation::Evaluate, json!({}), Box::new(|e| assert_eq!(e["maximumBytes"], 65536))),
        (failed("PreviewAutomationResultTooLargeError", "big", Some(json!({"maximumBytes": 0}))), PreviewAutomationOperation::Evaluate, json!({}), Box::new(|e| assert!(e.get("maximumBytes").is_none()))),
        (failed("PreviewAutomationControlInterruptedError", "x", Some(json!("humanControl"))), PreviewAutomationOperation::Click, json!({}), Box::new(|e| assert_eq!(e["reason"], "humanControl"))),
        (failed("PreviewAutomationControlInterruptedError", "x", Some(json!("nope"))), PreviewAutomationOperation::Click, json!({}), Box::new(|e| assert!(e.get("reason").is_none()))),
        (failed("PreviewAutomationUnavailableError", "x", None), PreviewAutomationOperation::Click, json!({}), Box::new(|e| assert_eq!(e["_tag"], "PreviewAutomationRemoteUnavailableError"))),
        (failed("PreviewAutomationUnsupportedClientError", "x", None), PreviewAutomationOperation::Hover, json!({}), Box::new(|e| assert_eq!(e["_tag"], "PreviewAutomationUnsupportedClientError"))),
        (failed("PreviewAutomationRecordingTooLargeError", "x", None), PreviewAutomationOperation::RecordingStop, json!({}), Box::new(|e| assert_eq!(e, &json!({"_tag":"PreviewAutomationRecordingTooLargeError","threadId":"thread-1","cause":{"name":"Error","message":"x"}})))),
        // Any other tag is an execution failure, and a host other than the environment's own browser does not get its text shown.
        (failed("SomethingElse", &long, None), PreviewAutomationOperation::Click, json!({}), Box::new(|e| assert_eq!((e["_tag"].as_str(), e.get("reason").is_none(), e["remoteMessageLength"].as_u64()), (Some("PreviewAutomationExecutionError"), true, Some(600))))),
        (json!({"ok": false}), PreviewAutomationOperation::Click, json!({}), Box::new(|e| assert_eq!(e["_tag"], "PreviewAutomationMalformedResponseError"))),
    ];
    for (outcome, operation, input, check) in cases {
        let call = invoke(&broker, &scope, operation, input);
        let serve = async {
            let asked = request(&mut connection).await;
            broker.respond(answer(&connection, "desktop", &asked, outcome));
        };
        let (result, ()) = tokio::join!(call, serve);
        let error = result.unwrap_err();
        let _: t3_contracts::PreviewAutomationError = serde_json::from_value(error.clone()).expect("a contract error");
        check(&error);
    }
}

#[tokio::test(start_paused = true)]
async fn the_environments_own_browser_may_show_its_own_failure_text_cut_to_five_hundred_characters() {
    let broker = PreviewAutomationBroker::default();
    let mut connection = broker.connect(host("server-browser"), true);
    let scope = default_scope();
    let message = format!("net::ERR_CONNECTION_REFUSED at http://localhost:1/ {}", "x".repeat(600));
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Navigate, json!({"url": "localhost:1"}));
    let serve = async {
        let asked = request(&mut connection).await;
        broker.respond(answer(&connection, "server-browser", &asked, failed("Error", &message, None)));
    };
    let error = tokio::join!(call, serve).0.unwrap_err();
    assert_eq!(error["_tag"], "PreviewAutomationExecutionError");
    assert_eq!(error["reason"].as_str().unwrap().chars().count(), 500);
    assert!(error["reason"].as_str().unwrap().starts_with("net::ERR_CONNECTION_REFUSED"));
    let typed: t3_contracts::PreviewAutomationError = serde_json::from_value(error).unwrap();
    assert!(typed.message().starts_with("Preview automation navigate failed: net::ERR_CONNECTION_REFUSED"));
}

#[tokio::test(start_paused = true)]
async fn a_stale_focus_report_for_another_environment_or_connection_is_ignored() {
    let broker = PreviewAutomationBroker::default();
    let plain = broker.connect(host("desktop"), false);
    let mut other = broker.connect(host("other"), false);
    for bad in [json!({"clientId":"desktop","environmentId":"env-2","connectionId":plain.connection_id(),"focused":true}), json!({"clientId":"desktop","environmentId":"env-1","connectionId":"stale","focused":true})] {
        broker.focus_host(serde_json::from_value(bad).unwrap());
    }
    broker.focus_host(serde_json::from_value(json!({"clientId":"other","environmentId":"env-1","connectionId":other.connection_id(),"focused":true})).unwrap());
    let scope = default_scope();
    let call = invoke(&broker, &scope, PreviewAutomationOperation::Status, json!({}));
    let serve = async {
        let asked = request(&mut other).await;
        broker.respond(answer(&other, "other", &asked, ok(json!({}))));
    };
    tokio::join!(call, serve).0.unwrap();
    assert!(!plain.connection_id().is_empty(), "the host the stale reports named is still registered");
}
