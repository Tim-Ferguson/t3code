use serde_json::{Value, json};
use t3_client::terminal_ui::*;
fn run(state: &PaneState, operation: &Value) -> PaneState {
    let id = operation["id"].as_str().unwrap_or_default();
    match operation["type"].as_str().unwrap() {
        "normalize" => state.normalized(),
        "open" => state.set_open(operation["value"].as_bool().unwrap()),
        "height" => state.set_height(operation["value"].as_f64().unwrap()),
        "new" => state.upsert(id, false, Direction::Horizontal),
        "split" => state.upsert(
            id,
            true,
            serde_json::from_value(operation["direction"].clone()).unwrap(),
        ),
        "active" => state.activate(id),
        "close" => state.close(id),
        "reconcile" => state
            .reconcile(&serde_json::from_value::<Vec<String>>(operation["ids"].clone()).unwrap()),
        other => panic!("unknown {other}"),
    }
}
fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| equal(a, b)))
        }
        _ => a == b,
    }
}
#[test]
fn original_groups_split_limits_activation_close_and_reconciliation() {
    for (index, line) in include_str!("fixtures/terminal-panes.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(line).unwrap();
        let mut state = serde_json::from_value(row["state"].clone()).unwrap();
        let actual = if row["kind"] == "sequence" {
            json!(
                row["operations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|operation| {
                        state = run(&state, operation);
                        state.clone()
                    })
                    .collect::<Vec<_>>()
            )
        } else {
            json!(run(&state, &row["operation"]))
        };
        assert!(
            equal(&actual, &row["expected"]),
            "witness {index}: {actual} != {}",
            row["expected"]
        );
    }
}
#[test]
fn identical_terminal_ids_in_other_destinations_do_not_share_layout_or_suppression() {
    let mut panes = ScopedPanes::default();
    let a = ScopedPanes::key("a", "thread");
    let b = ScopedPanes::key("b", "thread");
    panes.set_open(&a, true);
    panes.set_open(&b, true);
    panes.close(&a, DEFAULT_ID);
    let metadata = vec![DEFAULT_ID.to_owned(), "term-2".to_owned()];
    panes.reconcile(&a, &metadata);
    panes.reconcile(&b, &metadata);
    assert_eq!(panes.get(&a).terminal_ids, vec!["term-2"]);
    assert_eq!(panes.get(&b).terminal_ids, metadata);
    panes.upsert(&a, DEFAULT_ID, false, Direction::Horizontal);
    assert!(panes.get(&a).terminal_ids.iter().any(|id| id == DEFAULT_ID));
    panes.remove(&a);
    assert!(!panes.suppressed.contains_key(&a));
    assert_eq!(panes.get(&b).terminal_ids, metadata);
}
