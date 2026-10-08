//! Collection demand and recovery policy from NativeTelemetryClient.
use t3_contracts::{
    BackgroundBooleanState, HostPowerSnapshot, HostPowerSource, HostPowerThermalState,
    ResourceTelemetrySourceStatus,
};
pub fn sample_interval_ms(power: &HostPowerSnapshot, live_subscribers: usize) -> u64 {
    if power.stale || power.source == HostPowerSource::Unknown {
        return if live_subscribers > 0 { 1000 } else { 5000 };
    }
    if power.suspended
        || power.locked == BackgroundBooleanState::True
        || power.low_power_mode == BackgroundBooleanState::True
        || matches!(
            power.thermal_state,
            HostPowerThermalState::Serious | HostPowerThermalState::Critical
        )
    {
        return 15000;
    }
    if power.on_battery == BackgroundBooleanState::True {
        return 5000;
    }
    if live_subscribers > 0 { 1000 } else { 5000 }
}
pub fn can_retry(status: ResourceTelemetrySourceStatus, has_handle: bool) -> bool {
    !has_handle
        && !matches!(
            status,
            ResourceTelemetrySourceStatus::Healthy | ResourceTelemetrySourceStatus::Starting
        )
}
pub fn can_command(status: ResourceTelemetrySourceStatus, has_handle: bool) -> bool {
    has_handle
        && matches!(
            status,
            ResourceTelemetrySourceStatus::Healthy | ResourceTelemetrySourceStatus::Degraded
        )
}
pub fn recent_failures(failures: &[i64], now: i64) -> Vec<i64> {
    failures
        .iter()
        .copied()
        .filter(|&failed_at| now.saturating_sub(failed_at) <= 60000)
        .collect()
}
pub fn restart_delay_ms(attempt: u32) -> u64 {
    500_u64
        .saturating_mul(1_u64.checked_shl(attempt).unwrap_or(u64::MAX))
        .min(10000)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collection_and_recovery_match_original_source_witnesses() {
        for line in include_str!("../tests/fixtures/resource-policy.jsonl").lines() {
            let case: serde_json::Value = serde_json::from_str(line).unwrap();
            let actual = match case["kind"].as_str().unwrap() {
                "interval" => serde_json::json!(sample_interval_ms(
                    &serde_json::from_value(case["power"].clone()).unwrap(),
                    case["live"].as_u64().unwrap() as usize
                )),
                "retry" => serde_json::json!(can_retry(
                    serde_json::from_value(case["status"].clone()).unwrap(),
                    case["handle"].as_bool().unwrap()
                )),
                "command" => serde_json::json!(can_command(
                    serde_json::from_value(case["status"].clone()).unwrap(),
                    case["handle"].as_bool().unwrap()
                )),
                "failures" => serde_json::json!(recent_failures(
                    &serde_json::from_value::<Vec<i64>>(case["failures"].clone()).unwrap(),
                    case["now"].as_i64().unwrap()
                )),
                _ => panic!("unknown source fixture"),
            };
            assert_eq!(actual, case["expected"], "{case}");
        }
        assert_eq!(
            (0..7).map(restart_delay_ms).collect::<Vec<_>>(),
            vec![500, 1000, 2000, 4000, 8000, 10000, 10000]
        );
    }
}
