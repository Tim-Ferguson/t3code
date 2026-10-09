//! `SourceControlRateLimit.test.ts` and `githubGraphQlBudget.test.ts`, ported. Time is an
//! argument of both, so a window is crossed by passing a later instant; nothing waits.
use chrono::DateTime;
use t3_contracts::SourceControlProviderKind::{Github, Gitlab};
use t3_server::{
    source_control_github_budget::GraphQlBudget,
    source_control_rate_limit::{RateLimit, retry_at_from_header},
};

const HOST: &str = "github.com";

fn paused_at(limits: &RateLimit, now: i64) -> Option<i64> {
    limits.check(Github, HOST, "", now, false).err().map(|paused| paused.retry_at)
}

#[test]
fn isolates_cooldowns_for_verified_credentials_on_the_same_host() {
    let limits = RateLimit::default();
    limits.record_rate_limit(Github, HOST, "first", 0, None, 0);
    assert!(limits.check(Github, HOST, "second", 0, false).is_ok());
    assert!(limits.check(Github, HOST, "first", 0, false).is_err());
}

#[test]
fn parses_retry_after_seconds_and_http_dates() {
    assert_eq!(retry_at_from_header(Some("120"), 1_000), Some(121_000));
    assert_eq!(retry_at_from_header(Some("Thu, 01 Jan 1970 00:02:01 GMT"), 1_000), Some(121_000));
    assert_eq!(retry_at_from_header(Some("later"), 1_000), None);
}

#[test]
fn backs_off_repeated_rate_limits_until_a_successful_request() {
    let limits = RateLimit::default();
    let first = limits.check(Github, HOST, "", 0, false).unwrap();
    limits.record_rate_limit(Github, HOST, "", first, None, 0);
    let pause = limits.check(Github, HOST, "", 0, false).unwrap_err();
    assert_eq!((pause.provider, pause.host.as_str(), pause.retry_at), (Github, HOST, 30_000));
    assert_eq!(pause.message(), "github requests to github.com are paused until the rate limit resets.");

    let second = limits.check(Github, HOST, "", 30_000, false).unwrap();
    limits.record_rate_limit(Github, HOST, "", second, None, 30_000);
    assert_eq!(paused_at(&limits, 30_000), Some(90_000));

    let success = limits.check(Github, HOST, "", 90_000, false).unwrap();
    limits.record_success(Github, HOST, "", success, 90_000);
    let reset = limits.check(Github, HOST, "", 90_000, false).unwrap();
    limits.record_rate_limit(Github, HOST, "", reset, None, 90_000);
    assert_eq!(paused_at(&limits, 90_000), Some(120_000));
}

#[test]
fn honors_a_provider_reset_time() {
    let limits = RateLimit::default();
    let lease = limits.check(Github, HOST, "", 1_000, false).unwrap();
    limits.record_rate_limit(Github, HOST, "", lease, Some(121_000), 1_000);
    assert_eq!(paused_at(&limits, 1_000), Some(121_000));
    assert_eq!(paused_at(&limits, 120_999), Some(121_000));
    assert_eq!(limits.check(Github, HOST, "", 121_000, false), Ok(1));
}

#[test]
fn lets_an_interactive_request_through_without_clearing_an_active_pause() {
    let limits = RateLimit::default();
    let lease = limits.check(Github, HOST, "", 1_000, false).unwrap();
    limits.record_rate_limit(Github, HOST, "", lease, Some(121_000), 1_000);

    let interactive = limits.check(Github, HOST, "", 1_000, true).unwrap();
    limits.record_success(Github, HOST, "", interactive, 1_000);
    assert_eq!(interactive, 1);
    assert_eq!(paused_at(&limits, 1_000), Some(121_000));

    limits.record_rate_limit(Github, HOST, "", interactive, Some(61_000), 1_000);
    assert_eq!(paused_at(&limits, 1_000), Some(121_000));
}

#[test]
fn keeps_providers_and_hosts_isolated() {
    let limits = RateLimit::default();
    let lease = limits.check(Github, HOST, "", 0, false).unwrap();
    limits.record_rate_limit(Github, HOST, "", lease, None, 0);
    assert_eq!(limits.check(Gitlab, HOST, "", 0, false), Ok(0));
    assert_eq!(limits.check(Github, "github.example.com", "", 0, false), Ok(0));
}

#[test]
fn does_not_let_an_older_success_clear_a_concurrent_pause() {
    let limits = RateLimit::default();
    let first = limits.check(Github, HOST, "", 0, false).unwrap();
    let concurrent = limits.check(Github, HOST, "", 0, false).unwrap();
    limits.record_rate_limit(Github, HOST, "", first, None, 0);
    limits.record_success(Github, HOST, "", concurrent, 0);
    assert_eq!(paused_at(&limits, 0), Some(30_000));
}

#[test]
fn keeps_a_fresh_provider_reset_from_an_older_request() {
    let limits = RateLimit::default();
    let stale = limits.check(Github, HOST, "", 0, false).unwrap();
    limits.record_rate_limit(Github, HOST, "", stale, None, 0);
    limits.record_rate_limit(Github, HOST, "", stale, Some(61_000), 31_000);
    assert_eq!(paused_at(&limits, 31_000), Some(61_000));
}

fn at(text: &str) -> i64 {
    DateTime::parse_from_rfc3339(text).unwrap().timestamp_millis()
}

const RESET_AT: &str = "2026-08-13T14:00:00.000Z";
const NEXT_RESET_AT: &str = "2026-08-13T15:00:00.000Z";
const QUERY: &str = "query { viewer { login } }";

fn rate_limit(remaining: u32, limit: u32, reset_at: &str, cost: u32) -> String {
    format!(
        r#"{{"data":{{"viewer":{{"login":"bilal"}},"rateLimit":{{"cost":{cost},"limit":{limit},"remaining":{remaining},"resetAt":"{reset_at}"}}}}}}"#
    )
}

fn observe(budget: &GraphQlBudget, remaining: u32) {
    budget.observe(HOST, &rate_limit(remaining, 5_000, RESET_AT, 14), "");
}

fn query(budget: &GraphQlBudget, now: i64) -> Result<String, i64> {
    budget.query(HOST, QUERY, "", now, false).map_err(|paused| paused.retry_at)
}

#[test]
fn adds_rate_metadata_to_a_read_query() {
    let budget = GraphQlBudget::default();
    let document = budget.query(HOST, r#"query { repository(owner: "acme", name: "web") { name } }"#, "", 0, false).unwrap();
    assert!(document.contains("rateLimit { cost limit remaining resetAt }"));
    assert!(document.contains(r#"repository(owner: "acme", name: "web") { name }"#));
}

#[test]
fn protects_the_last_ten_percent_with_the_shared_provider_cooldown_error() {
    let budget = GraphQlBudget::default();
    let before = at("2026-08-13T13:30:00.000Z");
    observe(&budget, 500);
    let paused = budget.query(HOST, QUERY, "", before, false).unwrap_err();
    assert_eq!((paused.provider, paused.host.as_str(), paused.retry_at), (Github, HOST, at(RESET_AT)));
    assert_eq!(paused.message(), "github requests to github.com are paused until the rate limit resets.");
    assert!(query(&budget, at("2026-08-13T14:00:01.000Z")).unwrap().contains("rateLimit"));
}

#[test]
fn keeps_hosts_isolated_in_the_budget() {
    let budget = GraphQlBudget::default();
    budget.observe(HOST, &rate_limit(0, 5_000, RESET_AT, 14), "");
    let before = at("2026-08-13T13:30:00.000Z");
    assert!(query(&budget, before).is_err());
    assert!(budget.query("github.example.com", QUERY, "", before, false).unwrap().contains("rateLimit"));
}

#[test]
fn isolates_query_reservations_and_observations_by_credential() {
    let budget = GraphQlBudget::default();
    let before = at("2026-08-13T13:30:00.000Z");
    budget.observe(HOST, &rate_limit(0, 5_000, RESET_AT, 14), "first");
    budget.observe(HOST, &rate_limit(5_000, 5_000, RESET_AT, 14), "second");
    assert!(budget.query(HOST, QUERY, "second", before, false).is_ok());
    let paused = budget.query(HOST, QUERY, "first", before, false).unwrap_err();
    assert_eq!(paused.retry_at, at(RESET_AT));
}

#[test]
fn keeps_the_lower_remaining_value_from_out_of_order_responses() {
    let budget = GraphQlBudget::default();
    observe(&budget, 400);
    observe(&budget, 600);
    assert_eq!(query(&budget, at("2026-08-13T13:30:00.000Z")), Err(at(RESET_AT)));
}

#[test]
fn learns_a_cheaper_observed_cost_without_restoring_reserved_quota() {
    let budget = GraphQlBudget::default();
    let before = at("2026-08-13T13:30:00.000Z");
    budget.observe(HOST, &rate_limit(512, 5_000, RESET_AT, 8), "");
    budget.query(HOST, QUERY, "", before, false).unwrap();
    // The admission reserved eight points, but the completed read only cost one.
    budget.observe(HOST, &rate_limit(511, 5_000, RESET_AT, 1), "");
    for _ in 0..4 {
        assert!(query(&budget, before).unwrap().contains("rateLimit"));
    }
    assert_eq!(query(&budget, before), Err(at(RESET_AT)));
}

#[test]
fn ignores_a_response_from_an_older_reset_window_and_accepts_a_later_one() {
    let budget = GraphQlBudget::default();
    let before = at("2026-08-13T13:30:00.000Z");
    budget.observe(HOST, &rate_limit(400, 5_000, NEXT_RESET_AT, 14), "");
    budget.observe(HOST, &rate_limit(1_000, 5_000, RESET_AT, 14), "");
    assert_eq!(query(&budget, before), Err(at(NEXT_RESET_AT)));

    let later = GraphQlBudget::default();
    observe(&later, 1_000);
    later.observe(HOST, &rate_limit(400, 5_000, NEXT_RESET_AT, 14), "");
    assert_eq!(query(&later, before), Err(at(NEXT_RESET_AT)));
}

#[test]
fn allows_reads_above_the_reserve_and_interactive_reads_into_it() {
    let before = at("2026-08-13T13:30:00.000Z");
    let above = GraphQlBudget::default();
    observe(&above, 515);
    assert!(query(&above, before).is_ok());

    let reserve = GraphQlBudget::default();
    observe(&reserve, 500);
    assert!(reserve.query(HOST, QUERY, "", before, true).unwrap().contains("rateLimit"));

    let admitted = GraphQlBudget::default();
    observe(&admitted, 514);
    assert!(query(&admitted, before).is_ok());
    assert_eq!(query(&admitted, before), Err(at(RESET_AT)));
}

#[test]
fn stops_interactive_reads_when_the_reserve_is_exhausted() {
    let budget = GraphQlBudget::default();
    let before = at("2026-08-13T13:30:00.000Z");
    budget.observe(HOST, &rate_limit(1, 5_000, RESET_AT, 1), "");
    budget.query(HOST, QUERY, "", before, true).unwrap();
    assert_eq!(budget.query(HOST, QUERY, "", before, true).unwrap_err().retry_at, at(RESET_AT));
}

#[test]
fn ignores_malformed_rate_metadata_and_leaves_mutations_alone() {
    let budget = GraphQlBudget::default();
    let before = at("2026-08-13T13:30:00.000Z");
    budget.observe(HOST, "{", "");
    budget.observe(HOST, r#"{"data":{"rateLimit":{"limit":0,"remaining":-1,"resetAt":"never"}}}"#, "");
    assert!(query(&budget, before).unwrap().contains("rateLimit"));

    let mutation = "mutation { addComment(input: {}) { clientMutationId } }";
    observe(&budget, 0);
    assert_eq!(budget.query(HOST, mutation, "", before, false).unwrap(), mutation);
}
