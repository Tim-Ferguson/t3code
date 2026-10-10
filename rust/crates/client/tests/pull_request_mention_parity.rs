//! The `#` pull request menu, replayed against the original (`rust/tools/generate_client_pull_request_mention_fixtures.mjs`).
use serde_json::{Value, json};
use t3_client::pull_request_mention as mention;

fn rows(kind: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/pull-request-mention.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["type"] == kind).collect();
    assert!(!rows.is_empty());
    rows
}

#[test]
fn the_menu_offers_the_pull_requests_the_original_offers_in_its_order() {
    let mut offered = 0;
    for row in rows("matches") {
        let list = |name: &str| row[name].as_array().unwrap().clone();
        let got: Vec<String> = mention::matches(&list("entries"), &list("providers"), list("exact").first(), row["projectId"].as_str().unwrap(), row["repository"].as_str(), row["query"].as_str().unwrap()).iter().map(|entry| format!("{}:{}:{}", entry["projectId"].as_str().unwrap(), entry["repository"].as_str().unwrap(), entry["number"])).collect();
        assert_eq!(json!(got), row["output"], "{row}");
        offered += got.len();
    }
    assert!(offered > 100, "the fixtures offer pull requests: {offered}");
}

#[test]
fn the_chip_a_pick_attaches_is_the_originals() {
    for row in rows("reference") {
        assert_eq!(mention::reference_comment(&row["input"]), row["output"], "{row}");
    }
}

#[test]
fn what_is_typed_after_the_hash_is_a_number_or_a_search() {
    assert_eq!((mention::text_query(""), mention::text_query("12"), mention::text_query("wiz")), (None, None, Some("wiz")));
    assert_eq!((mention::number_query(""), mention::number_query("0"), mention::number_query("12"), mention::number_query("1a")), (None, None, Some(12), None));
    assert_eq!(mention::list_input("p", Some("wiz")), json!({"state":"all","projectId":"p","limit":99,"query":"wiz"}));
    assert_eq!(mention::empty_text(false, false, ""), "Pull requests are not available for this project.");
    assert_eq!(mention::empty_text(true, false, "x"), "No pull request matches x.");
}
