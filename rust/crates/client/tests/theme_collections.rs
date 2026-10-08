use serde_json::Value;
use t3_client::themes::{Definition, collections};
#[test]
fn original_collection_order_defaults_and_locale_case_witnesses() {
    for (index, line) in include_str!("fixtures/theme-collections.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(line).unwrap();
        match row["kind"].as_str().unwrap() {
            "labels" => {
                let labels: Vec<String> = serde_json::from_value(row["input"].clone()).unwrap();
                // Locale conversion is a platform API. Host witnesses inject the exact
                // original fold; the same corpus also runs through actual Rust WASM.
                let folded: Vec<Vec<String>> =
                    serde_json::from_value(row["folded"].clone()).unwrap();
                let original:Vec<Vec<&str>>=labels.iter().map(|s|s.trim_matches(|c:char|matches!(c,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')).split(|c:char|matches!(c,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')).filter(|v|!v.is_empty()).collect()).collect();
                let result = collections::variant_labels(&labels, |word| {
                    if word.is_empty() {
                        return String::new();
                    };
                    original
                        .iter()
                        .enumerate()
                        .find_map(|(i, words)| {
                            words
                                .iter()
                                .position(|w| *w == word)
                                .map(|j| folded[i][j].clone())
                        })
                        .unwrap()
                });
                assert_eq!(
                    serde_json::to_value(result).unwrap(),
                    row["expected"],
                    "labels row {index}"
                );
            }
            "group" => {
                let themes: Vec<Definition> =
                    serde_json::from_value(row["themes"].clone()).unwrap();
                let actual: Vec<_> = collections::groups(&themes)
                    .into_iter()
                    .map(|(id, members)| {
                        (id, members.into_iter().map(|t| t.id).collect::<Vec<_>>())
                    })
                    .collect();
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    row["groups"],
                    "groups row {index}"
                );
                let defaults = collections::defaults(&themes);
                let actual: [Option<&str>; 2] = [
                    t3_client::themes::Appearance::Light,
                    t3_client::themes::Appearance::Dark,
                ]
                .map(|mode| {
                    defaults
                        .iter()
                        .find(|(m, _)| *m == mode)
                        .map(|(_, id)| id.as_str())
                });
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    row["defaults"],
                    "defaults row {index}"
                );
                assert_eq!(
                    collections::initial_index(&themes, |id| row["active"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v == id)),
                    row["initial"].as_u64().unwrap() as usize,
                    "initial row {index}"
                );
            }
            _ => panic!("Unknown witness"),
        }
    }
}
#[test]
fn shrinking_collection_clamps_the_selected_variant_without_resetting_valid_index() {
    assert_eq!(collections::safe_index(3, 2), Some(1));
    assert_eq!(collections::safe_index(1, 4), Some(1));
    assert_eq!(collections::safe_index(0, 0), None);
}
