//! "Edit from here" through the mounted UI against the real native server, with real turns and
//! real checkpoints in a temporary git repository: the offer, the confirmation, restoring files
//! in an isolated worktree, rewinding the conversation only, the server's refusal and failure,
//! and the permission gate. Waits are on state or text that only the awaited step produces.
use super::diff_tests::{Fixture, Harness, assert_repaints_explained_by_events, error, git, open_thread, rendered_text, repaints, seed, turn, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, click_nth_control, control, input_control};
use std::path::PathBuf;

/// How many user messages offer "Edit from here".
fn offers(dom: &VirtualDom) -> usize {
    rendered_text(dom).matches('\u{21b6}').count()
}

fn draft(state: Store<UiModel>) -> String {
    state.peek().draft.clone()
}

fn projection(state: Store<UiModel>) -> serde_json::Value {
    state.peek().thread.projection.clone().unwrap()
}

fn run_statuses(state: Store<UiModel>) -> Vec<String> {
    projection(state)["runs"].as_array().unwrap().iter().map(|run| run["status"].as_str().unwrap().to_owned()).collect()
}

/// A thread of its own worktree of the project, the only place file restore is allowed.
async fn worktree_thread(fixture: &Fixture, dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>) -> PathBuf {
    let path = fixture.workspace.parent().unwrap().join("worktree");
    git(&fixture.workspace, &["worktree", "add", "-q", "-b", "feature", path.to_str().unwrap()]);
    let path = std::fs::canonicalize(path).unwrap();
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.create", json!({"createdBy":"user","creationSource":"web","threadId":"thread-w","projectId":"project","title":"thread-w","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","branch":"feature","worktreePath":path}))).unwrap();
    until(dom, "worktree thread", |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.iter().any(|t| t.id.as_str() == "thread-w"))).await;
    path
}

fn write(directory: &std::path::Path, name: &str, text: &str) {
    std::fs::write(directory.join(name), text).unwrap();
}

/// Three turns in the worktree thread: turn 2 adds a.txt, turn 3 adds b.txt.
async fn three_worktree_turns(fixture: &Fixture, dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>) -> PathBuf {
    let worktree = worktree_thread(fixture, dom, props, state).await;
    open_thread(dom, props, state, "thread-w").await;
    turn(dom, props, state, 1).await;
    write(&worktree, "a.txt", "one\n");
    turn(dom, props, state, 2).await;
    write(&worktree, "b.txt", "two\n");
    turn(dom, props, state, 3).await;
    until(dom, "an offer on each message", |dom| offers(dom) == 3).await;
    worktree
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn confirming_with_files_rewinds_an_isolated_worktree_and_returns_the_message_to_the_composer() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    let worktree = three_worktree_turns(&fixture, &mut dom, &props, state).await;
    // The diff of the newest turn is open when its turn is rewound.
    super::diff_tests::open_latest(&mut dom);
    until(&mut dom, "turn 3 diff", |dom| control(dom, "Diff scope").is_some_and(|(_, value)| value.as_deref() == Some("latest")) && control(dom, "Expand b.txt").is_some()).await;

    // Asking opens the original's question; Cancel leaves everything as it was.
    click_nth_control(&mut dom, "Edit from here", 2);
    let text = rendered_text(&dom);
    assert!(text.contains("Edit from here?") && text.contains("Rewind chat to before this message. Your prompt and attachments return to the composer."), "{text}");
    assert!(control(&dom, "Revert files too").is_some() && control(&dom, "Revert and keep changes").is_some());
    click_control(&mut dom, "Cancel");
    assert!(!rendered_text(&dom).contains("Edit from here?"));
    assert!(state.peek().rewind.active.is_none() && run_statuses(state).iter().all(|status| status == "completed"));
    assert!(worktree.join("b.txt").exists());

    click_nth_control(&mut dom, "Edit from here", 2);
    click_control(&mut dom, "Revert files too");
    // In progress at once, before the server has said anything, and the composer cannot send.
    assert!(rendered_text(&dom).contains("Rewinding the conversation and restoring files"), "{}", rendered_text(&dom));
    assert!(!rendered_text(&dom).contains("Edit from here?"));
    until(&mut dom, "the message back in the composer", |_| draft(state) == "Do the work 3").await;

    // Done: the status line is gone, the files and the conversation are back at turn 2.
    let text = rendered_text(&dom);
    assert!(!text.contains("Rewinding the conversation"), "{text}");
    assert!(text.contains("Do the work 2") && !text.contains("Do the work 3"), "{text}");
    assert_eq!(run_statuses(state), ["completed", "completed", "rolled_back"]);
    assert!(worktree.join("a.txt").exists() && !worktree.join("b.txt").exists());
    assert_eq!(offers(&dom), 2, "the rewound message offers nothing");
    // The diff no longer names the turn that was rewound and shows the one before it.
    until(&mut dom, "diff back on turn 2", |dom| control(dom, "Diff scope").is_some_and(|(_, value)| value.as_deref() == Some("latest")) && control(dom, "Expand a.txt").is_some()).await;
    let text = rendered_text(&dom);
    assert!(text.contains("Turn 1") && !text.contains("Turn 2") && !text.contains("Turn 3"), "{text}");
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn keeping_changes_rewinds_only_the_conversation_and_appends_to_what_was_typed() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    let worktree = three_worktree_turns(&fixture, &mut dom, &props, state).await;
    input_control(&mut dom, "Message", "keep me");
    click_nth_control(&mut dom, "Edit from here", 1);
    click_control(&mut dom, "Revert and keep changes");
    assert!(rendered_text(&dom).contains("Rewinding the conversation\u{2026}") && !rendered_text(&dom).contains("restoring files"));
    until(&mut dom, "the message after what was typed", |_| draft(state) == "keep me\n\nDo the work 2").await;
    assert_eq!(run_statuses(state), ["completed", "rolled_back", "rolled_back"]);
    // Both later runs are gone from the conversation, but the files are as the agent left them.
    assert!(worktree.join("a.txt").exists() && worktree.join("b.txt").exists());
    let text = rendered_text(&dom);
    assert!(text.contains("Do the work 1") && !text.contains("Do the work 2") && !text.contains("Do the work 3"), "{text}");
    assert_eq!(offers(&dom), 1);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_project_checkout_refuses_file_restore_with_its_reason_but_can_rewind_the_conversation() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    fixture.write("a.txt", "one\n");
    turn(&mut dom, &props, state, 2).await;
    until(&mut dom, "an offer on each message", |dom| offers(dom) == 2).await;

    click_nth_control(&mut dom, "Edit from here", 0);
    click_control(&mut dom, "Revert files too");
    wait_for_rendered_text(&mut dom, "File restore requires an isolated worktree.").await;
    let text = rendered_text(&dom);
    assert!(!text.contains("Invalid command") && !text.contains("Rewinding the conversation"), "{text}");
    assert!(state.peek().rewind.active.is_none(), "a refused rewind is not in progress");
    assert_eq!(run_statuses(state), ["completed", "completed"]);
    assert!(fixture.workspace.join("a.txt").exists());
    assert!(projection(state)["thread"]["rollbackRequestId"].is_null());
    assert_eq!(error(state), None, "the reason is shown beside the dialog, not in the app banner");
    click_control(&mut dom, "Dismiss rewind error");
    assert!(!rendered_text(&dom).contains("File restore requires"));

    // The same thread can still go back without touching its files.
    click_nth_control(&mut dom, "Edit from here", 0);
    click_control(&mut dom, "Revert and keep changes");
    until(&mut dom, "the first message back", |_| draft(state) == "Do the work 1").await;
    assert_eq!(run_statuses(state), ["rolled_back", "rolled_back"]);
    assert!(fixture.workspace.join("a.txt").exists());
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_rewind_the_server_gives_up_on_shows_its_reason_and_a_new_rewind_clears_it() {
    // The Codex fixture cannot revert legacy history, so every attempt at a rewind fails.
    let fixture = Fixture::start_with(true, &[("FIXTURE_LEGACY_HISTORY", "1")]).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    let worktree = worktree_thread(&fixture, &mut dom, &props, state).await;
    open_thread(&mut dom, &props, state, "thread-w").await;
    turn(&mut dom, &props, state, 1).await;
    write(&worktree, "b.txt", "two\n");
    turn(&mut dom, &props, state, 2).await;
    until(&mut dom, "an offer on each message", |dom| offers(dom) == 2).await;

    click_nth_control(&mut dom, "Edit from here", 1);
    click_control(&mut dom, "Revert and keep changes");
    assert!(rendered_text(&dom).contains("Rewinding the conversation\u{2026}"));
    let reason = "The provider could not roll back this conversation.";
    wait_for_rendered_text(&mut dom, reason).await;
    let text = rendered_text(&dom);
    assert!(!text.contains("Rewinding the conversation"), "the wait ended with the failure: {text}");
    assert!(state.peek().rewind.active.is_none());
    // Nothing changed: no run is gone and the composer is as it was.
    assert_eq!(run_statuses(state), ["completed", "completed"]);
    assert_eq!(draft(state), "");
    assert!(worktree.join("b.txt").exists());
    let first_request = projection(state)["thread"]["rollbackRequestId"].as_str().unwrap().to_owned();

    // Dismissing hides it; the thread keeps the record.
    click_control(&mut dom, "Dismiss rewind error");
    assert!(!rendered_text(&dom).contains(reason));

    // A new rewind replaces the failure the moment the server accepts it, and fails on its own.
    click_nth_control(&mut dom, "Edit from here", 1);
    click_control(&mut dom, "Revert and keep changes");
    until(&mut dom, "the second rewind accepted", |_| {
        let thread = &projection(state)["thread"];
        thread["rollbackRequestId"].as_str().is_some_and(|id| id != first_request) && thread["rollbackFailure"].is_null()
    })
    .await;
    assert!(!rendered_text(&dom).contains(reason), "an old failure does not show beside a new request");
    wait_for_rendered_text(&mut dom, reason).await;
    let thread = projection(state)["thread"].clone();
    assert_eq!(thread["rollbackFailure"]["requestId"], thread["rollbackRequestId"]);
    assert_ne!(thread["rollbackRequestId"].as_str().unwrap(), first_request);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn only_ready_checkpoints_on_a_connection_that_can_operate_offer_a_rewind() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    turn(&mut dom, &props, state, 2).await;
    until(&mut dom, "an offer on each message", |dom| offers(dom) == 2).await;

    // A checkpoint that is no longer ready cannot be restored, so its message offers nothing.
    state.thread().write().projection.as_mut().unwrap()["checkpoints"].as_array_mut().unwrap().iter_mut().find(|checkpoint| checkpoint["appRunOrdinal"] == 2).unwrap()["status"] = json!("error");
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(offers(&dom), 1);
    state.thread().write().projection.as_mut().unwrap()["checkpoints"].as_array_mut().unwrap().iter_mut().find(|checkpoint| checkpoint["appRunOrdinal"] == 1).unwrap()["status"] = json!("stale");
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(offers(&dom), 0);

    // A connection that can only read sees the conversation and nothing to rewind it with.
    let (mut reader, reader_props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead]);
    let reader_state = reader_props.state.borrow().unwrap();
    until(&mut reader, "reader shell", |_| reader_state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.len() == 2)).await;
    open_thread(&mut reader, &reader_props, reader_state, "thread-a").await;
    wait_for_rendered_text(&mut reader, "Do the work 2").await;
    assert_eq!(offers(&reader), 0);
    drop(reader);
    fixture.finish(dom).await;
}

#[derive(Clone)]
struct ProbeHarness {
    inner: Harness,
    probe: crate::RenderProbe,
}
fn probe_harness(props: ProbeHarness) -> Element {
    use_context_provider(|| props.probe.clone());
    super::diff_tests::harness(props.inner.clone())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn asking_and_cancelling_never_repaint_the_conversation_the_sidebar_or_the_composer() {
    let fixture = Fixture::start(true).await;
    let (_unused, inner) = fixture.operator(Gate::default());
    let props = ProbeHarness { inner, probe: crate::RenderProbe::default() };
    let mut dom = VirtualDom::new_with_props(probe_harness, props.clone());
    dom.rebuild_in_place();
    let state = seed(&mut dom, &props.inner, &fixture.workspace).await;
    open_thread(&mut dom, &props.inner, state, "thread-a").await;
    turn(&mut dom, &props.inner, state, 1).await;
    turn(&mut dom, &props.inner, state, 2).await;
    until(&mut dom, "an offer on each message", |dom| offers(dom) == 2).await;
    // The server's last replies and events of the turns land first.
    until(&mut dom, "the turns delivered", |_| state.peek().pending_messages.is_empty()).await;
    click_nth_control(&mut dom, "Edit from here", 1);
    click_control(&mut dom, "Cancel");
    let before = repaints(state, &props.probe);
    for _ in 0..3 {
        click_nth_control(&mut dom, "Edit from here", 0);
        assert!(rendered_text(&dom).contains("Edit from here?"));
        click_control(&mut dom, "Cancel");
    }
    assert_repaints_explained_by_events(before, repaints(state, &props.probe), "asking and cancelling");
    fixture.finish(dom).await;
}
