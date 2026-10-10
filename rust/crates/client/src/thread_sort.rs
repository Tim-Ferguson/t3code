//! The order the original lists threads in outside the sidebar's sections: the command palette, the thread an opened project
//! lands on, and the thread a deleted one hands the view to. Port of `getThreadSortTimestamp`, `sortThreads`,
//! `getLatestThreadForProject` (`state/threadSort.ts`) and `getFallbackThreadIdAfterDelete` (`Sidebar.logic.ts`).
//!
//! A shell sorts by when the person last wrote to it (`latestUserMessageAt`), else by when it last changed, else by when it
//! was made; `updatedAt` moves with every event, so it is only the second choice. Ties go to the greater id.
use t3_contracts::{SidebarThreadSortOrder, ThreadShell};

fn millis(at: chrono::DateTime<chrono::Utc>) -> i64 {
    at.timestamp_millis()
}

/// `getThreadSortTimestamp`.
pub fn sort_timestamp(thread: &ThreadShell, order: SidebarThreadSortOrder) -> i64 {
    match order {
        SidebarThreadSortOrder::CreatedAt => millis(thread.created_at.0),
        SidebarThreadSortOrder::UpdatedAt => thread.latest_user_message_at.map_or_else(|| millis(thread.updated_at.0), |at| millis(at.0)),
    }
}

/// The timestamp a palette row shows: `latestUserMessageAt ?? updatedAt`.
pub fn shown_at(thread: &ThreadShell) -> chrono::DateTime<chrono::Utc> {
    thread.latest_user_message_at.unwrap_or(thread.updated_at).0
}

/// `sortThreads`: newest first, the greater id first among equals.
pub fn sort_threads<'a>(threads: impl IntoIterator<Item = &'a ThreadShell>, order: SidebarThreadSortOrder) -> Vec<&'a ThreadShell> {
    let mut keyed: Vec<(i64, &ThreadShell)> = threads.into_iter().map(|thread| (sort_timestamp(thread, order), thread)).collect();
    keyed.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.id.as_str().cmp(left.1.id.as_str())));
    keyed.into_iter().map(|(_, thread)| thread).collect()
}

/// `getLatestThreadForProject`: the project's newest thread that is not archived (a deleted one is no thread).
pub fn latest_for_project<'a>(threads: impl IntoIterator<Item = &'a ThreadShell>, project_id: &str, order: SidebarThreadSortOrder) -> Option<&'a ThreadShell> {
    sort_threads(threads.into_iter().filter(|thread| thread.project_id.as_str() == project_id && thread.archived_at.is_none() && thread.deleted_at.is_none()), order).into_iter().next()
}

/// `getFallbackThreadIdAfterDelete`: the project's next thread by the same order.
pub fn fallback_after_delete<'a>(threads: impl IntoIterator<Item = &'a ThreadShell>, deleted: &str, order: SidebarThreadSortOrder) -> Option<&'a ThreadShell> {
    let threads: Vec<&ThreadShell> = threads.into_iter().collect();
    let project = threads.iter().find(|thread| thread.id.as_str() == deleted)?.project_id.clone();
    sort_threads(threads.into_iter().filter(|thread| thread.project_id == project && thread.id.as_str() != deleted && thread.archived_at.is_none() && thread.deleted_at.is_none()), order).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thread(id: &str, project: &str, extra: serde_json::Value) -> ThreadShell {
        let mut base = json!({"createdBy":"user","creationSource":"web","id":id,"projectId":project,"title":"T","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null,"lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":id},"forkedFrom":null,"activeProviderThreadId":null,"latestRunId":null,"activeRunId":null,"status":"idle","pendingRuntimeRequest":null,"latestVisibleMessage":null,"latestUserMessageAt":null,"hasActionableProposedPlan":false,"itemCount":1,"visibleItemCount":1,"pendingBackgroundTasks":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","archivedAt":null,"settledOverride":null,"settledAt":null,"deletedAt":null});
        for (key, value) in extra.as_object().unwrap() {
            base[key] = value.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn what_a_person_wrote_orders_threads_not_what_the_server_touched() {
        let written = thread("a", "p", json!({"latestUserMessageAt":"2026-02-01T00:00:00Z","updatedAt":"2026-02-01T00:00:00Z"}));
        // Touched later by events, but written to earlier: it must not outrank the thread written to last.
        let touched = thread("b", "p", json!({"latestUserMessageAt":"2026-01-15T00:00:00Z","updatedAt":"2026-03-01T00:00:00Z"}));
        // Never written to: its own last change stands in.
        let silent = thread("c", "p", json!({"updatedAt":"2026-01-20T00:00:00Z"}));
        let all = [written, touched, silent];
        let ids = |order| sort_threads(all.iter(), order).into_iter().map(|thread| thread.id.to_string()).collect::<Vec<_>>();
        assert_eq!(ids(SidebarThreadSortOrder::UpdatedAt), ["a", "c", "b"]);
        // Creation order is the same for all three here, so the greater id goes first.
        assert_eq!(ids(SidebarThreadSortOrder::CreatedAt), ["c", "b", "a"]);
    }

    #[test]
    fn the_view_after_a_delete_goes_to_the_projects_next_thread() {
        let gone = thread("gone", "p", json!({"latestUserMessageAt":"2026-02-01T00:00:00Z"}));
        let next = thread("next", "p", json!({"latestUserMessageAt":"2026-01-31T00:00:00Z"}));
        let archived = thread("old", "p", json!({"latestUserMessageAt":"2026-01-30T12:00:00Z","archivedAt":"2026-01-30T13:00:00Z"}));
        let other = thread("other", "q", json!({"latestUserMessageAt":"2026-02-02T00:00:00Z"}));
        let all = [gone, next, archived, other];
        assert_eq!(fallback_after_delete(all.iter(), "gone", SidebarThreadSortOrder::UpdatedAt).map(|thread| thread.id.as_str()), Some("next"));
        assert_eq!(latest_for_project(all.iter(), "q", SidebarThreadSortOrder::UpdatedAt).map(|thread| thread.id.as_str()), Some("other"));
        assert!(latest_for_project(all.iter(), "none", SidebarThreadSortOrder::UpdatedAt).is_none());
    }
}
