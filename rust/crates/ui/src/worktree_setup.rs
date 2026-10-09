//! Worktree setup: the progress card of a thread whose worktree is being prepared (with cancel),
//! and the retry a failed preparation offers. Port of `WorktreeSetupCard.tsx` and the retry row of
//! `MessagesTimeline.tsx`.
//!
//! The server sends the current snapshot (or null) on subscribing, then each change, and keeps
//! the final state for about thirty seconds. The stream is held only while the card is mounted.
//! Unlike the original, elapsed times are shown for stages that have settled and not ticked each
//! second while one runs: nothing here repaints on a clock.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::rc::Rc;
use t3_contracts::AuthEnvironmentScope;

/// The code a failed preparation carries on its error item.
pub(crate) const FAILURE_CODE: &str = "workspace_preparation_failed";

pub(crate) fn stage_label(id: &str) -> &'static str {
    match id {
        "fetch" => "Fetch base branch",
        "checkout" => "Check out files",
        "submodules" => "Init submodules",
        "setup-script" => "Run setup script",
        "agent" => "Start agent",
        _ => "Step",
    }
}
pub(crate) fn header_label(snapshot: &Value) -> &'static str {
    match snapshot["phase"].as_str() {
        Some("running") => "Setting up worktree\u{2026}",
        Some("done") if has_failed_stage(snapshot) => "Worktree ready, setup script failed",
        Some("done") => "Worktree ready",
        Some("failed") => "Worktree setup failed",
        Some("cancelled") => "Worktree setup cancelled",
        _ => "Worktree setup",
    }
}
fn has_failed_stage(snapshot: &Value) -> bool {
    snapshot["stages"].as_array().into_iter().flatten().any(|stage| stage["status"] == "failed")
}
fn mark(status: &str) -> &'static str {
    match status {
        "done" => "\u{2713}",
        "running" => "\u{25cf}",
        "failed" => "\u{2715}",
        "warning" => "!",
        "skipped" => "\u{2212}",
        _ => "\u{25cb}",
    }
}
/// "1.2s", "2m 3s": how long a settled stage took.
pub(crate) fn duration(from: &Value, to: &Value) -> Option<String> {
    let parse = |value: &Value| chrono::DateTime::parse_from_rfc3339(value.as_str()?).ok();
    let millis = (parse(to)? - parse(from)?).num_milliseconds().max(0);
    Some(if millis < 1000 { format!("{millis}ms") } else if millis < 60_000 { format!("{:.1}s", millis as f64 / 1000.0) } else { format!("{}m {}s", millis / 60_000, (millis % 60_000) / 1000) })
}

#[component]
pub(crate) fn Card(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    let mut snapshot = use_signal(|| None::<Rc<Value>>);
    let mut stale = use_signal(|| false);
    let mut details = use_signal(|| false);
    let mut cancelling = use_signal(|| false);
    let readable = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationRead));
    let operable = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationOperate));

    // Held while the card is mounted and the connection can read; a lost connection marks what was
    // seen as old instead of clearing it.
    {
        let (transport, thread_id) = (transport.clone(), thread_id.clone());
        use_resource(move || {
            let (transport, thread_id, readable) = (transport.clone(), thread_id.clone(), readable());
            async move {
                if !readable {
                    if snapshot.peek().is_some() {
                        stale.set(true);
                    }
                    return;
                }
                let Ok(mut stream) = runtime::request_stream(&transport, state, "subscribeWorktreeSetup", json!({"threadId":thread_id})) else { return };
                stale.set(false);
                while let Some(chunk) = stream.next().await {
                    let Ok(values) = chunk else { break };
                    // Each value replaces the last: a snapshot of this thread, or null.
                    if let Some(last) = values.into_iter().next_back() {
                        let next = (!last.is_null() && last["threadId"] == thread_id.as_str()).then(|| Rc::new(last));
                        if *snapshot.peek() != next {
                            snapshot.set(next);
                        }
                    }
                }
                if snapshot.peek().is_some() {
                    stale.set(true);
                }
            }
        });
    }
    let cancel = {
        let (transport, thread_id) = (transport.clone(), thread_id.clone());
        use_callback(move |_: ()| {
            if *cancelling.peek() || !*operable.peek() {
                return;
            }
            cancelling.set(true);
            let (transport, thread_id) = (transport.clone(), thread_id.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "worktreeSetup.cancel", json!({"threadId":thread_id})).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match answer {
                        Ok(result) if result["cancelled"] == false => notice::push(state, Level::Info, "Worktree setup", "Nothing was running to cancel."),
                        Ok(_) => {}
                        Err(message) => notice::push(state, Level::Error, "Could not cancel worktree setup", message),
                    }
                }
                cancelling.set(false);
            });
        })
    };

    let Some(shot) = snapshot.read().clone() else { return rsx! {} };
    let phase = shot["phase"].as_str().unwrap_or_default().to_owned();
    // A setup that finished cleanly leaves nothing to show; the agent's turn takes its place.
    if phase == "done" && !has_failed_stage(&shot) {
        return rsx! {};
    }
    let running = phase == "running";
    let script_name = shot["setupScript"]["name"].as_str().map(str::to_owned);
    let total = duration(&shot["startedAt"], &shot["endedAt"]);
    let stages = shot["stages"].as_array().cloned().unwrap_or_default();
    let tone = match phase.as_str() { "failed" => "failed", "cancelled" => "failed", "done" => "warning", _ => "running" };
    rsx! {
        section { class: "worktree-setup", "aria-label": "Worktree setup", "data-worktree-setup-phase": "{phase}", "data-tone": "{tone}",
            header { class: "worktree-setup-header", role: "status",
                span { "{header_label(&shot)}" }
                if let Some(total) = total { span { class: "muted", "{total}" } }
            }
            if *stale.read() { p { class: "muted", role: "status", "Offline. This is the last progress seen." } }
            ol { class: "worktree-setup-stages",
                for stage in stages.iter() {
                    {
                        let id = stage["id"].as_str().unwrap_or_default().to_owned();
                        let status = stage["status"].as_str().unwrap_or("pending").to_owned();
                        let label = if id == "setup-script" { script_name.clone().unwrap_or_else(|| stage_label(&id).to_owned()) } else { stage_label(&id).to_owned() };
                        let trailing = match status.as_str() {
                            "pending" => None,
                            "skipped" => Some(stage["detail"].as_str().unwrap_or("skipped").to_owned()),
                            "running" if id == "checkout" && stage["percent"].is_number() => Some(format!("{}%", stage["percent"])),
                            _ => stage["detail"].as_str().map(str::to_owned),
                        };
                        let took = (status != "pending" && status != "running" && status != "skipped").then(|| duration(&stage["startedAt"], &stage["endedAt"])).flatten();
                        let tail: Vec<String> = stage["tail"].as_array().into_iter().flatten().filter_map(|line| line.as_str().map(str::to_owned)).collect();
                        let show_tail = id == "setup-script" && !tail.is_empty() && (status == "failed" || status == "running");
                        rsx! {
                            li { key: "{id}", "data-worktree-setup-stage": "{id}", "data-worktree-setup-status": "{status}",
                                span { class: "worktree-setup-mark", "aria-hidden": "true", "{mark(&status)}" }
                                span { class: "worktree-setup-label", "{label}" }
                                span { class: "sr-only", "{status}" }
                                if let Some(trailing) = trailing { span { class: "muted", "{trailing}" } }
                                if let Some(took) = took { span { class: "muted", "{took}" } }
                                if show_tail { pre { class: "worktree-setup-tail", for line in tail.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev() { "{line}\n" } } }
                            }
                        }
                    }
                }
            }
            if phase == "failed" { if let Some(error) = shot["error"].as_str() { p { class: "muted", "{error}" } } }
            if *details.read() {
                dl { class: "worktree-setup-details",
                    for (term, key) in [("Branch", "branch"), ("Base", "baseRef"), ("Path", "worktreePath")] {
                        if let Some(value) = shot[key].as_str() { dt { "{term}" } dd { "{value}" } }
                    }
                    if let Some(command) = shot["setupScript"]["command"].as_str() { dt { "Setup" } dd { "{command}" } }
                }
            }
            div { class: "worktree-setup-actions",
                button { r#type: "button", "aria-label": "Worktree setup details", "aria-expanded": "{details()}", onclick: move |_| details.set(!details()), "Details" }
                if running {
                    button { r#type: "button", "aria-label": "Cancel worktree setup", title: if operable() { "" } else { "This connection cannot cancel worktree setup." }, disabled: *cancelling.read() || !operable() || *stale.read(), onclick: move |_| cancel.call(()), "Cancel" }
                }
            }
        }
    }
}

/// "Retry" on the error a failed preparation leaves: one retry per failed run, enabled again by
/// the server's answer (the run is preparing again, or the retry was refused).
#[component]
pub(crate) fn Retry(state: Store<UiModel>, transport: TransportHandle, thread_id: String, run_id: String) -> Element {
    let mut sending = use_signal(|| false);
    let operable = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationOperate));
    let send = {
        let (transport, thread_id, run_id) = (transport.clone(), thread_id.clone(), run_id.clone());
        use_callback(move |_: ()| {
            if *sending.peek() || !*operable.peek() {
                return;
            }
            sending.set(true);
            let (transport, thread_id, run_id) = (transport.clone(), thread_id.clone(), run_id.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let payload = json!({"type":"prepared-run.retry","commandId":uuid::Uuid::new_v4().to_string(),"threadId":thread_id,"runId":run_id});
                let answer = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", payload).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    if let Err(message) = answer {
                        notice::push(state, Level::Error, "Could not retry", message);
                    }
                }
                sending.set(false);
            });
        })
    };
    rsx! { button { r#type: "button", "aria-label": "Retry workspace preparation", title: if operable() { "" } else { "This connection cannot retry." }, disabled: *sending.read() || !operable(), onclick: move |_| send.call(()), "Retry" } }
}

/// The runs whose workspace preparation failed and can be run again: the run failed in its
/// preparation and its error item is still failed (`workspacePreparationRetryRunIds`). A retry
/// turns that item cancelled, so the offer goes with it.
pub(crate) fn retryable_runs(projection: &Value) -> Vec<String> {
    let failed: Vec<&str> = projection["runs"].as_array().into_iter().flatten().filter(|run| run["status"] == "failed" && !run["workspacePreparation"].is_null() && run.get("workspacePreparation").is_some()).filter_map(|run| run["id"].as_str()).collect();
    let mut found = Vec::new();
    for item in projection["turnItems"].as_array().into_iter().flatten() {
        if item["type"] == "error" && item["status"] == "failed" && item["failure"]["code"] == FAILURE_CODE {
            if let Some(run) = item["runId"].as_str().filter(|run| failed.contains(run) && !found.iter().any(|seen| seen == run)) {
                found.push(run.to_owned());
            }
        }
    }
    found
}

/// What a failed preparation offers: the retry, beside the card, wherever the error item sits in
/// the timeline. Reads the thread's projection on its own, so the conversation is not rebuilt.
#[component]
pub(crate) fn RetryBar(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    let runs = use_memo(move || state.thread().read().projection.as_ref().map(retryable_runs).unwrap_or_default());
    rsx! {
        for run_id in runs() {
            div { key: "{run_id}", class: "worktree-retry", role: "alert",
                span { "Workspace preparation failed." }
                Retry { state, transport: transport.clone(), thread_id: thread_id.clone(), run_id: run_id.clone() }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_preparation_of_a_failed_run_offers_a_retry_and_nothing_else_does() {
        let item = |status: &str, code: &str| json!({"type":"error","status":status,"runId":"r1","failure":{"code":code,"message":"Setup script exited with 3."}});
        let run = |status: &str, prepared: bool| json!({"id":"r1","status":status,"workspacePreparation": if prepared { json!({"type":"worktree"}) } else { Value::Null }});
        let projection = |item: Value, run: Value| json!({"runs":[run],"turnItems":[item]});
        assert_eq!(retryable_runs(&projection(item("failed", FAILURE_CODE), run("failed", true))), ["r1"]);
        assert!(retryable_runs(&projection(item("cancelled", FAILURE_CODE), run("failed", true))).is_empty(), "a retried failure no longer offers one");
        assert!(retryable_runs(&projection(item("failed", "other"), run("failed", true))).is_empty());
        assert!(retryable_runs(&projection(item("failed", FAILURE_CODE), run("completed", true))).is_empty());
        assert!(retryable_runs(&projection(item("failed", FAILURE_CODE), run("failed", false))).is_empty(), "a run that was not prepared has nothing to prepare again");
        assert!(retryable_runs(&json!({})).is_empty());
    }

    #[test]
    fn the_card_words_each_phase_and_each_stage_in_the_originals_words() {
        assert_eq!(header_label(&json!({"phase":"running"})), "Setting up worktree\u{2026}");
        assert_eq!(header_label(&json!({"phase":"done","stages":[{"status":"failed"}]})), "Worktree ready, setup script failed");
        assert_eq!(header_label(&json!({"phase":"done","stages":[]})), "Worktree ready");
        assert_eq!(header_label(&json!({"phase":"failed"})), "Worktree setup failed");
        assert_eq!(header_label(&json!({"phase":"cancelled"})), "Worktree setup cancelled");
        assert_eq!(["fetch", "checkout", "submodules", "setup-script", "agent"].map(stage_label), ["Fetch base branch", "Check out files", "Init submodules", "Run setup script", "Start agent"]);
        assert_eq!(duration(&json!("2026-01-01T00:00:00Z"), &json!("2026-01-01T00:00:01.250Z")).as_deref(), Some("1.2s"));
        assert_eq!(duration(&json!("2026-01-01T00:00:00Z"), &json!("2026-01-01T00:02:03Z")).as_deref(), Some("2m 3s"));
        assert_eq!(duration(&json!("2026-01-01T00:00:00Z"), &Value::Null), None);
    }
}
