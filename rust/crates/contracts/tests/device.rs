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
fn device_codecs_match_original_effect_json_oracle() {
    let mut count = 0;
    let mut failures = Vec::new();
    for (line_number, line) in include_str!("fixtures/device.jsonl").lines().enumerate() {
        let fixture: Fixture = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("fixture line {}: {error}: {line:?}", line_number + 1));
        let (decoded_valid, actual) = match fixture.schema.as_str() {
            // BEGIN RESOURCE DISPATCH
            "DeviceActionInput" => roundtrip::<DeviceActionInput>(fixture.input.clone()),
            "DeviceActionUnavailableError" => {
                roundtrip::<DeviceActionUnavailableError>(fixture.input.clone())
            }
            "DeviceAppearance" => roundtrip::<DeviceAppearance>(fixture.input.clone()),
            "DeviceBootError" => roundtrip::<DeviceBootError>(fixture.input.clone()),
            "DeviceCloseInput" => roundtrip::<DeviceCloseInput>(fixture.input.clone()),
            "DeviceColorFilter" => roundtrip::<DeviceColorFilter>(fixture.input.clone()),
            "DeviceConfigureInput" => roundtrip::<DeviceConfigureInput>(fixture.input.clone()),
            "DeviceDetail" => roundtrip::<DeviceDetail>(fixture.input.clone()),
            "DeviceDetailInput" => roundtrip::<DeviceDetailInput>(fixture.input.clone()),
            "DeviceError" => roundtrip::<DeviceError>(fixture.input.clone()),
            "DeviceForegroundApp" => roundtrip::<DeviceForegroundApp>(fixture.input.clone()),
            "DeviceHostId" => roundtrip::<DeviceHostId>(fixture.input.clone()),
            "DeviceHostStatus" => roundtrip::<DeviceHostStatus>(fixture.input.clone()),
            "DeviceHostSummary" => roundtrip::<DeviceHostSummary>(fixture.input.clone()),
            "DeviceHostUnavailableError" => {
                roundtrip::<DeviceHostUnavailableError>(fixture.input.clone())
            }
            "DeviceId" => roundtrip::<DeviceId>(fixture.input.clone()),
            "DeviceListInput" => roundtrip::<DeviceListInput>(fixture.input.clone()),
            "DeviceNotFoundError" => roundtrip::<DeviceNotFoundError>(fixture.input.clone()),
            "DeviceOpenInput" => roundtrip::<DeviceOpenInput>(fixture.input.clone()),
            "DeviceOperationError" => roundtrip::<DeviceOperationError>(fixture.input.clone()),
            "DeviceOrientation" => roundtrip::<DeviceOrientation>(fixture.input.clone()),
            "DevicePermission" => roundtrip::<DevicePermission>(fixture.input.clone()),
            "DevicePlatform" => roundtrip::<DevicePlatform>(fixture.input.clone()),
            "DevicePlatformAvailability" => {
                roundtrip::<DevicePlatformAvailability>(fixture.input.clone())
            }
            "DevicePlatformUnavailableError" => {
                roundtrip::<DevicePlatformUnavailableError>(fixture.input.clone())
            }
            "DeviceServiceState" => roundtrip::<DeviceServiceState>(fixture.input.clone()),
            "DeviceSession" => roundtrip::<DeviceSession>(fixture.input.clone()),
            "DeviceSettings" => roundtrip::<DeviceSettings>(fixture.input.clone()),
            "DeviceShutdownInput" => roundtrip::<DeviceShutdownInput>(fixture.input.clone()),
            "DeviceSummary" => roundtrip::<DeviceSummary>(fixture.input.clone()),
            "DeviceTextSize" => roundtrip::<DeviceTextSize>(fixture.input.clone()),
            "DeviceToolCloseInput" => roundtrip::<DeviceToolCloseInput>(fixture.input.clone()),
            "DeviceToolError" => roundtrip::<DeviceToolError>(fixture.input.clone()),
            "DeviceToolListResult" => roundtrip::<DeviceToolListResult>(fixture.input.clone()),
            "DeviceToolOpenInput" => roundtrip::<DeviceToolOpenInput>(fixture.input.clone()),
            "DeviceToolOpenResult" => roundtrip::<DeviceToolOpenResult>(fixture.input.clone()),
            "DeviceToolScreenshotResult" => {
                roundtrip::<DeviceToolScreenshotResult>(fixture.input.clone())
            }
            "DeviceToolTargetInput" => roundtrip::<DeviceToolTargetInput>(fixture.input.clone()),
            "DeviceToolUnavailableError" => {
                roundtrip::<DeviceToolUnavailableError>(fixture.input.clone())
            }
            "DeviceToolVersion" => roundtrip::<DeviceToolVersion>(fixture.input.clone()),
            "DeviceToolVersions" => roundtrip::<DeviceToolVersions>(fixture.input.clone()),
            // END RESOURCE DISPATCH
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
