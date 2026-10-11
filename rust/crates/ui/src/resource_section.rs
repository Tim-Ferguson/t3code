//! The resource telemetry of the Diagnostics page (`ResourceTelemetryDiagnostics.tsx`): what the server and the apps it runs use
//! now, the health of the monitor that measures it, a restart for that monitor, and the history of the last minutes as bars.
//! The numbers come from one stream (`subscribeResourceTelemetry`), which also tells the server the page is looking, and the
//! history from `getResourceTelemetryHistory` once per window chosen or refresh asked for: nothing here ticks or animates, and
//! a bar is a box with a height. A server that runs no monitor answers the stream and the history with its own refusal
//! (`NativeServiceUnavailableError`); the page says so instead of drawing empty charts, which the original, whose server
//! always has a monitor, never has to.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::resource_telemetry::{self as logic, some};
use t3_contracts::AuthEnvironmentScope;

#[component]
pub(crate) fn Section(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let readable = use_memo(move || connected_allows(state, AuthEnvironmentScope::DiagnosticsRead));
    let maintain = use_memo(move || connected_allows(state, AuthEnvironmentScope::EnvironmentMaintain));
    let mut snapshot = use_signal(|| None::<Result<Value, String>>);
    let mut window = use_signal(|| logic::DEFAULT_WINDOW);
    let mut history = use_signal(|| None::<Result<Value, String>>);
    let mut reading = use_signal(|| 0u32);
    let mut restarting = use_signal(|| false);

    // The live numbers. The stream ends when the page does, and the connection it was opened on is the only one it answers.
    {
        let transport = transport.clone();
        use_resource(move || {
            let (transport, wanted) = (transport.clone(), readable());
            async move {
                if !wanted {
                    return;
                }
                snapshot.set(None);
                let owner = runtime::response_owner(&transport, state);
                let mut stream = match runtime::request_stream_reported(&transport, state, logic::SUBSCRIBE, json!({})) {
                    Ok(stream) => stream,
                    Err(message) => {
                        snapshot.set(Some(Err(message)));
                        return;
                    }
                };
                while let Some(chunk) = stream.next().await {
                    if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                        return;
                    }
                    match chunk {
                        Ok(values) => {
                            if let Some(latest) = values.into_iter().last() {
                                snapshot.set(Some(Ok(latest)));
                            }
                        }
                        Err(message) => {
                            snapshot.set(Some(Err(message)));
                            return;
                        }
                    }
                }
            }
        });
    }
    // The history of the window chosen, read again when the window changes or Refresh is pressed.
    {
        let transport = transport.clone();
        use_resource(move || {
            let (transport, wanted, chosen, _refreshed) = (transport.clone(), readable(), *window.read(), *reading.read());
            async move {
                if !wanted {
                    return;
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, logic::HISTORY, logic::history_input(chosen)).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                history.set(Some(answer));
            }
        });
    }
    let restart = {
        let transport = transport.clone();
        move |_| {
            if *restarting.peek() || !connected_allows(state, AuthEnvironmentScope::EnvironmentMaintain) {
                return;
            }
            restarting.set(true);
            let transport = transport.clone();
            spawn(async move {
                match runtime::request_value_reported(transport, state, logic::RETRY, json!({})).await {
                    Ok(result) => {
                        if result["snapshot"].is_object() {
                            snapshot.set(Some(Ok(result["snapshot"].clone())));
                        }
                    }
                    Err(message) => notice::push(state, Level::Error, "Could not restart the resource monitor", message),
                }
                restarting.set(false);
            });
        }
    };

    if !readable() {
        return rsx! {};
    }
    let shown = snapshot.read().clone();
    let past = history.read().clone();
    let error = shown.as_ref().and_then(|answer| answer.as_ref().err().cloned());
    let native = shown.as_ref().and_then(|answer| answer.as_ref().ok()).and_then(|value| value["health"]["native"]["status"].as_str().map(str::to_owned));
    let retry = logic::show_retry(native.as_deref(), error.as_deref()) && error.is_none();
    rsx! {
        section { class: "sc-section resource-telemetry", "aria-label": "Resource usage",
            h2 { "Resource usage" }
            match shown {
                None => rsx! { p { class: "muted", role: "status", "Loading resource usage\u{2026}" } },
                Some(Err(message)) => rsx! { Unavailable { message } },
                Some(Ok(value)) => rsx! { Snapshot { value, retry, can_retry: maintain(), restarting: *restarting.read(), onretry: restart } },
            }
            div { class: "resource-history", role: "group", "aria-label": "Resource history",
                for (index, (label, _, _)) in logic::WINDOWS.iter().enumerate() {
                    button { key: "{label}", r#type: "button", "aria-label": "History window {label}", "aria-pressed": "{*window.read() == index}", class: if *window.read() == index { "selected" } else { "" }, onclick: move |_| window.set(index), "{label}" }
                }
                button { r#type: "button", "aria-label": "Refresh history", onclick: move |_| { let next = *reading.peek() + 1; reading.set(next); }, "Refresh" }
            }
            match past {
                None => rsx! { p { class: "muted", role: "status", "Loading history\u{2026}" } },
                Some(Err(message)) => rsx! { p { class: "diagnostics-error", role: "alert", "{message}" } },
                Some(Ok(value)) => rsx! { History { buckets: value["buckets"].as_array().cloned().unwrap_or_default() } },
            }
        }
    }
}

/// What a server with no resource monitor answers the stream with, and what that means for the page. The original's server always
/// has a monitor, so it has no such state; this one answers `NativeServiceUnavailableError`.
#[component]
pub(crate) fn Unavailable(message: String) -> Element {
    rsx! {
        p { class: "diagnostics-error", role: "alert", "{message}" }
        p { class: "muted", "This server runs no resource monitor, so there is nothing to measure. The charts appear when it does." }
    }
}

/// The groups, the sampler's health and the restart.
#[component]
pub(crate) fn Snapshot(value: Value, retry: bool, can_retry: bool, restarting: bool, onretry: EventHandler<()>) -> Element {
    let interval = logic::format_sample_interval(value["sampleIntervalMs"].as_f64().unwrap_or(0.0));
    let health = &value["health"];
    let source = |name: &str| -> (String, Option<String>) { (health[name]["status"].as_str().unwrap_or("unknown").to_owned(), some(&health[name]["lastError"]).and_then(Value::as_str).map(str::to_owned)) };
    let (native, native_error) = source("native");
    let (desktop, desktop_error) = source("desktop");
    rsx! {
        p { class: "muted", "Sampled every {interval}." }
        table { class: "process-table", "aria-label": "Resource groups",
            thead { tr { th { "Group" } th { "Processes" } th { "CPU" } th { "Memory" } th { "Read" } th { "Write" } } }
            tbody {
                for (key, label) in logic::GROUPS {
                    {
                        let group = &value["groups"][key];
                        rsx! { tr { key: "{key}",
                            td { "{label}" }
                            td { {group["processCount"].as_u64().unwrap_or(0).to_string()} }
                            td { {format!("{:.1}%", group["currentCpuPercent"].as_f64().unwrap_or(0.0))} }
                            td { {logic::format_bytes(group["currentRssBytes"].as_f64().unwrap_or(0.0))} }
                            td { {logic::format_rate(group["ioReadBytesPerSecond"].as_f64().unwrap_or(0.0))} }
                            td { {logic::format_rate(group["ioWriteBytesPerSecond"].as_f64().unwrap_or(0.0))} }
                        } }
                    }
                }
            }
        }
        dl { class: "stats",
            div { dt { "Process monitor" } dd { "{native}" if let Some(error) = native_error { span { class: "muted", " \u{b7} {error}" } } } }
            div { dt { "Desktop app" } dd { "{desktop}" if let Some(error) = desktop_error { span { class: "muted", " \u{b7} {error}" } } } }
        }
        if retry {
            button { r#type: "button", "aria-label": "Restart resource monitor", disabled: !can_retry || restarting, title: if can_retry { "" } else { "This connection may not restart the resource monitor." }, onclick: move |_| onretry.call(()), if restarting { "Restarting\u{2026}" } else { "Restart monitor" } }
        }
    }
}

/// CPU average (and the I/O of each bucket) as bars: boxes with a height, no animation. A bar says its numbers when hovered.
#[component]
pub(crate) fn History(buckets: Vec<Value>) -> Element {
    if buckets.is_empty() {
        return rsx! { p { class: "muted", "No samples in this window yet." } };
    }
    let max_cpu = logic::cpu_scale_max(&buckets);
    let max_io = buckets.iter().map(|bucket| bucket["ioReadBytes"].as_f64().unwrap_or(0.0) + bucket["ioWriteBytes"].as_f64().unwrap_or(0.0)).fold(1.0, f64::max);
    rsx! {
        div { class: "resource-legend muted", span { "CPU average" } span { "I/O reads" } span { "I/O writes" } }
        div { class: "resource-chart", role: "img", "aria-label": "CPU and I/O history, {buckets.len()} samples",
            for (index, bucket) in buckets.iter().enumerate() {
                {
                    let cpu = logic::bar_height(bucket["avgCpuPercent"].as_f64().unwrap_or(0.0), max_cpu, 2.0);
                    let read = logic::bar_height(bucket["ioReadBytes"].as_f64().unwrap_or(0.0), max_io, 1.0);
                    let write = logic::bar_height(bucket["ioWriteBytes"].as_f64().unwrap_or(0.0), max_io, 1.0);
                    let tip = format!("CPU avg {:.1}%\nCPU peak {:.1}%\nRead {}\nWrite {}", bucket["avgCpuPercent"].as_f64().unwrap_or(0.0), bucket["maxCpuPercent"].as_f64().unwrap_or(0.0), logic::format_bytes(bucket["ioReadBytes"].as_f64().unwrap_or(0.0)), logic::format_bytes(bucket["ioWriteBytes"].as_f64().unwrap_or(0.0)));
                    rsx! { div { key: "{index}", class: "resource-bucket", title: "{tip}",
                        span { class: "resource-bar cpu", style: "height:{cpu}%" }
                        span { class: "resource-bar read", style: "height:{read}%" }
                        span { class: "resource-bar write", style: "height:{write}%" }
                    } }
                }
            }
        }
    }
}
