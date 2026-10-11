//! Several drafts of new threads, through the mounted UI and the real native server: a new-thread request leaves a draft with
//! work in it where it is and opens another, the sidebar lists the drafts that hold work, a row opens its draft with its text, a
//! discard can be taken back, and sending a draft starts its thread and takes the draft away.
use super::diff_tests::{Fixture, seed, until};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control, input_control};

fn draft_text(state: Store<UiModel>, id: &str) -> String {
    let destination = state.peek().destination.clone().unwrap();
    state.peek().new_thread_drafts.get(&(destination, id.to_owned())).cloned().unwrap_or_default()
}
fn active(state: Store<UiModel>) -> String {
    state.peek().active_draft.clone().expect("the form shows a draft")
}
fn open_new(dom: &mut VirtualDom, props: &super::diff_tests::Harness, state: Store<UiModel>) {
    dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, Some("project".into())));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}

#[tokio::test(flavor = "current_thread")]
async fn drafts_with_work_stay_apart_list_in_the_sidebar_reopen_with_their_text_and_go_when_sent() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;

    open_new(&mut dom, &props, state);
    until(&mut dom, "the form", |dom| control(dom, "First message").is_some()).await;
    let first = active(state);
    input_control(&mut dom, "First message", "alpha: fix the flaky login test");
    assert!(control(&dom, "Discard draft").is_none(), "the open draft is not listed, and nothing else is");

    // A new thread while the draft holds work opens another and leaves the first in the sidebar.
    open_new(&mut dom, &props, state);
    until(&mut dom, "a second draft", |_| state.peek().active_draft.as_deref().is_some_and(|id| id != first)).await;
    let second = active(state);
    until(&mut dom, "its row", |dom| control(dom, "Unsent draft alpha: fix the flaky login test").is_some()).await;
    assert_eq!(draft_text(state, &second), "", "the second draft starts empty");
    input_control(&mut dom, "First message", "beta: add a changelog");

    // A third time: both are listed, newest work first among them is not promised; both are there.
    open_new(&mut dom, &props, state);
    until(&mut dom, "a third draft", |_| state.peek().active_draft.as_deref().is_some_and(|id| id != first && id != second)).await;
    until(&mut dom, "both rows", |dom| control(dom, "Unsent draft alpha: fix the flaky login test").is_some() && control(dom, "Unsent draft beta: add a changelog").is_some()).await;

    // A row opens its draft with the text in it; the empty one the form left is gone.
    click_control(&mut dom, "Unsent draft alpha: fix the flaky login test");
    until(&mut dom, "the first draft", |_| state.peek().active_draft.as_deref() == Some(first.as_str())).await;
    assert_eq!(draft_text(state, &first), "alpha: fix the flaky login test");
    until(&mut dom, "its row to leave the list", |dom| control(dom, "Unsent draft alpha: fix the flaky login test").is_none()).await;
    assert!(control(&dom, "Unsent draft beta: add a changelog").is_some());

    // Discarding the other takes its row away, and the notice takes it back.
    click_control(&mut dom, "Discard draft");
    until(&mut dom, "the row to go", |dom| control(dom, "Unsent draft beta: add a changelog").is_none()).await;
    click_control(&mut dom, "Undo discard");
    until(&mut dom, "the row to return", |dom| control(dom, "Unsent draft beta: add a changelog").is_some()).await;

    // Sending the open draft starts its thread; the draft is gone and the other stays.
    click_control(&mut dom, "Create new thread");
    until(&mut dom, "the thread", |_| state.peek().active_thread.as_deref().is_some_and(|id| id != "thread-a" && id != "thread-b")).await;
    assert!(state.peek().draft_storage.document.borrow().draft(&t3_client::draft_storage::DraftTarget::draft(state.peek().destination.as_ref().unwrap().to_string(), first.clone())).is_none(), "a sent draft is gone");
    until(&mut dom, "the other draft to stay listed", |dom| control(dom, "Unsent draft beta: add a changelog").is_some()).await;
    fixture.finish(dom).await;
}
