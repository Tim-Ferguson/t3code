//! Branch and working-tree status for a workspace, and the ref picker's logic. Port of
//! `applyGitStatusStreamEvent`/`mergeGitStatusParts` (`packages/shared/src/git.ts`), the pure
//! parts of `BranchToolbar.logic.ts`, `paginatedBranches.ts` and the pull and init wording of
//! `GitActionsControl.tsx`.
use serde_json::Value;

pub const SUBSCRIBE_STATUS: &str = "subscribeVcsStatus";
pub const REFRESH_STATUS: &str = "vcs.refreshStatus";
pub const LIST_REFS: &str = "vcs.listRefs";
pub const SWITCH_REF: &str = "vcs.switchRef";
pub const CREATE_REF: &str = "vcs.createRef";
pub const PULL: &str = "vcs.pull";
pub const INIT: &str = "vcs.init";
/// Refs per page, as the original asks.
pub const REF_PAGE_LIMIT: u64 = 100;
/// The pause after the last keystroke before the picker asks the server.
pub const QUERY_DEBOUNCE_MS: u64 = 120;

// ---- status ----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Local {
    pub is_repo: bool,
    pub has_primary_remote: bool,
    pub is_default_ref: bool,
    /// `None` on a detached HEAD.
    pub ref_name: Option<String>,
    pub has_working_tree_changes: bool,
    pub files: u64,
    pub insertions: u64,
    pub deletions: u64,
    /// The host's kind (`github`, `gitlab`, …), which names a change request.
    pub provider_kind: Option<String>,
    /// The files with uncommitted changes, for the commit dialog.
    pub changed: Vec<ChangedPath>,
}

/// One entry of `workingTree.files`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChangedPath {
    pub path: String,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Remote {
    pub has_upstream: bool,
    pub ahead: u64,
    pub behind: u64,
    pub ahead_of_default: Option<u64>,
    /// The change request of the current branch, as the server found it.
    pub pr: Option<StatusPr>,
}

/// Both halves, as `mergeGitStatusParts` joins them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub local: Local,
    pub remote: Remote,
}

/// `VcsStatusChangeRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusPr {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub base_ref: String,
    pub head_ref: String,
    /// `open`, `closed` or `merged`.
    pub state: String,
    pub is_draft: bool,
}
impl StatusPr {
    pub fn open(&self) -> bool {
        self.state == "open"
    }
}

fn count(value: &Value) -> u64 {
    value.as_u64().unwrap_or(0)
}

fn local_of(value: &Value) -> Result<Local, String> {
    let is_repo = value["isRepo"].as_bool().ok_or("The status has no repository state.")?;
    let tree = &value["workingTree"];
    Ok(Local {
        is_repo,
        has_primary_remote: value["hasPrimaryRemote"].as_bool().unwrap_or(false),
        is_default_ref: value["isDefaultRef"].as_bool().unwrap_or(false),
        ref_name: value["refName"].as_str().map(str::to_owned),
        has_working_tree_changes: value["hasWorkingTreeChanges"].as_bool().unwrap_or(false),
        files: tree["files"].as_array().map_or(0, |files| files.len() as u64),
        insertions: count(&tree["insertions"]),
        deletions: count(&tree["deletions"]),
        provider_kind: value["sourceControlProvider"]["kind"].as_str().map(str::to_owned),
        changed: tree["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|file| Some(ChangedPath { path: file["path"].as_str()?.to_owned(), insertions: count(&file["insertions"]), deletions: count(&file["deletions"]) }))
            .collect(),
    })
}

fn remote_of(value: &Value) -> Remote {
    if value.is_null() {
        return Remote::default();
    }
    Remote {
        has_upstream: value["hasUpstream"].as_bool().unwrap_or(false),
        ahead: count(&value["aheadCount"]),
        behind: count(&value["behindCount"]),
        ahead_of_default: value["aheadOfDefaultCount"].as_u64(),
        pr: value["pr"].is_object().then(|| StatusPr {
            number: value["pr"]["number"].as_u64().unwrap_or(0),
            title: value["pr"]["title"].as_str().unwrap_or_default().to_owned(),
            url: value["pr"]["url"].as_str().unwrap_or_default().to_owned(),
            base_ref: value["pr"]["baseRef"].as_str().unwrap_or_default().to_owned(),
            head_ref: value["pr"]["headRef"].as_str().unwrap_or_default().to_owned(),
            state: value["pr"]["state"].as_str().unwrap_or("open").to_owned(),
            is_draft: value["pr"]["isDraft"].as_bool().unwrap_or(false),
        }),
    }
}

impl Status {
    /// One `subscribeVcsStatus` event applied to what was known. A remote update before any
    /// snapshot gets an empty local half, as in the original.
    pub fn apply(current: Option<&Status>, event: &Value) -> Result<Status, String> {
        match event["_tag"].as_str() {
            Some("snapshot") => Ok(Status { local: local_of(&event["local"])?, remote: remote_of(&event["remote"]) }),
            Some("localUpdated") => Ok(Status { local: local_of(&event["local"])?, remote: current.map(|status| status.remote.clone()).unwrap_or_default() }),
            Some("remoteUpdated") => Ok(Status {
                local: current.map(|status| status.local.clone()).unwrap_or(Local { is_repo: true, ..Local::default() }),
                remote: remote_of(&event["remote"]),
            }),
            other => Err(format!("Unknown status update: {}", other.unwrap_or("none"))),
        }
    }

    /// Hover text for the uncommitted-changes mark.
    pub fn changes_summary(&self) -> String {
        let local = &self.local;
        let noun = if local.files == 1 { "file" } else { "files" };
        format!("{} {noun} with uncommitted changes (+{} \u{2212}{})", local.files, local.insertions, local.deletions)
    }

    /// The compact mark beside the branch name: `\u{25cf} 3` for changes, `\u{2191}1 \u{2193}2` for commits.
    pub fn marks(&self) -> Vec<String> {
        let mut marks = Vec::new();
        if self.local.has_working_tree_changes {
            marks.push(if self.local.files > 0 { format!("\u{25cf} {}", self.local.files) } else { "\u{25cf}".to_owned() });
        }
        if self.remote.has_upstream && self.remote.ahead > 0 {
            marks.push(format!("\u{2191}{}", self.remote.ahead));
        }
        if self.remote.has_upstream && self.remote.behind > 0 {
            marks.push(format!("\u{2193}{}", self.remote.behind));
        }
        marks
    }

    /// `resolveQuickAction`'s pull case, the only commit-sync action this client has.
    pub fn pull(&self) -> PullOffer {
        if self.local.ref_name.is_none() || !self.local.is_repo || !self.remote.has_upstream || self.remote.behind == 0 {
            PullOffer::None
        } else if self.remote.ahead > 0 {
            PullOffer::Diverged
        } else {
            PullOffer::Available
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOffer {
    None,
    Available,
    /// Both sides have commits the other lacks; a fast-forward pull cannot help.
    Diverged,
}
pub const DIVERGED_HINT: &str = "Branch has diverged from upstream. Rebase/merge first.";

/// The answer to `vcs.pull`, as the original words it: `(title, description)`.
pub fn pull_outcome(result: &Value) -> (String, String) {
    let name = result["refName"].as_str().unwrap_or("The branch");
    if result["status"] == "pulled" {
        ("Pulled".into(), format!("Updated {name} from {}", result["upstreamRef"].as_str().unwrap_or("upstream")))
    } else {
        ("Already up to date".into(), format!("{name} is already synchronized."))
    }
}

/// What the header says about the branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Label {
    /// The first answer has not arrived.
    Loading,
    NotRepo,
    Detached,
    Branch(String),
}
pub fn label(status: Option<&Status>) -> Label {
    match status {
        None => Label::Loading,
        Some(status) if !status.local.is_repo => Label::NotRepo,
        Some(status) => status.local.ref_name.clone().map_or(Label::Detached, Label::Branch),
    }
}

/// The text of a failed `vcs.*` request. The wire errors carry their parts, and the human
/// sentence is what the original's error classes build from them.
pub fn failure_message(cause: &Value) -> String {
    let errors: Vec<&Value> = match cause.as_array() {
        Some(causes) => causes.iter().map(|cause| &cause["error"]).collect(),
        None => vec![cause],
    };
    for error in errors {
        let text = |key: &str| error[key].as_str().map(str::trim).filter(|text| !text.is_empty());
        match error["_tag"].as_str() {
            Some("GitCommandError") => {
                let (Some(operation), Some(detail)) = (text("operation"), text("detail")) else { continue };
                let reason = text("reason").map(|reason| format!(" ({reason})")).unwrap_or_default();
                return format!("Git command failed in {operation} ({}): {detail}{reason}", text("cwd").unwrap_or(""));
            }
            Some("GitManagerError") => {
                let (Some(operation), Some(detail)) = (text("operation"), text("detail")) else { continue };
                return format!("Git manager failed in {operation}: {detail}");
            }
            _ => {}
        }
        if let Some(message) = text("message") {
            return message.to_owned();
        }
        if let Some(detail) = text("detail") {
            return detail.to_owned();
        }
    }
    "The server could not complete this request.".into()
}

// ---- refs ------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub is_remote: bool,
    pub current: bool,
    pub is_default: bool,
    /// Where the ref is checked out, when it is.
    pub worktree_path: Option<String>,
}

impl Ref {
    /// The tag on a row, in the original's precedence.
    pub fn badge(&self, project_cwd: Option<&str>) -> Option<&'static str> {
        let elsewhere = self.worktree_path.as_deref().is_some_and(|path| project_cwd.is_some_and(|cwd| path != cwd));
        if self.current {
            Some("current")
        } else if elsewhere {
            Some("worktree")
        } else if self.is_remote {
            Some("remote")
        } else if self.is_default {
            Some("default")
        } else {
            None
        }
    }
}

/// The refs loaded so far for one query, page by page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Refs {
    pub refs: Vec<Ref>,
    pub is_repo: bool,
    pub next_cursor: Option<u64>,
    pub total: u64,
}

impl Refs {
    /// Adds a page. A ref seen on an earlier page keeps its place, and the newest copy wins.
    pub fn merge(&mut self, page: &Value) -> Result<(), String> {
        let rows = page["refs"].as_array().ok_or("The ref list has no refs.")?;
        let mut parsed = Vec::with_capacity(rows.len());
        for row in rows {
            parsed.push(Ref {
                name: row["name"].as_str().ok_or("A ref has no name.")?.to_owned(),
                is_remote: row["isRemote"].as_bool().unwrap_or(false),
                current: row["current"].as_bool().unwrap_or(false),
                is_default: row["isDefault"].as_bool().unwrap_or(false),
                worktree_path: row["worktreePath"].as_str().map(str::to_owned),
            });
        }
        for incoming in parsed {
            match self.refs.iter_mut().find(|existing| existing.name == incoming.name) {
                Some(existing) => *existing = incoming,
                None => self.refs.push(incoming),
            }
        }
        self.is_repo = page["isRepo"].as_bool().unwrap_or(true);
        self.next_cursor = page["nextCursor"].as_u64();
        self.total = self.total.max(page["totalCount"].as_u64().unwrap_or(0));
        Ok(())
    }

    /// `branchStatusText`.
    pub fn status_text(&self, loading_next: bool) -> Option<String> {
        if loading_next {
            Some("Loading more refs...".into())
        } else if self.next_cursor.is_some() {
            Some(format!("Showing {} of {} refs", self.refs.len(), self.total))
        } else {
            None
        }
    }
}

/// `sanitizeNewRefName`: refs cannot contain white space, so a typed name gets dashes.
pub fn sanitize_new_ref_name(raw: &str) -> String {
    let mut name = String::new();
    let mut gap = false;
    for character in raw.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}').chars() {
        if is_ref_space(character) {
            gap = true;
            continue;
        }
        if gap {
            name.push('-');
            gap = false;
        }
        name.push(character);
    }
    name
}
/// The white space git rejects in ref names: the ASCII set `[ \t\n\r\f\v]` only.
fn is_ref_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{c}' | '\u{b}')
}

/// `deriveLocalBranchNameFromRemoteRef`: `origin/feature/x` is `feature/x` locally.
pub fn local_name_of_remote(name: &str) -> &str {
    match name.find('/') {
        Some(index) if index > 0 && index + 1 < name.len() => &name[index + 1..],
        _ => name,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Ref(Ref),
    /// "Create new ref", with the sanitized name it would create.
    Create(String),
}

/// `shouldIncludeBranchPickerItem` for a ref: a case-insensitive substring of the settled query,
/// or of its sanitized form (typing "new branch" must not hide "new-branch").
pub fn matches_query(name: &str, settled: &str) -> bool {
    let query = settled.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let name = name.to_lowercase();
    if name.contains(&query) {
        return true;
    }
    let sanitized = sanitize_new_ref_name(&query);
    !sanitized.is_empty() && sanitized != query && name.contains(&sanitized)
}

/// The rows of the picker. `typed` is the search box as typed, `settled` the query the loaded
/// refs answer; the create row follows what is typed and is offered unless a ref already has
/// the sanitized name.
pub fn items(refs: &Refs, typed: &str, settled: &str, can_create: bool) -> Vec<Item> {
    let mut rows: Vec<Item> = refs.refs.iter().filter(|candidate| matches_query(&candidate.name, settled)).cloned().map(Item::Ref).collect();
    let name = sanitize_new_ref_name(typed);
    if can_create && !name.is_empty() && !refs.refs.iter().any(|candidate| candidate.name == name) {
        rows.push(Item::Create(name));
    }
    rows
}

/// `shouldLoadNextBranchPageAfterScroll`: scrolling down to within 96px of the end.
pub fn should_load_next(previous_top: Option<f64>, top: f64, height: f64, client_height: f64) -> bool {
    match previous_top {
        Some(previous) if top > previous => height - top - client_height <= 96.0,
        _ => false,
    }
}

/// Where a chosen ref is checked out, and whether the thread moves to an existing worktree
/// instead of switching one (`resolveBranchSelectionTarget`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub checkout_cwd: String,
    pub next_worktree: Option<String>,
    pub reuse_existing_worktree: bool,
}
pub fn selection(project_cwd: &str, thread_worktree: Option<&str>, chosen: &Ref) -> Selection {
    if let Some(path) = &chosen.worktree_path {
        return Selection {
            checkout_cwd: path.clone(),
            next_worktree: (path != project_cwd).then(|| path.clone()),
            reuse_existing_worktree: true,
        };
    }
    let next_worktree = if thread_worktree.is_some() && chosen.is_default { None } else { thread_worktree.map(str::to_owned) };
    Selection { checkout_cwd: next_worktree.clone().unwrap_or_else(|| project_cwd.to_owned()), next_worktree, reuse_existing_worktree: false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn local(branch: &str, changes: bool) -> Value {
        json!({"isRepo": true, "hasPrimaryRemote": true, "isDefaultRef": branch == "main", "refName": branch, "hasWorkingTreeChanges": changes,
            "workingTree": {"files": if changes { json!([{"path": "a", "insertions": 2, "deletions": 1}]) } else { json!([]) }, "insertions": if changes { 2 } else { 0 }, "deletions": if changes { 1 } else { 0 }}})
    }

    #[test]
    fn a_snapshot_then_updates_join_the_two_halves() {
        let snapshot = Status::apply(None, &json!({"_tag": "snapshot", "local": local("main", false), "remote": null})).unwrap();
        assert_eq!(snapshot.remote, Remote::default());
        assert_eq!(label(Some(&snapshot)), Label::Branch("main".into()));
        assert!(snapshot.marks().is_empty());
        let remote = json!({"_tag": "remoteUpdated", "remote": {"hasUpstream": true, "aheadCount": 1, "behindCount": 2, "pr": null}});
        let with_remote = Status::apply(Some(&snapshot), &remote).unwrap();
        assert_eq!((with_remote.local.clone(), with_remote.remote.ahead, with_remote.remote.behind), (snapshot.local.clone(), 1, 2));
        // A local update keeps the remote half it was not told about.
        let dirty = Status::apply(Some(&with_remote), &json!({"_tag": "localUpdated", "local": local("feature", true)})).unwrap();
        assert_eq!((dirty.remote.behind, label(Some(&dirty))), (2, Label::Branch("feature".into())));
        assert_eq!(dirty.marks(), ["\u{25cf} 1", "\u{2191}1", "\u{2193}2"]);
        assert_eq!(dirty.changes_summary(), "1 file with uncommitted changes (+2 \u{2212}1)");
        // A remote update that clears the upstream empties the counts.
        let cleared = Status::apply(Some(&dirty), &json!({"_tag": "remoteUpdated", "remote": null})).unwrap();
        assert_eq!(cleared.remote, Remote::default());
    }

    #[test]
    fn a_remote_update_before_a_snapshot_has_an_empty_local_half() {
        let status = Status::apply(None, &json!({"_tag": "remoteUpdated", "remote": {"hasUpstream": true, "aheadCount": 0, "behindCount": 0}})).unwrap();
        assert!(status.local.is_repo && status.local.ref_name.is_none());
    }

    #[test]
    fn malformed_and_unknown_events_are_errors_not_state() {
        assert!(Status::apply(None, &json!({"_tag": "snapshot", "local": {}})).is_err());
        assert!(Status::apply(None, &json!({"_tag": "somethingElse"})).is_err());
        assert!(Status::apply(None, &json!({})).is_err());
    }

    #[test]
    fn labels_cover_loading_detached_and_not_a_repository() {
        assert_eq!(label(None), Label::Loading);
        let detached = Status::apply(None, &json!({"_tag": "snapshot", "local": {"isRepo": true, "refName": null}, "remote": null})).unwrap();
        assert_eq!(label(Some(&detached)), Label::Detached);
        let none = Status::apply(None, &json!({"_tag": "snapshot", "local": {"isRepo": false, "refName": null}, "remote": null})).unwrap();
        assert_eq!(label(Some(&none)), Label::NotRepo);
    }

    #[test]
    fn pull_is_offered_only_when_behind_without_local_commits() {
        let mut status = Status::apply(None, &json!({"_tag": "snapshot", "local": local("main", false), "remote": {"hasUpstream": true, "aheadCount": 0, "behindCount": 3}})).unwrap();
        assert_eq!(status.pull(), PullOffer::Available);
        status.remote.ahead = 1;
        assert_eq!(status.pull(), PullOffer::Diverged);
        status.remote.ahead = 0;
        status.remote.behind = 0;
        assert_eq!(status.pull(), PullOffer::None);
        status.remote.behind = 2;
        status.remote.has_upstream = false;
        assert_eq!(status.pull(), PullOffer::None);
        status.remote.has_upstream = true;
        status.local.ref_name = None;
        assert_eq!(status.pull(), PullOffer::None);
        // Counts without an upstream are not shown.
        assert!(status.marks().contains(&"\u{2193}2".to_owned()));
        status.remote.has_upstream = false;
        assert!(status.marks().is_empty());
    }

    #[test]
    fn pull_results_use_the_originals_words() {
        assert_eq!(pull_outcome(&json!({"status": "pulled", "refName": "main", "upstreamRef": "origin/main"})), ("Pulled".into(), "Updated main from origin/main".into()));
        assert_eq!(pull_outcome(&json!({"status": "pulled", "refName": "main", "upstreamRef": null})).1, "Updated main from upstream");
        assert_eq!(pull_outcome(&json!({"status": "skipped_up_to_date", "refName": "main", "upstreamRef": "origin/main"})), ("Already up to date".into(), "main is already synchronized.".into()));
    }

    #[test]
    fn failures_read_like_the_originals_error_classes() {
        let git = json!([{"_tag": "Fail", "error": {"_tag": "GitCommandError", "operation": "GitVcsDriver.switchRef.checkout", "command": "git", "cwd": "/repo", "detail": "error: Your local changes would be overwritten", "reason": "dirty"}}]);
        assert_eq!(failure_message(&git), "Git command failed in GitVcsDriver.switchRef.checkout (/repo): error: Your local changes would be overwritten (dirty)");
        let manager = json!([{"error": {"_tag": "GitManagerError", "operation": "pull", "cwd": "/repo", "detail": "no upstream"}}]);
        assert_eq!(failure_message(&manager), "Git manager failed in pull: no upstream");
        assert_eq!(failure_message(&json!([{"error": {"_tag": "SchemaDecodeError", "message": "Expected object"}}])), "Expected object");
        assert_eq!(failure_message(&json!({"_tag": "RepositoryDetection", "detail": "not a repository"})), "not a repository");
        assert_eq!(failure_message(&json!(null)), "The server could not complete this request.");
    }

    fn page(names: &[&str], next: Option<u64>, total: u64) -> Value {
        json!({"refs": names.iter().enumerate().map(|(index, name)| json!({"name": name, "current": index == 0, "isDefault": *name == "main", "isRemote": name.starts_with("origin/"), "worktreePath": null})).collect::<Vec<_>>(), "isRepo": true, "hasPrimaryRemote": true, "nextCursor": next, "totalCount": total})
    }

    #[test]
    fn pages_accumulate_without_duplicates_and_say_how_many_remain() {
        let mut refs = Refs::default();
        refs.merge(&page(&["main", "a"], Some(2), 3)).unwrap();
        assert_eq!(refs.status_text(false).as_deref(), Some("Showing 2 of 3 refs"));
        assert_eq!(refs.status_text(true).as_deref(), Some("Loading more refs..."));
        refs.merge(&page(&["a", "b"], None, 3)).unwrap();
        assert_eq!(refs.refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["main", "a", "b"]);
        assert_eq!(refs.status_text(false), None);
        assert!(Refs::default().merge(&json!({})).is_err());
    }

    #[test]
    fn typed_names_become_ref_names_the_way_the_original_does() {
        assert_eq!(sanitize_new_ref_name("  new branch\tname "), "new-branch-name");
        assert_eq!(sanitize_new_ref_name("Feature/X"), "Feature/X");
        assert_eq!(sanitize_new_ref_name("a--b"), "a--b");
        assert_eq!(sanitize_new_ref_name("   "), "");
        assert_eq!(sanitize_new_ref_name("a\u{a0}b"), "a\u{a0}b");
        assert_eq!(local_name_of_remote("origin/feature/x"), "feature/x");
        assert_eq!(local_name_of_remote("main"), "main");
        assert_eq!(local_name_of_remote("origin/"), "origin/");
    }

    #[test]
    fn the_picker_filters_and_offers_creation_for_what_is_typed() {
        let mut refs = Refs::default();
        refs.merge(&page(&["main", "new-branch", "Release/1"], None, 3)).unwrap();
        let names = |rows: &[Item]| rows.iter().map(|row| match row { Item::Ref(r) => r.name.clone(), Item::Create(name) => format!("create:{name}") }).collect::<Vec<_>>();
        assert_eq!(names(&items(&refs, "", "", true)), ["main", "new-branch", "Release/1"]);
        // Typing "new branch" keeps the existing "new-branch" and offers no duplicate of it.
        assert_eq!(names(&items(&refs, "new branch", "new branch", true)), ["new-branch"]);
        assert_eq!(names(&items(&refs, "fresh one", "fresh one", true)), ["create:fresh-one"]);
        assert_eq!(names(&items(&refs, "fresh one", "fresh one", false)), Vec::<String>::new());
        // Results of the previous query are filtered by the settled one until new ones arrive.
        assert_eq!(names(&items(&refs, "rel", "release", true)), ["Release/1", "create:rel"]);
        assert!(!matches_query("main", "zzz"));
    }

    #[test]
    fn rows_are_tagged_in_the_originals_order() {
        let base = Ref { name: "x".into(), is_remote: false, current: false, is_default: false, worktree_path: None };
        assert_eq!(Ref { current: true, is_default: true, ..base.clone() }.badge(Some("/p")), Some("current"));
        assert_eq!(Ref { worktree_path: Some("/other".into()), is_remote: true, ..base.clone() }.badge(Some("/p")), Some("worktree"));
        assert_eq!(Ref { worktree_path: Some("/p".into()), is_remote: true, ..base.clone() }.badge(Some("/p")), Some("remote"));
        assert_eq!(Ref { is_default: true, ..base.clone() }.badge(None), Some("default"));
        assert_eq!(base.badge(None), None);
    }

    #[test]
    fn the_next_page_loads_when_scrolling_down_near_the_end() {
        assert!(should_load_next(Some(100.0), 200.0, 500.0, 220.0));
        assert!(!should_load_next(Some(100.0), 200.0, 900.0, 220.0), "far from the end");
        assert!(!should_load_next(Some(300.0), 200.0, 500.0, 220.0), "scrolling up");
        assert!(!should_load_next(None, 200.0, 500.0, 220.0), "no earlier position");
    }

    #[test]
    fn choosing_a_ref_checked_out_elsewhere_moves_the_thread_there() {
        let plain = Ref { name: "b".into(), is_remote: false, current: false, is_default: false, worktree_path: None };
        assert_eq!(selection("/p", None, &plain), Selection { checkout_cwd: "/p".into(), next_worktree: None, reuse_existing_worktree: false });
        assert_eq!(selection("/p", Some("/w"), &plain).checkout_cwd, "/w");
        let default = Ref { is_default: true, ..plain.clone() };
        assert_eq!(selection("/p", Some("/w"), &default), Selection { checkout_cwd: "/p".into(), next_worktree: None, reuse_existing_worktree: false });
        let elsewhere = Ref { worktree_path: Some("/other".into()), ..plain.clone() };
        assert_eq!(selection("/p", None, &elsewhere), Selection { checkout_cwd: "/other".into(), next_worktree: Some("/other".into()), reuse_existing_worktree: true });
        let main_checkout = Ref { worktree_path: Some("/p".into()), ..plain };
        assert_eq!(selection("/p", Some("/w"), &main_checkout).next_worktree, None);
    }
}
