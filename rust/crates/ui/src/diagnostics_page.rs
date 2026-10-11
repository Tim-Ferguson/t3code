//! Settings, Diagnostics: the server's live child processes with a way to interrupt or kill one, and what its trace file says
//! about failures and slow work. Port of the "Live Processes" and "Trace Diagnostics" sections of `DiagnosticsSettings.tsx`
//! (`server.getProcessDiagnostics`, `server.signalProcess`, `server.getTraceDiagnostics`) for the one environment this app is
//! connected to, with the resource usage and its history (`resource_section.rs`). Not ported: the per-process history and the
//! process tree of `ResourceTelemetryDiagnostics.tsx`.
//!
//! Nothing here ticks: the page reads once when it opens and again when Refresh is pressed or a signal has been sent, so the
//! numbers are those of the moment shown beside "Checked".
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel},
    settings_common::{Confirm, Tabs, connected_allows},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_contracts::AuthEnvironmentScope;

/// The value inside an Effect `Option` on the wire (`{"_tag":"Some","value":…}`).
fn some(value: &Value) -> Option<&Value> {
    (value["_tag"] == "Some").then(|| &value["value"])
}

/// `formatBytes`: binary units, one decimal from kilobytes up.
pub(crate) fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut size, mut unit) = (count as f64, 0);
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{count} B") } else { format!("{size:.1} {}", UNITS[unit]) }
}

/// A span's duration as the page reads it.
fn duration(milliseconds: f64) -> String {
    if milliseconds >= 1000.0 { format!("{:.2} s", milliseconds / 1000.0) } else { format!("{milliseconds:.0} ms") }
}

/// A signal waiting for the person's word: the process, the instant it started (the server checks that it is still the same
/// one) and which signal.
#[derive(Clone, PartialEq)]
pub(crate) struct Signalling {
    pid: u64,
    started: u64,
    signal: &'static str,
}

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let readable = use_memo(move || connected_allows(state, AuthEnvironmentScope::DiagnosticsRead));
    let maintain = use_memo(move || connected_allows(state, AuthEnvironmentScope::EnvironmentMaintain));
    let mut processes = use_signal(|| None::<Result<Value, String>>);
    let mut traces = use_signal(|| None::<Result<Value, String>>);
    let mut reading = use_signal(|| 0u32);
    let mut asking = use_signal(|| None::<Signalling>);
    let mut signalling = use_signal(|| None::<u64>);

    let read_processes = {
        let transport = transport.clone();
        use_callback(move |()| {
            let transport = transport.clone();
            reading += 1;
            spawn(async move {
                let answer = runtime::request_value_reported(transport, state, "server.getProcessDiagnostics", json!({})).await;
                processes.set(Some(answer));
                reading -= 1;
            });
        })
    };
    let read_traces = {
        let transport = transport.clone();
        use_callback(move |()| {
            let transport = transport.clone();
            reading += 1;
            spawn(async move {
                let answer = runtime::request_value_reported(transport, state, "server.getTraceDiagnostics", json!({})).await;
                traces.set(Some(answer));
                reading -= 1;
            });
        })
    };
    // Read once the connection can be read; a reconnection reads again.
    use_effect(move || {
        if readable() {
            read_processes.call(());
            read_traces.call(());
        }
    });

    let send = {
        let transport = transport.clone();
        use_callback(move |signal: Signalling| {
            if signalling.peek().is_some() || !*maintain.peek() {
                return;
            }
            signalling.set(Some(signal.pid));
            let transport = transport.clone();
            spawn(async move {
                let answer = runtime::request_value_reported(transport, state, "server.signalProcess", json!({"pid": signal.pid, "startTimeMs": signal.started, "signal": signal.signal})).await;
                match answer {
                    Err(message) => notice::push(state, Level::Error, &format!("Could not send {}", signal.signal), &message),
                    Ok(result) if result["signaled"] != true => {
                        let message = some(&result["message"]).and_then(Value::as_str).map(str::to_owned);
                        notice::push(state, Level::Error, &format!("Could not send {}", signal.signal), &message.unwrap_or_else(|| format!("Failed to send {}.", signal.signal)));
                    }
                    Ok(_) => {}
                }
                signalling.set(None);
                read_processes.call(());
            });
        })
    };

    let busy = *reading.read() > 0;
    let process_view = processes.read().clone();
    let trace_view = traces.read().clone();
    let pending = asking.read().clone();
    rsx! {
        section { class: "settings-page diagnostics", Tabs { state, current: crate::runtime::View::Diagnostics }
            h1 { "Diagnostics" }
            p { class: "muted", "What this environment's server is running and where its traces show trouble." }
            div { class: "dialog-actions",
                button { r#type: "button", "aria-label": "Refresh diagnostics", disabled: busy || !readable(), onclick: move |_| { read_processes.call(()); read_traces.call(()); }, if busy { "Refreshing\u{2026}" } else { "Refresh" } }
            }

            if !readable() { p { class: "muted", role: "status", "This connection cannot read diagnostics." } }
            section { class: "sc-section", "aria-label": "Live Processes",
                h2 { "Live Processes" }
                match process_view {
                    None => rsx! { p { class: "muted", role: "status", "Loading live processes\u{2026}" } },
                    Some(Err(message)) => rsx! { p { class: "diagnostics-error", role: "alert", "{message}" } },
                    Some(Ok(result)) => rsx! { Processes { result, can_signal: maintain(), signalling: *signalling.read(), onsignal: move |signal: Signalling| { if signal.signal == "SIGKILL" { asking.set(Some(signal)); } else { send.call(signal); } } } },
                }
            }
            section { class: "sc-section", "aria-label": "Trace Diagnostics",
                h2 { "Trace Diagnostics" }
                match trace_view {
                    None => rsx! { p { class: "muted", role: "status", "Loading trace diagnostics\u{2026}" } },
                    Some(Err(message)) => rsx! { p { class: "diagnostics-error", role: "alert", "{message}" } },
                    Some(Ok(result)) => rsx! { Traces { result } },
                }
            }
            crate::resource_section::Section { state, transport: transport.clone() }
            if let Some(signal) = pending {
                Confirm { label: "SIGKILL", title: "Send SIGKILL?", body: format!("Send SIGKILL to process {}? This cannot be handled by the process.", signal.pid), confirm: "Send SIGKILL", busy: false,
                    oncancel: move |()| asking.set(None),
                    onconfirm: { let signal = signal.clone(); move |()| { asking.set(None); send.call(signal.clone()); } } }
            }
        }
    }
}

#[component]
pub(crate) fn Processes(result: Value, can_signal: bool, signalling: Option<u64>, onsignal: EventHandler<Signalling>) -> Element {
    let count = result["processCount"].as_u64().unwrap_or(0);
    let list = result["processes"].as_array().cloned().unwrap_or_default();
    let error = some(&result["error"]).and_then(|error| error["message"].as_str()).map(str::to_owned);
    rsx! {
        dl { class: "stats",
            div { dt { "Child processes" } dd { "{count}" } }
            div { dt { "CPU" } dd { {format!("{:.1}%", result["totalCpuPercent"].as_f64().unwrap_or(0.0))} } }
            div { dt { "Memory" } dd { {bytes(result["totalRssBytes"].as_u64().unwrap_or(0))} } }
            div { dt { "Server PID" } dd { {result["serverPid"].as_u64().unwrap_or(0).to_string()} } }
        }
        if let Some(message) = error { p { class: "diagnostics-error", role: "alert", "{message}" } }
        if list.is_empty() { p { class: "muted", "No live descendant processes found." } }
        else {
            table { class: "process-table", "aria-label": "Live processes",
                thead { tr { th { "PID" } th { "Status" } th { "CPU" } th { "Memory" } th { "Elapsed" } th { "Command" } th { "" } } }
                tbody {
                    for process in list {
                        {
                            let pid = process["pid"].as_u64().unwrap_or(0);
                            let started = process["startTimeMs"].as_u64().unwrap_or(0);
                            let indent = process["depth"].as_u64().unwrap_or(0) * 12;
                            let command = process["command"].as_str().unwrap_or_default().to_owned();
                            rsx! { tr { key: "{pid}:{started}",
                                td { style: "padding-left:{indent}px", "{pid}" }
                                td { {process["status"].as_str().unwrap_or_default().to_owned()} }
                                td { {format!("{:.1}%", process["cpuPercent"].as_f64().unwrap_or(0.0))} }
                                td { {bytes(process["rssBytes"].as_u64().unwrap_or(0))} }
                                td { {process["elapsed"].as_str().unwrap_or_default().to_owned()} }
                                td { class: "process-command", title: "{command}", "{command}" }
                                td { class: "process-actions",
                                    button { r#type: "button", "aria-label": "Send SIGINT to {pid}", disabled: !can_signal || signalling == Some(pid), onclick: move |_| onsignal.call(Signalling { pid, started, signal: "SIGINT" }), "INT" }
                                    button { class: "destructive", r#type: "button", "aria-label": "Send SIGKILL to {pid}", disabled: !can_signal || signalling == Some(pid), onclick: move |_| onsignal.call(Signalling { pid, started, signal: "SIGKILL" }), "KILL" }
                                }
                            } }
                        }
                    }
                }
            }
            if !can_signal { p { class: "muted", "Interrupting or killing a process needs the environment maintenance grant." } }
        }
    }
}

#[component]
pub(crate) fn Traces(result: Value) -> Element {
    if let Some(error) = some(&result["error"]) {
        let message = error["message"].as_str().unwrap_or("The trace file could not be read.").to_owned();
        return rsx! { p { class: "muted", role: "status", "{message}" } };
    }
    let count = |key: &str| result[key].as_u64().unwrap_or(0);
    let rows = |key: &str| result[key].as_array().cloned().unwrap_or_default();
    let (failures, slowest, top) = (rows("latestFailures"), rows("slowestSpans"), rows("topSpansByCount"));
    rsx! {
        dl { class: "stats",
            div { dt { "Records" } dd { {count("recordCount").to_string()} } }
            div { dt { "Failures" } dd { {count("failureCount").to_string()} } }
            div { dt { "Interruptions" } dd { {count("interruptionCount").to_string()} } }
            div { dt { "Slow spans" } dd { title: "Spans slower than {count(\"slowSpanThresholdMs\")} ms", {count("slowSpanCount").to_string()} } }
            div { dt { "Unreadable lines" } dd { {count("parseErrorCount").to_string()} } }
        }
        if failures.is_empty() && slowest.is_empty() && top.is_empty() { p { class: "muted", "The trace file holds no spans yet." } }
        if !failures.is_empty() {
            h3 { "Latest failures" }
            ul { class: "trace-list", "aria-label": "Latest failures",
                for failure in failures { li { key: "{failure[\"spanId\"]}", strong { {failure["name"].as_str().unwrap_or_default().to_owned()} } " " span { class: "muted", {failure["cause"].as_str().unwrap_or_default().to_owned()} } } }
            }
        }
        if !slowest.is_empty() {
            h3 { "Slowest spans" }
            ul { class: "trace-list", "aria-label": "Slowest spans",
                for span in slowest { li { key: "{span[\"spanId\"]}", strong { {span["name"].as_str().unwrap_or_default().to_owned()} } " " span { class: "muted", {duration(span["durationMs"].as_f64().unwrap_or(0.0))} } } }
            }
        }
        if !top.is_empty() {
            h3 { "Top span names" }
            ul { class: "trace-list", "aria-label": "Top span names",
                for span in top { li { key: "{span[\"name\"]}", strong { {span["name"].as_str().unwrap_or_default().to_owned()} } " " span { class: "muted", {format!("{} calls, {} failed, {} on average", span["count"].as_u64().unwrap_or(0), span["failureCount"].as_u64().unwrap_or(0), duration(span["averageDurationMs"].as_f64().unwrap_or(0.0)))} } } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_binary_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(duration(12.4), "12 ms");
        assert_eq!(duration(2500.0), "2.50 s");
    }

    #[test]
    fn an_effect_option_reads_only_when_it_is_some() {
        assert_eq!(some(&json!({"_tag": "Some", "value": 3})), Some(&json!(3)));
        assert_eq!(some(&json!({"_tag": "None"})), None);
        assert_eq!(some(&Value::Null), None);
    }
}
