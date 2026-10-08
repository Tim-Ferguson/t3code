use t3_client::terminal_preview::Echo;
#[test]
fn original_settings_echo_witnesses() {
    let preview: serde_json::Value =
        serde_json::from_str(include_str!("../../ui/assets/terminal-preview.json")).unwrap();
    let prompt = preview["prompt"].as_str().unwrap();
    for line in include_str!("fixtures/terminal-preview.jsonl").lines() {
        let case: serde_json::Value = serde_json::from_str(line).unwrap();
        let mut echo = Echo::default();
        let output: Vec<String> = case["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|input| echo.write(input.as_str().unwrap(), prompt))
            .collect();
        assert_eq!(serde_json::json!(output), case["output"], "{line}");
    }
}
