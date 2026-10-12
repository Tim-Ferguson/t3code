//! The tasks badge over the composer: how far the agent's plan of the running turn has got, and the steps behind it. Port of
//! `activeComposerTasksProgress` (`ChatView.tsx`), `deriveActivePlanState` (`session-logic.ts`) and the words of
//! `ComposerTasksBadge.tsx`. The badge shows only while the latest run is unsettled and owns the plan.
use serde_json::Value;

/// More steps than this have no segment bar; it would be a smear.
pub const MAX_SEGMENTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    InProgress,
    Completed,
}
impl Status {
    /// What a screen reader is told of the step (`taskStatusLabels`).
    pub fn label(self) -> &'static str {
        match self {
            Status::Pending => "Pending",
            Status::InProgress => "Running",
            Status::Completed => "Completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub step: String,
    pub status: Status,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// The step in progress, else the first pending one, else the last.
    pub step: String,
    pub completed_steps: usize,
    pub total_steps: usize,
}

/// The plan of the latest run and how far it has got, or none when that run is over, has no plan of its own, or the plan has no steps.
/// `settled` is whether the latest run has ended.
pub fn active(projection: &Value, settled: bool) -> Option<(Progress, Vec<Step>)> {
    if settled {
        return None;
    }
    let latest_run = projection["runs"].as_array()?.last().and_then(|run| run["id"].as_str());
    let plans: Vec<&Value> = projection["plans"].as_array()?.iter().filter(|plan| plan["kind"] == "todo_list").collect();
    let plan = plans.iter().rev().find(|plan| plan["runId"].as_str().is_some() && plan["runId"].as_str() == latest_run).or(plans.last())?;
    // The plan must belong to the run in front (`activePlan.runId !== (activeActivityRun?.runId ?? null)`).
    if plan["runId"].as_str() != latest_run {
        return None;
    }
    let steps: Vec<Step> = plan["steps"]
        .as_array()?
        .iter()
        .map(|step| Step {
            step: step["text"].as_str().unwrap_or_default().to_owned(),
            status: match step["status"].as_str() {
                Some("running") => Status::InProgress,
                Some("completed") => Status::Completed,
                _ => Status::Pending,
            },
            duration_ms: step["durationMs"].as_u64(),
        })
        .collect();
    let last = steps.last()?;
    let current = steps.iter().find(|step| step.status == Status::InProgress).or_else(|| steps.iter().find(|step| step.status == Status::Pending)).unwrap_or(last);
    let progress = Progress { step: current.step.clone(), completed_steps: steps.iter().filter(|step| step.status == Status::Completed).count(), total_steps: steps.len() };
    Some((progress, steps))
}

/// The badge's label (`aria-label` of its row).
pub fn badge_label(expanded: bool, progress: &Progress) -> String {
    format!("{}: {} of {} complete. Current task: {}", if expanded { "Collapse tasks" } else { "Tasks" }, progress.completed_steps, progress.total_steps, progress.step)
}
/// The label of the expanded list.
pub fn list_label(progress: &Progress) -> String {
    format!("Task list. {} of {} complete.", progress.completed_steps, progress.total_steps)
}
/// What the duration column says of a step: how long it took, or "now" while it runs.
pub fn duration_label(step: &Step) -> Option<String> {
    match (step.duration_ms, step.status) {
        (Some(ms), _) => Some(crate::timeline_rows::format_duration(ms as f64)),
        (None, Status::InProgress) => Some("now".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn projection(plans: Value) -> Value {
        json!({"runs":[{"id":"run-1","status":"running"},{"id":"run-2","status":"running"}],"plans":plans})
    }
    fn plan(run: &str, steps: Value) -> Value {
        json!({"id":format!("plan-{run}"),"kind":"todo_list","runId":run,"steps":steps})
    }

    #[test]
    fn progress_names_the_step_in_progress_then_the_first_pending_then_the_last() {
        let steps = json!([{"text":"Read","status":"completed","durationMs":1500},{"text":"Edit","status":"running"},{"text":"Test","status":"pending"}]);
        let (progress, listed) = active(&projection(json!([plan("run-2", steps)])), false).unwrap();
        assert_eq!(progress, Progress { step: "Edit".into(), completed_steps: 1, total_steps: 3 });
        assert_eq!(listed.iter().map(|step| step.status).collect::<Vec<_>>(), [Status::Completed, Status::InProgress, Status::Pending]);
        assert_eq!((duration_label(&listed[0]).as_deref(), duration_label(&listed[1]).as_deref(), duration_label(&listed[2])), (Some("1.5s"), Some("now"), None));
        let pending = json!([{"text":"Read","status":"completed"},{"text":"Test","status":"pending"},{"text":"Ship","status":"pending"}]);
        assert_eq!(active(&projection(json!([plan("run-2", pending)])), false).unwrap().0.step, "Test");
        let done = json!([{"text":"Read","status":"completed"},{"text":"Ship","status":"completed"}]);
        assert_eq!(active(&projection(json!([plan("run-2", done)])), false).unwrap().0, Progress { step: "Ship".into(), completed_steps: 2, total_steps: 2 });
    }

    #[test]
    fn a_settled_run_or_one_that_does_not_own_the_plan_or_a_plan_without_steps_has_no_badge() {
        let steps = json!([{"text":"Read","status":"running"}]);
        assert!(active(&projection(json!([plan("run-2", steps.clone())])), true).is_none(), "the run is over");
        assert!(active(&projection(json!([plan("run-1", steps.clone())])), false).is_none(), "an earlier run's plan");
        assert!(active(&projection(json!([plan("run-2", json!([]))])), false).is_none());
        assert!(active(&projection(json!([])), false).is_none());
        // The latest run's own plan wins over a later-listed plan of another run.
        let both = json!([plan("run-2", steps), plan("run-1", json!([{"text":"Old","status":"pending"}]))]);
        assert_eq!(active(&projection(both), false).unwrap().0.step, "Read");
    }

    #[test]
    fn the_badge_and_list_say_how_far_it_has_got() {
        let progress = Progress { step: "Edit".into(), completed_steps: 1, total_steps: 3 };
        assert_eq!(badge_label(false, &progress), "Tasks: 1 of 3 complete. Current task: Edit");
        assert_eq!(badge_label(true, &progress), "Collapse tasks: 1 of 3 complete. Current task: Edit");
        assert_eq!(list_label(&progress), "Task list. 1 of 3 complete.");
        assert_eq!(Status::InProgress.label(), "Running");
    }
}
