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
fn cooperative_mcp_codecs_match_original_effect_json_oracle() {
    let mut count = 0;
    let mut failures = Vec::new();
    for (line_number, line) in include_str!("fixtures/mcp-control.jsonl")
        .split('\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let fixture: Fixture = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("fixture line {}: {error}: {line:?}", line_number + 1));
        let (decoded_valid, actual) = match fixture.schema.as_str() {
            "OrchestratorMcpThreadDetail" => {
                roundtrip::<OrchestratorMcpThreadDetail>(fixture.input.clone())
            }
            "OrchestratorMcpThreadInterruptInput" => {
                roundtrip::<OrchestratorMcpThreadInterruptInput>(fixture.input.clone())
            }
            "OrchestratorMcpThreadInterruptResult" => {
                roundtrip::<OrchestratorMcpThreadInterruptResult>(fixture.input.clone())
            }
            "OrchestratorMcpThreadListInput" => {
                roundtrip::<OrchestratorMcpThreadListInput>(fixture.input.clone())
            }
            "OrchestratorMcpThreadListItem" => {
                roundtrip::<OrchestratorMcpThreadListItem>(fixture.input.clone())
            }
            "OrchestratorMcpThreadListResult" => {
                roundtrip::<OrchestratorMcpThreadListResult>(fixture.input.clone())
            }
            "OrchestratorMcpThreadReadInput" => {
                roundtrip::<OrchestratorMcpThreadReadInput>(fixture.input.clone())
            }
            "OrchestratorMcpThreadReadResult" => {
                roundtrip::<OrchestratorMcpThreadReadResult>(fixture.input.clone())
            }
            "OrchestratorMcpThreadRun" => {
                roundtrip::<OrchestratorMcpThreadRun>(fixture.input.clone())
            }
            "OrchestratorMcpThreadSendInput" => {
                roundtrip::<OrchestratorMcpThreadSendInput>(fixture.input.clone())
            }
            "OrchestratorMcpThreadSendResult" => {
                roundtrip::<OrchestratorMcpThreadSendResult>(fixture.input.clone())
            }
            "OrchestratorMcpThreadTimelineItem" => {
                roundtrip::<OrchestratorMcpThreadTimelineItem>(fixture.input.clone())
            }
            "OrchestratorMcpThreadWaitInput" => {
                roundtrip::<OrchestratorMcpThreadWaitInput>(fixture.input.clone())
            }
            "OrchestratorMcpThreadWaitResult" => {
                roundtrip::<OrchestratorMcpThreadWaitResult>(fixture.input.clone())
            }
            "ThreadMetadataMcpUpdateInput" => {
                roundtrip::<ThreadMetadataMcpUpdateInput>(fixture.input.clone())
            }
            "ThreadMetadataMcpUpdateResult" => {
                roundtrip::<ThreadMetadataMcpUpdateResult>(fixture.input.clone())
            }
            _ => panic!("missing codec {}", fixture.schema),
        };
        if decoded_valid != fixture.decoded_valid || actual.is_ok() != fixture.valid {
            failures.push(format!(
                "{} {} input={} valid={decoded_valid}/{} wire={}/{}",
                fixture.schema,
                fixture.label,
                fixture.input,
                fixture.decoded_valid,
                actual.is_ok(),
                fixture.valid
            ));
        } else if let Some(expected) = fixture.output {
            if actual.as_ref().ok() != Some(&expected) {
                failures.push(format!(
                    "{} {} expected={expected} actual={actual:?}",
                    fixture.schema, fixture.label
                ));
            }
        }
        count += 1;
    }
    assert!(count > 100);
    assert!(
        failures.is_empty(),
        "{} mismatches\n{}",
        failures.len(),
        failures.join("\n")
    );
}
