//! The branch and working-tree status control through the mounted UI against the real native
//! server and temporary git repositories: the label and the dirty mark, a branch changed outside
//! the app, the ref picker, switching and creating, a switch the working tree blocks, pull, a
//! folder that is not a repository, a connection that cannot write, the stream's lifetime and
//! what a status update repaints. Waits are on state or text that only the awaited step produces.
use super::diff_tests::{Fixture, Harness, assert_repaints_explained_by_events, error, git, open_thread, rendered_text, repaints, seed, turn, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{change_control, click_control, control, input_control};
use std::path::Path;


fn writer(fixture: &Fixture) -> (VirtualDom, Harness) {
    fixture.client(
        Gate::default(),
        &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SourceControlWrite],
    )
}

/// A repository on `main` with the thread open, the control connected to its workspace.
async fn open(fixture: &Fixture) -> (VirtualDom, Harness, Store<UiModel>) {
    git(&fixture.workspace, &["branch", "-m", "main"]);
    let (mut dom, props) = writer(fixture);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    (dom, props, state)
}

fn branch(directory: &Path) -> String {
    git(directory, &["branch", "--show-current"]).trim().to_owned()
}

fn recorded_branch(state: Store<UiModel>) -> Option<String> {
    state.peek().shell.snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == "thread-a").and_then(|thread| thread.branch.as_ref().map(ToString::to_string)))
}

async fn open_picker(dom: &mut VirtualDom) {
    click_control(dom, "Switch branch");
    until(dom, "the picker's refs", |dom| control(dom, "main").is_some()).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_label_shows_the_branch_and_dirty_state_and_follows_a_change_made_outside_the_app() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props, state) = open(&fixture).await;
    wait_for_rendered_text(&mut dom, "\u{2442} main").await;
    assert!(!rendered_text(&dom).contains('\u{25cf}'), "a clean tree has no mark");

    // The agent's turn changes the tree; the control looks again when the turn ends.
    fixture.write("notes.txt", "draft\n");
    turn(&mut dom, &props, state, 1).await;
    wait_for_rendered_text(&mut dom, "\u{25cf} 1").await;

    // A branch changed with git, not through the app, is not known until the control asks again.
    git(&fixture.workspace, &["checkout", "-q", "-b", "outside"]);
    assert!(rendered_text(&dom).contains("\u{2442} main") && !rendered_text(&dom).contains("\u{2442} outside"));
    click_control(&mut dom, "Switch branch");
    wait_for_rendered_text(&mut dom, "\u{2442} outside").await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_picker_lists_refs_and_narrows_them_as_the_search_is_typed() {
    let fixture = Fixture::start(true).await;
    for name in ["feature/a", "feature/b", "other", "new-one"] {
        git(&fixture.workspace, &["branch", name]);
    }
    let (mut dom, _props, _state) = open(&fixture).await;
    wait_for_rendered_text(&mut dom, "\u{2442} main").await;
    open_picker(&mut dom).await;
    for name in ["main", "feature/a", "feature/b", "other", "new-one"] {
        assert!(control(&dom, name).is_some(), "{name} is listed");
    }
    assert!(rendered_text(&dom).contains("current"), "the checked-out ref is tagged");

    input_control(&mut dom, "Search refs", "feat");
    // The list narrows once typing pauses; "other" can only vanish because of the search.
    until(&mut dom, "the narrowed list", |dom| control(dom, "other").is_none()).await;
    assert!(control(&dom, "feature/a").is_some() && control(&dom, "feature/b").is_some() && control(&dom, "main").is_none());
    assert!(control(&dom, "Create new ref \"feat\"").is_some(), "what is typed can become a ref");
    // Nothing matches: only the offer to create it is left.
    input_control(&mut dom, "Search refs", "zzz");
    until(&mut dom, "no matches", |dom| control(dom, "feature/a").is_none() && control(dom, "Create new ref \"zzz\"").is_some()).await;
    // A name with a space finds the ref it would be created as, and offers no duplicate of it.
    input_control(&mut dom, "Search refs", "new one");
    until(&mut dom, "the sanitized match", |dom| control(dom, "new-one").is_some() && control(dom, "other").is_none()).await;
    assert!(control(&dom, "Create new ref \"new-one\"").is_none());
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn switching_and_creating_change_the_workspace_and_the_threads_recorded_branch() {
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["branch", "feature/a"]);
    let (mut dom, _props, state) = open(&fixture).await;
    wait_for_rendered_text(&mut dom, "\u{2442} main").await;
    open_picker(&mut dom).await;
    click_control(&mut dom, "feature/a");
    assert!(rendered_text(&dom).contains("Switching to feature/a"), "the action says what it is doing");
    wait_for_rendered_text(&mut dom, "\u{2442} feature/a").await;
    assert_eq!(branch(&fixture.workspace), "feature/a");
    until(&mut dom, "the recorded branch", |_| recorded_branch(state).as_deref() == Some("feature/a")).await;
    assert!(!rendered_text(&dom).contains("Switching to"), "the busy text ends with the action");

    open_picker(&mut dom).await;
    input_control(&mut dom, "Search refs", "brand new");
    until(&mut dom, "the create row", |dom| control(dom, "Create new ref \"brand-new\"").is_some()).await;
    click_control(&mut dom, "Create new ref \"brand-new\"");
    wait_for_rendered_text(&mut dom, "\u{2442} brand-new").await;
    assert_eq!(branch(&fixture.workspace), "brand-new");
    until(&mut dom, "the recorded branch", |_| recorded_branch(state).as_deref() == Some("brand-new")).await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_switch_the_working_tree_blocks_shows_the_error_and_keeps_the_old_branch() {
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["checkout", "-q", "-b", "other"]);
    fixture.write("README.md", "other side\n");
    git(&fixture.workspace, &["commit", "-q", "-am", "diverge"]);
    git(&fixture.workspace, &["checkout", "-q", "-"]);
    // An uncommitted edit of the same file stops the checkout.
    fixture.write("README.md", "local edit\n");
    let (mut dom, _props, state) = open(&fixture).await;
    wait_for_rendered_text(&mut dom, "\u{2442} main").await;
    open_picker(&mut dom).await;
    click_control(&mut dom, "other");
    wait_for_rendered_text(&mut dom, "Failed to switch ref.").await;
    let text = rendered_text(&dom);
    assert!(text.contains("Git command failed in GitVcsDriver.switchRef.checkout"), "the server's error is shown: {text}");
    assert!(text.contains("\u{2442} main") && !text.contains("\u{2442} other"), "{text}");
    assert_eq!(branch(&fixture.workspace), "main");
    assert_eq!(error(state), None, "the failure is shown by the control, not the app banner");
    assert!(!text.contains("Switching to"), "the busy text ended with the failure");
    // The message goes away when asked.
    click_control(&mut dom, "Dismiss branch message");
    assert!(!rendered_text(&dom).contains("Failed to switch ref."));
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_branch_behind_its_upstream_offers_pull_and_pulling_catches_it_up() {
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["branch", "-m", "main"]);
    let root = fixture.workspace.parent().unwrap().to_path_buf();
    let origin = root.join("origin.git");
    git(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(&fixture.workspace, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&fixture.workspace, &["push", "-q", "-u", "origin", "main"]);
    let clone = root.join("clone");
    git(&root, &["clone", "-q", "-b", "main", origin.to_str().unwrap(), clone.to_str().unwrap()]);
    std::fs::write(clone.join("pulled.txt"), "from upstream\n").unwrap();
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "upstream work"]);
    git(&clone, &["push", "-q", "origin", "main"]);
    git(&fixture.workspace, &["fetch", "-q"]);

    let (mut dom, _props, _state) = open(&fixture).await;
    wait_for_rendered_text(&mut dom, "\u{2193}1").await;
    assert!(control(&dom, "Pull").is_some());
    click_control(&mut dom, "Pull");
    assert!(rendered_text(&dom).contains("Pulling"));
    wait_for_rendered_text(&mut dom, "Updated main from origin/main").await;
    assert!(rendered_text(&dom).contains("Pulled."));
    assert!(fixture.workspace.join("pulled.txt").exists());
    until(&mut dom, "no longer behind", |dom| !rendered_text(dom).contains("\u{2193}1") && control(dom, "Pull").is_none()).await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_folder_that_is_not_a_repository_says_so_and_can_be_initialized() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = writer(&fixture);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    wait_for_rendered_text(&mut dom, "Not a git repository").await;
    assert!(control(&dom, "Switch branch").is_none());
    click_control(&mut dom, "Initialize Git");
    assert!(rendered_text(&dom).contains("Initializing"));
    until(&mut dom, "the repository", |dom| fixture.workspace.join(".git").exists() && !rendered_text(dom).contains("Not a git repository")).await;
    assert!(control(&dom, "Initialize Git").is_none());
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_cannot_write_sees_the_branch_but_gets_no_way_to_change_it() {
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["branch", "-m", "main"]);
    // Operates threads but has no source control grant.
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    wait_for_rendered_text(&mut dom, "\u{2442} main").await;
    assert!(control(&dom, "Switch branch").is_none() && control(&dom, "Pull").is_none() && control(&dom, "Initialize Git").is_none());
    fixture.finish(dom).await;

    let plain = Fixture::start(false).await;
    let (mut dom, props) = plain.operator(Gate::default());
    let state = seed(&mut dom, &props, &plain.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    wait_for_rendered_text(&mut dom, "Not a git repository").await;
    assert!(control(&dom, "Initialize Git").is_none(), "a reader cannot create a repository");
    plain.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_status_subscription_lasts_exactly_as_long_as_the_thread_is_on_screen() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props, state) = open(&fixture).await;
    wait_for_rendered_text(&mut dom, "\u{2442} main").await;
    let workspace = fixture.workspace.to_string_lossy().into_owned();
    assert_eq!(t3_server::vcs_rpc::status_poller_subscribers(&workspace), Some(1), "one subscription holds the poller");

    // Leaving the thread drops the stream, and the server lets go of its poller.
    dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, None));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    tokio::time::timeout(std::time::Duration::from_secs(10), t3_server::vcs_rpc::wait_for_no_status_poller(&workspace)).await.expect("the poller was never released");
    assert_eq!(t3_server::vcs_rpc::status_poller_subscribers(&workspace), None);

    // Coming back subscribes again.
    open_thread(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "resubscribed", |_| t3_server::vcs_rpc::status_poller_subscribers(&workspace) == Some(1)).await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_dropped_connection_does_not_leave_the_branch_presented_as_current() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props, state) = open(&fixture).await;
    wait_for_rendered_text(&mut dom, "\u{2442} main").await;
    assert!(control(&dom, "Switch branch").is_some() && !rendered_text(&dom).contains("offline"));
    // The socket goes away; the last branch stays visible but is marked, and nothing can be changed.
    dom.in_scope(ScopeId::APP, || {
        state.status().set(ConnectionStatus::Interrupted("test".into()));
    });
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "marked offline", |dom| rendered_text(dom).contains("offline")).await;
    assert!(control(&dom, "Switch branch").is_none() && control(&dom, "Pull").is_none());
    assert!(rendered_text(&dom).contains("\u{2442} main"));
    drop(props);
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
async fn status_updates_the_picker_and_its_search_never_repaint_the_conversation_or_sidebar() {
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["branch", "-m", "main"]);
    git(&fixture.workspace, &["branch", "feature/a"]);
    let (_unused, inner) = writer(&fixture);
    let props = ProbeHarness { inner, probe: crate::RenderProbe::default() };
    let mut dom = VirtualDom::new_with_props(probe_harness, props.clone());
    dom.rebuild_in_place();
    let state = seed(&mut dom, &props.inner, &fixture.workspace).await;
    open_thread(&mut dom, &props.inner, state, "thread-a").await;
    turn(&mut dom, &props.inner, state, 1).await;
    wait_for_rendered_text(&mut dom, "\u{2442} main").await;
    // The turn is over for good: idle, with its checkpoint recorded.
    until(&mut dom, "an idle thread", |_| {
        state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.iter().all(|thread| !thread.status.is_active()))
            && state.peek().thread.projection.as_ref().is_some_and(|p| p["checkpoints"].as_array().is_some_and(|c| c.iter().any(|c| c["appRunOrdinal"] == 1 && c["status"] == "ready")))
    })
    .await;
    until(&mut dom, "the turn delivered", |_| state.peek().pending_messages.is_empty()).await;
    // One round trip through the picker first, so the tail of the turn has been delivered.
    open_picker(&mut dom).await;
    click_control(&mut dom, "Switch branch");
    let before = repaints(state, &props.probe);

    // A dirty tree arrives through the stream when the picker asks the server to look again.
    fixture.write("scratch.txt", "x\n");
    click_control(&mut dom, "Switch branch");
    wait_for_rendered_text(&mut dom, "\u{25cf} 1").await;
    until(&mut dom, "the refs", |dom| control(dom, "main").is_some()).await;
    // Typing searches the server and narrows the list without touching anything else.
    for typed in ["f", "fe", "fea"] {
        input_control(&mut dom, "Search refs", typed);
    }
    until(&mut dom, "the narrowed list", |dom| control(dom, "main").is_none() && control(dom, "feature/a").is_some()).await;
    // A clean tree arrives the same way.
    click_control(&mut dom, "Switch branch");
    assert!(control(&dom, "Search refs").is_none());
    git(&fixture.workspace, &["stash", "-u", "-q"]);
    click_control(&mut dom, "Switch branch");
    until(&mut dom, "the clean tree", |dom| !rendered_text(dom).contains('\u{25cf}')).await;
    assert_repaints_explained_by_events(before, repaints(state, &props.probe), "status updates and the picker");
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_new_thread_form_picks_its_base_branch_from_the_projects_refs() {
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["branch", "-m", "main"]);
    git(&fixture.workspace, &["branch", "feature/a"]);
    let (mut dom, props) = writer(&fixture);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, None));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the form", |dom| control(dom, "Workspace").is_some()).await;
    assert!(control(&dom, "Choose base branch").is_none(), "a local workspace has no base branch");
    change_control(&mut dom, "Workspace", "worktree");
    assert!(control(&dom, "Base branch").is_some());
    click_control(&mut dom, "Choose base branch");
    until(&mut dom, "the project's refs", |dom| control(dom, "feature/a").is_some() && control(dom, "main").is_some()).await;
    // Choosing a ref is all it does: there is no creating a branch here.
    input_control(&mut dom, "Search refs", "zzz");
    until(&mut dom, "no matches", |dom| control(dom, "feature/a").is_none()).await;
    assert!(control(&dom, "Create new ref \"zzz\"").is_none());
    input_control(&mut dom, "Search refs", "feature");
    until(&mut dom, "the match", |dom| control(dom, "feature/a").is_some()).await;
    click_control(&mut dom, "feature/a");
    assert!(control(&dom, "Search refs").is_none(), "choosing closes the picker");
    assert_eq!(control(&dom, "Base branch").unwrap().1.as_deref(), Some("feature/a"));
    assert_eq!(branch(&fixture.workspace), "main", "nothing was switched");
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_long_ref_list_loads_a_page_at_a_time() {
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["branch", "-m", "main"]);
    for number in 0..130 {
        git(&fixture.workspace, &["branch", &format!("topic-{number:03}")]);
    }
    let (mut dom, _props, _state) = open(&fixture).await;
    wait_for_rendered_text(&mut dom, "\u{2442} main").await;
    click_control(&mut dom, "Switch branch");
    // 131 refs in all: the first page of 100 says so, and the rest is one request away.
    wait_for_rendered_text(&mut dom, "Showing 100 of 131 refs").await;
    assert!(control(&dom, "topic-129").is_none() && control(&dom, "Load more refs").is_some());
    click_control(&mut dom, "Load more refs");
    until(&mut dom, "the second page", |dom| control(dom, "topic-129").is_some()).await;
    assert!(control(&dom, "Load more refs").is_none() && !rendered_text(&dom).contains("Showing"), "all refs are loaded");
    // A search starts again from the first page of its own results.
    input_control(&mut dom, "Search refs", "topic-12");
    until(&mut dom, "the narrowed list", |dom| control(dom, "main").is_none() && control(dom, "topic-129").is_some()).await;
    assert!(control(&dom, "topic-100").is_none());
    fixture.finish(dom).await;
}
