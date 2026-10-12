//! A published HTML page is an entry of its own in the timeline: never grouped with tool calls, and still shown when its turn or
//! superseded attempt folds (`kind: "html-render"` in `session-logic.ts`, `deriveSupersededAttemptFolds`).
use serde_json::{Value, json};
use std::collections::BTreeSet;
use t3_client::timeline_rows::{Input, Kind, LatestRun, Row, derive, entries};

fn row(item: Value) -> Value {
    json!({"sourceThreadId":"thread","sourceItemId":item["id"],"visibility":"local","item":item})
}
fn item(id: &str, kind: &str, extra: Value) -> Value {
    let mut item = json!({"id":id,"type":kind,"status":"completed","runId":"run-1","nodeId":"node","updatedAt":"2026-01-01T00:00:00Z","startedAt":"2026-01-01T00:00:00Z"});
    for (key, value) in extra.as_object().unwrap() {
        item[key] = value.clone();
    }
    item
}
fn page(id: &str) -> Value {
    row(item(id, "dynamic_tool", json!({"toolName":"mcp__t3-code__html_render","output":{"htmlRender":{"attachmentId":format!("att-{id}"),"title":"Chart","height":300}}})))
}
/// The rows of a settled turn (its work folds), or of one still open (`open`).
fn rows_of(items: &[Value], open: bool) -> (Vec<t3_client::timeline_rows::Entry>, Vec<Row>) {
    let (attempts, nodes): (&[Value], &[Value]) = (&[], &[]);
    let entries = entries(items, attempts, nodes);
    let none = BTreeSet::new();
    let latest = LatestRun { id: "run-1".into(), status: "completed".into(), started_at: Some("2026-01-01T00:00:00Z".into()), completed_at: Some("2026-01-01T00:01:00Z".into()) };
    let rows = derive(&Input { entries: &entries, latest_run: Some(&latest), running_run_id: open.then_some("run-1"), expanded_runs: &none, expanded_attempts: &none, expanded_groups: &none, is_working: false, runless_work_active: false, active_turn_started_at: None });
    (entries, rows)
}

#[test]
fn a_page_is_its_own_row_between_the_tool_calls_around_it() {
    let items = vec![
        row(item("read", "dynamic_tool", json!({"toolName":"fixture.read"}))),
        page("one"),
        row(item("edit", "dynamic_tool", json!({"toolName":"fixture.edit"}))),
    ];
    let (entries, rows) = rows_of(&items, true);
    assert!(matches!(&entries[1].kind, Kind::HtmlRender { run_id: Some(run), page } if run == "run-1" && page.attachment_id == "att-one"));
    // The calls before and after it are two groups, the page a row between them.
    let kinds: Vec<_> = rows
        .iter()
        .map(|row| match row {
            Row::Entry { entry } if matches!(entries[*entry].kind, Kind::HtmlRender { .. }) => "page",
            Row::Entry { .. } => "entry",
            Row::Single { .. } => "single",
            Row::Toggle { .. } => "toggle",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["single", "page", "single"], "{rows:?}");
}

#[test]
fn a_failed_running_or_other_call_stays_a_tool_call() {
    let mut failed = page("failed");
    failed["item"]["output"]["isError"] = json!(true);
    let mut running = page("running");
    running["item"]["status"] = json!("running");
    let mut other = page("other");
    other["item"]["toolName"] = json!("fixture.render");
    let (entries, _) = rows_of(&[failed, running, other], true);
    assert!(entries.iter().all(|entry| matches!(entry.kind, Kind::Work(_))));
}

#[test]
fn a_page_stays_visible_when_its_turn_folds() {
    let user = row(item("user", "user_message", json!({"messageId":"m1","text":"draw"})));
    let answer = row(item("answer", "assistant_message", json!({"messageId":"m2","text":"Done","streaming":false})));
    let items = vec![user, row(item("read", "dynamic_tool", json!({"toolName":"fixture.read"}))), page("one"), answer];
    let (entries, rows) = rows_of(&items, false);
    let shown = |id: &str| {
        rows.iter().any(|row| match row {
            Row::Entry { entry: at } | Row::Single { entry: at, .. } => entries[*at].id == id,
            _ => false,
        })
    };
    assert!(rows.iter().any(|row| matches!(row, Row::TurnFold { .. })), "the turn folds: {rows:?}");
    assert!(!shown("read"), "the tool call goes behind the fold: {rows:?}");
    assert!(shown("one"), "the page does not: {rows:?}");
}
