//! The timeline's words and rules replayed against what the original's helpers answered
//! (`rust/tools/generate_client_timeline_fixtures.mjs`): provider errors and retries, compaction
//! labels, the proposed-plan card's text, token and context-meter formatting and the context
//! window snapshot.
use serde_json::{Value, json};
use t3_client::timeline_items as items;

fn rows(function: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/timeline.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["fn"] == function).collect();
    assert!(!rows.is_empty(), "no rows for {function}");
    rows
}

#[test]
fn provider_errors_and_retries_read_as_the_original_words_them() {
    for row in rows("errorPresentation") {
        let (label, detail) = items::error_presentation(&row["input"]);
        assert_eq!(json!({"label":label,"detail":detail}), row["output"], "{}", row["input"]);
    }
}

#[test]
fn compaction_dividers_use_the_originals_labels() {
    for row in rows("compactionLabel") {
        assert_eq!(items::compaction_label(&row["input"]), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
}

#[test]
fn the_plan_card_text_matches_the_original() {
    for row in rows("planTitle") {
        assert_eq!(items::plan_title(row["input"].as_str().unwrap()).as_deref(), row["output"].as_str(), "{}", row["input"]);
    }
    for row in rows("planBody") {
        assert_eq!(items::plan_body(row["input"].as_str().unwrap()), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
    for row in rows("planPreview") {
        assert_eq!(items::plan_preview(row["input"].as_str().unwrap(), 10), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
    for row in rows("planPreview8") {
        assert_eq!(items::plan_preview(row["input"].as_str().unwrap(), 8), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
    for row in rows("planFilename") {
        assert_eq!(items::plan_filename(row["input"].as_str().unwrap()), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
    for row in rows("planImplementationPrompt") {
        assert_eq!(items::plan_implementation_prompt(row["input"].as_str().unwrap()), row["output"].as_str().unwrap());
    }
    for row in rows("planThreadTitle") {
        assert_eq!(items::plan_thread_title(row["input"].as_str().unwrap()), row["output"].as_str().unwrap());
    }
    for row in rows("planExport") {
        assert_eq!(items::plan_export(row["input"].as_str().unwrap()), row["output"].as_str().unwrap());
    }
    for row in rows("planFollowUp") {
        let (text, mode) = items::plan_follow_up(row["input"]["draftText"].as_str().unwrap(), row["input"]["planMarkdown"].as_str().unwrap());
        assert_eq!(json!({"text":text,"interactionMode":mode}), row["output"], "{}", row["input"]);
    }
}

#[test]
fn token_counts_and_the_meter_format_as_the_original_does() {
    for row in rows("formatTokens") {
        assert_eq!(items::format_tokens(row["input"].as_f64().unwrap()), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
    for row in rows("contextTokens") {
        assert_eq!(items::format_context_tokens(row["input"].as_f64()), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
    for row in rows("percentage") {
        assert_eq!(items::format_percentage(row["input"].as_f64()).as_deref(), row["output"].as_str(), "{}", row["input"]);
    }
    for row in rows("cost") {
        assert_eq!(items::format_context_cost(row["input"]["amount"].as_f64().unwrap(), row["input"]["currency"].as_str().unwrap()), row["output"].as_str().unwrap());
    }
    for row in rows("compactionMessage") {
        assert_eq!(items::format_compaction_message(row["input"]["name"].as_str(), row["input"]["threshold"].as_f64()), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
}

#[test]
fn the_context_window_is_derived_from_the_same_source_as_the_original() {
    for row in rows("snapshot") {
        let input = &row["input"];
        let entries: Vec<&Value> = input["entries"].as_array().unwrap().iter().map(|entry| &entry["item"]).collect();
        let window = items::derive_context_window(&entries, input["live"].as_object().map(|_| &input["live"]), input["thread"].as_object().map(|_| &input["thread"]));
        assert_eq!(float(serde_json::to_value(window).unwrap()), float(row["output"].clone()), "{input}");
    }
}

#[test]
fn a_collapsed_thought_shows_the_originals_one_line() {
    for row in rows("liveThought") {
        assert_eq!(items::live_thought_line(row["input"].as_str().unwrap()), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
}

/// JSON numbers compare by value, whether the original wrote them as integers or not.
fn float(value: Value) -> Value {
    match value {
        Value::Number(number) => json!(number.as_f64()),
        Value::Array(list) => Value::Array(list.into_iter().map(float).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(key, value)| (key, float(value))).collect()),
        other => other,
    }
}

#[test]
fn background_work_reads_as_the_originals_strip() {
    let task = |kind: &str, description: Option<&str>| json!({"kind":kind,"taskId":format!("{kind}-{}", description.unwrap_or("x")),"description":description});
    let title = |tasks: Vec<Value>| items::background_work(&tasks).map(|work| (work.title, work.waiting));
    assert_eq!(title(vec![]), None);
    assert_eq!(title(vec![task("command", Some("Start the dev server"))]), Some(("Running: Start the dev server".into(), false)));
    assert_eq!(title(vec![task("command", None)]), Some(("Running a command".into(), false)));
    assert_eq!(title(vec![task("subagent", Some("Review src/math.ts"))]), Some(("Waiting on subagent Review src/math.ts".into(), true)));
    assert_eq!(title(vec![task("monitor", None)]), Some(("Waiting on a monitor".into(), true)));
    assert_eq!(
        title(vec![task("command", Some("a")), task("subagent", Some("b")), task("subagent", Some("c"))]),
        Some(("Waiting on 2 subagents and 1 command".into(), true))
    );
    assert_eq!(title(vec![task("command", Some("a")), task("command", Some("b"))]), Some(("Running 2 commands".into(), false)));
    assert_eq!(
        title(vec![task("background_task", None), task("command", None), task("subagent", None)]),
        Some(("Waiting on 1 subagent, 1 command and 1 background task".into(), true))
    );
}

#[test]
fn the_working_timer_counts_in_the_originals_units() {
    let start = "2026-10-01T10:00:00.000Z";
    let at = |seconds: i64| chrono::DateTime::parse_from_rfc3339(start).unwrap().with_timezone(&chrono::Utc) + chrono::Duration::seconds(seconds);
    let shown: Vec<_> = [-5, 0, 59, 60, 61, 3600, 3661].into_iter().map(|seconds| items::working_elapsed(start, at(seconds)).unwrap()).collect();
    assert_eq!(shown, ["0s", "0s", "59s", "1m", "1m 1s", "1h", "1h 1m 1s"]);
    assert_eq!(items::working_elapsed("garbage", at(1)), None);
}

#[test]
fn only_the_users_queued_messages_are_listed_in_delivery_order() {
    let projection = json!({
        "runs":[
            {"id":"r1","status":"running","ordinal":1},
            {"id":"r3","status":"queued","ordinal":3,"queuePosition":2,"userMessageId":"m3"},
            {"id":"r2","status":"queued","ordinal":2,"queuePosition":1,"userMessageId":"m2"},
            {"id":"r4","status":"queued","ordinal":4,"queuePosition":3,"userMessageId":"m4"}
        ],
        "messages":[
            {"id":"m2","text":"second","attachments":[]},
            {"id":"m3","text":"third","attachments":[{"id":"a"}]},
            {"id":"m4","text":"done","delegatedCompletion":{"parentRunId":"r1"}}
        ]
    });
    let queue = items::queued_runs(&projection);
    assert_eq!(queue.iter().map(|run| (run.run_id.as_str(), run.text.as_str(), run.attachments.len())).collect::<Vec<_>>(), [("r2", "second", 0), ("r3", "third", 1)]);
}

#[test]
fn a_follow_up_queues_or_steers_as_configured_and_the_alternate_does_the_other() {
    assert_eq!(items::dispatch_mode(false, false, true), "auto");
    assert_eq!(items::dispatch_mode(false, true, false), "auto");
    assert_eq!(items::dispatch_mode(true, false, true), "queue");
    assert_eq!(items::dispatch_mode(true, true, true), "steer");
    assert_eq!(items::dispatch_mode(true, false, false), "steer");
    assert_eq!(items::dispatch_mode(true, true, false), "queue");
}
