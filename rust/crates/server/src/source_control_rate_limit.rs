//! Per host and credential cooldowns after a rate-limited answer (port of
//! `SourceControlRateLimit.ts`). A request first asks `check`; the answer carries a generation
//! (the lease) that `record_rate_limit` and `record_success` hand back, so a slow request that
//! started before a pause cannot clear it, and a fresh reset time from an older request cannot
//! shorten it. Without a reset time from the host, repeated limits back off from 30 seconds,
//! doubling up to 15 minutes, until a success.
use std::{collections::HashMap, sync::Mutex};
use t3_contracts::SourceControlProviderKind;

const FALLBACK_COOLDOWN_MS: i64 = 30_000;
const MAX_FALLBACK_COOLDOWN_MS: i64 = 15 * 60_000;

/// A request refused because its host is paused until `retry_at` (epoch milliseconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitPaused {
    pub provider: SourceControlProviderKind,
    pub host: String,
    pub retry_at: i64,
}

impl RateLimitPaused {
    pub fn message(&self) -> String {
        format!(
            "{} requests to {} are paused until the rate limit resets.",
            self.provider, self.host
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    attempt: u32,
    generation: u64,
    retry_at: i64,
}

#[derive(Default)]
pub struct RateLimit {
    entries: Mutex<HashMap<String, Entry>>,
}

fn key(provider: SourceControlProviderKind, host: &str, scope: &str) -> String {
    format!("{provider}\0{}\0{scope}", host.trim().to_lowercase())
}

fn fallback_cooldown_ms(attempt: u32) -> i64 {
    let doubled = FALLBACK_COOLDOWN_MS.saturating_mul(1i64 << attempt.saturating_sub(1).min(32));
    doubled.min(MAX_FALLBACK_COOLDOWN_MS)
}

/// `Retry-After` as seconds or an HTTP date, as an epoch time; `None` when absent or past.
pub fn retry_at_from_header(value: Option<&str>, now: i64) -> Option<i64> {
    let normalized = value?.trim();
    if !normalized.is_empty() && normalized.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds: i64 = normalized.parse().ok()?;
        return seconds.checked_mul(1000)?.checked_add(now).filter(|at| *at <= (1 << 53) - 1);
    }
    let parsed = chrono::DateTime::parse_from_rfc2822(normalized)
        .map(|date| date.timestamp_millis())
        .ok()
        .or_else(|| crate::source_control_decode::parse_date(normalized).map(|date| date.timestamp_millis()))?;
    (parsed > now).then_some(parsed)
}

impl RateLimit {
    /// The lease for a request, or the pause that refuses it. `allow_paused` lets an
    /// interactive request through a pause without clearing it.
    pub fn check(
        &self,
        provider: SourceControlProviderKind,
        host: &str,
        scope: &str,
        now: i64,
        allow_paused: bool,
    ) -> Result<u64, RateLimitPaused> {
        let entries = self.entries.lock().unwrap();
        let entry = entries.get(&key(provider, host, scope));
        if let Some(entry) = entry
            && entry.retry_at > now
            && !allow_paused
        {
            return Err(RateLimitPaused {
                provider,
                host: host.trim().to_lowercase(),
                retry_at: entry.retry_at,
            });
        }
        Ok(entry.map_or(0, |entry| entry.generation))
    }

    pub fn record_rate_limit(
        &self,
        provider: SourceControlProviderKind,
        host: &str,
        scope: &str,
        lease: u64,
        retry_at: Option<i64>,
        now: i64,
    ) {
        let mut entries = self.entries.lock().unwrap();
        let key = key(provider, host, scope);
        let previous = entries.get(&key).copied();
        if let Some(previous) = previous
            && previous.generation > lease
        {
            // A newer pause already exists: only a later reset time may extend it.
            if previous.retry_at <= now && retry_at.is_none_or(|at| at <= now) {
                return;
            }
            let extended = retry_at.filter(|at| *at > previous.retry_at).unwrap_or(previous.retry_at);
            if extended != previous.retry_at {
                entries.insert(key, Entry { retry_at: extended, ..previous });
            }
            return;
        }
        let attempt = previous.map_or(0, |entry| entry.attempt) + 1;
        let proposed = match retry_at {
            Some(at) if at > now => at,
            _ => now + fallback_cooldown_ms(attempt),
        };
        let retry_at = match previous {
            Some(previous) if previous.retry_at > now => previous.retry_at.max(proposed),
            _ => proposed,
        };
        entries.insert(
            key,
            Entry {
                attempt,
                generation: previous.map_or(0, |entry| entry.generation).max(lease) + 1,
                retry_at,
            },
        );
    }

    pub fn record_success(
        &self,
        provider: SourceControlProviderKind,
        host: &str,
        scope: &str,
        lease: u64,
        now: i64,
    ) {
        let mut entries = self.entries.lock().unwrap();
        let key = key(provider, host, scope);
        if let Some(previous) = entries.get(&key).copied()
            && previous.generation == lease
            && previous.retry_at <= now
        {
            entries.insert(
                key,
                Entry {
                    attempt: 0,
                    generation: previous.generation,
                    retry_at: 0,
                },
            );
        }
    }
}
