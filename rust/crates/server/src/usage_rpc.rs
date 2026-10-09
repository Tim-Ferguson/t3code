//! `server.getUsageSummary`, `server.refreshUsageRates` and `provider.consumeResetCredit`: decode,
//! one service call and the original's error envelopes.
use crate::{claude_reset_credits, provider_registry::ProviderRegistry, transport::ApiState, usage_service};
use serde_json::{Value, json};
use t3_contracts::{ProviderInstanceId, ProviderSetupError, ProviderSetupErrorTag};

pub const METHODS: [&str; 3] = ["server.getUsageSummary", "server.refreshUsageRates", "provider.consumeResetCredit"];

fn decode_error(message: impl Into<String>) -> Value {
    json!({"_tag":"SchemaDecodeError","message":message.into()})
}
fn day(value: &Value) -> bool {
    value.as_str().is_some_and(|day| day.len() == 10 && day.bytes().enumerate().all(|(index, byte)| if index == 4 || index == 7 { byte == b'-' } else { byte.is_ascii_digit() }))
}
fn state_dir(state: &ApiState) -> Result<std::path::PathBuf, Value> {
    state.store.state_dir().map(std::path::Path::to_path_buf).ok_or_else(|| json!({"_tag":"NativeServiceUnavailableError","message":"Usage needs a state directory."}))
}

pub async fn command(state: &ApiState, tag: &str, payload: Value) -> Result<Value, Value> {
    match tag {
        "server.getUsageSummary" => {
            if !day(&payload["sinceDay"]) || !day(&payload["untilDay"]) || payload["timeZone"].as_str().is_none_or(|zone| zone.trim().is_empty()) {
                return Err(decode_error("A usage window needs sinceDay and untilDay as YYYY-MM-DD and a non-empty timeZone."));
            }
            if !payload["resolution"].is_null() && !matches!(payload["resolution"].as_str(), Some("day" | "hour")) {
                return Err(decode_error("resolution must be \"day\" or \"hour\"."));
            }
            let settings = settings(state).await;
            usage_service::of(&state_dir(state)?).read_summary(&payload, &settings).await.map_err(|error| error.wire())
        }
        "server.refreshUsageRates" => Ok(usage_service::of(&state_dir(state)?).refresh_rates().await),
        "provider.consumeResetCredit" => consume_reset_credit(state, &payload).await,
        other => Err(json!({"_tag":"NativeMethodUnsupportedError","method":other})),
    }
}

/// The server settings usage reads: price overrides, model aliases and where each provider keeps its history.
async fn settings(state: &ApiState) -> Value {
    if let Some(providers) = &state.providers {
        return serde_json::to_value(providers.settings()).unwrap_or(Value::Null);
    }
    match &state.settings {
        Some(settings) => settings.snapshot().await.ok().and_then(|snapshot| serde_json::to_value(snapshot).ok()).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

fn setup_error(instance: &str, detail: impl Into<String>) -> Value {
    let error = ProviderSetupError {
        tag: ProviderSetupErrorTag::ProviderSetupError,
        instance_id: instance.parse::<ProviderInstanceId>().unwrap_or_else(|_| "claudeAgent".parse().unwrap()),
        operation: "consume-reset-credit".into(),
        detail: detail.into(),
        cause: None,
    };
    serde_json::to_value(error).expect("typed provider setup error")
}

async fn consume_reset_credit(state: &ApiState, payload: &Value) -> Result<Value, Value> {
    // A cliproxy usage-limit source banks credits on another account; this server has none configured.
    if payload.get("sourceId").is_some() {
        return Err(json!({"_tag":"UsageLimitSourceError","detail":"Usage limit sources are not configured on this server."}));
    }
    let Some(instance) = payload["instanceId"].as_str().filter(|id| !id.is_empty()) else { return Err(decode_error("provider.consumeResetCredit needs an instanceId or a sourceId.")) };
    let providers = state.providers.as_ref().ok_or_else(|| setup_error(instance, "Provider instance not found."))?;
    let snapshot = providers.snapshots().into_iter().find(|snapshot| snapshot["instanceId"] == instance);
    // A disabled instance must not spend anything on its account.
    let Some(snapshot) = snapshot.filter(|snapshot| snapshot["enabled"] == true) else {
        let known = providers.snapshots().iter().any(|snapshot| snapshot["instanceId"] == instance);
        return Err(setup_error(instance, if known { "This provider is disabled." } else { "Provider instance not found." }));
    };
    if snapshot["driver"] != "claudeAgent" {
        return Err(setup_error(instance, "This provider does not bank reset credits."));
    }
    match consume_claude(providers, instance, &snapshot).await {
        Ok(outcome) => Ok(json!({"outcome":outcome})),
        Err(detail) => Err(setup_error(instance, detail)),
    }
}

/// `ClaudeDriver.consumeResetCredit`: serialised on the config directory that holds the login, one
/// request id kept until Claude answers (a cooldown or rate limit is an answer), then a re-probe.
async fn consume_claude(providers: &ProviderRegistry, instance_id: &str, snapshot: &Value) -> Result<&'static str, String> {
    let instance = providers.claude(instance_id).map_err(|error| error.to_string())?;
    let (Some(grant), Some(version)) = (snapshot["usageLimits"]["resetCredits"]["nextCreditId"].as_str(), snapshot["version"].as_str()) else { return Ok("noCredit") };
    let config_dir = instance.config_dir();
    let environment = instance.process_environment();
    let explicit = !instance.config.home_path.as_str().trim().is_empty() || environment.get("CLAUDE_CONFIG_DIR").is_some_and(|dir| !dir.trim().is_empty());
    let home = environment.get("HOME").cloned().or_else(|| std::env::var("HOME").ok()).map(std::path::PathBuf::from);
    let account_path = claude_reset_credits::account_config_path(explicit.then_some(config_dir.as_path()), home.as_deref());
    let outcome = claude_reset_credits::redeem(&config_dir.to_string_lossy(), |request_id| {
        let (config_dir, account_path, environment, grant, version) = (config_dir.clone(), account_path.clone(), environment.clone(), grant.to_owned(), version.to_owned());
        async move { claude_reset_credits::consume(claude_reset_credits::Claim { config_dir: &config_dir, account_config_path: &account_path, version: &version, grant_id: &grant, request_id: &request_id, environment: &environment }).await }
    })
    .await
    .map_err(|failure| failure.message().to_owned())?;
    // Re-probe after any answer, but only a reset claims the limits changed.
    let before = snapshot["usageLimits"]["checkedAt"].clone();
    let refreshed = Box::pin(providers.refresh_instance(instance_id)).await.ok().and_then(|all| all.into_iter().find(|snapshot| snapshot["instanceId"] == instance_id));
    let after = refreshed.as_ref().map(|snapshot| snapshot["usageLimits"].clone()).unwrap_or(Value::Null);
    if outcome == "reset" && (after.is_null() || after["checkedAt"] == before || after["unavailable"]["reason"] == "probeFailed") {
        return Err("The reset was applied, but Claude could not confirm the new limits. Refresh to check.".into());
    }
    Ok(outcome)
}
