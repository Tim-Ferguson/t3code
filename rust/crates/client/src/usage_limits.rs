//! Which providers show a subscription limit, how their accounts pool and how a window is
//! worded: port of `packages/shared/src/usageLimits.ts`, replayed against it by
//! `tests/usage_parity.rs`. The reads are the provider snapshots' `usageLimits` and the
//! configuration's `usageLimitSources`; nothing here refreshes or guesses which pooled account
//! serves a turn.
use chrono::DateTime;
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::Value;

const MINUTE: f64 = 60_000.0;
const HOUR: f64 = 60.0 * MINUTE;
const DAY: f64 = 24.0 * HOUR;

pub const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/#settings/Usage";
pub const USAGE_LIMITS_COMMAND_NAME: &str = "usage-limits";
pub const USAGE_LIMITS_COMMAND_DESCRIPTION: &str = "Show this provider's usage limits";

fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned>(deserializer: D) -> Result<Vec<T>, D::Error> {
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values.into_iter().filter_map(|value| serde_json::from_value(value).ok()).collect())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_duration_mins: Option<f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCredits {
    pub available_count: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_credit_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct External {
    pub label: String,
    pub url: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unavailable {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub checked_at: String,
    #[serde(deserialize_with = "lenient")]
    pub windows: Vec<Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<ResetCredits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_usage: Option<External>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<Unavailable>,
}
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Auth {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub subscription_sharing: Option<bool>,
}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub instance_id: String,
    pub driver: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub accent_color: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub availability: Option<String>,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default)]
    pub usage_limits: Option<Limits>,
}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceAccount {
    pub id: String,
    pub driver: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    pub usage_limits: Limits,
}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub id: String,
    pub label: String,
    #[serde(default, deserialize_with = "lenient")]
    pub accounts: Vec<SourceAccount>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Providers read as the snapshots carry them. Entries that do not decode are skipped.
pub fn providers_from(value: &Value) -> Vec<Provider> {
    value.as_array().map(|items| items.iter().filter_map(|item| serde_json::from_value(item.clone()).ok()).collect()).unwrap_or_default()
}
pub fn sources_from(value: &Value) -> Vec<Source> {
    value.as_array().map(|items| items.iter().filter_map(|item| serde_json::from_value(item.clone()).ok()).collect()).unwrap_or_default()
}

pub fn uses_chatgpt_sharing(provider: &Provider) -> bool {
    provider.auth.status == "authenticated" && provider.auth.subscription_sharing == Some(true)
}

/// Providers that belong on the Limits view: enabled, installed, available, and one whose
/// driver reports subscription usage at all.
pub fn providers_with_limits(providers: &[Provider]) -> Vec<&Provider> {
    providers.iter().filter(|provider| provider.enabled && provider.installed && provider.availability.as_deref() != Some("unavailable") && provider.usage_limits.is_some()).collect()
}

/// The destination a provider's usage can be managed at, once per URL.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExternalLink {
    pub label: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub accounts: Vec<String>,
}

/// One environment as the pooled views read it.
#[derive(Debug, Clone, PartialEq)]
pub struct Presentation {
    pub environment_id: String,
    pub label: String,
    pub providers: Vec<Provider>,
    pub sources: Vec<Source>,
}

pub fn collect_external_usage_links(presentations: &[Presentation]) -> Vec<ExternalLink> {
    let mut links: IndexMap<String, ExternalLink> = IndexMap::new();
    for presentation in presentations {
        for provider in providers_with_limits(&presentation.providers) {
            let Some(limits) = &provider.usage_limits else { continue };
            if let (Some(external), true) = (&limits.external_usage, provider.auth.status == "authenticated") {
                let account = format!("{} on {}", provider.display_name.as_deref().unwrap_or(&provider.instance_id), presentation.label);
                let mut accounts = links.get(&external.url).map(|link| link.accounts.clone()).unwrap_or_default();
                if !accounts.contains(&account) {
                    accounts.push(account);
                }
                links.insert(external.url.clone(), ExternalLink { label: external.label.clone(), url: external.url.clone(), message: limits.unavailable.as_ref().and_then(|unavailable| unavailable.message.clone()), accounts });
            }
        }
    }
    links.into_values().collect()
}

/// Prefer the reported email; use an identical credential when no email is available.
fn account_key(driver: &str, email: Option<&str>, limits: Option<&Limits>) -> Option<String> {
    let normalized = email.map(|email| email.trim().to_lowercase()).filter(|email| !email.is_empty());
    if let Some(email) = normalized {
        return Some(format!("{driver}:{email}"));
    }
    limits.and_then(|limits| limits.credential_fingerprint.as_ref()).filter(|fingerprint| !fingerprint.is_empty()).map(|fingerprint| format!("{driver}:credential:{fingerprint}"))
}

/// Where a reset credit is redeemed: a provider instance, or an account of a usage-limit source.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ResetCreditInput {
    Instance {
        #[serde(rename = "instanceId")]
        instance_id: String,
    },
    Source {
        #[serde(rename = "sourceId")]
        source_id: String,
        #[serde(rename = "accountId")]
        account_id: String,
        #[serde(rename = "creditId")]
        credit_id: String,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Redeem {
    pub environment_id: String,
    pub input: ResetCreditInput,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentRef {
    pub environment_id: String,
    pub label: String,
}

/// One subscription account as the pooled views see it. Matching emails or credentials across
/// environments name one account; its quota is one bucket, so counting it twice would
/// misstate what is left.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub key: String,
    pub driver: String,
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
    pub environments: Vec<EnvironmentRef>,
    pub source_label: Option<String>,
    pub redeem: Option<Redeem>,
    pub limits: Limits,
}

fn time(text: &str) -> f64 {
    DateTime::parse_from_rfc3339(text).map(|at| at.timestamp_millis() as f64).unwrap_or(f64::NAN)
}
fn greater(a: &str, b: &str) -> bool {
    time(a) > time(b)
}

/// Every account with usable windows across the connected environments, one entry per
/// distinct account. The freshest reads supply windows and credits; native instances supply
/// names and environment labels.
pub fn collect_accounts(presentations: &[Presentation]) -> Vec<Account> {
    let mut accounts: IndexMap<String, Account> = IndexMap::new();
    let mut credit_sources: IndexMap<String, Account> = IndexMap::new();
    let mut hub_redeems: IndexMap<String, Account> = IndexMap::new();
    let mut merge = |key: String, next: Account| {
        // Redeeming through a hub also clears the routing cooldown the hub holds for the account,
        // so a hub target wins the redemption while the balance still follows the freshest read.
        let previous_hub = hub_redeems.get(&key);
        if matches!(next.redeem, Some(Redeem { input: ResetCreditInput::Source { .. }, .. })) && previous_hub.is_none_or(|hub| greater(&next.limits.checked_at, &hub.limits.checked_at)) {
            hub_redeems.insert(key.clone(), next.clone());
        }
        let previous_credit = credit_sources.get(&key);
        if next.limits.reset_credits.is_some() && previous_credit.is_none_or(|credit| greater(&next.limits.checked_at, &credit.limits.checked_at)) {
            credit_sources.insert(key.clone(), next.clone());
        }
        let Some(previous) = accounts.get(&key).cloned() else {
            accounts.insert(key, next);
            return;
        };
        let fresher = greater(&next.limits.checked_at, &previous.limits.checked_at);
        let mut environments = previous.environments.clone();
        environments.extend(next.environments.iter().filter(|candidate| !previous.environments.iter().any(|seen| seen.environment_id == candidate.environment_id)).cloned());
        let winner = if fresher { &next } else { &previous };
        let credit_source = credit_sources.get(&key);
        let mut limits = winner.limits.clone();
        limits.reset_credits = credit_source.and_then(|source| source.limits.reset_credits.clone());
        let redeem = if let Some(hub) = hub_redeems.get(&key).and_then(|hub| hub.redeem.clone()) {
            Some(hub)
        } else if let Some(source) = credit_source {
            source.redeem.clone()
        } else {
            winner.redeem.clone().or_else(|| previous.redeem.clone()).or_else(|| next.redeem.clone())
        };
        accounts.insert(
            key,
            Account {
                display_name: previous.display_name.clone().or_else(|| next.display_name.clone()),
                plan: previous.plan.clone().or_else(|| next.plan.clone()),
                accent_color: previous.accent_color.clone().or_else(|| next.accent_color.clone()),
                source_label: if environments.is_empty() { previous.source_label.clone().or_else(|| next.source_label.clone()) } else { None },
                environments,
                redeem,
                limits,
                ..previous
            },
        );
    };
    for presentation in presentations {
        for provider in providers_with_limits(&presentation.providers) {
            let Some(limits) = &provider.usage_limits else { continue };
            if limits_notice(limits).is_some() {
                continue;
            }
            let key = account_key(&provider.driver, provider.auth.email.as_deref(), Some(limits)).unwrap_or_else(|| format!("{}:{}", presentation.environment_id, provider.instance_id));
            merge(
                key,
                Account {
                    key: format!("{}:{}", presentation.environment_id, provider.instance_id),
                    driver: provider.driver.clone(),
                    display_name: provider.display_name.as_deref().map(str::trim).filter(|name| !name.is_empty()).map(str::to_owned),
                    email: provider.auth.email.clone(),
                    plan: provider.auth.label.clone(),
                    accent_color: provider.accent_color.clone(),
                    environments: vec![EnvironmentRef { environment_id: presentation.environment_id.clone(), label: presentation.label.clone() }],
                    source_label: None,
                    redeem: Some(Redeem { environment_id: presentation.environment_id.clone(), input: ResetCreditInput::Instance { instance_id: provider.instance_id.clone() } }),
                    limits: limits.clone(),
                },
            );
        }
    }
    // Every hub account, including those a native instance also knows: the hub may hold a
    // fresher read of the same subscription.
    let label_environment = presentations.len() > 1;
    for presentation in presentations {
        for source in &presentation.sources {
            let source_label = if label_environment { format!("{} · {}", presentation.label, source.label) } else { source.label.clone() };
            for account in &source.accounts {
                if limits_notice(&account.usage_limits).is_some() {
                    continue;
                }
                let key = account_key(&account.driver, account.email.as_deref(), Some(&account.usage_limits)).unwrap_or_else(|| format!("{}:{}", source.id, account.id));
                merge(
                    key,
                    Account {
                        key: format!("{}:{}", source.id, account.id),
                        driver: account.driver.clone(),
                        display_name: if account.email.is_some() { None } else { Some(strip_json_extension(&account.id)) },
                        email: account.email.clone(),
                        plan: account.plan.clone(),
                        accent_color: None,
                        environments: Vec::new(),
                        source_label: Some(source_label.clone()),
                        redeem: account.usage_limits.reset_credits.as_ref().and_then(|credits| credits.next_credit_id.clone()).filter(|id| !id.is_empty()).map(|credit_id| Redeem {
                            environment_id: presentation.environment_id.clone(),
                            input: ResetCreditInput::Source { source_id: source.id.clone(), account_id: account.id.clone(), credit_id },
                        }),
                        limits: account.usage_limits.clone(),
                    },
                );
            }
        }
    }
    accounts.into_values().collect()
}
fn strip_json_extension(id: &str) -> String {
    if id.len() >= 5 && id[id.len() - 5..].eq_ignore_ascii_case(".json") { id[..id.len() - 5].to_owned() } else { id.to_owned() }
}

/// What the pooled views cannot draw as a bar: a source that failed to read, a provider whose
/// probe failed. Accounts that can never report (API keys) are left out.
pub fn collect_notices(presentations: &[Presentation]) -> Vec<String> {
    let label = |environment: &str, subject: &str| if presentations.len() > 1 { format!("{environment} · {subject}") } else { subject.to_owned() };
    let mut notices = Vec::new();
    for presentation in presentations {
        for provider in providers_with_limits(&presentation.providers) {
            let Some(limits) = &provider.usage_limits else { continue };
            if limits.unavailable.as_ref().is_some_and(|unavailable| unavailable.reason == "unsupported") {
                continue;
            }
            if let Some(notice) = limits_notice(limits) {
                let name = provider.display_name.as_deref().map(str::trim).filter(|name| !name.is_empty()).unwrap_or(&provider.driver);
                notices.push(format!("{}: {}", label(&presentation.label, name), notice));
            }
        }
        for source in &presentation.sources {
            if let Some(error) = &source.error {
                notices.push(format!("{}: {}", label(&presentation.label, &source.label), error));
            } else if source.accounts.is_empty() {
                notices.push(format!("{}: No accounts reported.", label(&presentation.label, &source.label)));
            }
        }
    }
    notices
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Member {
    pub account: Account,
    pub window: Window,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Column {
    pub account: Account,
    pub window: Option<Window>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reset {
    pub member: Member,
    pub at: f64,
    pub restores_percent: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolWindow {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub members: Vec<Member>,
    pub columns: Vec<Column>,
    pub remaining_percent: f64,
    pub used_percent: f64,
    pub pace: Option<&'static str>,
    pub resets: Vec<Reset>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pool {
    pub driver: String,
    pub accounts: Vec<Account>,
    pub windows: Vec<PoolWindow>,
}

pub const CURSOR_WINDOWS: [(&str, &str, &str); 3] = [
    ("totalPercentUsed", "Overall", "Combined usage across both allowances, not a third quota."),
    ("autoPercentUsed", "Cursor Models", "Grok and Composer use this first. Auto can use either pool."),
    ("apiPercentUsed", "Other Models", "Claude, GPT, and Gemini use this pool. Grok and Composer fall back here."),
];
pub fn cursor_window_details(id: &str) -> Option<(&'static str, &'static str)> {
    CURSOR_WINDOWS.iter().find(|window| window.0 == id).map(|window| (window.1, window.2))
}
fn cursor_rank(id: &str) -> usize {
    CURSOR_WINDOWS.iter().position(|window| window.0 == id).unwrap_or(CURSOR_WINDOWS.len())
}
/// Cursor's two usable pools instead of a combined percentage, when both are available.
pub fn display_windows(pool: &Pool) -> Vec<&PoolWindow> {
    if pool.driver != "cursor" {
        return pool.windows.iter().collect();
    }
    let has = |id: &str| pool.windows.iter().any(|window| window.id == id);
    let both = has("autoPercentUsed") && has("apiPercentUsed");
    let mut windows: Vec<&PoolWindow> = pool.windows.iter().filter(|window| !both || window.id != "totalPercentUsed").collect();
    windows.sort_by_key(|window| cursor_rank(&window.id));
    windows
}

fn kind_order(kind: &str) -> u8 {
    match kind {
        "session" => 0,
        "weekly" => 1,
        "monthly" => 2,
        _ => 3,
    }
}
fn sort_name(account: &Account) -> String {
    account.display_name.as_deref().or(account.email.as_deref()).unwrap_or(&account.key).to_lowercase()
}

/// Accounts grouped by driver, each with its windows pooled by kind and id. A window id is
/// stable per provider; the kind is part of the key because Codex's `primary` is a position, not
/// a duration. Accounts share the session reset order, soonest first; a missing reset sorts last.
pub fn collect_pools(accounts: &[Account], now: f64) -> Vec<Pool> {
    let mut by_driver: IndexMap<String, Vec<Account>> = IndexMap::new();
    for account in accounts {
        by_driver.entry(account.driver.clone()).or_default().push(account.clone());
    }
    by_driver
        .into_iter()
        .map(|(driver, members)| {
            let order_window = members.iter().flat_map(|account| account.limits.windows.iter()).min_by_key(|window| kind_order(&window.kind));
            let order_reset = |account: &Account| {
                order_window
                    .and_then(|order| account.limits.windows.iter().find(|window| window.kind == order.kind && window.id == order.id))
                    .and_then(reset_millis)
                    .unwrap_or(f64::INFINITY)
            };
            let mut sorted = members.clone();
            sorted.sort_by(|left, right| {
                order_reset(left)
                    .partial_cmp(&order_reset(right))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| sort_name(left).cmp(&sort_name(right)))
                    .then_with(|| left.key.cmp(&right.key))
            });
            let windows = pool_windows(&sorted, now);
            Pool { driver, accounts: sorted, windows }
        })
        .collect()
}
fn pool_windows(accounts: &[Account], now: f64) -> Vec<PoolWindow> {
    let mut by_key: IndexMap<String, Vec<Member>> = IndexMap::new();
    for account in accounts {
        for window in &account.limits.windows {
            by_key.entry(format!("{}:{}", window.kind, window.id)).or_default().push(Member { account: account.clone(), window: window.clone() });
        }
    }
    let mut pools: Vec<PoolWindow> = by_key
        .into_values()
        .map(|members| {
            let first = members[0].window.clone();
            let count = members.len() as f64;
            let used = members.iter().fold(0.0, |sum, member| sum + member.window.used_percent) / count;
            // Pace is judged only over the members that have a clock.
            let timed: Vec<(f64, f64)> = members.iter().filter_map(|member| elapsed_share(&member.window, now).map(|share| (member.window.used_percent, share))).collect();
            let timed_used = timed.iter().fold(0.0, |sum, entry| sum + entry.0) / timed.len() as f64;
            let mean_elapsed = if timed.is_empty() { None } else { Some(timed.iter().fold(0.0, |sum, entry| sum + entry.1) / timed.len() as f64) };
            let mut resets: Vec<Reset> = members
                .iter()
                .filter_map(|member| reset_millis(&member.window).map(|at| Reset { member: member.clone(), at, restores_percent: round(member.window.used_percent / count) }))
                .collect();
            resets.sort_by(|left, right| left.at.partial_cmp(&right.at).unwrap_or(std::cmp::Ordering::Equal));
            let columns = accounts
                .iter()
                .map(|account| match members.iter().find(|member| member.account.key == account.key) {
                    Some(member) => Column { account: member.account.clone(), window: Some(member.window.clone()) },
                    None => Column { account: account.clone(), window: None },
                })
                .collect();
            PoolWindow {
                id: first.id,
                kind: first.kind,
                label: first.label,
                members,
                columns,
                used_percent: round(used),
                remaining_percent: round(100.0 - used),
                pace: mean_elapsed.map(|elapsed| pace_of_shares(timed_used, elapsed)),
                resets,
            }
        })
        .collect();
    pools.sort_by_key(|pool| kind_order(&pool.kind));
    pools
}
fn round(value: f64) -> f64 {
    (value + 0.5).floor()
}

/// The one-line status under a provider heading when there are no bars to draw.
pub fn limits_notice(limits: &Limits) -> Option<String> {
    match limits.unavailable.as_ref().map(|unavailable| (unavailable.reason.as_str(), unavailable.message.as_ref())) {
        Some(("unsupported", message)) => Some(message.cloned().unwrap_or_else(|| "This account has no subscription limits.".to_owned())),
        Some(("probeFailed", message)) => Some(message.cloned().unwrap_or_else(|| "Could not read limits.".to_owned())),
        _ if limits.windows.is_empty() => Some("No limits reported.".to_owned()),
        _ => None,
    }
}
/// Quota left in the window, 0..100. Bars and labels show what remains, as Codex does.
pub fn remaining_percent(window: &Window) -> f64 {
    round(100.0 - window.used_percent.clamp(0.0, 100.0))
}
fn reset_millis(window: &Window) -> Option<f64> {
    window.resets_at.as_deref().map(time).filter(|at| at.is_finite())
}
/// Elapsed share of the window, 0..1, or `None` when its length or reset is unknown.
pub fn elapsed_share(window: &Window, now: f64) -> Option<f64> {
    let resets_at = reset_millis(window)?;
    let length = window.window_duration_mins? * MINUTE;
    if length <= 0.0 {
        return None;
    }
    Some(((length - (resets_at - now)) / length).clamp(0.0, 1.0))
}
/// Usage against the clock. Spending evenly leaves the same share of quota as there is time
/// left in the window; within five points of that counts as on pace.
pub fn pace_of(window: &Window, now: f64) -> Option<&'static str> {
    elapsed_share(window, now).map(|elapsed| pace_of_shares(window.used_percent, elapsed))
}
fn pace_of_shares(used_percent: f64, elapsed: f64) -> &'static str {
    let gap = used_percent - elapsed * 100.0;
    if gap > 5.0 { "ahead" } else if gap < -5.0 { "under" } else { "on" }
}
/// `2h 13m`, `3d 4h`, `12m`.
pub fn format_duration(ms: f64) -> String {
    let remaining = ms.max(0.0);
    let days = (remaining / DAY).floor();
    let hours = ((remaining % DAY) / HOUR).floor();
    let minutes = ((remaining % HOUR) / MINUTE).floor();
    if days > 0.0 {
        format!("{days}d {hours}h")
    } else if hours > 0.0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}
/// `resets in 2h 13m`, or `None` when the window has no reset.
pub fn format_resets_in(window: &Window, now: f64) -> Option<String> {
    let at = reset_millis(window)?;
    Some(if at <= now { "resets now".to_owned() } else { format!("resets in {}", format_duration(at - now)) })
}

/// Handled by the client without sending a turn; anything with arguments stays an ordinary prompt.
pub fn is_usage_limits_command(prompt: &str) -> bool {
    prompt.trim().to_lowercase() == "/usage-limits"
}
/// Whether Limits has anything to say about this driver. A source that failed to read keeps no
/// accounts, so its error counts for every driver.
pub fn has_provider_limits(driver: &str, providers: &[Provider], sources: &[Source]) -> bool {
    providers_with_limits(providers).iter().any(|provider| provider.driver == driver)
        || sources.iter().any(|source| source.accounts.iter().any(|account| account.driver == driver) || (source.error.is_some() && source.accounts.is_empty()))
}
/// The drivers two sets of sources would offer the command to, a failed source counting for all.
pub fn same_command_coverage(previous: &[Source], next: &[Source]) -> bool {
    let coverage = |sources: &[Source]| -> std::collections::BTreeSet<String> {
        sources.iter().flat_map(|source| if source.error.is_some() && source.accounts.is_empty() { vec!["*".to_owned()] } else { source.accounts.iter().map(|account| account.driver.clone()).collect() }).collect()
    };
    coverage(previous) == coverage(next)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportAccount {
    pub id: String,
    pub driver: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_credit_input: Option<ResetCreditInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_label: Option<String>,
    pub limits: Limits,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub created_at: String,
    pub accounts: Vec<ReportAccount>,
    pub notices: Vec<String>,
}

/// A point-in-time report of one provider's accounts for the `/usage-limits` panel; it never
/// refreshes. Which balance is displayed follows the fresher snapshot; which path a credit is
/// redeemed through always prefers the hub that holds the account's routing cooldown.
pub fn collect_provider_report(instance_id: &str, providers: &[Provider], sources: &[Source], now: f64) -> Option<Report> {
    let selected = providers.iter().find(|provider| provider.instance_id == instance_id)?;
    if !has_provider_limits(&selected.driver, providers, sources) {
        return None;
    }
    let native: Vec<&Provider> = providers_with_limits(providers).into_iter().filter(|provider| provider.driver == selected.driver).collect();
    let native_accounts: Vec<String> = native
        .iter()
        .filter_map(|provider| {
            let key = account_key(&provider.driver, provider.auth.email.as_deref(), provider.usage_limits.as_ref())?;
            let limits = provider.usage_limits.as_ref()?;
            if !limits.windows.is_empty() && limits.unavailable.is_none() { Some(key) } else { None }
        })
        .collect();
    let mut accounts = Vec::new();
    let mut notices = Vec::new();
    for provider in &native {
        let Some(limits) = &provider.usage_limits else { continue };
        let key = account_key(&provider.driver, provider.auth.email.as_deref(), Some(limits));
        let hub = sources
            .iter()
            .flat_map(|source| source.accounts.iter().map(move |account| (source, account)))
            .filter(|(_, account)| {
                key.is_some() && account_key(&account.driver, account.email.as_deref(), Some(&account.usage_limits)) == key && account.usage_limits.reset_credits.is_some() && limits_notice(&account.usage_limits).is_none()
            })
            .max_by(|a, b| time(&a.1.usage_limits.checked_at).partial_cmp(&time(&b.1.usage_limits.checked_at)).unwrap_or(std::cmp::Ordering::Equal));
        let hub_credit_id = hub.and_then(|(_, account)| account.usage_limits.reset_credits.as_ref()).and_then(|credits| credits.next_credit_id.clone()).filter(|id| !id.is_empty());
        let show_hub_credits = hub.is_some_and(|(_, account)| limits.reset_credits.is_none() || greater(&account.usage_limits.checked_at, &limits.checked_at));
        let display_name = provider.display_name.as_deref().map(str::trim).filter(|name| !name.is_empty());
        let mut shown = limits.clone();
        if show_hub_credits {
            shown.reset_credits = hub.and_then(|(_, account)| account.usage_limits.reset_credits.clone());
        }
        accounts.push(ReportAccount {
            id: provider.instance_id.clone(),
            driver: provider.driver.clone(),
            label: format!("{} [{}]", display_name.unwrap_or(&provider.driver), provider.instance_id),
            plan: provider.auth.label.clone().filter(|label| !label.is_empty()),
            instance_id: Some(provider.instance_id.clone()),
            reset_credit_input: Some(match (hub_credit_id, hub) {
                (Some(credit_id), Some((source, account))) => ResetCreditInput::Source { source_id: source.id.clone(), account_id: account.id.clone(), credit_id },
                _ => ResetCreditInput::Instance { instance_id: provider.instance_id.clone() },
            }),
            display_name: provider.display_name.clone().filter(|name| !name.is_empty()),
            accent_color: provider.accent_color.clone().filter(|color| !color.is_empty()),
            email: provider.auth.email.clone().filter(|email| !email.is_empty()),
            source_label: None,
            limits: shown,
        });
    }
    for source in sources {
        let matching: Vec<&SourceAccount> = source.accounts.iter().filter(|account| account.driver == selected.driver).collect();
        for account in &matching {
            let key = account_key(&account.driver, account.email.as_deref(), Some(&account.usage_limits));
            if key.as_ref().is_some_and(|key| native_accounts.contains(key)) {
                continue;
            }
            accounts.push(ReportAccount {
                id: format!("{}:{}", source.id, account.id),
                driver: account.driver.clone(),
                label: format!("{} · {}", source.label, account.id),
                plan: account.plan.clone().filter(|plan| !plan.is_empty()),
                instance_id: None,
                reset_credit_input: account
                    .usage_limits
                    .reset_credits
                    .as_ref()
                    .and_then(|credits| credits.next_credit_id.clone())
                    .filter(|id| !id.is_empty())
                    .map(|credit_id| ResetCreditInput::Source { source_id: source.id.clone(), account_id: account.id.clone(), credit_id }),
                display_name: None,
                accent_color: None,
                email: account.email.clone().filter(|email| !email.is_empty()),
                source_label: Some("CLI Proxy".to_owned()),
                limits: account.usage_limits.clone(),
            });
        }
        // A source that failed to read has no accounts to match on, so its error goes to every provider.
        if let Some(error) = &source.error {
            if !matching.is_empty() || source.accounts.is_empty() {
                notices.push(format!("{}: {}", source.label, error));
            }
        }
    }
    let created_at = DateTime::from_timestamp_millis(now as i64).map(|at| at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()).unwrap_or_default();
    Some(Report { created_at, accounts, notices })
}

// ---- the run a usage limit stopped -----------------------------------------------------------

/// The latest run that actually started, when it stopped because the subscription limit was
/// reached: its id, the reset time the provider reported and when it stopped. Port of
/// `usageLimitBlockedRun` in `orchestrationV2ThreadError.ts`; a distinct session error is not
/// known to a client and is taken as absent.
#[derive(Debug, Clone, PartialEq)]
pub struct Blocked {
    pub run_id: String,
    pub reset_at: Option<String>,
    pub stopped_at: Option<String>,
    /// Whether a newer run was queued or cancelled before it started, so the limit is not the latest run.
    pub presented_as_latest: bool,
}

fn run_end(run: &Value) -> f64 {
    run["completedAt"].as_str().map(time).filter(|at| at.is_finite()).unwrap_or(f64::INFINITY)
}
fn ran_after(run: &Value, other: &Value) -> bool {
    let (end, other_end) = (run_end(run), run_end(other));
    if end == other_end { run["ordinal"].as_f64() > other["ordinal"].as_f64() } else { end > other_end }
}
fn latest_executed_run(runs: &[Value]) -> Option<&Value> {
    let mut latest: Option<&Value> = None;
    for run in runs {
        if run["status"] == "queued" || (run["status"] == "cancelled" && run["startedAt"].is_null()) {
            continue;
        }
        if latest.is_none_or(|current| ran_after(run, current)) {
            latest = Some(run);
        }
    }
    latest
}
/// Only a failed root turn of the run owns the thread's failure state.
fn latest_root_failure<'a>(run: &Value, items: &'a [Value]) -> Option<&'a Value> {
    if run["status"] != "failed" {
        return None;
    }
    let mut latest: Option<&Value> = None;
    for item in items {
        if item["type"] != "error" || item["status"] != "failed" || item["runId"] != run["id"] || item["nodeId"] != run["rootNodeId"] {
            continue;
        }
        let newer = latest.is_none_or(|current| {
            let (at, current_at) = (item["updatedAt"].as_str().map_or(f64::NAN, time), current["updatedAt"].as_str().map_or(f64::NAN, time));
            at > current_at || (at == current_at && (item["ordinal"].as_f64() > current["ordinal"].as_f64() || (item["ordinal"] == current["ordinal"] && item["id"].as_str() > current["id"].as_str())))
        });
        if newer {
            latest = Some(item);
        }
    }
    latest.map(|item| &item["failure"])
}
pub fn blocked_run(runs: &[Value], items: &[Value]) -> Option<Blocked> {
    let executed = latest_executed_run(runs)?;
    if executed["status"] != "failed" {
        return None;
    }
    let failure = latest_root_failure(executed, items)?;
    if failure["class"] != "usage_limit" {
        return None;
    }
    Some(Blocked {
        run_id: executed["id"].as_str()?.to_owned(),
        reset_at: failure["resetAt"].as_str().map(str::to_owned),
        stopped_at: executed["completedAt"].as_str().map(str::to_owned),
        presented_as_latest: runs.iter().any(|run| run["ordinal"].as_f64() > executed["ordinal"].as_f64()),
    })
}
