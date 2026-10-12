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

/// `processIdentityKey`: a process is its pid and the instant it started, so a pid the system reused is another process.
pub fn identity_key(process: &Value) -> String {
    format!("{}:{}", process["identity"]["pid"].as_u64().unwrap_or(0), process["identity"]["startTimeMs"].as_u64().unwrap_or(0))
}

/// `visibleResourceTelemetryProcesses`: the rows left when the processes in `collapsed` hide their descendants, found by parentage, so
/// that rows which are not in depth-first order are still hidden with their parent. Indexes into `processes`, in order.
pub fn visible_processes(processes: &[Value], collapsed: &std::collections::BTreeSet<String>) -> Vec<usize> {
    let mut children: std::collections::BTreeMap<u64, Vec<usize>> = std::collections::BTreeMap::new();
    for (index, process) in processes.iter().enumerate() {
        children.entry(process["ppid"].as_u64().unwrap_or(0)).or_default().push(index);
    }
    fn hide(pid: u64, processes: &[Value], children: &std::collections::BTreeMap<u64, Vec<usize>>, hidden: &mut std::collections::BTreeSet<usize>) {
        for &child in children.get(&pid).into_iter().flatten() {
            if hidden.insert(child) {
                hide(processes[child]["identity"]["pid"].as_u64().unwrap_or(0), processes, children, hidden);
            }
        }
    }
    let mut hidden = std::collections::BTreeSet::new();
    for process in processes {
        if collapsed.contains(&identity_key(process)) {
            hide(process["identity"]["pid"].as_u64().unwrap_or(0), processes, &children, &mut hidden);
        }
    }
    (0..processes.len()).filter(|index| !hidden.contains(index)).collect()
}

/// `formatProcessName`: the name, else the first word of the command without quotes, else its last path segment.
pub fn process_name(name: &str, command: &str) -> String {
    if !name.trim().is_empty() {
        return name.to_owned();
    }
    let first = command.trim().split_whitespace().next().unwrap_or(command);
    let normalized = first.trim_start_matches(['\'', '"']).trim_end_matches(['\'', '"']);
    normalized.rsplit(['/', '\\']).find(|segment| !segment.is_empty()).unwrap_or(normalized).to_owned()
}

/// `categoryLabel`.
pub fn category_label(category: &str) -> &'static str {
    match category {
        "server" => "Server",
        "server-child" => "Backend child",
        "provider-root" => "Provider",
        "terminal-root" => "Terminal",
        "electron-main" => "Electron main",
        "electron-renderer" => "Renderer",
        "electron-gpu" => "GPU",
        "electron-utility" => "Electron utility",
        "resource-monitor" => "Monitor",
        _ => "T3 process",
    }
}

/// `categoryDotClass`, as the name of the colour: the monitor warns, the desktop app informs, the server is its own, the rest are fine.
pub fn category_tone(category: &str) -> &'static str {
    if category == "resource-monitor" {
        "warning"
    } else if category.starts_with("electron-") {
        "info"
    } else if category == "server" {
        "server"
    } else {
        "success"
    }
}

/// `ioSemanticsLabel`.
pub fn io_semantics_label(semantics: &str) -> &'static str {
    match semantics {
        "storage" => "Storage bytes",
        "logical" => "Logical bytes",
        "all-io" => "All I/O bytes",
        _ => "Unavailable",
    }
}

/// `canSignalProcess`: only what this server started (a backend child, a provider, a terminal) can be signalled from the page.
pub fn can_signal(category: &str) -> bool {
    matches!(category, "server-child" | "provider-root" | "terminal-root")
}

/// `formatCpuTime`.
pub fn format_cpu_time(milliseconds: f64) -> String {
    let seconds = milliseconds / 1000.0;
    if seconds < 60.0 {
        return format!("{:.*}s", if seconds >= 10.0 { 1 } else { 2 }, seconds);
    }
    let minutes = seconds / 60.0;
    if minutes < 60.0 {
        return format!("{:.*}m", if minutes >= 10.0 { 1 } else { 2 }, minutes);
    }
    format!("{:.2}h", minutes / 60.0)
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
    fn tree_process(pid: u64, ppid: u64, depth: u64) -> Value {
        json!({"identity":{"pid":pid,"startTimeMs":pid * 1000},"ppid":ppid,"childPids":[],"depth":depth,"name":format!("process-{pid}"),"command":format!("process-{pid}")})
    }

    // The case of `ResourceTelemetryDiagnostics.logic.test.ts`: descendants are hidden by parentage even when rows are not depth-first.
    #[test]
    fn a_collapsed_process_hides_its_descendants_by_parentage_whatever_the_order_of_the_rows() {
        let processes = [tree_process(1, 0, 0), tree_process(10, 0, 0), tree_process(2, 1, 1), tree_process(11, 10, 1), tree_process(3, 2, 2)];
        let shown = |collapsed: &[&str]| -> Vec<u64> { visible_processes(&processes, &collapsed.iter().map(|key| key.to_string()).collect()).into_iter().map(|at| processes[at]["identity"]["pid"].as_u64().unwrap()).collect() };
        assert_eq!(shown(&[]), [1, 10, 2, 11, 3]);
        assert_eq!(shown(&["1:1000"]), [1, 10, 11], "the child and the grandchild go with it");
        assert_eq!(shown(&["2:2000"]), [1, 10, 2, 11]);
        assert_eq!(shown(&["1:1000", "10:10000"]), [1, 10]);
        // A pid the system reused has another start time and is another process.
        assert_eq!(shown(&["1:999"]), [1, 10, 2, 11, 3]);
        assert_eq!(identity_key(&processes[2]), "2:2000");
    }

    #[test]
    fn a_process_is_named_by_its_name_or_the_first_word_of_its_command() {
        assert_eq!(process_name("node", "/usr/bin/node server.js"), "node");
        assert_eq!(process_name("  ", "/usr/bin/node server.js"), "node");
        assert_eq!(process_name("", "'C:\\Program Files\\app.exe' --flag"), "Program", "the first word only, as the original takes it");
        assert_eq!(process_name("", "\"/opt/tool\" run"), "tool");
        assert_eq!(category_label("provider-root"), "Provider");
        assert_eq!(category_label("anything-else"), "T3 process");
        assert_eq!((category_tone("resource-monitor"), category_tone("electron-gpu"), category_tone("server"), category_tone("server-child")), ("warning", "info", "server", "success"));
        assert!(can_signal("server-child") && can_signal("provider-root") && can_signal("terminal-root") && !can_signal("server") && !can_signal("electron-main"));
        assert_eq!(io_semantics_label("all-io"), "All I/O bytes");
    }

    #[test]
    fn cpu_time_reads_in_seconds_minutes_or_hours() {
        assert_eq!(format_cpu_time(1500.0), "1.50s");
        assert_eq!(format_cpu_time(12_000.0), "12.0s");
        assert_eq!(format_cpu_time(90_000.0), "1.50m");
        assert_eq!(format_cpu_time(15.0 * 60_000.0), "15.0m");
        assert_eq!(format_cpu_time(90.0 * 60_000.0), "1.50h");
    }
}
