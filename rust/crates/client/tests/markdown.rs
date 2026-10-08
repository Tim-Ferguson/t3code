use serde_json::{Value, json};
#[test]
fn original_code_fence_and_shell_command_policy() {
    for (index, line) in include_str!("fixtures/markdown.jsonl").lines().enumerate() {
        let row: Value = serde_json::from_str(line).unwrap();
        let result = match row["kind"].as_str().unwrap() {
            "language" => json!(t3_client::markdown::fence_language(row["value"].as_str())),
            "title" => json!(t3_client::markdown::fence_title(row["value"].as_str())),
            "closed" => json!(t3_client::markdown::closed_fence(
                row["value"].as_str().unwrap()
            )),
            "run" => json!(t3_client::markdown::can_run_shell(
                row["code"].as_str().unwrap(),
                row["language"].as_str().unwrap(),
                row["streaming"] == true,
                row["available"] == true
            )),
            kind => panic!("unknown witness {kind}"),
        };
        assert_eq!(result, row["expected"], "original markdown witness {index}");
    }
}
