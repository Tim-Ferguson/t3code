//! One message to several models, through the mounted new-thread form and the real native server with temporary git
//! repositories: Shift-click in the picker chooses several, the workspace is fixed to a new worktree, and sending starts one
//! thread for each model with its own worktree. The checks, the refusals, a lost answer (a thread that may exist) and an
//! older server are told in the original's words. Waits are on state that only the awaited step produces.
use super::diff_tests::{Fixture, Harness, git, rendered_text, seed, until, wait_for_rendered_text};
use super::keys_tests::key_on;
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, click_nth_control, control, control_attribute, input_control, mouse_control};

const OPERATOR: [AuthEnvironmentScope; 2] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];

async fn with_form(git_repository: bool, body: impl AsyncFnOnce(&Fixture, &mut VirtualDom, &Harness, Store<UiModel>)) {
    // The provider offers two models, so that two can be chosen together.
    let fixture = Fixture::start_with(git_repository, &[("FIXTURE_SECOND_MODEL", "1")]).await;
    let (mut dom, props) = fixture.client(Gate::default(), &OPERATOR);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    body(&fixture, &mut dom, &props, state).await;
    fixture.finish(dom).await;
}

/// The threads other than the two the fixture seeds.
fn started(state: Store<UiModel>) -> Vec<t3_contracts::ThreadShell> {
    state.peek().shell.snapshot.as_ref().map(|snapshot| snapshot.threads.iter().filter(|thread| !matches!(thread.id.as_str(), "thread-a" | "thread-b")).cloned().collect()).unwrap_or_default()
}
fn models_of(threads: &[t3_contracts::ThreadShell]) -> Vec<String> {
    let mut models: Vec<String> = threads.iter().map(|thread| thread.model_selection.model.to_string()).collect();
    models.sort();
    models
}
fn notice_titles(state: Store<UiModel>) -> Vec<String> {
    state.peek().notices.iter().map(|notice| notice.title.clone()).collect()
}
fn form_key(state: Store<UiModel>) -> (t3_contracts::EnvironmentId, String) {
    (state.peek().destination.clone().unwrap(), state.peek().active_draft.clone().expect("the form shows a draft"))
}
fn prompt_of(state: Store<UiModel>) -> String {
    state.peek().new_thread_drafts.get(&form_key(state)).cloned().unwrap_or_default()
}

/// Opens the picker and Shift-clicks the other model, as a person adds one.
async fn choose_both(dom: &mut VirtualDom) {
    click_control(dom, "Model");
    until(dom, "the picker", |dom| control(dom, "Search models").is_some()).await;
    mouse_control(dom, "Codex · Fixture Alternate", "click", false, false, true);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn shift_click_chooses_several_models_and_sending_starts_a_thread_in_its_own_worktree_for_each() {
    with_form(true, async |fixture, dom, _props, state| {
        git(&fixture.workspace, &["branch", "-m", "main"]);
        input_control(dom, "First message", "compare the two");
        // One model is a plain choice: the workspace is a choice too.
        assert!(control(dom, "Workspace").is_some_and(|(_, value)| value.as_deref() == Some("local")));

        choose_both(dom).await;
        // The list stays open for more, both rows are checked, and the button names both models.
        assert!(control(dom, "Search models").is_some());
        assert_eq!(control_attribute(dom, "Model", "title").and_then(|(_, value)| value).as_deref(), Some("Fixture Model, Fixture Alternate"));
        assert!(rendered_text(dom).contains("Fixture Model, Fixture Alternate"));
        assert!(rendered_text(dom).contains('\u{2713}'));
        key_on(dom, "Search models", Key::Escape);
        input_control(dom, "Base branch", "main");
        // The workspace is fixed to a new worktree for each model, and says so.
        assert!(control(dom, "Workspace").is_some_and(|(_, value)| value.is_none()), "no longer a select");
        assert_eq!(control_attribute(dom, "Workspace", "title").and_then(|(_, value)| value).as_deref(), Some("Each model starts in its own worktree."));

        click_control(dom, "Create new thread");
        until(dom, "both threads in their worktrees", |_| {
            let threads = started(state);
            threads.len() == 2 && threads.iter().all(|thread| thread.worktree_path.is_some())
        })
        .await;
        let threads = started(state);
        assert_eq!(models_of(&threads), ["fixture-model", "fixture-model-alt"]);
        let paths: std::collections::BTreeSet<_> = threads.iter().map(|thread| thread.worktree_path.as_ref().unwrap().to_string()).collect();
        assert_eq!(paths.len(), 2, "a worktree for each model");
        for path in &paths {
            assert!(std::path::Path::new(path).join("README.md").exists(), "{path} is a checkout of the base branch");
        }
        assert_eq!(git(&fixture.workspace, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 3, "the project and one for each model");
        // Both threads carry the one message, and the person stays on the form with the draft cleared.
        assert!(threads.iter().all(|thread| thread.title.as_str() == "compare the two"));
        assert_eq!(state.peek().active_thread, None);
        assert_eq!(prompt_of(state), "");
        wait_for_rendered_text(dom, "Started 2 threads in background").await;
        assert_eq!(state.peek().error, None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn choosing_one_model_again_ends_the_several_and_a_plain_click_chooses_alone() {
    with_form(true, async |_fixture, dom, _props, state| {
        choose_both(dom).await;
        assert!(control(dom, "Workspace").is_some_and(|(_, value)| value.is_none()));
        // Shift-click on a chosen model removes it; one model left is the ordinary choice, with its workspace choice back.
        mouse_control(dom, "Codex · Fixture Alternate", "click", false, false, true);
        key_on(dom, "Search models", Key::Escape);
        assert!(control(dom, "Workspace").is_some_and(|(_, value)| value.as_deref() == Some("local")));
        assert_eq!(control(dom, "Model").and_then(|(_, value)| value).as_deref(), Some("[\"codex\",\"fixture-model\"]"));
        assert!(!rendered_text(dom).contains("Fixture Model, Fixture Alternate"));

        // A plain click on a row while several are chosen picks that one alone.
        choose_both(dom).await;
        click_control(dom, "Codex · Fixture Alternate");
        assert!(control(dom, "Search models").is_none(), "a plain click closes the list");
        assert!(control(dom, "Workspace").is_some_and(|(_, value)| value.as_deref() == Some("local")));
        assert_eq!(control(dom, "Model").and_then(|(_, value)| value).as_deref(), Some("[\"codex\",\"fixture-model-alt\"]"));
        assert!(started(state).is_empty());
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn several_models_without_a_base_branch_or_a_message_start_nothing_and_say_why() {
    with_form(true, async |_fixture, dom, _props, state| {
        choose_both(dom).await;
        key_on(dom, "Search models", Key::Escape);
        // Nothing to send yet.
        assert_eq!(control_attribute(dom, "Create new thread", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
        input_control(dom, "First message", "go");
        // A message but no base branch: the original's warning, and nothing is taken from the draft.
        click_control(dom, "Create new thread");
        wait_for_rendered_text(dom, "Choose models and a base branch").await;
        assert!(rendered_text(dom).contains("Multiple models need a new thread in a Git project. Each gets its own worktree."));
        assert_eq!(prompt_of(state), "go");
        assert!(started(state).is_empty());
        assert_eq!(notice_titles(state), ["Choose models and a base branch"]);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn several_models_in_a_folder_that_is_not_a_git_repository_start_nothing() {
    with_form(false, async |_fixture, dom, _props, state| {
        choose_both(dom).await;
        key_on(dom, "Search models", Key::Escape);
        input_control(dom, "First message", "go");
        input_control(dom, "Base branch", "main");
        click_control(dom, "Create new thread");
        wait_for_rendered_text(dom, "Choose models and a base branch").await;
        assert_eq!(prompt_of(state), "go", "the message stays in the draft");
        assert!(started(state).is_empty());
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_server_that_cannot_prepare_a_worktree_for_each_thread_offers_one_model_only() {
    with_form(true, async |_fixture, dom, _props, state| {
        // An older server does not advertise `requiredWorktreeBootstrap`.
        state.typed_config().write().as_mut().unwrap().environment.capabilities.required_worktree_bootstrap = None;
        click_control(dom, "Model");
        until(dom, "the picker", |dom| control(dom, "Search models").is_some()).await;
        mouse_control(dom, "Codex · Fixture Alternate", "click", false, false, true);
        // The Shift-click is a plain click on this server: the one model chosen, no several.
        assert!(control(dom, "Search models").is_none());
        assert!(control(dom, "Workspace").is_some_and(|(_, value)| value.as_deref() == Some("local")));
        assert_eq!(control(dom, "Model").and_then(|(_, value)| value).as_deref(), Some("[\"codex\",\"fixture-model-alt\"]"));
        assert!(crate::multi_model::chosen(state, &form_key(state)).is_none());
        assert!(!rendered_text(dom).contains('\u{2713}'));
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_send_whose_answers_are_lost_may_have_started_its_threads_so_it_is_not_repeated_until_allowed() {
    with_form(true, async |fixture, dom, props, state| {
        git(&fixture.workspace, &["branch", "-m", "main"]);
        input_control(dom, "First message", "lost answers");
        choose_both(dom).await;
        key_on(dom, "Search models", Key::Escape);
        input_control(dom, "Base branch", "main");
        click_control(dom, "Create new thread");
        // The requests are out once the draft is cleared; the connection goes before any answer is read.
        until(dom, "the requests out", |_| prompt_of(state).is_empty() && !crate::multi_model::sending(state, &form_key(state))).await;
        {
            // A connection that went: its requests are not cancelled (the new connection knows nothing of them) and nothing answers them.
            let mut transport = props.transport.borrow_mut();
            transport.socket_generation += 1;
            transport.fail_waiters();
        }
        wait_for_rendered_text(dom, "Could not start fixture-model").await;
        wait_for_rendered_text(dom, "Could not start fixture-model-alt").await;
        // The server did get them: both threads exist even though the client never heard.
        until(dom, "both threads", |_| started(state).len() == 2).await;
        // The message and both models are back for another try; each failure offers the thread and a retry.
        assert_eq!(prompt_of(state), "lost answers");
        assert_eq!(crate::multi_model::chosen(state, &form_key(state)).map(|models| models.len()), Some(2));
        assert!(control(dom, "Open thread").is_some() && control(dom, "Allow retry").is_some());

        // Sending again is refused for both, in the original's words, and starts nothing.
        click_control(dom, "Create new thread");
        until(dom, "the refusals", |_| state.peek().notices.iter().filter(|notice| notice.detail == "The previous request may have started. Open its thread to check before sending again.").count() == 2).await;
        assert_eq!(started(state).len(), 2);
        assert_eq!(prompt_of(state), "lost answers");

        // "Allow retry" asks first; declining keeps the refusal.
        click_nth_control(dom, "Allow retry", 0);
        wait_for_rendered_text(dom, "Check its thread first. Allow another send that could create a duplicate thread?").await;
        click_control(dom, "Cancel retry");
        assert!(control(dom, "Cancel retry").is_none());
        // Accepting forgets that one thread may exist, so the next send makes a new one for that model only.
        click_nth_control(dom, "Allow retry", 0);
        click_control(dom, "Allow another send");
        assert!(control(dom, "Cancel retry").is_none());
        click_control(dom, "Create new thread");
        until(dom, "the new thread", |_| started(state).len() == 3).await;
        wait_for_rendered_text(dom, "Started 1 thread in background").await;
        // The other model, still unsure, is chosen alone with the message back in the draft.
        until(dom, "the draft back", |_| prompt_of(state) == "lost answers" && crate::multi_model::chosen(state, &form_key(state)).map(|models| models.len()) == Some(1)).await;
    })
    .await;
}
