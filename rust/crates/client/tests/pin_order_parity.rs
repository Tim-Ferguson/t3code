//! Pin-order keys replayed against the original (`rust/tools/generate_client_pin_order_fixtures.mjs`).
use serde_json::{Value, json};
use std::collections::BTreeMap;
use t3_client::pin_order;

fn rows(kind: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/pin-order.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["type"] == kind).collect();
    assert!(!rows.is_empty());
    rows
}

#[test]
fn keys_between_neighbours_and_spread_keys_are_the_originals() {
    for row in rows("between") {
        assert_eq!(pin_order::key_between(row["before"].as_str(), row["after"].as_str()).as_deref(), row["output"].as_str(), "{row}");
    }
    for row in rows("spread") {
        assert_eq!(json!(pin_order::spread_keys(row["count"].as_u64().unwrap() as usize)), row["output"], "{row}");
    }
}

#[test]
fn a_move_writes_what_the_original_writes() {
    for row in rows("plan") {
        let ids: Vec<String> = serde_json::from_value(row["ids"].clone()).unwrap();
        let keys: BTreeMap<String, Option<String>> = row["keys"].as_object().unwrap().iter().map(|(id, key)| (id.clone(), key.as_str().map(str::to_owned))).collect();
        let writes: Vec<Value> = pin_order::plan_reorder(&ids, &keys, row["moved"].as_str().unwrap()).into_iter().map(|(id, key)| json!({"id":id,"orderKey":key})).collect();
        assert_eq!(json!(writes), row["output"], "{row}");
    }
    let ids: Vec<String> = ["a", "b", "c"].map(String::from).into();
    assert_eq!(pin_order::moved(&ids, "b", true), Some(["b", "a", "c"].map(String::from).into()));
    assert_eq!(pin_order::moved(&ids, "a", true), None);
    assert_eq!(pin_order::moved(&ids, "c", false), None);
}
