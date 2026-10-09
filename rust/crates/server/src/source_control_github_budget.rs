//! A local ledger of the GitHub GraphQL quota (port of `githubGraphQlBudget.ts`). Read documents
//! get a `rateLimit` selection appended, every answer's reported balance is remembered per
//! host and credential, and a read that would dip into the last tenth of the quota is refused
//! (interactive reads may spend that reserve) until the quota resets.
use crate::source_control_rate_limit::RateLimitPaused;
use serde_json::Value;
use std::{collections::HashMap, sync::Mutex};
use t3_contracts::SourceControlProviderKind;

const GRAPHQL_RESERVE_RATIO: f64 = 0.1;
const RATE_LIMIT_SELECTION: &str = "rateLimit { cost limit remaining resetAt }";

#[derive(Debug, Clone, Copy, PartialEq)]
struct Snapshot {
    cost: f64,
    limit: f64,
    remaining: f64,
    reset_at_ms: f64,
}

#[derive(Default)]
pub struct GraphQlBudget {
    snapshots: Mutex<HashMap<String, Snapshot>>,
}

fn key(host: &str, scope: &str) -> String {
    format!("{}\0{scope}", host.trim().to_lowercase())
}

fn finite_non_negative(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite() && *number >= 0.0)
}

fn snapshot_from(raw: &str) -> Option<Snapshot> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let rate_limit = parsed.get("data")?.get("rateLimit")?;
    if !rate_limit.is_object() {
        return None;
    }
    let cost = finite_non_negative(&rate_limit["cost"])?;
    let limit = rate_limit["limit"].as_f64().filter(|limit| limit.is_finite() && *limit > 0.0)?;
    let remaining = finite_non_negative(&rate_limit["remaining"])?;
    let reset_at = chrono::DateTime::parse_from_rfc3339(rate_limit["resetAt"].as_str()?).ok()?;
    Some(Snapshot {
        cost,
        limit,
        remaining,
        reset_at_ms: reset_at.timestamp_millis() as f64,
    })
}

fn is_read_operation(document: &str) -> bool {
    let operation = document.trim_start();
    operation.starts_with("query") || operation.starts_with('{')
}

fn with_rate_limit(document: &str) -> String {
    if !is_read_operation(document) {
        return document.to_string();
    }
    match document.rfind('}') {
        Some(end) if !document.contains(RATE_LIMIT_SELECTION) => {
            format!("{}\n  {RATE_LIMIT_SELECTION}\n{}", &document[..end], &document[end..])
        }
        _ => document.to_string(),
    }
}

impl GraphQlBudget {
    /// The document to send (with rate metadata requested), or the pause that refuses it.
    /// Admitting a read reserves its expected cost, so concurrent reads cannot all pass on one
    /// stale balance.
    pub fn query(
        &self,
        host: &str,
        document: &str,
        scope: &str,
        now: i64,
        allow_reserve: bool,
    ) -> Result<String, RateLimitPaused> {
        if !is_read_operation(document) {
            return Ok(document.to_string());
        }
        let key = key(host, scope);
        let mut snapshots = self.snapshots.lock().unwrap();
        if let Some(snapshot) = snapshots.get(&key).copied() {
            if snapshot.reset_at_ms <= now as f64 {
                snapshots.remove(&key);
            } else {
                let remaining = snapshot.remaining - snapshot.cost.max(1.0);
                if remaining < 0.0
                    || (!allow_reserve && remaining < snapshot.limit * GRAPHQL_RESERVE_RATIO)
                {
                    return Err(RateLimitPaused {
                        provider: SourceControlProviderKind::Github,
                        host: host.trim().to_lowercase(),
                        retry_at: snapshot.reset_at_ms as i64,
                    });
                }
                snapshots.insert(key, Snapshot { remaining, ..snapshot });
            }
        }
        Ok(with_rate_limit(document))
    }

    /// Learns the balance an answer reported.
    pub fn observe(&self, host: &str, raw: &str, scope: &str) {
        let Some(snapshot) = snapshot_from(raw) else {
            return;
        };
        let key = key(host, scope);
        let mut snapshots = self.snapshots.lock().unwrap();
        let previous = snapshots.get(&key).copied();
        // Concurrent reads finish out of order, and an answer from an older reset window must
        // not replace the current one.
        if previous.is_some_and(|previous| snapshot.reset_at_ms < previous.reset_at_ms) {
            return;
        }
        // Keep the conservative balance but learn the observed cost, so one expensive read
        // does not make every later cheap read spend its cost.
        let next = match previous {
            Some(previous)
                if snapshot.reset_at_ms == previous.reset_at_ms
                    && snapshot.remaining >= previous.remaining =>
            {
                Snapshot { cost: snapshot.cost, ..previous }
            }
            _ => snapshot,
        };
        snapshots.insert(key, next);
    }
}
