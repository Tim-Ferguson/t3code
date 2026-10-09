//! What a provider's report of finished background work says to the user (`Notification.ts`): the
//! notification a wake message carries, and the timeline item such a message is shown as. A report is
//! `{kind, label?, outcome, exitCode?, childThreadId?}` where kind is `subagent`, `command`, `monitor`
//! or `background_task`.
use serde_json::{Value, json};

const LABEL_MAX_LENGTH: usize = 80;

fn report_label(report: &Value) -> Option<String> {
    let first_line = report["label"].as_str()?.trim().split('\n').next()?.trim().to_owned();
    if first_line.is_empty() {
        return None;
    }
    let units: Vec<u16> = first_line.encode_utf16().collect();
    Some(if units.len() > LABEL_MAX_LENGTH { format!("{}…", String::from_utf16_lossy(&units[..LABEL_MAX_LENGTH - 1])) } else { first_line })
}
fn noun(kind: &str, plural: bool) -> &'static str {
    match (kind, plural) {
        ("subagent", false) => "Subagent",
        ("subagent", true) => "subagents",
        ("command", false) => "Command",
        ("command", true) => "commands",
        ("monitor", false) => "Monitor",
        ("monitor", true) => "monitors",
        (_, false) => "Background task",
        (_, true) => "background tasks",
    }
}
fn outcome_verb(kind: &str, outcome: &str) -> &'static str {
    match outcome {
        "failed" => "failed",
        "cancelled" => "was stopped",
        "updated" => {
            if kind == "monitor" {
                "reported new output"
            } else {
                "updated"
            }
        }
        _ => "finished",
    }
}
fn combined_outcome(outcomes: &[&str]) -> &'static str {
    if outcomes.contains(&"failed") {
        "failed"
    } else if outcomes.contains(&"cancelled") {
        "cancelled"
    } else if !outcomes.is_empty() && outcomes.iter().all(|outcome| *outcome == "completed") {
        "completed"
    } else if outcomes.contains(&"updated") {
        "updated"
    } else {
        "unknown"
    }
}
fn exit_suffix(report: &Value) -> String {
    match (report["kind"].as_str(), report["exitCode"].as_i64()) {
        (Some("command"), Some(code)) => format!(" (exit {code})"),
        _ => String::new(),
    }
}
fn named_report(report: &Value, capitalize: bool) -> String {
    let noun = noun(report["kind"].as_str().unwrap_or_default(), false);
    let label = report_label(report);
    let noun = if capitalize { noun.to_owned() } else { noun.to_lowercase() };
    format!("{noun}{}{}", label.map_or(String::new(), |label| format!(" \"{label}\"")), exit_suffix(report))
}
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}
fn reports_summary(reports: &[Value], outcome: &str) -> String {
    let first = &reports[0];
    let kind = first["kind"].as_str().unwrap_or_default();
    if reports.len() == 1 {
        let label = report_label(first);
        return format!("{}{} {}{}", noun(kind, false), label.map_or(String::new(), |label| format!(" \"{label}\"")), outcome_verb(kind, first["outcome"].as_str().unwrap_or_default()), exit_suffix(first));
    }
    let verb = match outcome {
        "failed" => "failed",
        "cancelled" => "were stopped",
        "updated" => "updated",
        _ => "finished",
    };
    if reports.len() > 3 || reports.iter().all(|report| report_label(report).is_none()) {
        let same = reports.iter().all(|report| report["kind"] == first["kind"]);
        return format!("{} {} {verb}", reports.len(), if same { noun(kind, true) } else { "background tasks" });
    }
    let names: Vec<String> = reports.iter().enumerate().map(|(index, report)| named_report(report, index == 0)).collect();
    format!("{} {verb}", join_names(&names))
}
fn reports_source(reports: &[Value]) -> Value {
    let first = &reports[0];
    if reports[1..].iter().any(|report| report["kind"] != first["kind"]) {
        return json!({"kind":"background_task"});
    }
    match first["kind"].as_str() {
        // One subagent is the thing the row opens; several open nothing.
        Some("subagent") if reports.len() == 1 && first["childThreadId"].is_string() => json!({"kind":"subagent","childThreadId":first["childThreadId"]}),
        Some("subagent") => json!({"kind":"subagent"}),
        Some(kind) => json!({"kind":kind}),
        None => json!({"kind":"background_task"}),
    }
}

/// `backgroundWorkNotification`: none when nothing is known, so the caller keeps its generic one.
pub(crate) fn background_work_notification(reports: &[Value]) -> Option<Value> {
    if reports.is_empty() {
        return None;
    }
    let outcomes: Vec<&str> = reports.iter().map(|report| report["outcome"].as_str().unwrap_or("unknown")).collect();
    let outcome = combined_outcome(&outcomes);
    Some(json!({"source":reports_source(reports),"outcome":outcome,"summary":reports_summary(reports, outcome)}))
}

/// `notificationTurnItem` for a message that carries a notification: its timeline item is the
/// notification, not the message text. Delegated completions are not ported.
pub(crate) fn notification_turn_item(mut item: Value, message: &Value) -> Value {
    if item["type"] != "user_message" || message["delegatedCompletion"].as_object().is_some() {
        return item;
    }
    let Some(notification) = message["notification"].as_object() else { return item };
    let object = item.as_object_mut().unwrap();
    for key in ["messageId", "inputIntent", "text", "attachments", "createdBy", "creationSource", "scheduledTaskId"] {
        object.remove(key);
    }
    object.insert("type".into(), json!("notification"));
    for (key, value) in notification {
        object.insert(key.clone(), value.clone());
    }
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_read_as_the_user_should() {
        let one = background_work_notification(&[json!({"kind":"command","label":"sleep 5\nrest","outcome":"completed","exitCode":0})]).unwrap();
        assert_eq!(one, json!({"source":{"kind":"command"},"outcome":"completed","summary":"Command \"sleep 5\" finished (exit 0)"}));
        let two = background_work_notification(&[json!({"kind":"subagent","label":"A","outcome":"completed"}), json!({"kind":"subagent","label":"B","outcome":"cancelled"})]).unwrap();
        assert_eq!(two["summary"], "Subagent \"A\" and subagent \"B\" were stopped");
        assert_eq!(two["outcome"], "cancelled");
        assert!(background_work_notification(&[]).is_none());
    }
}
