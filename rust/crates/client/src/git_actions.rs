//! The rules behind the git actions control: which action the primary button offers for a status,
//! what the menu lists, when the default branch asks first, and how the progress events of
//! `git.runStackedAction` add up. Port of `GitActionsControl.logic.ts` and the wording of
//! `GitActionsControl.tsx`; the tests replay what the original answers for a matrix of statuses.
use crate::vcs::Status;
use serde_json::Value;

pub const RUN_STACKED_ACTION: &str = "git.runStackedAction";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Commit,
    Push,
    CreatePr,
    CommitPush,
    CommitPushPr,
}
impl Action {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Push => "push",
            Self::CreatePr => "create_pr",
            Self::CommitPush => "commit_push",
            Self::CommitPushPr => "commit_push_pr",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "commit" => Self::Commit,
            "push" => Self::Push,
            "create_pr" => Self::CreatePr,
            "commit_push" => Self::CommitPush,
            "commit_push_pr" => Self::CommitPushPr,
            _ => return None,
        })
    }
    /// Whether the action commits (when there is anything to commit).
    pub fn commits(self) -> bool {
        matches!(self, Self::Commit | Self::CommitPush | Self::CommitPushPr)
    }
}

/// What the host calls a change request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terms {
    pub short: &'static str,
    pub singular: &'static str,
}
pub const PULL_REQUEST: Terms = Terms { short: "PR", singular: "pull request" };
pub const MERGE_REQUEST: Terms = Terms { short: "MR", singular: "merge request" };
pub fn terms(provider_kind: Option<&str>) -> Terms {
    if provider_kind == Some("gitlab") { MERGE_REQUEST } else { PULL_REQUEST }
}
fn terms_of(status: &Status) -> Terms {
    terms(status.local.provider_kind.as_deref())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickKind {
    RunAction,
    RunPull,
    OpenPublish,
    ShowHint,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quick {
    pub label: String,
    pub disabled: bool,
    pub kind: QuickKind,
    pub action: Option<Action>,
    pub hint: Option<String>,
}
fn run(label: impl Into<String>, action: Action) -> Quick {
    Quick { label: label.into(), disabled: false, kind: QuickKind::RunAction, action: Some(action), hint: None }
}
fn hint(label: &str, text: impl Into<String>) -> Quick {
    Quick { label: label.into(), disabled: true, kind: QuickKind::ShowHint, action: None, hint: Some(text.into()) }
}

fn has_open_pr(status: &Status) -> bool {
    status.remote.pr.as_ref().is_some_and(|pr| pr.open())
}
fn delta(status: &Status) -> bool {
    status.remote.ahead_of_default.unwrap_or(status.remote.ahead) > 0
}

/// `resolveQuickAction`: the primary button.
pub fn quick_action(status: Option<&Status>, busy: bool) -> Quick {
    if busy {
        return hint("Commit", "Git action in progress.");
    }
    let Some(status) = status else { return hint("Commit", "Git status is unavailable.") };
    let (local, remote) = (&status.local, &status.remote);
    let (has_branch, changes, open_pr) = (local.ref_name.is_some(), local.has_working_tree_changes, has_open_pr(status));
    let (ahead, behind) = (remote.ahead > 0, remote.behind > 0);
    let (is_default, primary_remote) = (local.is_default_ref, local.has_primary_remote);
    let t = terms_of(status);
    if !has_branch {
        return hint("Commit", format!("Create and checkout a ref before pushing or opening a {}.", t.singular));
    }
    if changes {
        if !remote.has_upstream && !primary_remote {
            return run("Commit", Action::Commit);
        }
        if open_pr || is_default {
            return run("Commit & push", Action::CommitPush);
        }
        return run(format!("Commit, push & {}", t.short), Action::CommitPushPr);
    }
    if !remote.has_upstream {
        if !primary_remote {
            return Quick { label: "Publish repository".into(), disabled: false, kind: QuickKind::OpenPublish, action: None, hint: None };
        }
        if !ahead {
            return if open_pr { hint("Commit", "Branch is up to date. No action needed.") } else { hint("Push", "No local commits to push.") };
        }
        if open_pr || is_default {
            return run("Push", if is_default { Action::CommitPush } else { Action::Push });
        }
        return run(format!("Push & create {}", t.short), Action::CreatePr);
    }
    if ahead && behind {
        return hint("Sync ref", crate::vcs::DIVERGED_HINT);
    }
    if behind {
        return Quick { label: "Pull".into(), disabled: false, kind: QuickKind::RunPull, action: None, hint: None };
    }
    if ahead {
        if open_pr || is_default {
            return run("Push", if is_default { Action::CommitPush } else { Action::Push });
        }
        return run(format!("Push & create {}", t.short), Action::CreatePr);
    }
    if open_pr && remote.has_upstream {
        return hint("Commit", "Branch is up to date. No action needed.");
    }
    if delta(status) && !is_default {
        return run(format!("Create {}", t.short), Action::CreatePr);
    }
    hint("Commit", "Branch is up to date. No action needed.")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuId {
    Commit,
    Push,
    Pr,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub id: MenuId,
    pub label: String,
    pub disabled: bool,
}

/// `buildMenuItems`.
pub fn menu_items(status: Option<&Status>, busy: bool) -> Vec<MenuItem> {
    let Some(status) = status else { return Vec::new() };
    let (local, remote) = (&status.local, &status.remote);
    let primary_remote = local.has_primary_remote;
    let (has_branch, changes, open_pr, behind) = (local.ref_name.is_some(), local.has_working_tree_changes, has_open_pr(status), remote.behind > 0);
    let can_push_without_upstream = primary_remote && !remote.has_upstream;
    let can_commit = !busy && changes;
    let can_push = !busy && has_branch && !behind && remote.ahead > 0 && (remote.has_upstream || can_push_without_upstream);
    let can_pr = !busy && has_branch && !changes && !open_pr && delta(status) && !behind && (remote.has_upstream || can_push_without_upstream);
    let commit = MenuItem { id: MenuId::Commit, label: "Commit".into(), disabled: !can_commit };
    if !primary_remote {
        return vec![commit];
    }
    let push = MenuItem { id: MenuId::Push, label: "Push".into(), disabled: !can_push };
    if open_pr {
        return vec![commit, push];
    }
    vec![commit, push, MenuItem { id: MenuId::Pr, label: format!("Create {}", terms_of(status).short), disabled: !can_pr }]
}

/// Why a disabled menu entry is disabled, in the original's words.
pub fn disabled_reason(item: &MenuItem, status: Option<&Status>, busy: bool) -> Option<String> {
    if !item.disabled {
        return None;
    }
    if busy {
        return Some("Git action in progress.".into());
    }
    let status = status?;
    let (local, remote) = (&status.local, &status.remote);
    let (has_branch, changes, ahead, behind) = (local.ref_name.is_some(), local.has_working_tree_changes, remote.ahead > 0, remote.behind > 0);
    let singular = terms_of(status).singular;
    Some(match item.id {
        MenuId::Commit if !changes => "Worktree is clean. Make changes before committing.".into(),
        MenuId::Commit => "Commit is currently unavailable.".into(),
        MenuId::Push if !has_branch => "Detached HEAD: check out a branch before pushing.".into(),
        MenuId::Push if changes => "Commit or stash local changes before pushing.".into(),
        MenuId::Push if behind => "Branch is behind upstream. Pull/rebase before pushing.".into(),
        MenuId::Push if !remote.has_upstream && !local.has_primary_remote => "Add an \"origin\" remote before pushing.".into(),
        MenuId::Push if !ahead => "No local commits to push.".into(),
        MenuId::Push => "Push is currently unavailable.".into(),
        MenuId::Pr if !has_branch => format!("Detached HEAD: check out a branch before creating a {singular}."),
        MenuId::Pr if changes => format!("Commit local changes before creating a {singular}."),
        MenuId::Pr if !remote.has_upstream && !local.has_primary_remote => format!("Add an \"origin\" remote before creating a {singular}."),
        MenuId::Pr if !ahead => format!("No local commits to include in a {singular}."),
        MenuId::Pr if behind => format!("Branch is behind upstream. Pull/rebase before creating a {singular}."),
        MenuId::Pr => format!("Create {singular} is currently unavailable."),
    })
}

/// `requiresDefaultBranchConfirmation`.
pub fn requires_default_confirmation(action: Action, is_default_ref: bool) -> bool {
    is_default_ref && action != Action::Commit
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultBranchCopy {
    pub title: String,
    pub description: String,
    pub continue_label: String,
}

/// `resolveDefaultBranchActionDialogCopy`: the wording of the question asked before pushing to
/// the default branch.
pub fn default_branch_copy(action: Action, branch: &str, includes_commit: bool, terms: Terms) -> DefaultBranchCopy {
    let suffix = format!(" on \"{branch}\". You can continue on this ref or create a feature ref and run the same action there.");
    if matches!(action, Action::Push | Action::CommitPush) {
        return if includes_commit {
            DefaultBranchCopy { title: "Commit & push to default ref?".into(), description: format!("This action will commit and push changes{suffix}"), continue_label: format!("Commit & push to {branch}") }
        } else {
            DefaultBranchCopy { title: "Push to default ref?".into(), description: format!("This action will push local commits{suffix}"), continue_label: format!("Push to {branch}") }
        };
    }
    if includes_commit {
        DefaultBranchCopy {
            title: format!("Commit, push & create {} from default ref?", terms.short),
            description: format!("This action will commit, push, and create a {}{suffix}", terms.singular),
            continue_label: format!("Commit, push & create {}", terms.short),
        }
    } else {
        DefaultBranchCopy {
            title: format!("Push & create {} from default ref?", terms.short),
            description: format!("This action will push local commits and create a {}{suffix}", terms.singular),
            continue_label: format!("Push & create {}", terms.short),
        }
    }
}

/// `resolveThreadBranchUpdate`: the branch an action created, which the thread should now name.
pub fn created_branch(result: &Value) -> Option<String> {
    (result["branch"]["status"] == "created").then(|| result["branch"]["name"].as_str().filter(|name| !name.is_empty()).map(str::to_owned)).flatten()
}

/// `buildGitActionProgressStages`: the steps an action will take, for the line shown before the
/// server has said which it is on.
pub fn progress_stages(action: Action, custom_message: bool, has_changes: bool, push_target: Option<&str>, feature_branch: bool, push_before_pr: bool, terms: Terms) -> Vec<String> {
    let branch: Vec<String> = if feature_branch { vec!["Preparing feature ref...".into()] } else { Vec::new() };
    let push = push_target.map_or_else(|| "Pushing...".to_owned(), |target| format!("Pushing to {target}..."));
    let pr = vec![format!("Preparing {}...", terms.short), format!("Generating {} content...", terms.short), format!("Creating {}...", terms.singular)];
    match action {
        Action::Push => return vec![push],
        Action::CreatePr => return if push_before_pr { std::iter::once(push).chain(pr).collect() } else { pr },
        _ => {}
    }
    let commit: Vec<String> = if action == Action::Commit || has_changes {
        if custom_message { vec!["Committing...".into()] } else { vec!["Generating commit message...".into(), "Committing...".into()] }
    } else {
        Vec::new()
    };
    let mut stages = [branch, commit].concat();
    if action == Action::Commit {
        return stages;
    }
    stages.push(push);
    if action == Action::CommitPushPr {
        stages.extend(pr);
    }
    stages
}

/// What the finished action offers next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cta {
    None,
    OpenPr { label: String, url: String },
    Run { label: String, action: Action },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub title: String,
    pub description: Option<String>,
    pub cta: Cta,
    /// The branch the action created, for the thread to follow.
    pub branch: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Finished(Finished),
    Failed { phase: Option<String>, message: String, generation: Option<Generation> },
}

/// A text generation step that failed, as the stream's failure names it: the operation that was
/// writing (`generateCommitMessage`) and what went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub operation: String,
    pub detail: String,
}
/// The generation failure a stream ended with, if it was one: `TextGenerationError`.
pub fn generation_of(cause: &Value) -> Option<Generation> {
    let error = cause.as_array().and_then(|causes| causes.iter().find_map(|cause| cause.get("error"))).unwrap_or(cause);
    (error["_tag"] == "TextGenerationError").then(|| Generation { operation: error["operation"].as_str().unwrap_or_default().to_owned(), detail: error["detail"].as_str().unwrap_or_default().to_owned() })
}

/// One run of `git.runStackedAction`, as its events tell it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Progress {
    pub action_id: String,
    pub action: Option<Action>,
    /// The step the server says it is on.
    pub label: Option<String>,
    /// The hook running now.
    pub hook: Option<String>,
    pub output: Vec<String>,
    pub outcome: Option<Outcome>,
}
/// Output kept for display; a noisy hook cannot grow the page without bound.
const OUTPUT_LINES: usize = 200;

impl Progress {
    pub fn started(action_id: &str, action: Action) -> Self {
        Self { action_id: action_id.to_owned(), action: Some(action), ..Self::default() }
    }

    /// Applies one event; events of another action are ignored. Returns whether anything changed.
    pub fn apply(&mut self, event: &Value) -> bool {
        if event["actionId"].as_str() != Some(self.action_id.as_str()) {
            return false;
        }
        match event["kind"].as_str() {
            Some("phase_started") => self.label = event["label"].as_str().map(str::to_owned),
            Some("hook_started") => self.hook = event["hookName"].as_str().map(str::to_owned),
            Some("hook_output") => {
                for line in event["text"].as_str().unwrap_or_default().lines().map(str::trim_end).filter(|line| !line.trim().is_empty()) {
                    self.output.push(line.to_owned());
                }
                if self.output.len() > OUTPUT_LINES {
                    self.output.drain(..self.output.len() - OUTPUT_LINES);
                }
            }
            Some("hook_finished") => self.hook = None,
            Some("action_finished") => {
                let toast = &event["result"]["toast"];
                let cta = match toast["cta"]["kind"].as_str() {
                    Some("open_pr") => Cta::OpenPr { label: toast["cta"]["label"].as_str().unwrap_or_default().to_owned(), url: toast["cta"]["url"].as_str().unwrap_or_default().to_owned() },
                    Some("run_action") => match toast["cta"]["action"]["kind"].as_str().and_then(Action::parse) {
                        Some(action) => Cta::Run { label: toast["cta"]["label"].as_str().unwrap_or_default().to_owned(), action },
                        None => Cta::None,
                    },
                    _ => Cta::None,
                };
                self.outcome = Some(Outcome::Finished(Finished { title: toast["title"].as_str().unwrap_or("Done").to_owned(), description: toast["description"].as_str().map(str::to_owned), cta, branch: created_branch(&event["result"]) }));
                self.hook = None;
            }
            Some("action_failed") => {
                self.outcome = Some(Outcome::Failed { phase: event["phase"].as_str().map(str::to_owned), message: event["message"].as_str().unwrap_or("An error occurred.").to_owned(), generation: None });
                self.hook = None;
            }
            Some("action_started") => {}
            _ => return false,
        }
        true
    }

    /// The stream ended with this error. A text generation failure names its operation and what
    /// went wrong, which the `action_failed` event before it only worded.
    pub fn apply_failure(&mut self, cause: &Value) {
        let Some(generation) = generation_of(cause) else { return };
        match &mut self.outcome {
            Some(Outcome::Failed { generation: slot, .. }) => *slot = Some(generation),
            _ => self.outcome = Some(Outcome::Failed { phase: None, message: generation.detail.clone(), generation: Some(generation) }),
        }
        self.hook = None;
    }

    /// `resolveGitActionProgressPresentation`: the status line, and the last line of output.
    pub fn status(&self) -> String {
        match self.label.as_deref().map(str::trim).filter(|label| !label.is_empty() && *label != "Running source control action") {
            Some(label) => label.to_owned(),
            None => "Starting source control action...".into(),
        }
    }
    pub fn last_output(&self) -> Option<&str> {
        self.output.last().map(|line| line.trim()).filter(|line| !line.is_empty())
    }
}

/// What a failed generation step was writing, for the sentence that reports it.
pub fn generation_subject(operation: &str, terms: Terms) -> String {
    match operation {
        "generateCommitMessage" => "commit message".into(),
        "generatePrContent" => format!("{} title and description", terms.singular),
        "generateBranchName" => "branch name".into(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(row: &Value) -> Status {
        let s = &row["status"];
        let local = json!({"isRepo":true,"hasPrimaryRemote":s["hasPrimaryRemote"],"isDefaultRef":s["isDefaultRef"],"refName":s["refName"],"hasWorkingTreeChanges":s["hasWorkingTreeChanges"],"workingTree":{"files":[],"insertions":0,"deletions":0},
            "sourceControlProvider": s["kind"].as_str().map(|kind| json!({"kind":kind,"name":kind,"baseUrl":""}))});
        let remote = json!({"hasUpstream":s["hasUpstream"],"aheadCount":s["aheadCount"],"behindCount":s["behindCount"],"aheadOfDefaultCount":s["aheadOfDefaultCount"],
            "pr": s["pr"].as_str().map(|state| json!({"number":1,"title":"t","url":"u","baseRef":"main","headRef":"f","state":state}))});
        Status::apply(None, &json!({"_tag":"snapshot","local":local,"remote":remote})).unwrap()
    }
    fn quick_of(quick: &Quick) -> Value {
        let kind = match quick.kind {
            QuickKind::RunAction => "run_action",
            QuickKind::RunPull => "run_pull",
            QuickKind::OpenPublish => "open_publish",
            QuickKind::ShowHint => "show_hint",
        };
        json!({"label":quick.label,"disabled":quick.disabled,"kind":kind,"action":quick.action.map(Action::wire),"hint":quick.hint})
    }
    fn menu_of(items: &[MenuItem]) -> Value {
        items.iter().map(|item| json!([match item.id { MenuId::Commit => "commit", MenuId::Push => "push", MenuId::Pr => "pr" }, item.label, item.disabled])).collect()
    }
    fn fixtures() -> Vec<Value> {
        serde_json::from_str(include_str!("../tests/fixtures/git_actions.json")).unwrap()
    }

    #[test]
    fn the_primary_action_and_the_menu_follow_the_original_for_every_status() {
        let mut seen = 0;
        for row in fixtures() {
            match row["type"].as_str().unwrap() {
                "status" => {
                    let status = status(&row);
                    assert_eq!(quick_of(&quick_action(Some(&status), false)), row["quick"], "{}", row["status"]);
                    assert_eq!(menu_of(&menu_items(Some(&status), false)), row["menu"], "{}", row["status"]);
                    seen += 1;
                }
                "busy" => {
                    assert_eq!(quick_of(&quick_action(None, true))["hint"], row["quick"]["hint"]);
                    assert_eq!(quick_of(&quick_action(None, true))["disabled"], true);
                }
                "absent" => {
                    assert_eq!(quick_action(None, false).hint, row["quick"]["hint"].as_str().map(str::to_owned));
                    assert!(menu_items(None, false).is_empty() && row["menu"] == json!([]));
                }
                _ => {}
            }
        }
        assert!(seen > 1700);
    }

    #[test]
    fn a_busy_menu_disables_every_entry() {
        let row = fixtures().into_iter().find(|row| row["type"] == "busy").unwrap();
        assert!(row["menu"].as_array().unwrap().iter().all(|item| item[2] == true));
    }

    #[test]
    fn default_branch_questions_and_their_wording_follow_the_original() {
        let mut checked = 0;
        for row in fixtures() {
            match row["type"].as_str().unwrap() {
                "confirm" => {
                    let action = Action::parse(row["action"].as_str().unwrap()).unwrap();
                    assert_eq!(requires_default_confirmation(action, row["isDefault"].as_bool().unwrap()), row["answer"]);
                }
                "copy" => {
                    let action = Action::parse(row["action"].as_str().unwrap()).unwrap();
                    let t = terms(row["terminology"].as_str());
                    let copy = default_branch_copy(action, "main", row["includesCommit"].as_bool().unwrap(), t);
                    assert_eq!(json!({"title":copy.title,"description":copy.description,"continueLabel":copy.continue_label}), row["copy"]);
                    checked += 1;
                }
                "branch" => assert_eq!(created_branch(&row["result"]), row["update"]["branch"].as_str().map(str::to_owned)),
                "stages" => {
                    let action = Action::parse(row["action"].as_str().unwrap()).unwrap();
                    let stages = progress_stages(action, row["hasCustomCommitMessage"].as_bool().unwrap(), true, Some("origin/main"), row["featureBranch"].as_bool().unwrap(), true, PULL_REQUEST);
                    assert_eq!(json!(stages), row["stages"], "{row}");
                }
                _ => {}
            }
        }
        assert_eq!(checked, 16);
    }

    #[test]
    fn disabled_menu_entries_say_why() {
        let base = |changes: bool, upstream: bool, ahead: u64, behind: u64, remote: bool, branch: Option<&str>| {
            Status::apply(None, &json!({"_tag":"snapshot","local":{"isRepo":true,"hasPrimaryRemote":remote,"isDefaultRef":false,"refName":branch,"hasWorkingTreeChanges":changes,"workingTree":{"files":[],"insertions":0,"deletions":0}},"remote":{"hasUpstream":upstream,"aheadCount":ahead,"behindCount":behind,"pr":null}})).unwrap()
        };
        let reason = |status: &Status, index: usize| {
            let items = menu_items(Some(status), false);
            disabled_reason(&items[index], Some(status), false)
        };
        let clean = base(false, true, 0, 0, true, Some("f"));
        assert_eq!(reason(&clean, 0).as_deref(), Some("Worktree is clean. Make changes before committing."));
        assert_eq!(reason(&clean, 1).as_deref(), Some("No local commits to push."));
        assert_eq!(reason(&clean, 2).as_deref(), Some("No local commits to include in a pull request."));
        assert_eq!(reason(&base(true, true, 0, 0, true, Some("f")), 1).as_deref(), Some("Commit or stash local changes before pushing."));
        assert_eq!(reason(&base(false, true, 1, 1, true, Some("f")), 1).as_deref(), Some("Branch is behind upstream. Pull/rebase before pushing."));
        assert_eq!(reason(&base(false, true, 1, 0, true, None), 1).as_deref(), Some("Detached HEAD: check out a branch before pushing."));
        assert_eq!(reason(&base(false, true, 1, 0, true, None), 2).as_deref(), Some("Detached HEAD: check out a branch before creating a pull request."));
        assert_eq!(reason(&base(false, false, 0, 0, true, Some("f")), 2).as_deref(), Some("No local commits to include in a pull request."));
        assert_eq!(disabled_reason(&MenuItem { id: MenuId::Push, label: "Push".into(), disabled: true }, Some(&clean), true).as_deref(), Some("Git action in progress."));
        assert_eq!(disabled_reason(&MenuItem { id: MenuId::Push, label: "Push".into(), disabled: false }, Some(&clean), false), None);
    }

    #[test]
    fn events_add_up_to_a_status_line_output_and_an_outcome() {
        let mut progress = Progress::started("a1", Action::CommitPushPr);
        assert_eq!(progress.status(), "Starting source control action...");
        assert!(!progress.apply(&json!({"actionId":"other","kind":"phase_started","label":"x"})), "another action's events are not ours");
        progress.apply(&json!({"actionId":"a1","kind":"action_started","phases":["commit","push"]}));
        progress.apply(&json!({"actionId":"a1","kind":"phase_started","phase":"commit","label":"Committing..."}));
        assert_eq!(progress.status(), "Committing...");
        progress.apply(&json!({"actionId":"a1","kind":"hook_started","hookName":"pre-commit"}));
        progress.apply(&json!({"actionId":"a1","kind":"hook_output","hookName":"pre-commit","stream":"stdout","text":"lint ok\n\ntests ok\n"}));
        assert_eq!((progress.hook.as_deref(), progress.last_output()), (Some("pre-commit"), Some("tests ok")));
        progress.apply(&json!({"actionId":"a1","kind":"hook_finished","hookName":"pre-commit"}));
        assert_eq!(progress.hook, None);
        progress.apply(&json!({"actionId":"a1","kind":"action_finished","result":{"branch":{"status":"created","name":"f/x"},"toast":{"title":"Committed abc","description":"Add x","cta":{"kind":"run_action","label":"Push","action":{"kind":"push"}}}}}));
        assert_eq!(progress.outcome, Some(Outcome::Finished(Finished { title: "Committed abc".into(), description: Some("Add x".into()), cta: Cta::Run { label: "Push".into(), action: Action::Push }, branch: Some("f/x".into()) })));
        let mut failed = Progress::started("a2", Action::Push);
        failed.apply(&json!({"actionId":"a2","kind":"action_failed","phase":"push","message":"rejected"}));
        assert_eq!(failed.outcome, Some(Outcome::Failed { phase: Some("push".into()), message: "rejected".into(), generation: None }));
        let mut noisy = Progress::started("a3", Action::Commit);
        noisy.apply(&json!({"actionId":"a3","kind":"hook_output","text":(0..500).map(|n| n.to_string()).collect::<Vec<_>>().join("\n")}));
        assert_eq!((noisy.output.len(), noisy.last_output()), (OUTPUT_LINES, Some("499")));
    }

    #[test]
    fn a_generation_failure_is_read_from_the_error_the_stream_ends_with() {
        let wire = json!({"_tag":"TextGenerationError","operation":"generateCommitMessage","detail":"Codex CLI command failed: codex exploded"});
        let expected = Some(Generation { operation: "generateCommitMessage".into(), detail: "Codex CLI command failed: codex exploded".into() });
        assert_eq!(generation_of(&wire), expected);
        assert_eq!(generation_of(&json!([{"_tag":"Fail","error":wire}])), expected);
        assert_eq!(generation_of(&json!({"_tag":"GitCommandError","detail":"push rejected: non-fast-forward"})), None);
        // The event before it keeps its phase; the error adds the operation and detail.
        let mut progress = Progress::started("a1", Action::Commit);
        progress.apply(&json!({"actionId":"a1","kind":"action_failed","phase":"commit","message":"Text generation failed in generateCommitMessage: Codex CLI command failed: codex exploded"}));
        progress.apply_failure(&wire);
        assert_eq!(progress.outcome, Some(Outcome::Failed { phase: Some("commit".into()), message: "Text generation failed in generateCommitMessage: Codex CLI command failed: codex exploded".into(), generation: expected.clone() }));
        let mut bare = Progress::started("a2", Action::Commit);
        bare.apply_failure(&wire);
        assert_eq!(bare.outcome, Some(Outcome::Failed { phase: None, message: "Codex CLI command failed: codex exploded".into(), generation: expected }));
        assert_eq!(generation_subject("generatePrContent", MERGE_REQUEST), "merge request title and description");
        assert_eq!(generation_subject("generateCommitMessage", PULL_REQUEST), "commit message");
    }
}
