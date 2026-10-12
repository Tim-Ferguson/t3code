//! The tabbed panel beside the conversation, one per thread: which tabs it holds, which one is shown
//! and whether the panel is open. Port of `rightPanelStore.ts`, with `closedViewStore.ts` and the
//! planning half of `reopenClosedView.ts` (what "Reopen closed tab" restores).
//!
//! The model is shallow on purpose, as the original's: it owns the ordered tab descriptors and the
//! shown one, while each feature keeps its own resource state (a file's text, a pull request's
//! detail). Tabs here are the diff, the project's files, a file, a pull request and the thread's
//! linked pull requests; the browser, terminal and device tabs of the original are not part of this
//! port's panel.
//!
//! A "key" is the panel's owner, `environment:thread` (`terminal_ui::ScopedPanes::key`).
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Where the panels are kept (the original's is `t3code:right-panel-state:v2` and holds tabs this
/// port has no kind for, so this port neither reads nor writes it).
pub const STORAGE_KEY: &str = "t3code:rust-right-panel:v1";
/// Where the closed tabs are kept (the original's is `t3code:closed-views:v2`).
pub const CLOSED_STORAGE_KEY: &str = "t3code:rust-closed-views:v1";
pub const MAX_CLOSED: usize = 20;
const VERSION: u64 = 1;

/// The tabs that exist once per panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Diff,
    Files,
    PullRequests,
}

/// What a pull request tab shows. The reference lives in the tab's id, so several pull requests can
/// stay open side by side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub repository: String,
    pub number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum Surface {
    Diff,
    Files,
    File {
        /// The folder the path is relative to: the thread's checkout, or the project's folder for the new-thread form.
        cwd: String,
        relative_path: String,
        #[serde(default)]
        reveal_line: Option<u64>,
        /// Grows with every opening, so choosing the same file again scrolls to the line again.
        #[serde(default)]
        reveal_request_id: u64,
    },
    PullRequest(PullRequestTarget),
    PullRequests,
}
impl Surface {
    /// `pullRequestSurfaceId`: the environment leads where there is one, so one pull request read from two servers is two
    /// tabs, and the host is lower-cased.
    pub fn pull_request_id(target: &PullRequestTarget) -> String {
        let encode = |text: &str| url::form_urlencoded::byte_serialize(text.as_bytes()).collect::<String>().replace('+', "%20");
        let scope = target.environment_id.as_deref().map(|environment| format!("{}:", encode(environment))).unwrap_or_default();
        let host = target.host.as_deref().map(|host| format!("{}:", encode(&host.to_lowercase()))).unwrap_or_default();
        format!("pull-request:{scope}{}:{host}{}:{}", encode(&target.project_id), encode(&target.repository), target.number)
    }
    pub fn id(&self) -> String {
        match self {
            Surface::Diff => "diff".into(),
            Surface::Files => "files".into(),
            Surface::File { relative_path, .. } => format!("file:{relative_path}"),
            Surface::PullRequest(target) => Self::pull_request_id(target),
            Surface::PullRequests => "pull-requests".into(),
        }
    }
    pub fn of(kind: Kind) -> Self {
        match kind {
            Kind::Diff => Surface::Diff,
            Kind::Files => Surface::Files,
            Kind::PullRequests => Surface::PullRequests,
        }
    }
    pub fn kind(&self) -> Option<Kind> {
        match self {
            Surface::Diff => Some(Kind::Diff),
            Surface::Files => Some(Kind::Files),
            Surface::PullRequests => Some(Kind::PullRequests),
            _ => None,
        }
    }
    /// The tab's title: the file's name, the pull request's number.
    pub fn title(&self) -> String {
        match self {
            Surface::Diff => "Diff".into(),
            Surface::Files => "Files".into(),
            Surface::File { relative_path, .. } => relative_path.rsplit(['/', '\\']).next().unwrap_or(relative_path).to_owned(),
            Surface::PullRequest(target) => format!("#{}", target.number),
            Surface::PullRequests => "Pull requests".into(),
        }
    }
    /// A pull request tab with its host in the form the id uses.
    fn normalized(self) -> Self {
        match self {
            Surface::PullRequest(mut target) => {
                target.host = target.host.map(|host| host.to_lowercase());
                Surface::PullRequest(target)
            }
            other => other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPanel {
    pub is_open: bool,
    pub active_surface_id: Option<String>,
    pub surfaces: Vec<Surface>,
}
impl ThreadPanel {
    pub fn active(&self) -> Option<&Surface> {
        let id = self.active_surface_id.as_deref()?;
        self.surfaces.iter().find(|surface| surface.id() == id)
    }
    fn is_empty(&self) -> bool {
        !self.is_open && self.active_surface_id.is_none() && self.surfaces.is_empty()
    }
    /// `upsertSurface`: the panel opens, the tab is added when it is new, and it is shown.
    fn upsert(&mut self, surface: Surface) {
        let id = surface.id();
        if !self.surfaces.iter().any(|entry| entry.id() == id) {
            self.surfaces.push(surface);
        }
        self.is_open = true;
        self.active_surface_id = Some(id);
    }
}

#[derive(Debug, Clone, Default)]
pub struct Panels {
    by_thread: BTreeMap<String, ThreadPanel>,
    /// How many panel choices the person has made per thread this session (`userActionRevisionByThreadKey`). An automatic
    /// opening (`open_proactive`, `reconcile_files`) does not advance it, so it can tell a choice made after it looked.
    revisions: BTreeMap<String, u64>,
}

/// Two sets of panels are the same when they hold the same tabs: the count of choices is bookkeeping of this session.
impl PartialEq for Panels {
    fn eq(&self, other: &Self) -> bool {
        self.by_thread == other.by_thread
    }
}

static EMPTY: ThreadPanel = ThreadPanel { is_open: false, active_surface_id: None, surfaces: Vec::new() };

impl Panels {
    pub fn get(&self, key: &str) -> &ThreadPanel {
        self.by_thread.get(key).unwrap_or(&EMPTY)
    }
    /// The shown tab, while the panel is open (`selectActiveRightPanelSurface`).
    pub fn active(&self, key: &str) -> Option<&Surface> {
        let panel = self.get(key);
        if panel.is_open { panel.active() } else { None }
    }
    pub fn is_active(&self, key: &str, kind: Kind) -> bool {
        self.active(key).is_some_and(|surface| surface.kind() == Some(kind))
    }
    /// How many choices the person has made in this thread's panel.
    pub fn user_revision(&self, key: &str) -> u64 {
        self.revisions.get(key).copied().unwrap_or(0)
    }
    /// An edit that is the person's choice: it counts, even when it changes nothing.
    fn user<R>(&mut self, key: &str, change: impl FnOnce(&mut ThreadPanel) -> R) -> R {
        *self.revisions.entry(key.to_owned()).or_insert(0) += 1;
        self.edit(key, change)
    }
    /// A panel with nothing in it is not kept (`updateThreadStateMap`).
    fn edit<R>(&mut self, key: &str, change: impl FnOnce(&mut ThreadPanel) -> R) -> R {
        let mut panel = self.by_thread.get(key).cloned().unwrap_or_default();
        let before = panel.clone();
        let result = change(&mut panel);
        if panel.is_empty() {
            self.by_thread.remove(key);
        } else if panel != before {
            self.by_thread.insert(key.to_owned(), panel);
        }
        result
    }

    /// Opens a tab that exists once, or shows the one there is.
    pub fn open(&mut self, key: &str, kind: Kind) {
        self.user(key, |panel| panel.upsert(Surface::of(kind)));
    }
    /// Opens a file as a tab beside the others; the standalone explorer gives way. `.` is the project's root, which is the
    /// explorer. Files of another folder start a new row of tabs, since a path means nothing against another checkout.
    pub fn open_file(&mut self, key: &str, cwd: &str, requested: &str, line: Option<u64>) {
        self.user(key, |panel| {
            if requested == "." {
                panel.upsert(Surface::Files);
                return;
            }
            let path = if is_drive_root(requested) { requested.to_owned() } else { requested.trim_end_matches('/').to_owned() };
            let path = if path.is_empty() { requested.to_owned() } else { path };
            panel.surfaces.retain(|surface| match surface {
                Surface::Files => false,
                Surface::File { cwd: other, .. } => other == cwd,
                _ => true,
            });
            let id = format!("file:{path}");
            let request = panel.surfaces.iter().find_map(|surface| match surface {
                Surface::File { relative_path, reveal_request_id, .. } if format!("file:{relative_path}") == id => Some(*reveal_request_id),
                _ => None,
            });
            let surface = Surface::File { cwd: cwd.to_owned(), relative_path: path, reveal_line: line.map(|line| line.max(1)), reveal_request_id: request.unwrap_or(0) + 1 };
            match panel.surfaces.iter_mut().find(|entry| entry.id() == id) {
                Some(existing) => *existing = surface,
                None => panel.surfaces.push(surface),
            }
            panel.is_open = true;
            panel.active_surface_id = Some(id);
        });
    }
    pub fn open_pull_request(&mut self, key: &str, target: PullRequestTarget) {
        self.user(key, |panel| {
            let surface = Surface::PullRequest(target).normalized();
            let id = surface.id();
            panel.upsert(surface.clone());
            // A target that carries its URL fills in a tab that was opened without one.
            if matches!(&surface, Surface::PullRequest(target) if target.url.is_some()) {
                if let Some(existing) = panel.surfaces.iter_mut().find(|entry| entry.id() == id) {
                    *existing = surface;
                }
            }
        });
    }
    pub fn activate(&mut self, key: &str, id: &str) {
        self.user(key, |panel| {
            if panel.surfaces.iter().any(|surface| surface.id() == id) {
                panel.is_open = true;
                panel.active_surface_id = Some(id.to_owned());
            }
        });
    }

    /// Closes tabs; returns what went, in the order a reopen should find them: the shown tab last, so it comes back first.
    fn close(&mut self, key: &str, change: impl FnOnce(&mut ThreadPanel)) -> Vec<Surface> {
        self.user(key, |panel| {
            let before = panel.surfaces.clone();
            let active = panel.active_surface_id.clone();
            change(panel);
            let still: Vec<String> = panel.surfaces.iter().map(Surface::id).collect();
            let removed: Vec<Surface> = before.into_iter().filter(|surface| !still.contains(&surface.id())).collect();
            let (shown, others): (Vec<_>, Vec<_>) = removed.into_iter().partition(|surface| Some(surface.id()) == active);
            [others, shown].concat()
        })
    }
    pub fn close_surface(&mut self, key: &str, id: &str) -> Vec<Surface> {
        self.close(key, |panel| {
            let Some(index) = panel.surfaces.iter().position(|surface| surface.id() == id) else { return };
            panel.surfaces.remove(index);
            panel.is_open = !panel.surfaces.is_empty() && panel.is_open;
            if panel.active_surface_id.as_deref() == Some(id) {
                panel.active_surface_id = panel.surfaces.get(index.min(panel.surfaces.len().saturating_sub(1))).map(Surface::id);
            }
        })
    }
    pub fn close_others(&mut self, key: &str, id: &str) -> Vec<Surface> {
        self.close(key, |panel| {
            let Some(kept) = panel.surfaces.iter().find(|surface| surface.id() == id).cloned() else { return };
            if panel.surfaces.len() == 1 {
                return;
            }
            panel.is_open = true;
            panel.surfaces = vec![kept];
            panel.active_surface_id = Some(id.to_owned());
        })
    }
    pub fn close_to_right(&mut self, key: &str, id: &str) -> Vec<Surface> {
        self.close(key, |panel| {
            let Some(index) = panel.surfaces.iter().position(|surface| surface.id() == id) else { return };
            if index + 1 == panel.surfaces.len() {
                return;
            }
            panel.surfaces.truncate(index + 1);
            let shown = panel.active_surface_id.as_deref().is_some_and(|active| panel.surfaces.iter().any(|surface| surface.id() == active));
            if !shown {
                panel.active_surface_id = Some(id.to_owned());
            }
        })
    }
    pub fn close_all(&mut self, key: &str) -> Vec<Surface> {
        self.close(key, |panel| {
            if !panel.surfaces.is_empty() {
                *panel = ThreadPanel::default();
            }
        })
    }
    /// A file tab whose workspace is gone goes with it (`reconcileFileSurfaces`); not a choice of the reader's.
    pub fn reconcile_files(&mut self, key: &str, workspace_available: bool) {
        self.edit(key, |panel| {
            if workspace_available {
                return;
            }
            let before = panel.surfaces.len();
            panel.surfaces.retain(|surface| !matches!(surface, Surface::Files | Surface::File { .. }));
            if panel.surfaces.len() == before {
                return;
            }
            if panel.surfaces.is_empty() {
                panel.is_open = false;
            }
            if !panel.surfaces.iter().any(|surface| Some(surface.id()) == panel.active_surface_id) {
                panel.active_surface_id = panel.surfaces.last().map(Surface::id);
            }
        });
    }
    pub fn show(&mut self, key: &str) {
        self.user(key, |panel| panel.is_open = true);
    }
    /// Hides the panel; its tabs and the shown one stay.
    pub fn hide(&mut self, key: &str) {
        self.user(key, |panel| panel.is_open = false);
    }
    pub fn toggle_visibility(&mut self, key: &str) {
        self.user(key, |panel| panel.is_open = !panel.is_open);
    }
    /// `toggle`: hides the panel when `kind` is what it shows, else shows `kind`.
    pub fn toggle(&mut self, key: &str, kind: Kind) {
        self.user(key, |panel| {
            let shown = panel.is_open && panel.active().is_some_and(|surface| surface.kind() == Some(kind));
            if shown {
                panel.is_open = false;
            } else {
                panel.upsert(Surface::of(kind));
            }
        });
    }
    pub fn remove_thread(&mut self, key: &str) {
        self.by_thread.remove(key);
        self.revisions.remove(key);
    }
    /// `openProactive`: opens a tab on the app's account, not the person's. Refused when the person has made a panel choice
    /// since `expected_revision` was read, and a diff never replaces a pull request the panel is showing (a linked pull request
    /// takes priority over a finished turn's diff). Returns whether it opened.
    pub fn open_proactive(&mut self, key: &str, surface: Surface, expected_revision: u64) -> bool {
        if self.user_revision(key) != expected_revision {
            return false;
        }
        if surface == Surface::Diff && matches!(self.active(key), Some(Surface::PullRequest(_) | Surface::PullRequests)) {
            return false;
        }
        let surface = surface.normalized();
        self.edit(key, |panel| panel.upsert(surface));
        true
    }
    /// Every key that holds a panel, for pruning.
    pub fn keys(&self) -> Vec<String> {
        self.by_thread.keys().cloned().collect()
    }

    // ---- storage -------------------------------------------------------------------------------

    pub fn to_json(&self) -> String {
        serde_json::json!({"version":VERSION,"byThreadKey":self.by_thread}).to_string()
    }
    /// What storage held (`migratePersistedRightPanelState`): a tab that cannot be read is dropped; a panel that is left with
    /// none is not kept open; an open panel always shows one.
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(map) = raw.and_then(|raw| serde_json::from_str::<Value>(raw).ok()).and_then(|value| value.get("byThreadKey").and_then(Value::as_object).cloned()) else {
            return Self::default();
        };
        let mut by_thread = BTreeMap::new();
        for (key, stored) in map {
            let surfaces: Vec<Surface> = stored["surfaces"]
                .as_array()
                .map(|list| list.iter().filter_map(|surface| serde_json::from_value::<Surface>(surface.clone()).ok()).filter(valid).map(Surface::normalized).collect())
                .unwrap_or_default();
            let mut seen = Vec::new();
            let surfaces: Vec<Surface> = surfaces.into_iter().filter(|surface| { let id = surface.id(); let fresh = !seen.contains(&id); seen.push(id); fresh }).collect();
            let stored_active = stored["activeSurfaceId"].as_str().filter(|id| surfaces.iter().any(|surface| surface.id() == *id)).map(str::to_owned);
            let is_open = !surfaces.is_empty() && stored["isOpen"].as_bool().unwrap_or(stored_active.is_some());
            let active_surface_id = stored_active.or_else(|| if is_open { surfaces.first().map(Surface::id) } else { None });
            let panel = ThreadPanel { is_open, active_surface_id, surfaces };
            if !panel.is_empty() {
                by_thread.insert(key, panel);
            }
        }
        Self { by_thread, revisions: BTreeMap::new() }
    }
}

fn valid(surface: &Surface) -> bool {
    match surface {
        Surface::File { relative_path, .. } => !relative_path.is_empty(),
        Surface::PullRequest(target) => target.number >= 1 && !target.repository.is_empty() && !target.project_id.is_empty(),
        _ => true,
    }
}
fn is_drive_root(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2..].iter().all(|byte| *byte == b'/')
}

// ---- closed tabs ---------------------------------------------------------------------------------

/// A tab that was closed, and where.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Closed {
    pub id: String,
    pub environment: String,
    pub thread_id: String,
    pub surface: Surface,
}
impl Closed {
    pub fn key(&self) -> String {
        format!("{}:{}", self.environment, self.thread_id)
    }
}

/// The history behind "Reopen closed tab": newest first, twenty at most, one entry per tab.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct History {
    pub entries: Vec<Closed>,
}
impl History {
    /// Remembers a closed tab; closing it again moves it to the front.
    pub fn remember(&mut self, environment: &str, thread_id: &str, surface: Surface, id: String) {
        let key = surface.id();
        self.entries.retain(|entry| !(entry.environment == environment && entry.thread_id == thread_id && entry.surface.id() == key));
        self.entries.insert(0, Closed { id, environment: environment.to_owned(), thread_id: thread_id.to_owned(), surface });
        self.entries.truncate(MAX_CLOSED);
    }
    /// Puts an entry that could not be restored last, so the next press tries the one before it.
    pub fn defer(&mut self, id: &str) {
        if let Some(at) = self.entries.iter().position(|entry| entry.id == id) {
            let entry = self.entries.remove(at);
            self.entries.push(entry);
        }
    }
    pub fn remove(&mut self, id: &str) {
        self.entries.retain(|entry| entry.id != id);
    }
    pub fn to_json(&self) -> String {
        serde_json::json!({"version":VERSION,"entries":self.entries}).to_string()
    }
    /// An unreadable history is an empty one, and an entry that cannot be read is dropped.
    pub fn parse(raw: Option<&str>) -> Self {
        let entries = raw
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| value.get("entries").and_then(Value::as_array).cloned())
            .map(|list| list.into_iter().filter_map(|entry| serde_json::from_value::<Closed>(entry).ok()).filter(|entry| valid(&entry.surface) && !entry.id.is_empty()).collect())
            .unwrap_or_default();
        Self { entries }
    }
}

/// What the planner needs to know about the owner of one history entry (`ReopenOwnerState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner<'a> {
    /// False when the entry's environment is not one this window can reach.
    pub environment_known: bool,
    pub catalog_ready: bool,
    /// Whether the owning thread still exists.
    pub exists: bool,
    /// A live shell that lacks the thread means it was deleted; a cached one may just be stale.
    pub shell_live: bool,
    pub panel: &'a ThreadPanel,
}

/// Picks the entry the next press restores, newest first (`planNextReopen`). Entries whose environment is gone or whose tab is
/// already open are dropped; a thread missing from a live shell is skipped but kept; anything not yet knowable stops the scan
/// with nothing to restore.
pub fn plan_next_reopen<'a>(entries: &'a [Closed], owner: impl Fn(&Closed) -> Owner<'a>) -> (Vec<&'a Closed>, Option<&'a Closed>) {
    let mut drop = Vec::new();
    for entry in entries {
        let owner = owner(entry);
        if !owner.environment_known {
            if !owner.catalog_ready {
                break;
            }
            drop.push(entry);
            continue;
        }
        if !owner.exists {
            if owner.shell_live {
                continue;
            }
            break;
        }
        let id = entry.surface.id();
        if owner.panel.is_open && owner.panel.surfaces.iter().any(|surface| surface.id() == id) {
            drop.push(entry);
            continue;
        }
        return (drop, Some(entry));
    }
    (drop, None)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_proactive_opening_gives_way_to_a_choice_of_the_persons_and_a_diff_to_a_pull_request() {
        let mut panels = Panels::default();
        let key = "e:t";
        let seen = panels.user_revision(key);
        assert!(panels.open_proactive(key, Surface::Diff, seen));
        assert!(panels.is_active(key, Kind::Diff));
        assert_eq!(panels.user_revision(key), seen, "opening on the app's own account is not a choice");
        // A tab the person opens after the count was read stops later proactive openings.
        panels.open(key, Kind::Files);
        assert!(panels.user_revision(key) > seen);
        assert!(!panels.open_proactive(key, Surface::PullRequests, seen));
        assert!(panels.is_active(key, Kind::Files));
        // Closing is a choice too, and so is showing or hiding the panel.
        let after_open = panels.user_revision(key);
        panels.hide(key);
        assert!(!panels.open_proactive(key, Surface::Diff, after_open));
        // A diff never takes the place of the pull requests the panel shows; a pull request tab does.
        let mut panels = Panels::default();
        panels.open(key, Kind::PullRequests);
        let revision = panels.user_revision(key);
        assert!(!panels.open_proactive(key, Surface::Diff, revision));
        assert!(panels.open_proactive(key, Surface::PullRequests, revision));
        // A file tab going away with its workspace is not a choice.
        panels.open_file(key, "/w", "a.txt", None);
        let revision = panels.user_revision(key);
        panels.reconcile_files(key, false);
        assert_eq!(panels.user_revision(key), revision);
        assert!(panels.open_proactive(key, Surface::PullRequests, revision));
    }
    use super::*;

    const A: &str = "env:thread-a";
    const B: &str = "env:thread-b";

    fn pr(number: u64) -> PullRequestTarget {
        PullRequestTarget { environment_id: None, project_id: "project-a".into(), host: None, repository: "pingdotgg/t3code".into(), number, url: None }
    }
    fn ids(panels: &Panels, key: &str) -> Vec<String> {
        panels.get(key).surfaces.iter().map(Surface::id).collect()
    }
    fn remembered(history: &History) -> Vec<String> {
        history.entries.iter().map(|entry| entry.surface.id()).collect()
    }
    /// Closes the way the app does: every tab that went is remembered, newest first.
    fn closing(panels: &mut Panels, history: &mut History, key: &str, close: impl FnOnce(&mut Panels) -> Vec<Surface>) {
        for surface in close(panels) {
            history.remember("env", key.trim_start_matches("env:"), surface, format!("{}", history.entries.len() + 1));
        }
    }

    #[test]
    fn opening_a_kind_shows_it_and_keeps_the_others() {
        let mut panels = Panels::default();
        panels.open(A, Kind::Files);
        assert_eq!(panels.get(A), &ThreadPanel { is_open: true, active_surface_id: Some("files".into()), surfaces: vec![Surface::Files] });
        panels.open(A, Kind::Diff);
        assert_eq!(ids(&panels, A), ["files", "diff"]);
        assert_eq!(panels.get(A).active_surface_id.as_deref(), Some("diff"));
        panels.open(A, Kind::Files);
        assert_eq!(ids(&panels, A), ["files", "diff"], "an open kind is shown, not added again");
        assert_eq!(panels.get(A).active_surface_id.as_deref(), Some("files"));
        assert_eq!(panels.get(B), &ThreadPanel::default(), "another thread's panel is its own");
    }

    #[test]
    fn toggling_hides_the_panel_without_losing_the_shown_tab() {
        let mut panels = Panels::default();
        panels.toggle(A, Kind::Diff);
        assert!(panels.is_active(A, Kind::Diff));
        panels.toggle(A, Kind::Diff);
        assert_eq!(panels.get(A), &ThreadPanel { is_open: false, active_surface_id: Some("diff".into()), surfaces: vec![Surface::Diff] });
        assert!(panels.active(A).is_none());
        panels.toggle_visibility(A);
        assert!(panels.is_active(A, Kind::Diff));
        // Toggling to another kind while another tab is shown switches.
        panels.open(A, Kind::Files);
        panels.toggle(A, Kind::Diff);
        assert!(panels.is_active(A, Kind::Diff));
        // An empty panel shows its launcher and keeps nothing once hidden.
        panels.close_all(A);
        panels.toggle_visibility(A);
        assert!(panels.get(A).is_open && panels.get(A).surfaces.is_empty());
        panels.hide(A);
        assert!(panels.keys().is_empty());
        panels.hide(B);
        assert!(panels.keys().is_empty(), "closing a panel that was never opened keeps nothing");
    }

    #[test]
    fn files_replace_the_standalone_explorer_and_stay_one_tab_per_path() {
        let mut panels = Panels::default();
        panels.open(A, Kind::Files);
        panels.open_file(A, "/work", "src/index.ts", None);
        panels.open_file(A, "/work", "src/index.ts", Some(12));
        panels.open_file(A, "/work", "README.md/", None);
        assert_eq!(ids(&panels, A), ["file:src/index.ts", "file:README.md"]);
        let Surface::File { reveal_line, reveal_request_id, .. } = &panels.get(A).surfaces[0] else { panic!() };
        assert_eq!((*reveal_line, *reveal_request_id), (Some(12), 2), "choosing a file again reveals its line again");
        // The workspace root is the explorer, once.
        panels.open_file(A, "/work", ".", None);
        panels.open_file(A, "/work", ".", None);
        assert_eq!(ids(&panels, A), ["file:src/index.ts", "file:README.md", "files"]);
        assert_eq!(panels.get(A).active_surface_id.as_deref(), Some("files"));
        panels.close_surface(A, "files");
        assert_eq!(panels.get(A).active_surface_id.as_deref(), Some("file:README.md"));
        // Another folder starts a new row of tabs; a line below 1 is line 1.
        panels.open_file(A, "/other", "a.rs", Some(0));
        assert_eq!(ids(&panels, A), ["file:a.rs"]);
        assert!(matches!(&panels.get(A).surfaces[0], Surface::File { reveal_line: Some(1), .. }));
    }

    #[test]
    fn pull_requests_are_one_tab_per_reference() {
        let mut panels = Panels::default();
        panels.open_pull_request(A, pr(42));
        panels.open_pull_request(A, pr(42));
        panels.open_pull_request(A, pr(43));
        panels.open_pull_request(A, PullRequestTarget { host: Some("GitHub.com".into()), ..pr(42) });
        panels.open_pull_request(A, PullRequestTarget { environment_id: Some("env-2".into()), ..pr(42) });
        assert_eq!(
            ids(&panels, A),
            ["pull-request:project-a:pingdotgg%2Ft3code:42", "pull-request:project-a:pingdotgg%2Ft3code:43", "pull-request:project-a:github.com:pingdotgg%2Ft3code:42", "pull-request:env-2:project-a:pingdotgg%2Ft3code:42"],
            "the host and the environment tell two tabs apart, and the host is lower-cased"
        );
        // A later opening that carries the URL fills it in.
        panels.open_pull_request(A, PullRequestTarget { url: Some("https://github.com/pingdotgg/t3code/pull/43".into()), ..pr(43) });
        assert!(matches!(&panels.get(A).surfaces[1], Surface::PullRequest(target) if target.url.is_some()));
        assert_eq!(panels.get(A).active_surface_id.as_deref(), Some("pull-request:project-a:pingdotgg%2Ft3code:43"));
        assert_eq!(panels.get(A).surfaces[1].title(), "#43");
    }

    #[test]
    fn closing_the_shown_tab_shows_its_neighbour_and_the_last_closes_the_panel() {
        let mut panels = Panels::default();
        panels.open(A, Kind::Files);
        panels.open(A, Kind::Diff);
        panels.open(A, Kind::PullRequests);
        panels.activate(A, "diff");
        assert_eq!(panels.close_surface(A, "diff"), [Surface::Diff]);
        assert_eq!(panels.get(A).active_surface_id.as_deref(), Some("pull-requests"), "the tab that took its place");
        panels.close_surface(A, "pull-requests");
        assert_eq!(panels.get(A).active_surface_id.as_deref(), Some("files"));
        // Closing one that is not shown keeps the shown one.
        panels.open(A, Kind::Diff);
        panels.close_surface(A, "files");
        assert_eq!(panels.get(A).active_surface_id.as_deref(), Some("diff"));
        panels.close_surface(A, "diff");
        assert!(panels.keys().is_empty(), "the last tab takes the panel with it");
        assert!(panels.close_surface(A, "diff").is_empty());
    }

    #[test]
    fn closing_others_to_the_right_or_all() {
        let mut panels = Panels::default();
        for kind in [Kind::Files, Kind::Diff, Kind::PullRequests] {
            panels.open(A, kind);
        }
        panels.activate(A, "files");
        // To the right of the shown tab: the shown one stays.
        assert_eq!(panels.close_to_right(A, "files"), [Surface::PullRequests, Surface::Diff].into_iter().rev().collect::<Vec<_>>());
        assert_eq!(ids(&panels, A), ["files"]);
        for kind in [Kind::Diff, Kind::PullRequests] {
            panels.open(A, kind);
        }
        // The shown one went with the choice: the tab the menu was on is shown.
        panels.close_to_right(A, "diff");
        assert_eq!(ids(&panels, A), ["files", "diff"]);
        panels.open(A, Kind::PullRequests);
        panels.activate(A, "pull-requests");
        assert_eq!(panels.close_to_right(A, "pull-requests"), []);
        panels.close_to_right(A, "files");
        assert_eq!(panels.get(A).active_surface_id.as_deref(), Some("files"));
        panels.open(A, Kind::Diff);
        assert_eq!(panels.close_others(A, "diff"), [Surface::Files]);
        assert_eq!((ids(&panels, A), panels.get(A).is_open), (vec!["diff".to_owned()], true));
        assert_eq!(panels.close_others(A, "diff"), [], "nothing else to close");
        assert_eq!(panels.close_all(A), [Surface::Diff]);
        assert!(panels.keys().is_empty());
    }

    #[test]
    fn closed_tabs_are_remembered_newest_first_with_the_shown_one_first_after_a_bulk_close() {
        let mut panels = Panels::default();
        let mut history = History::default();
        panels.open_file(A, "/work", "src/app.ts", None);
        panels.open(A, Kind::Files);
        panels.open_pull_request(A, pr(42));
        panels.open(A, Kind::Diff);
        let pr_id = Surface::pull_request_id(&pr(42));
        closing(&mut panels, &mut history, A, |panels| panels.close_surface(A, &pr_id));
        closing(&mut panels, &mut history, A, |panels| panels.close_to_right(A, "files"));
        closing(&mut panels, &mut history, A, |panels| panels.close_others(A, "files"));
        assert_eq!(remembered(&history), ["file:src/app.ts".to_owned(), "diff".to_owned(), pr_id.clone()]);

        // The shown tab of a bulk close comes back first.
        let mut panels = Panels::default();
        let mut history = History::default();
        for kind in [Kind::Files, Kind::Diff, Kind::PullRequests] {
            panels.open(A, kind);
        }
        panels.activate(A, "diff");
        closing(&mut panels, &mut history, A, |panels| panels.close_all(A));
        assert_eq!(remembered(&history), ["diff", "pull-requests", "files"]);
    }

    #[test]
    fn hiding_the_panel_or_replacing_the_explorer_remembers_nothing() {
        let mut panels = Panels::default();
        let history = History::default();
        panels.open(A, Kind::Files);
        panels.open_file(A, "/work", "src/app.ts", None);
        panels.hide(A);
        panels.toggle_visibility(A);
        panels.reconcile_files(A, true);
        assert!(history.entries.is_empty());
        // A workspace that is gone takes the file tabs with it and not the others.
        panels.open(A, Kind::Diff);
        panels.open_file(A, "/work", "src/app.ts", None);
        panels.reconcile_files(A, false);
        assert_eq!(ids(&panels, A), ["diff"]);
        assert_eq!(panels.get(A).active_surface_id.as_deref(), Some("diff"));
        panels.close_all(A);
        panels.open(A, Kind::Files);
        panels.reconcile_files(A, false);
        assert!(panels.keys().is_empty() || !panels.get(A).is_open);
    }

    #[test]
    fn the_history_keeps_twenty_and_moves_a_closed_again_tab_to_the_front() {
        let mut history = History::default();
        history.remember("env", "a", Surface::Diff, "1".into());
        history.remember("env", "b", Surface::Diff, "2".into());
        history.remember("env", "a", Surface::Diff, "3".into());
        assert_eq!(history.entries.iter().map(|entry| (entry.thread_id.as_str(), entry.id.as_str())).collect::<Vec<_>>(), [("a", "3"), ("b", "2")]);
        for index in 0..24 {
            history.remember("env", "a", Surface::File { cwd: "/w".into(), relative_path: format!("{index}.ts"), reveal_line: None, reveal_request_id: 0 }, format!("f{index}"));
        }
        assert_eq!(history.entries.len(), MAX_CLOSED);
        assert_eq!(history.entries[0].surface.id(), "file:23.ts");
        history.defer("f23");
        assert_eq!(history.entries.last().unwrap().id, "f23");
        history.remove("f23");
        assert!(history.entries.iter().all(|entry| entry.id != "f23"));
    }

    #[test]
    fn panels_and_history_come_back_from_storage_and_a_damaged_entry_is_dropped() {
        let mut panels = Panels::default();
        panels.open_file(A, "/work", "src/app.ts", Some(7));
        panels.open_pull_request(A, PullRequestTarget { host: Some("github.com".into()), url: Some("https://github.com/o/r/pull/3".into()), ..pr(3) });
        panels.open(A, Kind::Diff);
        panels.hide(A);
        panels.open(B, Kind::PullRequests);
        let again = Panels::parse(Some(&panels.to_json()));
        assert_eq!(again, panels);
        assert!(!again.get(A).is_open && again.get(A).active_surface_id.as_deref() == Some("diff"), "hidden stays hidden and keeps its shown tab");

        // Damage: an unknown kind, a pull request without a number, an empty path, a repeated tab, a shown tab that is not there.
        let damaged = serde_json::json!({"version":1,"byThreadKey":{
            A: {"isOpen":true,"activeSurfaceId":"missing","surfaces":[{"kind":"terminal","resourceId":"t"},{"kind":"pull-request","projectId":"p","repository":"r","number":0},{"kind":"file","cwd":"/w","relativePath":""},{"kind":"diff"},{"kind":"diff"}]},
            B: {"isOpen":true,"activeSurfaceId":null,"surfaces":[{"kind":"terminal","resourceId":"t"}]},
            "env:c": {"activeSurfaceId":"files","surfaces":[{"kind":"files"}]},
        }});
        let panels = Panels::parse(Some(&damaged.to_string()));
        assert_eq!((ids(&panels, A), panels.get(A).is_open, panels.get(A).active_surface_id.as_deref()), (vec!["diff".to_owned()], true, Some("diff")), "an open panel always shows a tab");
        assert!(panels.get(B).surfaces.is_empty() && !panels.get(B).is_open, "nothing left, nothing kept open");
        assert!(panels.get("env:c").is_open, "a panel saved without the flag is open when it had a shown tab");
        assert_eq!(Panels::parse(Some("not json")), Panels::default());
        assert_eq!(Panels::parse(None), Panels::default());

        let mut history = History::default();
        history.remember("env", "thread-a", Surface::PullRequests, "x".into());
        assert_eq!(History::parse(Some(&history.to_json())), history);
        assert!(History::parse(Some(r#"{"entries":[{"id":"y","environment":"e","threadId":"t","surface":{"kind":"mystery"}}]}"#)).entries.is_empty());
    }

    #[test]
    fn reopening_plans_in_the_originals_order() {
        let closed = |id: &str, surface: Surface| Closed { id: id.into(), environment: "env".into(), thread_id: "thread-a".into(), surface };
        let entries = [closed("a", Surface::Diff), closed("b", Surface::Files)];
        let hidden = ThreadPanel { is_open: false, active_surface_id: Some("diff".into()), surfaces: vec![Surface::Diff] };
        let shown = ThreadPanel { is_open: true, ..hidden.clone() };
        let empty = ThreadPanel::default();
        fn known(panel: &ThreadPanel) -> Owner<'_> {
            Owner { environment_known: true, catalog_ready: true, exists: true, shell_live: true, panel }
        }
        let plan = |owners: Vec<Owner<'_>>| {
            let (dropped, restored) = plan_next_reopen(&entries[..owners.len()], |entry| owners[entries.iter().position(|candidate| candidate == entry).unwrap()]);
            (dropped.iter().map(|entry| entry.id.clone()).collect::<Vec<_>>(), restored.map(|entry| entry.id.clone()))
        };
        assert_eq!(plan(vec![known(&empty), known(&empty)]), (vec![], Some("a".into())), "the newest entry");
        assert_eq!(plan(vec![Owner { environment_known: false, ..known(&empty) }, known(&empty)]), (vec!["a".into()], Some("b".into())), "a removed environment is dropped once the catalog is ready");
        assert_eq!(plan(vec![Owner { environment_known: false, catalog_ready: false, ..known(&empty) }, known(&empty)]), (vec![], None), "wait for the catalog");
        assert_eq!(plan(vec![Owner { exists: false, ..known(&empty) }, known(&empty)]), (vec![], Some("b".into())), "a thread deleted from a live shell is skipped and kept");
        assert_eq!(plan(vec![Owner { exists: false, shell_live: false, ..known(&empty) }, known(&empty)]), (vec![], None), "a cached shell may be stale: wait");
        assert_eq!(plan(vec![known(&shown), known(&empty)]), (vec!["a".into()], Some("b".into())), "a tab that is already open is dropped");
        assert_eq!(plan(vec![known(&hidden)]), (vec![], Some("a".into())), "a tab of a hidden panel is restored");
    }
}
