//! The window's report of its own activity through the mounted UI and the real native server: the server's background
//! policy (`subscribeBackgroundPolicy`) shows the lease the report opened, with the baseline scope and the status stream
//! the branch control holds, and shows the next report's window state. The page's sensor is replaced by a feed the test
//! controls; the timing around it (debounce, interval) is the component's own. Waits are on the policy's own snapshots.
use super::diff_tests::{Fixture, open_thread, seed, until, wait_for_rendered_text};
use super::*;
use crate::background_activity::ObservationFeed;
use crate::diff_panel::Gate;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use t3_client::background_activity::Observation;

fn observation(visible: bool, focused: bool, idle_ms: i64) -> Observation {
    Observation { client_id: "test-window".into(), visible, focused, idle_ms: Some(idle_ms) }
}

/// The next snapshot of the policy stream for which `ready` holds, while the document keeps running.
async fn snapshot_where(dom: &mut VirtualDom, stream: &mut StreamRequest, ready: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        loop {
            tokio::select! {
                item = stream.next() => {
                    let values = item.expect("the policy stream ended").expect("the policy stream failed");
                    if let Some(found) = values.into_iter().find(|value| ready(value)) {
                        return found;
                    }
                }
                () = dom.wait_for_work() => { dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations); }
            }
        }
    })
    .await
    .expect("the policy did not reach the awaited lease")
}

fn lease_of<'a>(snapshot: &'a Value, client: &str) -> Option<&'a Value> {
    snapshot["leases"].as_array()?.iter().find(|lease| lease["clientId"] == client)
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_window_reports_its_state_and_the_scopes_its_streams_hold_and_the_server_keeps_a_lease() {
    let fixture = Fixture::start_with_background(true).await;
    let (sender, receiver) = futures_channel::mpsc::unbounded::<Observation>();
    let feed = ObservationFeed(Rc::new(RefCell::new(Some(receiver))));
    let (mut dom, props) = fixture.client_with(
        Gate::default(),
        &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SourceControlWrite],
        |dom| dom.provide_root_context(feed),
    );
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    // The branch control subscribes to the directory's status with remote refresh, which keeps that scope wanted.
    until(&mut dom, "the branch control's status stream", |_| !crate::background_activity::retained_scopes(&state.peek().destination.as_ref().unwrap().to_string()).is_empty()).await;
    let mut policy = dom.in_scope(ScopeId::APP, || request_stream(&props.transport, state, "subscribeBackgroundPolicy", json!({}))).unwrap();

    sender.unbounded_send(observation(true, true, 0)).unwrap();
    let cwd = fixture.workspace.to_string_lossy().into_owned();
    let seen = snapshot_where(&mut dom, &mut policy, |snapshot| lease_of(snapshot, "test-window").is_some_and(|lease| lease["scopes"].as_array().is_some_and(|scopes| scopes.len() == 2))).await;
    let lease = lease_of(&seen, "test-window").unwrap();
    assert_eq!((lease["visible"].clone(), lease["focused"].clone(), lease["recentlyInteracted"].clone()), (json!(true), json!(true), json!(true)));
    assert_eq!(lease["clientKind"], "web");
    assert_eq!(lease["scopes"][0], json!({"type": "provider-status"}));
    assert_eq!(lease["scopes"][1]["type"], "vcs-status");
    assert!(lease["scopes"][1]["cwd"].as_str().is_some_and(|reported| std::path::Path::new(reported).file_name() == std::path::Path::new(&cwd).file_name()));

    // Hidden and idle: the next report replaces the lease, it does not add one.
    sender.unbounded_send(observation(false, false, 300_000)).unwrap();
    let seen = snapshot_where(&mut dom, &mut policy, |snapshot| lease_of(snapshot, "test-window").is_some_and(|lease| lease["visible"] == json!(false))).await;
    let lease = lease_of(&seen, "test-window").unwrap();
    assert_eq!((lease["focused"].clone(), lease["recentlyInteracted"].clone(), lease["appState"].clone()), (json!(false), json!(false), json!("background")));
    assert_eq!(seen["leases"].as_array().unwrap().iter().filter(|lease| lease["clientId"] == "test-window").count(), 1);

    drop(policy);
    wait_for_rendered_text(&mut dom, "\u{2442}").await;
    fixture.finish(dom).await;
}
