//! Subscription limits as bars: one account's windows (the composer's `/usage-limits` panel),
//! the accounts pooled per provider (Usage, Limits), and redeeming a banked reset credit.
//! Port of `UsageLimits.tsx` and `UsageLimitsPooled.tsx`.
//!
//! Nothing here ticks: the page advances `now` when it is opened or refreshed, because a live
//! clock would repaint the page for no decision-changing gain. A reset credit is spent once per
//! confirmation; its control stays disabled from the click until the server has answered.
use crate::{
    provider_manage::{allowed_now, ask, follow_config},
    runtime::{TransportHandle, UiModel, UiModelStoreExt},
    settings_common::Confirm,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use t3_client::{
    connection::ConnectionStatus,
    usage::presentation,
    usage_limits::{self as logic, Account, Limits, Pool, PoolWindow, Presentation, ResetCreditInput, Window},
};
use t3_contracts::AuthEnvironmentScope;

pub(crate) const REDEEM_TITLE: &str = "Use a reset credit?";
pub(crate) const REDEEM_BODY: &str = "This redeems one credit on your account and clears the current rate-limit windows. It cannot be undone.";
pub(crate) const REDEEM_CONFIRM: &str = "Use credit";

/// Reset credits being spent, and what the server said of the last attempt for each account.
#[derive(Debug, Clone, Default)]
pub(crate) struct Redeem {
    /// The credit the confirmation dialog is asking about.
    pub confirming: Option<(String, ResetCreditInput)>,
    pub busy: BTreeSet<String>,
    pub status: BTreeMap<String, String>,
}

fn edit(state: Store<UiModel>, change: impl FnOnce(&mut Redeem)) {
    let mut redeem = state.redeem();
    change(&mut redeem.write());
}
pub(crate) fn key_of(input: &ResetCreditInput) -> String {
    serde_json::to_string(input).unwrap_or_default()
}
fn outcome_text(outcome: &str) -> &'static str {
    match outcome {
        "reset" => "Reset applied. Your windows have cleared.",
        "nothingToReset" => "Nothing to reset right now.",
        "noCredit" => "No reset credit left.",
        "alreadyRedeemed" => "That credit was already redeemed.",
        _ => "Could not use the reset credit.",
    }
}

/// Spends the credit the dialog asked about, once. The control is released only by the answer.
pub(crate) fn redeem(transport: &TransportHandle, state: Store<UiModel>) {
    let Some((key, input)) = state.redeem().peek().confirming.clone() else { return };
    state.redeem().write().confirming = None;
    if state.redeem().peek().busy.contains(&key) || !allowed_now(state, AuthEnvironmentScope::ProvidersManage) {
        return;
    }
    edit(state, |model| {
        model.busy.insert(key.clone());
        model.status.remove(&key);
    });
    let transport = transport.clone();
    spawn(async move {
        let answer = ask(&transport, state, "provider.consumeResetCredit", serde_json::to_value(&input).unwrap_or(Value::Null)).await;
        let status = match answer {
            Some(Ok(value)) => {
                follow_config(&transport, state).await;
                value["warning"].as_str().map(str::to_owned).unwrap_or_else(|| outcome_text(value["outcome"].as_str().unwrap_or("")).to_owned())
            }
            Some(Err(message)) if message == "Request was interrupted." => "The connection was lost before the server answered. Refresh the limits to see whether the credit was used.".to_owned(),
            Some(Err(message)) => message,
            None => "The connection changed before the server answered. Refresh the limits to see whether the credit was used.".to_owned(),
        };
        edit(state, |model| {
            model.busy.remove(&key);
            model.status.insert(key, status);
        });
    });
}

/// The confirmation, mounted outside any popover that holds the button.
#[component]
pub(crate) fn RedeemDialog(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let open = state.redeem().read().confirming.is_some();
    let allowed = *state.status().read() == ConnectionStatus::Connected;
    rsx! { if open {
        Confirm { label: "reset credit", title: REDEEM_TITLE, body: REDEEM_BODY, confirm: REDEEM_CONFIRM, busy: !allowed,
            oncancel: move |_| state.redeem().write().confirming = None,
            onconfirm: { let transport = transport.clone(); move |_| redeem(&transport, state) } }
    } }
}

fn ask_to_redeem(state: Store<UiModel>, input: &ResetCreditInput) {
    if allowed_now(state, AuthEnvironmentScope::ProvidersManage) {
        state.redeem().write().confirming = Some((key_of(input), input.clone()));
    }
}

/// `2 reset credits banked · next expires in 27d 23h`, or the short form for a popover.
pub(crate) fn credits_summary(credits: &logic::ResetCredits, now: f64, compact: bool) -> String {
    let expires_in = credits.next_expires_at.as_deref().and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok()).map(|at| logic::format_duration(at.timestamp_millis() as f64 - now));
    if credits.available_count == 0.0 {
        return "No reset credits banked".into();
    }
    let count = credits.available_count;
    if compact {
        return format!("{count} banked{}", expires_in.map(|text| format!(" \u{b7} expires in {text}")).unwrap_or_default());
    }
    format!("{count} {} banked{}", if count == 1.0 { "reset credit" } else { "reset credits" }, expires_in.map(|text| format!(" \u{b7} next expires in {text}")).unwrap_or_default())
}

pub(crate) fn bar_color(driver: &str) -> &'static str {
    match driver {
        "codex" => presentation("codex").map_or("var(--foreground)", |provider| provider.color),
        "claudeAgent" => presentation("claude").map_or("var(--foreground)", |provider| provider.color),
        _ => "var(--foreground)",
    }
}
pub(crate) fn pace_label(pace: &str) -> &'static str {
    match pace {
        "ahead" => "Ahead of pace: spending faster than the window elapses",
        "on" => "On pace with the window",
        _ => "Under pace: headroom left for the rest of the window",
    }
}
fn pace_glyph(pace: &str) -> &'static str {
    match pace {
        "ahead" => "\u{2197}",
        "on" => "\u{25ce}",
        _ => "\u{2198}",
    }
}
#[component]
pub(crate) fn Pace(pace: String) -> Element {
    let label = pace_label(&pace);
    rsx! { span { class: "limit-pace", role: "img", "aria-label": "{label}", title: "{label}", "{pace_glyph(&pace)}" } }
}

fn offset_minutes(now: f64) -> i32 {
    crate::scheduled_tasks_page::zone(now as i64)
}
fn reset_time(window: &Window, now: f64) -> Option<String> {
    let at = window.resets_at.as_deref()?;
    let moment = chrono::DateTime::from_timestamp_millis(now as i64)?;
    Some(t3_client::timeline_items::upcoming_timestamp(at, offset_minutes(now), moment))
}

/// One window as a bar from the moment it opened to its reset. The fill is the quota left; the
/// hairline is how far into the window the clock is, which is also where even spending would
/// have put the fill.
#[component]
fn WindowBar(color: String, window: Window, now: f64) -> Element {
    let remaining = logic::remaining_percent(&window);
    let time_left = logic::elapsed_share(&window, now).map(|elapsed| ((1.0 - elapsed) * 100.0 + 0.5).floor());
    let resets_in = logic::format_resets_in(&window, now);
    let resets_at = reset_time(&window, now);
    let summary = format!("{}: {remaining}% left{}{}", window.label, time_left.map(|left| format!(", {left}% of the window left")).unwrap_or_default(), resets_in.as_ref().map(|text| format!(", {text}")).unwrap_or_default());
    let mut tip = format!("{remaining}% left{}", time_left.map(|left| format!(" \u{b7} {left}% of the window left")).unwrap_or_default());
    if time_left.is_some() {
        tip += "\nThe line is where even spending would be.";
    }
    if let Some(at) = resets_at {
        tip += &format!("\nResets {at}{}", resets_in.map(|text| format!(" \u{b7} {text}")).unwrap_or_default());
    }
    rsx! { div { class: "limit-bar", role: "img", "aria-label": "{summary}", title: "{tip}", tabindex: "0",
        div { class: "limit-bar-track" }
        if remaining > 0.0 { div { class: "limit-bar-fill", style: "width:{remaining}%;background-color:{color}" } }
        if let Some(left) = time_left { span { class: "limit-bar-mark", "aria-hidden": "true", style: "left:{left}%" } }
    } }
}

/// One account's windows as rows: label and percent, bar, pace and countdown.
#[component]
pub(crate) fn LimitWindows(driver: String, windows: Vec<Window>, now: f64, compact: bool) -> Element {
    let color = bar_color(&driver);
    rsx! { div { class: if compact { "limit-windows compact" } else { "limit-windows" },
        for window in windows {
            div { key: "{window.id}", class: "limit-window-row",
                span { class: "limit-window-label", span { class: "limit-window-name", "{window.label}" } span { class: "limit-window-left", "{logic::remaining_percent(&window)}% left" } }
                WindowBar { color: color.to_owned(), window: window.clone(), now }
                span { class: "limit-window-reset",
                    if let Some(pace) = logic::pace_of(&window, now) { Pace { pace: pace.to_owned() } }
                    span { "{logic::format_resets_in(&window, now).unwrap_or_default()}" }
                }
            }
        }
    } }
}

/// Banked reset credits with the redeem button, for one account.
#[component]
pub(crate) fn ResetCredits(state: Store<UiModel>, input: ResetCreditInput, credits: logic::ResetCredits, now: f64) -> Element {
    let key = key_of(&input);
    let busy = state.redeem().read().busy.contains(&key);
    let status = state.redeem().read().status.get(&key).cloned();
    let allowed = *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|id| state.environments().read().allows(id, AuthEnvironmentScope::ProvidersManage));
    if credits.available_count == 0.0 && status.is_none() {
        return rsx! {};
    }
    rsx! { div { class: "limit-credits",
        span { "{credits_summary(&credits, now, false)}" }
        if credits.available_count > 0.0 {
            button { r#type: "button", "aria-label": "Use reset", disabled: busy || !allowed, "aria-busy": "{busy}", onclick: { let input = input.clone(); move |_| ask_to_redeem(state, &input) }, if busy { "Using\u{2026}" } else { "Use reset" } }
        }
        if let Some(status) = status { span { class: "limit-credit-status", role: "status", "{status}" } }
    } }
}

fn initials(email: &str) -> String {
    let (local, domain) = email.split_once('@').unwrap_or((email, ""));
    let text = format!("{}{}", local.chars().next().map(String::from).unwrap_or_default(), domain.chars().next().map(String::from).unwrap_or_default()).to_uppercase();
    if text.is_empty() { "?".into() } else { text }
}
fn hue(email: &str) -> i32 {
    let mut hash: i32 = 0;
    for unit in email.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(unit as i32);
    }
    (hash.unsigned_abs() % 360) as i32
}
fn driver_label(driver: &str) -> String {
    match driver {
        "codex" => "Codex",
        "claudeAgent" => "Claude",
        "cursor" => "Cursor",
        "opencode" => "OpenCode",
        "grok" => "Grok",
        "antigravity" => "Antigravity",
        other => other,
    }
    .to_owned()
}
/// Who an account is, without printing the email: the instance name when there is one, else a
/// two-letter chip. The address itself is revealed on demand.
#[component]
fn AccountName(account: Account) -> Element {
    if let Some(name) = &account.display_name {
        return rsx! { span { class: "limit-account-name", "{name}" } };
    }
    if let Some(email) = &account.email {
        let hue = hue(email);
        return rsx! { span { class: "limit-account-chip", role: "img", "aria-label": "Account {initials(email)}", style: "background-color:oklch(0.85 0.08 {hue});color:oklch(0.35 0.1 {hue})", "{initials(email)}" } };
    }
    rsx! { span { class: "limit-account-name", "{driver_label(&account.driver)}" } }
}

/// One account's share of one pooled window, and the details a click reveals.
#[component]
fn Segment(state: Store<UiModel>, account: Account, window: Window, reset: Option<logic::Reset>, color: String, now: f64, index: usize, show_name: bool, open: bool, ontoggle: EventHandler<()>) -> Element {
    let remaining = logic::remaining_percent(&window);
    let resets_in = logic::format_resets_in(&window, now);
    let credits = account.limits.reset_credits.as_ref().map_or(0.0, |credits| credits.available_count);
    let label = format!("{}: {remaining}% left{}{}", account.display_name.clone().or_else(|| account.email.as_deref().map(initials)).unwrap_or_else(|| account.driver.clone()), resets_in.as_ref().map(|text| format!(", {text}")).unwrap_or_default(), if credits > 0.0 { format!(", {credits} reset {} banked", if credits == 1.0 { "credit" } else { "credits" }) } else { String::new() });
    let short_reset = resets_in.as_ref().map(|text| text.replace("resets in ", "\u{21bb} ")).unwrap_or_default();
    let fill_opacity = 0.35;
    rsx! {
        button { r#type: "button", class: if open { "limit-segment open" } else { "limit-segment" }, style: "grid-column:{index}", "aria-label": "{label}", "aria-expanded": "{open}", onclick: move |_| ontoggle.call(()),
            div { class: "limit-segment-fill", "aria-hidden": "true", style: "width:{remaining}%;background-color:{color};opacity:{fill_opacity}" }
            if remaining < 100.0 && reset.is_some() { div { class: "limit-segment-spent", "aria-hidden": "true", style: "width:{100.0 - remaining}%;background-image:repeating-linear-gradient(135deg, {color} 0 1px, transparent 1px 5px)" } }
            span { class: "limit-segment-index", "aria-hidden": "true", "{index}" }
            span { class: "limit-segment-text",
                if show_name { AccountName { account: account.clone() } }
                span { class: "limit-segment-percent", "{remaining}%" }
                span { class: "limit-segment-reset", "{short_reset}" if credits > 0.0 { " \u{b7} \u{1f39f}{credits}" } }
            }
        }
    }
}

/// Everything about one account in one window: plan, where it is signed in, the email on
/// request, reset time and share of the pool it restores, and the reset-credit action.
#[component]
fn SegmentDetails(state: Store<UiModel>, account: Account, window: Window, reset: Option<logic::Reset>, now: f64) -> Element {
    let mut revealed = use_signal(|| false);
    let remaining = logic::remaining_percent(&window);
    let resets_in = logic::format_resets_in(&window, now);
    let where_ = if account.environments.is_empty() { account.source_label.clone() } else { Some(account.environments.iter().map(|environment| environment.label.clone()).collect::<Vec<_>>().join(", ")) };
    let credits = account.limits.reset_credits.clone().filter(|credits| credits.available_count > 0.0);
    let title = account.display_name.clone().unwrap_or_else(|| driver_label(&account.driver));
    let reset_line = reset_time(&window, now).map(|at| format!("{at}{}", resets_in.map(|text| format!(" \u{b7} {}", text.replace("resets in ", "in "))).unwrap_or_default()));
    rsx! { div { class: "limit-details", role: "group", "aria-label": "Details of {title}",
        div { class: "limit-details-head", AccountName { account: account.clone() } strong { "{title}" }
            if let Some(email) = account.email.clone() {
                button { r#type: "button", class: "limit-email", "aria-label": "Toggle account email visibility", "aria-pressed": "{revealed}", onclick: move |_| revealed.toggle(),
                    if *revealed.read() { "{email}" } else { "{mask(&email)}" } }
            }
        }
        if let Some(plan) = account.plan.clone() { div { class: "limit-detail-row", span { "Plan" } span { "{plan}" } } }
        if let Some(where_) = where_ { div { class: "limit-detail-row", span { if account.environments.is_empty() { "Via" } else { "Signed in" } } span { "{where_}" } } }
        div { class: "limit-detail-row", span { "Left" } span { "{remaining}%" } }
        if let Some(at) = reset_line { div { class: "limit-detail-row", span { "Resets" } span { "{at}" } } }
        if let Some(reset) = reset.filter(|reset| reset.restores_percent > 0.0) { div { class: "limit-detail-row", span { "Restores" } span { "+{reset.restores_percent}% of pool" } } }
        if let (Some(credits), Some(redeem)) = (credits, account.redeem.clone()) {
            ResetCredits { state, input: redeem.input, credits, now }
        }
    } }
}
/// An address with only its first letter showing, so a screen share does not print it.
fn mask(email: &str) -> String {
    let first: String = email.chars().take(1).collect();
    format!("{first}{}", "\u{2022}".repeat(email.chars().count().saturating_sub(1).min(12)))
}

/// One pooled window as equal-width segments, one per account, each filled by the share of that
/// account's quota still open. Equal widths are honest: every account contributes the same share
/// of the pool, whatever its plan.
#[component]
fn PoolWindowCard(state: Store<UiModel>, pool: PoolWindow, color: String, now: f64, label: Option<String>, description: Option<String>) -> Element {
    let mut open = use_signal(|| None::<usize>);
    let next_refill = pool.resets.iter().find(|reset| reset.restores_percent > 0.0).cloned();
    let restores: BTreeMap<String, logic::Reset> = pool.resets.iter().map(|reset| (reset.member.account.key.clone(), reset.clone())).collect();
    let count = pool.columns.len();
    let selected = (*open.read()).and_then(|index| pool.columns.get(index - 1).and_then(|column| column.window.clone().map(|window| (index, column.account.clone(), window))));
    rsx! { div { class: "limit-pool-card", onkeydown: move |event| { if event.key() == Key::Escape && open.peek().is_some() { event.stop_propagation(); open.set(None); } },
        div { class: "limit-pool-summary",
            span { class: "limit-pool-label", "{label.clone().unwrap_or_else(|| pool.label.clone())}" }
            span { class: "limit-pool-number", span { class: "limit-pool-percent", "{pool.remaining_percent}%" } span { "left" } if let Some(pace) = pool.pace { Pace { pace: pace.to_owned() } } }
            if let (Some(refill), true) = (next_refill, count > 1) { span { class: "limit-pool-refill", "\u{21bb} +{refill.restores_percent}%" } }
        }
        div { class: "limit-pool-bar", style: "grid-template-columns:repeat({count}, minmax(0, 1fr))",
            for (position, column) in pool.columns.iter().enumerate() {
                if let Some(window) = column.window.clone() {
                    Segment { key: "{column.account.key}", state, account: column.account.clone(), window, reset: restores.get(&column.account.key).cloned(), color: color.clone(), now, index: position + 1, show_name: count > 1, open: *open.read() == Some(position + 1),
                        ontoggle: move |_| { let next = if *open.peek() == Some(position + 1) { None } else { Some(position + 1) }; open.set(next); } }
                }
            }
        }
        if let Some((_, account, window)) = selected { SegmentDetails { state, account: account.clone(), window, reset: restores.get(&account.key).cloned(), now } }
        if let Some(description) = description { p { class: "limit-pool-description", "{description}" } }
    } }
}

#[component]
fn PoolSection(state: Store<UiModel>, pool: Pool, now: f64) -> Element {
    let color = bar_color(&pool.driver);
    let label = driver_label(&pool.driver);
    rsx! { section { class: "limit-pool",
        h2 { "{label}" }
        for window in logic::display_windows(&pool) {
            {
                let details = if pool.driver == "cursor" { logic::cursor_window_details(&window.id) } else { None };
                rsx! { PoolWindowCard { key: "{window.kind}:{window.id}", state, pool: window.clone(), color: color.to_owned(), now, label: details.map(|(label, _)| label.to_owned()), description: details.map(|(_, text)| text.to_owned()) } }
            }
        }
    } }
}

/// The accounts pooled per provider: what is open across all of them, who resets next, and how
/// much of the pool that hands back. Answers "can I keep going" before "on which account".
#[component]
pub(crate) fn Pooled(state: Store<UiModel>, presentations: Vec<Presentation>, now: f64) -> Element {
    let accounts = logic::collect_accounts(&presentations);
    let pools = logic::collect_pools(&accounts, now);
    let notices = logic::collect_notices(&presentations);
    let links = logic::collect_external_usage_links(&presentations);
    rsx! { div { class: "limit-pools",
        if pools.is_empty() && notices.is_empty() && links.is_empty() { p { class: "muted", "No provider on this environment reports subscription limits." } }
        for pool in pools { PoolSection { key: "{pool.driver}", state, pool: pool.clone(), now } }
        for link in links {
            section { key: "{link.url}", class: "limit-external",
                div { h2 { "{link.label}" }
                    if link.url == logic::CHATGPT_USAGE_URL { p { "View usage in ChatGPT with your connected account." } }
                    else if let Some(message) = link.message.clone() { p { "{message}" } }
                }
                a { class: "limit-external-link", href: "{link.url}", target: "_blank", rel: "noopener noreferrer", "Manage usage" }
            }
        }
        if !notices.is_empty() {
            div { class: "stale-banner limit-notices", role: "status", for notice in notices { p { key: "{notice}", "{notice}" } } }
        }
    } }
}

/// Providers and sources as the connected environment's configuration carries them.
pub(crate) fn presentation_of(state: Store<UiModel>) -> Option<Presentation> {
    let destination = state.destination().read().clone()?;
    let label = state.environments().read().records.get(&destination).map(|record| record.label.clone()).unwrap_or_else(|| destination.to_string());
    let config = state.typed_config();
    let config = config.read();
    let config = config.as_ref()?;
    let providers = logic::providers_from(&serde_json::to_value(&config.providers).unwrap_or(Value::Null));
    let sources = logic::sources_from(&serde_json::to_value(config.usage_limit_sources.clone().flatten()).unwrap_or(Value::Null));
    Some(Presentation { environment_id: destination.to_string(), label, providers, sources })
}

/// A provider's limits as the `/usage-limits` panel and the provider settings show them.
pub(crate) fn notice_of(limits: &Limits) -> Option<String> {
    logic::limits_notice(limits)
}

// ---- the composer's `/usage-limits` panel ------------------------------------------------

/// The `/usage-limits` result for the open thread: a point-in-time report that never refreshes
/// and closes when the thread or its provider changes.
#[derive(Debug, Clone)]
pub(crate) struct Panel {
    pub thread_id: String,
    pub instance_id: String,
    pub report: logic::Report,
}

/// Whether the thread's provider has anything to say about its limits (the command is offered
/// and accepted only then).
pub(crate) fn offers_command(state: Store<UiModel>, instance_id: &str) -> bool {
    let Some(presentation) = presentation_of(state) else { return false };
    presentation.providers.iter().find(|provider| provider.instance_id == instance_id).is_some_and(|provider| logic::has_provider_limits(&provider.driver, &presentation.providers, &presentation.sources))
}

/// Opens the panel for this thread's provider from the snapshots the page already holds.
pub(crate) fn open_panel(state: Store<UiModel>, thread_id: &str, instance_id: &str) {
    let Some(presentation) = presentation_of(state) else { return };
    let report = logic::collect_provider_report(instance_id, &presentation.providers, &presentation.sources, chrono::Utc::now().timestamp_millis() as f64);
    let mut panel = state.usage_panel();
    *panel.write() = report.map(|report| Panel { thread_id: thread_id.to_owned(), instance_id: instance_id.to_owned(), report });
}

fn account_label(account: &logic::ReportAccount) -> String {
    let Some(instance_id) = &account.instance_id else { return account.label.clone() };
    let driver = driver_label(&account.driver);
    let instance = account.display_name.as_deref().map(str::trim).filter(|name| !name.is_empty()).map(str::to_owned).unwrap_or_else(|| if *instance_id != account.driver { instance_id.clone() } else { String::new() });
    // The default instance is often named after its driver; saying it twice adds nothing.
    if !instance.is_empty() && instance.to_lowercase() != driver.to_lowercase() { format!("{driver} \u{b7} {instance}") } else { driver }
}

fn account_line(account: &logic::ReportAccount) -> String {
    format!("{}{}", account_label(account), account.plan.as_ref().map(|plan| format!(" \u{b7} {plan}")).unwrap_or_default())
}

#[component]
pub(crate) fn UsageLimitsPanel(state: Store<UiModel>, thread_id: String, instance_id: String) -> Element {
    let panel = state.usage_panel().read().clone().filter(|panel| panel.thread_id == thread_id && panel.instance_id == instance_id);
    let Some(panel) = panel else { return rsx! {} };
    let report = panel.report;
    let now = chrono::DateTime::parse_from_rfc3339(&report.created_at).map(|at| at.timestamp_millis() as f64).unwrap_or(0.0);
    let summary = match report.accounts.as_slice() {
        [only] => account_line(only),
        many => format!("{} accounts", many.len()),
    };
    let many = report.accounts.len() > 1;
    rsx! { section { class: "composer-panel usage-limits-panel", role: "status", "aria-label": "Usage limits",
        onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); state.usage_panel().set(None); } },
        div { class: "usage-limits-head", strong { "Usage limits" } span { class: "muted", "{summary}" }
            button { r#type: "button", "aria-label": "Dismiss usage limits", onclick: move |_| state.usage_panel().set(None), "\u{d7}" } }
        for account in report.accounts.iter() {
            div { key: "{account.id}", class: "usage-limits-account",
                if many { span { class: "muted", "{account_line(account)}" } }
                if let Some(notice) = logic::limits_notice(&account.limits) { span { class: "muted", "{notice}" } }
                else { LimitWindows { driver: account.driver.clone(), windows: account.limits.windows.clone(), now, compact: true } }
                if let Some(external) = account.limits.external_usage.clone() { a { class: "limit-external-link", href: "{external.url}", target: "_blank", rel: "noopener noreferrer", "Manage usage" } }
                if let (Some(input), Some(credits)) = (account.reset_credit_input.clone().or_else(|| account.instance_id.clone().map(|instance_id| ResetCreditInput::Instance { instance_id })), account.limits.reset_credits.clone()) {
                    ResetCredits { state, input, credits, now }
                }
            }
        }
        for notice in report.notices.iter() { span { key: "{notice}", class: "muted", "{notice}" } }
    } }
}

// ---- the usage-limit recovery banner ----------------------------------------------------

/// What the shell says of a limit recovery already scheduled for this thread.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecoveryState {
    pub run_id: String,
    pub reset_at: String,
    pub auto_resume: bool,
    pub snooze: bool,
}

fn parse_ms(text: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|at| at.timestamp_millis() as f64)
}
/// `Date.prototype.toLocaleString` in `en-US`: `10/9/2026, 11:51:00 AM`.
fn local_string(at: &str) -> String {
    use chrono::{Datelike, Duration, Timelike};
    let Some(ms) = parse_ms(at) else { return String::new() };
    let Some(moment) = chrono::DateTime::from_timestamp_millis(ms as i64) else { return String::new() };
    let local = moment + Duration::minutes(offset_minutes(ms) as i64);
    let hour = if local.hour() % 12 == 0 { 12 } else { local.hour() % 12 };
    format!("{}/{}/{}, {hour}:{:02}:{:02} {}", local.month(), local.day(), local.year(), local.minute(), local.second(), if local.hour() < 12 { "AM" } else { "PM" })
}

/// What the thread's shell contributes to the banner.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ShellRecovery {
    /// The latest run, when the shell itself says a usage limit stopped it.
    pub limited_run: Option<String>,
    pub reset_at: Option<String>,
    pub stopped_at: String,
    pub snoozed_until: Option<String>,
    pub recovery: Option<RecoveryState>,
}

/// Finds the run a limit stopped in the open thread's projection (a narrow memo, so streaming
/// text repaints nothing here) and shows the banner for it.
#[component]
pub(crate) fn RecoveryHost(state: Store<UiModel>, transport: TransportHandle, thread_id: String, shell: ShellRecovery) -> Element {
    let blocked = use_memo(use_reactive((&thread_id,), move |(thread_id,)| {
        let field = state.thread();
        let model = field.read();
        let projection = model.projection.as_ref()?;
        if projection["thread"]["id"] != thread_id.as_str() {
            return None;
        }
        let runs = projection["runs"].as_array()?;
        let items: Vec<Value> = projection["visibleTurnItems"].as_array().into_iter().flatten().filter(|row| row["item"]["type"] == "error").map(|row| row["item"].clone()).collect();
        logic::blocked_run(runs, &items)
    }));
    let found = blocked.read().clone();
    let (run_id, reset_at, stopped_at) = match (found, &shell.limited_run) {
        (Some(blocked), _) => (blocked.run_id, blocked.reset_at.or(shell.reset_at.clone()), blocked.stopped_at.unwrap_or(shell.stopped_at.clone())),
        (None, Some(run)) => (run.clone(), shell.reset_at.clone(), shell.stopped_at.clone()),
        (None, None) => return rsx! {},
    };
    rsx! { RecoveryBanner { state, transport, thread_id, run_id, reset_at, stopped_at, snoozed_until: shell.snoozed_until, recovery: shell.recovery } }
}

/// The banner over the composer of a thread that stopped on a usage limit. Resuming at the reset
/// and snoozing until it are the thread's own recovery, saved on the server.
#[component]
pub(crate) fn RecoveryBanner(state: Store<UiModel>, transport: TransportHandle, thread_id: String, run_id: String, reset_at: Option<String>, stopped_at: String, snoozed_until: Option<String>, recovery: Option<RecoveryState>) -> Element {
    let can_schedule = match (&reset_at, parse_ms(&stopped_at)) {
        (Some(reset), Some(stopped)) => parse_ms(reset).is_some_and(|reset| reset > stopped),
        _ => false,
    };
    let mut pending = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    // One timer, set for the reset, turns "Snooze until reset" off once the reset has passed.
    let mut tick = use_signal(|| 0u64);
    use_resource(use_reactive((&reset_at,), move |(reset_at,)| async move {
        let _ = tick();
        let Some(reset) = reset_at.as_deref().and_then(parse_ms) else { return };
        let wait = reset - chrono::Utc::now().timestamp_millis() as f64;
        if wait <= 0.0 {
            return;
        }
        crate::runtime::delay((wait as u64 + 1).min(86_400_000)).await;
        let next = *tick.peek() + 1;
        tick.set(next);
    }));
    let _ = tick();
    let now = chrono::Utc::now().timestamp_millis() as f64;
    let scheduled = recovery.as_ref().is_some_and(|recovery| recovery.run_id == run_id && Some(&recovery.reset_at) == reset_at.as_ref() && recovery.auto_resume);
    let snoozed = recovery.as_ref().is_some_and(|recovery| recovery.snooze && recovery.run_id == run_id && Some(&recovery.reset_at) == reset_at.as_ref()) && matches!((&snoozed_until, &reset_at), (Some(until), Some(reset)) if parse_ms(until) == parse_ms(reset));
    let description = match &reset_at {
        Some(reset) => format!("Resets {}", local_string(reset)),
        None => "Reset time unavailable; retry manually".to_owned(),
    };
    let toggle = {
        let (transport, thread_id, run_id, reset_at) = (transport.clone(), thread_id.clone(), run_id.clone(), reset_at.clone());
        move |snooze: bool| {
            let Some(reset) = reset_at.clone() else { return };
            if *pending.peek() {
                return;
            }
            if snooze && !snoozed && parse_ms(&reset).is_some_and(|at| at <= chrono::Utc::now().timestamp_millis() as f64) {
                failure.set(Some("The reset time has passed. Retry the thread manually.".into()));
                let next = *tick.peek() + 1;
                tick.set(next);
                return;
            }
            pending.set(true);
            failure.set(None);
            let update = if snooze { json!({"runId":run_id,"resetAt":reset,"snooze":!snoozed}) } else { json!({"runId":run_id,"resetAt":reset,"autoResume":!scheduled}) };
            let payload = json!({"type":"thread.metadata.update","commandId":uuid::Uuid::new_v4().to_string(),"threadId":thread_id,"limitRecovery":update});
            let transport = transport.clone();
            spawn(async move {
                let answer = ask(&transport, state, "orchestration.dispatchCommand", payload).await;
                pending.set(false);
                match answer {
                    Some(Err(message)) => failure.set(Some(if message.is_empty() { "Could not change limit recovery.".into() } else { message })),
                    _ => {}
                }
            });
        }
    };
    let writable = *state.status().read() == ConnectionStatus::Connected;
    let past = reset_at.as_deref().and_then(parse_ms).is_some_and(|reset| reset <= now);
    rsx! { section { class: "composer-panel limit-recovery", role: "status", "aria-label": "Usage limit reached",
        div { class: "usage-limits-head", strong { "Usage limit reached" } span { class: "muted", "{description}" } }
        if can_schedule {
            div { class: "limit-recovery-actions",
                button { r#type: "button", "aria-label": if scheduled { "Cancel auto-resume" } else { "Resume at reset" }, disabled: *pending.read() || !writable, onclick: { let mut toggle = toggle.clone(); move |_| toggle(false) },
                    if *pending.read() { "Saving..." } else if scheduled { "Cancel auto-resume" } else { "Resume at reset" } }
                if !snoozed {
                    button { r#type: "button", "aria-label": "Snooze until reset", disabled: *pending.read() || !writable || past, onclick: { let mut toggle = toggle.clone(); move |_| toggle(true) },
                        if *pending.read() { "Saving..." } else { "Snooze until reset" } }
                }
                if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
            }
        }
    } }
}
