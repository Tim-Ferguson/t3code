//! The words and rules of ending a thread or a project: what each question says, what the
//! reasons the server gives look like, and which threads may be archived. Port of
//! `ConfirmDialogHost.tsx` (how a message becomes a title and a description), the delete and
//! archive questions of `Sidebar.tsx`, `useThreadActions.ts` and `LegacySidebar.tsx`, and the
//! undo notice of `showThreadUndoNotice.ts`.
use t3_contracts::ThreadStatus;

/// What a dialog shows for a message of lines, as `resolveConfirmDialogCopy` splits it: the first
/// line that ends with a question mark is the title, the other lines are the description.
pub fn confirm_copy(message: &str) -> (String, Option<String>) {
    let message = message.trim();
    let lines: Vec<&str> = message.split('\n').collect();
    if let Some(index) = lines.iter().position(|line| line.trim().ends_with('?')) {
        let description = lines.iter().enumerate().filter(|(at, _)| *at != index).map(|(_, line)| *line).collect::<Vec<_>>().join("\n");
        let description = description.trim();
        return (lines[index].trim().to_owned(), (!description.is_empty()).then(|| description.to_owned()));
    }
    if let Some(at) = message.find('?') {
        let (title, rest) = message.split_at(at + 1);
        return (title.trim().to_owned(), (!rest.trim().is_empty()).then(|| rest.trim().to_owned()));
    }
    ("Confirm action".into(), Some(if message.is_empty() { "This action requires your confirmation.".into() } else { message.into() }))
}

/// The confirm button of every such dialog.
pub const CONFIRM_BUTTON: &str = "Confirm";

pub fn delete_thread_message(title: &str) -> String {
    [format!("Delete thread \"{title}\"?"), "This permanently clears conversation history for this thread.".into()].join("\n")
}
pub fn archive_thread_message(title: &str) -> String {
    format!("Archive thread \"{title}\"?")
}

/// The first answer to removing a project that still has threads, and the button that goes on.
pub const NOT_EMPTY_TITLE: &str = "Project is not empty";
pub const NOT_EMPTY_BODY: &str = "Delete all threads in this project before removing it.";
pub const NOT_EMPTY_ACTION: &str = "Delete anyway";

/// The question asked before a project goes. `environment` is named when more than one
/// environment is connected, as the original names the member's environment.
pub fn remove_project_message(title: &str, path: &str, environment: Option<&str>, threads: usize) -> String {
    let mut lines = Vec::new();
    if threads > 0 {
        lines.push(format!("Remove project \"{title}\" and delete its {threads} thread{}?", if threads == 1 { "" } else { "s" }));
    } else {
        lines.push(format!("Remove project \"{title}\"?"));
    }
    lines.push(format!("Path: {path}"));
    if let Some(environment) = environment.filter(|label| !label.is_empty()) {
        lines.push(format!("Environment: {environment}"));
    }
    if threads > 0 {
        lines.push("This permanently clears conversation history for those threads and any archived threads.".into());
        lines.push("This removes only this project entry.".into());
        lines.push("This action cannot be undone.".into());
    } else {
        lines.push("This permanently clears any archived conversation history.".into());
        lines.push("This removes only this project entry.".into());
    }
    lines.join("\n")
}

/// `threadRuntimeCanArchive`: archiving may discard queued work but must not detach a provider
/// that is preparing, starting or running a turn.
pub fn can_archive(status: ThreadStatus, has_active_run: bool) -> bool {
    match status {
        ThreadStatus::Queued => !has_active_run,
        ThreadStatus::Preparing | ThreadStatus::Starting | ThreadStatus::Running => false,
        _ => true,
    }
}

/// The line the undo notice shows: "Archived 2 threads,".
pub fn undo_line(action: &str, count: usize) -> String {
    format!("{action} {count} thread{},", if count == 1 { "" } else { "s" })
}

/// `getOrphanedWorktreePathForThread` (`worktreeCleanup.ts`): the worktree this thread alone is linked to, which is the one
/// deleting it may take away. `threads` are `(id, worktree path)` of the threads that survive, the target included.
pub fn orphaned_worktree_path(threads: &[(&str, Option<&str>)], thread_id: &str) -> Option<String> {
    let trimmed = |path: Option<&str>| path.map(str::trim).filter(|path| !path.is_empty()).map(str::to_owned);
    let target = trimmed(threads.iter().find(|(id, _)| *id == thread_id)?.1)?;
    let shared = threads.iter().any(|(id, path)| *id != thread_id && trimmed(*path).as_deref() == Some(target.as_str()));
    (!shared).then_some(target)
}

/// `formatWorktreePathForDisplay`: the folder's own name.
pub fn display_worktree_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return path.to_owned();
    }
    let normalized = trimmed.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    let last = normalized.rsplit('/').next().map(str::trim).unwrap_or_default();
    if last.is_empty() { trimmed.to_owned() } else { last.to_owned() }
}

/// The question asked about a worktree no other thread uses, when the thread goes.
pub fn delete_worktree_message(display_path: &str) -> String {
    ["This thread is the only one linked to this worktree:", display_path, "", "Delete the worktree too?"].join("\n")
}

/// A project's own worktree cleanup policy (`inherit`, `off` or `custom`), as the settings hold it for that project.
pub fn project_cleanup_mode(settings: &serde_json::Value, project_id: &str) -> &'static str {
    let overrides = &settings["projectSettingsOverrides"][project_id];
    if !overrides.get("worktreeCleanup").is_some_and(|policy| policy.is_object()) {
        return "inherit";
    }
    match overrides["worktreeCleanup"]["mode"].as_str() {
        Some("off") => "off",
        Some("custom") => "custom",
        _ => "inherit",
    }
}

/// `resolveWorktreeCleanup(settings, projectId)`: the four rules in force, as JSON with the settings' own keys. A project's
/// own policy (`custom` rules or `off`) outranks the environment's `storageCleanup`; `project_id` is none for the
/// environment itself.
pub fn worktree_cleanup_rules(settings: &serde_json::Value, project_id: Option<&str>) -> serde_json::Value {
    use serde_json::json;
    let overrides = project_id.map(|id| &settings["projectSettingsOverrides"][id]);
    let policy = match overrides.filter(|entry| entry.get("worktreeCleanup").is_some()) {
        Some(entry) => &entry["worktreeCleanup"],
        None => &settings["worktreeCleanup"],
    };
    match policy["mode"].as_str() {
        Some("custom") => policy["rules"].clone(),
        Some("off") => json!({"worktreeAfterDays": null, "worktreeOnMerge": false, "worktreeOnDelete": false, "worktreeUnchanged": false}),
        _ => {
            let cleanup = &settings["storageCleanup"];
            json!({"worktreeAfterDays": cleanup["worktreeAfterDays"], "worktreeOnMerge": cleanup["worktreeOnMerge"], "worktreeOnDelete": cleanup["worktreeOnDelete"], "worktreeUnchanged": cleanup["worktreeUnchanged"]})
        }
    }
}

/// `resolveWorktreeCleanup(settings, projectId).worktreeOnDelete`: whether the server takes the worktree away by itself when
/// the thread goes, in which case the client does not ask.
pub fn worktree_on_delete(settings: &serde_json::Value, project_id: &str) -> bool {
    worktree_cleanup_rules(settings, Some(project_id))["worktreeOnDelete"].as_bool().unwrap_or(false)
}

/// Whether the project is the environment's Scratch project (`isScratchProject`): its folder is the scratch root.
pub fn is_scratch_project(workspace_root: &str, scratch_root: Option<&str>) -> bool {
    let comparable = |path: &str| {
        let normalized = crate::source_control::normalize_project_path(path);
        let windows = normalized.starts_with("\\\\") || (normalized.len() >= 2 && normalized.as_bytes()[0].is_ascii_alphabetic() && normalized.as_bytes()[1] == b':');
        if windows { normalized.replace('/', "\\").to_lowercase() } else { normalized }
    };
    scratch_root.is_some_and(|root| !comparable(root).is_empty() && comparable(root) == comparable(workspace_root))
}

/// A thread as far as picking the previous worktree cares.
pub struct WorktreeThread<'a> {
    pub branch: Option<&'a str>,
    pub worktree_path: Option<&'a str>,
    /// Milliseconds since the epoch.
    pub updated_at: i64,
    pub archived: bool,
}

/// `resolvePreviousWorktreeSeed`: the most recently touched worktree of the project that the form
/// is not already pointing at, as `(branch, path)`. Archived threads do not compete: the rest of
/// the app hides them, so their worktrees should not resurface here.
pub fn previous_worktree(threads: &[WorktreeThread<'_>], current: Option<&str>) -> Option<(Option<String>, String)> {
    threads
        .iter()
        .filter(|thread| !thread.archived)
        .filter_map(|thread| Some((thread, thread.worktree_path.filter(|path| !path.is_empty() && Some(*path) != current)?)))
        .fold(None::<(&WorktreeThread<'_>, &str)>, |best, (thread, path)| match best {
            Some((latest, _)) if latest.updated_at >= thread.updated_at => best,
            _ => Some((thread, path)),
        })
        .map(|(thread, path)| (thread.branch.map(str::to_owned), path.to_owned()))
}
pub fn previous_worktree_label(branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("Previous worktree ({branch})"),
        None => "Previous worktree".into(),
    }
}
pub fn workspace_label(existing: bool, worktree: bool) -> &'static str {
    if existing {
        "Existing worktree"
    } else if worktree {
        "New worktree"
    } else {
        "Current checkout"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_splits_into_a_question_and_what_follows_as_the_dialog_host_does() {
        let (title, description) = confirm_copy(&delete_thread_message("Fix \"it\""));
        assert_eq!(title, "Delete thread \"Fix \"it\"\"?");
        assert_eq!(description.as_deref(), Some("This permanently clears conversation history for this thread."));
        assert_eq!(confirm_copy("Archive thread \"a\"?"), ("Archive thread \"a\"?".to_owned(), None));
        assert_eq!(confirm_copy("No question here"), ("Confirm action".to_owned(), Some("No question here".to_owned())));
        assert_eq!(confirm_copy("Really? Yes, really."), ("Really?".to_owned(), Some("Yes, really.".to_owned())));
        assert_eq!(confirm_copy("  "), ("Confirm action".to_owned(), Some("This action requires your confirmation.".to_owned())));
    }

    #[test]
    fn a_worktree_is_offered_only_when_no_other_surviving_thread_uses_it() {
        let threads = [("a", Some("/w/feature")), ("b", Some(" /w/feature ")), ("c", Some("/w/other")), ("d", None), ("e", Some("  "))];
        assert_eq!(orphaned_worktree_path(&threads, "c").as_deref(), Some("/w/other"));
        assert_eq!(orphaned_worktree_path(&threads, "a"), None, "b shares it");
        assert_eq!(orphaned_worktree_path(&threads[..1], "a").as_deref(), Some("/w/feature"));
        assert_eq!(orphaned_worktree_path(&threads, "d"), None);
        assert_eq!(orphaned_worktree_path(&threads, "e"), None);
        assert_eq!(orphaned_worktree_path(&threads, "missing"), None);
        assert_eq!(display_worktree_path("/w/feature/"), "feature");
        assert_eq!(display_worktree_path("C:\\work\\feature"), "feature");
        let (title, body) = confirm_copy(&delete_worktree_message("feature"));
        assert_eq!((title.as_str(), body.as_deref()), ("Delete the worktree too?", Some("This thread is the only one linked to this worktree:\nfeature")));
    }

    #[test]
    fn a_projects_own_cleanup_policy_decides_whether_the_server_removes_the_worktree_for_us() {
        use serde_json::json;
        let settings = json!({"worktreeCleanup": null, "storageCleanup": {"worktreeOnDelete": true}, "projectSettingsOverrides": {
            "off": {"worktreeCleanup": {"mode": "off"}},
            "custom": {"worktreeCleanup": {"mode": "custom", "rules": {"worktreeOnDelete": false}}},
            "inherit": {"worktreeCleanup": {"mode": "inherit"}},
        }});
        assert!(worktree_on_delete(&settings, "p"));
        assert!(worktree_on_delete(&settings, "inherit"));
        assert!(!worktree_on_delete(&settings, "off"));
        assert!(!worktree_on_delete(&settings, "custom"));
        assert!(!worktree_on_delete(&json!({"storageCleanup": {"worktreeOnDelete": false}}), "p"));
        assert!(is_scratch_project("/scratch/", Some("/scratch")));
        assert!(!is_scratch_project("/work", Some("/scratch")));
        assert!(!is_scratch_project("/work", None));
    }

    #[test]
    fn removing_a_project_says_what_goes_in_the_originals_words() {
        let with_threads = remove_project_message("Web", "/work/web", None, 2);
        assert_eq!(
            with_threads,
            "Remove project \"Web\" and delete its 2 threads?\nPath: /work/web\nThis permanently clears conversation history for those threads and any archived threads.\nThis removes only this project entry.\nThis action cannot be undone."
        );
        assert!(remove_project_message("Web", "/w", None, 1).starts_with("Remove project \"Web\" and delete its 1 thread?\n"));
        assert_eq!(
            remove_project_message("Web", "/work/web", Some("Laptop"), 0),
            "Remove project \"Web\"?\nPath: /work/web\nEnvironment: Laptop\nThis permanently clears any archived conversation history.\nThis removes only this project entry."
        );
        let (title, description) = confirm_copy(&with_threads);
        assert_eq!(title, "Remove project \"Web\" and delete its 2 threads?");
        assert!(description.unwrap().ends_with("This action cannot be undone."));
    }

    #[test]
    fn the_previous_worktree_is_the_latest_one_that_is_not_archived_and_not_in_use() {
        let thread = |branch: Option<&'static str>, path: Option<&'static str>, updated_at, archived| WorktreeThread { branch, worktree_path: path, updated_at, archived };
        let threads = [thread(Some("a"), Some("/w/a"), 10, false), thread(Some("b"), Some("/w/b"), 30, false), thread(Some("c"), Some("/w/c"), 50, true), thread(None, None, 90, false), thread(Some("d"), Some("/w/d"), 20, false)];
        assert_eq!(previous_worktree(&threads, None), Some((Some("b".into()), "/w/b".into())));
        assert_eq!(previous_worktree(&threads, Some("/w/b")), Some((Some("d".into()), "/w/d".into())), "the one in use is skipped");
        assert_eq!(previous_worktree(&[thread(Some("x"), None, 1, false)], None), None);
        assert_eq!(previous_worktree_label(Some("b")), "Previous worktree (b)");
        assert_eq!(previous_worktree_label(None), "Previous worktree");
        assert_eq!((workspace_label(false, false), workspace_label(false, true), workspace_label(true, true)), ("Current checkout", "New worktree", "Existing worktree"));
    }

    #[test]
    fn a_thread_with_a_provider_at_work_is_not_archived() {
        assert!(!can_archive(ThreadStatus::Running, true) && !can_archive(ThreadStatus::Starting, true) && !can_archive(ThreadStatus::Preparing, false));
        assert!(can_archive(ThreadStatus::Queued, false) && !can_archive(ThreadStatus::Queued, true));
        assert!(can_archive(ThreadStatus::Idle, false) && can_archive(ThreadStatus::Waiting, true));
        assert_eq!(undo_line("Archived", 1), "Archived 1 thread,");
        assert_eq!(undo_line("Archived", 3), "Archived 3 threads,");
    }
}
