//! The "Changes" and "Uncommitted" diff scopes through the mounted UI against the real native
//! server and temporary git repositories: the default scope, the scope menu, the base-ref picker,
//! expansion of unchanged lines, a truncated preview read file by file, refresh, and a connection
//! that may not read files. Waits are on text or controls that only the awaited step produces.
use super::diff_tests::{Fixture, Harness, error, git, open_thread, pump, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{change_control, click_control, control};
use crate::runtime::diff_tests::turn;

fn reviewer(fixture: &Fixture) -> (VirtualDom, Harness) {
    fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead])
}

fn numbered(count: usize, changed: &[usize]) -> String {
    (1..=count).map(|n| if changed.contains(&n) { format!("LINE {n}\n") } else { format!("line {n}\n") }).collect()
}

/// `main` with a 40-line file, `feature/x` one commit on (the file changed at lines 5 and 35, and
/// `b.txt` added), a branch `mid` at that commit, and an uncommitted edit to README.md.
fn repository(fixture: &Fixture) {
    let repo = &fixture.workspace;
    git(repo, &["branch", "-m", "main"]);
    fixture.write("long.txt", &numbered(40, &[]));
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "base"]);
    git(repo, &["checkout", "-q", "-b", "feature/x"]);
    fixture.write("long.txt", &numbered(40, &[5, 35]));
    fixture.write("b.txt", "bee\n");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "work"]);
    git(repo, &["branch", "mid"]);
    fixture.write("README.md", "hello\nuncommitted\n");
}

async fn open_panel(fixture: &Fixture) -> (VirtualDom, Harness, Store<UiModel>) {
    let (mut dom, props) = reviewer(fixture);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    click_control(&mut dom, "Diff");
    (dom, props, state)
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn changes_is_the_default_scope_and_uncommitted_shows_the_working_tree() {
    let fixture = Fixture::start(true).await;
    repository(&fixture);
    let (mut dom, _props, state) = open_panel(&fixture).await;

    // The header button lands on "Changes": the checkout against the branch's base, which counts
    // the uncommitted edit too.
    assert_eq!(control(&dom, "Diff scope").unwrap().1.as_deref(), Some("branch"));
    until(&mut dom, "the branch's files", |dom| control(dom, "Expand long.txt").is_some()).await;
    assert!(control(&dom, "Expand b.txt").is_some() && control(&dom, "Expand README.md").is_some(), "{}", rendered_text(&dom));
    assert!(control(&dom, "Change comparison target. Currently main").is_some());
    assert!(rendered_text(&dom).contains("feature/x"));

    // Uncommitted: only the edit that is not in a commit.
    change_control(&mut dom, "Diff scope", "unstaged");
    until(&mut dom, "the working tree", |dom| control(dom, "Expand long.txt").is_none() && control(dom, "Expand README.md").is_some()).await;
    assert!( control(&dom, "Change comparison target. Currently main").is_none(), "the base picker belongs to Changes");
    click_control(&mut dom, "Expand README.md");
    assert!(rendered_text(&dom).contains("uncommitted"));

    // A refresh reads the checkout again: an edit made outside the app appears.
    fixture.write("README.md", "hello\nuncommitted\nand again\n");
    assert!(!rendered_text(&dom).contains("and again"), "nothing reads on its own without a trigger");
    click_control(&mut dom, "Refresh diff");
    wait_for_rendered_text(&mut dom, "and again").await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

/// The checkout is read again when a lost connection returns: an edit made while it was away appears without
/// asking, where the offline mark would otherwise just be lifted from a page that is out of date.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_working_tree_is_read_again_when_a_lost_connection_returns() {
    let fixture = Fixture::start(true).await;
    repository(&fixture);
    let (mut dom, _props, state) = open_panel(&fixture).await;
    change_control(&mut dom, "Diff scope", "unstaged");
    until(&mut dom, "the working tree", |dom| control(dom, "Expand README.md").is_some()).await;
    click_control(&mut dom, "Expand README.md");
    assert!(rendered_text(&dom).contains("uncommitted") && !rendered_text(&dom).contains("made while away"));
    state.status().set(ConnectionStatus::Interrupted("test".into()));
    wait_for_rendered_text(&mut dom, "Reconnect to load this diff.").await;
    // The page has drawn the loss and its effects have seen it, as they would before the connection returns.
    pump(&mut dom).await;
    fixture.write("README.md", "hello\nuncommitted\nmade while away\n");
    state.status().set(ConnectionStatus::Connected);
    wait_for_rendered_text(&mut dom, "made while away").await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_base_picker_compares_against_another_branch_and_automatic_goes_back() {
    let fixture = Fixture::start(true).await;
    repository(&fixture);
    let (mut dom, _props, _state) = open_panel(&fixture).await;
    until(&mut dom, "the branch's files", |dom| control(dom, "Expand b.txt").is_some()).await;

    click_control(&mut dom, "Change comparison target. Currently main");
    until(&mut dom, "the refs", |dom| control(dom, "mid").is_some()).await;
    assert!(control(&dom, "Automatic").is_some() && control(&dom, "main").is_some());
    assert!(control(&dom, "feature/x").is_none(), "the branch being compared is not a base for itself");
    // `mid` is where the branch is: only the uncommitted edit differs.
    click_control(&mut dom, "mid");
    until(&mut dom, "against mid", |dom| control(dom, "Expand b.txt").is_none() && control(dom, "Expand README.md").is_some()).await;
    assert!(control(&dom, "Change comparison target. Currently mid").is_some());
    assert!(control(&dom, "Search refs").is_none(), "choosing closes the picker");

    click_control(&mut dom, "Change comparison target. Currently mid");
    until(&mut dom, "the refs again", |dom| control(dom, "Automatic").is_some()).await;
    click_control(&mut dom, "Automatic");
    until(&mut dom, "automatic base", |dom| control(dom, "Expand b.txt").is_some()).await;
    assert!(control(&dom, "Change comparison target. Currently main").is_some());
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn unchanged_lines_between_hunks_are_expanded_from_the_files_contents() {
    let fixture = Fixture::start(true).await;
    repository(&fixture);
    let (mut dom, _props, state) = open_panel(&fixture).await;
    until(&mut dom, "the files", |dom| control(dom, "Expand long.txt").is_some()).await;
    click_control(&mut dom, "Expand long.txt");
    let text = rendered_text(&dom);
    assert!(text.contains("LINE 5") && text.contains("LINE 35") && !text.contains("line 20"), "{text}");
    assert!(text.contains("23 unchanged lines"));

    // The first click reads the file from the server; the lines appear and the hunks join.
    click_control(&mut dom, "Expand 23 unchanged lines in long.txt");
    wait_for_rendered_text(&mut dom, "line 20").await;
    let text = rendered_text(&dom);
    assert!(!text.contains("23 unchanged lines") && text.contains("line 9") && text.contains("line 31"), "{text}");
    assert_eq!(text.matches("@@ -").count(), 1, "the two hunks are one now");
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_truncated_preview_lists_every_file_and_reads_patches_as_they_are_asked_for() {
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["branch", "-m", "main"]);
    git(&fixture.workspace, &["checkout", "-q", "-b", "big"]);
    // Six files of 30 KB each: more than the server puts in one preview.
    for index in 1..=6 {
        let body: String = (0..1500).map(|line| format!("file {index} line {line:05}\n")).collect();
        fixture.write(&format!("f{index}.txt"), &body);
    }
    git(&fixture.workspace, &["add", "."]);
    git(&fixture.workspace, &["commit", "-q", "-m", "big"]);
    let (mut dom, _props, state) = open_panel(&fixture).await;
    // The first four files are read; the last two wait to be asked for, with their counts known.
    until(&mut dom, "the first files", |dom| control(dom, "Expand f4.txt").is_some()).await;
    assert!(control(&dom, "Load diff for f5.txt").is_some() && control(&dom, "Load diff for f6.txt").is_some());
    assert!(!rendered_text(&dom).contains("exceeds the size limit"), "a preview read file by file is not called incomplete");
    assert!(rendered_text(&dom).contains("+1,5k") || rendered_text(&dom).contains("1.5k") || rendered_text(&dom).contains("1500"), "counts of every file are shown: {}", rendered_text(&dom));

    // Asking for one file out of order reads just that one.
    click_control(&mut dom, "Load diff for f6.txt");
    until(&mut dom, "f6", |dom| control(dom, "Expand f6.txt").is_some()).await;
    assert!(control(&dom, "Load diff for f5.txt").is_some());
    // The rest, together.
    click_control(&mut dom, "Load more file diffs");
    until(&mut dom, "f5", |dom| control(dom, "Expand f5.txt").is_some()).await;
    assert!(control(&dom, "Load more file diffs").is_none());
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_may_not_read_files_is_told_so_and_asks_nothing() {
    let fixture = Fixture::start(true).await;
    repository(&fixture);
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    click_control(&mut dom, "Diff");
    wait_for_rendered_text(&mut dom, "This connection cannot read local diffs.").await;
    assert!(control(&dom, "Refresh diff").is_some_and(|_| true));
    assert!(control(&dom, "Expand long.txt").is_none());
    // The checkpoint scopes need no file grant.
    change_control(&mut dom, "Diff scope", "latest");
    wait_for_rendered_text(&mut dom, "No completed turns yet.").await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_window_gaining_focus_and_a_command_finishing_in_the_thread_read_the_checkout_again() {
    let fixture = Fixture::start(true).await;
    repository(&fixture);
    let (mut dom, props, state) = open_panel(&fixture).await;
    change_control(&mut dom, "Diff scope", "unstaged");
    until(&mut dom, "the working tree", |dom| control(dom, "Expand README.md").is_some()).await;
    click_control(&mut dom, "Expand README.md");
    assert!(rendered_text(&dom).contains("uncommitted"));

    // The reader comes back from an editor: the window gains focus and the checkout is read again.
    fixture.write("README.md", "hello\nuncommitted\nfrom the editor\n");
    assert!(!rendered_text(&dom).contains("from the editor"), "nothing reads on its own");
    crate::review_diff::window_gained_focus();
    wait_for_rendered_text(&mut dom, "from the editor").await;

    // A turn finishes in the thread (its command ran): the panel reads again without being asked.
    fixture.write("README.md", "hello\nuncommitted\nfrom the editor\nfrom the turn\n");
    assert!(!rendered_text(&dom).contains("from the turn"));
    turn(&mut dom, &props, state, 1).await;
    wait_for_rendered_text(&mut dom, "from the turn").await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}
