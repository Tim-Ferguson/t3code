//! Replays the answers recorded from the original decoders (see
//! `tools/generate_pull_request_decode_fixtures.mjs`).
use super::*;
use serde_json::json;

fn value<T: Serialize>(decoded: Decoded<T>) -> Option<Value> {
    decoded.ok().map(|decoded| serde_json::to_value(decoded).unwrap())
}

#[test]
fn every_decoder_matches_the_original_on_recorded_answers() {
    let mut count = 0;
    for line in include_str!("../tests/fixtures/pull-request-decode.jsonl").lines().filter(|line| !line.is_empty()) {
        let row: Value = serde_json::from_str(line).unwrap();
        let raw = row["raw"].as_str().unwrap();
        let actual = match row["decoder"].as_str().unwrap() {
            "core" => value(decode_core(raw)),
            "contexts" => value(decode_check_contexts(raw)),
            "preview" => value(decode_preview(raw)),
            "perms" => value(decode_viewer_permissions(raw)),
            "activity" => value(decode_activity(raw)),
            "threads" => value(decode_review_threads(raw)),
            "dismissals" => decode_review_dismissals(raw)
                .ok()
                .map(|(dismissals, next)| json!({"dismissalsByReviewId":dismissals,"nextCursor":next})),
            "threadComments" => value(decode_review_thread_comments(raw)),
            "files" => value(decode_pull_request_files(raw)),
            "commitFiles" => value(decode_commit_files(raw)),
            "filesViewed" => value(decode_files_viewed(raw)),
            "stats" => decode_stats(raw).ok().map(|stats| {
                json!(stats.into_iter().map(|(index, (additions, deletions))| (index.to_string(), json!({"additions":additions,"deletions":deletions}))).collect::<serde_json::Map<_, _>>())
            }),
            "avatars" => value(decode_actor_avatars(raw)),
            "search" => value(decode_pull_request_search(raw)),
            "list" => value(decode_pull_request_list(raw)),
            "summaries" => decode_summaries(raw).ok().map(|summaries| {
                json!(summaries.into_iter().map(|(index, summary)| (index.to_string(), serde_json::to_value(summary).unwrap())).collect::<serde_json::Map<_, _>>())
            }),
            "stacks" => decode_stacks(raw).ok().map(|stack| serde_json::to_value(stack).unwrap()),
            other => panic!("unknown decoder {other}"),
        };
        let expected = row["ok"].as_bool().unwrap().then(|| row["value"].clone());
        // The recorded stats are keyed by position, which JSON writes as strings.
        let expected = match (row["decoder"].as_str().unwrap(), expected) {
            ("stats", Some(map)) => Some(map),
            (_, expected) => expected,
        };
        assert_eq!(actual, expected, "case {count} ({}): {raw}", row["decoder"]);
        count += 1;
    }
    assert_eq!(count, 74);
}

#[test]
fn check_deduplication_keeps_the_newest_run_in_the_first_position() {
    let checks = to_checks(&[
        json!({"name":"build","status":"COMPLETED","conclusion":"FAILURE","completedAt":"2026-01-01T00:00:00Z"}),
        json!({"name":"lint","status":"COMPLETED","conclusion":"SUCCESS"}),
        json!({"name":"build","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-01-02T00:00:00Z"}),
    ]);
    assert_eq!(checks.iter().map(|check| (check.name.as_str(), check.status)).collect::<Vec<_>>(), vec![
        ("build", PullRequestCheckStatus::Success),
        ("lint", PullRequestCheckStatus::Success),
    ]);
}
