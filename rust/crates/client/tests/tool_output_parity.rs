//! How many images a tool returned inline, replayed against the original
//! (`rust/tools/generate_client_tool_output_fixtures.mjs`).
use serde_json::Value;
use t3_client::work_log::output_image_count;

#[test]
fn a_tool_output_holds_the_images_the_original_finds_in_it() {
    let rows: Vec<Value> = include_str!("fixtures/tool-output.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(rows.len() > 20);
    for row in rows {
        assert_eq!(output_image_count(&row["output"]) as u64, row["images"].as_u64().unwrap(), "{row}");
    }
}
