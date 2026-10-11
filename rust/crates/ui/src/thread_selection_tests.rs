//! Choosing several threads in the sidebar and acting on them together, through the mounted UI and the real native
//! server: Ctrl/Cmd+click and Shift+click choose rows without opening them, the right-click menu counts what its
//! actions touch, and settling or deleting acts on every chosen thread.
use super::diff_tests::{Fixture, open_thread, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_when_enabled, control, control_attribute, mouse_control};

const SCOPES: [AuthEnvironmentScope; 2] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];

async fn connected(fixture: &Fixture) -> (VirtualDom, super::diff_tests::Harness, Store<UiModel>) {
    let (mut dom, props) = fixture.client(Gate::default(), &SCOPES);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "config", |_| state.peek().typed_config.is_some()).await;
    (dom, props, state)
}
fn chosen(dom: &VirtualDom, thread: &str) -> bool {
    control_attribute(dom, &format!("Thread {thread}"), "aria-pressed").and_then(|(_, value)| value).as_deref() == Some("true")
}
/// The modifier of this machine's Ctrl/Cmd+click.
fn toggle(dom: &mut VirtualDom, thread: &str) {
    let mac = crate::keys::platform().is_mac();
    mouse_control(dom, &format!("Thread {thread}"), "click", !mac, mac, false);
}

#[tokio::test(flavor = "current_thread")]
async fn modifier_clicks_choose_rows_without_opening_them_and_a_plain_click_ends_the_choice() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = connected(&fixture).await;
    until(&mut dom, "both rows", |dom| control(dom, "Thread thread-a").is_some() && control(dom, "Thread thread-b").is_some()).await;
    toggle(&mut dom, "thread-a");
    until(&mut dom, "the first row chosen", |dom| chosen(dom, "thread-a")).await;
    assert!(state.peek().active_thread.is_none(), "choosing a row does not open it");
    // Shift+click extends from the anchor to the row clicked.
    mouse_control(&mut dom, "Thread thread-b", "click", false, false, true);
    until(&mut dom, "the range chosen", |dom| chosen(dom, "thread-a") && chosen(dom, "thread-b")).await;
    assert!(state.peek().active_thread.is_none());
    // Toggling a chosen row takes it out.
    toggle(&mut dom, "thread-a");
    until(&mut dom, "one left", |dom| !chosen(dom, "thread-a") && chosen(dom, "thread-b")).await;
    // A plain click opens the thread and ends the choice.
    click_when_enabled(&mut dom, "Thread thread-a").await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "no rows chosen", |dom| !chosen(dom, "thread-a") && !chosen(dom, "thread-b")).await;
    assert!(!state.peek().selection.has_selection());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_menu_of_chosen_rows_counts_them_and_settling_acts_on_each() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connected(&fixture).await;
    until(&mut dom, "both rows", |dom| control(dom, "Thread thread-a").is_some() && control(dom, "Thread thread-b").is_some()).await;
    toggle(&mut dom, "thread-a");
    mouse_control(&mut dom, "Thread thread-b", "click", false, false, true);
    until(&mut dom, "both chosen", |dom| chosen(dom, "thread-a") && chosen(dom, "thread-b")).await;
    // The menu belongs to the whole choice, whichever chosen row it was opened on.
    mouse_control(&mut dom, "Thread thread-b", "contextmenu", false, false, false);
    until(&mut dom, "the bulk menu", |dom| control(dom, "Selected threads").is_some()).await;
    for label in ["Mark unread (2)", "Delete (2)"] {
        assert!(control(&dom, label).is_some(), "missing {label}");
    }
    assert!(control(&dom, "Thread actions").is_none(), "not the single-row menu");
    if control(&dom, "Settle (2)").is_none() {
        // The server of this fixture does not settle threads: nothing more to prove here.
        fixture.finish(dom).await;
        return;
    }
    click_when_enabled(&mut dom, "Settle (2)").await;
    until(&mut dom, "both settled and the choice ended", |dom| control(dom, "Settled (2)").is_some() && !state.peek().selection.has_selection() && !chosen(dom, "thread-a")).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn deleting_the_chosen_threads_asks_once_and_removes_them_all() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connected(&fixture).await;
    until(&mut dom, "both rows", |dom| control(dom, "Thread thread-a").is_some() && control(dom, "Thread thread-b").is_some()).await;
    toggle(&mut dom, "thread-a");
    mouse_control(&mut dom, "Thread thread-b", "click", false, false, true);
    until(&mut dom, "both chosen", |dom| chosen(dom, "thread-a") && chosen(dom, "thread-b")).await;
    mouse_control(&mut dom, "Thread thread-a", "contextmenu", false, false, false);
    click_when_enabled(&mut dom, "Delete (2)").await;
    wait_for_rendered_text(&mut dom, "Delete 2 threads?").await;
    click_when_enabled(&mut dom, "Confirm").await;
    until(&mut dom, "both threads gone", |dom| control(dom, "Thread thread-a").is_none() && control(dom, "Thread thread-b").is_none()).await;
    until(&mut dom, "the choice emptied", |_| !state.peek().selection.has_selection()).await;
    fixture.finish(dom).await;
}
