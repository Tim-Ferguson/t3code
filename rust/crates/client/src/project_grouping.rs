//! Which projects share a sidebar row. Port of `packages/client-runtime/src/state/projectGrouping.ts`
//! (`buildProjectGroups`) and the parts of `apps/web/src/sidebarProjectGrouping.ts` the sidebar uses.
//! Pure: a project is read as the few fields grouping looks at, so the same code serves the sidebar,
//! the thread rows (which scope their lists by a group's projects) and the project picker.
use std::collections::BTreeMap;

use serde_json::Value;
pub use t3_contracts::SidebarProjectGroupingMode as Mode;
use t3_contracts::ClientSettings;

use crate::project_import::comparable;

/// The two settings grouping reads (`selectProjectGroupingSettings`).
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub mode: Mode,
    /// By physical project key (`environmentId:path`).
    pub overrides: BTreeMap<String, Mode>,
}
impl Settings {
    pub fn of(settings: &ClientSettings) -> Self {
        Self {
            mode: settings.sidebar_project_grouping_mode,
            overrides: settings.sidebar_project_grouping_overrides.iter().map(|(key, mode)| (key.to_string(), *mode)).collect(),
        }
    }
    pub fn mode_of(&self, project: &Project) -> Mode {
        self.overrides.get(&override_key(project)).copied().unwrap_or(self.mode)
    }
}

/// A project as grouping reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub environment: String,
    pub id: String,
    pub title: String,
    pub workspace_root: String,
    pub created_at: String,
    pub updated_at: String,
    /// The `repositoryIdentity` the server resolved for the project's directory, as sent.
    pub identity: Option<Value>,
}

/// The one member a physical project (one directory in one environment) contributes.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub physical_key: String,
    /// Index into the projects the groups were built from.
    pub index: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub key: String,
    pub label: String,
    /// Index of the project that stands for the group (the preferred environment's, else the first).
    pub representative: usize,
    pub members: Vec<Member>,
    /// Every project of the group, duplicates at one directory included, as `(environment, id)` in input order.
    pub project_refs: Vec<(String, String)>,
}
impl Group {
    pub fn project_ids(&self) -> Vec<String> {
        self.project_refs.iter().map(|(_, id)| id.clone()).collect()
    }
    pub fn contains(&self, environment: &str, id: &str) -> bool {
        self.project_refs.iter().any(|(env, project)| env == environment && project == id)
    }
}

pub fn physical_key_of(environment: &str, cwd: &str) -> String {
    format!("{environment}:{}", comparable(cwd))
}
pub fn physical_key(project: &Project) -> String {
    physical_key_of(&project.environment, &project.workspace_root)
}
/// What `sidebarProjectGroupingOverrides` is keyed by.
pub fn override_key(project: &Project) -> String {
    physical_key(project)
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
/// `repositoryGroupingKeyOf`: a fork's own remote, otherwise the canonical repository.
fn identity_key(identity: &Value) -> Option<&str> {
    identity.get("origin").and_then(|origin| text(origin, "canonicalKey")).or_else(|| text(identity, "canonicalKey")).filter(|key| !key.is_empty())
}
/// `repositoryGroupingDisplayNameOf`.
fn identity_display_name(identity: &Value) -> Option<&str> {
    match identity.get("origin").filter(|origin| origin.is_object()) {
        Some(origin) => text(origin, "displayName").or_else(|| text(origin, "canonicalKey")),
        None => text(identity, "displayName"),
    }
}

/// The project's directory relative to its repository's root: empty when it is the root, none when it
/// is not inside it or the root is unknown.
fn repository_relative_path(project: &Project) -> Option<String> {
    let root = project.identity.as_ref().and_then(|identity| text(identity, "rootPath")).map(str::trim).filter(|root| !root.is_empty())?;
    let path = comparable(&project.workspace_root);
    let root = comparable(root);
    if path.is_empty() || root.is_empty() {
        return None;
    }
    if path == root {
        return Some(String::new());
    }
    let separator = if root.contains('\\') { '\\' } else { '/' };
    let prefix = format!("{root}{separator}");
    path.strip_prefix(&prefix).map(|rest| rest.replace('\\', "/"))
}

fn repository_scoped_key(project: &Project, mode: Mode) -> Option<String> {
    let canonical = project.identity.as_ref().and_then(identity_key)?.to_owned();
    if mode == Mode::Repository {
        return Some(canonical);
    }
    Some(match repository_relative_path(project) {
        Some(relative) if !relative.is_empty() => format!("{canonical}::{relative}"),
        _ => canonical,
    })
}

/// `deriveLogicalProjectKey`.
pub fn logical_key(project: &Project, mode: Mode) -> String {
    if mode == Mode::Separate {
        return physical_key(project);
    }
    repository_scoped_key(project, mode).unwrap_or_else(|| physical_key(project))
}

fn unique_non_empty<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Vec<&'a str> {
    let mut seen: Vec<&str> = Vec::new();
    for value in values.into_iter().flatten().map(str::trim).filter(|value| !value.is_empty()) {
        if !seen.contains(&value) {
            seen.push(value);
        }
    }
    seen
}

/// `deriveProjectGroupLabel`: a title the members share and that is not the repository's own name, else
/// the repository's name, else the representative's title.
fn group_label(representative: &Project, members: &[&Project]) -> String {
    let titles = unique_non_empty(members.iter().map(|member| Some(member.title.as_str())));
    let display_names = unique_non_empty(members.iter().map(|member| member.identity.as_ref().and_then(identity_display_name)));
    let repository_names = unique_non_empty(members.iter().map(|member| member.identity.as_ref().and_then(|identity| text(identity, "name"))));
    if let [title] = titles[..] {
        if !display_names.contains(&title) && !repository_names.contains(&title) {
            return title.to_owned();
        }
    }
    if let [name] = display_names[..] {
        return name.to_owned();
    }
    if let [name] = repository_names[..] {
        return name.to_owned();
    }
    representative.title.clone()
}

fn freshness(project: &Project) -> i64 {
    let parse = |text: &str| chrono::DateTime::parse_from_rfc3339(text).ok().map(|time| time.timestamp_millis());
    parse(&project.updated_at).or_else(|| parse(&project.created_at)).unwrap_or(0)
}
fn replaces(existing: &Project, candidate: &Project) -> bool {
    let delta = freshness(candidate) - freshness(existing);
    delta > 0 || (delta == 0 && candidate.id > existing.id)
}
/// The project a physical project's identity is read from: the winner, else the freshest one that has one.
fn identity_source<'a>(projects: &[&'a Project], winner: &'a Project) -> &'a Project {
    if winner.identity.is_some() {
        return winner;
    }
    let mut freshest: Option<&Project> = None;
    for project in projects {
        if project.identity.is_none() {
            continue;
        }
        if freshest.is_none_or(|current| replaces(current, project)) {
            freshest = Some(project);
        }
    }
    freshest.unwrap_or(winner)
}

/// `buildProjectGroups`. Groups come in the order their first project appears.
pub fn build_groups(projects: &[Project], settings: &Settings, preferred_environment: Option<&str>) -> Vec<Group> {
    // Projects at one directory of one environment are one physical project; the freshest speaks for it.
    let mut physical: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, project) in projects.iter().enumerate() {
        let key = physical_key(project);
        match physical.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, indexes)) => indexes.push(index),
            None => physical.push((key, vec![index])),
        }
    }
    let mut logical_by_physical: BTreeMap<String, String> = BTreeMap::new();
    let mut grouped: Vec<(String, Vec<Member>)> = Vec::new();
    for (key, indexes) in &physical {
        let candidates: Vec<&Project> = indexes.iter().map(|index| &projects[*index]).collect();
        let mut winner = indexes[0];
        for index in &indexes[1..] {
            if replaces(&projects[winner], &projects[*index]) {
                winner = *index;
            }
        }
        let source = identity_source(&candidates, &projects[winner]);
        let logical = logical_key(source, settings.mode_of(&projects[winner]));
        logical_by_physical.insert(key.clone(), logical.clone());
        let member = Member { physical_key: key.clone(), index: winner };
        match grouped.iter_mut().find(|(existing, _)| *existing == logical) {
            Some((_, members)) => members.push(member),
            None => grouped.push((logical, vec![member])),
        }
    }
    let mut refs: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for project in projects {
        let logical = logical_by_physical.get(&physical_key(project)).cloned().unwrap_or_else(|| logical_key(project, settings.mode_of(project)));
        let reference = (project.environment.clone(), project.id.clone());
        let list = refs.entry(logical).or_default();
        if !list.contains(&reference) {
            list.push(reference);
        }
    }
    grouped
        .into_iter()
        .map(|(key, members)| {
            let representative = preferred_environment.and_then(|environment| members.iter().find(|member| projects[member.index].environment == environment)).unwrap_or(&members[0]).index;
            let label = if members.len() > 1 {
                group_label(&projects[representative], &members.iter().map(|member| &projects[member.index]).collect::<Vec<_>>())
            } else {
                projects[representative].title.clone()
            };
            let project_refs = refs.remove(&key).unwrap_or_default();
            Group { key, label, representative, members, project_refs }
        })
        .collect()
}

/// The group the project of `(environment, id)` belongs to.
pub fn group_of<'a>(groups: &'a [Group], environment: &str, id: &str) -> Option<&'a Group> {
    groups.iter().find(|group| group.contains(environment, id))
}

/// `isProjectGroupingEnabled`.
pub fn enabled(mode: Mode) -> bool {
    mode != Mode::Separate
}
/// `projectGroupingModeFromToggle`.
pub fn mode_from_toggle(enabled: bool, last_enabled: Mode) -> Mode {
    if !enabled {
        Mode::Separate
    } else if last_enabled == Mode::RepositoryPath {
        Mode::RepositoryPath
    } else {
        Mode::Repository
    }
}
pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Repository => "Group by repository",
        Mode::RepositoryPath => "Group by repository path",
        Mode::Separate => "Keep separate",
    }
}
pub fn mode_description(mode: Mode) -> &'static str {
    match mode {
        Mode::Repository => "Projects from the same repository share one sidebar row.",
        Mode::RepositoryPath => "Projects group only when both the repository and repo-relative path match.",
        Mode::Separate => "Every project path gets its own sidebar row.",
    }
}
pub fn mode_wire(mode: Mode) -> &'static str {
    match mode {
        Mode::Repository => "repository",
        Mode::RepositoryPath => "repository_path",
        Mode::Separate => "separate",
    }
}

/// The overrides after "Group into…" saved `choice` (none: inherit the global default) for a project.
pub fn overrides_with(current: &BTreeMap<String, Mode>, key: &str, choice: Option<Mode>) -> BTreeMap<String, Mode> {
    let mut next = current.clone();
    match choice {
        Some(mode) => next.insert(key.to_owned(), mode),
        None => next.remove(key),
    };
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn project(id: &str, title: &str, root: &str, identity: Option<Value>, updated: &str) -> Project {
        Project { environment: "env".into(), id: id.into(), title: title.into(), workspace_root: root.into(), created_at: "2026-01-01T00:00:00.000Z".into(), updated_at: updated.into(), identity }
    }
    fn identity(key: &str, root: &str) -> Value {
        json!({"canonicalKey": key, "rootPath": root, "name": "t3code", "displayName": "pingdotgg/t3code", "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "x"}})
    }
    fn settings(mode: Mode) -> Settings {
        Settings { mode, overrides: BTreeMap::new() }
    }

    #[test]
    fn checkouts_of_one_repository_share_a_row() {
        let projects = vec![
            project("a", "t3code", "/work/t3code", Some(identity("github.com/pingdotgg/t3code", "/work/t3code")), "2026-02-01T00:00:00.000Z"),
            project("b", "t3code-fix", "/work/wt/fix", Some(identity("github.com/pingdotgg/t3code", "/work/wt/fix")), "2026-02-02T00:00:00.000Z"),
            project("c", "notes", "/work/notes", None, "2026-02-03T00:00:00.000Z"),
        ];
        let groups = build_groups(&projects, &settings(Mode::Repository), None);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].key, "github.com/pingdotgg/t3code");
        assert_eq!(groups[0].project_ids(), vec!["a", "b"]);
        assert_eq!(groups[0].label, "pingdotgg/t3code", "a title that differs per member gives way to the repository's name");
        assert_eq!(groups[0].representative, 0);
        assert_eq!(groups[1].key, "env:/work/notes");
        assert_eq!(groups[1].label, "notes");
    }

    #[test]
    fn separate_mode_keeps_every_directory_apart() {
        let projects = vec![project("a", "t3code", "/work/t3code", Some(identity("k", "/work/t3code")), ""), project("b", "t3code", "/work/other", Some(identity("k", "/work/other")), "")];
        assert_eq!(build_groups(&projects, &settings(Mode::Separate), None).len(), 2);
        assert_eq!(build_groups(&projects, &settings(Mode::Repository), None).len(), 1);
    }

    #[test]
    fn repository_path_mode_tells_folders_of_one_repository_apart() {
        let root = "/work/mono";
        let projects = vec![
            project("a", "mono", root, Some(identity("k", root)), ""),
            project("b", "web", "/work/mono/apps/web", Some(identity("k", root)), ""),
            project("c", "web again", "/work/wt/apps/web", Some(identity("k", "/work/wt")), ""),
        ];
        let groups = build_groups(&projects, &settings(Mode::RepositoryPath), None);
        assert_eq!(groups.iter().map(|group| group.key.as_str()).collect::<Vec<_>>(), vec!["k", "k::apps/web"]);
        assert_eq!(groups[1].project_ids(), vec!["b", "c"]);
    }

    #[test]
    fn an_override_regroups_one_directory_only() {
        let mut settings = settings(Mode::Repository);
        settings.overrides.insert("env:/work/b".into(), Mode::Separate);
        let projects = vec![project("a", "a", "/work/a", Some(identity("k", "/work/a")), ""), project("b", "b", "/work/b", Some(identity("k", "/work/b")), "")];
        let groups = build_groups(&projects, &settings, None);
        assert_eq!(groups.iter().map(|group| group.key.as_str()).collect::<Vec<_>>(), vec!["k", "env:/work/b"]);
    }

    #[test]
    fn projects_at_one_directory_are_one_member_and_the_freshest_speaks() {
        let projects = vec![project("a", "old", "/work/a/", None, "2026-01-01T00:00:00.000Z"), project("b", "new", "/work/a", None, "2026-03-01T00:00:00.000Z")];
        let groups = build_groups(&projects, &settings(Mode::Repository), None);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members.len(), 1);
        assert_eq!(groups[0].members[0].index, 1);
        assert_eq!(groups[0].label, "new");
        assert_eq!(groups[0].project_ids(), vec!["a", "b"], "both projects remain targets of the row");
    }

    #[test]
    fn a_member_without_identity_takes_it_from_a_fresher_sibling_at_its_directory() {
        let projects = vec![project("a", "x", "/work/a", Some(identity("k", "/work/a")), "2026-01-01T00:00:00.000Z"), project("b", "x", "/work/a", None, "2026-02-01T00:00:00.000Z")];
        let groups = build_groups(&projects, &settings(Mode::Repository), None);
        assert_eq!(groups[0].key, "k");
    }

    #[test]
    fn a_fork_groups_by_its_own_remote() {
        let mut fork = identity("github.com/me/t3code", "/work/fork");
        fork["origin"] = json!({"canonicalKey": "github.com/pingdotgg/t3code"});
        let projects = vec![project("a", "a", "/work/a", Some(identity("github.com/pingdotgg/t3code", "/work/a")), ""), project("b", "b", "/work/fork", Some(fork), "")];
        assert_eq!(build_groups(&projects, &settings(Mode::Repository), None).len(), 1);
    }

    #[test]
    fn the_toggle_remembers_the_mode_it_turned_off() {
        assert_eq!(mode_from_toggle(false, Mode::RepositoryPath), Mode::Separate);
        assert_eq!(mode_from_toggle(true, Mode::RepositoryPath), Mode::RepositoryPath);
        assert_eq!(mode_from_toggle(true, Mode::Separate), Mode::Repository);
        assert!(enabled(Mode::Repository) && !enabled(Mode::Separate));
    }

    #[test]
    fn saving_an_override_adds_or_clears_it() {
        let current = BTreeMap::from([("a".to_owned(), Mode::Separate)]);
        assert_eq!(overrides_with(&current, "b", Some(Mode::Repository)).len(), 2);
        assert!(overrides_with(&current, "a", None).is_empty());
    }
}
