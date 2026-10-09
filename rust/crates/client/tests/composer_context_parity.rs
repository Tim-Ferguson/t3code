//! Composer context records replayed against the original (`rust/tools/generate_client_context_fixtures.mjs`):
//! labels and references, kind-scoped ids, and the records a terminal selection, a review comment
//! and a thread reference become.
use serde_json::{Value, json};
use t3_client::composer_context as context;

fn rows(kind: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/composer-context.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["type"] == kind).collect();
    assert!(!rows.is_empty());
    rows
}

#[test]
fn labels_references_and_ids_read_as_the_original_writes_them() {
    for row in rows("label") {
        assert_eq!(context::sanitize_label(row["label"].as_str().unwrap(), row["kind"].as_str().unwrap()), row["output"].as_str().unwrap(), "{row}");
    }
    for row in rows("reference") {
        assert_eq!(context::reference(row["kind"].as_str().unwrap(), row["id"].as_str().unwrap(), row["label"].as_str().unwrap()), row["output"].as_str().unwrap(), "{row}");
    }
    for row in rows("id") {
        assert_eq!(context::scoped_id(row["kind"].as_str().unwrap(), row["id"].as_str().unwrap()), row["output"].as_str().unwrap(), "{row}");
    }
}

#[test]
fn the_records_chips_become_match_the_originals() {
    for row in rows("terminal") {
        let draft = &row["draft"];
        let record = context::terminal_record(draft["id"].as_str().unwrap(), draft["terminalId"].as_str().unwrap(), draft["terminalLabel"].as_str().unwrap(), draft["lineStart"].as_u64().unwrap(), draft["lineEnd"].as_u64().unwrap(), draft["text"].as_str().unwrap()).unwrap();
        assert_eq!(record, row["record"], "{row}");
        assert_eq!(context::terminal_label(draft["terminalLabel"].as_str().unwrap(), draft["lineStart"].as_u64().unwrap(), draft["lineEnd"].as_u64().unwrap()), row["label"].as_str().unwrap());
        assert_eq!(context::normalize_terminal_text(draft["text"].as_str().unwrap()), row["text"].as_str().unwrap());
    }
    for row in rows("review") {
        assert_eq!(context::review_label(&row["comment"]), row["label"].as_str().unwrap(), "{row}");
        assert_eq!(context::review_record(&row["comment"]), row["record"], "{row}");
    }
    for row in rows("thread") {
        assert_eq!(context::thread_record(row["environmentId"].as_str().unwrap(), row["threadId"].as_str().unwrap(), row["title"].as_str().unwrap()), row["record"], "{row}");
    }
}

#[test]
fn a_message_names_each_chip_once_after_its_prose_and_a_saved_prompt_keeps_its_own_references() {
    let record = context::thread_record("env", "thread-a", "Login bug");
    let (text, attached) = context::compose("please read", &[record.clone()]);
    assert_eq!(text, "please read [Login bug](t3-context://v1/thread/thread_thread-a)");
    assert_eq!(attached.unwrap()["records"], json!([record]));
    // A saved prompt that already names the chip is left alone.
    let saved = "see [Login bug](t3-context://v1/thread/thread_thread-a) first";
    assert_eq!(context::compose(saved, &[record.clone()]).0, saved);
    assert_eq!(context::compose("plain", &[]), ("plain".to_owned(), None));
    // Only a selection with text is a chip.
    assert!(context::terminal_record("t", "term", "Terminal 1", 1, 1, "\n\n").is_none());
    // A draft from the original app brings its chips; one with annotations is still held.
    let draft = json!({"terminalContexts":[{"id":"t1","terminalId":"x","terminalLabel":"Build","lineStart":2,"lineEnd":3,"text":"boom"},{"id":"t2","terminalId":"x","terminalLabel":"Build","lineStart":2,"lineEnd":3}],"threadContexts":[record],"previewAnnotations":[]});
    assert_eq!(context::records_from_draft(&draft).len(), 2, "the expired selection is left out");
    assert!(!context::holds_unsupported(&draft));
    assert!(context::holds_unsupported(&json!({"previewAnnotations":[{"id":"a"}]})));
}
