//! The prompt stash: the words of a menu row replayed against the original
//! (`rust/tools/generate_client_prompt_stash_fixtures.mjs`), and the rules of putting an entry aside
//! and bringing it back.
use serde_json::{Value, json};
use t3_client::{attachments::{Kind, Persisted}, prompt_stash::{self as stash, Entry, Stash}};

fn attachment(kind: Kind) -> Persisted {
    Persisted { id: "a".into(), r#type: kind, name: "n".into(), mime_type: "x/y".into(), size_bytes: 1, attachment_id: Some("up".into()) }
}
fn entry(id: &str, prompt: &str) -> Entry {
    Entry { id: id.into(), created_at: "2026-01-01T00:00:00Z".into(), environment: "env".into(), prompt: prompt.into(), attachments: vec![], records: vec![] }
}

#[test]
fn a_menu_row_reads_as_the_original_words_it() {
    let rows: Vec<Value> = include_str!("fixtures/prompt-stash.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(rows.len() > 90);
    for row in rows {
        let mut value = entry("e", row["prompt"].as_str().unwrap());
        value.attachments = (0..row["images"].as_u64().unwrap()).map(|_| attachment(Kind::Image)).chain((0..row["files"].as_u64().unwrap()).map(|_| attachment(Kind::File))).collect();
        assert_eq!(stash::snippet(&value), row["snippet"].as_str().unwrap(), "{row}");
    }
}

#[test]
fn the_stash_keeps_the_newest_twenty_and_survives_a_reload() {
    let mut held = Stash::default();
    let mut evicted = vec![];
    for n in 0..22 {
        let (next, out) = held.with(entry(&format!("e{n}"), "p"));
        held = next;
        evicted.extend(out.map(|entry| entry.id));
    }
    assert_eq!(held.entries.len(), 20);
    assert_eq!((held.entries[0].id.as_str(), evicted), ("e21", vec!["e0".to_owned(), "e1".to_owned()]));
    assert_eq!(Stash::parse(Some(&held.to_json())), held);
    // Whatever cannot be read is an empty stash, and an entry that cannot be read is left out.
    assert_eq!(Stash::parse(Some("not json")), Stash::default());
    assert_eq!(Stash::parse(None), Stash::default());
    let mixed = json!({"version":1,"entries":[{"id":"ok","createdAt":"t","environment":"e","prompt":"p"},{"nonsense":true}]}).to_string();
    assert_eq!(Stash::parse(Some(&mixed)).entries.len(), 1);
    let (rest, taken) = held.without("e5");
    assert_eq!((rest.entries.len(), taken.map(|entry| entry.id)), (19, Some("e5".to_owned())));
    assert_eq!(held.without("missing").1, None);
}

#[test]
fn a_restored_prompt_follows_what_is_written_and_an_attachment_only_entry_leaves_it_alone() {
    assert_eq!(stash::merge_prompt("", "back"), "back");
    assert_eq!(stash::merge_prompt("  \n", "back"), "back");
    assert_eq!(stash::merge_prompt("now  \n", "back"), "now\n\nback");
    assert_eq!(stash::merge_prompt("now", ""), "now");
}

#[test]
fn chips_come_back_once_under_their_own_ids_unless_the_draft_holds_another_by_that_id() {
    let record = |id: &str, text: &str| json!({"version":1,"kind":"review-comment","contextId":id,"label":"a.rs L1","text":text});
    let thread = |env: &str| json!({"version":1,"kind":"thread","contextId":"thread_t","label":"T","environmentId":env,"threadId":"t","title":"T"});
    let prompt = "see [a.rs L1](t3-context://v1/review-comment/rc1) and [T](t3-context://v1/thread/thread_t)";
    // Nothing held: all of them, as they were. A thread of another server is left behind.
    let (text, added) = stash::bring_in(prompt, &[record("rc1", "x"), thread("env")], &[], "env", || "fresh".into());
    assert_eq!((text.as_str(), added.len()), (prompt, 2));
    assert_eq!(stash::bring_in(prompt, &[thread("other")], &[], "env", || "fresh".into()).1.len(), 0);
    // The same chip already in the draft is not added twice.
    assert_eq!(stash::bring_in(prompt, &[record("rc1", "x")], &[record("rc1", "x")], "env", || "fresh".into()).1.len(), 0);
    // A different one under the same id is renamed, and the prompt's link follows it.
    let (text, added) = stash::bring_in(prompt, &[record("rc1", "x")], &[record("rc1", "other")], "env", || "fresh".into());
    let new_id = added[0]["contextId"].as_str().unwrap();
    assert_ne!(new_id, "rc1");
    assert!(text.contains(&format!("review-comment/{new_id}")) && !text.contains("review-comment/rc1)"), "{text}");
}

#[test]
fn what_stands_in_the_way_of_stashing_is_said_in_the_originals_words() {
    assert_eq!(stash::blocker(false, false, false), None);
    assert_eq!(stash::blocker(true, false, false), Some("Wait for file uploads before stashing this prompt"));
    assert_eq!(stash::blocker(true, true, false), Some("Attach dropped files again or remove them before stashing"));
}
