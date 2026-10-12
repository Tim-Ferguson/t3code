//! The "Resource History" of Settings, Diagnostics: how long the server's process and its descendants have used the processor over a
//! window, and which processes used it. The rules that read `server.getProcessResourceHistory` as the original's
//! `DiagnosticsSettings.tsx` does (the periods, the formats of its numbers, the name a process goes by).

pub const METHOD: &str = "server.getProcessResourceHistory";

/// A period to look back over, and how finely it is divided for the chart (`RESOURCE_HISTORY_WINDOWS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub label: &'static str,
    pub window_ms: u64,
    pub bucket_ms: u64,
}
pub const WINDOWS: [Window; 4] = [
    Window { label: "5m", window_ms: 5 * 60_000, bucket_ms: 30_000 },
    Window { label: "15m", window_ms: 15 * 60_000, bucket_ms: 60_000 },
    Window { label: "30m", window_ms: 30 * 60_000, bucket_ms: 2 * 60_000 },
    Window { label: "1h", window_ms: 60 * 60_000, bucket_ms: 5 * 60_000 },
];
/// The period the page opens on.
pub const DEFAULT_WINDOW_MS: u64 = 15 * 60_000;

/// The window a period names; the default one for a period the page does not offer.
pub fn window_of(window_ms: u64) -> Window {
    WINDOWS.iter().copied().find(|window| window.window_ms == window_ms).unwrap_or(WINDOWS[1])
}

/// The request for a period.
pub fn input(window: Window) -> serde_json::Value {
    serde_json::json!({"windowMs": window.window_ms, "bucketMs": window.bucket_ms})
}

/// `formatCpuTime`: seconds, minutes or hours, with two decimals under ten and one above.
pub fn cpu_time(seconds: f64) -> String {
    if seconds < 60.0 {
        return format!("{:.*}s", if seconds >= 10.0 { 1 } else { 2 }, seconds);
    }
    let minutes = seconds / 60.0;
    if minutes < 60.0 {
        return format!("{:.*}m", if minutes >= 10.0 { 1 } else { 2 }, minutes);
    }
    format!("{:.2}h", minutes / 60.0)
}

/// `formatDuration`: the sampling interval.
pub fn duration(milliseconds: f64) -> String {
    if milliseconds < 1_000.0 {
        return format!("{} ms", milliseconds.round());
    }
    format!("{:.*} s", if milliseconds >= 10_000.0 { 1 } else { 2 }, milliseconds / 1_000.0)
}

/// `formatCount`: thousands separated by commas.
pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `formatShortProcessName`: the program of a command line, its path dropped, cut at 42 characters.
pub fn short_name(command: &str) -> String {
    let first = command.split_whitespace().next();
    let name = match first {
        None => command.to_owned(),
        Some(token) => {
            let token = token.trim_matches(|c| c == '\'' || c == '"');
            token.split(['/', '\\']).filter(|part| !part.is_empty()).next_back().unwrap_or(token).to_owned()
        }
    };
    if name.chars().count() > 42 { format!("{}...", name.chars().take(39).collect::<String>()) } else { name }
}

/// The height of a chart bar as a share of the tallest peak, never below a sliver so an idle bucket still shows.
pub fn bar_height(percent: f64, tallest: f64) -> f64 {
    (percent / tallest.max(1.0) * 100.0).max(2.0)
}

/// How far a process is indented: the processes under the server's root, shallowest first.
pub fn visual_depth(is_server_root: bool, depth: u64, shallowest_child: Option<u64>) -> u64 {
    match (is_server_root, shallowest_child) {
        (true, _) | (_, None) => 0,
        (false, Some(shallowest)) => depth.saturating_sub(shallowest).saturating_add(1).max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_periods_and_their_requests_are_the_originals() {
        assert_eq!(WINDOWS.map(|window| (window.label, window.window_ms / 60_000, window.bucket_ms / 1000)), [("5m", 5, 30), ("15m", 15, 60), ("30m", 30, 120), ("1h", 60, 300)]);
        assert_eq!(window_of(DEFAULT_WINDOW_MS).label, "15m");
        assert_eq!(window_of(7).label, "15m", "a period that is not offered is the default");
        assert_eq!(input(window_of(5 * 60_000)), serde_json::json!({"windowMs": 300_000, "bucketMs": 30_000}));
    }

    #[test]
    fn numbers_read_as_the_originals_format_them() {
        assert_eq!((cpu_time(0.5).as_str(), cpu_time(12.34).as_str(), cpu_time(90.0).as_str(), cpu_time(1500.0).as_str(), cpu_time(7200.0).as_str()), ("0.50s", "12.3s", "1.50m", "25.0m", "2.00h"));
        assert_eq!((duration(250.0).as_str(), duration(1500.0).as_str(), duration(15_000.0).as_str()), ("250 ms", "1.50 s", "15.0 s"));
        assert_eq!((count(0).as_str(), count(999).as_str(), count(1000).as_str(), count(1234567).as_str()), ("0", "999", "1,000", "1,234,567"));
    }

    #[test]
    fn a_process_goes_by_the_program_of_its_command_without_its_path() {
        assert_eq!(short_name("/usr/local/bin/node --max-old-space-size=4096 server.js"), "node");
        // The first word only, as the original takes it: a path with a space in it is cut there.
        assert_eq!(short_name("\"C:\\Program Files\\Code\\code.exe\" --flag"), "Program");
        assert_eq!(short_name("   "), "   ");
        let long = format!("/bin/{}", "x".repeat(60));
        assert_eq!(short_name(&long).chars().count(), 42);
        assert!(short_name(&long).ends_with("..."));
    }

    #[test]
    fn bars_scale_to_the_tallest_peak_and_children_indent_below_the_shallowest() {
        assert_eq!(bar_height(50.0, 100.0), 50.0);
        assert_eq!(bar_height(0.0, 100.0), 2.0);
        assert_eq!(bar_height(0.5, 0.0), 50.0, "an idle window is scaled against one percent");
        assert_eq!((visual_depth(true, 0, Some(1)), visual_depth(false, 1, Some(1)), visual_depth(false, 3, Some(1)), visual_depth(false, 2, None)), (0, 1, 3, 0));
    }
}
