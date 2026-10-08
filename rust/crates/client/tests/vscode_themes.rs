use serde_json::Value;
use t3_client::themes::{Catalog, Definition, import, library, vscode};
#[test]
fn original_vscode_workbench_conversion_pairing_and_collisions() {
    let catalog = Catalog::default();
    let mut lines = include_str!("fixtures/vscode-themes.jsonl").lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let dictionary = header["dictionary"].as_array().unwrap();
    for (index, line) in lines.enumerate() {
        let row: Value = serde_json::from_str(line).unwrap();
        let input = &dictionary[row["input"].as_u64().unwrap() as usize];
        let expected = &dictionary[row["expected"].as_u64().unwrap() as usize];
        match row["kind"].as_str().unwrap() {
            "copy" => {
                let mut catalog = catalog.clone();
                catalog.custom = serde_json::from_value(input["existing"].clone()).unwrap();
                let theme = serde_json::from_value(input["theme"].clone()).unwrap();
                match import::versioned_copy(&catalog, &theme, input["preferred"].as_str()) {
                    Ok(theme) => {
                        assert!(row["error"].is_null(), "copy row {index}");
                        assert_eq!(
                            library::definition_value(&catalog, &theme),
                            *expected,
                            "copy row {index}"
                        );
                    }
                    Err(error) => assert_eq!(
                        Some(error.as_str()),
                        row["error"].as_str(),
                        "copy row {index}"
                    ),
                }
            }
            "import" => {
                assert_eq!(
                    vscode::is_file(input),
                    row["isFile"].as_bool().unwrap(),
                    "detect row {index}"
                );
                match vscode::import(&catalog, input) {
                    Err(error) => assert_eq!(
                        Some(error.as_str()),
                        row["error"].as_str(),
                        "error row {index}"
                    ),
                    Ok(theme) => {
                        assert!(row["error"].is_null(), "row {index}: {}", row["error"]);
                        assert_eq!(
                            library::definition_value(&catalog, &theme),
                            *expected,
                            "import row {index}"
                        );
                    }
                }
            }
            "humanize" => assert_eq!(
                vscode::humanize_name(input.as_str().unwrap()),
                expected.as_str().unwrap(),
                "name row {index}"
            ),
            "pair" => {
                let themes: Vec<Definition> = serde_json::from_value(input.clone()).unwrap();
                let actual = vscode::pair(&catalog, &themes, None);
                assert_eq!(
                    serde_json::to_value(
                        actual
                            .iter()
                            .map(|t| library::definition_value(&catalog, t))
                            .collect::<Vec<_>>()
                    )
                    .unwrap(),
                    *expected,
                    "pair row {index}"
                );
            }
            "collisions" => {
                let entries = input
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| vscode::Entry {
                        theme: serde_json::from_value(entry["theme"].clone()).unwrap(),
                        source_name: entry["sourceName"].as_str().map(str::to_owned),
                    })
                    .collect::<Vec<_>>();
                let actual = vscode::resolve_collisions(&catalog, &entries);
                assert_eq!(
                    serde_json::to_value(
                        actual
                            .iter()
                            .map(|t| library::definition_value(&catalog, t))
                            .collect::<Vec<_>>()
                    )
                    .unwrap(),
                    *expected,
                    "collision row {index}"
                );
            }
            kind => panic!("unexpected fixture {kind}"),
        }
    }
}
#[test]
fn json_number_spelling_does_not_change_theme_format_detection() {
    let text = r##"{"version":1.0,"name":"Number spelling","appearance":"dark","colors":{"canvas":"#123456"},"tokenColors":[]}"##;
    let input: Value = serde_json::from_str(text).unwrap();
    assert!(!vscode::is_file(&input));
    assert_eq!(
        import::parse(&Catalog::default(), text).unwrap().label,
        "Number spelling"
    );
}
