//! "Resource History" of the Diagnostics page (`server.getProcessResourceHistory`, the second section of `DiagnosticsSettings.tsx`):
//! how much processor time the server's process and its descendants used over a period, as a chart of the period's buckets and a
//! table of the processes that used it. Read once for the period chosen and again when the period changes or Refresh is pressed;
//! nothing here ticks, and a bar is a box with a height.
use crate::{
    diagnostics_page::bytes,
    runtime::{self, TransportHandle, UiModel},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::Value;
use t3_client::{process_history as logic, resource_telemetry::some};
use t3_contracts::AuthEnvironmentScope;

#[component]
pub(crate) fn Section(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let readable = use_memo(move || connected_allows(state, AuthEnvironmentScope::DiagnosticsRead));
    let mut window_ms = use_signal(|| logic::DEFAULT_WINDOW_MS);
    let mut answer = use_signal(|| None::<Result<Value, String>>);
    // Bumped by Refresh: the read below depends on it.
    let mut refreshed = use_signal(|| 0u32);
    let mut reading = use_signal(|| false);
    {
        let transport = transport.clone();
        use_resource(move || {
            let (transport, wanted, window, _refreshed) = (transport.clone(), readable(), logic::window_of(*window_ms.read()), *refreshed.read());
            async move {
                if !wanted {
                    return;
                }
                reading.set(true);
                let owner = runtime::response_owner(&transport, state);
                let read = runtime::request_value_reported(transport.clone(), state, logic::METHOD, logic::input(window)).await;
                // Another connection (or another period) answers for itself.
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                answer.set(Some(read));
                reading.set(false);
            }
        });
    }
    if !readable() {
        return rsx! {};
    }
    let shown = answer.read().clone();
    let selected = logic::window_of(*window_ms.read()).window_ms;
    rsx! {
        section { class: "sc-section", "aria-label": "Resource History",
            h2 { "Resource History" }
            div { class: "resource-history", role: "group", "aria-label": "Process history period",
                for window in logic::WINDOWS {
                    button { key: "{window.label}", r#type: "button", "aria-label": "Period {window.label}", "aria-pressed": "{window.window_ms == selected}", class: if window.window_ms == selected { "selected" } else { "" }, onclick: move |_| window_ms.set(window.window_ms), "{window.label}" }
                }
                button { r#type: "button", "aria-label": "Refresh resource history", disabled: *reading.read(), onclick: move |_| { let next = *refreshed.peek() + 1; refreshed.set(next); }, if *reading.read() { "Refreshing\u{2026}" } else { "Refresh" } }
            }
            match shown {
                None => rsx! { p { class: "muted", role: "status", "Collecting process resource samples\u{2026}" } },
                Some(Err(message)) => rsx! { p { class: "diagnostics-error", role: "alert", "{message}" } },
                Some(Ok(result)) => rsx! { History { result } },
            }
        }
    }
}

/// What the server answered: the totals, the chart of the buckets and the table of the processes.
#[component]
pub(crate) fn History(result: Value) -> Element {
    let buckets = result["buckets"].as_array().cloned().unwrap_or_default();
    let processes = result["topProcesses"].as_array().cloned().unwrap_or_default();
    let error = some(&result["error"]).and_then(|error| error["message"].as_str()).map(str::to_owned);
    let tallest = buckets.iter().map(|bucket| bucket["maxCpuPercent"].as_f64().unwrap_or(0.0)).fold(1.0_f64, f64::max);
    let shallowest_child = processes.iter().filter(|process| process["isServerRoot"] != true).filter_map(|process| process["depth"].as_u64()).min();
    rsx! {
        dl { class: "stats",
            div { dt { "CPU Time" } dd { title: "Approximate active CPU time for the T3 server root process and its descendants during the selected window.", {logic::cpu_time(result["totalCpuSecondsApprox"].as_f64().unwrap_or(0.0))} } }
            div { dt { "Samples" } dd { title: "In-memory process samples retained by the server. This resets when the server restarts.", {logic::count(result["retainedSampleCount"].as_u64().unwrap_or(0))} } }
            div { dt { "Interval" } dd { {logic::duration(result["sampleIntervalMs"].as_f64().unwrap_or(0.0))} } }
            div { dt { "Processes" } dd { {logic::count(processes.len() as u64)} } }
        }
        if let Some(message) = error { p { class: "diagnostics-error", role: "alert", "{message}" } }
        div { class: "history-bars", role: "img", "aria-label": "CPU use over the period",
            for bucket in buckets {
                {
                    let (average, peak) = (bucket["avgCpuPercent"].as_f64().unwrap_or(0.0), bucket["maxCpuPercent"].as_f64().unwrap_or(0.0));
                    let key = bucket["startedAt"].as_str().unwrap_or_default().to_owned();
                    rsx! { div { key: "{key}", class: "history-bar", title: "Avg {average:.1}%, peak {peak:.1}%", "aria-label": "Average CPU {average:.1}%, peak CPU {peak:.1}%",
                        div { class: "history-bar-peak", style: "height:{logic::bar_height(peak, tallest):.1}%" }
                        div { class: "history-bar-average", style: "height:{logic::bar_height(average, tallest):.1}%" }
                    } }
                }
            }
        }
        if processes.is_empty() { p { class: "muted", "No process resource samples found for this window." } }
        else {
            table { class: "process-table", "aria-label": "Process resource history",
                thead { tr { th { "Process" } th { "CPU Time" } th { "Current" } th { "Average" } th { "Peak" } th { "Max Mem" } th { "Command" } th { "PID" } } }
                tbody {
                    for process in processes {
                        {
                            let command = process["command"].as_str().unwrap_or_default().to_owned();
                            let name = logic::short_name(&command);
                            let root = process["isServerRoot"] == true;
                            let indent = logic::visual_depth(root, process["depth"].as_u64().unwrap_or(0), shallowest_child).min(6) * 10;
                            let key = process["processKey"].as_str().unwrap_or_default().to_owned();
                            let percent = |field: &str| format!("{:.1}%", process[field].as_f64().unwrap_or(0.0));
                            rsx! { tr { key: "{key}",
                                td { style: "padding-left:{indent}px", "aria-label": if root { format!("Root process {name}") } else { format!("Child process {name}") }, title: "{command}", "{name}" }
                                td { {logic::cpu_time(process["cpuSecondsApprox"].as_f64().unwrap_or(0.0))} }
                                td { {percent("currentCpuPercent")} }
                                td { {percent("avgCpuPercent")} }
                                td { {percent("maxCpuPercent")} }
                                td { {bytes(process["maxRssBytes"].as_u64().unwrap_or(0))} }
                                td { class: "process-command", title: "{command}", "{command}" }
                                td { {process["pid"].as_u64().unwrap_or(0).to_string()} }
                            } }
                        }
                    }
                }
            }
        }
    }
}
