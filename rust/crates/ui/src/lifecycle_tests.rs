//! Ending threads and projects, and starting them somewhere other than the project checkout,
//! through the mounted UI and the real native server with temporary git repositories: the delete
//! and archive questions in the original's words, undo, unarchive, removing a project that has
//! threads, the workspace choice (a worktree that already exists, the previous one, and what is
//! remembered), and a failed worktree preparation with its progress card and retry. Waits are on
//! text or state that only the awaited step produces.
use super::diff_tests::{Fixture, Harness, error, git, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::keys_tests::{PALETTE, chord, key_on, open_palette, press};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{all_controls, change_control, click_control, click_nth_control, control, control_attribute, input_control};
use t3_client::keybindings::Dom;

const OPERATOR: [AuthEnvironmentScope; 2] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];
const READER: [AuthEnvironmentScope; 1] = [AuthEnvironmentScope::OrchestrationRead];

/// Runs a body on a seeded application and shuts the fixture down whatever the body does, so a
/// failing assertion reports instead of hanging at runtime drop.
async fn with_app(git_repository: bool, scopes: &[AuthEnvironmentScope], body: impl AsyncFnOnce(&Fixture, &mut VirtualDom, &Harness, Store<UiModel>)) {
    let fixture = Fixture::start(git_repository).await;
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = if scopes.contains(&AuthEnvironmentScope::OrchestrationOperate) {
        seed(&mut dom, &props, &fixture.workspace).await
    } else {
        // A reader cannot create the project; an operator's connection makes it first.
        let (mut maker, maker_props) = fixture.client(Gate::default(), &OPERATOR);
        seed(&mut maker, &maker_props, &fixture.workspace).await;
        drop(maker);
        let state = props.state.borrow().unwrap();
        until(&mut dom, "the shell", |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.len() == 2)).await;
        state
    };
    body(&fixture, &mut dom, &props, state).await;
    fixture.finish(dom).await;
}

fn thread_state(state: Store<UiModel>, id: &str) -> Option<t3_contracts::ThreadShell> {
    state.peek().shell.snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == id).cloned())
}
fn live(state: Store<UiModel>, id: &str) -> bool {
    thread_state(state, id).is_some_and(|thread| thread.deleted_at.is_none())
}
/// An archived thread leaves the active shell; the archived shell keeps it.
fn archived(state: Store<UiModel>, id: &str) -> bool {
    thread_state(state, id).is_none()
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn deleting_a_thread_asks_in_the_originals_words_declines_with_escape_and_removes_it_once_confirmed() {
    with_app(true, &OPERATOR, async |_fixture, dom, props, state| {
        open_thread(dom, props, state, "thread-a").await;
        click_control(dom, "Delete thread");
        wait_for_rendered_text(dom, "Delete thread \"thread-a\"?").await;
        assert!(rendered_text(dom).contains("This permanently clears conversation history for this thread."));
        assert!(live(state, "thread-a"), "asking deletes nothing");
        key_on(dom, "Cancel", Key::Escape);
        assert!(control(dom, "Confirm").is_none() && live(state, "thread-a"), "Escape declines");
        click_control(dom, "Delete thread");
        click_control(dom, "Confirm");
        until(dom, "the thread to go", |_| !live(state, "thread-a")).await;
        assert!(live(state, "thread-b"));
        until(dom, "the thread to be left", |_| state.peek().active_thread.is_none()).await;
        assert_eq!(error(state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn archiving_can_be_taken_back_from_the_notice_or_the_archived_list() {
    with_app(true, &OPERATOR, async |_fixture, dom, props, state| {
        open_thread(dom, props, state, "thread-a").await;
        click_control(dom, "Archive thread");
        until(dom, "the archive", |_| archived(state, "thread-a")).await;
        wait_for_rendered_text(dom, "Archived 1 thread,").await;
        click_control(dom, "Undo archive");
        until(dom, "the archive taken back", |_| !archived(state, "thread-a")).await;
        until(dom, "the thread back in view", |_| state.peek().active_thread.as_deref() == Some("thread-a")).await;
        assert!(!rendered_text(dom).contains("Archived 1 thread,"), "an undo is used once");
        // Archived threads live in their own list, from which they come back.
        click_control(dom, "Archive thread");
        until(dom, "the second archive", |_| archived(state, "thread-a")).await;
        click_control(dom, "Show archived threads");
        until(dom, "the archived list", |dom| control(dom, "Unarchive thread-a").is_some()).await;
        click_control(dom, "Unarchive thread-a");
        until(dom, "unarchived", |_| !archived(state, "thread-a")).await;
        // With the reader's confirmation setting on, archiving asks first.
        state.client_settings().write().confirm_thread_archive = true;
        open_thread(dom, props, state, "thread-b").await;
        click_control(dom, "Archive thread");
        wait_for_rendered_text(dom, "Archive thread \"thread-b\"?").await;
        assert!(!archived(state, "thread-b"));
        click_control(dom, "Confirm");
        until(dom, "the confirmed archive", |_| archived(state, "thread-b")).await;
        assert_eq!(error(state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn removing_a_project_with_threads_is_refused_first_then_asked_in_full_and_removes_it_with_its_threads() {
    with_app(true, &OPERATOR, async |fixture, dom, _props, state| {
        click_control(dom, "Remove project Project");
        wait_for_rendered_text(dom, "Project is not empty").await;
        assert!(rendered_text(dom).contains("Delete all threads in this project before removing it."));
        key_on(dom, "Cancel", Key::Escape);
        assert!(control(dom, "Delete anyway").is_none());
        click_control(dom, "Remove project Project");
        click_control(dom, "Delete anyway");
        wait_for_rendered_text(dom, "Remove project \"Project\" and delete its 2 threads?").await;
        let text = rendered_text(dom);
        for line in [
            format!("Path: {}", fixture.workspace.display()),
            "This permanently clears conversation history for those threads and any archived threads.".to_owned(),
            "This removes only this project entry.".to_owned(),
            "This action cannot be undone.".to_owned(),
        ] {
            assert!(text.contains(&line), "{line}: {text}");
        }
        assert_eq!(state.peek().shell.snapshot.as_ref().unwrap().projects.len(), 1, "asking removes nothing");
        key_on(dom, "Cancel", Key::Escape);
        assert_eq!(state.peek().shell.snapshot.as_ref().unwrap().projects.len(), 1);
        click_control(dom, "Remove project Project");
        click_control(dom, "Delete anyway");
        click_control(dom, "Confirm");
        until(dom, "the project to go", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.projects.is_empty())).await;
        assert!(!live(state, "thread-a") && !live(state, "thread-b"));
        assert!(fixture.workspace.join("README.md").exists(), "the files on disk are not touched");
        assert_eq!(error(state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_may_not_operate_cannot_end_anything() {
    with_app(true, &READER, async |_fixture, dom, props, state| {
        open_thread(dom, props, state, "thread-a").await;
        for label in ["Archive thread", "Delete thread", "Remove project Project"] {
            assert!(disabled(dom, label), "{label}");
        }
        assert!(live(state, "thread-a"));
    })
    .await;
}

fn worktree_of(state: Store<UiModel>, path: &std::path::Path) -> Option<t3_contracts::ThreadShell> {
    let wanted = std::fs::canonicalize(path).unwrap();
    state.peek().shell.snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.worktree_path.as_ref().is_some_and(|found| std::fs::canonicalize(found.as_str()).ok().as_deref() == Some(wanted.as_path()))).cloned())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_thread_starts_in_an_existing_worktree_the_choice_is_remembered_and_the_previous_one_is_one_key_away() {
    with_app(true, &OPERATOR, async |fixture, dom, props, state| {
        git(&fixture.workspace, &["branch", "-m", "main"]);
        let worktree = fixture.workspace.parent().unwrap().join("existing-wt");
        git(&fixture.workspace, &["worktree", "add", "-q", "-b", "wt-branch", worktree.to_str().unwrap()]);
        let key = (state.peek().destination.clone().unwrap(), "project".to_owned());

        // A new worktree is the other choice; browsing to a branch that lives in a worktree
        // points the form at that worktree.
        change_control(dom, "Workspace", "worktree");
        until(dom, "the base branch picker", |dom| control(dom, "Choose base branch").is_some()).await;
        click_control(dom, "Choose base branch");
        until(dom, "the refs", |dom| control(dom, "wt-branch").is_some()).await;
        click_control(dom, "wt-branch");
        wait_for_rendered_text(dom, "Starts in the existing worktree").await;
        let remembered = state.peek().new_thread_choices.get(&key).cloned().unwrap();
        assert_eq!((remembered.environment_mode, remembered.base_ref.as_str()), (Some(t3_contracts::ThreadEnvMode::Worktree), "wt-branch"));
        assert_eq!(remembered.worktree_path.as_deref().map(|path| std::fs::canonicalize(path).unwrap()), Some(std::fs::canonicalize(&worktree).unwrap()));
        // Back to the project checkout and again to the worktree: the choices follow the select.
        change_control(dom, "Workspace", "local");
        assert!(state.peek().new_thread_choices.get(&key).unwrap().worktree_path.is_none());
        assert!(!rendered_text(dom).contains("Starts in the existing worktree"));
        change_control(dom, "Workspace", "worktree");
        click_control(dom, "Choose base branch");
        until(dom, "the refs", |dom| control(dom, "wt-branch").is_some()).await;
        click_control(dom, "wt-branch");
        wait_for_rendered_text(dom, "Starts in the existing worktree").await;

        // Launching starts the thread in that checkout, not in a new one.
        input_control(dom, "First message", "hello from the worktree");
        click_control(dom, "Create new thread");
        until(dom, "the thread in the worktree", |_| worktree_of(state, &worktree).is_some()).await;
        assert!(!fixture.directory.path().join("worktrees").join("workspace").exists(), "no new worktree was made");

        // The next form offers the thread's worktree as the previous one, and the shortcut takes it.
        dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, None));
        wait_for_rendered_text(dom, "Previous worktree (wt-branch)").await;
        assert_eq!(press(dom, chord("l", true, false), Dom::default()).await.as_deref(), Some("composer.previousWorktree"));
        wait_for_rendered_text(dom, "Starts in the existing worktree").await;
        assert_eq!(press(dom, chord("x", true, false), Dom::default()).await.as_deref(), Some("composer.workspace"));
        assert_eq!(error(state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_failed_worktree_preparation_shows_its_card_and_a_retry_that_runs_the_same_run_again() {
    with_app(true, &OPERATOR, async |fixture, dom, _props, state| {
        git(&fixture.workspace, &["branch", "-m", "main"]);
        change_control(dom, "Workspace", "worktree");
        input_control(dom, "Base branch", "later-branch");
        input_control(dom, "First message", "go");
        click_control(dom, "Create new thread");
        // The base does not exist yet: the preparation fails, the card says so, and the error
        // item offers the retry.
        wait_for_rendered_text(dom, "Worktree setup failed").await;
        until(dom, "the retry", |dom| control(dom, "Retry workspace preparation").is_some()).await;
        assert!(control(dom, "Cancel worktree setup").is_none(), "nothing is running to cancel");
        // The base appears; the retry runs the same run again, once.
        git(&fixture.workspace, &["branch", "later-branch"]);
        let run = state.peek().thread.projection.as_ref().unwrap()["runs"][0]["id"].clone();
        click_control(dom, "Retry workspace preparation");
        click_control(dom, "Retry workspace preparation");
        until(dom, "the run to complete", |_| state.peek().thread.projection.as_ref().is_some_and(|projection| projection["runs"][0]["status"] == "completed")).await;
        assert_eq!(state.peek().thread.projection.as_ref().unwrap()["runs"][0]["id"], run);
        until(dom, "no retry left", |dom| control(dom, "Retry workspace preparation").is_none()).await;
        assert_eq!(error(state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_palette_archives_asks_before_deleting_and_the_undo_shortcut_follows_what_can_be_undone() {
    with_app(true, &OPERATOR, async |_fixture, dom, props, state| {
        open_thread(dom, props, state, "thread-a").await;
        assert_eq!(press(dom, chord("z", false, false), Dom::default()).await, None, "nothing to undo yet");
        // The header has a button of the same name; the palette's row is the second.
        open_palette(dom).await;
        input_control(dom, PALETTE, ">archive");
        until(dom, "the entry", |dom| all_controls(dom, "Archive thread").len() >= 2).await;
        click_nth_control(dom, "Archive thread", 1);
        until(dom, "the archive", |_| archived(state, "thread-a")).await;
        assert_eq!(press(dom, chord("z", false, false), Dom::default()).await.as_deref(), Some("thread.undo"));
        until(dom, "the archive taken back", |_| !archived(state, "thread-a")).await;
        assert_eq!(press(dom, chord("z", false, false), Dom::default()).await, None, "an undo is used once");
        // Deleting from the palette asks first, in the same words.
        open_palette(dom).await;
        input_control(dom, PALETTE, ">delete thread");
        until(dom, "the entry", |dom| all_controls(dom, "Delete thread").len() >= 2).await;
        click_nth_control(dom, "Delete thread", 1);
        wait_for_rendered_text(dom, "Delete thread \"thread-a\"?").await;
        assert!(live(state, "thread-a"));
        assert_eq!(error(state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn cancelling_a_running_worktree_setup_stops_it_once_and_rolls_the_thread_back() {
    let fixture = Fixture::start_with_terminals(true).await;
    let gate = fixture.directory.path().join("setup-release");
    let (mut dom, props) = fixture.client(Gate::default(), &OPERATOR);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    git(&fixture.workspace, &["branch", "-m", "main"]);
    // A setup script that runs until the test lets it go, in the terminal the server gives it.
    let script = json!([{"id":"setup","name":"Install","command":format!("echo started; while [ ! -f '{}' ]; do sleep 0.05; done", gate.display()),"icon":"configure","runOnWorktreeCreate":true,"async":false}]);
    dom.in_scope(ScopeId::APP, || request(&props.transport, state, "projects.mutate", json!({"type":"project.update","commandId":"scripts","projectId":"project","scripts":script}), RequestKind::Unary)).unwrap();
    until(&mut dom, "the script", |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| s.projects.iter().any(|p| !p.scripts.is_empty()))).await;
    let _release = Release(gate.clone());
    dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, None));
    change_control(&mut dom, "Workspace", "worktree");
    input_control(&mut dom, "Base branch", "main");
    input_control(&mut dom, "First message", "go");
    click_control(&mut dom, "Create new thread");
    // The card names the script and shows what it printed: it is running.
    wait_for_rendered_text(&mut dom, "started").await;
    until(&mut dom, "the cancel control", |dom| control(dom, "Cancel worktree setup").is_some()).await;
    click_control(&mut dom, "Cancel worktree setup");
    click_control(&mut dom, "Cancel worktree setup");
    // The server unwinds the setup: the run fails with the reason, and the worktree is gone.
    until(&mut dom, "the run to fail", |_| state.peek().thread.projection.as_ref().is_some_and(|projection| projection["runs"][0]["status"] == "failed")).await;
    wait_for_rendered_text(&mut dom, "Worktree setup cancelled.").await;
    let projection = state.peek().thread.projection.clone().unwrap();
    assert_eq!(projection["thread"]["worktreePath"], Value::Null);
    assert!(control(&dom, "Cancel worktree setup").is_none(), "nothing is left to cancel");
    assert!(git(&fixture.workspace, &["worktree", "list", "--porcelain"]).matches("worktree ").count() == 1, "only the project's own checkout is left");
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}
/// Lets a gated script finish whatever the test does.
struct Release(std::path::PathBuf);
impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "go");
    }
}
