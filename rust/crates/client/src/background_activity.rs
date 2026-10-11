//! The client's report of its own activity to the server (`server.reportClientActivity`), which the server's background
//! policy turns into leases: while no window is visible, focused or recently used, periodic provider, VCS and
//! diagnostics work waits. This is `apps/web/src/lib/backgroundActivityReporter.ts`: the report carries the baseline
//! `provider-status` scope plus one scope for each open `subscribeVcsStatus` (with remote refresh) and
//! `subscribeResourceTelemetry` stream, a lease lasts 45 seconds and is renewed every 25.
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const METHOD: &str = "server.reportClientActivity";
/// A report is repeated at this interval whatever else happens, so the lease never lapses under an idle window.
pub const REPORT_INTERVAL_MS: u64 = 25_000;
pub const LEASE_TTL_MS: u64 = 45_000;
/// Input newer than this makes the report say `recentlyInteracted`.
pub const RECENT_INTERACTION_WINDOW_MS: i64 = LEASE_TTL_MS as i64;
/// Triggers within this window of the first one are answered by one report.
pub const DEBOUNCE_MS: u64 = 250;
/// What a browser that cannot keep an id reports, as the original does.
pub const EPHEMERAL_CLIENT_ID: &str = "ephemeral-browser-client";
pub const CLIENT_ID_STORAGE_KEY: &str = "t3.backgroundActivity.clientId";

/// The resource telemetry stream, which the Diagnostics page opens.
const RESOURCE_TELEMETRY: &str = crate::resource_telemetry::SUBSCRIBE;

const BASELINE_SCOPES: [&str; 1] = ["provider-status"];

pub fn was_recently_interacted(last_interaction_at_ms: i64, observed_at_ms: i64) -> bool {
    last_interaction_at_ms <= observed_at_ms && observed_at_ms - last_interaction_at_ms <= RECENT_INTERACTION_WINDOW_MS
}

/// The scope a stream holds open for as long as it runs: `subscribeResourceTelemetry` keeps diagnostics wanted, and
/// `subscribeVcsStatus` keeps its directory's status wanted unless it asked for the local state only.
pub fn scope_for_subscription(method: &str, input: &Value) -> Option<Value> {
    match method {
        RESOURCE_TELEMETRY => Some(json!({"type": "diagnostics"})),
        "subscribeVcsStatus" => {
            if input.get("includeRemote") == Some(&json!(false)) {
                return None;
            }
            input.get("cwd").and_then(Value::as_str).map(|cwd| json!({"type": "vcs-status", "cwd": cwd}))
        }
        _ => None,
    }
}

/// The scopes streams hold open, counted: two streams of one directory are one scope until both end.
#[derive(Debug, Default)]
pub struct RetainedScopes {
    entries: BTreeMap<String, (String, Value, usize)>,
}

impl RetainedScopes {
    /// Holds `scope` for `environment`; the returned key is what `release` takes. The second element is whether the set
    /// of scopes changed (a first holder), which is when a new report is due.
    pub fn retain(&mut self, environment: &str, scope: Value) -> (String, bool) {
        let key = json!([environment, scope]).to_string();
        let entry = self.entries.entry(key.clone()).or_insert_with(|| (environment.to_owned(), scope, 0));
        entry.2 += 1;
        let first = entry.2 == 1;
        (key, first)
    }
    /// Whether the set of scopes changed (the last holder let go).
    pub fn release(&mut self, key: &str) -> bool {
        let Some(entry) = self.entries.get_mut(key) else { return false };
        entry.2 -= 1;
        if entry.2 == 0 {
            self.entries.remove(key);
            return true;
        }
        false
    }
    pub fn scopes_of(&self, environment: &str) -> Vec<Value> {
        self.entries.values().filter(|(owner, _, _)| owner == environment).map(|(_, scope, _)| scope.clone()).collect()
    }
}

/// What the page tells the client about its window; the rest of a report is the client's own.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub client_id: String,
    pub visible: bool,
    pub focused: bool,
    /// Milliseconds since the last pointer move, key, wheel or touch, at the moment of the reading.
    pub idle_ms: Option<i64>,
}

impl Observation {
    /// A message of the page's sensor: `{"clientId","visible","focused","idleMs"}`.
    pub fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            client_id: value.get("clientId")?.as_str().filter(|id| !id.is_empty()).unwrap_or(EPHEMERAL_CLIENT_ID).to_owned(),
            visible: value.get("visible")?.as_bool()?,
            focused: value.get("focused")?.as_bool()?,
            idle_ms: value.get("idleMs").and_then(Value::as_i64),
        })
    }
}

fn iso(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// The `ClientActivityReportInput` for one environment.
pub fn report(environment_id: &str, observation: &Observation, kind: &str, retained: &[Value], observed_at_ms: i64) -> Value {
    let last_interaction_at = observed_at_ms - observation.idle_ms.unwrap_or(0).max(0);
    let mut scopes: Vec<Value> = BASELINE_SCOPES.iter().map(|kind| json!({"type": kind})).collect();
    scopes.extend(retained.iter().cloned());
    json!({
        "environmentId": environment_id,
        "clientId": observation.client_id,
        "clientKind": kind,
        "visible": observation.visible,
        "focused": observation.focused,
        "recentlyInteracted": was_recently_interacted(last_interaction_at, observed_at_ms),
        "appState": if observation.visible { "active" } else { "background" },
        "scopes": scopes,
        "ttlMs": LEASE_TTL_MS,
        "observedAt": iso(observed_at_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interaction_counts_for_as_long_as_a_lease_and_never_from_the_future() {
        assert!(was_recently_interacted(1_000, 1_000));
        assert!(was_recently_interacted(1_000, 46_000));
        assert!(!was_recently_interacted(1_000, 46_001));
        assert!(!was_recently_interacted(2_000, 1_000));
    }

    #[test]
    fn only_remote_refreshing_vcs_status_and_resource_telemetry_hold_a_scope() {
        let status = |input: Value| scope_for_subscription("subscribeVcsStatus", &input);
        assert_eq!(status(json!({"cwd": "/work", "includeRemote": true})), Some(json!({"type": "vcs-status", "cwd": "/work"})));
        assert_eq!(status(json!({"cwd": "/work"})), Some(json!({"type": "vcs-status", "cwd": "/work"})));
        assert_eq!(status(json!({"cwd": "/work", "includeRemote": false})), None);
        assert_eq!(status(json!({"includeRemote": true})), None);
        assert_eq!(scope_for_subscription(RESOURCE_TELEMETRY, &json!({})), Some(json!({"type": "diagnostics"})));
        assert_eq!(scope_for_subscription("subscribeShell", &json!({})), None);
    }

    #[test]
    fn a_scope_lives_until_its_last_stream_ends_and_belongs_to_its_environment() {
        let mut scopes = RetainedScopes::default();
        let work = json!({"type": "vcs-status", "cwd": "/work"});
        let (first, changed) = scopes.retain("env-a", work.clone());
        assert!(changed);
        let (second, changed) = scopes.retain("env-a", work.clone());
        assert!(!changed, "a second stream of the same directory adds nothing");
        assert_eq!(first, second);
        scopes.retain("env-b", json!({"type": "diagnostics"}));
        assert_eq!(scopes.scopes_of("env-a"), vec![work.clone()]);
        assert!(!scopes.release(&first));
        assert_eq!(scopes.scopes_of("env-a"), vec![work]);
        assert!(scopes.release(&first));
        assert!(scopes.scopes_of("env-a").is_empty());
        assert_eq!(scopes.scopes_of("env-b").len(), 1);
    }

    #[test]
    fn a_report_has_the_baseline_scope_the_retained_ones_and_the_window_state() {
        let observation = Observation::parse(&json!({"clientId": "c1", "visible": true, "focused": false, "idleMs": 50_000})).unwrap();
        let value = report("env", &observation, "web", &[json!({"type": "diagnostics"})], 1_760_000_000_000);
        assert_eq!(value["scopes"], json!([{"type": "provider-status"}, {"type": "diagnostics"}]));
        assert_eq!(value["recentlyInteracted"], false);
        assert_eq!(value["appState"], "active");
        assert_eq!(value["ttlMs"], 45_000);
        assert_eq!(value["observedAt"], "2025-10-09T08:53:20.000Z");
        let hidden = Observation { visible: false, idle_ms: Some(10), ..observation };
        let value = report("env", &hidden, "desktop-renderer", &[], 1_760_000_000_000);
        assert_eq!((value["appState"].clone(), value["recentlyInteracted"].clone(), value["clientKind"].clone()), (json!("background"), json!(true), json!("desktop-renderer")));
    }
}
