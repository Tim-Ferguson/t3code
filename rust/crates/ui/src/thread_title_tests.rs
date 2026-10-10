//! Thread titles through the mounted UI and the real native server: the first message carries the
//! seed that gives the thread its temporary title, and "Regenerate title" shows its work in flight
//! from the thread's own marker, so that it ends when the marker clears and not on a timer.
use super::diff_tests::{Fixture, error, open_thread, seed, turn, until};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control, control_attribute, rendered_text};

fn title_of(state: Store<UiModel>) -> String {
    state.peek().shell.snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == "thread-a")).map(|thread| thread.title.to_string()).unwrap_or_default()
}

fn set_marker(state: Store<UiModel>, on: bool) {
    let marker = on.then(|| serde_json::from_value::<t3_contracts::TitleRegeneration>(json!({"requestId":"r1","startedAt":"2026-01-01T00:00:00Z"})).unwrap());
    let mut shell = state.shell();
    let mut shell = shell.write();
    let snapshot = shell.snapshot.as_mut().unwrap();
    snapshot.threads.iter_mut().find(|thread| thread.id.as_str() == "thread-a").unwrap().title_regeneration = Some(marker);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_first_message_titles_an_empty_thread_and_a_later_one_leaves_the_title_alone() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert_eq!(title_of(state), "thread-a");
    // The thread has no messages, so its first one carries the seed made from that message ("Do the work 1").
    turn(&mut dom, &props, state, 1).await;
    assert_eq!(title_of(state), "Do the work 1");
    assert_eq!(error(state), None);
    // With a message in the projection nothing is sent, and the title stays.
    turn(&mut dom, &props, state, 2).await;
    assert_eq!(title_of(state), "Do the work 1");
    assert_eq!(error(state), None);
    // A new thread takes one from its first message.
    dom.in_scope(ScopeId::APP, || crate::runtime::new_thread(&props.transport, state, Some("project".into())));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    crate::runtime::transport_tests::input_control(&mut dom, "First message", "Name the new thread");
    click_control(&mut dom, "Create new thread");
    until(&mut dom, "the new thread, titled from its message", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|thread| thread.title == "Name the new thread"))).await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn regenerating_a_title_is_offered_when_the_server_can_and_shows_the_threads_marker() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    // A server that does not say it can write titles gets no button.
    assert!(control(&dom, "Regenerate title").is_none());
    let mut config = state.peek().typed_config.clone().unwrap();
    config.environment.capabilities.thread_title_regeneration = Some(true);
    state.typed_config().set(Some(config));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(control_attribute(&dom, "Regenerate title", "disabled").unwrap().1.as_deref(), Some("false"));
    assert!(rendered_text(&dom).contains("Regenerate title"));
    // The server arms the marker: the button says so, cannot be pressed again, and the sidebar row says it too.
    set_marker(state, true);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(control_attribute(&dom, "Regenerate title", "disabled").unwrap().1.as_deref(), Some("true"));
    assert!(rendered_text(&dom).contains("Regenerating\u{2026}") && rendered_text(&dom).contains("Regenerating title"));
    // The marker clears and the indicator is gone with it.
    set_marker(state, false);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(!rendered_text(&dom).contains("Regenerating") && control_attribute(&dom, "Regenerate title", "disabled").unwrap().1.as_deref() == Some("false"));
    // Pressing it sends the request the server answers with its marker.
    click_control(&mut dom, "Regenerate title");
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}
