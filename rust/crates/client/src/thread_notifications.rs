//! Telling the person that a thread they are not looking at finished, failed or needs them. Port of the scan of
//! `ThreadNotificationCoordinator.tsx` (`EnvironmentNotifications`) and the mode helpers of `threadNotifications.ts`.
//!
//! A thread raises an alert when its attention state changes to input, approval, failure or a usage limit, or when
//! a run completes later than the last one seen. The first time a thread is seen it only records its state, so
//! connecting does not announce everything that already happened. Subagent threads never alert, and an archived
//! thread's state is recorded without an alert.
use crate::sidebar::{self, Status};
use std::collections::BTreeMap;
use t3_contracts::{NotificationMode, ThreadRelationship, ThreadShell};

pub const MODE_OPTIONS: [(&str, &str); 4] = [("\"off\"", "Off"), ("\"notifications\"", "Notifications only"), ("\"sound\"", "Sound only"), ("\"notifications-and-sound\"", "Notifications with sound")];
pub const PERMISSION_DENIED: &str = "Allow notifications in your browser or system settings, then choose this option again. Sound only is still available.";
pub const UNSUPPORTED: &str = "Notifications need a supported browser over HTTPS, or the desktop app. Sound only is still available.";
pub const UNAVAILABLE: &str = "Notifications are unavailable in this browser. Sound only is still available.";
pub const DESCRIPTION: &str = "System alerts when a thread finishes, fails, or needs input or approval. Applies to this device while T3 Code is open.";

pub fn has_sound(mode: NotificationMode) -> bool {
    matches!(mode, NotificationMode::Sound | NotificationMode::NotificationsAndSound)
}
pub fn has_desktop_notifications(mode: NotificationMode) -> bool {
    matches!(mode, NotificationMode::Notifications | NotificationMode::NotificationsAndSound)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Sounded and shown with the input sound: the thread needs the person.
    Input,
    Completion,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub thread_id: String,
    pub kind: Kind,
    pub status: Status,
    pub title: &'static str,
    pub thread_title: String,
}
impl Alert {
    /// The tag a system notification is replaced by: one per thread.
    pub fn tag(&self, environment: &str) -> String {
        format!("{environment}:{}", self.thread_id)
    }
    /// A toast's tone, as the original's `type`.
    pub fn level(&self) -> &'static str {
        match (self.kind, self.status) {
            (Kind::Completion, _) => "success",
            (_, Status::Failed) => "error",
            _ => "warning",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Seen {
    attention: Option<String>,
    completion: Option<i64>,
}

/// What one environment's threads were the last time they were scanned.
#[derive(Debug, Default)]
pub struct Tracker {
    seen: BTreeMap<String, Seen>,
}

impl Tracker {
    /// The environment is not live: the next scan starts from nothing.
    pub fn reset(&mut self) {
        self.seen.clear();
    }

    pub fn scan(&mut self, threads: &[ThreadShell]) -> Vec<Alert> {
        let mut next = BTreeMap::new();
        let mut alerts = Vec::new();
        for thread in threads {
            if thread.lineage.relationship_to_parent == Some(ThreadRelationship::Subagent) {
                continue;
            }
            let id = thread.id.to_string();
            let prior = self.seen.get(&id);
            let mut status = sidebar::status(thread);
            if status == Status::Ready && sidebar::latest_run_status(thread) == Some("failed") {
                status = Status::Failed;
            }
            let run = thread.latest_run_id.as_ref().map(ToString::to_string).unwrap_or_default();
            let attention = matches!(status, Status::Input | Status::Approval | Status::Failed | Status::Limited).then(|| format!("{run}:{}", status.as_str()));
            // Commands left running (a dev server) read as ready; subagents and monitors wait.
            let completed = sidebar::completed_at(thread).map(|at| at.timestamp_millis());
            let completion = if status == Status::Ready && sidebar::latest_run_status(thread) == Some("completed") && completed.is_some() { completed } else { prior.and_then(|prior| prior.completion) };
            next.insert(id.clone(), Seen { attention: attention.clone(), completion });
            let Some(prior) = prior else { continue };
            if thread.archived_at.is_some() {
                continue;
            }
            let kind = if attention.is_some() && attention != prior.attention {
                Kind::Input
            } else if completion.is_some_and(|completion| prior.completion.is_none_or(|before| completion > before)) {
                Kind::Completion
            } else {
                continue;
            };
            let title = match (kind, status) {
                (Kind::Completion, _) => "Thread completed",
                (_, Status::Approval) => "Approval needed",
                (_, Status::Limited) => "Usage limit reached",
                (_, Status::Failed) => "Thread failed",
                _ => "Input needed",
            };
            alerts.push(Alert { thread_id: id, kind, status, title, thread_title: thread.title.clone() });
        }
        self.seen = next;
        alerts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn thread(id: &str, status: &str, edit: impl FnOnce(&mut Value)) -> ThreadShell {
        let mut value = json!({
            "createdBy": "user", "creationSource": "web", "id": id, "projectId": "p", "title": format!("Thread {id}"),
            "providerInstanceId": "codex", "modelSelection": {"instanceId": "codex", "model": "m"}, "runtimeMode": "approval-required",
            "interactionMode": "default", "branch": null, "worktreePath": null, "lineage": {"parentThreadId": null, "relationshipToParent": null, "rootThreadId": id},
            "forkedFrom": null, "activeProviderThreadId": null, "latestRunId": "run-1", "activeRunId": null, "status": status,
            "pendingRuntimeRequest": null, "latestVisibleMessage": null, "latestUserMessageAt": null, "hasActionableProposedPlan": false, "pendingBackgroundTasks": [],
            "itemCount": 0, "visibleItemCount": 0, "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:01:00Z", "archivedAt": null,
            "settledOverride": null, "settledAt": null, "deletedAt": null
        });
        edit(&mut value);
        serde_json::from_value(value).expect("a thread shell")
    }

    #[test]
    fn a_thread_seen_for_the_first_time_only_records_its_state() {
        let mut tracker = Tracker::default();
        assert!(tracker.scan(&[thread("a", "completed", |_| {}), thread("b", "failed", |_| {})]).is_empty());
        assert!(tracker.scan(&[thread("a", "completed", |_| {}), thread("b", "failed", |_| {})]).is_empty(), "nothing changed");
    }

    #[test]
    fn a_run_that_completes_later_alerts_once() {
        let mut tracker = Tracker::default();
        tracker.scan(&[thread("a", "running", |_| {})]);
        let done = thread("a", "completed", |value| value["updatedAt"] = json!("2026-01-01T00:05:00Z"));
        let alerts = tracker.scan(std::slice::from_ref(&done));
        assert_eq!(alerts.len(), 1);
        assert_eq!((alerts[0].kind, alerts[0].title, alerts[0].level()), (Kind::Completion, "Thread completed", "success"));
        assert!(tracker.scan(&[done]).is_empty(), "the same completion is not announced again");
        let later = thread("a", "completed", |value| { value["latestRunId"] = json!("run-2"); value["updatedAt"] = json!("2026-01-01T00:09:00Z"); });
        assert_eq!(tracker.scan(&[later]).len(), 1, "a later run is");
    }

    #[test]
    fn needing_the_person_alerts_with_what_is_needed_and_only_when_it_changes() {
        let mut tracker = Tracker::default();
        tracker.scan(&[thread("a", "running", |_| {}), thread("b", "running", |_| {}), thread("c", "running", |_| {})]);
        let approval = |value: &mut Value| value["pendingRuntimeRequest"] = json!({"id": "r", "kind": "command", "createdAt": "2026-01-01T00:02:00Z"});
        let input = |value: &mut Value| value["pendingRuntimeRequest"] = json!({"id": "r", "kind": "user_input", "createdAt": "2026-01-01T00:02:00Z"});
        let alerts = tracker.scan(&[thread("a", "waiting", approval), thread("b", "waiting", input), thread("c", "failed", |_| {})]);
        let titles: Vec<_> = alerts.iter().map(|alert| (alert.thread_id.as_str(), alert.title, alert.level())).collect();
        assert_eq!(titles, [("a", "Approval needed", "warning"), ("b", "Input needed", "warning"), ("c", "Thread failed", "error")]);
        assert!(tracker.scan(&[thread("a", "waiting", approval), thread("b", "waiting", input), thread("c", "failed", |_| {})]).is_empty());
    }

    #[test]
    fn subagents_and_archived_threads_never_alert_and_a_reset_forgets() {
        let mut tracker = Tracker::default();
        let subagent = |status: &str| thread("s", status, |value| value["lineage"]["relationshipToParent"] = json!("subagent"));
        tracker.scan(&[subagent("running"), thread("a", "running", |_| {})]);
        let archived = thread("a", "failed", |value| value["archivedAt"] = json!("2026-01-02T00:00:00Z"));
        assert!(tracker.scan(&[subagent("failed"), archived]).is_empty());
        tracker.reset();
        assert!(tracker.scan(&[thread("a", "failed", |_| {})]).is_empty(), "after a reset a thread is new again");
    }

    #[test]
    fn the_modes_say_what_they_do() {
        assert!(has_sound(NotificationMode::Sound) && has_sound(NotificationMode::NotificationsAndSound) && !has_sound(NotificationMode::Notifications));
        assert!(has_desktop_notifications(NotificationMode::Notifications) && !has_desktop_notifications(NotificationMode::Sound) && !has_desktop_notifications(NotificationMode::Off));
    }
}
