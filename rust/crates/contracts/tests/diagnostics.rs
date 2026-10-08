use serde::{Deserialize, Serialize};
use serde_json::Value;
use t3_contracts::*;
#[derive(Deserialize)]
struct Fixture {
    schema: String,
    label: String,
    input: Value,
    valid: bool,
    decoded_valid: bool,
    output: Option<Value>,
}
fn roundtrip<T: serde::de::DeserializeOwned + Serialize>(
    input: Value,
) -> (bool, Result<Value, serde_json::Error>) {
    match serde_json::from_value::<T>(input) {
        Ok(value) => (true, serde_json::to_value(value)),
        Err(error) => (false, Err(error)),
    }
}
#[test]
fn diagnostics_codecs_match_original_effect_json_oracle() {
    let mut count = 0;
    for line in include_str!("fixtures/diagnostics.jsonl").lines() {
        let fixture: Fixture = serde_json::from_str(line).unwrap();
        let (decoded_valid, actual) = match fixture.schema.as_str() {
            // BEGIN RESOURCE DISPATCH
            "ServerProcessDiagnosticsEntry" => {
                roundtrip::<ServerProcessDiagnosticsEntry>(fixture.input)
            }
            "ServerProcessDiagnosticsResult" => {
                roundtrip::<ServerProcessDiagnosticsResult>(fixture.input)
            }
            "ServerProcessResourceHistoryBucket" => {
                roundtrip::<ServerProcessResourceHistoryBucket>(fixture.input)
            }
            "ServerProcessResourceHistoryFailureTag" => {
                roundtrip::<ServerProcessResourceHistoryFailureTag>(fixture.input)
            }
            "ServerProcessResourceHistoryInput" => {
                roundtrip::<ServerProcessResourceHistoryInput>(fixture.input)
            }
            "ServerProcessResourceHistoryResult" => {
                roundtrip::<ServerProcessResourceHistoryResult>(fixture.input)
            }
            "ServerProcessResourceHistorySummary" => {
                roundtrip::<ServerProcessResourceHistorySummary>(fixture.input)
            }
            "ServerProcessSignal" => roundtrip::<ServerProcessSignal>(fixture.input),
            "ServerSignalProcessInput" => roundtrip::<ServerSignalProcessInput>(fixture.input),
            "ServerSignalProcessResult" => roundtrip::<ServerSignalProcessResult>(fixture.input),
            // END RESOURCE DISPATCH
            _ => panic!("missing codec {}", fixture.schema),
        };
        assert_eq!(
            decoded_valid, fixture.decoded_valid,
            "{} {} decode",
            fixture.schema, fixture.label
        );
        assert_eq!(
            actual.is_ok(),
            fixture.valid,
            "{} {} wire",
            fixture.schema,
            fixture.label
        );
        if let Some(expected) = fixture.output {
            assert_eq!(
                actual.unwrap(),
                expected,
                "{} {}",
                fixture.schema,
                fixture.label
            );
        }
        count += 1;
    }
    assert!(count > 100);
}
