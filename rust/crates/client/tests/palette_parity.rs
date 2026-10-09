//! The palette's search against the original's (recorded by `tools/generate_client_palette_fixtures.mjs`):
//! text normalization, which items match a query, and the order they come in.
use serde_json::Value;
use t3_client::palette::{Extra, Group, Item, filter_groups, normalize_search_text};

fn items(value: &Value) -> Vec<Item> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            let terms: Vec<&str> = item["searchTerms"].as_array().unwrap().iter().map(|term| term.as_str().unwrap()).collect();
            let mut built = Item::action(item["value"].as_str().unwrap(), "", &terms);
            built.secondary = item["secondary"].as_bool().unwrap_or(false);
            built.recency = item["searchRecency"].as_i64();
            built
        })
        .collect()
}
fn group(value: &Value) -> Group {
    Group::new(value["value"].as_str().unwrap(), value["label"].as_str().unwrap(), items(&value["items"]))
}

#[test]
fn normalization_filtering_and_ranking_agree_with_the_original() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/palette-search.json")).unwrap();
    for row in fixture["normalized"].as_array().unwrap() {
        assert_eq!(normalize_search_text(row["text"].as_str().unwrap()), row["normalized"].as_str().unwrap(), "{row}");
    }
    let inputs = &fixture["inputs"];
    let active = vec![group(&inputs["actions"]), group(&inputs["recent"])];
    let extra = Extra { projects: items(&inputs["projects"]), settings: items(&inputs["settings"]), threads: items(&inputs["threads"]) };
    let mut checked = 0;
    for row in fixture["rows"].as_array().unwrap() {
        let actual: Vec<Value> = filter_groups(&active, row["query"].as_str().unwrap(), row["isInSubmenu"].as_bool().unwrap(), &extra)
            .into_iter()
            .map(|group| serde_json::json!([group.value, group.label, group.items.iter().map(|item| item.value.clone()).collect::<Vec<_>>()]))
            .collect();
        assert_eq!(Value::Array(actual), row["groups"], "query {:?} submenu {}", row["query"], row["isInSubmenu"]);
        checked += 1;
    }
    assert!(checked > 70);
}
