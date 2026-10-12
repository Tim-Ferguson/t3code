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

/// The files a thread carries (`getThreadAttachmentIds`), in first-seen order: those its messages reference, then the pages its tool calls
/// published (`html_render` pages and captured MCP App documents).
pub(crate) fn attachment_ids(projection: &Value) -> Vec<String> {
    let mut ids: Vec<String> = vec![];
    let mut add = |id: &str| {
        if !ids.iter().any(|seen| seen == id) {
            ids.push(id.to_owned());
        }
    };
    for message in rows(projection, "messages") {
        for attachment in message["attachments"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            if let Some(id) = attachment["id"].as_str() {
                add(id);
            }
        }
    }
    for id in published_page_ids(projection) {
        add(&id);
    }
    ids
}

/// The patterns of the tool-name resolver: a trailing status, the three shapes of prefix, and OpenCode's server per thread.
fn t3_tool_patterns() -> &'static [regex::Regex; 5] {
    use crate::html_render_shared::JS_SPACE;
    static PATTERNS: std::sync::OnceLock<[regex::Regex; 5]> = std::sync::OnceLock::new();
    let line = r"[^\n\r\u{2028}\u{2029}]";
    PATTERNS.get_or_init(|| {
        let re = |source: String| regex::Regex::new(&source).expect("a fixed pattern");
        [
            re(format!("(?i){JS_SPACE}+(?:complete|completed){JS_SPACE}*$")),
            re(format!("(?i)^mcp__(?<server>{line}+?)__(?<tool>{line}+)$")),
            re(format!("(?i)^(?:t3-code|t3_code|t3code)(?:[.:/]|{JS_SPACE}*·{JS_SPACE}*)(?<tool>{line}+)$")),
            re(format!("(?i)^(?:mcp[-_]{{1,2}})?t3[-_ ]?code(?:__|[-_.:/ ])(?<tool>{line}+)$")),
            re("(?i)^t3-code-".to_owned()),
        ]
    })
}

/// Whether a provider's spelling of a tool name is T3's `html_render` (`resolveT3McpToolId`): `mcp__t3-code__html_render` (Claude, Cursor),
/// `t3-code.html_render` (Codex), the loose separators ACP agents use, and OpenCode's `t3-code-<thread>_html_render`.
fn is_html_render_tool(name: &str) -> bool {
    use crate::html_render_shared::is_js_space;
    const HTML_RENDER: &str = "html_render";
    let [complete, mcp, namespace, prefixed, opencode] = t3_tool_patterns();
    // A trailing "complete" is a label's status, not part of the name.
    let stripped = complete.replace(name, "");
    let label = stripped.trim_matches(is_js_space);
    if let Some(found) = mcp.captures(label) {
        let server = found["server"].to_lowercase();
        return matches!(server.as_str(), "t3-code" | "t3_code" | "t3code") && &found["tool"] == HTML_RENDER;
    }
    if let Some(found) = namespace.captures(label) {
        return &found["tool"] == HTML_RENDER;
    }
    let candidate = prefixed.captures(label).map_or(label, |found| found.name("tool").map_or(label, |tool| tool.as_str()));
    if candidate == HTML_RENDER {
        return true;
    }
    // OpenCode registers one server per thread, `t3-code-<thread>`, and joins it to the tool with `_`.
    opencode.is_match(label) && label.ends_with("_html_render")
}

/// The pages the thread's tool calls published, and only those minted for this thread, so deleting a fork never removes its source's pages
/// (`threadHtmlRenderAttachmentIds`). A page is the one a completed `html_render` call reported, or the document of an MCP App the item names.
fn published_page_ids(projection: &Value) -> Vec<String> {
    let Some(segment) = projection["thread"]["id"].as_str().and_then(crate::attachment_store::safe_thread_segment) else { return vec![] };
    let mut ids = vec![];
    for item in rows(projection, "turnItems").iter().filter(|item| item["type"] == "dynamic_tool") {
        let Some(output) = crate::wire_projection::compact(&item["output"]) else { continue };
        let name = item["toolName"].as_str();
        let page = if name.is_some_and(is_html_render_tool) && output["isError"] != true { output["htmlRender"]["attachmentId"].as_str() } else { None };
        let app = &output["t3McpApp"];
        let document = match (name, app["server"].as_str(), app["tool"].as_str()) {
            (Some(name), Some(server), Some(tool)) if name == format!("{server}.{tool}") => app["attachmentId"].as_str(),
            _ => None,
        };
        for id in [page, document].into_iter().flatten() {
            if crate::attachment_store::parse_thread_segment(id).as_deref() == Some(segment.as_str()) {
                ids.push(id.to_owned());
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

    /// The spellings of `html_render` the original resolves, from its own `resolveT3McpToolId`.
    #[test]
    fn provider_spellings_of_the_html_render_tool_resolve_as_the_originals_do() {
        let mut checked = 0;
        for line in include_str!("../tests/fixtures/html-render-images.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            if row["kind"] != "tool" {
                continue;
            }
            let name = row["name"].as_str().unwrap();
            assert_eq!(is_html_render_tool(name), row["htmlRender"] == true, "{name:?}");
            checked += 1;
        }
        assert!(checked >= 60, "{checked} names");
    }

    fn page_item(tool: &str, output: Value) -> Value {
        json!({"id":"item","type":"dynamic_tool","toolName":tool,"output":output})
    }

    /// A thread's published pages go with it, but only the ones minted for it, and only for a completed call.
    #[test]
    fn the_pages_a_thread_published_are_collected_with_its_attachments_and_a_forks_sources_are_not() {
        let own = "thread-1-00000000-0000-4000-8000-000000000001-html";
        let source = "other-00000000-0000-4000-8000-000000000002-html";
        let app = "thread-1-00000000-0000-4000-8000-000000000003-html";
        let reference = |id: &str| json!({"structuredContent":{"htmlRender":{"attachmentId":id,"title":"Page","height":300},"message":"shown"},"isError":false});
        let projection = json!({
            "thread": {"id": "thread:1"},
            "messages": [{"attachments": [{"id": "a"}]}],
            "turnItems": [
                page_item("mcp__t3-code__html_render", reference(own)),
                // The same page again, a page minted for another thread (a fork's source), a failed call and another tool.
                page_item("t3-code.html_render", reference(own)),
                page_item("html_render", reference(source)),
                page_item("html_render", json!({"structuredContent":{"htmlRender":{"attachmentId":"thread-1-00000000-0000-4000-8000-000000000009-html","title":"x","height":300}},"isError":true})),
                page_item("mcp__t3-code__html_preview", reference("thread-1-00000000-0000-4000-8000-000000000008-html")),
                {"id": "text", "type": "assistant_text"},
                // An MCP App's stored document, only when the item names the server and tool the reference does.
                page_item("widget.render", json!({"t3McpApp":{"attachmentId":app,"server":"widget","tool":"render","resourceUri":"ui://widget/app.html"},"result":{}})),
                page_item("other.render", json!({"t3McpApp":{"attachmentId":"thread-1-00000000-0000-4000-8000-000000000004-html","server":"widget","tool":"render","resourceUri":"ui://widget/app.html"},"result":{}})),
            ]
        });
        assert_eq!(attachment_ids(&projection), ["a", own, app]);
    }
}
