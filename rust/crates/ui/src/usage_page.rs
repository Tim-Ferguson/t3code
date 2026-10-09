//! The Usage page: what the coding agents on this machine cost and processed (Cost, Tokens) and
//! how much of each subscription is left (Limits). Port of `UsagePage.tsx`,
//! `UsageProviderChart.tsx`, `UsageShareBar.tsx` and `UsageModelDialog.tsx` for the one
//! environment this app is connected to. Price overrides and the Cursor keychain prompt are
//! not ported.
//!
//! The summary read scans transcript files and can take seconds. It runs when the page opens,
//! when the period changes and when the refresh button is pressed, never on a timer, and says
//! it is scanning without inventing progress. A late answer for another window, connection or
//! environment is dropped. After a lost connection the last result stays on screen marked out
//! of date, and refreshing is unavailable until it returns.
//!
//! The chart is plain SVG drawn from the computed series; hovering a period shows its readout
//! through the chart's own signal, so it repaints nothing else.
use crate::{
    limits_view,
    provider_manage::{allowed_now, ask, follow_config},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
};
use dioxus::prelude::*;
use serde_json::json;
use std::{collections::HashMap, rc::Rc};
use t3_client::{
    connection::ConnectionStatus,
    usage::{self as logic, EnvironmentUsage, MergedUsage, Preferences, Window, Zone},
};
use t3_contracts::{AuthEnvironmentScope, EnvironmentId};

/// What a finished scan produced, with the connection and window it answered.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub environment: EnvironmentId,
    pub days: i64,
    pub window: Window,
    pub read_at: String,
    pub usage: Vec<EnvironmentUsage>,
    pub merged: MergedUsage,
    pub messages: Vec<String>,
    pub pricing: logic::Pricing,
}
#[derive(Debug, Clone)]
pub(crate) struct Data(pub Rc<Loaded>);
impl PartialEq for Data {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Model {
    pub metric: &'static str,
    pub days: i64,
    pub hydrated: bool,
    pub loaded: Option<Data>,
    pub scanning: bool,
    pub error: Option<String>,
    /// Bumped by every scan; only the latest may answer.
    pub generation: u64,
    /// The instant the limits were last read or refreshed, which is the clock their countdowns use.
    pub limits_now: f64,
    pub limits_refreshing: bool,
    /// An automatic refresh of the limits waits until this time (a manual one never does).
    pub limits_refresh_after: Option<(EnvironmentId, i64)>,
}
impl Default for Model {
    fn default() -> Self {
        let prefs = Preferences::default();
        Self { metric: prefs.metric, days: prefs.window_days, hydrated: false, loaded: None, scanning: false, error: None, generation: 0, limits_now: 0.0, limits_refreshing: false, limits_refresh_after: None }
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
/// The viewer's IANA zone, which the server buckets days in.
pub(crate) fn zone_name() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let options = js_sys::Object::new();
        let format = js_sys::Intl::DateTimeFormat::new(&js_sys::Array::new(), &options);
        js_sys::Reflect::get(&format.resolved_options(), &"timeZone".into()).ok().and_then(|value| value.as_string()).filter(|name| !name.is_empty()).unwrap_or_else(|| "UTC".into())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        iana_time_zone::get_timezone().ok().filter(|name| !name.is_empty()).unwrap_or_else(|| "UTC".into())
    }
}
fn window_for(days: i64) -> Window {
    let name = zone_name();
    let offset = |ms: i64| crate::scheduled_tasks_page::zone(ms);
    logic::make_window(days, now_ms(), Zone { name: &name, offset: &offset }, days == 1)
}

// ---- preferences ----------------------------------------------------------------------------

async fn read_preferences() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()?.local_storage().ok().flatten()?.get_item(logic::PREFERENCES_KEY).ok().flatten()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let args = json!({"key":logic::PREFERENCES_KEY});
        document::eval(&format!("return window.localStorage.getItem({args}.key);")).join::<Option<String>>().await.ok().flatten()
    }
}
fn write_preferences(preferences: Preferences) {
    let bytes = preferences.write();
    #[cfg(target_arch = "wasm32")]
    if let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten()) {
        let _ = storage.set_item(logic::PREFERENCES_KEY, &bytes);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let args = json!({"key":logic::PREFERENCES_KEY,"bytes":bytes});
        let _ = document::eval(&format!("try{{window.localStorage.setItem({args}.key,{args}.bytes);}}catch{{}}return true;"));
    }
}

// ---- actions --------------------------------------------------------------------------------

/// Opens the page.
pub(crate) fn open(state: Store<UiModel>) {
    if *state.view().peek() != View::Usage {
        state.view().set(View::Usage);
    }
}
fn connected(state: Store<UiModel>) -> bool {
    *state.status().peek() == ConnectionStatus::Connected
}

/// Reads the usage summary for the selected period. `rates` refreshes the price table first, as
/// the refresh button does. The newest scan answers; an older one is dropped.
pub(crate) fn scan(transport: &TransportHandle, state: Store<UiModel>, rates: bool) {
    if !allowed_now(state, AuthEnvironmentScope::DiagnosticsRead) {
        return;
    }
    let Some(destination) = state.destination().peek().clone() else { return };
    let days = state.usage().peek().days;
    let window = window_for(days);
    let generation = {
        let mut model = state.usage().write();
        model.generation += 1;
        model.scanning = true;
        model.error = None;
        model.generation
    };
    let transport = transport.clone();
    spawn(async move {
        if rates {
            // The table is refreshed so the new scan prices against it; a failure leaves the old one, which the summary reports.
            let _ = ask(&transport, state, "server.refreshUsageRates", json!({})).await;
        }
        let answer = ask(&transport, state, "server.getUsageSummary", window.to_value()).await;
        if state.usage().peek().generation != generation {
            return;
        }
        let mut model = state.usage().write();
        model.scanning = false;
        let Some(answer) = answer else { return };
        match answer.and_then(|value| logic::Summary::from_value(&value)) {
            Ok(summary) => {
                let label = state.environments().peek().records.get(&destination).map(|record| record.label.clone()).unwrap_or_else(|| destination.to_string());
                let messages = {
                    let mut seen: Vec<String> = Vec::new();
                    for source in &summary.sources {
                        if let Some(message) = &source.message {
                            if source.action.is_none() && (source.status == "partial" || source.status == "failed" || source.fingerprint.provider == "cursor") && !seen.contains(message) {
                                seen.push(message.clone());
                            }
                        }
                    }
                    seen
                };
                let read_at = summary.read_at.clone();
                let pricing = summary.pricing.clone();
                let usage = vec![EnvironmentUsage { environment_id: destination.to_string(), label, summary }];
                let merged = logic::merge_usage(&usage, logic::USAGE_CONTRACT_VERSION);
                model.loaded = Some(Data(Rc::new(Loaded { environment: destination, days, window, read_at, usage, merged, messages, pricing })));
            }
            Err(message) => model.error = Some(message),
        }
    });
}

/// Asks the server to read the providers again, which refreshes their limits. An automatic call
/// (opening the tab) waits out five minutes since the last one; a manual call never does.
pub(crate) fn refresh_limits(transport: &TransportHandle, state: Store<UiModel>, automatic: bool) {
    if !allowed_now(state, AuthEnvironmentScope::OrchestrationRead) || state.usage().peek().limits_refreshing {
        return;
    }
    let Some(destination) = state.destination().peek().clone() else { return };
    let now = now_ms();
    if automatic && state.usage().peek().limits_refresh_after.as_ref().is_some_and(|(id, after)| *id == destination && now < *after) {
        state.usage().write().limits_now = now as f64;
        return;
    }
    state.usage().write().limits_refreshing = true;
    let transport = transport.clone();
    spawn(async move {
        let answer = ask(&transport, state, "server.refreshProviders", json!({})).await;
        if answer.as_ref().is_some_and(Result::is_ok) {
            follow_config(&transport, state).await;
        }
        let mut model = state.usage().write();
        model.limits_refreshing = false;
        model.limits_now = now_ms() as f64;
        model.limits_refresh_after = Some((destination, now_ms() + 5 * 60_000));
    });
}

/// What the open page shows, started when the metric or period asks for data it does not have.
fn ensure(transport: &TransportHandle, state: Store<UiModel>) {
    let (metric, days, covered) = {
        let model = state.usage().peek();
        let destination = state.destination().peek().clone();
        (model.metric, model.days, model.loaded.as_ref().is_some_and(|loaded| Some(&loaded.0.environment) == destination.as_ref() && loaded.0.days == days))
    };
    if metric == "limits" {
        state.usage().write().limits_now = now_ms() as f64;
        refresh_limits(transport, state, true);
    } else if !covered && !state.usage().peek().scanning {
        scan(transport, state, false);
    }
}

pub(crate) fn select_metric(transport: &TransportHandle, state: Store<UiModel>, metric: &'static str) {
    if state.usage().peek().metric != metric {
        state.usage().write().metric = metric;
        let days = state.usage().peek().days;
        write_preferences(Preferences { metric, window_days: days });
    }
    if *state.view().peek() == View::Usage {
        ensure(transport, state);
    }
}
pub(crate) fn select_period(transport: &TransportHandle, state: Store<UiModel>, days: i64) {
    let metric = state.usage().peek().metric;
    if metric == "limits" {
        return;
    }
    state.usage().write().days = days;
    write_preferences(Preferences { metric, window_days: days });
    if *state.view().peek() == View::Usage {
        scan(transport, state, false);
    }
}
fn refresh(transport: &TransportHandle, state: Store<UiModel>) {
    if state.usage().peek().metric == "limits" {
        state.usage().write().limits_now = now_ms() as f64;
        refresh_limits(transport, state, false);
    } else {
        scan(transport, state, true);
    }
}

// ---- the page -------------------------------------------------------------------------------

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let destination = state.destination().read().clone();
    rsx! { section { class: "settings-page usage-page",
        if let Some(destination) = destination {
            Body { key: "{destination}", state, transport }
        } else {
            h1 { "Usage" }
            p { "Connect an environment to see usage." }
            button { onclick: move |_| state.view().set(View::Connections), "Open connections" }
        }
    } }
}

#[component]
fn Body(state: Store<UiModel>, transport: TransportHandle) -> Element {
    // Saved choices apply once, then the page reads what its first view needs.
    {
        let transport = transport.clone();
        use_future(move || {
            let transport = transport.clone();
            async move {
                if !state.usage().peek().hydrated {
                    let saved = read_preferences().await;
                    let mut model = state.usage().write();
                    if !model.hydrated {
                        let preferences = Preferences::read(saved.as_deref());
                        model.metric = preferences.metric;
                        model.days = preferences.window_days;
                        model.hydrated = true;
                    }
                }
                ensure(&transport, state);
            }
        });
    }
    // A connection that returns reads what the page shows again; the last result was from before.
    {
        let transport = transport.clone();
        let mut seen_connected = use_signal(|| connected(state));
        use_effect(move || {
            let now = *state.status().read() == ConnectionStatus::Connected;
            if now && !*seen_connected.peek() && state.usage().peek().hydrated {
                state.usage().write().loaded = state.usage().peek().loaded.clone();
                if state.usage().peek().metric == "limits" {
                    refresh_limits(&transport, state, false);
                } else {
                    scan(&transport, state, false);
                }
            }
            seen_connected.set(now);
        });
    }
    let model = state.usage().read().clone();
    let label = state.destination().read().as_ref().and_then(|id| state.environments().read().records.get(id).map(|record| record.label.clone())).unwrap_or_default();
    let is_connected = *state.status().read() == ConnectionStatus::Connected;
    let can_read = state.destination().read().as_ref().is_some_and(|id| state.environments().read().allows(id, AuthEnvironmentScope::DiagnosticsRead));
    let busy = if model.metric == "limits" { model.limits_refreshing } else { model.scanning };
    let keys = try_consume_context::<crate::keys::Keys>();
    let hint = |command: &str| keys.and_then(|keys| keys.label_in(command, "usagePageOpen")).map(|label| format!(" ({label})")).unwrap_or_default();
    let loaded = model.loaded.clone().filter(|loaded| state.destination().read().as_ref() == Some(&loaded.0.environment));
    let showing_limits = model.metric == "limits";
    rsx! {
        header { class: "usage-header",
            h1 { "Usage" }
            span { class: "muted usage-environment", "{label}" }
            div { class: "usage-controls",
                div { class: "usage-segmented", role: "group", "aria-label": "Usage metric",
                    for (value, text, command) in logic::METRIC_OPTIONS {
                        button { key: "{value}", r#type: "button", "aria-pressed": "{model.metric == value}", title: "{text}{hint(command)}",
                            onclick: { let transport = transport.clone(); move |_| select_metric(&transport, state, value) }, "{text}" }
                    }
                }
                div { class: "usage-segmented", role: "group", "aria-label": "Usage period",
                    for (days, text, command) in logic::WINDOW_OPTIONS {
                        button { key: "{days}", r#type: "button", "aria-pressed": "{model.days == days}", disabled: showing_limits, title: "{text}{hint(command)}",
                            onclick: { let transport = transport.clone(); move |_| select_period(&transport, state, days) }, "{text}" }
                    }
                }
                button { r#type: "button", "aria-label": if showing_limits { "Refresh limits" } else { "Refresh usage" }, "aria-busy": "{busy}", disabled: busy || !is_connected || (!showing_limits && !can_read),
                    onclick: { let transport = transport.clone(); move |_| refresh(&transport, state) }, if busy { "Refreshing\u{2026}" } else { "\u{21bb} Refresh" } }
            }
        }
        if !is_connected { p { class: "stale-banner", role: "status", "The connection was lost. What is shown was read before it, and cannot be refreshed until the connection returns." } }
        if showing_limits { LimitsTab { state, transport: transport.clone() } }
        else if !can_read { p { class: "muted", role: "status", "This connection cannot read usage." } }
        else { Totals { state, transport: transport.clone(), loaded, scanning: model.scanning, error: model.error.clone(), metric: model.metric, days: model.days } }
        limits_view::RedeemDialog { state, transport }
    }
}

#[component]
fn LimitsTab(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let now = state.usage().read().limits_now;
    let presentation = limits_view::presentation_of(state);
    let refreshing = state.usage().read().limits_refreshing;
    let _ = &transport;
    rsx! {
        if refreshing { p { class: "muted", role: "status", "Refreshing limits\u{2026}" } }
        match presentation {
            Some(presentation) if now > 0.0 => rsx! { limits_view::Pooled { state, presentations: vec![presentation], now } },
            Some(_) | None => rsx! { p { class: "muted", role: "status", "Reading limits\u{2026}" } },
        }
    }
}

fn metric_value(merged: &MergedUsage, cost: bool) -> String {
    if cost { logic::format_usd(merged.cost_usd) } else { logic::format_tokens(merged.total_tokens) }
}

#[component]
fn Totals(state: Store<UiModel>, transport: TransportHandle, loaded: Option<Data>, scanning: bool, error: Option<String>, metric: &'static str, days: i64) -> Element {
    let _ = (&state, &transport);
    let cost = metric == "cost";
    let hourly = days == 1;
    let mut model_key = use_signal(|| None::<String>);
    let mut by_time = use_signal(|| false);
    let mut unpriced_open = use_signal(|| false);
    let Some(data) = loaded else {
        return rsx! {
            if let Some(error) = error { p { class: "error-banner", role: "alert", "Could not read usage. {error}" } }
            else if scanning { p { class: "muted", role: "status", "Scanning transcripts\u{2026} this can take a few seconds." } }
            else { p { class: "muted", role: "status", "No usage has been read yet." } }
        };
    };
    let loaded = data.0.clone();
    let merged = &loaded.merged;
    let active = logic::providers_with_usage(&merged.providers);
    let read = t3_client::timeline_items::short_timestamp(&loaded.read_at, crate::scheduled_tasks_page::zone(now_ms()));
    let zone = loaded.window.time_zone.clone();
    let offset = |ms: i64| crate::scheduled_tasks_page::zone(ms);
    let zone_value = Zone { name: &zone, offset: &offset };
    let window_label = match (&loaded.window.since_time, &loaded.window.until_time) {
        (Some(since), Some(until)) if hourly => format!("{} to {}", logic::format_date_time_short(since, zone_value), logic::format_date_time_short(until, zone_value)),
        _ => format!("{} to {}", logic::format_day_short(&loaded.window.since_day), logic::format_day_short(&loaded.window.until_day)),
    };
    let periods: Vec<String> = if hourly { loaded.window.since_time.as_deref().zip(loaded.window.until_time.as_deref()).map(|(since, until)| logic::enumerate_hour_starts(since, until)).unwrap_or_default() } else { logic::enumerate_days(&loaded.window.since_day, &loaded.window.until_day) };
    let columns = {
        let maps: Vec<(&str, &indexmap::IndexMap<String, logic::Share>)> = if hourly { merged.hourly.iter().map(|entry| (entry.hour_start.as_str(), &entry.by_provider)).collect() } else { merged.daily.iter().map(|entry| (entry.day.as_str(), &entry.by_provider)).collect() };
        let by_period: HashMap<&str, &indexmap::IndexMap<String, logic::Share>> = maps.into_iter().collect();
        logic::build_period_columns(&periods, &by_period, !cost)
    };
    let chart = logic::build_chart(&active, columns);
    let labels: Vec<String> = periods.iter().map(|period| if hourly { logic::format_hour_short(period, zone_value) } else { logic::format_day_short(period) }).collect();
    let tooltips: Vec<String> = periods.iter().map(|period| match (&loaded.window.until_time, hourly) { (Some(reference), true) => logic::format_relative_hour_short(period, reference, zone_value), _ => if hourly { logic::format_hour_short(period, zone_value) } else { logic::format_day_short(period) } }).collect();
    let selected = model_key.read().clone().and_then(|key| merged.models.iter().find(|model| model.key() == key).cloned());
    let unpriced_share = merged.cost_quality.unpriced_share;
    rsx! {
        if let Some(error) = error { p { class: "error-banner", role: "alert", "Could not read usage. {error}" } }
        if scanning { p { class: "muted", role: "status", "Scanning transcripts\u{2026} this can take a few seconds. The totals below are from the previous read." } }
        for message in loaded.messages.iter() { p { key: "{message}", class: "muted", "{message}" } }
        p { class: "muted usage-window", "{window_label} \u{b7} read {read}" }
        section { class: "usage-overview",
            div { class: "usage-summary",
                div { class: "usage-headline",
                    span { class: "usage-headline-value", "{metric_value(merged, cost)}" }
                    span { class: "muted usage-headline-note", "{logic::format_count(merged.sessions)} sessions"
                        if cost { " \u{b7} API estimate" if unpriced_share > 0.0 { " " button { r#type: "button", class: "usage-info", "aria-label": "Unpriced usage details", "aria-expanded": "{unpriced_open}", onclick: move |_| unpriced_open.toggle(), "\u{24d8}" } } }
                    }
                    if cost && *unpriced_open.read() && unpriced_share > 0.0 { span { class: "muted", role: "status", "API estimate excludes {logic::format_percent(unpriced_share, 1)} unpriced records." } }
                }
                for provider in active.iter().copied() {
                    ProviderSummary { key: "{provider}", provider, totals: merged.providers.iter().find(|entry| entry.provider == provider).cloned(), cost }
                }
            }
            div { class: "usage-chart-block",
                h2 { "{if hourly { \"Hourly\" } else { \"Daily\" }} {if cost { \"cost\" } else { \"processed tokens\" }}" }
                ProviderChart { chart: chart.clone(), labels, tooltips, tokens: !cost, hourly, providers: active.clone() }
            }
        }
        section { class: "usage-totals",
            h2 { "Totals" }
            div { class: "usage-metrics",
                Metric { label: "Processed tokens", value: logic::format_tokens(merged.total_tokens) }
                Metric { label: "Cached input", value: logic::format_tokens(merged.cached_input_tokens) }
                Metric { label: "Uncached input", value: logic::format_tokens(merged.uncached_input_tokens) }
                Metric { label: "Output", value: logic::format_tokens(merged.output_tokens) }
                Metric { label: "Cache savings", value: logic::format_usd(merged.cost_quality.cache_savings_usd) }
            }
        }
        if merged.total_tokens > 0.0 {
            section { class: "usage-shares",
                if !cost { ShareBar { label: "Tokens by type", segments: logic::token_type_segments(&logic::TokenTotals { uncached_input_tokens: merged.uncached_input_tokens, cached_input_tokens: merged.cached_input_tokens, cache_creation_tokens: merged.cache_creation_tokens, output_tokens: merged.output_tokens, reasoning_tokens: 0.0 }), tokens: true, aside: None } }
                else {
                    ShareBar { label: "Cost by type", segments: logic::cost_type_segments(&merged.category_cost), tokens: false, aside: None }
                    if merged.speed_cost.fast + merged.speed_cost.ultrafast > 0.0 { ShareBar { label: "Cost by speed", segments: logic::speed_cost_segments(&merged.speed_cost), tokens: false, aside: Some(merged.speed_cost.premium) } }
                }
            }
        }
        section { class: "usage-breakdown",
            div { class: "usage-breakdown-head", h2 { "Breakdown" }
                div { class: "usage-segmented", role: "group", "aria-label": "Usage breakdown",
                    button { r#type: "button", "aria-pressed": "{!*by_time.read()}", onclick: move |_| by_time.set(false), "Model" }
                    button { r#type: "button", "aria-pressed": "{*by_time.read()}", onclick: move |_| by_time.set(true), if hourly { "Hour" } else { "Day" } }
                }
            }
            if *by_time.read() { TimeTable { loaded: data.clone(), hourly, active: active.clone() } }
            else { ModelTable { loaded: data.clone(), tokens: !cost, onopen: move |key| model_key.set(Some(key)) } }
        }
        if let Some(model) = selected {
            ModelDialog { loaded: data.clone(), model, cost, hourly, periods: periods.clone(), labels: chart_labels(&periods, hourly, zone_value), onclose: move |_| model_key.set(None) }
        }
    }
}
fn chart_labels(periods: &[String], hourly: bool, zone: Zone) -> Vec<String> {
    periods.iter().map(|period| if hourly { logic::format_hour_short(period, zone) } else { logic::format_day_short(period) }).collect()
}

#[component]
fn Metric(label: &'static str, value: String) -> Element {
    rsx! { div { class: "usage-metric", span { class: "muted", "{label}" } span { class: "usage-metric-value", "{value}" } } }
}

#[component]
fn ProviderSummary(provider: &'static str, totals: Option<logic::ProviderTotals>, cost: bool) -> Element {
    let presentation = logic::presentation(provider);
    let (label, color) = presentation.map_or((provider, "var(--foreground)"), |presentation| (presentation.label, presentation.color));
    let share = totals.as_ref().map_or(0.0, |totals| if cost { totals.cost_share } else { totals.token_share });
    let sessions = totals.as_ref().map_or(0.0, |totals| totals.sessions);
    let (tokens, usd) = totals.as_ref().map_or((0.0, 0.0), |totals| (totals.total_tokens, totals.cost_usd));
    rsx! { div { class: "usage-provider",
        div { class: "usage-provider-head",
            span { class: "usage-provider-name", span { class: "usage-dot", "aria-hidden": "true", style: "background-color:{color}" } span { "{label}" } span { class: "muted usage-provider-sessions", "{logic::format_count(sessions)} {if sessions == 1.0 { \"session\" } else { \"sessions\" }}" } }
            span { class: "usage-provider-value", "{if cost { logic::format_usd(usd) } else { logic::format_tokens(tokens) }}" }
        }
        span { class: "muted", if cost { "{logic::format_percent(share, 1)} of cost \u{b7} {logic::format_tokens(tokens)} tokens" } else { "{logic::format_percent(share, 1)} of tokens \u{b7} {logic::format_usd(usd)}" } }
    } }
}

#[component]
fn ShareBar(label: &'static str, segments: Vec<logic::Segment>, tokens: bool, aside: Option<f64>) -> Element {
    let format = |value: f64| if tokens { logic::format_tokens(value) } else { logic::format_usd(value) };
    let visible: Vec<&logic::Segment> = segments.iter().filter(|segment| segment.value > 0.0).collect();
    let total: f64 = visible.iter().map(|segment| segment.value).sum();
    if total <= 0.0 {
        return rsx! {};
    }
    let summary = visible.iter().map(|segment| format!("{} {}", segment.label, format(segment.value))).collect::<Vec<_>>().join(", ");
    rsx! { div { class: "usage-share",
        div { class: "usage-share-head", h3 { "{label}" } if let Some(premium) = aside { span { class: "muted", "Premium " span { "{logic::format_usd(premium)}" } } } }
        div { class: "usage-share-bar", role: "img", "aria-label": "{label}: {summary}",
            for segment in visible.iter() { div { key: "{segment.label}", class: "usage-share-segment", title: "{segment.label} \u{b7} {format(segment.value)} \u{b7} {logic::format_percent(segment.value / total, 1)}", style: "flex:{segment.value} 1 0;background-color:{segment.color}" } }
        }
        div { class: "usage-share-legend",
            for segment in visible.iter() { span { key: "{segment.label}", span { class: "usage-swatch", "aria-hidden": "true", style: "background-color:{segment.color}" } span { class: "muted", "{segment.label}" } span { "{format(segment.value)}" } } }
        }
    } }
}

/// The provider chart: gridlines and labels from the scale, one filled curve per provider, and a
/// readout for the period the pointer or focus is on.
#[component]
fn ProviderChart(chart: logic::Chart, labels: Vec<String>, tooltips: Vec<String>, tokens: bool, hourly: bool, providers: Vec<&'static str>) -> Element {
    let mut hover = use_signal(|| None::<usize>);
    let count = labels.len();
    let format = |value: f64| if tokens { logic::format_tokens(value) } else { logic::format_usd(value) };
    let summary = format!("{} {} by provider", if hourly { "Hourly" } else { "Daily" }, if tokens { "processed tokens" } else { "cost" });
    let index = *hover.read();
    let readout = index.and_then(|index| chart.columns.get(index).map(|column| (index, column.clone())));
    let color = |provider: &str| logic::presentation(provider).map_or("var(--foreground)", |presentation| presentation.color);
    rsx! { div { class: "usage-chart",
        div { class: "usage-chart-body",
            div { class: "usage-axis", for (tick, y) in chart.ticks.iter().copied() { span { key: "{tick}", style: "top:{y / logic::VIEW_HEIGHT * 100.0}%", "{if tick == 0.0 { \"0\".to_owned() } else { format(tick) }}" } } }
            div { class: "usage-plot", onmouseleave: move |_| hover.set(None),
                svg { class: "usage-plot-svg", view_box: "0 0 960 260", preserve_aspect_ratio: "none", role: "img", "aria-label": "{summary}",
                    for (tick, y) in chart.ticks.iter().copied() { line { key: "{tick}", class: "usage-grid", x1: "0", x2: "960", y1: "{y}", y2: "{y}", stroke: "currentColor", "stroke-width": "1", "vector-effect": "non-scaling-stroke" } }
                    for series in chart.series.iter() { path { key: "{series.provider}", d: "{series.area}", fill: "{color(series.provider)}", "fill-opacity": "0.12" } }
                    for series in chart.series.iter() { path { key: "{series.provider}", d: "{series.line}", fill: "none", stroke: "{color(series.provider)}", "stroke-width": "2", "vector-effect": "non-scaling-stroke" } }
                    if let Some(index) = index { line { class: "usage-cursor", x1: "{index as f64 * chart.step_x}", x2: "{index as f64 * chart.step_x}", y1: "{logic::PLOT_TOP}", y2: "{logic::VIEW_HEIGHT}", stroke: "currentColor", "stroke-width": "1", "vector-effect": "non-scaling-stroke" } }
                }
                div { class: "usage-hit", for position in 0..count { button { key: "{position}", r#type: "button", class: "usage-hit-cell", "aria-label": "{tooltips[position]}", onmouseenter: move |_| hover.set(Some(position)), onfocus: move |_| hover.set(Some(position)), onclick: move |_| hover.set(Some(position)) } } }
                if let Some((position, column)) = readout {
                    div { class: if count > 1 && position * 2 > count { "usage-tooltip left" } else { "usage-tooltip" }, style: "left:{if count > 1 { position as f64 / (count as f64 - 1.0) * 100.0 } else { 0.0 }}%", role: "status",
                        div { class: "muted", "{tooltips[position]}" }
                        for provider in providers.iter().copied() {
                            div { key: "{provider}", class: "usage-tooltip-row", span { class: "muted", "{logic::presentation(provider).map_or(provider, |presentation| presentation.label)}" } span { "{format(column.bands.iter().find(|band| band.provider == provider).map_or(0.0, |band| band.value))}" } }
                        }
                        div { class: "usage-tooltip-row total", span { class: "muted", "Total" } span { "{format(column.total)}" } }
                    }
                }
            }
        }
        div { class: "usage-axis-x muted", span { "{labels.first().cloned().unwrap_or_default()}" } span { "{labels.get(count / 2).cloned().unwrap_or_default()}" } span { "{labels.last().cloned().unwrap_or_default()}" } }
    } }
}

#[component]
fn ModelTable(loaded: Data, tokens: bool, onopen: EventHandler<String>) -> Element {
    let merged = &loaded.0.merged;
    let models = if tokens { logic::sort_models_by_tokens(&merged.models) } else { merged.models.clone() };
    let peak = models.iter().fold(0.0f64, |peak, model| peak.max(if tokens { model.total_tokens } else { model.cost_usd }));
    rsx! { table { class: "usage-table", "aria-label": "Usage by model",
        thead { tr { th { class: "left", "#" } th { class: "left", "Model" } th { "Cost" } th { class: "usage-share-col", "Share" } th { "Tokens" } } }
        tbody {
            if models.is_empty() { tr { td { colspan: "5", class: "usage-empty", "No activity in this window." } } }
            for (index, model) in models.iter().enumerate() {
                {
                    let value = if tokens { model.total_tokens } else { model.cost_usd };
                    let share = logic::model_share(model, tokens);
                    let width = if value > 0.0 && peak > 0.0 { format!("max(0.5rem, {}%)", value / peak * 100.0) } else { "0".into() };
                    let color = logic::presentation(&model.provider).map_or("var(--foreground)", |presentation| presentation.color);
                    let key = model.key();
                    rsx! { tr { key: "{key}",
                        td { class: "left", "{index + 1}" }
                        td { class: "left", button { r#type: "button", class: "usage-model-open", "aria-label": "Open {model.model}", onclick: move |_| onopen.call(key.clone()), "{model.model}" }
                            div { class: "usage-model-bar", "aria-hidden": "true", div { style: "width:{width};background-color:{color}" } } }
                        td { if model.cost_unknown() { span { class: "muted", "Unpriced" } } else { "{logic::format_usd(model.cost_usd)}" } }
                        td { class: "usage-share-col", "{share.map(|share| logic::format_percent(share, 1)).unwrap_or_default()}" }
                        td { "{logic::format_tokens(model.total_tokens)}" }
                    } }
                }
            }
        }
    } }
}

#[component]
fn TimeTable(loaded: Data, hourly: bool, active: Vec<&'static str>) -> Element {
    let data = &loaded.0;
    let zone = data.window.time_zone.clone();
    let offset = |ms: i64| crate::scheduled_tasks_page::zone(ms);
    let zone_value = Zone { name: &zone, offset: &offset };
    // Newest first: the window can run 90 periods, so the interesting end belongs at the top.
    let rows: Vec<(String, String, f64, f64, &indexmap::IndexMap<String, logic::Share>)> = if hourly {
        data.merged.hourly.iter().rev().map(|entry| (entry.hour_start.clone(), logic::format_hour_short(&entry.hour_start, zone_value), entry.cost_usd, entry.total_tokens, &entry.by_provider)).collect()
    } else {
        data.merged.daily.iter().rev().map(|entry| (entry.day.clone(), logic::format_day_short(&entry.day), entry.cost_usd, entry.total_tokens, &entry.by_provider)).collect()
    };
    rsx! { table { class: "usage-table", "aria-label": "Usage by {if hourly { \"hour\" } else { \"day\" }}",
        thead { tr { th { class: "left", "{if hourly { \"Hour\" } else { \"Day\" }}" } for provider in active.iter().copied() { th { key: "{provider}", "{logic::presentation(provider).map_or(provider, |presentation| presentation.label)}" } } th { "Total" } th { "Tokens" } } }
        tbody {
            if rows.is_empty() { tr { td { colspan: "{active.len() + 3}", class: "usage-empty", "No activity in this window." } } }
            for (key, label, cost, tokens, by_provider) in rows {
                tr { key: "{key}", td { class: "left", "{label}" }
                    for provider in active.iter().copied() { td { key: "{provider}", "{logic::format_usd(by_provider.get(provider).map_or(0.0, |share| share.cost_usd))}" } }
                    td { "{logic::format_usd(cost)}" } td { "{logic::format_tokens(tokens)}" } }
            }
        }
    } }
}

/// One model's usage in the current window, opened from the Breakdown list. The trend and mixes
/// come from the same merge as the page, narrowed to this model's buckets.
#[component]
fn ModelDialog(loaded: Data, model: logic::ModelTotals, cost: bool, hourly: bool, periods: Vec<String>, labels: Vec<String>, onclose: EventHandler<()>) -> Element {
    let data = &loaded.0;
    let narrowed: Vec<EnvironmentUsage> = data.usage.iter().map(|environment| {
        let mut environment = environment.clone();
        environment.summary.buckets.retain(|bucket| bucket.provider == model.provider && bucket.model == model.model);
        environment
    }).collect();
    let usage = logic::merge_usage(&narrowed, logic::USAGE_CONTRACT_VERSION);
    let unknown = model.cost_unknown();
    // Unpriced cost is unknown, not zero, so its trend shows tokens.
    let tokens = !cost || unknown;
    let columns = {
        let maps: Vec<(&str, &indexmap::IndexMap<String, logic::Share>)> = if hourly { usage.hourly.iter().map(|entry| (entry.hour_start.as_str(), &entry.by_provider)).collect() } else { usage.daily.iter().map(|entry| (entry.day.as_str(), &entry.by_provider)).collect() };
        let by_period: HashMap<&str, &indexmap::IndexMap<String, logic::Share>> = maps.into_iter().collect();
        logic::build_period_columns(&periods, &by_period, tokens)
    };
    let provider: &'static str = logic::presentation(&model.provider).map_or("codex", |presentation| presentation.kind);
    let chart = logic::build_chart(&[provider], columns);
    let presentation = logic::presentation(&model.provider);
    let hit = logic::cache_hit_rate(&model);
    let per_million = logic::cost_per_million_tokens(&model);
    let mut stats = vec![("Cost", if unknown { "Unpriced".to_owned() } else { logic::format_usd(model.cost_usd) }), ("Tokens", logic::format_tokens(model.total_tokens))];
    if let Some(value) = per_million { stats.push(("Per 1M tokens", logic::format_usd(value))); }
    if let Some(value) = hit { stats.push(("Cache hit", logic::format_percent(value, 1))); }
    let tooltips = labels.clone();
    rsx! { div { class: "modal-backdrop", onclick: move |_| onclose.call(()),
        div { class: "dialog usage-model-dialog", role: "dialog", "aria-modal": "true", "aria-label": "{model.model}", tabindex: "-1", onclick: move |event| event.stop_propagation(),
            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); onclose.call(()); } },
            h2 { "{model.model}" }
            p { class: "muted", "{presentation.map_or(\"\", |presentation| presentation.label)}{if unknown { String::new() } else { format!(\" \u{b7} {} of cost\", logic::format_percent(model.cost_share, 1)) }}" }
            div { class: "usage-metrics", for (label, value) in stats { div { key: "{label}", class: "usage-metric", span { class: "muted", "{label}" } span { class: "usage-metric-value", "{value}" } } } }
            ProviderChart { chart, labels, tooltips, tokens, hourly, providers: vec![provider] }
            div { class: "usage-shares",
                if !unknown { ShareBar { label: "Cost by type", segments: logic::cost_type_segments(&usage.category_cost), tokens: false, aside: None } }
                ShareBar { label: "Tokens by type", segments: logic::token_type_segments(&model.tokens), tokens: true, aside: None }
                if usage.speed_cost.fast + usage.speed_cost.ultrafast > 0.0 { ShareBar { label: "Cost by speed", segments: logic::speed_cost_segments(&usage.speed_cost), tokens: false, aside: Some(usage.speed_cost.premium) } }
            }
            if model.unpriced_tokens > 0.0 { p { class: "muted", "{logic::format_tokens(model.unpriced_tokens)} tokens have no known price" } }
            div { class: "dialog-actions", button { r#type: "button", onclick: move |_| onclose.call(()), "Close" } }
        }
    } }
}
