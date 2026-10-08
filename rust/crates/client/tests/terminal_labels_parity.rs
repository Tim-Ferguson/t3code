use serde_json::Value;
use t3_client::terminal_labels;
#[test]
fn original_labels_whitespace_numeric_ids_and_unseen_session_suffixes() {
    for (index, line) in include_str!("fixtures/terminal-labels.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(line).unwrap();
        let actual = if row["kind"] == "label" {
            terminal_labels::label(row["id"].as_str().unwrap(), row["summary"].as_str())
        } else {
            terminal_labels::next_id(
                &serde_json::from_value::<Vec<String>>(row["ids"].clone()).unwrap(),
                row["suffix"].as_str(),
            )
        };
        assert_eq!(
            actual,
            row["expected"].as_str().unwrap(),
            "source label witness {index}"
        );
    }
}
