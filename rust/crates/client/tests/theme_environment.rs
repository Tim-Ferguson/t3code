use serde_json::Value;
use t3_client::themes::{Catalog, environment, library};
#[test]
fn actual_source_environment_palette_seeds_overrides_variants_and_reserved_ids() {
    let catalog = Catalog::default();
    for (i, line) in include_str!("fixtures/theme-environment.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(line).unwrap();
        let input: Vec<t3_contracts::EnvironmentTheme> =
            match serde_json::from_value(row["input"].clone()) {
                Ok(input) => input,
                Err(error) => {
                    assert!(
                        error.to_string().contains("non-reserved"),
                        "row {i}: {error}"
                    );
                    assert_eq!(
                        row["expected"],
                        serde_json::json!([]),
                        "reserved source-filter witness {i}"
                    );
                    continue;
                }
            };
        let actual: Vec<Value> = environment::definitions(&catalog, &input)
            .iter()
            .map(|t| library::definition_value(&catalog, t))
            .collect();
        assert_eq!(serde_json::json!(actual), row["expected"], "row {i}");
    }
}
