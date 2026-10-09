//! What names a pull request, replayed against what the original's parser answered
//! (`rust/tools/generate_client_pull_request_reference_fixtures.mjs`).
use serde_json::Value;
use t3_client::pull_request_reference::parse;

#[test]
fn every_way_of_naming_a_pull_request_reads_as_the_original_reads_it() {
    let rows: Vec<Value> = serde_json::from_str(include_str!("fixtures/pull_request_reference.json")).unwrap();
    assert!(rows.len() >= 40);
    for row in rows {
        assert_eq!(parse(row["input"].as_str().unwrap()).as_deref(), row["reference"].as_str(), "{row}");
    }
}
