use serde_json::Value;
use t3_client::themes::{Catalog, vivid};
#[test]
fn source_perceptual_palette_generation_all_roles_and_contrast_branches() {
    let catalog = Catalog::default();
    let mut failures = vec![];
    for (i, line) in include_str!("fixtures/theme-vivid.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(line).unwrap();
        let actual = serde_json::to_value(vivid::create(
            &catalog,
            serde_json::from_value(row["appearance"].clone()).unwrap(),
            row["background"].as_str().unwrap(),
            row["accent"].as_str().unwrap(),
        ))
        .unwrap();
        for role in &catalog.data.roles {
            if actual[role] != row["expected"][role] {
                failures.push(format!(
                    "{i} {} {} {role}: {} vs {}",
                    row["background"], row["accent"], actual[role], row["expected"][role]
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
#[test]
fn original_advanced_family_edit_preserves_unrelated_hand_tuned_roles() {
    for (i, line) in include_str!("fixtures/theme-families.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(line).unwrap();
        let colors = serde_json::from_value(row["colors"].clone()).unwrap();
        let actual = vivid::update(
            serde_json::from_value(row["appearance"].clone()).unwrap(),
            &colors,
            row["role"].as_str().unwrap(),
            row["color"].as_str().unwrap(),
        );
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            row["expected"],
            "row {i}"
        );
    }
}
