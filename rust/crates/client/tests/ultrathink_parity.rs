//! Claude's prompt-injected effort, replayed against the original
//! (`rust/tools/generate_client_ultrathink_fixtures.mjs`).
use serde_json::Value;
use t3_client::models;

#[test]
fn the_prefix_is_applied_stripped_and_recognised_as_the_original_does() {
    let rows: Vec<Value> = include_str!("fixtures/ultrathink.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(rows.len() > 60);
    for row in rows {
        let prompt = row["prompt"].as_str().unwrap();
        assert_eq!(models::is_ultrathink_prompt(prompt), row["is"] == true, "{row}");
        assert_eq!(models::apply_effort_prefix(prompt, row["effort"].as_str()), row["applied"].as_str().unwrap(), "{row}");
        assert_eq!(models::strip_effort_prefix(prompt), row["stripped"].as_str().unwrap(), "{row}");
        assert_eq!(models::is_ultrathink_prompt(&models::strip_effort_prefix(prompt)), row["inBody"] == true, "{row}");
    }
}

#[test]
fn choosing_an_effort_rewrites_the_prompt_the_way_the_picker_does() {
    let injected = ["ultrathink"];
    // Choosing it: the prefix, or the prefix alone for an empty box.
    assert_eq!(models::prompt_after_effort("", "ultrathink", &injected).as_deref(), Some("Ultrathink:\n"));
    assert_eq!(models::prompt_after_effort("fix it", "ultrathink", &injected).as_deref(), Some("Ultrathink:\nfix it"));
    // Choosing another one removes the prefix it added; the control then owns the prompt again.
    assert_eq!(models::prompt_after_effort("Ultrathink:\nfix it", "high", &injected).as_deref(), Some("fix it"));
    assert_eq!(models::prompt_after_effort("fix it", "high", &injected), Some("fix it".into()));
    // The word typed into the text itself is the reader's: the control will not edit it away.
    assert_eq!(models::prompt_after_effort("please ultrathink", "high", &injected), None);
    assert_eq!(models::ultrathink_control(true, "please ultrathink"), (true, true));
    assert_eq!(models::ultrathink_control(true, "Ultrathink:\nfix"), (true, false));
    assert_eq!(models::ultrathink_control(false, "ultrathink"), (false, false));
}

#[test]
fn saved_values_of_a_model_that_is_gone_are_listed_with_readable_labels() {
    use t3_contracts::{ProviderOptionSelection, ProviderOptionSelectionValue, TrimmedNonEmptyString};
    let selection = |id: &str, value: ProviderOptionSelectionValue| ProviderOptionSelection { id: TrimmedNonEmptyString::new(id).unwrap(), value };
    let rows = models::unavailable_option_rows(&[
        selection("variant", ProviderOptionSelectionValue::String(TrimmedNonEmptyString::new("high").unwrap())),
        selection("contextWindow", ProviderOptionSelectionValue::String(TrimmedNonEmptyString::new("1m").unwrap())),
        selection("fastMode", ProviderOptionSelectionValue::Boolean(true)),
    ]);
    assert_eq!(rows, vec![("Reasoning".into(), "high".into()), ("Context Window".into(), "1m".into()), ("Fast Mode".into(), "On".into())]);
}
