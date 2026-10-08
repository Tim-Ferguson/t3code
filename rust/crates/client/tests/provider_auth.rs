use serde_json::{Value, json};
#[test]
fn original_provider_auth_policy_redaction_and_terminal_offsets() {
    let mut exact = 0;
    let mut isolated = 0;
    for (index, line) in include_str!("fixtures/provider-auth.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value =
            serde_json::from_str(line).unwrap_or_else(|error| panic!("fixture {index}: {error}"));
        let result = match row["kind"].as_str().unwrap() {
            "account" => serde_json::to_value(t3_client::provider_auth::account(
                &row["provider"],
                (!row["auth"].is_null()).then_some(&row["auth"]),
                !row["queryError"].is_null(),
                row["readOnly"] == true,
                row["pending"] == true,
                row["method"].as_str().unwrap(),
                row["environment"].as_str().unwrap(),
            ))
            .unwrap(),
            "redact" => json!(t3_client::provider_auth::redacted_placeholder(
                row["value"].as_str().unwrap()
            )),
            "paint" => {
                if row["isolatedSurrogate"] == true {
                    isolated += 1;
                    let units: Vec<u16> = row["expected"]["utf16"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|unit| unit.as_u64().unwrap() as u16)
                        .collect();
                    assert!(
                        String::from_utf16(&units).is_err(),
                        "tagged original slice must be isolated UTF16"
                    );
                    continue;
                }
                let mut written = row["written"].as_i64().unwrap();
                let result = serde_json::to_value(t3_client::provider_auth::terminal_paint(
                    &mut written,
                    row["output"].as_str().unwrap(),
                    row["offset"].as_i64(),
                ))
                .unwrap();
                assert_eq!(written, row["next"].as_i64().unwrap(), "cursor {index}");
                result
            }
            kind => panic!("unknown fixture {kind}"),
        };
        exact += 1;
        assert_eq!(result, row["expected"], "source row {index}");
    }
    assert_eq!((exact, isolated), (979, 7));
    eprintln!(
        "{exact} exact provider-auth rows; {isolated} explicit isolated-UTF16 compatibility witnesses"
    );
}
#[test]
fn terminal_responses_preserve_unicode_within_utf16_wire_bound() {
    for data in [
        "".to_owned(),
        "a".repeat(4095) + "😀" + &"b".repeat(4096),
        "😀".repeat(6000),
    ] {
        let chunks = t3_client::provider_auth::terminal_chunks(&data);
        assert_eq!(chunks.concat(), data);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.encode_utf16().count() <= 4096)
        );
    }
}
