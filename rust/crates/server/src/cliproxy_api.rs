//! The CLIProxyAPI hub's built-in management API (`cliproxyApi.ts`): the accounts a hub pools, the quota of each, and the
//! redemption of a Codex reset credit through it. The hub calls the providers on its accounts' behalf (`api-call`), so
//! this server holds no provider credential and only the hub's management key.
//!
//! Every wire shape is decoded as strictly as the original's schemas (a key of the wrong type fails the whole body, `null` is not
//! an absent key), and every failure becomes one of the original's fixed sentences: nothing the hub or a provider said reaches
//! a client.
use crate::{claude_usage_limits, codex_usage_limits};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};

const CODEX_BASE: &str = "https://chatgpt.com/backend-api/wham";
const CREDIT_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The hub answers `api-call` only after it has called the provider, so this is generous.
const MANAGEMENT_TIMEOUT: Duration = Duration::from_secs(15);
/// `Effect.forEach(..., { concurrency: 4 })`
const CONCURRENCY: usize = 4;
/// `6f1c2a9e-2d4b-4c1e-9a7f-3b8d5e0c1a42`
const CREDIT_REDEEM_NAMESPACE: [u8; 16] = [0x6f, 0x1c, 0x2a, 0x9e, 0x2d, 0x4b, 0x4c, 0x1e, 0x9a, 0x7f, 0x3b, 0x8d, 0x5e, 0x0c, 0x1a, 0x42];

/// The milliseconds since the epoch, as the clock of the service reads them.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// What the hub needs of a `usageLimitSources` entry.
#[derive(Clone, Debug)]
pub struct HubConfig {
    pub url: String,
    pub management_key: String,
}

/// `UsageLimitSourceError`: a sentence a client may show.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceError(pub String);
impl SourceError {
    pub fn wire(&self) -> Value {
        json!({"_tag":"UsageLimitSourceError","detail":self.0})
    }
}

/// A step that failed: either with a sentence of the original's (`UsageLimitSourceError`), or because a body did not decode.
enum Failure {
    Source(String),
    Schema,
}
fn source(detail: &str) -> Failure {
    Failure::Source(detail.to_owned())
}

/// The hub as the usage-limit source service sees it. The service is generic over it so its polling is tested without a socket.
#[async_trait]
pub trait Hub: Send + Sync {
    /// `readAccounts`: one `UsageLimitSourceAccount` per enabled Codex or Claude account the hub lists.
    async fn read_accounts(&self, config: &HubConfig, clock: &Clock) -> Result<Vec<Value>, SourceError>;
    /// `consume`: redeems the credit through the hub, and clears the hub's cooldown for the account.
    async fn consume(&self, config: &HubConfig, account_id: &str, credit_id: &str, clock: &Clock) -> Result<Value, SourceError>;
}

pub struct CliproxyApi {
    client: reqwest::Client,
}
impl Default for CliproxyApi {
    fn default() -> Self {
        Self { client: reqwest::Client::new() }
    }
}

// ---- strict decoding ---------------------------------------------------------------------------

type Decoded<T> = Result<T, ()>;
fn object(value: &Value) -> Decoded<&Map<String, Value>> {
    value.as_object().ok_or(())
}
fn required<'a>(map: &'a Map<String, Value>, key: &str) -> Decoded<&'a Value> {
    map.get(key).ok_or(())
}
fn string(value: &Value) -> Decoded<String> {
    value.as_str().map(str::to_owned).ok_or(())
}
fn number(value: &Value) -> Decoded<f64> {
    value.as_f64().ok_or(())
}
/// `Schema.optional(T)`: a missing key, but not `null`.
fn optional<'a, T>(map: &'a Map<String, Value>, key: &str, decode: impl Fn(&'a Value) -> Decoded<T>) -> Decoded<Option<T>> {
    map.get(key).map(decode).transpose()
}
/// `Schema.optional(Schema.NullOr(T))`
fn nullable<'a, T>(map: &'a Map<String, Value>, key: &str, decode: impl Fn(&'a Value) -> Decoded<T>) -> Decoded<Option<T>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => decode(value).map(Some),
    }
}

#[derive(Clone)]
struct AuthFile {
    id: String,
    auth_index: String,
    provider: String,
    email: Option<String>,
    disabled: bool,
    chatgpt_account_id: Option<String>,
    chatgpt_plan_type: Option<String>,
}
fn decode_auth_files(body: &Value) -> Decoded<Vec<AuthFile>> {
    let files = required(object(body)?, "files")?.as_array().ok_or(())?;
    files
        .iter()
        .map(|file| {
            let file = object(file)?;
            let token = optional(file, "id_token", object)?;
            Ok(AuthFile {
                id: string(required(file, "id")?)?,
                auth_index: string(required(file, "auth_index")?)?,
                provider: string(required(file, "provider")?)?,
                email: optional(file, "email", string)?,
                disabled: optional(file, "disabled", |value| value.as_bool().ok_or(()))?.unwrap_or(false),
                chatgpt_account_id: token.map(|token| optional(token, "chatgpt_account_id", string)).transpose()?.flatten(),
                chatgpt_plan_type: token.map(|token| optional(token, "chatgpt_plan_type", string)).transpose()?.flatten(),
            })
        })
        .collect()
}

/// The `{ status_code, body }` envelope the hub wraps a provider's answer in.
fn decode_api_response(body: &Value) -> Decoded<(f64, String)> {
    let envelope = object(body)?;
    Ok((number(required(envelope, "status_code")?)?, string(required(envelope, "body")?)?))
}
fn decode_json_text(text: &str) -> Decoded<Value> {
    serde_json::from_str(text).map_err(|_| ())
}

struct CodexWindow {
    used_percent: f64,
    reset_at: Option<f64>,
    limit_window_seconds: Option<f64>,
}
fn decode_codex_window(value: &Value) -> Decoded<CodexWindow> {
    let window = object(value)?;
    Ok(CodexWindow { used_percent: number(required(window, "used_percent")?)?, reset_at: nullable(window, "reset_at", number)?, limit_window_seconds: optional(window, "limit_window_seconds", number)? })
}
struct CodexUsage {
    plan_type: Option<String>,
    primary: Option<CodexWindow>,
    secondary: Option<CodexWindow>,
}
fn decode_codex_usage(body: &str) -> Decoded<CodexUsage> {
    let body = decode_json_text(body)?;
    let usage = object(&body)?;
    let limit = match required(usage, "rate_limit")? {
        Value::Null => None,
        value => Some(object(value)?),
    };
    Ok(CodexUsage {
        plan_type: optional(usage, "plan_type", string)?,
        primary: limit.map(|limit| nullable(limit, "primary_window", decode_codex_window)).transpose()?.flatten(),
        secondary: limit.map(|limit| nullable(limit, "secondary_window", decode_codex_window)).transpose()?.flatten(),
    })
}

fn decode_claude_window(value: &Value) -> Decoded<Value> {
    let window = object(value)?;
    let resets_at = match required(window, "resets_at")? {
        Value::Null => Value::Null,
        value => json!(string(value)?),
    };
    Ok(json!({"utilization":number(required(window, "utilization")?)?,"resets_at":resets_at}))
}
/// The `get_usage` shape `claudeUsageResponseToLimits` reads, built from the hub's copy of Anthropic's usage body.
fn decode_claude_usage(body: &str) -> Decoded<Value> {
    let body = decode_json_text(body)?;
    let usage = object(&body)?;
    let mut model_scoped = vec![];
    for limit in optional(usage, "limits", |value| value.as_array().ok_or(()))?.into_iter().flatten() {
        let limit = object(limit)?;
        let kind = string(required(limit, "kind")?)?;
        let percent = nullable(limit, "percent", number)?;
        let resets_at = nullable(limit, "resets_at", string)?;
        let scope = nullable(limit, "scope", object)?;
        let display_name = match scope {
            Some(scope) => nullable(scope, "model", object)?.map(|model| string(required(model, "display_name")?)).transpose()?,
            None => None,
        };
        if let (true, Some(display_name), Some(percent)) = (kind == "weekly_scoped", display_name, percent) {
            model_scoped.push(json!({"display_name":display_name,"utilization":percent,"resets_at":resets_at}));
        }
    }
    Ok(json!({
        "rate_limits_available": true,
        "rate_limits": {
            "five_hour": nullable(usage, "five_hour", decode_claude_window)?.unwrap_or(Value::Null),
            "seven_day": nullable(usage, "seven_day", decode_claude_window)?.unwrap_or(Value::Null),
            "model_scoped": model_scoped,
        },
    }))
}

struct Credit {
    id: String,
    expires_at_ms: i64,
}
fn decode_credit_list(body: &str) -> Decoded<Vec<(String, String, String, String)>> {
    let body = decode_json_text(body)?;
    required(object(&body)?, "credits")?
        .as_array()
        .ok_or(())?
        .iter()
        .map(|credit| {
            let credit = object(credit)?;
            Ok((string(required(credit, "id")?)?, string(required(credit, "status")?)?, string(required(credit, "reset_type")?)?, string(required(credit, "expires_at")?)?))
        })
        .collect()
}

/// `Date.parse` for the shapes a provider sends: RFC 3339, a bare date, and a date-time without a zone. (JavaScript reads that
/// last one as local time; this reads it as UTC, which is the same wherever the hub's clock and this server's agree.)
fn js_date_parse(text: &str) -> Option<i64> {
    if let Ok(time) = DateTime::parse_from_rfc3339(text) {
        return Some(time.timestamp_millis());
    }
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0).map(|time| time.and_utc().timestamp_millis());
    }
    NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").ok().map(|time| time.and_utc().timestamp_millis())
}
fn iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(String::new, |time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// `creditRedeemRequestId`: a UUIDv5 per account and credit, which also deduplicates retries across T3 environments.
pub fn credit_redeem_request_id(account_id: &str, credit_id: &str) -> String {
    let mut input = CREDIT_REDEEM_NAMESPACE.to_vec();
    input.extend_from_slice(format!("{account_id}:{credit_id}").as_bytes());
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &input);
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_ref()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

fn status_text(status: f64) -> String {
    if status.fract() == 0.0 && status.abs() < 9e15 { (status as i64).to_string() } else { status.to_string() }
}

impl CliproxyApi {
    /// `management`: one authenticated call of the hub's management API. Any failure is the same sentence.
    async fn management(&self, config: &HubConfig, path: &str, body: Option<&Value>) -> Result<Value, Failure> {
        let url = url::Url::parse(&config.url).and_then(|base| base.join(&format!("/v0/management/{path}"))).map_err(|_| source("The hub URL is not valid."))?;
        let request = match body {
            None => self.client.get(url),
            Some(body) => self.client.post(url).json(body),
        };
        let failed = || source("The hub management request failed.");
        let response = request.bearer_auth(&config.management_key).timeout(MANAGEMENT_TIMEOUT).send().await.map_err(|_| failed())?;
        if !response.status().is_success() {
            return Err(failed());
        }
        response.json::<Value>().await.map_err(|_| failed())
    }

    async fn auth_files(&self, config: &HubConfig) -> Result<Vec<AuthFile>, Failure> {
        let body = self.management(config, "auth-files", None).await?;
        decode_auth_files(&body).map_err(|()| Failure::Schema)
    }

    /// `apiCall`: the hub calls `url` with the account's own token (`$TOKEN$`), and answers with the provider's status and body.
    async fn api_call(&self, config: &HubConfig, account: &AuthFile, url: &str, data: Option<&Value>) -> Result<String, Failure> {
        let header = if account.provider == "codex" {
            let mut header = json!({"Authorization":"Bearer $TOKEN$","Content-Type":"application/json","OpenAI-Beta":"codex-1","Originator":"Codex Desktop"});
            if let Some(id) = account.chatgpt_account_id.as_deref().filter(|id| !id.is_empty()) {
                header["Chatgpt-Account-Id"] = json!(id);
            }
            header
        } else {
            json!({"Authorization":"Bearer $TOKEN$","anthropic-beta":"oauth-2025-04-20"})
        };
        let mut call = json!({"auth_index":account.auth_index,"method":if data.is_none() { "GET" } else { "POST" },"url":url,"header":header});
        if let Some(data) = data {
            call["data"] = json!(data.to_string());
        }
        let raw = self.management(config, "api-call", Some(&call)).await?;
        let (status, body) = decode_api_response(&raw).map_err(|()| Failure::Schema)?;
        if !(200.0..300.0).contains(&status) {
            return Err(Failure::Source(format!("The provider refused the hub request (HTTP {}).", status_text(status))));
        }
        Ok(body)
    }

    /// `credits`: the unexpired Codex reset credits the account still has, the one that lapses first leading.
    async fn credits(&self, config: &HubConfig, account: &AuthFile, now_ms: i64) -> Result<Vec<Credit>, Failure> {
        let body = self.api_call(config, account, CREDIT_URL, None).await?;
        let mut credits: Vec<Credit> = decode_credit_list(&body)
            .map_err(|()| Failure::Schema)?
            .into_iter()
            .filter_map(|(id, status, reset_type, expires_at)| {
                let expires_at_ms = js_date_parse(&expires_at)?;
                (reset_type == "codex_rate_limits" && status == "available" && expires_at_ms > now_ms).then_some(Credit { id, expires_at_ms })
            })
            .collect();
        credits.sort_by_key(|credit| credit.expires_at_ms);
        Ok(credits)
    }

    async fn read_account(&self, config: &HubConfig, account: &AuthFile, clock: &Clock) -> Value {
        let checked_at = iso(clock());
        let mut base = json!({"id":account.id,"driver":if account.provider == "codex" { "codex" } else { "claudeAgent" }});
        if let Some(email) = account.email.as_deref().filter(|email| !email.is_empty()) {
            base["email"] = json!(email);
        }
        match self.read_account_limits(config, account, &checked_at, clock).await {
            Ok((plan, usage_limits)) => {
                if let Some(plan) = plan {
                    base["plan"] = json!(plan);
                }
                base["usageLimits"] = usage_limits;
                base
            }
            Err(_) => {
                base["usageLimits"] = claude_usage_limits::make_unavailable_usage_limits(&checked_at, "probeFailed", Some("The hub could not read this account's usage."));
                base
            }
        }
    }

    async fn read_account_limits(&self, config: &HubConfig, account: &AuthFile, checked_at: &str, clock: &Clock) -> Result<(Option<String>, Value), Failure> {
        if account.provider == "claude" {
            let body = self.api_call(config, account, CLAUDE_USAGE_URL, None).await?;
            let response = decode_claude_usage(&body).map_err(|()| Failure::Schema)?;
            let (limits, _) = claude_usage_limits::usage_response_to_limits(&response, checked_at);
            return Ok((Some("Claude Subscription".to_owned()), limits));
        }
        let body = self.api_call(config, account, &format!("{CODEX_BASE}/usage"), None).await?;
        let usage = decode_codex_usage(&body).map_err(|()| Failure::Schema)?;
        let window = |window: &Option<CodexWindow>| match window {
            None => Value::Null,
            Some(window) => {
                let mut row = json!({"usedPercent":window.used_percent,"resetsAt":window.reset_at});
                if let Some(seconds) = window.limit_window_seconds {
                    row["windowDurationMins"] = json!(seconds / 60.0);
                }
                row
            }
        };
        // A credits outage must not hide successfully fetched quota windows.
        let available = self.credits(config, account, clock()).await.ok();
        let snapshot = json!({"planType":usage.plan_type,"primary":window(&usage.primary),"secondary":window(&usage.secondary)});
        let mut limits = codex_usage_limits::rate_limits_to_limits(&snapshot, None, None, checked_at);
        if let Some(available) = &available {
            let mut credits = json!({"availableCount":available.len()});
            if let Some(next) = available.first() {
                credits["nextCreditId"] = json!(next.id);
                credits["nextExpiresAt"] = json!(iso(next.expires_at_ms));
            }
            limits["resetCredits"] = credits;
        }
        let plan = codex_usage_limits::plan_label(usage.plan_type.as_deref().or(account.chatgpt_plan_type.as_deref())).map(str::to_owned);
        Ok((plan, limits))
    }

    async fn consume_through(&self, config: &HubConfig, account_id: &str, credit_id: &str) -> Result<Value, Failure> {
        let account = self.auth_files(config).await?.into_iter().find(|account| account.id == account_id);
        let Some(account) = account.filter(|account| !account.disabled && account.provider == "codex") else {
            return Err(source("The Codex hub account is missing or disabled."));
        };
        let redeem = json!({
            "redeem_request_id": credit_redeem_request_id(account.chatgpt_account_id.as_deref().unwrap_or(&account.id), credit_id),
            "credit_id": credit_id,
        });
        let body = self.api_call(config, &account, &format!("{CREDIT_URL}/consume"), Some(&redeem)).await?;
        let code = decode_json_text(&body).ok().and_then(|body| object(&body).ok().and_then(|body| body.get("code").and_then(Value::as_str).map(str::to_owned))).ok_or(Failure::Schema)?;
        let outcome = match code.as_str() {
            "reset" => "reset",
            "nothing_to_reset" => "nothingToReset",
            "no_credit" => "noCredit",
            "already_redeemed" => "alreadyRedeemed",
            _ => return Err(Failure::Schema),
        };
        if outcome != "reset" && outcome != "alreadyRedeemed" {
            return Ok(json!({"outcome":outcome}));
        }
        let mut result = json!({"outcome":outcome});
        if self.management(config, "reset-quota", Some(&json!({"auth_index":account.auth_index}))).await.is_err() {
            result["warning"] = json!("Credit redeemed, but the hub cooldown could not be cleared. Routing may resume after its cooldown expires.");
        }
        Ok(result)
    }
}

#[async_trait]
impl Hub for CliproxyApi {
    async fn read_accounts(&self, config: &HubConfig, clock: &Clock) -> Result<Vec<Value>, SourceError> {
        let accounts = self.auth_files(config).await.map_err(|_| SourceError("The hub could not list accounts.".into()))?;
        let enabled: Vec<AuthFile> = accounts.into_iter().filter(|account| !account.disabled && (account.provider == "codex" || account.provider == "claude")).collect();
        Ok(stream::iter(enabled).map(|account| async move { self.read_account(config, &account, clock).await }).buffered(CONCURRENCY).collect().await)
    }

    async fn consume(&self, config: &HubConfig, account_id: &str, credit_id: &str, _clock: &Clock) -> Result<Value, SourceError> {
        self.consume_through(config, account_id, credit_id).await.map_err(|failure| match failure {
            Failure::Source(detail) => SourceError(detail),
            Failure::Schema => SourceError("The hub returned an unexpected reset-credit response.".into()),
        })
    }
}
