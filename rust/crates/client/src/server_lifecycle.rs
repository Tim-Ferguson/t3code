//! What a client does with the server's lifecycle stream (`subscribeServerLifecycle`) when it connects:
//! port of `handleWelcome` in `routes/__root.tsx` and `LegacyThreadMigrationToast.tsx`. The server says
//! `welcome` when startup has finished its recovery (naming the thread a first run opens), `ready` once
//! commands are accepted (nothing to show here: only a launcher-managed update reads its outcome), and
//! `legacyThreadMigration` while threads of the previous version are being restored.
use serde_json::Value;

pub const MIGRATION_TITLE: &str = "Restoring your threads\u{2026}";

/// What a lifecycle event asks the client to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Open the thread a first run bootstraps: only from the empty start screen, once per thread.
    OpenBootstrapThread(String),
    /// Show the persistent notice that threads are being restored, with its text.
    ShowMigration(String),
    /// The restoration is over (or never was): the notice goes.
    ClearMigration,
}

/// `Number.prototype.toLocaleString("en-US")` for a count: thousands separated by commas.
fn grouped(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}
pub fn migration_text(total: u64) -> String {
    format!("Migrating {} {} from the previous version. You can keep working while this finishes.", grouped(total), if total == 1 { "thread" } else { "threads" })
}

/// The actions for one event. `handled` is the bootstrap thread already opened, and `on_start_screen`
/// whether the client shows the empty start screen (the original: the route is "/").
pub fn actions(event: &Value, handled: Option<&str>, on_start_screen: bool) -> Vec<Action> {
    let payload = &event["payload"];
    match event["type"].as_str() {
        Some("welcome") => {
            let thread = payload["bootstrapThreadId"].as_str().filter(|_| payload["bootstrapProjectId"].as_str().is_some());
            match thread {
                Some(thread) if on_start_screen && handled != Some(thread) => vec![Action::OpenBootstrapThread(thread.to_owned())],
                _ => vec![],
            }
        }
        Some("legacyThreadMigration") => match (payload["status"].as_str(), payload["totalThreadCount"].as_u64()) {
            (Some("running"), Some(total)) => vec![Action::ShowMigration(migration_text(total))],
            _ => vec![Action::ClearMigration],
        },
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_first_run_opens_its_bootstrap_thread_once_and_only_from_the_start_screen() {
        let welcome = json!({"type":"welcome","payload":{"bootstrapProjectId":"p","bootstrapThreadId":"t"}});
        assert_eq!(actions(&welcome, None, true), [Action::OpenBootstrapThread("t".into())]);
        assert!(actions(&welcome, Some("t"), true).is_empty(), "once");
        assert!(actions(&welcome, None, false).is_empty(), "not over a thread the user has open");
        assert!(actions(&json!({"type":"welcome","payload":{"bootstrapThreadId":"t"}}), None, true).is_empty(), "a thread needs its project");
        assert!(actions(&json!({"type":"welcome","payload":{}}), None, true).is_empty());
        assert!(actions(&json!({"type":"ready","payload":{"at":"x"}}), None, true).is_empty());
    }

    #[test]
    fn restoring_threads_is_a_notice_for_as_long_as_it_runs() {
        let running = |total: u64| json!({"type":"legacyThreadMigration","payload":{"status":"running","totalThreadCount":total}});
        assert_eq!(actions(&running(1), None, true), [Action::ShowMigration("Migrating 1 thread from the previous version. You can keep working while this finishes.".into())]);
        assert_eq!(actions(&running(12_345), None, true), [Action::ShowMigration("Migrating 12,345 threads from the previous version. You can keep working while this finishes.".into())]);
        assert_eq!(actions(&json!({"type":"legacyThreadMigration","payload":{"status":"complete","totalThreadCount":3}}), None, true), [Action::ClearMigration]);
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }
}
