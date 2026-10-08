use flate2::read::GzDecoder;
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader};
#[derive(Deserialize)]
struct Case {
    schema: String,
    input: Value,
    valid: bool,
    #[serde(default)]
    output: Value,
}
#[test]
fn all_pinned_acp_codecs_match_original_effect_decode_and_encode() {
    let bytes = include_bytes!("fixtures/source-codecs.jsonl.gz");
    let reader = BufReader::new(GzDecoder::new(&bytes[..]));
    let mut count = 0;
    for (index, line) in reader.lines().enumerate() {
        let c: Case = serde_json::from_str(&line.unwrap()).unwrap();
        let result = t3_acp::schema::decode(&c.schema, c.input.clone());
        assert_eq!(
            result.is_ok(),
            c.valid,
            "case {index} {} input {} result {result:?}",
            c.schema,
            c.input
        );
        if c.valid {
            assert_eq!(result.unwrap(), c.output, "case {index} {}", c.schema)
        }
        count += 1;
    }
    assert!(count >= 6254);
}
#[test]
fn only_source_never_schemas_lack_a_positive_witness() {
    let bytes = include_bytes!("fixtures/source-codecs.jsonl.gz");
    let reader = BufReader::new(GzDecoder::new(&bytes[..]));
    let mut inhabited = std::collections::HashSet::new();
    for line in reader.lines() {
        let c: Case = serde_json::from_str(&line.unwrap()).unwrap();
        if c.valid {
            inhabited.insert(c.schema);
        }
    }
    let mut missing = t3_acp::schema::schema_names()
        .filter(|s| !inhabited.contains(*s))
        .collect::<Vec<_>>();
    missing.sort();
    assert_eq!(
        missing,
        ["v1.TitledMultiSelectItems", "v2.TitledMultiSelectItems"]
    );
    for name in missing {
        assert!(t3_acp::schema::decode(name, serde_json::json!({"oneOf":[]})).is_err());
    }
}
#[test]
fn numeric_request_identity_matches_original_javascript_number_rendering() {
    for line in include_str!("fixtures/request-identities.jsonl").lines() {
        let case: Value = serde_json::from_str(line).unwrap();
        let id: t3_acp::RequestId = serde_json::from_str(case["wire"].as_str().unwrap()).unwrap();
        assert_eq!(
            id.identity(),
            case["identity"].as_str().unwrap(),
            "wire {}",
            case["wire"]
        );
    }
}
