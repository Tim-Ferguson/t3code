use serde_json::{Value, json};
use t3_client::themes::{Catalog, Definition, library::*};
#[test]
fn original_theme_library_recovery_import_export_and_lossless_mutations() {
    let catalog = Catalog::default();
    for (index, line) in include_str!("fixtures/theme-library.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(line).unwrap();
        let input = &row["input"];
        match row["kind"].as_str().unwrap() {
            "stored" => assert_eq!(
                stored_theme(&catalog, input)
                    .as_ref()
                    .map(|t| definition_value(&catalog, t))
                    .unwrap_or(Value::Null),
                row["expected"],
                "row {index}"
            ),
            "many" => assert_eq!(
                json!(
                    stored_themes(&catalog, input.as_array().unwrap())
                        .iter()
                        .map(|t| definition_value(&catalog, t))
                        .collect::<Vec<_>>()
                ),
                row["expected"],
                "row {index}"
            ),
            "import" => match import(&catalog, input) {
                Ok(theme) => {
                    assert!(row["error"].is_null(), "row {index}");
                    assert_eq!(
                        definition_value(&catalog, &theme),
                        row["expected"],
                        "row {index}"
                    );
                    assert_eq!(
                        export(&catalog, &theme).unwrap(),
                        row["serialized"].as_str().unwrap(),
                        "row {index}"
                    )
                }
                Err(error) => {
                    if row["error"]
                        .as_str()
                        .is_some_and(|e| e.starts_with("Cannot read properties"))
                    {
                        assert!(error.contains("literal CSS color"), "row {index}")
                    } else {
                        assert_eq!(json!(error), row["error"], "row {index}")
                    }
                }
            },
            "read" => {
                let library = Library::read(&catalog, Ok(row["raw"].as_str()));
                let expected = match library {
                    Library::Ready { stored, themes } => {
                        json!({"status":"ready","storedThemes":stored,"themes":themes.iter().map(|t|definition_value(&catalog,t)).collect::<Vec<_>>()})
                    }
                    Library::Unavailable { reason } => {
                        json!({"status":"unavailable","reason":reason})
                    }
                };
                assert_eq!(expected, row["expected"], "row {index}");
            }
            "mutation" => {
                let raw = serde_json::to_string(&row["initial"]).unwrap();
                let library = Library::read(&catalog, Ok(Some(&raw)));
                let action = row["operation"].as_str().unwrap();
                let candidate = match action {
                    "install" => library
                        .install(
                            &catalog,
                            &serde_json::from_value::<Definition>(input.clone()).unwrap(),
                        )
                        .map(Some),
                    "update" => library
                        .update(
                            &catalog,
                            &serde_json::from_value::<Definition>(input.clone()).unwrap(),
                        )
                        .map(Some),
                    "remove" => library.remove(
                        &catalog,
                        &serde_json::from_value::<Vec<String>>(input.clone()).unwrap(),
                    ),
                    "replace" => library
                        .replace_collection(
                            &catalog,
                            "group",
                            input.as_array().unwrap(),
                            row.get("expected")
                                .and_then(Value::as_array)
                                .map(Vec::as_slice),
                        )
                        .map(Some),
                    _ => panic!("unknown action"),
                };
                let mut writes = 0;
                let mut next = row["initial"].clone();
                let error = match candidate {
                    Err(error) => Some(error),
                    Ok(None) => None,
                    Ok(Some(candidate)) => {
                        writes = 1;
                        if row["failWrite"] == true {
                            Some(format!(
                                "Failed to write the theme library to {}.",
                                t3_client::themes::CUSTOM_KEY
                            ))
                        } else {
                            next = serde_json::from_str(&candidate.bytes().unwrap()).unwrap();
                            None
                        }
                    }
                };
                assert_eq!(json!(error), row["error"], "row {index}");
                assert_eq!(json!(writes), row["writes"], "row {index}");
                assert_eq!(next, row["stored"], "row {index}");
                // Planning and a failed receipt do not mutate recovered source records.
                assert_eq!(Library::read(&catalog, Ok(Some(&raw))), library);
            }
            _ => panic!("unknown fixture"),
        }
    }
}
#[test]
fn future_records_remain_exact_and_failed_reads_cannot_become_empty_writes() {
    let catalog = Catalog::default();
    for raw in ["[", "{}", "null"] {
        let library = Library::read(&catalog, Ok(Some(raw)));
        let theme = import(
            &catalog,
            &json!({"version":1,"name":"New","appearance":"dark","colors":{"canvas":"black"}}),
        )
        .unwrap();
        assert!(library.install(&catalog, &theme).is_err());
        assert!(library.bytes().is_err());
        assert!(library.remove(&catalog, &[]).unwrap().is_none());
    }
    let opaque = json!({"id":"future","opaque":{"fields":[1,null,"keep"]},"colors":{"futureRole":"var(--future)"}});
    let raw = serde_json::to_string(&json!([opaque, null])).unwrap();
    let library = Library::read(&catalog, Ok(Some(&raw)));
    assert!(
        library
            .remove(&catalog, &["future".into()])
            .unwrap()
            .is_none()
    );
    let theme = import(
        &catalog,
        &json!({"version":1,"name":"New","appearance":"dark","colors":{"canvas":"black"}}),
    )
    .unwrap();
    let added = library.install(&catalog, &theme).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&added.bytes().unwrap()).unwrap()[0],
        opaque
    );
}

#[test]
fn crashing_original_unicode_literal_recovers_without_touching_the_source_record() {
    // Culori4.0.2 throws for this modern coordinate separator; the Rust decoder
    // rejects the literal while retaining every stored field until an explicit edit.
    let raw = "[ {\"id\":\"unicode\",\"label\":\"Kept\",\"appearance\":\"dark\",\"colors\":{\"canvas\":\"rgb(1\u{00a0}2\u{00a0}3)\"},\"future\":{\"preserve\":true}} ]";
    let catalog = Catalog::default();
    let library = Library::read(&catalog, Ok(Some(raw)));
    let (stored, themes) = library.ready().unwrap();
    assert_eq!(stored[0], serde_json::from_str::<Value>(raw).unwrap()[0]);
    assert_eq!(themes.len(), 1);
    assert_eq!(
        themes[0].colors,
        defaults(&catalog, t3_client::themes::Appearance::Dark)
    );
}
