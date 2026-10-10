//! The one quiet line after a conversation's last message for a thread that is snoozed, settled or
//! just woke: port of `threadStatusLine` of `ChatView.tsx` (`ThreadStatusLine.tsx`) with the relative
//! labels of `timestampFormat.ts`. Sending a message clears each of them, so the action is only the
//! explicit way out.
use crate::sidebar;
use chrono::{DateTime, Utc};
use t3_contracts::{SettledOverride, ThreadShell};

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// "Snoozed, 2h left" with "Wake now"; `wake_at` is the instant the snooze ends.
    Snoozed { label: String, wake_at: DateTime<Utc> },
    /// "Settled 3h ago" with "Un-settle".
    Settled { label: String },
    /// "Woke from snooze" with "Dismiss", which marks the wake as visited at `at`.
    Woke { at: String },
}

fn instant(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|at| at.with_timezone(&Utc))
}

/// `formatRelativeTimeUntilLabel`: "Soon", "45s left", "12m left", "3h left", "2d left", or "Expired".
pub fn until_label(until: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let remaining = (until - now).num_milliseconds();
    if remaining <= 0 {
        return "Expired".into();
    }
    let seconds = remaining / 1000;
    match seconds {
        0..=4 => "Soon".into(),
        5..=59 => format!("{seconds}s left"),
        _ if seconds < 3600 => format!("{}m left", seconds / 60),
        _ if seconds < 86_400 => format!("{}h left", seconds / 3600),
        _ => format!("{}d left", seconds / 86_400),
    }
}

/// `formatRelativeTimeLabel`: "just now", "5m ago", "3h ago", "2d ago".
pub fn ago_label(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - at).num_seconds();
    if seconds < 60 {
        return "just now".into();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 { format!("{hours}h ago") } else { format!("{}d ago", hours / 24) }
}

/// Which line the thread shows, if any: snoozed first, then settled, then a wake not yet visited.
/// A snooze counts only where the server snoozes and a settle only where it settles; the wake
/// needs the snooze capability and is hidden once the latest run completed at or after it (having
/// the thread open counts as a visit then).
pub fn line(thread: &ThreadShell, snooze: bool, settlement: bool, now: DateTime<Utc>) -> Option<Line> {
    if snooze {
        if let Some(wake_at) = sidebar::snoozed_until(thread, now) {
            return Some(Line::Snoozed { label: format!("Snoozed, {}", until_label(wake_at, now)), wake_at });
        }
    }
    if settlement && thread.settled_override == Some(SettledOverride::Settled) {
        let label = thread.settled_at.map(|at| format!("Settled {}", ago_label(at.0, now))).unwrap_or_else(|| "Settled".into());
        return Some(Line::Settled { label });
    }
    if snooze {
        let woke = sidebar::woke_at(thread, now)?;
        let completed = thread.latest_run_completed_at.flatten().map(|at| at.0);
        if completed.is_some_and(|completed| completed >= woke) {
            return None;
        }
        return Some(Line::Woke { at: woke.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) });
    }
    None
}

/// How long until the line next changes by itself: the wake of a snooze, else the next minute
/// (the labels count in minutes). None when nothing on the line moves.
pub fn next_change_ms(line: &Line, now: DateTime<Utc>) -> Option<u64> {
    const MINUTE: i64 = 60_000;
    match line {
        Line::Snoozed { wake_at, .. } => Some(((*wake_at - now).num_milliseconds() + 50).clamp(1, MINUTE) as u64),
        Line::Settled { .. } => Some(MINUTE as u64),
        Line::Woke { .. } => None,
    }
}

/// `recoveryOwnsSnooze`: a snooze the usage-limit recovery put the thread to sleep with is woken by
/// clearing the recovery's snooze, not by unsnoozing.
pub fn recovery_snooze(thread: &ThreadShell) -> Option<(String, String)> {
    let recovery = thread.limit_recovery.clone().flatten()?;
    let owns = recovery.snooze.flatten() == Some(true)
        && thread.latest_run_id.as_ref().is_some_and(|run| *run.as_str() == *recovery.run_id.as_str())
        && matches!((thread.snoozed_until.flatten().map(|at| at.0), instant(recovery.reset_at.as_str())), (Some(until), Some(reset)) if until == reset);
    owns.then(|| (recovery.run_id.to_string(), recovery.reset_at.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thread(extra: serde_json::Value) -> ThreadShell {
        let mut base = json!({"createdBy":"user","creationSource":"web","id":"t","projectId":"p","title":"T","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null,"lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"t"},"forkedFrom":null,"activeProviderThreadId":null,"latestRunId":null,"activeRunId":null,"status":"idle","pendingRuntimeRequest":null,"latestVisibleMessage":null,"latestUserMessageAt":null,"hasActionableProposedPlan":false,"itemCount":1,"visibleItemCount":1,"pendingBackgroundTasks":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","archivedAt":null,"settledOverride":null,"settledAt":null,"deletedAt":null});
        for (key, value) in extra.as_object().unwrap() {
            base[key] = value.clone();
        }
        serde_json::from_value(base).unwrap()
    }
    fn at(text: &str) -> DateTime<Utc> {
        instant(text).unwrap()
    }

    #[test]
    fn relative_labels_read_as_the_originals() {
        let now = at("2026-01-02T00:00:00Z");
        let after = |seconds: i64| now + chrono::Duration::seconds(seconds);
        assert_eq!([2, 5, 59, 60, 3599, 3600, 86_399, 86_400, 3 * 86_400].map(|s| until_label(after(s), now)), ["Soon", "5s left", "59s left", "1m left", "59m left", "1h left", "23h left", "1d left", "3d left"].map(String::from));
        assert_eq!(until_label(now, now), "Expired");
        let before = |seconds: i64| now - chrono::Duration::seconds(seconds);
        assert_eq!([0, 59, 60, 3599, 3600, 86_399, 172_800].map(|s| ago_label(before(s), now)), ["just now", "just now", "1m ago", "59m ago", "1h ago", "23h ago", "2d ago"].map(String::from));
        assert_eq!(ago_label(after(30), now), "just now");
    }

    #[test]
    fn a_thread_shows_one_line_snoozed_before_settled_before_woke() {
        let now = at("2026-01-02T00:00:00Z");
        let snoozed = thread(json!({"snoozedUntil":"2026-01-02T03:00:00Z","snoozedAt":"2026-01-01T23:00:00Z"}));
        assert_eq!(line(&snoozed, true, true, now), Some(Line::Snoozed { label: "Snoozed, 3h left".into(), wake_at: at("2026-01-02T03:00:00Z") }));
        // Without the snooze capability the thread is not snoozed.
        assert_eq!(line(&snoozed, false, true, now), None);
        let settled = thread(json!({"settledOverride":"settled","settledAt":"2026-01-01T21:00:00Z"}));
        assert_eq!(line(&settled, true, true, now), Some(Line::Settled { label: "Settled 3h ago".into() }));
        assert_eq!(line(&settled, true, false, now), None, "no settlement, no settled thread");
        assert_eq!(line(&thread(json!({"settledOverride":"settled"})), true, true, now), Some(Line::Settled { label: "Settled".into() }));
        // Both: the snooze is the line.
        let both = thread(json!({"settledOverride":"settled","settledAt":"2026-01-01T21:00:00Z","snoozedUntil":"2026-01-02T03:00:00Z","snoozedAt":"2026-01-01T23:00:00Z"}));
        assert!(matches!(line(&both, true, true, now), Some(Line::Snoozed { .. })));
        // A timer that has gone off and was not visited since: woke, until visited or a run completed since.
        let woke = thread(json!({"snoozedUntil":"2026-01-01T23:00:00Z","snoozedAt":"2026-01-01T20:00:00Z"}));
        assert_eq!(line(&woke, true, true, now), Some(Line::Woke { at: "2026-01-01T23:00:00.000Z".into() }));
        let visited = thread(json!({"snoozedUntil":"2026-01-01T23:00:00Z","snoozedAt":"2026-01-01T20:00:00Z","lastVisitedAt":"2026-01-01T23:30:00Z"}));
        assert_eq!(line(&visited, true, true, now), None);
        assert_eq!(line(&thread(json!({})), true, true, now), None);
    }

    #[test]
    fn the_line_changes_at_the_wake_and_at_most_a_minute_apart() {
        let now = at("2026-01-02T00:00:00Z");
        let snoozed = Line::Snoozed { label: String::new(), wake_at: now + chrono::Duration::seconds(20) };
        assert_eq!(next_change_ms(&snoozed, now), Some(20_050));
        assert_eq!(next_change_ms(&Line::Snoozed { label: String::new(), wake_at: now + chrono::Duration::hours(5) }, now), Some(60_000));
        assert_eq!(next_change_ms(&Line::Settled { label: String::new() }, now), Some(60_000));
        assert_eq!(next_change_ms(&Line::Woke { at: String::new() }, now), None);
    }

    #[test]
    fn a_snooze_the_limit_recovery_set_is_woken_by_clearing_the_recovery() {
        let reset = "2026-01-02T03:00:00.000Z";
        let owned = thread(json!({"latestRunId":"r","snoozedUntil":"2026-01-02T03:00:00Z","limitRecovery":{"runId":"r","resetAt":reset,"autoResume":false,"snooze":true,"requestId":"c"}}));
        assert_eq!(recovery_snooze(&owned), Some(("r".into(), reset.into())));
        let other_run = thread(json!({"latestRunId":"x","snoozedUntil":"2026-01-02T03:00:00Z","limitRecovery":{"runId":"r","resetAt":reset,"autoResume":false,"snooze":true,"requestId":"c"}}));
        assert_eq!(recovery_snooze(&other_run), None);
        let user_snooze = thread(json!({"latestRunId":"r","snoozedUntil":"2026-01-02T05:00:00Z","limitRecovery":{"runId":"r","resetAt":reset,"autoResume":false,"snooze":true,"requestId":"c"}}));
        assert_eq!(recovery_snooze(&user_snooze), None);
    }
}
