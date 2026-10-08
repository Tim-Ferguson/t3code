use serde_json::Value;
use t3_terminal::fonts;
#[test]
fn source_terminal_family_size_and_monospace_probes() {
    for line in include_str!("fixtures/fonts.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let actual = match row["kind"].as_str().unwrap() {
            "family" => Value::String(fonts::quote_families(row["family"].as_str().unwrap())),
            "stack" => Value::String(fonts::unchecked_family(row["family"].as_str().unwrap())),
            "size" => serde_json::to_value(fonts::size(row["size"].as_f64().unwrap())).unwrap(),
            "advances" => Value::Bool(fonts::monospace_advances(
                &serde_json::from_value::<Vec<f64>>(row["advances"].clone()).unwrap(),
            )),
            _ => panic!("unknown fixture"),
        };
        if row["kind"] == "size" {
            assert_eq!(actual.as_f64(), row["expected"].as_f64(), "{line}")
        } else {
            assert_eq!(actual, row["expected"], "{line}")
        }
    }
}
