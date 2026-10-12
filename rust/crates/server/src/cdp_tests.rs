//! The DevTools client against a scripted endpoint, over both transports: commands and their replies, events and the order they arrive in,
//! what a close does to what is waiting, and what a message that is not for anyone does. No browser runs.
use crate::cdp::{CdpConnectError, CdpConnection, CdpError};
use crate::cdp_fake::FakeBrowser;
use crate::test_support::HANG_BOUND;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, future).await.expect("the awaited event happened")
}

/// Answers `Echo` with its params, `Fail` with an error, `Big` with a result that spans many reads, `Noisy` with events around its reply and
/// `Silent` by hanging up.
fn endpoint() -> FakeBrowser {
    FakeBrowser::with_handler(|command| match command.method.as_str() {
        "Echo" => vec![json!({"id": command.id, "result": command.params, "sessionId": command.session_id})],
        "Fail" => vec![json!({"id": command.id, "error": {"code": -32000, "message": "Cannot find context with specified id"}})],
        "Big" => vec![json!({"id": command.id, "result": {"data": "x".repeat(300_000)}})],
        "Silent" => vec![json!({"$hangUp": true})],
        "Noisy" => vec![
            json!({"method": "Page.frameNavigated", "sessionId": "S1", "params": {"n": 1}}),
            json!({"method": "Page.loadEventFired", "sessionId": "S2", "params": {"n": 2}}),
            json!({"method": "Target.targetCreated", "params": {"n": 3}}),
            json!({"id": command.id, "result": {}}),
        ],
        _ => vec![json!({"id": command.id, "result": {}})],
    })
}

async fn pipe(browser: &FakeBrowser) -> CdpConnection {
    let (reader, writer) = browser.pipe();
    CdpConnection::over_pipe(reader, writer)
}

async fn websocket(browser: &FakeBrowser) -> (CdpConnection, tokio::task::JoinHandle<()>) {
    let (url, task) = browser.websocket().await;
    (CdpConnection::connect_websocket(&url).await.unwrap(), task)
}

/// Runs the same checks over each transport.
macro_rules! over_both_transports {
    ($name:ident, |$browser:ident, $connection:ident| $body:block) => {
        mod $name {
            use super::*;
            #[tokio::test]
            async fn over_a_pipe() {
                let $browser = endpoint();
                let $connection = pipe(&$browser).await;
                $body
            }
            #[tokio::test]
            async fn over_a_websocket() {
                let $browser = endpoint();
                let ($connection, _server) = websocket(&$browser).await;
                $body
            }
        }
    };
}

over_both_transports!(a_command_gets_its_own_reply, |browser, connection| {
    let first = within(connection.send("Echo", json!({"a": 1}), None)).await.unwrap();
    let second = within(connection.send("Echo", json!({"b": 2}), Some("S1"))).await.unwrap();
    assert_eq!((first, second), (json!({"a": 1}), json!({"b": 2})));
    // Ids count up from one, and a page's commands carry its session.
    let seen = browser.commands();
    assert_eq!(seen.iter().map(|command| command.id).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(seen.iter().map(|command| command.session_id.clone()).collect::<Vec<_>>(), [None, Some("S1".to_owned())]);
});

over_both_transports!(a_remote_error_reaches_the_command_that_caused_it, |browser, connection| {
    let error = within(connection.send("Fail", json!({}), None)).await.unwrap_err();
    assert_eq!(error, CdpError::Remote { code: Some(-32000), message: "Cannot find context with specified id".into() });
    // And the connection still works.
    assert_eq!(within(connection.send("Echo", json!({"ok": true}), None)).await.unwrap(), json!({"ok": true}));
    assert_eq!(browser.commands().len(), 2);
});

over_both_transports!(a_reply_larger_than_any_read_arrives_whole, |browser, connection| {
    let reply = within(connection.send("Big", json!({}), None)).await.unwrap();
    assert_eq!(reply["data"].as_str().unwrap().len(), 300_000);
    let _ = browser;
});

over_both_transports!(concurrent_commands_are_each_answered, |browser, connection| {
    let sends: Vec<_> = (0..20).map(|n| connection.send("Echo", json!({"n": n}), None)).collect();
    let replies = within(futures_util::future::join_all(sends)).await;
    for (n, reply) in replies.into_iter().enumerate() {
        assert_eq!(reply.unwrap(), json!({"n": n}));
    }
    assert_eq!(browser.commands().len(), 20);
});

over_both_transports!(events_reach_the_subscribers_that_asked_for_them_in_order, |browser, connection| {
    let mut everything = connection.events(None);
    let mut first_page = connection.events(Some("S1"));
    let mut second_page = connection.events(Some("S2"));
    within(connection.send("Noisy", json!({}), None)).await.unwrap();
    // A reply is routed after the events that preceded it, so by now all three have been delivered.
    let mut all = Vec::new();
    while let Ok(event) = everything.try_recv() {
        all.push((event.method.clone(), event.session_id.clone(), event.params["n"].as_i64().unwrap()));
    }
    assert_eq!(
        all,
        [("Page.frameNavigated".to_owned(), Some("S1".to_owned()), 1), ("Page.loadEventFired".to_owned(), Some("S2".to_owned()), 2), ("Target.targetCreated".to_owned(), None, 3)]
    );
    assert_eq!(first_page.try_recv().unwrap().params["n"], 1);
    assert!(first_page.try_recv().is_err());
    assert_eq!(second_page.try_recv().unwrap().params["n"], 2);
    assert!(second_page.try_recv().is_err());
    let _ = browser;
});

over_both_transports!(a_callback_has_seen_every_event_before_the_reply_that_followed_them_is_returned, |browser, connection| {
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let seen = seen.clone();
        connection.on_event(Some("S1"), move |event| seen.lock().unwrap().push(event.method.clone()));
    }
    within(connection.send("Noisy", json!({}), None)).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), ["Page.frameNavigated"]);
    let _ = browser;
});

over_both_transports!(a_hang_up_fails_what_waits_and_ends_what_listens, |browser, connection| {
    let mut events = connection.events(None);
    // The endpoint hangs up when it receives this command, so the command can only end by the close.
    assert_eq!(within(connection.send("Silent", json!({}), None)).await, Err(CdpError::Closed));
    within(connection.closed()).await;
    assert!(connection.is_closed());
    assert_eq!(within(connection.send("Echo", json!({}), None)).await, Err(CdpError::Closed));
    assert!(within(events.recv()).await.is_none());
    // A subscription made after the close is already over.
    let mut late = connection.events(None);
    assert!(within(late.recv()).await.is_none());
    let _ = browser;
});

#[tokio::test]
async fn the_browser_going_away_is_noticed_without_a_command_in_flight() {
    let browser = endpoint();
    let connection = pipe(&browser).await;
    within(connection.send("Echo", json!({}), None)).await.unwrap();
    browser.hang_up();
    within(connection.closed()).await;
    let browser = endpoint();
    let (socket, task) = websocket(&browser).await;
    within(socket.send("Echo", json!({}), None)).await.unwrap();
    browser.hang_up();
    within(socket.closed()).await;
    within(task).await.unwrap();
}

#[tokio::test]
async fn messages_that_are_not_for_anyone_are_dropped_and_do_not_disturb_the_connection() {
    let browser = endpoint();
    let connection = pipe(&browser).await;
    let mut events = connection.events(None);
    for frame in [json!("a string"), json!(7), json!([1, 2]), json!({"id": 9999, "result": {}}), json!({"nothing": true}), json!({"method": 5})] {
        browser.push(frame);
    }
    browser.push(json!({"method": "Page.frameNavigated", "params": {"n": 1}}));
    // A reply and an event both still come through, in order, after all of it.
    assert_eq!(within(connection.send("Echo", json!({"after": true}), None)).await.unwrap(), json!({"after": true}));
    let event = within(events.recv()).await.unwrap();
    assert_eq!(event.method, "Page.frameNavigated");
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn a_posted_command_is_sent_and_its_reply_is_dropped() {
    let browser = endpoint();
    let connection = pipe(&browser).await;
    connection.post("Fire", json!({"x": 1}), Some("S1"));
    connection.post_parts(|id| vec![Arc::from(format!("{{\"id\":{id},\"method\":\"Fire\",\"params\":{{\"body\":\"").into_bytes()), Arc::from(&b"AAAA"[..]), Arc::from(&b"\"}}"[..])]);
    // A later command is answered, which means the posted ones were read first and their replies went nowhere.
    within(connection.send("Echo", json!({}), None)).await.unwrap();
    let fired = browser.commands_named("Fire");
    assert_eq!(fired.len(), 2);
    assert_eq!((fired[0].id, fired[0].session_id.clone(), fired[0].params.clone()), (1, Some("S1".to_owned()), json!({"x": 1})));
    assert_eq!((fired[1].id, fired[1].params.clone()), (2, json!({"body": "AAAA"})));
}

#[tokio::test]
async fn closing_one_clone_closes_them_all() {
    let browser = endpoint();
    let connection = pipe(&browser).await;
    let other = connection.clone();
    assert!(!other.is_closed());
    connection.close();
    assert!(other.is_closed());
    assert_eq!(other.send("Echo", json!({}), None).await, Err(CdpError::Closed));
}

#[tokio::test]
async fn only_a_websocket_on_this_machine_is_connected_to() {
    for url in ["ws://example.com:9222/devtools/browser/x", "ws://127.0.0.1.example.com/x", "wss://127.0.0.1:9222/x", "http://127.0.0.1:9222/x", "ws://192.168.1.5:9222/x", "ws://[2001:db8::1]:9222/x", "not a url", "ws:///x"] {
        match CdpConnection::connect_websocket(url).await {
            Err(CdpConnectError::NotLoopback(refused)) => assert_eq!(refused, url),
            other => panic!("{url} was not refused: {:?}", other.err()),
        }
    }
    // A loopback address nothing listens on is a connection failure, not a refusal.
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    for url in [format!("ws://127.0.0.1:{free}/x"), format!("ws://localhost:{free}/x"), format!("ws://[::1]:{free}/x")] {
        assert!(matches!(CdpConnection::connect_websocket(&url).await.err(), Some(CdpConnectError::Connect(_))), "{url}");
    }
}

#[test]
fn what_a_command_waiting_for_a_closed_connection_says() {
    assert_eq!(CdpError::Closed.to_string(), "the browser connection closed");
    assert_eq!(CdpError::Remote { code: None, message: "boom".into() }.to_string(), "boom");
    let _: Option<Value> = None;
}
