//! When the app opens a panel on its own (setting "Proactive panels"): a thread's linked pull requests when they first appear,
//! and the diff of a turn that has just finished and changed enough. Port of the rules in `ChatView.logic.ts`
//! (`shouldOpenProactivePullRequest`, `observeProactivePanelUserChoice`, `shouldRetargetThreadPullRequestPanel`,
//! `shouldOpenProactiveTurnDiff`, `resolveProactiveTurnDiffAction`). A choice of the person's always wins: every proactive
//! opening names the count of the person's panel choices it looked at, and `Panels::open_proactive` refuses it if that moved.
use crate::right_panel::Surface;

/// The change request a thread is linked to (`ThreadLinkedPullRequest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    pub project_id: String,
    pub repository: String,
    pub number: u64,
}

/// `shouldOpenProactivePullRequest`: the set of linked pull requests is new to this thread's observation.
pub fn should_open_pull_request(previous_target_key: Option<&str>, target_key: Option<&str>) -> bool {
    target_key.is_some() && target_key != previous_target_key
}

/// What the effect remembers of a thread between runs. An outer `None` is "not observed yet" (the original's `undefined`), which
/// is different from an observed "no run" / "no target" (`null`): the first run in a thread may open the diff of a turn that
/// finished before the thread was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub thread_key: String,
    pub running_run_id: Option<Option<String>>,
    pub target_key: Option<Option<String>>,
    pub user_action_run_id: Option<String>,
    pub user_action_revision: u64,
}

/// `observeProactivePanelUserChoice`: captures the person's panel choices before loading or metadata writes can defer an
/// opening. Entering a thread, or the start of a new run, reads the count again; anything the person did before that is theirs.
pub fn observe_user_choice(previous: Option<&Observation>, thread_key: &str, running_run_id: Option<&str>, user_action_revision: u64) -> Observation {
    let same_thread = previous.is_some_and(|previous| previous.thread_key == thread_key);
    let previous = previous.filter(|_| same_thread);
    let new_turn = previous.is_some_and(|previous| running_run_id.is_some() && running_run_id != previous.user_action_run_id.as_deref());
    Observation {
        thread_key: thread_key.to_owned(),
        running_run_id: previous.and_then(|previous| previous.running_run_id.clone()),
        target_key: previous.and_then(|previous| previous.target_key.clone()),
        user_action_run_id: running_run_id.map(str::to_owned).or_else(|| previous.and_then(|previous| previous.user_action_run_id.clone())),
        user_action_revision: match previous {
            Some(previous) if !new_turn => previous.user_action_revision,
            _ => user_action_revision,
        },
    }
}

/// `shouldRetargetThreadPullRequestPanel`: follow a changed link only when the panel still shows the previous linked pull request.
pub fn should_retarget_pull_request_panel(previous: Option<&Linked>, current: Option<&Linked>, shown: Option<&Surface>) -> bool {
    let (Some(previous), Some(current), Some(Surface::PullRequest(target))) = (previous, current, shown) else { return false };
    let previous_repository = previous.repository.to_lowercase();
    (previous.project_id != current.project_id || previous_repository != current.repository.to_lowercase() || previous.number != current.number)
        && target.project_id == previous.project_id
        && target.repository.to_lowercase() == previous_repository
        && target.number == previous.number
}

/// `shouldOpenProactiveTurnDiff`: the run that settled is the one that was running (or none was seen), it completed, and nothing runs now.
pub fn should_open_turn_diff(previous_running_run_id: Option<&Option<String>>, running_run_id: Option<&str>, settled_run_id: Option<&str>, completed: bool) -> bool {
    running_run_id.is_none()
        && completed
        && settled_run_id.is_some()
        && match previous_running_run_id {
            None => true,
            Some(previous) => settled_run_id == previous.as_deref(),
        }
}

/// What the effect does with the turn that just finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffAction {
    /// Look again when more is known (the checkpoint, the repository).
    Defer,
    Ignore,
    Open,
}

/// The part of a turn's checkpoint the rule reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    Ready,
    Missing,
    /// Present but not ready (error, stale).
    Other,
}

/// A turn is worth a panel when it touched this many files, or this many lines.
pub const MIN_FILES: usize = 3;
pub const MIN_LINES: u64 = 50;

/// `resolveProactiveTurnDiffAction`.
pub fn resolve_turn_diff_action(checkpoint: Option<(Checkpoint, usize, u64)>, is_git_repo: Option<bool>, shown: Option<&Surface>) -> DiffAction {
    // An open diff already shows the work; reopening it would reset the chosen scope.
    if matches!(shown, Some(Surface::PullRequest(_) | Surface::Diff)) {
        return DiffAction::Ignore;
    }
    let Some((status, files, changed_lines)) = checkpoint else { return DiffAction::Defer };
    if status == Checkpoint::Missing {
        return DiffAction::Defer;
    }
    let Some(is_git_repo) = is_git_repo else { return DiffAction::Defer };
    if !is_git_repo || status != Checkpoint::Ready || files == 0 {
        return DiffAction::Ignore;
    }
    if files >= MIN_FILES || changed_lines >= MIN_LINES { DiffAction::Open } else { DiffAction::Ignore }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::right_panel::PullRequestTarget;

    fn linked(number: u64) -> Linked {
        Linked { project_id: "p".into(), repository: "Org/Repo".into(), number }
    }
    fn surface(number: u64) -> Surface {
        Surface::PullRequest(PullRequestTarget { environment_id: None, project_id: "p".into(), host: None, repository: "org/repo".into(), number, url: None })
    }

    #[test]
    fn a_set_of_links_opens_once_and_again_only_when_it_changes() {
        assert!(should_open_pull_request(None, Some("[1]")));
        assert!(!should_open_pull_request(Some("[1]"), Some("[1]")));
        assert!(should_open_pull_request(Some("[1]"), Some("[1,2]")));
        assert!(!should_open_pull_request(Some("[1]"), None));
        assert!(!should_open_pull_request(None, None));
    }

    #[test]
    fn a_thread_is_observed_afresh_when_entered_and_the_count_is_read_again_at_a_new_run() {
        let first = observe_user_choice(None, "t1", None, 4);
        assert_eq!((first.running_run_id.clone(), first.target_key.clone(), first.user_action_revision), (None, None, 4));
        // The same thread keeps what was seen, and the count from when it was entered.
        let mut seen = first.clone();
        seen.running_run_id = Some(None);
        seen.target_key = Some(Some("[1]".into()));
        let again = observe_user_choice(Some(&seen), "t1", None, 9);
        assert_eq!((again.running_run_id.clone(), again.target_key.clone(), again.user_action_revision), (Some(None), Some(Some("[1]".into())), 4));
        // A run that starts is a new turn: choices made before it are read again.
        let running = observe_user_choice(Some(&seen), "t1", Some("r1"), 9);
        assert_eq!(running.user_action_revision, 9);
        assert_eq!(running.user_action_run_id.as_deref(), Some("r1"));
        // The same run later (or none running) keeps the count it started with.
        let later = observe_user_choice(Some(&running), "t1", Some("r1"), 12);
        assert_eq!(later.user_action_revision, 9);
        let after = observe_user_choice(Some(&running), "t1", None, 12);
        assert_eq!((after.user_action_revision, after.user_action_run_id.as_deref()), (9, Some("r1")));
        // Another thread starts over.
        let other = observe_user_choice(Some(&running), "t2", Some("r9"), 30);
        assert_eq!((other.running_run_id, other.target_key, other.user_action_revision), (None, None, 30));
    }

    #[test]
    fn the_panel_follows_a_changed_link_only_while_it_shows_the_previous_one() {
        assert!(should_retarget_pull_request_panel(Some(&linked(1)), Some(&linked(2)), Some(&surface(1))));
        assert!(!should_retarget_pull_request_panel(Some(&linked(1)), Some(&linked(2)), Some(&surface(5))), "the person moved to another one");
        assert!(!should_retarget_pull_request_panel(Some(&linked(1)), Some(&linked(1)), Some(&surface(1))), "nothing changed");
        assert!(!should_retarget_pull_request_panel(Some(&linked(1)), Some(&linked(2)), Some(&Surface::Diff)));
        assert!(!should_retarget_pull_request_panel(None, Some(&linked(2)), Some(&surface(1))));
        assert!(!should_retarget_pull_request_panel(Some(&linked(1)), None, Some(&surface(1))));
    }

    #[test]
    fn a_finished_turn_opens_the_diff_once_whether_it_was_watched_running_or_found_finished() {
        let none = None;
        let was = Some(Some("r1".to_owned()));
        assert!(should_open_turn_diff(was.as_ref(), None, Some("r1"), true), "watched running, then settled");
        assert!(should_open_turn_diff(none.as_ref(), None, Some("r1"), true), "found finished on the first look");
        assert!(!should_open_turn_diff(was.as_ref(), None, Some("r0"), true), "not the run that was watched");
        assert!(!should_open_turn_diff(was.as_ref(), Some("r2"), Some("r1"), true), "another run is running");
        assert!(!should_open_turn_diff(was.as_ref(), None, Some("r1"), false), "it did not complete");
        assert!(!should_open_turn_diff(was.as_ref(), None, None, true));
        let seen_idle = Some(None);
        assert!(!should_open_turn_diff(seen_idle.as_ref(), None, Some("r1"), true), "finished before this thread was last looked at");
    }

    #[test]
    fn a_turn_opens_the_diff_when_it_touched_three_files_or_fifty_lines() {
        let ready = |files, lines| Some((Checkpoint::Ready, files, lines));
        assert_eq!(resolve_turn_diff_action(ready(3, 3), Some(true), None), DiffAction::Open);
        assert_eq!(resolve_turn_diff_action(ready(1, 50), Some(true), None), DiffAction::Open);
        assert_eq!(resolve_turn_diff_action(ready(2, 49), Some(true), None), DiffAction::Ignore);
        assert_eq!(resolve_turn_diff_action(ready(0, 0), Some(true), None), DiffAction::Ignore);
        assert_eq!(resolve_turn_diff_action(Some((Checkpoint::Other, 9, 900)), Some(true), None), DiffAction::Ignore);
        assert_eq!(resolve_turn_diff_action(ready(9, 900), Some(false), None), DiffAction::Ignore, "not a repository");
    }

    #[test]
    fn what_is_not_known_yet_defers_and_an_open_diff_or_pull_request_is_left_alone() {
        assert_eq!(resolve_turn_diff_action(None, Some(true), None), DiffAction::Defer);
        assert_eq!(resolve_turn_diff_action(Some((Checkpoint::Missing, 9, 900)), Some(true), None), DiffAction::Defer);
        assert_eq!(resolve_turn_diff_action(Some((Checkpoint::Ready, 9, 900)), None, None), DiffAction::Defer);
        assert_eq!(resolve_turn_diff_action(None, None, Some(&Surface::Diff)), DiffAction::Ignore);
        assert_eq!(resolve_turn_diff_action(Some((Checkpoint::Ready, 9, 900)), Some(true), Some(&surface(1))), DiffAction::Ignore);
        assert_eq!(resolve_turn_diff_action(Some((Checkpoint::Ready, 9, 900)), Some(true), Some(&Surface::Files)), DiffAction::Open);
    }
}
