use serde_json::Value;
use t3_client::themes::{
    Appearance, Catalog,
    editor::{self, Draft},
    library,
};
#[test]
fn original_panel_submit_candidates_unicode_names_collisions_and_removed_edits() {
    let mut lines = include_str!("fixtures/theme-editor.jsonl").lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let dictionary = header["dictionary"].as_array().unwrap();
    for (index, line) in lines.enumerate() {
        let row: Value = serde_json::from_str(line).unwrap();
        let mut catalog = Catalog::default();
        catalog.custom = library::stored_themes(
            &catalog,
            dictionary[row["initial"].as_u64().unwrap() as usize]
                .as_array()
                .unwrap(),
        );
        let value = &row["draft"];
        let draft = Draft {
            editing_id: value["editingId"].as_str().map(str::to_owned),
            name: value["name"].as_str().unwrap().into(),
            appearance: serde_json::from_value(value["appearance"].clone()).unwrap(),
            colors: serde_json::from_value(
                dictionary[value["colors"].as_u64().unwrap() as usize].clone(),
            )
            .unwrap(),
            advanced: value["advanced"].as_bool().unwrap(),
        };
        let existing = draft
            .editing_id
            .as_deref()
            .filter(|id| catalog.custom.iter().any(|t| t.id == *id));
        assert_eq!(
            editor::merge_target(&catalog, &draft.name, existing).map(|t| t.id.as_str()),
            row["target"].as_str(),
            "target row {index}"
        );
        match editor::save(&catalog, &draft) {
            Err(error) => assert_eq!(
                Some(error.as_str()),
                row["error"].as_str(),
                "error row {index}"
            ),
            Ok(plan) => {
                assert!(row["error"].is_null(), "row {index}: {}", row["error"]);
                assert_eq!(
                    library::definition_value(&catalog, &plan.theme),
                    dictionary[row["saved"].as_u64().unwrap() as usize],
                    "saved row {index}"
                );
                assert_eq!(
                    plan.created,
                    row["context"]["created"].as_bool().unwrap(),
                    "created row {index}"
                );
                assert_eq!(
                    plan.merged_appearance.map(|m| m.key()),
                    row["context"]["mergedAppearance"].as_str(),
                    "merged row {index}"
                );
            }
        }
    }
}
#[test]
fn missing_draft_palette_returns_recovery_error() {
    let draft = Draft {
        editing_id: None,
        name: "Safe".into(),
        appearance: Appearance::Dark,
        colors: Default::default(),
        advanced: false,
    };
    assert!(
        editor::save(&Catalog::default(), &draft)
            .unwrap_err()
            .contains("Reopen the editor")
    );
}
