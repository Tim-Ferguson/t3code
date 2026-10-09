//! The git actions control through the mounted UI against the real native server, temporary git
//! repositories and a local bare remote: the primary action for a status, the commit dialog with
//! a message, live hook output, push, a push the remote refuses, the question asked before the
//! default branch, a text generation failure, and a connection that may not write.
//! Waits are on state or text that only the awaited step produces; a hook that must be caught
//! mid-run waits for a file the test creates, not for a clock.
use super::diff_tests::{Fixture, Harness, error, git, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control, control_attribute, input_control};
use std::path::{Path, PathBuf};

fn client(fixture: &Fixture, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Harness) {
    fixture.client(Gate::default(), scopes)
}
const WRITER: &[AuthEnvironmentScope] = &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SourceControlWrite];

/// `main` pushed to a bare `origin` (whose default is `main`), and the thread open on the
/// workspace, on `feature/p` unless `default_branch` keeps it on `main`.
async fn open(fixture: &Fixture, scopes: &[AuthEnvironmentScope], default_branch: bool) -> (VirtualDom, Store<UiModel>, PathBuf) {
    let (dom, state, origin, _props) = open_with(fixture, scopes, default_branch, None).await;
    (dom, state, origin)
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
async fn open_with(fixture: &Fixture, scopes: &[AuthEnvironmentScope], default_branch: bool, probe: Option<crate::RenderProbe>) -> (VirtualDom, Store<UiModel>, PathBuf, Harness) {
    let repo = &fixture.workspace;
    git(repo, &["branch", "-m", "main"]);
    let root = repo.parent().unwrap().to_path_buf();
    let origin = root.join("origin.git");
    git(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(repo, &["push", "-q", "-u", "origin", "main"]);
    git(repo, &["remote", "set-head", "origin", "main"]);
    if !default_branch {
        git(repo, &["checkout", "-q", "-b", "feature/p"]);
        git(repo, &["push", "-q", "-u", "origin", "feature/p"]);
    }
    let (mut dom, props) = client(fixture, scopes);
    if let Some(probe) = probe {
        dom = VirtualDom::new_with_props(probe_harness, ProbeHarness { inner: props.clone(), probe });
        dom.rebuild_in_place();
    }
    let state = seed(&mut dom, &props, repo).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    (dom, state, origin, props)
}

fn remote_head(origin: &Path, branch: &str) -> String {
    git(origin, &["rev-parse", &format!("refs/heads/{branch}")]).trim().to_owned()
}

#[cfg(unix)]
fn hook(directory: &Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Lets a waiting hook go when the test ends, however it ends: a failed test must not leave the
/// server blocked on it.
struct Release(PathBuf);
impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "go");
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_commit_with_a_message_shows_hook_output_while_it_runs_and_cannot_be_sent_twice() {
    let fixture = Fixture::start(true).await;
    let gate = fixture.directory.path().join("hook-release");
    let _release = Release(gate.clone());
    let (mut dom, state, _origin) = open(&fixture, WRITER, false).await;
    fixture.write("widget.txt", "one\ntwo\n");
    fixture.write("other.txt", "x\n");
    // The hook prints, then waits for the test to let it finish.
    hook(&fixture.workspace.join(".git/hooks"), "pre-commit", &format!("echo 'lint started'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\necho 'lint done'", gate.display()));

    wait_for_rendered_text(&mut dom, "\u{25cf} 2").await;
    // A dirty feature branch offers to commit, push and open a pull request in one go.
    assert!(control(&dom, "Commit, push & PR").is_some());
    click_control(&mut dom, "Git action options");
    assert!(control(&dom, "Commit").is_some() && control(&dom, "Push").is_some() && control(&dom, "Create PR").is_some());
    assert_eq!(control_attribute(&dom, "Push", "disabled").unwrap().1.as_deref(), Some("true"), "nothing to push yet");
    assert!(rendered_text(&dom).contains("Commit or stash local changes before pushing."), "a disabled entry says why");
    click_control(&mut dom, "Commit");
    until(&mut dom, "the dialog", |dom| control(dom, "Commit message").is_some()).await;
    assert!(rendered_text(&dom).contains("Commit changes") && rendered_text(&dom).contains("Leave the message blank to auto-generate one."));
    assert!(rendered_text(&dom).contains("widget.txt") && rendered_text(&dom).contains("other.txt"));
    // Choose the files: leave one out.
    click_control(&mut dom, "Choose files");
    click_control(&mut dom, "Include other.txt");
    click_control(&mut dom, "Done choosing files");
    assert!(rendered_text(&dom).contains("(1 of 2)"));
    input_control(&mut dom, "Commit message", "Add widget\n\nBecause.");
    click_control(&mut dom, "Commit");
    assert!(control(&dom, "Commit message").is_none(), "the dialog closes when the action starts");

    // The hook's output is on screen while it runs, and nothing else can be started.
    wait_for_rendered_text(&mut dom, "lint started").await;
    assert!(control(&dom, "Commit").is_none(), "the primary action is replaced by the progress");
    assert_eq!(control_attribute(&dom, "Git action options", "disabled").unwrap().1.as_deref(), Some("true"));
    assert!(state.peek().git_runs.values().all(|run| run.running));
    std::fs::write(&gate, "go").unwrap();
    // The notice names the commit; the control is back, with the file left out still dirty.
    until(&mut dom, "the result", |dom| control(dom, "Dismiss notice").is_some()).await;
    assert_eq!(git(&fixture.workspace, &["log", "-1", "--format=%B"]).trim(), "Add widget\n\nBecause.");
    assert_eq!(git(&fixture.workspace, &["show", "--name-only", "--format=", "HEAD"]).trim(), "widget.txt");
    assert!(state.peek().git_runs.is_empty());
    until(&mut dom, "the primary action again", |dom| control(dom, "Git action options").is_some_and(|_| control_attribute(dom, "Git action options", "disabled").unwrap().1.as_deref() == Some("false"))).await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn push_sends_the_commits_and_a_refused_push_is_shown_with_its_phase() {
    let fixture = Fixture::start(true).await;
    let (mut dom, state, origin) = open(&fixture, WRITER, false).await;
    fixture.write("a.txt", "a\n");
    git(&fixture.workspace, &["add", "."]);
    git(&fixture.workspace, &["commit", "-q", "-m", "local work"]);
    wait_for_rendered_text(&mut dom, "\u{2191}1").await;
    // Ahead with no pull request and not the default branch: push and open one.
    assert!(control(&dom, "Push & create PR").is_some(), "{}", rendered_text(&dom));

    // The remote turns the push away: the action says where it stopped, and offers Retry.
    hook(&origin.join("hooks"), "pre-receive", "exit 1");
    click_control(&mut dom, "Git action options");
    click_control(&mut dom, "Push");
    until(&mut dom, "the failure", |dom| control(dom, "Retry action").is_some()).await;
    assert!(rendered_text(&dom).contains("Action failed while pushing"), "{}", rendered_text(&dom));
    assert!(control(&dom, "Dismiss notice").is_some(), "and the notice says the action failed");
    assert_ne!(remote_head(&origin, "feature/p"), git(&fixture.workspace, &["rev-parse", "HEAD"]).trim());

    // The remote relents; retrying sends it, and the failure goes away.
    std::fs::remove_file(origin.join("hooks/pre-receive")).unwrap();
    click_control(&mut dom, "Retry action");
    until(&mut dom, "pushed", |_| remote_head(&origin, "feature/p") == git(&fixture.workspace, &["rev-parse", "HEAD"]).trim()).await;
    until(&mut dom, "no failure left", |dom| control(dom, "Retry action").is_none() && state.peek().git_runs.is_empty()).await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_default_branch_asks_first_and_abort_sends_nothing() {
    let fixture = Fixture::start(true).await;
    let (mut dom, state, origin) = open(&fixture, WRITER, true).await;
    fixture.write("a.txt", "a\n");
    git(&fixture.workspace, &["add", "."]);
    git(&fixture.workspace, &["commit", "-q", "-m", "work on main"]);
    let before = remote_head(&origin, "main");
    wait_for_rendered_text(&mut dom, "\u{2191}1").await;
    assert!(control(&dom, "Push").is_some(), "{}", rendered_text(&dom));
    click_control(&mut dom, "Push");
    until(&mut dom, "the question", |dom| control(dom, "Abort").is_some()).await;
    let text = rendered_text(&dom);
    assert!(text.contains("Push to default ref?") && text.contains("This action will push local commits on \"main\". You can continue on this ref or create a feature ref and run the same action there."), "{text}");
    assert!(control(&dom, "Push to main").is_some() && control(&dom, "Check out feature branch & continue").is_some());
    click_control(&mut dom, "Abort");
    assert!(control(&dom, "Abort").is_none());
    assert!(state.peek().git_runs.is_empty(), "nothing was sent");
    assert_eq!(remote_head(&origin, "main"), before);

    click_control(&mut dom, "Push");
    until(&mut dom, "the question again", |dom| control(dom, "Push to main").is_some()).await;
    click_control(&mut dom, "Push to main");
    until(&mut dom, "pushed", |_| remote_head(&origin, "main") != before).await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_text_generation_failure_shows_its_detail_and_offers_retry_or_a_message_of_your_own() {
    let fixture = Fixture::start(true).await;
    let (mut dom, state, _origin) = open(&fixture, WRITER, false).await;
    fixture.write("a.txt", "a\n");
    wait_for_rendered_text(&mut dom, "\u{25cf} 1").await;
    let cwd = fixture.workspace.to_string_lossy().into_owned();
    // What the server streams when no model can write the message, as the control receives it.
    // The event words it and the stream's own failure names the operation and the detail; the control reads the latter.
    let mut progress = t3_client::git_actions::Progress::started("a1", t3_client::git_actions::Action::Commit);
    progress.apply(&json!({"actionId":"a1","kind":"action_failed","phase":"commit","message":"Text generation failed in generateCommitMessage: No provider is configured to write text."}));
    progress.apply_failure(&json!({"_tag":"TextGenerationError","operation":"generateCommitMessage","detail":"No provider is configured to write text."}));
    state.git_runs().write().insert(cwd.clone(), crate::git_control::Run {
        progress,
        spec: crate::git_control::Spec { action: t3_client::git_actions::Action::Commit, message: None, feature_branch: false, file_paths: None },
        running: false,
        previous_branch: None,
    });
    until(&mut dom, "the failure", |dom| control(dom, "Retry action").is_some()).await;
    let text = rendered_text(&dom);
    assert!(text.contains("Could not write the commit message: No provider is configured to write text."), "{text}");
    assert!(!text.contains("environment cannot generate text"), "no special-cased wording");
    // Writing the message yourself opens the dialog and clears the failure.
    click_control(&mut dom, "Write a commit message");
    until(&mut dom, "the dialog", |dom| control(dom, "Commit message").is_some()).await;
    assert!(control(&dom, "Retry action").is_none() && state.peek().git_runs.is_empty());
    click_control(&mut dom, "Cancel commit");
    assert!(control(&dom, "Commit message").is_none());
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_may_not_write_sees_the_actions_disabled_with_the_reason() {
    let fixture = Fixture::start(true).await;
    let (mut dom, _state, _origin) = open(&fixture, &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], false).await;
    fixture.write("a.txt", "a\n");
    wait_for_rendered_text(&mut dom, "\u{25cf} 1").await;
    assert_eq!(control_attribute(&dom, "Commit, push & PR", "disabled").unwrap().1.as_deref(), Some("true"));
    assert_eq!(control_attribute(&dom, "Commit, push & PR", "title").unwrap().1.as_deref(), Some("This connection cannot change source control."));
    click_control(&mut dom, "Git action options");
    assert_eq!(control_attribute(&dom, "Commit", "disabled").unwrap().1.as_deref(), Some("true"));
    assert_eq!(control_attribute(&dom, "Commit", "title").unwrap().1.as_deref(), Some("This connection cannot change source control."));
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn streaming_hook_output_repaints_only_the_git_control() {
    let fixture = Fixture::start(true).await;
    let gate = fixture.directory.path().join("hook-release");
    let _release = Release(gate.clone());
    let probe = crate::RenderProbe::default();
    let (mut dom, state, _origin, props) = open_with(&fixture, WRITER, false, Some(probe.clone())).await;
    // A turn first, and the tail of it delivered, so that what is measured is the hook's.
    super::diff_tests::turn(&mut dom, &props, state, 1).await;
    until(&mut dom, "an idle thread", |_| {
        state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.iter().all(|thread| !thread.status.is_active()))
            && state.peek().thread.projection.as_ref().is_some_and(|p| p["checkpoints"].as_array().is_some_and(|c| c.iter().any(|c| c["appRunOrdinal"] == 1 && c["status"] == "ready")))
    })
    .await;
    until(&mut dom, "the turn delivered", |_| state.peek().pending_messages.is_empty()).await;
    fixture.write("widget.txt", "one\n");
    hook(&fixture.workspace.join(".git/hooks"), "pre-commit", &format!("echo 'lint started'\necho 'lint second line'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\necho 'lint done'", gate.display()));
    wait_for_rendered_text(&mut dom, "\u{25cf} 1").await;
    let before = super::diff_tests::repaints(state, &probe);
    click_control(&mut dom, "Git action options");
    click_control(&mut dom, "Commit");
    until(&mut dom, "the dialog", |dom| control(dom, "Commit message").is_some()).await;
    input_control(&mut dom, "Commit message", "Add widget");
    click_control(&mut dom, "Commit");
    // Both lines of output arrive while the hook is still running.
    wait_for_rendered_text(&mut dom, "lint second line").await;
    std::fs::write(&gate, "go").unwrap();
    until(&mut dom, "the result", |dom| control(dom, "Dismiss notice").is_some()).await;
    let after = super::diff_tests::repaints(state, &probe);
    // The application (and so the sidebar), the conversation and the composer are not built again.
    assert_eq!(after.0, before.0, "hook output streaming: app, conversation and composer repaints");
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}
