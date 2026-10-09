//! A subagent's words replayed against the original's helpers
//! (`rust/tools/generate_client_subagent_display_fixtures.mjs`).
use serde_json::Value;
use t3_client::subagent_display as display;

fn rows(function: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/subagent-display.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["fn"] == function).collect();
    assert!(!rows.is_empty(), "no rows for {function}");
    rows
}

#[test]
fn groups_and_states_read_as_the_original_words_them() {
    for row in rows("groupSummary") {
        let list: Vec<&str> = row["input"].as_array().unwrap().iter().map(|status| status.as_str().unwrap()).collect();
        let (label, active, failed) = display::group_summary(&list);
        assert_eq!((label.as_str(), active, failed), (row["output"]["label"].as_str().unwrap(), row["output"]["active"] == true, row["output"]["failed"] == true), "{list:?}");
    }
    for row in rows("statusSummary") {
        let list: Vec<&str> = row["input"].as_array().unwrap().iter().map(|status| status.as_str().unwrap()).collect();
        assert_eq!(display::status_summary(&list), row["output"].as_str().unwrap(), "{list:?}");
    }
}

#[test]
fn titles_details_and_durations_match_the_original() {
    for row in rows("displayTitle") {
        assert_eq!(display::display_title(row["input"].as_str().unwrap()), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
    for row in rows("detailPreview") {
        let input = &row["input"];
        assert_eq!(display::detail_preview(input["status"].as_str().unwrap(), input["result"].as_str(), input["progress"].as_str()).as_deref(), row["output"].as_str(), "{input}");
    }
    for row in rows("rowDetail") {
        let input = &row["input"];
        assert_eq!(display::row_detail(input["status"].as_str().unwrap(), input["result"].as_str(), input["progress"].as_str()).as_deref(), row["output"].as_str(), "{input}");
    }
    for row in rows("elapsedMs") {
        let input = &row["input"];
        assert_eq!(display::elapsed_ms(input["status"].as_str().unwrap(), input["startedAt"].as_str(), input["completedAt"].as_str(), input["now"].as_i64().unwrap()), row["output"].as_i64(), "{input}");
    }
    for row in rows("elapsed") {
        assert_eq!(display::format_elapsed(row["input"].as_f64().unwrap()), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
    for row in rows("modelName") {
        assert_eq!(display::model_slug_name(row["input"].as_str().unwrap()), row["output"].as_str().unwrap(), "{}", row["input"]);
    }
}
