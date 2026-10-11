//! Publishing agent activity to the relay so mobile clients get push notifications and Live
//! Activities (`relay/AgentAwarenessRelay.ts`). What this crate owns is everything between "this
//! thread's activity state is X" and the relay: sanitising the state, deciding whether and when a
//! change is published, signing the publish proof, and posting it. Projecting a thread into a state
//! (`projectThreadAwarenessV2`) and listening to orchestration events stay in the server.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use t3_contracts::TrimmedNonEmptyString;

use crate::clock::Clock;
use crate::control_plane::{ControlPlane, RelayResult};
use crate::jwt::{RELAY_ACTIVITY_PUBLISH_TYP, sign_relay_jwt};
use crate::relay_url::normalize_relay_issuer;
use crate::secrets::*;
use crate::types::{
    AgentActivityPublishRequest, AgentActivityState, AgentAwarenessPhase, PublishResponse,
};

const DETAIL_MAX_LENGTH: usize = 160;
const REDACTED_FAILURE_DETAIL: &str = "The agent run failed.";
/// Backoff of a failed publish: five attempts, then the thread waits for fresh activity.
pub const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(1_000),
    Duration::from_millis(2_000),
    Duration::from_millis(4_000),
    Duration::from_millis(8_000),
    Duration::from_millis(16_000),
];
/// How long a tombstone, or a thread's first "completed", waits to be confirmed before publishing.
pub const PUBLISH_CONFIRM_DELAY: Duration = Duration::from_secs(5);
/// While not linked, the catch-up retry backs off from 5 s to 60 s.
pub const CATCH_UP_UNLINKED_RETRY_START: Duration = Duration::from_secs(5);
pub const CATCH_UP_UNLINKED_RETRY_MAX: Duration = Duration::from_secs(60);
pub const CATCH_UP_RETRY: Duration = Duration::from_secs(5);

/// Failed runs never publish their detail, and detail is cut to 160 UTF-16 units
/// (`sanitizeRelayAgentActivityState`).
pub fn sanitize_state(state: Option<AgentActivityState>) -> Option<AgentActivityState> {
    let mut state = state?;
    let detail = if state.phase == AgentAwarenessPhase::Failed {
        Some(REDACTED_FAILURE_DETAIL.to_owned())
    } else {
        state.detail.as_ref().map(|d| d.as_str().to_owned())
    };
    state.detail = detail.and_then(|detail| {
        let units: Vec<u16> = detail
            .trim()
            .encode_utf16()
            .take(DETAIL_MAX_LENGTH)
            .collect();
        let cut: String = char::decode_utf16(units)
            .map(|c| c.unwrap_or('\u{fffd}'))
            .collect();
        TrimmedNonEmptyString::new(cut.trim()).ok()
    });
    Some(state)
}

/// What makes a state worth publishing again: everything but `updatedAt`.
pub fn publish_identity(state: Option<&AgentActivityState>) -> String {
    match state {
        None => "null".to_owned(),
        Some(state) => {
            let mut value = serde_json::to_value(state).expect("state json");
            value.as_object_mut().expect("object").remove("updatedAt");
            value.to_string()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishingStartupState {
    WaitingForLink,
    Disabled,
    Enabled,
}

pub fn publishing_startup_state(
    relay_configured: bool,
    publish_enabled: bool,
) -> PublishingStartupState {
    if !relay_configured {
        PublishingStartupState::WaitingForLink
    } else if publish_enabled {
        PublishingStartupState::Enabled
    } else {
        PublishingStartupState::Disabled
    }
}

/// The link the publisher posts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRelayConfig {
    pub url: String,
    pub issuer: String,
    pub environment_credential: String,
}

/// `readRelayConfig`: needs a URL and a credential; the issuer falls back to the URL only when it
/// is absent.
/// TODO(original quirk, relay/AgentAwarenessRelay.ts readRelayConfig): `issuer ?? url` keeps an
/// empty stored issuer, so the publish proof's audience would be empty rather than the relay URL.
pub fn read_publish_relay_config(store: &dyn SecretStore) -> Option<PublishRelayConfig> {
    let read = |name| get_string(store, name).ok().flatten();
    let url = read(RELAY_URL_SECRET).filter(|v| !v.is_empty())?;
    let credential = read(RELAY_ENVIRONMENT_CREDENTIAL_SECRET).filter(|v| !v.is_empty())?;
    let issuer = read(RELAY_ISSUER_SECRET).unwrap_or_else(|| url.clone());
    Some(PublishRelayConfig {
        url,
        issuer,
        environment_credential: credential,
    })
}

pub fn read_publish_agent_activity_enabled(store: &dyn SecretStore) -> bool {
    is_agent_activity_publishing_enabled_value(
        get_string(store, PUBLISH_AGENT_ACTIVITY_SECRET)
            .ok()
            .flatten()
            .as_deref(),
    )
}

/// What to do with a freshly projected state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishDecision {
    /// Publish now.
    Publish,
    /// Nothing to publish (unchanged, historical, or still inside its confirmation window).
    Skip,
    /// Wait [`PUBLISH_CONFIRM_DELAY`], then decide again; the projection may be mid-write.
    DeferThenRecheck,
}

/// The per-thread bookkeeping of what was published and what awaits confirmation.
#[derive(Default)]
pub struct PublishTracker {
    published: HashMap<String, String>,
    confirm_deadlines: HashMap<String, i64>,
}

impl PublishTracker {
    /// A new relay link or credential invalidates everything published to the old one.
    pub fn reset_connection(&mut self) {
        self.confirm_deadlines.clear();
        self.published.clear();
    }

    pub fn has_published(&self, thread: &str) -> bool {
        self.published.contains_key(thread)
    }

    /// `terminal_since_start`: the thread finished a run after this server started, so a first
    /// "completed" or "failed" is news rather than history.
    pub fn decide(
        &mut self,
        thread: &str,
        state: Option<&AgentActivityState>,
        terminal_since_start: bool,
        now_ms: i64,
    ) -> PublishDecision {
        let identity = publish_identity(state);
        let published = self.published.get(thread);
        let terminal = state.is_some_and(|s| {
            matches!(
                s.phase,
                AgentAwarenessPhase::Completed | AgentAwarenessPhase::Failed
            )
        });
        if terminal && published.is_none() && !terminal_since_start {
            // Startup has no publish history; only work from this process may alert.
            return PublishDecision::Skip;
        }
        if published == Some(&identity) {
            // Back at (or never left) the last published state: any pending confirmation is moot.
            self.confirm_deadlines.remove(thread);
            return PublishDecision::Skip;
        }
        // Two projections need confirmation, because both can appear transiently while the
        // projector is mid-write: null while the previous state was live (it would delete the
        // thread from every armed card), and "completed" as a thread's first state (sessions boot
        // at "ready", which projects as completed for an instant).
        let requires_confirmation = (state.is_none() && published != Some(&publish_identity(None)))
            || (state.is_some_and(|s| s.phase == AgentAwarenessPhase::Completed)
                && published.is_none());
        if !requires_confirmation {
            self.confirm_deadlines.remove(thread);
            return PublishDecision::Publish;
        }
        match self.confirm_deadlines.get(thread) {
            None => {
                self.confirm_deadlines.insert(
                    thread.to_owned(),
                    now_ms + PUBLISH_CONFIRM_DELAY.as_millis() as i64,
                );
                PublishDecision::DeferThenRecheck
            }
            Some(deadline) if now_ms < *deadline => PublishDecision::Skip,
            Some(_) => PublishDecision::Publish,
        }
    }

    /// After a successful publish. A tombstone forgets the thread.
    pub fn record_published(&mut self, thread: &str, state: Option<&AgentActivityState>) {
        self.confirm_deadlines.remove(thread);
        match state {
            None => {
                self.published.remove(thread);
            }
            Some(state) => {
                self.published
                    .insert(thread.to_owned(), publish_identity(Some(state)));
            }
        }
    }
}

/// The retry budget of failed publishes, per thread.
#[derive(Default)]
pub struct PublishRetries {
    attempts: HashMap<String, usize>,
}

impl PublishRetries {
    /// The wait before the next retry, `None` once the budget is spent. The exhausted budget stays
    /// until fresh activity or a success clears it.
    pub fn next_delay(&mut self, thread: &str) -> Option<Duration> {
        let attempts = self.attempts.entry(thread.to_owned()).or_insert(0);
        let delay = RETRY_DELAYS.get(*attempts).copied()?;
        *attempts += 1;
        Some(delay)
    }

    pub fn clear(&mut self, thread: &str) {
        self.attempts.remove(thread);
    }
}

/// Counts of what the relay did with one publish, for the log (`deliveryStats`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeliveryStats {
    pub total: usize,
    pub queued: usize,
    pub successful: usize,
    pub failed: usize,
    pub failed_reasons: Vec<String>,
}

pub fn delivery_stats(response: &PublishResponse) -> DeliveryStats {
    let mut stats = DeliveryStats {
        total: response.deliveries.len(),
        ..Default::default()
    };
    for delivery in &response.deliveries {
        if delivery.queued == Some(true) {
            stats.queued += 1;
        } else if delivery.ok {
            stats.successful += 1;
        } else {
            stats.failed += 1;
            let status = delivery
                .apns_status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "transport".to_owned());
            let reason = delivery
                .apns_reason
                .clone()
                .unwrap_or_else(|| "unknown".to_owned());
            stats.failed_reasons.push(format!("{status}:{reason}"));
        }
    }
    stats
}

/// Signs the publish proof: the environment vouches for exactly this state of exactly this thread.
pub fn make_publish_proof(
    private_key: &str,
    relay_issuer: &str,
    environment_id: &str,
    thread_id: &str,
    state: Option<&AgentActivityState>,
    jti: &str,
    now_seconds: i64,
) -> Result<String, crate::jwt::JwtError> {
    let payload = json!({
        "iss": format!("t3-env:{environment_id}"),
        "aud": normalize_relay_issuer(relay_issuer),
        "sub": environment_id,
        "jti": jti,
        "iat": now_seconds,
        "exp": now_seconds + 5 * 60,
        "environmentId": environment_id,
        "threadId": thread_id,
        "state": state,
    });
    sign_relay_jwt(private_key, RELAY_ACTIVITY_PUBLISH_TYP, &payload)
}

/// Signs and posts one thread's state (`publishState` in the original).
pub async fn publish_thread_state(
    control_plane: &dyn ControlPlane,
    clock: &dyn Clock,
    key_pair: &crate::jwt::KeyPair,
    config: &PublishRelayConfig,
    environment_id: &str,
    thread_id: &str,
    state: Option<AgentActivityState>,
) -> RelayResult<PublishResponse> {
    let proof = make_publish_proof(
        &key_pair.private_key,
        &config.issuer,
        environment_id,
        thread_id,
        state.as_ref(),
        &uuid::Uuid::new_v4().to_string(),
        clock.now_seconds(),
    )
    .map_err(|_| crate::relay_error::RelayRequestError::unexpected_response())?;
    let response = control_plane
        .publish_agent_activity(
            &config.url,
            &config.environment_credential,
            environment_id,
            thread_id,
            &AgentActivityPublishRequest {
                state,
                proof: TrimmedNonEmptyString::new(&proof).expect("proof"),
            },
        )
        .await?;
    let stats = delivery_stats(&response);
    tracing::info!(
        thread_id,
        ok = response.ok,
        ?stats,
        "agent activity publish completed"
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use t3_contracts::{EnvironmentId, ThreadId};

    fn state(phase: AgentAwarenessPhase, detail: Option<&str>) -> AgentActivityState {
        let s = |v: &str| TrimmedNonEmptyString::new(v).unwrap();
        AgentActivityState {
            environment_id: EnvironmentId::new("env_1").unwrap(),
            thread_id: ThreadId::new("thread-1").unwrap(),
            project_title: s("Project"),
            thread_title: s("Thread"),
            phase,
            headline: s("Working"),
            detail: detail.map(s),
            model_title: s("Model"),
            updated_at: s("2026-01-01T00:00:00.000Z"),
            deep_link: s("t3://thread/1"),
        }
    }

    #[test]
    fn failed_runs_never_publish_their_detail() {
        let sanitized = sanitize_state(Some(state(
            AgentAwarenessPhase::Failed,
            Some("secret path /home/x"),
        )))
        .unwrap();
        assert_eq!(
            sanitized.detail.as_ref().map(|d| d.as_str()),
            Some("The agent run failed.")
        );
        let none = sanitize_state(Some(state(AgentAwarenessPhase::Failed, None))).unwrap();
        assert_eq!(
            none.detail.as_ref().map(|d| d.as_str()),
            Some("The agent run failed.")
        );
    }

    #[test]
    fn detail_is_cut_to_160_units_and_trimmed() {
        let long = format!("{} tail", "a".repeat(159));
        let sanitized =
            sanitize_state(Some(state(AgentAwarenessPhase::Running, Some(&long)))).unwrap();
        assert_eq!(sanitized.detail.as_ref().unwrap().as_str().len(), 159);
        let blank_after_cut = format!("{}   x", " ".repeat(0));
        let kept = sanitize_state(Some(state(
            AgentAwarenessPhase::Running,
            Some(&blank_after_cut),
        )))
        .unwrap();
        assert_eq!(kept.detail.as_ref().unwrap().as_str(), "x");
        assert!(sanitize_state(None).is_none());
    }

    #[test]
    fn identity_ignores_updated_at() {
        let mut a = state(AgentAwarenessPhase::Running, None);
        let mut b = a.clone();
        b.updated_at = TrimmedNonEmptyString::new("2030-01-01T00:00:00.000Z").unwrap();
        assert_eq!(publish_identity(Some(&a)), publish_identity(Some(&b)));
        a.headline = TrimmedNonEmptyString::new("Other").unwrap();
        assert_ne!(publish_identity(Some(&a)), publish_identity(Some(&b)));
        assert_eq!(publish_identity(None), "null");
    }

    #[test]
    fn startup_state_follows_link_and_opt_in() {
        assert_eq!(
            publishing_startup_state(false, true),
            PublishingStartupState::WaitingForLink
        );
        assert_eq!(
            publishing_startup_state(true, false),
            PublishingStartupState::Disabled
        );
        assert_eq!(
            publishing_startup_state(true, true),
            PublishingStartupState::Enabled
        );
    }

    #[test]
    fn first_completed_and_tombstones_wait_for_confirmation() {
        let mut tracker = PublishTracker::default();
        let completed = state(AgentAwarenessPhase::Completed, None);
        // A first "completed" from this process is deferred, then published once the deadline passes.
        assert_eq!(
            tracker.decide("t", Some(&completed), true, 1_000),
            PublishDecision::DeferThenRecheck
        );
        assert_eq!(
            tracker.decide("t", Some(&completed), true, 3_000),
            PublishDecision::Skip
        );
        assert_eq!(
            tracker.decide("t", Some(&completed), true, 6_000),
            PublishDecision::Publish
        );
        tracker.record_published("t", Some(&completed));
        assert_eq!(
            tracker.decide("t", Some(&completed), true, 7_000),
            PublishDecision::Skip
        );
        // A tombstone while live state was published is deferred the same way.
        assert_eq!(
            tracker.decide("t", None, false, 8_000),
            PublishDecision::DeferThenRecheck
        );
        assert_eq!(
            tracker.decide("t", None, false, 14_000),
            PublishDecision::Publish
        );
        tracker.record_published("t", None);
        assert!(!tracker.has_published("t"));
    }

    #[test]
    fn historical_terminal_threads_stay_quiet() {
        let mut tracker = PublishTracker::default();
        let failed = state(AgentAwarenessPhase::Failed, None);
        assert_eq!(
            tracker.decide("t", Some(&failed), false, 0),
            PublishDecision::Skip
        );
        let running = state(AgentAwarenessPhase::Running, None);
        assert_eq!(
            tracker.decide("t", Some(&running), false, 0),
            PublishDecision::Publish
        );
    }

    #[test]
    fn recovered_projection_clears_the_pending_confirmation() {
        let mut tracker = PublishTracker::default();
        let running = state(AgentAwarenessPhase::Running, None);
        tracker.record_published("t", Some(&running));
        assert_eq!(
            tracker.decide("t", None, false, 0),
            PublishDecision::DeferThenRecheck
        );
        assert_eq!(
            tracker.decide("t", Some(&running), false, 1_000),
            PublishDecision::Skip
        );
        // The deadline is gone, so a later transient null defers again instead of publishing at once.
        assert_eq!(
            tracker.decide("t", None, false, 60_000),
            PublishDecision::DeferThenRecheck
        );
    }

    #[test]
    fn retry_budget_runs_out_after_five_attempts() {
        let mut retries = PublishRetries::default();
        let delays: Vec<_> = std::iter::from_fn(|| retries.next_delay("t")).collect();
        assert_eq!(delays, RETRY_DELAYS);
        assert_eq!(retries.next_delay("t"), None);
        retries.clear("t");
        assert_eq!(retries.next_delay("t"), Some(Duration::from_secs(1)));
    }

    #[test]
    fn delivery_stats_count_queued_ok_and_failed() {
        let response: PublishResponse = serde_json::from_value(json!({
            "ok": true,
            "deliveries": [
                {"deviceId": "a", "kind": "push_notification", "ok": true, "apnsStatus": 200, "apnsReason": null, "apnsId": null},
                {"deviceId": "b", "kind": "push_notification", "ok": false, "queued": true, "apnsStatus": null, "apnsReason": null, "apnsId": null},
                {"deviceId": "c", "kind": "live_activity_update", "ok": false, "apnsStatus": 410, "apnsReason": "Unregistered", "apnsId": null},
                {"deviceId": "d", "kind": "live_activity_update", "ok": false, "apnsStatus": null, "apnsReason": null, "apnsId": null}
            ]
        }))
        .unwrap();
        let stats = delivery_stats(&response);
        assert_eq!(
            (stats.total, stats.queued, stats.successful, stats.failed),
            (4, 1, 1, 2)
        );
        assert_eq!(
            stats.failed_reasons,
            ["410:Unregistered", "transport:unknown"]
        );
    }
}
