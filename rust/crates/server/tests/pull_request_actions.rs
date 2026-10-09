//! Pull request actions against `fixtures/pull-request-actions.jsonl`, recorded from the original
//! by `rust/tools/generate_pull_request_action_fixtures.mjs`: the decoders and builders, the
//! viewer permissions, every request each action sends (replayed against a scripted local server),
//! the checks a fork's pending workflows add, and the gates that turn an action down before
//! anything is sent. No network, no real host CLI.
#![recursion_limit = "256"]
mod source_control_support;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use source_control_support::*;
use std::sync::{Arc, Mutex};
use t3_contracts::*;
use t3_server::{
    pull_request_actions::*,
    pull_request_decode::{GitHubDetail, GitHubListItem, GitHubViewerAccess},
    pull_request_github::{self as github, GitHub, PrFailure, Reason, Target},
    pull_request_service::{PrError, PrResult},
};

fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/pull-request-actions.jsonl")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == kind)
        .collect()
}

#[test]
fn decoders_builders_and_credit_removal_match_the_original() {
    for row in rows("decode") {
        let raw = row["raw"].as_str().unwrap();
        let actual = match row["decoder"].as_str().unwrap() {
            "decodeReviewerCandidatesJson" => decode_reviewer_candidates(raw).ok().map(|list| serde_json::to_value(list).unwrap()),
            "decodeLabelCandidatesJson" => decode_label_candidates(raw).ok().map(|list| serde_json::to_value(list).unwrap()),
            "decodeWorkflowRunsJson" => decode_workflow_runs(raw).ok().map(|page| {
                json!({"runs":page.runs.iter().map(|run| json!({"id":run.id,"name":run.name,"url":run.url})).collect::<Vec<_>>(),"rawCount":page.raw_count})
            }),
            "decodePullRequestHeadsJson" => decode_pull_request_heads(raw).ok().map(|page| {
                json!({"heads":page.heads.iter().map(|head| {
                    let mut value = json!({"number":head.number,"headSha":head.head_sha,"headRepositoryOwner":head.head_repository_owner});
                    if let Some(cross) = head.is_cross_repository {
                        value["isCrossRepository"] = json!(cross);
                    }
                    value
                }).collect::<Vec<_>>(),"nextCursor":page.next_cursor})
            }),
            other => panic!("unknown decoder {other}"),
        };
        let expected = row["ok"].as_bool().unwrap().then(|| row["value"].clone());
        assert_eq!(actual, expected, "{row}");
    }
    for row in rows("build") {
        let actual = match row["builder"].as_str().unwrap() {
            "buildReviewerRequest" => {
                let targets: Vec<PullRequestReviewerRequestTarget> = serde_json::from_value(row["args"][0].clone()).unwrap();
                build_reviewer_request(&targets)
            }
            "buildLabelRequest" => {
                let labels: Vec<String> = serde_json::from_value(row["args"][0].clone()).unwrap();
                build_label_request(&labels.iter().map(String::as_str).collect::<Vec<_>>())
            }
            other => panic!("unknown builder {other}"),
        };
        assert_eq!(actual, row["value"], "{row}");
    }
    for row in rows("credits") {
        assert_eq!(remove_agent_credits(row["message"].as_str().unwrap()), row["value"].as_str().unwrap(), "{row}");
    }
}

#[test]
fn viewer_permissions_match_the_original() {
    let rows = rows("permissions");
    assert_eq!(rows.len(), 48);
    for row in rows {
        let access = &row["access"];
        let flag = |key: &str| access[key].as_bool().unwrap();
        let merge = &access["mergeCapabilities"];
        let access = GitHubViewerAccess {
            can_write: flag("canWrite"),
            can_triage: flag("canTriage"),
            can_update: flag("canUpdate"),
            did_author: flag("didAuthor"),
            can_update_branch: access["canUpdateBranch"].as_bool(),
            merge_capabilities: PullRequestMergeCapabilities { merge: merge["merge"].as_bool().unwrap(), squash: merge["squash"].as_bool().unwrap(), rebase: merge["rebase"].as_bool().unwrap() },
        };
        assert_eq!(serde_json::to_value(github::viewer_permissions(&access)).unwrap(), row["value"], "{row}");
    }
}

// ---- the requests each action sends ----------------------------------------------------------

/// A scripted step as HTTP: an error the original raised becomes the answer GitHub gives for it.
fn step_of(step: &Value) -> Step {
    match step["error"].as_str() {
        None => Step::from_fixture(step),
        Some("notFound") => Step::json(404, json!({"message":"Not Found"})),
        Some("rateLimit") => Step { status: 429, headers: vec![("retry-after".into(), "60".into())], body: Some("{}".into()) },
        Some("response") => {
            let messages = step["githubErrors"].as_array().cloned().unwrap_or_default();
            let status = step["status"].as_u64().unwrap() as u16;
            if status == 200 {
                Step::json(200, json!({"errors":messages.iter().map(|message| json!({"message":message})).collect::<Vec<_>>()}))
            } else {
                Step::json(status, json!({"message":messages.first().cloned().unwrap_or(Value::Null)}))
            }
        }
        Some(other) => panic!("unknown error {other}"),
    }
}

fn recorded_call(request: &Recorded) -> Value {
    if request.path.starts_with("/graphql/") {
        let body = request.body.clone().unwrap();
        return json!({"kind":"graphql","query":body["query"],"variables":body["variables"]});
    }
    let path = request.path.trim_start_matches("/rest/github.com/");
    let mut call = json!({"kind":"rest","method":request.method,"path":match &request.query { Some(query) => format!("{path}?{query}"), None => path.to_string() }});
    if let Some(body) = &request.body {
        call["body"] = body.clone();
    }
    call
}

fn expected_call(call: &Value) -> Value {
    if call["kind"] == "graphql" {
        return json!({"kind":"graphql","query":call["query"],"variables":call["variables"]});
    }
    let mut expected = json!({"kind":"rest","method":call["method"],"path":call["path"]});
    if let Some(body) = call.get("body") {
        expected["body"] = body.clone();
    }
    expected
}

fn text<'a>(row: &'a Value, key: &str) -> &'a str {
    row["input"][key].as_str().unwrap()
}

async fn replay(row: &Value) {
    let name = row["name"].as_str().unwrap();
    t3_server::pull_request_write::clear_node_ids();
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let server = ScriptedServer::start(row["steps"].as_array().unwrap().iter().map(step_of).collect()).await;
    let cli = FakeCliDir::new();
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(server.github_resolver()), &base);
    let gh = GitHub::new(test.host.clone());
    let target = Target { host: "github.com".into(), repository: text(row, "repository").into(), number: row["input"]["number"].as_u64().unwrap() };
    let input = &row["input"];
    let method = input["mergeMethod"].as_str().map(|method| serde_json::from_value::<PullRequestMergeMethod>(json!(method)).unwrap());
    let update = input["updateMethod"].as_str().map(|method| serde_json::from_value::<PullRequestUpdateMethod>(json!(method)).unwrap());
    let result: Result<Value, PrFailure> = if name.starts_with("action ") {
        let run = RunAction {
            action: serde_json::from_value(input["action"].clone()).unwrap(),
            merge_method: method,
            update_method: update,
            remove_agent_credits: input["removeAgentCreditsOnMerge"].as_bool() == Some(true),
        };
        Box::pin(gh.run_action(&target, &run)).await.map(|()| Value::Null)
    } else if name.starts_with("labels ") {
        let labels: Vec<String> = serde_json::from_value(input["labels"].clone()).unwrap();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        Box::pin(gh.set_labels(&target, &refs, input["applied"].as_bool().unwrap())).await.map(|()| Value::Null)
    } else if name.starts_with("reviewers ") {
        let reviewers: Vec<PullRequestReviewerRequestTarget> = serde_json::from_value(input["reviewers"].clone()).unwrap();
        Box::pin(gh.set_reviewer_request(&target, &reviewers, input["requested"].as_bool().unwrap())).await.map(|()| Value::Null)
    } else if name == "reviewer candidates" || name == "reviewer candidates unreadable" {
        Box::pin(gh.list_reviewer_candidates(&target)).await.map(|list| serde_json::to_value(list).unwrap())
    } else if name.starts_with("label candidates") {
        Box::pin(gh.list_label_candidates(&target)).await.map(|list| serde_json::to_value(list).unwrap())
    } else if name.starts_with("workflow runs") {
        Box::pin(gh.list_workflow_runs_requiring_approval(&target, text(row, "headSha"), text(row, "headBranch"), text(row, "headRepositoryOwner")))
            .await
            .map(|runs| json!(runs.iter().map(|run| json!({"id":run.id,"name":run.name,"url":run.url})).collect::<Vec<_>>()))
    } else {
        panic!("unknown scenario {name}")
    };
    let actual: Vec<Value> = server.recorded().iter().map(recorded_call).collect();
    let expected: Vec<Value> = row["calls"].as_array().unwrap().iter().map(expected_call).collect();
    assert_eq!(actual, expected, "{name}: requests");
    match (&result, row["ok"].as_bool().unwrap()) {
        (Ok(value), true) => assert_eq!(value, if row["value"].is_null() { &Value::Null } else { &row["value"] }, "{name}"),
        (Err(failure), false) => assert_eq!(failure.detail, row["error"]["message"].as_str().unwrap(), "{name}"),
        (other, expected) => panic!("{name}: expected ok={expected}, got {other:?}"),
    }
}

#[test]
fn every_action_sends_what_the_original_sends() {
    let rows = rows("cli");
    assert_eq!(rows.len(), 63);
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
        for row in &rows {
            Box::pin(replay(row)).await;
        }
    });
}

// ---- the checks a fork's pending workflows add --------------------------------------------------

fn detail_of(row: &Value) -> GitHubDetail {
    let detail = &row["detail"];
    let text = |key: &str| detail[key].as_str().map(str::to_string);
    GitHubDetail {
        item: GitHubListItem {
            stack: None,
            author_id: None,
            number: 7,
            title: "T".into(),
            url: "https://github.com/acme/web/pull/7".into(),
            author: None,
            head_branch: text("headBranch").unwrap(),
            base_branch: "main".into(),
            state: serde_json::from_value(detail["state"].clone()).unwrap(),
            is_draft: false,
            mergeability: PullRequestMergeability::Mergeable,
            review_decision: None,
            additions: 0,
            deletions: 0,
            created_at: "2026-07-01T00:00:00Z".into(),
            updated_at: "2026-07-02T00:00:00Z".into(),
            review_request_logins: vec![],
            has_team_review_request: false,
            labels: vec![],
            checks_state: None,
        },
        is_cross_repository: detail["isCrossRepository"].as_bool(),
        head_repository_owner: text("headRepositoryOwner"),
        head_sha: text("headSha"),
        body: String::new(),
        changed_files: 0,
        merged_at: None,
        closed_at: None,
        checks: serde_json::from_value(detail["checks"].clone()).unwrap(),
        auto_merge_enabled: None,
        auto_merge_method: None,
    }
}

#[test]
fn workflow_approvals_join_the_checks_as_the_original_joins_them() {
    let rows = rows("providerChecks");
    assert_eq!(rows.len(), 9);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for row in rows {
            let asked = Arc::new(Mutex::new(0));
            let counter = asked.clone();
            let waiting = row["waiting"].clone();
            let list = move |_sha: String, _branch: String, _owner: String| -> BoxFuture<'static, Result<Vec<WorkflowRun>, PrFailure>> {
                *counter.lock().unwrap() += 1;
                Box::pin(async move {
                    match waiting["error"].as_str() {
                        Some("GitHubApiRateLimitError") => Err(PrFailure { reason: Reason::RateLimited, detail: "GitHub API rate limit exceeded.".into(), retry_at: None }),
                        Some(_) => Err(PrFailure::failed("GitHub returned HTTP 500.")),
                        None => Ok(waiting
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|run| WorkflowRun { id: run["id"].as_u64().unwrap(), name: run["name"].as_str().unwrap().to_string(), url: run["url"].as_str().map(str::to_string) })
                            .collect()),
                    }
                })
            };
            let result = workflow_approvals_with(&detail_of(&row), list).await;
            assert_eq!(*asked.lock().unwrap(), row["askedRuns"].as_u64().unwrap(), "{row}");
            match (result, row["ok"].as_bool().unwrap()) {
                (Ok((checks, required)), true) => {
                    assert_eq!(serde_json::to_value(checks).unwrap(), row["checks"], "{}", row["name"]);
                    assert_eq!(required.map(Value::from).unwrap_or(Value::Null), row["workflowApprovalsRequired"], "{}", row["name"]);
                }
                (Err(failure), false) => assert_eq!(failure.reason, Reason::RateLimited),
                (other, expected) => panic!("{}: expected ok={expected}, got {:?}", row["name"], other.map(|(checks, _)| checks.len())),
            }
        }
    });
}

// ---- the gates ---------------------------------------------------------------------------------

#[test]
fn gates_refuse_before_any_request_exactly_as_the_original_does() {
    let rows = rows("service");
    assert_eq!(rows.len(), 148);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for row in rows {
            let name = row["name"].as_str().unwrap().to_string();
            let capabilities: PullRequestCapabilities = serde_json::from_value(row["capabilities"].clone()).unwrap();
            let viewer: PullRequestViewerPermissions = serde_json::from_value(row["viewerPermissions"].clone()).unwrap();
            let operation = row["method"].as_str().unwrap().to_string();
            let fails = row["failViewer"] == true;
            let asked: Arc<Mutex<Vec<bool>>> = Arc::default();
            let log = asked.clone();
            let fetch = move |include: bool| -> BoxFuture<'static, PrResult<PullRequestViewerPermissions>> {
                log.lock().unwrap().push(include);
                Box::pin(async move { if fails { Err(PrError::operation(&operation, "down")) } else { Ok(viewer) } })
            };
            let mut reference = json!({"projectId":"p1","repository":"acme/web","number":7});
            for (key, value) in row["input"].as_object().unwrap() {
                reference[key] = value.clone();
            }
            let outcome = match row["method"].as_str().unwrap() {
                "runAction" => {
                    let input: PullRequestActionInput = serde_json::from_value(reference).unwrap();
                    authorize_action(&input, &capabilities, Box::new(fetch)).await
                }
                "reviewerCandidates" => authorize_reviewer_candidates(&capabilities, Box::new(fetch)).await,
                "requestReviewers" => authorize_request_reviewers(&capabilities, Box::new(fetch)).await,
                "labelCandidates" => authorize_labels("labelCandidates", &capabilities, Box::new(fetch)).await,
                "setLabels" => authorize_labels("setLabels", &capabilities, Box::new(fetch)).await,
                other => panic!("unknown method {other}"),
            };
            let events = row["events"].as_array().unwrap();
            let asked: Vec<Value> = asked.lock().unwrap().iter().map(|include| json!({"call":"getViewerPermissions","includeUpdateBranch":include})).collect();
            let original_asked: Vec<&Value> = events.iter().filter(|event| event["call"] == "getViewerPermissions").collect();
            assert_eq!(asked.iter().collect::<Vec<_>>(), original_asked, "{name}: the account's permissions are asked for exactly when the original asks");
            let reached = events.len() > original_asked.len();
            match (&outcome, row["ok"].as_bool().unwrap()) {
                (Ok(()), true) => assert!(reached, "{name}: the original reached the provider"),
                (Err(error), false) => {
                    assert!(!reached, "{name}: a refused call sends nothing");
                    let wire = error.wire();
                    assert_eq!(wire["operation"], row["error"]["operation"], "{name}");
                    assert_eq!(wire["detail"], row["error"]["detail"], "{name}");
                }
                (other, expected) => panic!("{name}: expected ok={expected}, got {other:?}"),
            }
            // Whether the merge strips agent credits is the project's choice, else the
            // environment's, and only for a GitHub merge made directly.
            if row["method"] == "runAction" && outcome.is_ok() && row["kind"] == "github" {
                let settings: ServerSettings = serde_json::from_value(row["settings"].clone()).unwrap();
                let action: PullRequestAction = serde_json::from_value(row["input"]["action"].clone()).unwrap();
                let stack = row["input"].get("stackNumber").is_some();
                let sent = events.iter().find(|event| event["call"] == "runAction").unwrap();
                let expected = sent["removeAgentCreditsOnMerge"] == true;
                assert_eq!(removes_agent_credits(Some(&settings), &"p1".parse().unwrap(), action, stack), expected, "{name}");
            }
        }
    });
}

#[test]
fn encoding_matches_encode_uri_component() {
    assert_eq!(encode_uri_component("needs triage"), "needs%20triage");
    assert_eq!(encode_uri_component("a/b&c=d"), "a%2Fb%26c%3Dd");
    assert_eq!(encode_uri_component("keep-_.!~*'()"), "keep-_.!~*'()");
    assert_eq!(encode_uri_component("é"), "%C3%A9");
}
