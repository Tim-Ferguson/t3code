use serde_json::Value;
use t3_client::drafts::recover_state;

#[test]
fn original_composer_store_recovery_fixtures() {
    for (index, line) in include_str!("fixtures/draft-recovery.jsonl")
        .lines()
        .enumerate()
    {
        let fixture: Value = serde_json::from_str(line).unwrap();
        let actual = recover_state(
            &fixture["state"],
            fixture["version"].as_u64().unwrap(),
            fixture["now"].as_str().unwrap(),
        );
        if fixture.get("error").is_some() {
            assert!(actual.is_err(), "source fixture {} must reject", index + 1);
        } else {
            assert_eq!(
                actual.unwrap(),
                fixture["expected"],
                "source fixture {}",
                index + 1
            );
        }
    }
}
