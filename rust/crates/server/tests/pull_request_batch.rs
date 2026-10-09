//! Summaries read together: the document the original writes for them (fixtures recorded by
//! `rust/tools/generate_pull_request_batch_fixtures.mjs`), and what is sent for reads that arrive
//! at the same moment, for one the batch could not answer, and for a batch that failed. No network.
mod source_control_support;

use serde_json::{Value, json};
use source_control_support::*;
use t3_server::{
    pull_request_batch::build_summaries_query,
    pull_request_github::{GitHub, Target},
};

#[test]
fn the_batch_document_is_the_originals() {
    let rows: Vec<Value> = include_str!("fixtures/pull-request-batch.jsonl").lines().filter(|line| !line.is_empty()).map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(rows.len(), 13);
    for row in rows {
        let requests = row["requests"].as_array().unwrap();
        // A number that is not a positive whole number cannot be asked about at all.
        let lookups: Option<Vec<(&str, u64)>> = requests.iter().map(|request| Some((request["repository"].as_str()?, request["number"].as_u64()?))).collect();
        let actual = lookups.and_then(|lookups| build_summaries_query(&lookups, row["stacks"].as_bool().unwrap()));
        assert_eq!(actual.map(Value::from).unwrap_or(Value::Null), row["query"], "{row}");
    }
}

fn pull_request(number: u64) -> Value {
    json!({"number":number,"title":format!("PR {number}"),"url":format!("https://github.com/acme/web/pull/{number}"),"state":"OPEN","isDraft":false,"mergeable":"MERGEABLE","reviewDecision":null,
        "additions":1,"deletions":1,"changedFiles":1,"updatedAt":"2026-01-02T00:00:00Z","mergedAt":null,"closedAt":null,"headRefName":"h","baseRefName":"main","author":null,
        "latestReviews":{"nodes":[]},"commits":{"nodes":[]},"stack":null})
}

fn answer(numbers: &[Option<u64>]) -> Step {
    let data: serde_json::Map<String, Value> = numbers
        .iter()
        .enumerate()
        .map(|(index, number)| (format!("s{index}"), json!({"pullRequest":number.map(pull_request)})))
        .collect();
    Step::json(200, json!({"data":data}))
}

fn queries_sent(server: &ScriptedServer) -> Vec<String> {
    server.recorded().iter().map(|request| request.body.as_ref().unwrap()["query"].as_str().unwrap().lines().next().unwrap().to_string()).collect()
}

fn target(number: u64) -> Target {
    Target { host: "github.com".into(), repository: "acme/web".into(), number }
}

#[tokio::test]
async fn reads_at_the_same_moment_share_a_request_and_the_rest_are_read_alone() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let cli = FakeCliDir::new();

    // Three at once are one request.
    let server = ScriptedServer::start(vec![answer(&[Some(1), Some(2), Some(3)])]).await;
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(server.github_resolver()), &base);
    let gh = GitHub::new(test.host.clone());
    let (first, second, third) = (target(1), target(2), target(3));
    let (one, two, three) = tokio::join!(gh.summary(&first), gh.summary(&second), gh.summary(&third));
    assert_eq!([one.unwrap().number, two.unwrap().number, three.unwrap().number], [1, 2, 3]);
    assert_eq!(queries_sent(&server), vec!["query PullRequestSummaries {"]);
    let document = server.recorded()[0].body.as_ref().unwrap()["query"].as_str().unwrap().to_string();
    assert!(document.contains("s2: repository(owner: \"acme\", name: \"web\") { pullRequest(number: 3)"), "{document}");

    // One the batch has no answer for is read on its own; the others are not.
    let server = ScriptedServer::start(vec![answer(&[Some(1), None]), answer(&[Some(2)])]).await;
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(server.github_resolver()), &base);
    let gh = GitHub::new(test.host.clone());
    let (first, second) = (target(1), target(2));
    let (one, two) = tokio::join!(gh.summary(&first), gh.summary(&second));
    assert_eq!((one.unwrap().number, two.unwrap().number), (1, 2));
    let sent = queries_sent(&server);
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[0].starts_with("query PullRequestSummaries"));
    assert!(sent[1].starts_with("query($owner"), "the second is the lookup on its own: {sent:?}");
    assert_eq!(server.recorded()[1].body.as_ref().unwrap()["variables"]["number"], 2);

    // A batch GitHub refuses as a whole leaves every read to its own.
    let refused = Step::json(200, json!({"errors":[{"type":"NOT_FOUND","message":"Could not resolve to a Repository"}]}));
    let server = ScriptedServer::start(vec![refused, answer(&[Some(1)]), answer(&[Some(2)])]).await;
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(server.github_resolver()), &base);
    let gh = GitHub::new(test.host.clone());
    let (first, second) = (target(1), target(2));
    let (one, two) = tokio::join!(gh.summary(&first), gh.summary(&second));
    assert_eq!((one.unwrap().number, two.unwrap().number), (1, 2));
    assert_eq!(server.recorded().len(), 3);

    // Different repositories are different documents' worth of aliases, one request; a selector
    // the document cannot carry is read alone.
    let server = ScriptedServer::start(vec![answer(&[Some(1)]), answer(&[Some(9)])]).await;
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(server.github_resolver()), &base);
    let gh = GitHub::new(test.host.clone());
    let odd = Target { host: "github.com".into(), repository: "acme/we b".into(), number: 9 };
    let first = target(1);
    let (one, other) = tokio::join!(gh.summary(&first), gh.summary(&odd));
    assert_eq!((one.unwrap().number, other.unwrap().number), (1, 9));
    let sent = queries_sent(&server);
    assert!(sent[0].starts_with("query PullRequestSummaries") && sent[1].starts_with("query($owner"), "{sent:?}");
}
