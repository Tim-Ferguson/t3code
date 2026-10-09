//! Commit, push and create a pull request: the primary action and its menu beside the branch
//! label, the commit dialog, the question asked before pushing to the default branch, live
//! progress with hook output, and the follow-up the finished action offers. Port of
//! `GitActionsControl.tsx`.
//!
//! The control reads the branch control's status (one subscription per workspace) and keeps its
//! own state local, so progress and hook output repaint only the workspace header. A run lives in
//! the app model keyed by workspace, not in the control, so leaving the thread does not
//! interrupt it. Results go to `notice.rs`, since this UI has no toast system: they stay until
//! dismissed, where the original's success toast hides itself after ten seconds.
use crate::{
    branch::View,
    notice::{self, Follow, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use t3_client::{
    connection::ConnectionStatus,
    git_actions::{self, Action, Cta, MenuId, Outcome, Progress, QuickKind},
    vcs::Status,
};
use t3_contracts::AuthEnvironmentScope;

/// What was asked of the server, kept so a failed run can be asked again.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Spec {
    pub action: Action,
    pub message: Option<String>,
    pub feature_branch: bool,
    pub file_paths: Option<Vec<String>>,
}
impl Spec {
    fn of(action: Action) -> Self {
        Self { action, message: None, feature_branch: false, file_paths: None }
    }
}

/// The run of a workspace: in flight, or failed and not yet dismissed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Run {
    pub progress: Progress,
    pub spec: Spec,
    pub running: bool,
    /// What the thread named as its branch before, to compare when a new one was created.
    pub(crate) previous_branch: Option<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FollowRequest {
    pub id: u64,
    pub cwd: String,
    pub action: Action,
}

/// Starts the action unless the workspace is already running one, which is what makes a double
/// click send it once. The control re-enables from the run ending, never from a timer.
pub(crate) fn begin(state: Store<UiModel>, transport: &TransportHandle, cwd: &str, thread_id: &str, project_id: Option<&str>, spec: Spec, previous_branch: Option<String>) {
    if state.peek().git_runs.get(cwd).is_some_and(|run| run.running) {
        return;
    }
    let action_id = uuid::Uuid::new_v4().to_string();
    let mut payload = json!({"actionId":action_id,"cwd":cwd,"action":spec.action.wire()});
    if !thread_id.is_empty() {
        payload["threadId"] = json!(thread_id);
    }
    if let Some(project_id) = project_id {
        payload["projectId"] = json!(project_id);
    }
    if let Some(message) = spec.message.as_deref().filter(|message| !message.is_empty()) {
        payload["commitMessage"] = json!(message);
    }
    if spec.feature_branch {
        payload["featureBranch"] = json!(true);
    }
    if let Some(paths) = spec.file_paths.as_ref().filter(|paths| !paths.is_empty()) {
        payload["filePaths"] = json!(paths);
    }
    state.git_runs().write().insert(cwd.to_owned(), Run { progress: Progress::started(&action_id, spec.action), spec: spec.clone(), running: true, previous_branch: previous_branch.clone() });
    let (transport, cwd, thread_id) = (transport.clone(), cwd.to_owned(), thread_id.to_owned());
    dioxus::dioxus_core::spawn_forever(async move {
        let mut failure: Option<String> = None;
        let mut cause: Option<Value> = None;
        match runtime::request_stream(&transport, state, git_actions::RUN_STACKED_ACTION, payload) {
            Err(error) => failure = Some(error),
            Ok(mut stream) => {
                while let Some(result) = stream.next().await {
                    match result {
                        Ok(values) => {
                            let mut store = state.git_runs();
                            let mut runs = store.write();
                            if let Some(run) = runs.get_mut(&cwd).filter(|run| run.progress.action_id == action_id) {
                                for value in &values {
                                    run.progress.apply(value);
                                }
                            }
                        }
                        Err(error) => {
                            failure = Some(error);
                            cause = stream.cause();
                            break;
                        }
                    }
                }
            }
        }
        finish(state, &transport, &cwd, &thread_id, &action_id, failure, cause, previous_branch);
    });
}

/// The run is over: say how it ended, follow a created branch, and ask for the status again.
fn finish(state: Store<UiModel>, transport: &TransportHandle, cwd: &str, thread_id: &str, action_id: &str, failure: Option<String>, cause: Option<Value>, previous_branch: Option<String>) {
    let outcome = {
        let mut store = state.git_runs();
        let mut runs = store.write();
        let Some(run) = runs.get_mut(cwd).filter(|run| run.progress.action_id == action_id) else { return };
        run.running = false;
        if let Some(cause) = &cause {
            run.progress.apply_failure(cause);
        }
        if run.progress.outcome.is_none() {
            run.progress.outcome = Some(Outcome::Failed { phase: None, message: failure.unwrap_or_else(|| "The action ended without a result.".into()), generation: None });
        }
        run.progress.outcome.clone()
    };
    match outcome {
        Some(Outcome::Finished(done)) => {
            state.git_runs().write().remove(cwd);
            let follow = match done.cta {
                Cta::None => None,
                Cta::OpenPr { label, url } => Some(Follow::Open { label, url }),
                Cta::Run { label, action } => Some(Follow::Git { label, cwd: cwd.to_owned(), action }),
            };
            notice::push_with(state, Level::Success, done.title, done.description.unwrap_or_default(), follow);
            if let (Some(branch), false) = (done.branch, thread_id.is_empty()) {
                // The thread follows the branch the action created, as the original's does.
                runtime::command(transport, state, "thread.metadata.update", json!({"threadId":thread_id,"branch":branch,"expectedBranch":previous_branch}));
            }
        }
        Some(Outcome::Failed { message, generation, .. }) => {
            let shown = generation.map_or(message.clone(), |generation| generation.detail);
            notice::push(state, Level::Error, "Action failed", shown);
        }
        None => {}
    }
    let (transport, cwd) = (transport.clone(), cwd.to_owned());
    dioxus::dioxus_core::spawn_forever(async move {
        let _ = runtime::request_value_reported(transport, state, t3_client::vcs::REFRESH_STATUS, json!({"cwd":cwd})).await;
    });
}

/// The files the commit dialog offers, and which of them are left out.
#[derive(Debug, Clone, PartialEq, Default)]
struct CommitDraft {
    message: String,
    excluded: BTreeSet<String>,
    editing: bool,
}

/// The question asked before an action pushes to the default branch.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    spec: Spec,
    branch: String,
    includes_commit: bool,
}

#[component]
pub(crate) fn Actions(
    state: Store<UiModel>,
    transport: TransportHandle,
    view: Signal<View>,
    thread_id: String,
    project_id: String,
    cwd: String,
    /// Whether the branch control is busy with its own write (a switch, a pull).
    busy: Signal<Option<String>>,
    on_pull: Callback<()>,
) -> Element {
    let mut menu = use_signal(|| false);
    let mut commit = use_signal(|| None::<CommitDraft>);
    let mut confirm = use_signal(|| None::<Pending>);
    let can_write = use_memo(move || {
        let destination = state.destination().read().clone();
        *state.status().read() == ConnectionStatus::Connected && destination.is_some_and(|destination| state.environments().read().allows(&destination, AuthEnvironmentScope::SourceControlWrite))
    });
    let can_operate = use_memo(move || {
        let destination = state.destination().read().clone();
        destination.is_some_and(|destination| state.environments().read().allows(&destination, AuthEnvironmentScope::OrchestrationOperate))
    });
    let run = use_memo({
        let cwd = cwd.clone();
        move || state.git_runs().read().get(&cwd).cloned()
    });

    // The dialogs belong to the thread on screen: leaving it closes them.
    let start = {
        let (transport, cwd, thread_id, project_id) = (transport.clone(), cwd.clone(), thread_id.clone(), project_id.clone());
        use_callback(move |(spec, skip_prompt): (Spec, bool)| {
            if !*can_write.peek() || (spec.feature_branch && !*can_operate.peek()) {
                return;
            }
            let snapshot = view.peek().clone();
            let Some(status) = snapshot.status.filter(|_| snapshot.live) else { return };
            let on_default = !spec.feature_branch && status.local.is_default_ref;
            if !skip_prompt && git_actions::requires_default_confirmation(spec.action, on_default) {
                if let Some(branch) = status.local.ref_name.clone() {
                    let includes_commit = spec.action.commits() && (spec.action == Action::Commit || status.local.has_working_tree_changes || spec.feature_branch);
                    confirm.set(Some(Pending { spec, branch, includes_commit }));
                    return;
                }
            }
            let previous = status.local.ref_name.clone();
            begin(state, &transport, &cwd, &thread_id, (!project_id.is_empty()).then_some(project_id.as_str()), spec, previous);
        })
    };

    // The pull request of the current branch, from the status the branch control already reads.
    let pr_target = use_memo({
        let (project_id, thread_id) = (project_id.clone(), thread_id.clone());
        move || {
            view.read().status.as_ref().and_then(|status| status.remote.pr.clone()).and_then(|found| {
                t3_client::pull_request::Reference::from_url(&project_id, &found.url, found.number).map(|reference| (crate::pull_request_panel::Target { thread_id: thread_id.clone(), reference }, found))
            })
        }
    });
    // Tells the palette what the branch has to offer, as the branch picker does.
    use_effect(move || {
        let available = pr_target().map(|(target, _)| target);
        if state.pr_available().peek().clone() != available {
            state.pr_available().set(available);
        }
    });
    use_drop(move || {
        if state.pr_available().peek().is_some() {
            state.pr_available().set(None);
        }
    });

    // A notice's "Push" or "Create PR" asks this workspace's control, which asks what the
    // original asks before anything is sent.
    {
        let cwd = cwd.clone();
        use_effect(move || {
            let Some(request) = state.git_follow().read().clone() else { return };
            if request.cwd != cwd {
                return;
            }
            state.git_follow().set(None);
            start.call((Spec::of(request.action), false));
        });
    }
    let snapshot = view.read().clone();
    let status: Option<Status> = snapshot.status.clone();
    let live = snapshot.live;
    let Some(status_ref) = status.as_ref().filter(|status| status.local.is_repo) else { return rsx! {} };
    let running = run().is_some_and(|run| run.running);
    let busy_other = busy.read().is_some();
    let blocked = running || busy_other;
    let writable = can_write() && live;
    let terms = git_actions::terms(status_ref.local.provider_kind.as_deref());
    let quick = git_actions::quick_action(Some(status_ref), blocked);
    let items = git_actions::menu_items(Some(status_ref), blocked);
    let quick_reason = if !can_write() {
        Some("This connection cannot change source control.".to_owned())
    } else if !live {
        Some("Not connected. This is the last status seen here.".to_owned())
    } else if quick.disabled {
        Some(quick.hint.clone().unwrap_or_else(|| "This action is currently unavailable.".into()))
    } else {
        None
    };
    let progress = run().filter(|run| run.running).map(|run| run.progress);
    let failed = run().filter(|run| !run.running).and_then(|run| match &run.progress.outcome {
        Some(Outcome::Failed { phase, message, generation }) => Some((run.spec.clone(), phase.clone(), message.clone(), generation.clone())),
        _ => None,
    });
    let files = status_ref.local.changed.clone();
    let branch_name = status_ref.local.ref_name.clone().unwrap_or_else(|| "(detached HEAD)".to_owned());
    let is_default = status_ref.local.is_default_ref;
    let quick_clone = quick.clone();
    let primary = move |_| match quick_clone.kind {
        QuickKind::RunPull => on_pull.call(()),
        QuickKind::OpenPublish => notice::push(state, Level::Info, "Publish repository", "Publishing a repository is not available in this client yet. Add a remote with git, then push."),
        QuickKind::RunAction => {
            if let Some(action) = quick_clone.action {
                start.call((Spec::of(action), false));
            }
        }
        QuickKind::ShowHint => {}
    };
    let open_menu = {
        let (transport, cwd) = (transport.clone(), cwd.clone());
        move |_| {
            let open = !menu();
            menu.set(open);
            if open {
                let (transport, cwd) = (transport.clone(), cwd.clone());
                dioxus::dioxus_core::spawn_forever(async move {
                    let _ = runtime::request_value_reported(transport, state, t3_client::vcs::REFRESH_STATUS, json!({"cwd":cwd})).await;
                });
            }
        }
    };

    let draft = commit.read().clone();
    let pending = confirm.read().clone();
    rsx! {
        div { class: "git-actions", role: "group", "aria-label": "Git actions",
            if let Some(progress) = progress.clone() {
                {
                    let status_line = progress.status();
                    let last = progress.last_output().map(str::to_owned);
                    let label = match &last { Some(line) => format!("{status_line} {line}"), None => status_line.clone() };
                    rsx! {
                        button { class: "branch-action git-primary", r#type: "button", disabled: true, "aria-busy": "true", "aria-label": "{label}",
                            span { role: "status", "{status_line}" }
                            if let Some(line) = last { span { class: "git-output-line", "{line}" } }
                        }
                        if !progress.output.is_empty() {
                            details { class: "git-output",
                                summary { "Output" }
                                pre { aria_label: "Hook output", for line in progress.output.iter() { "{line}\n" } }
                            }
                        }
                    }
                }
            } else if let Some(reason) = quick_reason.clone() {
                button { class: "branch-action git-primary", r#type: "button", "aria-disabled": "true", disabled: quick.kind != QuickKind::RunPull && quick.kind != QuickKind::OpenPublish || !writable, "aria-label": "{quick.label}", title: "{reason}", onclick: primary.clone(), "{quick.label}" }
                span { class: "git-reason sr-only", "{reason}" }
            } else {
                button { class: "branch-action git-primary", r#type: "button", "aria-label": "{quick.label}", onclick: primary, "{quick.label}" }
            }
            if let Some((target, found)) = pr_target() {
                {
                    let label = match (found.state.as_str(), found.is_draft) { ("merged", _) => "Merged", ("closed", _) => "Closed", (_, true) => "Draft", _ => "Open" };
                    rsx! {
                        button { class: "branch-action pr-badge", r#type: "button", "data-state": "{label}", "aria-label": "{terms.singular} #{found.number}", title: "{found.title}", onclick: move |_| crate::pull_request_panel::open(state, target.clone()),
                            "{terms.short} #{found.number} \u{b7} {label}" }
                    }
                }
            }
            button { class: "branch-action git-options", r#type: "button", "aria-label": "Git action options", "aria-haspopup": "menu", "aria-expanded": "{menu()}", disabled: blocked, onclick: open_menu, "\u{25be}" }
            if menu() && !blocked {
                div { class: "branch-picker-backdrop", onclick: move |_| menu.set(false) }
                div { class: "branch-picker git-menu", role: "menu", "aria-label": "Git action options",
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); menu.set(false); } },
                    for item in items.iter() {
                        {
                            let reason = git_actions::disabled_reason(item, Some(status_ref), blocked);
                            let off = item.disabled || !writable;
                            let title = if !can_write() { Some("This connection cannot change source control.".to_owned()) } else { reason.clone() };
                            let (id, label) = (item.id, item.label.clone());
                            rsx! {
                                button { key: "{label}", class: "branch-row", role: "menuitem", r#type: "button", "aria-label": "{label}", "aria-disabled": "{off}", disabled: off, title: title.clone().unwrap_or_default(),
                                    onclick: move |_| {
                                        menu.set(false);
                                        match id {
                                            MenuId::Commit => commit.set(Some(CommitDraft::default())),
                                            MenuId::Push => start.call((Spec::of(Action::Push), false)),
                                            MenuId::Pr => start.call((Spec::of(Action::CreatePr), false)),
                                        }
                                    },
                                    span { class: "branch-row-name", "{label}" }
                                    if let Some(reason) = title.clone() { span { class: "branch-row-badge git-why", "{reason}" } }
                                }
                            }
                        }
                    }
                }
            }
            if let Some((spec, phase, message, generation)) = failed {
                {
                    let text = match (generation.clone(), &phase) {
                        (Some(generation), _) => format!("Could not write the {}: {}", git_actions::generation_subject(&generation.operation, terms), generation.detail),
                        (None, Some(phase)) => format!("Action failed while {}: {message}", match phase.as_str() { "branch" => "preparing the branch", "commit" => "committing", "push" => "pushing", "pr" => "creating the pull request", other => other }),
                        (None, None) => format!("Action failed: {message}"),
                    };
                    let writes_commit_message = generation.as_ref().is_some_and(|generation| generation.operation == "generateCommitMessage") && !files.is_empty();
                    rsx! {
                        div { class: "branch-notice failure git-failure", role: "alert",
                            span { "{text}" }
                            button { r#type: "button", "aria-label": "Retry action", disabled: !writable, onclick: { let spec = spec.clone(); move |_| start.call((spec.clone(), false)) }, "Retry" }
                            if writes_commit_message { button { r#type: "button", "aria-label": "Write a commit message", disabled: !writable, onclick: { let key = cwd.clone(); move |_| { state.git_runs().write().remove(&key); commit.set(Some(CommitDraft::default())); } }, "Write a message" } }
                            button { r#type: "button", "aria-label": "Dismiss git action message", onclick: { let key = cwd.clone(); move |_| { state.git_runs().write().remove(&key); } }, "\u{d7}" }
                        }
                    }
                }
            }
            if let Some(draft) = draft {
                {
                    let selected: Vec<_> = files.iter().filter(|file| !draft.excluded.contains(&file.path)).collect();
                    let all_selected = draft.excluded.is_empty();
                    let none_selected = selected.is_empty();
                    let (additions, deletions) = selected.iter().fold((0, 0), |(a, d), file| (a + file.insertions, d + file.deletions));
                    let message = draft.message.clone();
                    let paths: Vec<String> = selected.iter().map(|file| file.path.clone()).collect();
                    let spec = Spec { action: Action::Commit, message: Some(message.trim().to_owned()).filter(|message| !message.is_empty()), feature_branch: false, file_paths: (!all_selected).then(|| paths.clone()) };
                    let new_branch = Spec { feature_branch: true, ..spec.clone() };
                    let can_branch = can_write() && can_operate() && live;
                    rsx! {
                        div { class: "modal-backdrop",
                            div { class: "dialog commit-dialog", role: "dialog", "aria-modal": "true", "aria-labelledby": "commit-heading", "aria-describedby": "commit-description", tabindex: "-1",
                                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                                onkeydown: move |event| { if event.key() == Key::Escape { commit.set(None); } },
                                h2 { id: "commit-heading", "Commit changes" }
                                p { id: "commit-description", "Review and confirm your commit. Leave the message blank to auto-generate one." }
                                div { class: "commit-summary",
                                    div { class: "commit-row", span { class: "muted", "Branch" } span { "{branch_name}" } if is_default { span { class: "commit-warning", "Default branch" } } }
                                    div { class: "commit-row",
                                        if draft.editing && !files.is_empty() {
                                            input { r#type: "checkbox", "aria-label": "Select all files", checked: all_selected,
                                                onclick: { let all: BTreeSet<String> = files.iter().map(|file| file.path.clone()).collect(); move |_| commit.with_mut(|draft| if let Some(draft) = draft { draft.excluded = if draft.excluded.is_empty() { all.clone() } else { BTreeSet::new() }; }) } }
                                        }
                                        span { class: "muted", "Files" }
                                        if !all_selected && !draft.editing { span { class: "muted", "({selected.len()} of {files.len()})" } }
                                        if !files.is_empty() {
                                            button { r#type: "button", "aria-label": if draft.editing { "Done choosing files" } else { "Choose files" }, onclick: move |_| commit.with_mut(|draft| if let Some(draft) = draft { draft.editing = !draft.editing; }), if draft.editing { "Done" } else { "Edit" } }
                                        }
                                    }
                                    if files.is_empty() { p { class: "commit-none", "none" } }
                                    else {
                                        ul { class: "commit-files", "aria-label": "Files to commit",
                                            for file in files.iter() {
                                                {
                                                    let excluded = draft.excluded.contains(&file.path);
                                                    let path = file.path.clone();
                                                    rsx! {
                                                        li { key: "{path}",
                                                            if draft.editing {
                                                                input { r#type: "checkbox", "aria-label": "Include {path}", checked: !excluded,
                                                                    onclick: { let path = path.clone(); move |_| commit.with_mut(|draft| if let Some(draft) = draft { if !draft.excluded.remove(&path) { draft.excluded.insert(path.clone()); } }) } }
                                                            }
                                                            span { class: "commit-path", title: "{path}", "{path}" }
                                                            if excluded { span { class: "muted", "Excluded" } } else { span { class: "commit-counts", span { class: "stat-add", "+{file.insertions}" } " / " span { class: "stat-del", "-{file.deletions}" } } }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        div { class: "commit-totals", span { class: "stat-add", "+{additions}" } " / " span { class: "stat-del", "-{deletions}" } }
                                    }
                                }
                                label { r#for: "commit-message", "Commit message (optional)" }
                                textarea { id: "commit-message", "aria-label": "Commit message", placeholder: "Leave empty to auto-generate", value: "{message}",
                                    oninput: move |event| commit.with_mut(|draft| if let Some(draft) = draft { draft.message = event.value(); }) }
                                div { class: "dialog-actions",
                                    button { r#type: "button", "aria-label": "Cancel commit", onclick: move |_| commit.set(None), "Cancel" }
                                    button { r#type: "button", "aria-label": "Commit on new branch", disabled: !can_branch || none_selected || blocked,
                                        onclick: { let new_branch = new_branch.clone(); move |_| { commit.set(None); start.call((new_branch.clone(), true)); } }, "Commit on new branch" }
                                    button { class: "primary", r#type: "button", "aria-label": "Commit", disabled: !writable || none_selected || blocked,
                                        onclick: { let spec = spec.clone(); move |_| { commit.set(None); start.call((spec.clone(), false)); } }, "Commit" }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(pending) = pending {
                {
                    let copy = git_actions::default_branch_copy(pending.spec.action, &pending.branch, pending.includes_commit, terms);
                    let on_branch = Spec { feature_branch: true, ..pending.spec.clone() };
                    let same = pending.spec.clone();
                    rsx! {
                        div { class: "modal-backdrop",
                            div { class: "dialog default-branch-dialog", role: "alertdialog", "aria-modal": "true", "aria-labelledby": "default-branch-heading", "aria-describedby": "default-branch-description", tabindex: "-1",
                                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                                onkeydown: move |event| { if event.key() == Key::Escape { confirm.set(None); } },
                                h2 { id: "default-branch-heading", "{copy.title}" }
                                p { id: "default-branch-description", "{copy.description}" }
                                div { class: "dialog-actions",
                                    button { r#type: "button", "aria-label": "Abort", onclick: move |_| confirm.set(None), "Abort" }
                                    button { class: "primary", r#type: "button", "aria-label": "{copy.continue_label}", disabled: !writable || blocked,
                                        onclick: move |_| { confirm.set(None); start.call((same.clone(), true)); }, "{copy.continue_label}" }
                                    button { r#type: "button", "aria-label": "Check out feature branch & continue", disabled: !(can_write() && can_operate() && live) || blocked,
                                        onclick: move |_| { confirm.set(None); start.call((on_branch.clone(), true)); }, "Check out feature branch & continue" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
