//! Open in editor, end to end: a file named in a message goes through the real server to a real
//! editor. This is the one test that launches anything, and what it opens is named for this work
//! (a directory `t3code-rust-port-test-…` holding `t3code-rust-port-test-open-in-editor.txt`,
//! whose first line says it is safe to close). Every other fixture offers no editor.
use super::diff_tests::{Fixture, completed_runs, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control, input_control};

const FILE: &str = "t3code-rust-port-test-open-in-editor.txt";
const FIRST_LINE: &str = "This file was opened by an automated test of the T3 Code Rust port. It is safe to close.";

#[tokio::test(flavor = "current_thread")]
async fn a_file_link_launches_the_editor_once_and_with_no_editor_installed_says_so() {
    let fixture = Fixture::start_with_editors().await;
    assert!(fixture.workspace.to_string_lossy().contains("t3code-rust-port-test-"), "{:?}", fixture.workspace);
    fixture.write(FILE, &format!("{FIRST_LINE}\n"));
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    input_control(&mut dom, "Message", &format!("open [the file]({FILE}:1)"));
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the turn", |_| completed_runs(state) == 1).await;
    let label = format!("Open {FILE} at line 1");
    until(&mut dom, "the link", |dom| control(dom, &label).is_some()).await;
    let offered = crate::open_in::available(state);

    click_control(&mut dom, &label);
    if offered.is_empty() {
        // No editor on this machine: nothing is asked of the server, and the person is told.
        wait_for_rendered_text(&mut dom, "No editor to open the file in").await;
        assert!(props.transport.borrow().unary_waiters.is_empty());
    } else {
        // The launch went out with the click, once; the server answers it, and a refusal would show.
        assert_eq!(props.transport.borrow().unary_waiters.len(), 1, "one click, one launch");
        until(&mut dom, "the server's answer", |_| props.transport.borrow().unary_waiters.is_empty()).await;
        assert!(state.peek().notices.is_empty(), "the launch was refused: {:?}", state.peek().notices.iter().map(|notice| notice.detail.clone()).collect::<Vec<_>>());
        assert!(!rendered_text(&dom).contains("No editor to open the file in"));
    }
    fixture.finish(dom).await;
}
