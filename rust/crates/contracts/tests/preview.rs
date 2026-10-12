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

/// Every case `tools/generate_preview_contract_fixtures.mjs` took from the original's codec: what it accepts, refuses and writes back.
#[test]
fn preview_codecs_match_original_effect_json_oracle() {
    let mut count = 0;
    let mut failures = Vec::new();
    for (line_number, line) in include_str!("fixtures/preview.jsonl").lines().enumerate() {
        let fixture: Fixture = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("fixture line {}: {error}: {line:?}", line_number + 1));
        let (decoded_valid, actual) = match fixture.schema.as_str() {
            // BEGIN DISPATCH
            "PreviewAdjustInput" => roundtrip::<PreviewAdjustInput>(fixture.input.clone()),
            "PreviewAppearancePreference" => {
                roundtrip::<PreviewAppearancePreference>(fixture.input.clone())
            }
            "PreviewAutomationActionEvent" => {
                roundtrip::<PreviewAutomationActionEvent>(fixture.input.clone())
            }
            "PreviewAutomationClientDisconnectedError" => {
                roundtrip::<PreviewAutomationClientDisconnectedError>(fixture.input.clone())
            }
            "PreviewAutomationColorScheme" => {
                roundtrip::<PreviewAutomationColorScheme>(fixture.input.clone())
            }
            "PreviewAutomationConsoleEntry" => {
                roundtrip::<PreviewAutomationConsoleEntry>(fixture.input.clone())
            }
            "PreviewAutomationControlInterruptedError" => {
                roundtrip::<PreviewAutomationControlInterruptedError>(fixture.input.clone())
            }
            "PreviewAutomationControlReason" => {
                roundtrip::<PreviewAutomationControlReason>(fixture.input.clone())
            }
            "PreviewAutomationElement" => {
                roundtrip::<PreviewAutomationElement>(fixture.input.clone())
            }
            "PreviewAutomationError" => roundtrip::<PreviewAutomationError>(fixture.input.clone()),
            "PreviewAutomationExecutionError" => {
                roundtrip::<PreviewAutomationExecutionError>(fixture.input.clone())
            }
            "PreviewAutomationHost" => roundtrip::<PreviewAutomationHost>(fixture.input.clone()),
            "PreviewAutomationHostFocus" => {
                roundtrip::<PreviewAutomationHostFocus>(fixture.input.clone())
            }
            "PreviewAutomationHostIdentity" => {
                roundtrip::<PreviewAutomationHostIdentity>(fixture.input.clone())
            }
            "PreviewAutomationInvalidSelectorError" => {
                roundtrip::<PreviewAutomationInvalidSelectorError>(fixture.input.clone())
            }
            "PreviewAutomationMalformedResponseError" => {
                roundtrip::<PreviewAutomationMalformedResponseError>(fixture.input.clone())
            }
            "PreviewAutomationNetworkEntry" => {
                roundtrip::<PreviewAutomationNetworkEntry>(fixture.input.clone())
            }
            "PreviewAutomationNoAvailableHostError" => {
                roundtrip::<PreviewAutomationNoAvailableHostError>(fixture.input.clone())
            }
            "PreviewAutomationOperation" => {
                roundtrip::<PreviewAutomationOperation>(fixture.input.clone())
            }
            "PreviewAutomationRecordingArtifact" => {
                roundtrip::<PreviewAutomationRecordingArtifact>(fixture.input.clone())
            }
            "PreviewAutomationRecordingStatus" => {
                roundtrip::<PreviewAutomationRecordingStatus>(fixture.input.clone())
            }
            "PreviewAutomationRecordingTooLargeError" => {
                roundtrip::<PreviewAutomationRecordingTooLargeError>(fixture.input.clone())
            }
            "PreviewAutomationRecordingTransferError" => {
                roundtrip::<PreviewAutomationRecordingTransferError>(fixture.input.clone())
            }
            "PreviewAutomationRemoteUnavailableError" => {
                roundtrip::<PreviewAutomationRemoteUnavailableError>(fixture.input.clone())
            }
            "PreviewAutomationRequest" => {
                roundtrip::<PreviewAutomationRequest>(fixture.input.clone())
            }
            "PreviewAutomationRequestQueueClosedError" => {
                roundtrip::<PreviewAutomationRequestQueueClosedError>(fixture.input.clone())
            }
            "PreviewAutomationResizeResult" => {
                roundtrip::<PreviewAutomationResizeResult>(fixture.input.clone())
            }
            "PreviewAutomationResponse" => {
                roundtrip::<PreviewAutomationResponse>(fixture.input.clone())
            }
            "PreviewAutomationResultTooLargeError" => {
                roundtrip::<PreviewAutomationResultTooLargeError>(fixture.input.clone())
            }
            "PreviewAutomationSelectResult" => {
                roundtrip::<PreviewAutomationSelectResult>(fixture.input.clone())
            }
            "PreviewAutomationSetColorSchemeResult" => {
                roundtrip::<PreviewAutomationSetColorSchemeResult>(fixture.input.clone())
            }
            "PreviewAutomationSnapshot" => {
                roundtrip::<PreviewAutomationSnapshot>(fixture.input.clone())
            }
            "PreviewAutomationStatus" => {
                roundtrip::<PreviewAutomationStatus>(fixture.input.clone())
            }
            "PreviewAutomationStreamEvent" => {
                roundtrip::<PreviewAutomationStreamEvent>(fixture.input.clone())
            }
            "PreviewAutomationTabNotFoundError" => {
                roundtrip::<PreviewAutomationTabNotFoundError>(fixture.input.clone())
            }
            "PreviewAutomationTabTargetInput" => {
                roundtrip::<PreviewAutomationTabTargetInput>(fixture.input.clone())
            }
            "PreviewAutomationTargetNotEditableError" => {
                roundtrip::<PreviewAutomationTargetNotEditableError>(fixture.input.clone())
            }
            "PreviewAutomationTimeoutError" => {
                roundtrip::<PreviewAutomationTimeoutError>(fixture.input.clone())
            }
            "PreviewAutomationUnavailableError" => {
                roundtrip::<PreviewAutomationUnavailableError>(fixture.input.clone())
            }
            "PreviewAutomationUnsupportedClientError" => {
                roundtrip::<PreviewAutomationUnsupportedClientError>(fixture.input.clone())
            }
            "PreviewClearProfileError" => {
                roundtrip::<PreviewClearProfileError>(fixture.input.clone())
            }
            "PreviewClearProfileInput" => {
                roundtrip::<PreviewClearProfileInput>(fixture.input.clone())
            }
            "PreviewCloseInput" => roundtrip::<PreviewCloseInput>(fixture.input.clone()),
            "PreviewControlRequiredError" => {
                roundtrip::<PreviewControlRequiredError>(fixture.input.clone())
            }
            "PreviewError" => roundtrip::<PreviewError>(fixture.input.clone()),
            "PreviewEvent" => roundtrip::<PreviewEvent>(fixture.input.clone()),
            "PreviewInvalidUrlError" => roundtrip::<PreviewInvalidUrlError>(fixture.input.clone()),
            "PreviewListInput" => roundtrip::<PreviewListInput>(fixture.input.clone()),
            "PreviewListResult" => roundtrip::<PreviewListResult>(fixture.input.clone()),
            "PreviewNavStatus" => roundtrip::<PreviewNavStatus>(fixture.input.clone()),
            "PreviewNavigateInput" => roundtrip::<PreviewNavigateInput>(fixture.input.clone()),
            "PreviewOpenInput" => roundtrip::<PreviewOpenInput>(fixture.input.clone()),
            "PreviewRefreshInput" => roundtrip::<PreviewRefreshInput>(fixture.input.clone()),
            "PreviewRenderedViewportSize" => {
                roundtrip::<PreviewRenderedViewportSize>(fixture.input.clone())
            }
            "PreviewReportStatusInput" => {
                roundtrip::<PreviewReportStatusInput>(fixture.input.clone())
            }
            "PreviewResizeInput" => roundtrip::<PreviewResizeInput>(fixture.input.clone()),
            "PreviewRuntime" => roundtrip::<PreviewRuntime>(fixture.input.clone()),
            "PreviewSessionLookupError" => {
                roundtrip::<PreviewSessionLookupError>(fixture.input.clone())
            }
            "PreviewSessionSnapshot" => roundtrip::<PreviewSessionSnapshot>(fixture.input.clone()),
            "PreviewStreamHostSetup" => roundtrip::<PreviewStreamHostSetup>(fixture.input.clone()),
            "PreviewTabId" => roundtrip::<PreviewTabId>(fixture.input.clone()),
            "PreviewUrlResolution" => roundtrip::<PreviewUrlResolution>(fixture.input.clone()),
            "PreviewViewportSetting" => roundtrip::<PreviewViewportSetting>(fixture.input.clone()),
            "PreviewZoomFactor" => roundtrip::<PreviewZoomFactor>(fixture.input.clone()),
            // END DISPATCH
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

/// The text of each failure the original's classes compute, which is what an agent reads.
#[test]
fn failure_messages_match_the_original() {
    #[derive(Deserialize)]
    struct Message {
        schema: String,
        input: Value,
        message: String,
    }
    let mut count = 0;
    for line in include_str!("fixtures/preview-messages.jsonl").lines() {
        let case: Message = serde_json::from_str(line).unwrap();
        let actual = match case.schema.as_str() {
            "PreviewClearProfileError" => {
                serde_json::from_value::<PreviewClearProfileError>(case.input.clone())
                    .unwrap()
                    .message()
                    .to_owned()
            }
            name if name.starts_with("PreviewAutomation") => {
                serde_json::from_value::<PreviewAutomationError>(case.input.clone())
                    .unwrap()
                    .message()
            }
            _ => serde_json::from_value::<PreviewError>(case.input.clone())
                .unwrap()
                .message(),
        };
        assert_eq!(actual, case.message, "{} {}", case.schema, case.input);
        count += 1;
    }
    assert!(count > 30, "{count}");
}
