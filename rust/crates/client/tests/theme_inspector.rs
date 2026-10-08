use serde_json::Value;
use t3_client::themes::inspector;
#[test]
fn original_token_utility_priority_families_and_offscreen_bounds() {
    let mut lines = include_str!("fixtures/theme-inspector.jsonl").lines();
    let dictionary: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let roles: Vec<String> = serde_json::from_value(dictionary["roles"].clone()).unwrap();
    for (index, line) in lines.enumerate() {
        let row: Value = serde_json::from_str(line).unwrap();
        let actual = match row["kind"].as_str().unwrap() {
            "utility" => serde_json::to_value(inspector::utility(
                row["className"].as_str().unwrap(),
                serde_json::from_value(row["paint"].clone()).unwrap(),
            ))
            .unwrap(),
            "paint" => serde_json::to_value(inspector::changed(
                &serde_json::from_value(row["before"].clone()).unwrap(),
                &serde_json::from_value(row["after"].clone()).unwrap(),
            ))
            .unwrap(),
            "family" => {
                serde_json::to_value(inspector::family(row["role"].as_str().unwrap())).unwrap()
            }
            "filter" => {
                serde_json::to_value(inspector::filtered_groups(row["query"].as_str().unwrap()))
                    .unwrap()
            }
            "highlight" => serde_json::to_value(inspector::highlight_roles(
                row["selected"].as_str(),
                row["advanced"].as_bool().unwrap(),
                &serde_json::from_value(
                    dictionary["palettes"][row["palette"].as_u64().unwrap() as usize].clone(),
                )
                .unwrap(),
                &roles,
            ))
            .unwrap(),
            "rectangle" => {
                let actual = inspector::rectangle(
                    serde_json::from_value(row["bounds"].clone()).unwrap(),
                    serde_json::from_value(row["viewport"].clone()).unwrap(),
                    row["radius"].as_f64().unwrap(),
                );
                let expected: Option<inspector::Rectangle> =
                    serde_json::from_value(row["expected"].clone()).unwrap();
                assert_eq!(actual, expected, "rectangle row {index}");
                continue;
            }
            _ => panic!("Unknown original witness"),
        };
        assert_eq!(actual, row["expected"], "row {index}: {}", row["kind"]);
    }
}
