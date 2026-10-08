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
fn resource_codecs_match_original_effect_json_oracle() {
    let mut count = 0;
    for line in include_str!("fixtures/resource-telemetry.jsonl").lines() {
        let fixture: Fixture = serde_json::from_str(line).unwrap();
        let (decoded_valid, actual) = match fixture.schema.as_str() {
            // BEGIN RESOURCE DISPATCH
            "BackgroundBooleanState" => roundtrip::<BackgroundBooleanState>(fixture.input),
            "HostPowerSnapshot" => roundtrip::<HostPowerSnapshot>(fixture.input),
            "HostPowerSource" => roundtrip::<HostPowerSource>(fixture.input),
            "HostPowerThermalState" => roundtrip::<HostPowerThermalState>(fixture.input),
            "DesktopElectronProcessMetric" => {
                roundtrip::<DesktopElectronProcessMetric>(fixture.input)
            }
            "DesktopElectronProcessType" => roundtrip::<DesktopElectronProcessType>(fixture.input),
            "DesktopHostTelemetryHello" => roundtrip::<DesktopHostTelemetryHello>(fixture.input),
            "DesktopHostTelemetryMessage" => {
                roundtrip::<DesktopHostTelemetryMessage>(fixture.input)
            }
            "DesktopHostTelemetrySnapshot" => {
                roundtrip::<DesktopHostTelemetrySnapshot>(fixture.input)
            }
            "DesktopTelemetryCancelDesktopUpdate" => {
                roundtrip::<DesktopTelemetryCancelDesktopUpdate>(fixture.input)
            }
            "DesktopTelemetryCommitDesktopUpdate" => {
                roundtrip::<DesktopTelemetryCommitDesktopUpdate>(fixture.input)
            }
            "DesktopTelemetryControlMessage" => {
                roundtrip::<DesktopTelemetryControlMessage>(fixture.input)
            }
            "DesktopTelemetryRequestDesktopUpdate" => {
                roundtrip::<DesktopTelemetryRequestDesktopUpdate>(fixture.input)
            }
            "DesktopTelemetrySetDiagnosticsDemand" => {
                roundtrip::<DesktopTelemetrySetDiagnosticsDemand>(fixture.input)
            }
            "DesktopTelemetrySetHostPowerIntervals" => {
                roundtrip::<DesktopTelemetrySetHostPowerIntervals>(fixture.input)
            }
            "DesktopUpdateRemoteOutcome" => roundtrip::<DesktopUpdateRemoteOutcome>(fixture.input),
            "DesktopUpdateStatusReport" => roundtrip::<DesktopUpdateStatusReport>(fixture.input),
            "HostResourcesSnapshot" => roundtrip::<HostResourcesSnapshot>(fixture.input),
            "ResourceAttributionEntry" => roundtrip::<ResourceAttributionEntry>(fixture.input),
            "ResourceAttributionSnapshot" => {
                roundtrip::<ResourceAttributionSnapshot>(fixture.input)
            }
            "ResourceMonitorCapabilities" => {
                roundtrip::<ResourceMonitorCapabilities>(fixture.input)
            }
            "ResourceMonitorCommand" => roundtrip::<ResourceMonitorCommand>(fixture.input),
            "ResourceMonitorConfigureCommand" => {
                roundtrip::<ResourceMonitorConfigureCommand>(fixture.input)
            }
            "ResourceMonitorErrorEvent" => roundtrip::<ResourceMonitorErrorEvent>(fixture.input),
            "ResourceMonitorEvent" => roundtrip::<ResourceMonitorEvent>(fixture.input),
            "ResourceMonitorExternalProcess" => {
                roundtrip::<ResourceMonitorExternalProcess>(fixture.input)
            }
            "ResourceMonitorHelloEvent" => roundtrip::<ResourceMonitorHelloEvent>(fixture.input),
            "ResourceMonitorHistoryChunkEvent" => {
                roundtrip::<ResourceMonitorHistoryChunkEvent>(fixture.input)
            }
            "ResourceMonitorProcessSample" => {
                roundtrip::<ResourceMonitorProcessSample>(fixture.input)
            }
            "ResourceMonitorProcessTableCommand" => {
                roundtrip::<ResourceMonitorProcessTableCommand>(fixture.input)
            }
            "ResourceMonitorProcessTableEntry" => {
                roundtrip::<ResourceMonitorProcessTableEntry>(fixture.input)
            }
            "ResourceMonitorProcessTableEvent" => {
                roundtrip::<ResourceMonitorProcessTableEvent>(fixture.input)
            }
            "ResourceMonitorReadHistoryCommand" => {
                roundtrip::<ResourceMonitorReadHistoryCommand>(fixture.input)
            }
            "ResourceMonitorSampleNowCommand" => {
                roundtrip::<ResourceMonitorSampleNowCommand>(fixture.input)
            }
            "ResourceMonitorSetExternalProcessesCommand" => {
                roundtrip::<ResourceMonitorSetExternalProcessesCommand>(fixture.input)
            }
            "ResourceMonitorSetSampleIntervalCommand" => {
                roundtrip::<ResourceMonitorSetSampleIntervalCommand>(fixture.input)
            }
            "ResourceMonitorSetStreamingCommand" => {
                roundtrip::<ResourceMonitorSetStreamingCommand>(fixture.input)
            }
            "ResourceMonitorShutdownCommand" => {
                roundtrip::<ResourceMonitorShutdownCommand>(fixture.input)
            }
            "ResourceMonitorSnapshotEvent" => {
                roundtrip::<ResourceMonitorSnapshotEvent>(fixture.input)
            }
            "ResourceTelemetryAggregate" => roundtrip::<ResourceTelemetryAggregate>(fixture.input),
            "ResourceTelemetryGroups" => roundtrip::<ResourceTelemetryGroups>(fixture.input),
            "ResourceTelemetryHealth" => roundtrip::<ResourceTelemetryHealth>(fixture.input),
            "ResourceTelemetryHistory" => roundtrip::<ResourceTelemetryHistory>(fixture.input),
            "ResourceTelemetryHistoryBucket" => {
                roundtrip::<ResourceTelemetryHistoryBucket>(fixture.input)
            }
            "ResourceTelemetryHistoryInput" => {
                roundtrip::<ResourceTelemetryHistoryInput>(fixture.input)
            }
            "ResourceTelemetryIoSemantics" => {
                roundtrip::<ResourceTelemetryIoSemantics>(fixture.input)
            }
            "ResourceTelemetryProcess" => roundtrip::<ResourceTelemetryProcess>(fixture.input),
            "ResourceTelemetryProcessCategory" => {
                roundtrip::<ResourceTelemetryProcessCategory>(fixture.input)
            }
            "ResourceTelemetryProcessIdentity" => {
                roundtrip::<ResourceTelemetryProcessIdentity>(fixture.input)
            }
            "ResourceTelemetryProcessSummary" => {
                roundtrip::<ResourceTelemetryProcessSummary>(fixture.input)
            }
            "ResourceTelemetryRetryResult" => {
                roundtrip::<ResourceTelemetryRetryResult>(fixture.input)
            }
            "ResourceTelemetrySnapshot" => roundtrip::<ResourceTelemetrySnapshot>(fixture.input),
            "ResourceTelemetrySourceHealth" => {
                roundtrip::<ResourceTelemetrySourceHealth>(fixture.input)
            }
            "ResourceTelemetrySourceStatus" => {
                roundtrip::<ResourceTelemetrySourceStatus>(fixture.input)
            }
            "ConfiguredLocalServerUrls" => roundtrip::<ConfiguredLocalServerUrls>(fixture.input),
            "DiscoveredLocalServer" => roundtrip::<DiscoveredLocalServer>(fixture.input),
            "DiscoveredLocalServerList" => roundtrip::<DiscoveredLocalServerList>(fixture.input),
            "DesktopRuntimeArchSchema" => roundtrip::<DesktopRuntimeArchSchema>(fixture.input),
            "DesktopUpdateChannelSchema" => roundtrip::<DesktopUpdateChannelSchema>(fixture.input),
            "DesktopUpdateReleaseNoteSchema" => {
                roundtrip::<DesktopUpdateReleaseNoteSchema>(fixture.input)
            }
            "DesktopUpdateStateSchema" => roundtrip::<DesktopUpdateStateSchema>(fixture.input),
            "DesktopUpdateStatusSchema" => roundtrip::<DesktopUpdateStatusSchema>(fixture.input),
            // END RESOURCE DISPATCH
            other => panic!("unhandled resource schema {other}"),
        };
        assert_eq!(
            decoded_valid, fixture.decoded_valid,
            "decode {} {}",
            fixture.schema, fixture.label
        );
        assert_eq!(
            actual.is_ok(),
            fixture.valid,
            "encode {} {}: {actual:?}",
            fixture.schema,
            fixture.label
        );
        if fixture.valid {
            assert_eq!(
                actual.unwrap(),
                fixture.output.unwrap(),
                "{} {}",
                fixture.schema,
                fixture.label
            );
        }
        count += 1;
    }
    assert!(count > 1000);
}
