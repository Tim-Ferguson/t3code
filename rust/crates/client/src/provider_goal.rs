//! The status line of a native `/goal`, which keeps the agent working across turns. Port of `presentProviderGoal`
//! (`threadExecution.ts`) and `formatDuration` (`orchestrationTiming.ts`). The shell's `goal` is the one on the provider thread
//! that owns the conversation; the strip above the composer and the row's "Goal" label both read it.
use serde_json::Value;

/// What the strip shows of a goal.
#[derive(Debug, Clone, PartialEq)]
pub struct Presentation {
    pub title: String,
    pub objective: String,
    pub usage: Option<String>,
    /// Pursuing again is offered for every state but one already pursued or finished.
    pub can_resume: bool,
}

fn tokens(count: u64) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    if count < 1_000_000 {
        return format!("{}k", (count as f64 / 1_000.0).round() as u64);
    }
    let millions = format!("{:.1}", count as f64 / 1_000_000.0);
    format!("{}m", millions.strip_suffix(".0").unwrap_or(&millions))
}

/// `formatDuration` for a span of whole seconds that is at least a minute: "1h 5m", "2m 3s".
fn duration(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3_600, seconds % 3_600 / 60, seconds % 60);
    [(hours, "h"), (minutes, "m"), (seconds, "s")].into_iter().filter(|(n, _)| *n > 0).map(|(n, unit)| format!("{n}{unit}")).collect::<Vec<_>>().join(" ")
}

/// `presentProviderGoal`: an active goal on an idle thread (Claude after Stop) is set, not pursued.
pub fn present(goal: &Value, working: bool) -> Presentation {
    let status = goal["status"].as_str().unwrap_or("active");
    let mut usage: Vec<String> = vec![];
    if let Some(used) = goal["tokensUsed"].as_u64().filter(|used| *used > 0) {
        usage.push(match goal["tokenBudget"].as_u64() {
            None => format!("{} tokens", tokens(used)),
            Some(budget) => format!("{} / {} tokens", tokens(used), tokens(budget)),
        });
    }
    if let Some(seconds) = goal["timeUsedSeconds"].as_u64().filter(|seconds| *seconds >= 60) {
        usage.push(duration(seconds));
    }
    if let Some(checks) = goal["checks"].as_u64().filter(|checks| *checks > 0) {
        usage.push(format!("{checks} {}", if checks == 1 { "check" } else { "checks" }));
    }
    let title = match status {
        "active" if !working => "Goal set",
        "active" => "Pursuing goal",
        "paused" => "Goal paused",
        "blocked" => "Goal blocked",
        "usage_limited" => "Goal hit a usage limit",
        "budget_limited" => "Goal reached its token budget",
        _ => "Goal complete",
    };
    Presentation { title: title.into(), objective: goal["objective"].as_str().unwrap_or_default().to_owned(), usage: (!usage.is_empty()).then(|| usage.join(" \u{b7} ")), can_resume: status != "active" && status != "complete" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_active_goal_is_set_while_the_thread_is_idle_and_pursued_while_it_works() {
        let goal = json!({"objective":"Ship it","status":"active"});
        assert_eq!(present(&goal, false).title, "Goal set");
        assert_eq!((present(&goal, true).title.as_str(), present(&goal, true).can_resume), ("Pursuing goal", false));
        let paused = json!({"objective":"Ship it","status":"paused"});
        assert_eq!((present(&paused, false).title.as_str(), present(&paused, false).can_resume), ("Goal paused", true));
        assert!(!present(&json!({"objective":"x","status":"complete"}), false).can_resume);
    }

    #[test]
    fn usage_reads_as_tokens_against_the_budget_then_time_then_checks() {
        let goal = json!({"objective":"x","status":"budget_limited","tokensUsed":12_400,"tokenBudget":2_000_000,"timeUsedSeconds":3_905,"checks":1});
        assert_eq!(present(&goal, false).usage.as_deref(), Some("12k / 2m tokens \u{b7} 1h 5m 5s \u{b7} 1 check"));
        assert_eq!(present(&json!({"objective":"x","status":"active","tokensUsed":900,"timeUsedSeconds":59}), false).usage.as_deref(), Some("900 tokens"));
        assert_eq!(present(&json!({"objective":"x","status":"active"}), false).usage, None);
        assert_eq!(tokens(1_500_000), "1.5m");
    }
}
