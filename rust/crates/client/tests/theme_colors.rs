use serde_json::Value;
#[test]
fn original_f64_theme_color_conversion_and_hex_boundaries() {
    let mut failures = vec![];
    for (index, row) in include_str!("fixtures/theme-colors.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(row).unwrap();
        assert!(row["error"].is_null(), "original extraction error");
        let input = row["input"].as_str();
        let canonical = input.and_then(t3_client::themes::color::canonical);
        let hex = input.and_then(t3_client::themes::color::hex);
        if let Some(error) = row["originalError"].as_str() {
            assert_eq!(
                error,
                "TypeError: Cannot read properties of undefined (reading 'type')"
            );
            assert!(
                canonical.is_none() && hex.is_none(),
                "unsafe original color: {}",
                row["input"]
            );
            continue;
        }
        if serde_json::json!(canonical) != row["canonical"] || serde_json::json!(hex) != row["hex"]
        {
            failures.push(format!(
                "{index}: {:?}: canonical {:?} expected {}; hex {:?} expected {}",
                row["input"], canonical, row["canonical"], hex, row["hex"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
