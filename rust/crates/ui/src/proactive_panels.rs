//! Panels that open on their own (setting "Proactive panels"): the effect of `ChatView.tsx` that opens a thread's linked pull
//! requests when they first appear, and the diff of a turn that has just finished and changed enough. It draws nothing. What
//! it may do is decided by `t3_client::proactive_panels`; a panel choice of the person's made after it looked always wins,
//! because every opening names the count of choices it read (`Panels::open_proactive`).
//!
//! Not ported: the original does not open a panel while the plan sidebar is a sheet (a narrow window; this layout has none),
//! and follows a replacement pull request when the linked one was merged or closed and the branch has a newer open one.
use crate::{
    right_panel::key_of,
    runtime::{UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use std::{cell::RefCell, rc::Rc};
use t3_client::{
    checkpoints::{self, Status, Turns},
    proactive_panels::{self as logic, Checkpoint, DiffAction, Linked, Observation},
    right_panel::{PullRequestTarget, Surface},
    thread_pull_requests,
};

/// What the open thread's runs say.
#[derive(Debug, Clone, PartialEq, Default)]
struct Runs {
    /// The run in progress (`activeRuntime.activeRunId`).
    running: Option<String>,
    /// The newest run, and whether it completed.
    latest: Option<(String, bool)>,
}
impl Runs {
    /// A run is over when it is the newest and nothing else is in progress (`isLatestRunSettled`).
    fn settled(&self) -> Option<&str> {
        self.latest.as_ref().filter(|(id, _)| self.running.as_deref() != Some(id.as_str())).map(|(id, _)| id.as_str())
    }
}

/// What the thread's pull request links say.
#[derive(Debug, Clone, PartialEq, Default)]
struct Links {
    /// How many pull requests the thread shows.
    count: usize,
    /// The set, to tell a new one from one already opened for.
    key: Option<String>,
    linked: Option<Linked>,
    linked_url: Option<String>,
    linked_host: Option<String>,
}

fn runs_of(state: Store<UiModel>, thread_id: &str) -> Runs {
    let domain = state.thread();
    let domain = domain.read();
    let Some(projection) = domain.projection.as_ref().filter(|projection| projection["thread"]["id"] == thread_id) else { return Runs::default() };
    let runs = projection["runs"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let running = runs
        .iter()
        .filter(|run| matches!(run["status"].as_str(), Some("preparing" | "starting" | "running" | "waiting")))
        .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0))
        .and_then(|run| run["id"].as_str().map(str::to_owned));
    let latest = runs
        .iter()
        .filter(|run| run["queueHeld"] != true)
        .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0))
        .and_then(|run| Some((run["id"].as_str()?.to_owned(), run["status"] == "completed")));
    Runs { running, latest }
}

fn links_of(state: Store<UiModel>, thread_id: &str, supports_thread_links: bool) -> Links {
    let Some(thread) = crate::thread_pull_requests_panel::thread_value(state, thread_id) else { return Links::default() };
    let visible = thread_pull_requests::links(&thread);
    let linked = thread["linkedPullRequest"].as_object().and_then(|linked| {
        Some(Linked { project_id: thread["projectId"].as_str()?.to_owned(), repository: linked.get("repository")?.as_str()?.to_owned(), number: linked.get("number")?.as_u64()? })
    });
    let linked_url = thread["linkedPullRequest"]["url"].as_str().map(str::to_owned);
    let linked_host = linked.as_ref().and_then(|linked| visible.iter().find(|link| link.repository.eq_ignore_ascii_case(&linked.repository) && link.number == linked.number).and_then(|link| link.host.clone()));
    let surface_available = supports_thread_links && !visible.is_empty();
    let key = if surface_available {
        let mut entries: Vec<_> = visible.iter().map(|link| (link.host.clone(), link.repository.clone(), link.number)).collect();
        entries.sort();
        Some(serde_json::to_string(&entries).expect("links"))
    } else {
        linked.as_ref().map(|linked| serde_json::to_string(&(&linked.project_id, &linked.repository, linked.number)).expect("linked"))
    };
    Links { count: visible.len(), key, linked, linked_url, linked_host }
}

fn linked_target(links: &Links, linked: &Linked) -> Surface {
    Surface::PullRequest(PullRequestTarget { environment_id: None, project_id: linked.project_id.clone(), host: links.linked_host.clone(), repository: linked.repository.clone(), number: linked.number, url: links.linked_url.clone() })
}

/// What the effect keeps between runs, per window.
#[derive(Default)]
struct Memory {
    observation: Option<Observation>,
    /// The linked pull request seen last, and the thread it was seen for.
    pull_request: Option<(String, Option<Linked>)>,
}

#[component]
pub(crate) fn Host(state: Store<UiModel>) -> Element {
    let memory = use_hook(|| Rc::new(RefCell::new(Memory::default())));
    let owner = use_memo(move || {
        let environment = state.destination().read().as_ref()?.to_string();
        let thread = state.active_thread().read().clone()?;
        Some((environment, thread))
    });
    let runs = use_memo(move || owner().map(|(_, thread)| runs_of(state, &thread)).unwrap_or_default());
    let turns = use_memo(move || owner().map(|(_, thread)| checkpoints::summarize(state.thread().read().projection.as_ref(), &thread)).unwrap_or_default());
    let capabilities = use_memo(move || {
        let field = state.typed_config();
        let config = field.read();
        let capabilities = config.as_ref().map(|config| &config.environment.capabilities);
        (config.is_some(), capabilities.is_some_and(|capabilities| capabilities.pull_requests == Some(true)), capabilities.is_some_and(|capabilities| capabilities.thread_pull_requests == Some(true)))
    });
    let links = use_memo(move || owner().map(|(_, thread)| links_of(state, &thread, capabilities().2)).unwrap_or_default());
    let enabled = use_memo(move || state.client_settings().read().proactive_panels_enabled);
    let writer = try_consume_context::<crate::client_settings::Writer>();
    let hydrated = use_memo(move || writer.as_ref().is_none_or(|writer| writer.document.read().hydrated()));
    use_effect(move || {
        let Some((environment, thread)) = owner() else {
            *memory.borrow_mut() = Memory::default();
            return;
        };
        let (runs, turns, links, (config_known, supports_pull_requests, supports_thread_links)) = (runs(), turns(), links(), capabilities());
        let (enabled, hydrated) = (enabled(), hydrated());
        let key = key_of(&environment, Some(&thread));
        let mut memory = memory.borrow_mut();
        let (revision, shown) = {
            let field = state.right_panel();
            let panels = field.peek();
            (panels.panels.user_revision(&key), panels.panels.active(&key).cloned())
        };
        let observation = logic::observe_user_choice(memory.observation.as_ref(), &key, runs.running.as_deref(), revision);
        let (previous_running, previous_target) = (observation.running_run_id.clone(), observation.target_key.clone());
        memory.observation = Some(observation.clone());
        let open = |surface: Surface| state.right_panel().write().panels.open_proactive(&key, surface, observation.user_action_revision);

        // Following the selected linked pull request opens no unrelated panel, so it works with proactive panels off.
        let previous_link = memory.pull_request.take().filter(|(seen, _)| *seen == key).map(|(_, linked)| linked);
        memory.pull_request = Some((key.clone(), links.linked.clone()));
        let follow = previous_link.as_ref().is_some_and(|previous| logic::should_retarget_pull_request_panel(previous.as_ref(), links.linked.as_ref(), shown.as_ref()));
        if let (true, Some(linked)) = (follow, links.linked.as_ref()) {
            open(linked_target(&links, linked));
        }
        if !hydrated {
            return;
        }

        let surface_available = supports_thread_links && links.count > 0;
        let eligible_link = enabled && logic::should_open_pull_request(previous_target.as_ref().and_then(|key| key.as_deref()), links.key.as_deref());
        let defer_link = eligible_link && !config_known;
        if let Some(remembered) = memory.observation.as_mut() {
            remembered.target_key = Some(if defer_link { previous_target.clone().flatten() } else { links.key.clone() });
        }
        if eligible_link && config_known {
            if surface_available && (links.count > 1 || links.linked.is_none() || !supports_pull_requests) {
                open(Surface::PullRequests);
            } else if !follow && supports_pull_requests {
                if let Some(linked) = links.linked.as_ref() {
                    open(linked_target(&links, linked));
                }
            }
        }
        // Nothing of a turn is known before the thread has arrived.
        if !turns.loaded {
            return;
        }

        let completed = runs.latest.as_ref().is_some_and(|(_, completed)| *completed);
        let newly_completed = logic::should_open_turn_diff(previous_running.as_ref(), runs.running.as_deref(), runs.settled(), completed).then(|| runs.settled()).flatten();
        // A linked pull request takes the place of the diff.
        let eligible_completion = enabled && newly_completed.is_some() && !(links.key.is_some() && (!config_known || supports_pull_requests || surface_available));
        let action = match newly_completed.filter(|_| eligible_completion) {
            Some(run_id) => {
                let checkpoint = turns_checkpoint(&turns, run_id);
                // A checkpoint is a git ref, so one that exists says the folder is a repository.
                let is_repo = checkpoint.map(|_| true);
                logic::resolve_turn_diff_action(checkpoint.map(|(status, files, lines)| (status, files, lines)), is_repo, shown.as_ref())
            }
            None => DiffAction::Ignore,
        };
        // Keep first-entry eligibility while the checkpoint or capabilities load.
        if let Some(remembered) = memory.observation.as_mut() {
            remembered.running_run_id = if action == DiffAction::Defer || defer_link { previous_running.clone() } else { Some(runs.running.clone()) };
        }
        if action != DiffAction::Open {
            return;
        }
        if open(Surface::Diff) {
            // The diff starts on the branch's changes, as the add button's does.
            state.diff().write().selections.insert((environment.clone(), thread.clone()), (checkpoints::Scope::Changes, None));
        }
    });
    rsx! {}
}

/// The part of the turn's checkpoint the rule reads: its state, how many files it changed, and how many lines.
fn turns_checkpoint(turns: &Turns, run_id: &str) -> Option<(Checkpoint, usize, u64)> {
    let turn = turns.turns.iter().find(|turn| turn.run_id == run_id)?;
    let status = match turn.status {
        Status::Ready => Checkpoint::Ready,
        Status::Missing => Checkpoint::Missing,
        Status::Error | Status::Stale => Checkpoint::Other,
    };
    let lines = turn.files.iter().map(|file| file.additions.unwrap_or(0) + file.deletions.unwrap_or(0)).sum();
    Some((status, turn.files.len(), lines))
}
