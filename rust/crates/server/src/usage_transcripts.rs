//! Parsers for the Claude Code and Codex on-disk session transcripts (`usageTranscripts.ts`).
//!
//! Each is a line-at-a-time reducer so large files stream without being materialised. None touch
//! the filesystem. Grok, Cursor, OpenCode and Antigravity transcripts are not read: this server has
//! no adapters for them.
use serde_json::Value;

/// Billing speed of a request. Faster speeds bill at a model-specific premium. Claude fast mode and
/// Codex `priority` are `fast`; Codex `ultrafast` is its own, more expensive tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Standard,
    Fast,
    Ultrafast,
}

/// Token counts for a bucket. `cached` and `cache_creation` are disjoint from `uncached_input`;
/// `reasoning` is a subset of `output` and must never be added on top.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub uncached_input: u64,
    pub cached_input: u64,
    pub cache_creation: u64,
    pub output: u64,
    pub reasoning: u64,
}
impl Totals {
    pub fn total(&self) -> u64 {
        self.uncached_input + self.cached_input + self.cache_creation + self.output
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageRecord {
    /// `claude` or `codex`.
    pub provider: &'static str,
    pub timestamp_ms: i64,
    pub model: String,
    pub session_id: String,
    pub totals: Totals,
    pub reported_cost_usd: Option<f64>,
    pub speed: Speed,
    /// Key for cross-file de-duplication, `None` when the record is inherently unique.
    pub dedupe_key: Option<String>,
}

fn int(value: &Value) -> u64 {
    value.as_f64().filter(|value| value.is_finite() && *value > 0.0).map_or(0, |value| value.trunc() as u64)
}

/// `Date.parse` for the shapes transcripts carry: RFC 3339, or a date-time without a zone (UTC).
fn parse_timestamp_ms(value: &Value) -> Option<i64> {
    let text = value.as_str()?;
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(time.timestamp_millis());
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(time) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Some(time.and_utc().timestamp_millis());
        }
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok().and_then(|date| date.and_hms_opt(0, 0, 0)).map(|time| time.and_utc().timestamp_millis())
}

/// The cheap substring gate applied before `JSON.parse`: only a minority of lines carry usage.
pub fn might_carry_usage(line: &str, provider: &str) -> bool {
    if provider == "claude" { line.contains("\"usage\"") } else { line.contains("\"token_count\"") }
}

/// `parseClaudeRecord`. T3 Code writes one record per assistant content block and each repeats the
/// parent message's complete `usage`, so the caller must drop repeats by `dedupe_key`.
pub fn parse_claude_record(record: &Value) -> Option<UsageRecord> {
    if record["type"] != "assistant" {
        return None;
    }
    let message = record["message"].as_object()?;
    let usage = message.get("usage")?.as_object()?;
    let timestamp_ms = parse_timestamp_ms(&record["timestamp"])?;
    let model = message.get("model").and_then(Value::as_str).unwrap_or("");
    if model.is_empty() {
        return None;
    }
    let message_id = message.get("id").and_then(Value::as_str);
    let request_id = record["requestId"].as_str();
    // Matches ccusage: prefer the message/request pair, fall back to whichever half exists.
    let dedupe_key = (message_id.is_some() || request_id.is_some()).then(|| format!("{}:{}", message_id.unwrap_or(""), request_id.unwrap_or("")));
    let field = |name: &str| usage.get(name).map_or(0, int);
    Some(UsageRecord {
        provider: "claude",
        timestamp_ms,
        model: model.to_owned(),
        session_id: record["sessionId"].as_str().unwrap_or("").to_owned(),
        totals: Totals {
            uncached_input: field("input_tokens"),
            cached_input: field("cache_read_input_tokens"),
            cache_creation: field("cache_creation_input_tokens"),
            output: field("output_tokens"),
            // Anthropic folds thinking tokens into output and does not break them out.
            reasoning: 0,
        },
        reported_cost_usd: record["costUSD"].as_f64().filter(|cost| cost.is_finite()),
        speed: if usage.get("speed").and_then(Value::as_str) == Some("fast") { Speed::Fast } else { Speed::Standard },
        dedupe_key,
    })
}
pub fn parse_claude_line(line: &str) -> Option<UsageRecord> {
    parse_claude_record(&serde_json::from_str(line).ok()?)
}

/// Rolling state for a single Codex rollout file. `token_count` events carry no model or service
/// tier, so both are carried forward from `turn_context` and `thread_settings_applied`.
#[derive(Clone, Debug, PartialEq)]
pub struct CodexScanState {
    pub model: String,
    pub speed: Speed,
    pub session_id: String,
    pub(crate) last_usage_signature: Option<String>,
    pub(crate) saw_session_meta: bool,
    /// While true, leading usage events are re-stamped copies of parent history.
    pub(crate) suppressing_fork_copies: bool,
    pub(crate) fork_copy_anchor_ms: i64,
}
impl Default for CodexScanState {
    fn default() -> Self {
        Self { model: String::new(), speed: Speed::Standard, session_id: String::new(), last_usage_signature: None, saw_session_meta: false, suppressing_fork_copies: false, fork_copy_anchor_ms: 0 }
    }
}

/// A fork's copied history is written in one synchronous burst; its first genuine event lands after
/// a real model turn. One second of separation splits the two (`ccusage` uses the same threshold).
const FORK_COPY_MAX_GAP_MS: i64 = 1000;

fn is_forked_session_meta(payload: &Value) -> bool {
    payload["forked_from_id"].is_string() || payload["source"]["subagent"]["thread_spawn"]["parent_thread_id"].is_string()
}
fn codex_speed(tier: &Value) -> Speed {
    match tier.as_str() {
        Some("priority" | "fast") => Speed::Fast,
        Some("ultrafast") => Speed::Ultrafast,
        _ => Speed::Standard,
    }
}

/// `parseCodexRecord`: deltas come from `last_token_usage`; consecutive duplicates are dropped.
pub fn parse_codex_record(record: &Value, state: &mut CodexScanState) -> Option<UsageRecord> {
    let payload = record["payload"].as_object().map(|_| &record["payload"])?;
    let payload_type = payload["type"].as_str();
    if record["type"] == "session_meta" {
        // Only the first meta describes this file's own session; a fork repeats the ancestors'.
        if state.saw_session_meta {
            return None;
        }
        state.saw_session_meta = true;
        if let Some(id) = payload["id"].as_str().or_else(|| payload["session_id"].as_str()).filter(|_| payload["id"].is_string() || payload["id"].is_null()) {
            state.session_id = id.to_owned();
        }
        if let Some(at) = parse_timestamp_ms(&record["timestamp"]) {
            if is_forked_session_meta(payload) {
                state.suppressing_fork_copies = true;
                state.fork_copy_anchor_ms = at;
            }
        }
        return None;
    }
    if record["type"] == "turn_context" {
        if let Some(model) = payload["model"].as_str() {
            state.model = model.to_owned();
        }
        return None;
    }
    if payload_type == Some("thread_settings_applied") {
        if payload["thread_settings"].is_object() {
            state.speed = codex_speed(&payload["thread_settings"]["service_tier"]);
        }
        return None;
    }
    if payload_type != Some("token_count") {
        return None;
    }
    let last = payload["info"]["last_token_usage"].as_object().map(|_| &payload["info"]["last_token_usage"])?;
    // Only an event that is otherwise eligible may consume the duplicate signature.
    let timestamp_ms = parse_timestamp_ms(&record["timestamp"])?;
    if state.model.is_empty() {
        return None;
    }
    // Codex re-emits an unchanged token_count on some stream boundaries.
    let signature = last.to_string();
    if state.last_usage_signature.as_deref() == Some(signature.as_str()) {
        return None;
    }
    state.last_usage_signature = Some(signature);
    if state.suppressing_fork_copies {
        if timestamp_ms - state.fork_copy_anchor_ms < FORK_COPY_MAX_GAP_MS {
            state.fork_copy_anchor_ms = timestamp_ms;
            return None;
        }
        state.suppressing_fork_copies = false;
    }
    let (input, cached, creation, output) = (int(&last["input_tokens"]), int(&last["cached_input_tokens"]), int(&last["cache_write_input_tokens"]), int(&last["output_tokens"]));
    let totals = Totals {
        // Codex reports `input_tokens` inclusive of the cached portion.
        uncached_input: input.saturating_sub(cached).saturating_sub(creation),
        cached_input: cached,
        cache_creation: creation,
        output,
        reasoning: output.min(int(&last["reasoning_output_tokens"])),
    };
    if totals.total() == 0 {
        return None;
    }
    Some(UsageRecord { provider: "codex", timestamp_ms, model: state.model.clone(), session_id: state.session_id.clone(), totals, reported_cost_usd: None, speed: state.speed, dedupe_key: None })
}
pub fn parse_codex_line(line: &str, state: &mut CodexScanState) -> Option<UsageRecord> {
    parse_codex_record(&serde_json::from_str(line).ok()?, state)
}

/// `dedupeWithinFile`: the first record of a key wins; `seen` spans the caller's chunks.
pub fn dedupe_within_file(records: Vec<UsageRecord>, seen: &mut std::collections::HashSet<String>) -> Vec<UsageRecord> {
    records.into_iter().filter(|record| record.dedupe_key.as_ref().is_none_or(|key| seen.insert(key.clone()))).collect()
}
