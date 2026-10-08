use serde_json::{Value, json};
use std::io::Read;
use t3_client::themes::{Catalog, library, openvsx};
fn bytes(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks_exact(2)
        .map(|chunk| u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap())
        .collect()
}
#[test]
fn original_openvsx_metadata_paths_jsonc_archive_and_package_pipeline() {
    let mut text = String::new();
    flate2::read::GzDecoder::new(&include_bytes!("fixtures/openvsx-themes.jsonl.gz")[..])
        .read_to_string(&mut text)
        .unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let dictionary = header["dictionary"].as_array().unwrap();
    let catalog = Catalog::default();
    for (index, line) in lines.enumerate() {
        let row: Value = serde_json::from_str(line).unwrap();
        let input = &dictionary[row["input"].as_u64().unwrap() as usize];
        let mut expected = dictionary[row["expected"].as_u64().unwrap() as usize].clone();
        // Original JavaScript has one Number type; JSON's 123 vs 123.0 spelling
        // is immaterial to the native floating-point download count.
        if row["kind"] == "detail" && expected.is_object() {
            expected["downloadCount"] = json!(expected["downloadCount"].as_f64().unwrap());
        }
        let result = match row["kind"].as_str().unwrap() {
            "detail" => openvsx::detail(input).map(|value| json!(value)),
            "path" => openvsx::normalize_path(
                input["path"].as_str().unwrap(),
                input["relative"].as_str().unwrap(),
            )
            .map(|value| json!(value)),
            "jsonc" => openvsx::jsonc(input.as_str().unwrap(), "Witness"),
            "sanitize" => Ok(openvsx::sanitize(input)),
            "collectionId" => Ok(json!(openvsx::collection_id(input.as_str().unwrap()))),
            "themeId" => Ok(json!(openvsx::theme_id(
                input["id"].as_str().unwrap(),
                input["path"].as_str().unwrap()
            ))),
            "directory" => openvsx::inspect_directory(&bytes(input.as_str().unwrap()))
                .map(|offset| json!(offset + 22)),
            "package" => openvsx::import_package(
                &catalog,
                &serde_json::from_value(input["extension"].clone()).unwrap(),
                input["manifest"].as_str().unwrap(),
                &bytes(input["bytes"].as_str().unwrap()),
                input["checksum"].as_str().unwrap(),
            )
            .map(|themes| {
                json!(
                    themes
                        .iter()
                        .map(|theme| library::definition_value(&catalog, theme))
                        .collect::<Vec<_>>()
                )
            }),
            kind => panic!("Unknown source case {kind}"),
        };
        match result {
            Ok(value) => {
                assert!(
                    row["error"].is_null(),
                    "row {index}: source error {}",
                    row["error"]
                );
                assert_eq!(value, expected, "row {index} kind {}", row["kind"]);
            }
            Err(error) => assert_eq!(
                Some(error.as_str()),
                row["error"].as_str(),
                "row {index} kind {}",
                row["kind"]
            ),
        }
    }
}
