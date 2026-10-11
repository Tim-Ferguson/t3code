//! The resource telemetry of the Diagnostics page: the windows the history can show, the height of a bar and when the monitor
//! may be restarted. Port of `ResourceTelemetryDiagnostics.logic.ts` and the formatting of `ResourceTelemetryDiagnostics.tsx`.
use serde_json::{Value, json};

pub const SUBSCRIBE: &str = "subscribeResourceTelemetry";
pub const HISTORY: &str = "server.getResourceTelemetryHistory";
pub const RETRY: &str = "server.retryResourceTelemetry";

/// The windows the history offers, with the width of one bar each.
pub const WINDOWS: [(&str, u64, u64); 4] = [("5m", 5 * 60_000, 15_000), ("15m", 15 * 60_000, 30_000), ("30m", 30 * 60_000, 60_000), ("1h", 60 * 60_000, 2 * 60_000)];
/// The window the page opens on.
pub const DEFAULT_WINDOW: usize = 1;

pub fn history_input(window: usize) -> Value {
    let (_, window_ms, bucket_ms) = WINDOWS[window.min(WINDOWS.len() - 1)];
    json!({"windowMs": window_ms, "bucketMs": bucket_ms})
}

/// `resourceHistoryBarHeight`: a bar's height in percent of the chart, never less than `minimum` unless there is nothing.
pub fn bar_height(value: f64, max: f64, minimum_percent: f64) -> f64 {
    if value <= 0.0 { 0.0 } else { minimum_percent.max(value / max.max(1.0) * 100.0) }
}
/// `resourceHistoryCpuScaleMax`.
pub fn cpu_scale_max(buckets: &[Value]) -> f64 {
    buckets.iter().filter_map(|bucket| bucket["avgCpuPercent"].as_f64()).fold(1.0, f64::max)
}

/// `shouldShowResourceMonitorRetry`: the monitor is offered a restart when it is not sampling, or when nothing came from it and
/// the reading failed. `native_status` is the monitor's own source (`ok`, `degraded`, `unavailable`, `stopped`).
pub fn show_retry(native_status: Option<&str>, error: Option<&str>) -> bool {
    (native_status.is_none() && error.is_some()) || matches!(native_status, Some("degraded" | "unavailable" | "stopped"))
}

/// An Effect `Option` on the wire.
pub fn some(value: &Value) -> Option<&Value> {
    (value["_tag"] == "Some").then(|| &value["value"])
}

pub fn format_bytes(value: f64) -> String {
    if value < 1024.0 {
        return format!("{} B", value.round());
    }
    let units = ["KB", "MB", "GB", "TB"];
    let (mut next, mut unit) = (value, 0usize);
    loop {
        next /= 1024.0;
        if next < 1024.0 || unit == units.len() - 1 {
            break;
        }
        unit += 1;
    }
    let digits = if next >= 100.0 { 0 } else if next >= 10.0 { 1 } else { 2 };
    format!("{next:.digits$} {}", units[unit])
}
pub fn format_rate(value: f64) -> String {
    format!("{}/s", format_bytes(value))
}
pub fn format_sample_interval(milliseconds: f64) -> String {
    if milliseconds < 1000.0 {
        return format!("{} ms", milliseconds.max(0.0).round());
    }
    let seconds = milliseconds / 1000.0;
    let text = format!("{:.1}", seconds);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text} {}", if seconds == 1.0 { "second" } else { "seconds" })
}

/// How a group of the snapshot is called on the page, in order.
pub const GROUPS: [(&str, &str); 4] = [("backend", "Backend"), ("electron", "Desktop app"), ("monitor", "Monitor"), ("allT3", "All T3 Code")];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bar_is_never_invisible_unless_it_is_empty_and_the_scale_never_drops_below_one() {
        assert_eq!(bar_height(0.0, 50.0, 2.0), 0.0);
        assert_eq!(bar_height(0.1, 50.0, 2.0), 2.0);
        assert_eq!(bar_height(25.0, 50.0, 2.0), 50.0);
        assert_eq!(bar_height(5.0, 0.0, 1.0), 500.0, "an empty scale is one");
        assert_eq!(cpu_scale_max(&[]), 1.0);
        assert_eq!(cpu_scale_max(&[json!({"avgCpuPercent": 0.2}), json!({"avgCpuPercent": 12.5})]), 12.5);
    }

    #[test]
    fn the_monitor_may_be_restarted_when_it_is_not_sampling() {
        assert!(show_retry(None, Some("unavailable")));
        assert!(!show_retry(None, None));
        assert!(show_retry(Some("degraded"), None) && show_retry(Some("stopped"), None) && show_retry(Some("unavailable"), None));
        assert!(!show_retry(Some("ok"), Some("ignored when a status is known")));
    }

    #[test]
    fn sizes_rates_and_intervals_read_as_the_page_writes_them() {
        assert_eq!(format_bytes(512.0), "512 B");
        assert_eq!(format_bytes(1536.0), "1.50 KB");
        assert_eq!(format_bytes(15.0 * 1024.0 * 1024.0), "15.0 MB");
        assert_eq!(format_bytes(300.0 * 1024.0 * 1024.0), "300 MB");
        assert_eq!(format_rate(2048.0), "2.00 KB/s");
        assert_eq!(format_sample_interval(250.0), "250 ms");
        assert_eq!(format_sample_interval(1000.0), "1 second");
        assert_eq!(format_sample_interval(2500.0), "2.5 seconds");
        assert_eq!(history_input(1), json!({"windowMs": 900_000, "bucketMs": 30_000}));
    }
}
