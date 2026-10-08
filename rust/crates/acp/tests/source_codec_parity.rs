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
    #[serde(default)]
    diagnostics: Option<t3_acp::schema::IssueDiagnostics>,
    #[serde(default)]
    formatted: Option<String>,
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
fn validation_failure_formatter_matches_original_effect_default_formatter() {
    let bytes = include_bytes!("fixtures/source-codecs.jsonl.gz");
    let reader = BufReader::new(GzDecoder::new(&bytes[..]));
    let mut mismatches = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let c: Case = serde_json::from_str(&line.unwrap()).unwrap();
        if let Some(expected) = c.formatted {
            let error = t3_acp::schema::decode(&c.schema, c.input.clone()).unwrap_err();
            let actual = error.issue.formatted();
            if actual != expected {
                mismatches.push(format!(
                    "{index} {} input{} expected{expected:?} actual{actual:?}",
                    c.schema, c.input
                ));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} formatter mismatches:\n{}",
        mismatches.len(),
        mismatches
            .into_iter()
            .take(20)
            .collect::<Vec<_>>()
            .join("\n")
    );
}
#[test]
fn ergonomic_protocol_error_matches_original_wire_error_schema() {
    let bytes = include_bytes!("fixtures/source-codecs.jsonl.gz");
    let reader = BufReader::new(GzDecoder::new(&bytes[..]));
    let mut count = 0;
    for line in reader.lines() {
        let c: Case = serde_json::from_str(&line.unwrap()).unwrap();
        if !matches!(c.schema.as_str(), "v1.Error" | "v2.Error") {
            continue;
        }
        let result = serde_json::from_value::<t3_acp::RpcError>(c.input.clone());
        assert_eq!(result.is_ok(), c.valid, "{} input{}", c.schema, c.input);
        if let Ok(error) = result {
            assert_eq!(serde_json::to_value(error).unwrap(), c.output);
        }
        count += 1;
    }
    assert!(count > 20);
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
#[test]
fn validation_issue_diagnostics_match_original_effect_failure_tree() {
    let bytes = include_bytes!("fixtures/source-codecs.jsonl.gz");
    let reader = BufReader::new(GzDecoder::new(&bytes[..]));
    let mut mismatches = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let case: Case = serde_json::from_str(&line.unwrap()).unwrap();
        if let Some(expected) = case.diagnostics {
            let error = t3_acp::schema::decode(&case.schema, case.input).unwrap_err();
            let actual = error.issue.diagnostics();
            if actual != expected {
                mismatches.push(format!(
                    "{index} {} {:?} expected{:?} actual{:?}",
                    case.schema, error.path, expected, actual
                ));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} diagnostic mismatches:\n{}",
        mismatches.len(),
        mismatches
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
