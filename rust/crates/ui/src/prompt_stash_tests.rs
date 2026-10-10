//! The prompt stash through the mounted UI and the real native server: a prompt with a chip and an
//! uploaded file is put aside from one thread and brought back into another; it survives a reload;
//! a refused write leaves the composer alone; the shortcut restores a lone entry and opens the list
//! for several. The browser's storage is the test's own memory. Waits are on state only the awaited
//! step produces.
use super::diff_tests::{Fixture, open_thread, rendered_text, seed, until};
use super::*;
use crate::diff_panel::Gate;
use crate::prompt_stash::{MEMORY, REJECT, shortcut};
use crate::runtime::keys_tests::key_on;
use crate::runtime::transport_tests::{click_control, control};
use t3_client::{attachments::Upload, draft_storage::DraftTarget};

fn target(state: Store<UiModel>, thread: &str) -> DraftTarget {
    DraftTarget::thread(state.peek().destination.as_ref().unwrap().to_string(), thread)
}
fn stashed(state: Store<UiModel>) -> Vec<t3_client::prompt_stash::Entry> {
    state.peek().stash.entries.clone()
}
fn type_prompt(dom: &mut VirtualDom, state: Store<UiModel>, text: &str) {
    dom.in_scope(ScopeId::APP, || crate::draft_storage::edit_thread_prompt(state, text.to_owned()));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}
fn attach_file(dom: &mut VirtualDom, props: &super::diff_tests::Harness, state: Store<UiModel>, target: &DraftTarget, name: &str) {
    dom.in_scope(ScopeId::APP, || crate::attachments::attach(&props.transport, state, target, vec![crate::attachments::Incoming { name: name.into(), mime_type: "text/plain".into(), size: 5, source: crate::attachments::Source::Bytes(std::rc::Rc::from(&b"hello"[..])) }]));
}
fn uploads(state: Store<UiModel>, target: &DraftTarget) -> Vec<Upload> {
    state.draft_attachments().peek().get(target).map(|list| list.iter().map(|attachment| attachment.upload.clone()).collect()).unwrap_or_default()
}

#[tokio::test(flavor = "current_thread")]
async fn a_prompt_with_its_chip_and_file_is_put_aside_here_and_brought_back_there() {
    MEMORY.with(|memory| *memory.borrow_mut() = None);
    REJECT.with(|reject| reject.set(false));
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let here = target(state, "thread-a");
    // A thread attached as a chip, a file uploaded, and words around them.
    key_on(&mut dom, "Thread thread-b", Key::ContextMenu);
    click_control(&mut dom, "Reference in chat");
    until(&mut dom, "the chip", |dom| control(dom, "Remove thread-b").is_some()).await;
    attach_file(&mut dom, &props, state, &here, "notes.txt");
    until(&mut dom, "the upload", |_| uploads(state, &here) == vec![Upload::Ready]).await;
    type_prompt(&mut dom, state, "  compare these  ");
    dom.in_scope(ScopeId::APP, || shortcut(&props.transport, state));
    until(&mut dom, "the stash", |_| stashed(state).len() == 1 && state.peek().stash.busy == false).await;
    // The composer is empty: no words, no chip, no file; the entry holds them, and storage too.
    assert_eq!(state.peek().draft, "");
    assert!(control(&dom, "Remove thread-b").is_none());
    assert!(uploads(state, &here).is_empty());
    let entry = stashed(state).remove(0);
    assert_eq!((entry.prompt.as_str(), entry.attachments.len(), entry.records.len()), ("compare these", 1, 1));
    assert!(MEMORY.with(|memory| memory.borrow().clone()).unwrap().contains(&entry.id));
    wait(&mut dom, "Stashed prompts: 1. Open stash.").await;
    assert!(control(&dom, "Stashed prompts: 1. Open stash.").is_some());

    // In another thread, with words already written: the entry follows them and brings its chip and file.
    open_thread(&mut dom, &props, state, "thread-b").await;
    let there = target(state, "thread-b");
    type_prompt(&mut dom, state, "already here");
    click_control(&mut dom, "Stashed prompts: 1. Open stash.");
    click_control(&mut dom, "Restore stashed prompt: compare these");
    until(&mut dom, "the restore", |_| stashed(state).is_empty() && uploads(state, &there) == vec![Upload::Ready]).await;
    let draft = state.peek().draft.clone();
    assert_eq!(draft, "already here\n\ncompare these");
    assert_eq!(crate::composer_chips::contexts_of(state, &there).len(), 1);
    assert!(control(&dom, "Stashed prompts: 1. Open stash.").is_none(), "an empty stash draws nothing");
    // Taken out of the stash for good, in storage too.
    assert!(!MEMORY.with(|memory| memory.borrow().clone()).unwrap().contains(&entry.id));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_stash_survives_a_reload_and_a_refused_write_leaves_the_composer_alone() {
    MEMORY.with(|memory| *memory.borrow_mut() = None);
    REJECT.with(|reject| reject.set(false));
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    // Storage refuses: nothing is stashed, nothing is cleared, and the reader is told.
    REJECT.with(|reject| reject.set(true));
    type_prompt(&mut dom, state, "keep me");
    dom.in_scope(ScopeId::APP, || shortcut(&props.transport, state));
    until(&mut dom, "the refusal", |dom| rendered_text(dom).contains("Could not stash this prompt")).await;
    assert_eq!((state.peek().draft.as_str(), stashed(state).len(), state.peek().stash.busy), ("keep me", 0, false));
    // Accepted, then "reloaded": a new reading of storage finds it.
    REJECT.with(|reject| reject.set(false));
    dom.in_scope(ScopeId::APP, || shortcut(&props.transport, state));
    until(&mut dom, "the stash", |_| stashed(state).len() == 1).await;
    assert_eq!(state.peek().draft, "");
    state.stash().write().entries.clear();
    dom.in_scope(ScopeId::APP, || spawn(crate::prompt_stash::hydrate(state)));
    until(&mut dom, "the reading", |_| stashed(state).len() == 1).await;
    assert_eq!(stashed(state)[0].prompt, "keep me");
    // On an empty composer the shortcut brings the only entry back at once.
    dom.in_scope(ScopeId::APP, || shortcut(&props.transport, state));
    until(&mut dom, "the restore", |_| stashed(state).is_empty()).await;
    assert_eq!(state.peek().draft, "keep me");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn several_entries_open_a_list_that_restores_deletes_and_closes_with_the_keyboard() {
    MEMORY.with(|memory| *memory.borrow_mut() = None);
    REJECT.with(|reject| reject.set(false));
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    for (n, text) in ["first", "second"].into_iter().enumerate() {
        type_prompt(&mut dom, state, text);
        dom.in_scope(ScopeId::APP, || shortcut(&props.transport, state));
        until(&mut dom, "a stash", |_| stashed(state).len() == n + 1 && !state.peek().stash.busy).await;
    }
    // Newest first. The shortcut on an empty composer opens the list instead.
    assert_eq!(stashed(state).iter().map(|entry| entry.prompt.as_str()).collect::<Vec<_>>(), ["second", "first"]);
    dom.in_scope(ScopeId::APP, || shortcut(&props.transport, state));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(control(&dom, "Close stash").is_some() && control(&dom, "Restore stashed prompt: second").is_some());
    // Escape closes it.
    key_on(&mut dom, "Stash", Key::Escape);
    assert!(control(&dom, "Close stash").is_none());
    dom.in_scope(ScopeId::APP, || shortcut(&props.transport, state));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    // Down arrow moves to the older one, Enter restores it; Delete takes the other away.
    key_on(&mut dom, "Stash", Key::ArrowDown);
    key_on(&mut dom, "Stash", Key::Enter);
    until(&mut dom, "the restore", |_| stashed(state).len() == 1).await;
    assert_eq!(state.peek().draft, "first");
    click_control(&mut dom, "Stashed prompts: 1. Open stash.");
    click_control(&mut dom, "Delete stashed prompt");
    assert!(stashed(state).is_empty());
    assert!(control(&dom, "Close stash").is_none(), "the list closes with its last entry");
    fixture.finish(dom).await;
}

async fn wait(dom: &mut VirtualDom, label: &str) {
    until(dom, label, |dom| control(dom, label).is_some()).await;
}
