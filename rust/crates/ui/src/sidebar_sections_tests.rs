//! The sidebar's sections and moving a row, through the mounted UI and the real native server.
//! The pointer drag itself runs in the page (`assets/sidebar_drag_abi.js`) and is not exercised here;
//! these tests send the one `drop` it reports, which is where the work is. Waits are on state or text
//! that only the awaited step produces.
use super::diff_tests::{Fixture, rendered_text, seed, until};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::key_on;
use crate::runtime::transport_tests::{click_control, control, control_attribute};
use crate::sidebar_sections::{DropRequest, drop_row};
use t3_client::sidebar::Section;

async fn thread(dom: &mut VirtualDom, props: &super::diff_tests::Harness, state: Store<UiModel>, id: &str) {
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.create", json!({"createdBy":"user","creationSource":"web","threadId":id,"projectId":"project","title":id,"modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null}))).unwrap();
    until(dom, id, |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.iter().any(|t| t.id.as_str() == id))).await;
}
async fn pin(dom: &mut VirtualDom, props: &super::diff_tests::Harness, state: Store<UiModel>, id: &str) {
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.pin", json!({"threadId":id}))).unwrap();
    until(dom, id, |_| find(state, id).is_some_and(|t| t3_client::sidebar::is_pinned(&t))).await;
}
fn find(state: Store<UiModel>, id: &str) -> Option<t3_contracts::ThreadShell> {
    state.peek().shell.snapshot.as_ref()?.threads.iter().find(|thread| thread.id.as_str() == id).cloned()
}
/// The ids a section lists, in order, as the sidebar shows them.
fn listed(state: Store<UiModel>, section: Section) -> Vec<String> {
    let model = state.peek();
    let snapshot = model.shell.snapshot.as_ref().unwrap();
    let caps = crate::sidebar_rows::caps(state);
    let environment = model.destination.clone().unwrap();
    let scope = crate::sidebar_groups::Scope::single(environment.as_str(), "project");
    let listed: Vec<_> = snapshot.threads.iter().map(|thread| (&environment, thread)).collect();
    crate::sidebar_sections::sections_of(&listed, &scope, chrono::Utc::now(), caps, model.reorder.held.as_ref()).section(section).iter().map(|thread| thread.id.to_string()).collect()
}
fn pin_keys(state: Store<UiModel>) -> Vec<(String, Option<String>)> {
    let mut keys: Vec<_> = state.peek().shell.snapshot.as_ref().unwrap().threads.iter().map(|thread| (thread.id.to_string(), thread.pin_order_key.clone().flatten().map(|key| key.to_string()))).collect();
    keys.sort();
    keys
}
fn drop_on(dom: &mut VirtualDom, props: &super::diff_tests::Harness, state: Store<UiModel>, id: &str, section: Section, before: Option<&str>) {
    let request = DropRequest { id: id.into(), section, before: before.map(str::to_owned) };
    dom.in_scope(ScopeId::APP, || drop_row(&props.transport, state, request));
}

#[tokio::test(flavor = "current_thread")]
async fn a_dropped_pin_takes_its_place_with_one_key_once_its_neighbours_have_keys() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    thread(&mut dom, &props, state, "thread-c").await;
    for id in ["thread-a", "thread-b", "thread-c"] {
        pin(&mut dom, &props, state, id).await;
    }
    // Keyless pins list newest first; moving one materialises keys for the section.
    let before = listed(state, Section::Pinned);
    assert_eq!(before.len(), 3);
    let last = before[2].clone();
    drop_on(&mut dom, &props, state, &last, Section::Pinned, Some(&before[0]));
    // Held where it was dropped, before the server has answered.
    assert_eq!(listed(state, Section::Pinned), vec![last.clone(), before[0].clone(), before[1].clone()]);
    until(&mut dom, "the keys", |_| pin_keys(state).iter().all(|(_, key)| key.is_some()) && state.peek().reorder == Default::default()).await;
    assert_eq!(listed(state, Section::Pinned), vec![last.clone(), before[0].clone(), before[1].clone()]);
    // The sidebar lists them in that order.
    let text = rendered_text(&dom);
    let at = |id: &str| text.find(&format!("{id}\u{25c6}")).unwrap_or_else(|| panic!("{id} is not listed as pinned: {text}"));
    assert!(at(&last) < at(&before[0]) && at(&before[0]) < at(&before[1]));

    // With every pin keyed, moving one writes that one key and no other.
    let keyed = pin_keys(state);
    let order = listed(state, Section::Pinned);
    drop_on(&mut dom, &props, state, &order[2], Section::Pinned, Some(&order[1]));
    until(&mut dom, "the move", |_| listed(state, Section::Pinned) == vec![order[0].clone(), order[2].clone(), order[1].clone()] && state.peek().reorder == Default::default()).await;
    let now = pin_keys(state);
    let changed: Vec<_> = keyed.iter().zip(&now).filter(|(a, b)| a != b).map(|(a, _)| a.0.clone()).collect();
    assert_eq!(changed, vec![order[2].clone()], "one write to one thread");
    // Dropping where it already is writes nothing.
    let same = pin_keys(state);
    drop_on(&mut dom, &props, state, &order[0], Section::Pinned, Some(&order[2]));
    assert_eq!(state.peek().reorder, Default::default());
    assert_eq!(pin_keys(state), same);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_refused_drop_puts_the_row_back_and_says_so() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    for id in ["thread-a", "thread-b"] {
        pin(&mut dom, &props, state, id).await;
    }
    // A pin this server has never heard of, as when another client removed it a moment ago.
    {
        let mut shell = state.shell();
        let mut shell = shell.write();
        let snapshot = shell.snapshot.as_mut().unwrap();
        let mut ghost = snapshot.threads[0].clone();
        ghost.id = "thread-ghost".parse().unwrap();
        ghost.title = "Ghost".into();
        snapshot.threads.push(ghost);
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let before = listed(state, Section::Pinned);
    assert!(before.contains(&"thread-ghost".to_owned()));
    drop_on(&mut dom, &props, state, "thread-ghost", Section::Pinned, Some(&before[0]));
    assert_eq!(listed(state, Section::Pinned)[0], "thread-ghost", "held at the drop");
    until(&mut dom, "the refusal", |dom| rendered_text(dom).contains("Could not reorder the pinned threads")).await;
    assert_eq!(listed(state, Section::Pinned), before, "the row is back where the server has it");
    assert_eq!(state.peek().reorder, Default::default(), "the next drop is not blocked");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_between_pinned_and_active_pins_and_unpins() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    pin(&mut dom, &props, state, "thread-a").await;
    assert_eq!(listed(state, Section::Active), vec!["thread-b"]);
    // Into the pins, in front of the other one.
    drop_on(&mut dom, &props, state, "thread-b", Section::Pinned, Some("thread-a"));
    until(&mut dom, "the pin", |_| find(state, "thread-b").is_some_and(|t| t3_client::sidebar::is_pinned(&t)) && state.peek().reorder == Default::default()).await;
    assert_eq!(listed(state, Section::Pinned), vec!["thread-b", "thread-a"]);
    assert!(listed(state, Section::Active).is_empty());
    // And back out: unpinned, and placed in the active order.
    drop_on(&mut dom, &props, state, "thread-b", Section::Active, None);
    until(&mut dom, "the unpin", |_| find(state, "thread-b").is_some_and(|t| !t3_client::sidebar::is_pinned(&t)) && state.peek().reorder == Default::default()).await;
    assert_eq!(listed(state, Section::Pinned), vec!["thread-a"]);
    assert_eq!(listed(state, Section::Active), vec!["thread-b"]);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn active_threads_move_from_the_menu_by_writing_active_order_keys() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    let first = listed(state, Section::Active);
    assert_eq!(first.len(), 2);
    key_on(&mut dom, &format!("Thread {}", first[0]), Key::ContextMenu);
    assert_eq!(control_attribute(&dom, "Move thread up", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"), "the top one cannot go higher");
    click_control(&mut dom, "Move thread down");
    // Neither thread had a key, so the move writes one to each, as two commands: the order is already the new one when
    // the first has landed, and the second key is still on its way.
    until(&mut dom, "the new order, with a key on each", |_| listed(state, Section::Active) == vec![first[1].clone(), first[0].clone()] && state.peek().shell.snapshot.as_ref().unwrap().threads.iter().all(|thread| thread.active_order_key.clone().flatten().is_some())).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn settled_and_snoozed_threads_fold_into_shelves_that_open_on_their_headers() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    // The server advertises settlement, so the (empty) Settled shelf is there to drop a row on.
    assert!(rendered_text(&dom).contains("Settled (0)"));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    key_on(&mut dom, "Thread thread-a", Key::ContextMenu);
    click_control(&mut dom, "Settle thread");
    until(&mut dom, "the shelf", |dom| rendered_text(dom).contains("Settled (1)")).await;
    // Folded away by default: the row is not listed until the header opens it.
    assert_eq!(listed(state, Section::Settled), vec!["thread-a"]);
    assert!(control(&dom, "Thread thread-a").is_none());
    click_control(&mut dom, "Settled (1)");
    assert!(control(&dom, "Thread thread-a").is_some());
    // From the shelf a thread comes back to the active list, by the menu or by a drop.
    drop_on(&mut dom, &props, state, "thread-a", Section::Active, None);
    until(&mut dom, "the unsettle", |_| listed(state, Section::Active).len() == 2 && state.peek().reorder == Default::default()).await;
    assert!(listed(state, Section::Settled).is_empty());
    // A snooze folds the same way, under its own header.
    let wake = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.snooze", json!({"threadId":"thread-b","snoozedUntil":wake}))).unwrap();
    until(&mut dom, "the snoozed shelf", |dom| rendered_text(dom).contains("Snoozed (1)")).await;
    assert!(control(&dom, "Thread thread-b").is_none());
    click_control(&mut dom, "Snoozed (1)");
    until(&mut dom, "the open shelf", |dom| control(dom, "Thread thread-b").is_some()).await;
    // The shelf is a place a row can be taken from but never dropped on: a snooze needs a wake time.
    drop_on(&mut dom, &props, state, "thread-a", Section::Snoozed, None);
    assert_eq!(state.peek().reorder, Default::default());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_drop_while_one_is_being_written_or_without_the_right_is_ignored() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    pin(&mut dom, &props, state, "thread-a").await;
    drop_on(&mut dom, &props, state, "thread-b", Section::Pinned, Some("thread-a"));
    let held = state.peek().reorder.clone();
    assert!(held.busy && held.held.is_some());
    // The second drop while the first is in flight does nothing.
    drop_on(&mut dom, &props, state, "thread-a", Section::Pinned, None);
    assert_eq!(state.peek().reorder, held);
    until(&mut dom, "the first drop", |_| state.peek().reorder == Default::default()).await;
    fixture.finish(dom).await;
}
