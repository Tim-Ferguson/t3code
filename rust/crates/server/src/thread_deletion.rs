//! The plan `thread.delete` commits, for a direct deletion and for each thread of a project
//! deletion (port of `planThreadDeletion` in `ThreadDeletion.ts`): the thread is marked deleted,
//! its active work is cancelled, its pending requests are cancelled, and its provider sessions are
//! detached. Cleanup that touches the disk or other services is left to effects that run after
//! the commit: `terminal.cleanup` closes the thread's terminals and deletes their history, and
//! `attachment.cleanup` removes the attachment files the thread's messages carry.
//!
//! Deliberately left behind, as in the original: the thread's git worktree and branch, its
//! checkpoint refs, scheduled tasks bound to it (their next run fails because the thread is gone),
//! and its events and projection (a deleted thread is a flag, not a removal).
use crate::{
    execution::{at, checked, event},
    persistence::{Event, NewEffect, StoreError},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

const ACTIVE: [&str; 5] = ["preparing", "queued", "starting", "running", "waiting"];

fn rows<'a>(projection: &'a Value, field: &str) -> &'a [Value] {
    projection[field].as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// What a deletion commits.
pub(crate) struct Plan {
    pub events: Vec<Event>,
    pub effects: Vec<NewEffect>,
}

/// `attachment_ids` are the files the thread's messages reference, in any order, repeats allowed.
pub(crate) fn plan(
    command: &Value,
    projection: &Value,
    attachment_ids: &[String],
    now: DateTime<Utc>,
) -> Result<Plan, StoreError> {
    let (thread_id, command_id) = (
        command["threadId"].as_str().unwrap_or(""),
        command["commandId"].as_str().unwrap_or(""),
    );
    let timestamp = at(now);
    let mut events = vec![];
    let mut effects = vec![];
    macro_rules! emit {
        ($kind:expr, $payload:expr $(,)?) => {
            events.push(event(thread_id, command_id, $kind, $payload, now))
        };
    }

    let mut thread = projection["thread"].clone();
    if thread["deletedAt"].is_null() {
        thread["deletedAt"] = json!(timestamp);
    }
    thread["titleRegeneration"] = Value::Null;
    thread["updatedAt"] = json!(timestamp);
    emit!("thread.deleted", thread);

    let active: Vec<&Value> = rows(projection, "runs")
        .iter()
        .filter(|run| run["status"].as_str().is_some_and(|status| ACTIVE.contains(&status)))
        .collect();
    let is_active = |run_id: &Value| active.iter().any(|run| run["id"] == *run_id);
    for run in &active {
        let mut run = (*run).clone();
        run["status"] = json!("cancelled");
        run["queuePosition"] = Value::Null;
        run["completedAt"] = json!(timestamp);
        emit!("run.updated", checked::<t3_contracts::Run>(run)?);
    }
    for attempt in rows(projection, "attempts") {
        if !is_active(&attempt["runId"]) || !matches!(attempt["status"].as_str(), Some("pending" | "running")) {
            continue;
        }
        let mut attempt = attempt.clone();
        attempt["status"] = json!("cancelled");
        attempt["completedAt"] = json!(timestamp);
        emit!("run-attempt.updated", checked::<t3_contracts::RunAttempt>(attempt)?);
    }
    for node in rows(projection, "nodes") {
        if node["runId"].is_null()
            || !is_active(&node["runId"])
            || !matches!(node["status"].as_str(), Some("pending" | "running" | "waiting"))
        {
            continue;
        }
        let mut node = node.clone();
        node["status"] = json!("cancelled");
        node["completedAt"] = json!(timestamp);
        emit!("node.updated", checked::<t3_contracts::ExecutionNode>(node)?);
    }
    for request in rows(projection, "runtimeRequests").iter().filter(|request| request["status"] == "pending") {
        let mut request = request.clone();
        request["status"] = json!("cancelled");
        request["responseCapability"] = json!({"type":"not_resumable","reason":"The thread was deleted."});
        request["resolvedAt"] = json!(timestamp);
        emit!("runtime-request.updated", checked::<t3_contracts::RuntimeRequest>(request)?);
    }
    // Delegated work still owing a completion is disposed. The projection used is the one with the
    // cancellations above, so disposing a cohort cannot revive its run.
    let mut current = projection.clone();
    for event in &events {
        if event.event_type == "run.updated" {
            if let Some(runs) = current["runs"].as_array_mut() {
                if let Some(run) = runs.iter_mut().find(|run| run["id"] == event.payload["id"]) {
                    *run = event.payload.clone();
                }
            }
        }
    }
    let cohorts = crate::delegated_tasks::dispose_cohorts(&current, &crate::delegated_tasks::cohort_run_ids(&current), "disposed", false, &timestamp);
    for (kind, payload) in cohorts {
        let payload = match kind {
            "run.updated" => checked::<t3_contracts::Run>(payload)?,
            _ => checked::<t3_contracts::Subagent>(payload)?,
        };
        events.push(event(thread_id, command_id, kind, payload, now));
    }
    for session in rows(projection, "providerSessions") {
        if matches!(session["status"].as_str(), Some("stopped" | "error")) {
            continue;
        }
        emit!(
            "provider-session.detached",
            json!({"providerSessionId":session["id"],"detachedAt":timestamp,"reason":"Thread deleted."}),
        );
        effects.push(NewEffect {
            id: format!("effect:{command_id}:provider-session.detach:{}", session["id"].as_str().unwrap_or("")),
            command_id: command_id.into(),
            thread_id: thread_id.into(),
            request: json!({"type":"provider-session.detach","providerSessionId":session["id"],"detail":"Thread deleted.","revokeMcpCredential":true}),
            available_at: timestamp.clone(),
        });
    }
    effects.push(NewEffect {
        id: format!("effect:{command_id}:terminal.cleanup"),
        command_id: command_id.into(),
        thread_id: thread_id.into(),
        request: json!({"type":"terminal.cleanup"}),
        available_at: timestamp.clone(),
    });
    let mut unique: Vec<&String> = vec![];
    for id in attachment_ids {
        if !unique.contains(&id) {
            unique.push(id);
        }
    }
    if !unique.is_empty() {
        effects.push(NewEffect {
            id: format!("effect:{command_id}:attachment.cleanup"),
            command_id: command_id.into(),
            thread_id: thread_id.into(),
            request: json!({"type":"attachment.cleanup","attachmentIds":unique}),
            available_at: timestamp,
        });
    }
    Ok(Plan { events, effects })
}

/// The files a thread's messages carry (`getThreadAttachmentIds`), in first-seen order. The
/// pages `html_render` and MCP Apps publish are not collected: nothing in this server publishes
/// them yet.
pub(crate) fn attachment_ids(projection: &Value) -> Vec<String> {
    let mut ids: Vec<String> = vec![];
    for message in rows(projection, "messages") {
        for attachment in message["attachments"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            if let Some(id) = attachment["id"].as_str() {
                if !ids.iter().any(|seen| seen == id) {
                    ids.push(id.to_owned());
                }
            }
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every scripted projection plans the same events and effects as the original.
    #[test]
    fn plans_match_the_original() {
        let now: DateTime<Utc> = "2026-09-04T00:00:00.000Z".parse().unwrap();
        let mut compared = 0;
        for line in include_str!("../tests/fixtures/thread-deletion.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let name = row["name"].as_str().unwrap();
            let ids: Vec<String> = row["attachmentIds"].as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect();
            let command = json!({"type":"thread.delete","commandId":"command:delete-plan","threadId":"thread:delete-plan"});
            let planned = plan(&command, &row["projection"], &ids, now).unwrap_or_else(|error| panic!("{name}: {error}"));
            let events: Vec<Value> = planned.events.iter().map(|event| json!({"type":event.event_type,"payload":event.payload})).collect();
            assert_eq!(Value::Array(events), row["output"]["events"], "{name}");
            let effects: Vec<Value> = planned.effects.iter().map(|effect| json!({"id":effect.id,"request":effect.request})).collect();
            assert_eq!(Value::Array(effects), row["output"]["effects"], "{name}");
            compared += 1;
        }
        assert_eq!(compared, 21);
    }

    #[test]
    fn attachments_are_collected_once_in_first_seen_order() {
        let projection = json!({"messages":[
            {"attachments":[{"id":"b"},{"id":"a"}]},
            {"attachments":[{"id":"a"},{"id":"c"}]},
            {"attachments":[]}
        ]});
        assert_eq!(attachment_ids(&projection), ["b", "a", "c"]);
    }
}
