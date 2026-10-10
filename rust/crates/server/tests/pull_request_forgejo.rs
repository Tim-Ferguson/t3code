//! Forgejo pull requests against `fixtures/pull-request-forgejo.jsonl`, recorded from the original
//! by `rust/tools/generate_pull_request_forgejo_fixtures.mjs`: the provider over a scripted API, as
//! the original's own tests drive it. A scenario answers each request by its method and a fragment
//! of its path; the test asserts on the answers the provider gives and on every request it made.
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use t3_contracts::*;
use t3_server::{
    pull_request_actions::RunAction,
    pull_request_forgejo::{ForgejoApi, ForgejoPullRequests, capabilities},
    pull_request_github::{PrFailure, Reason},
    pull_request_provider::*,
    source_control_forgejo::{ApiInput, ApiOutput, ForgejoCliError, Reason as CliReason},
};

fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/pull-request-forgejo.jsonl")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == kind)
        .collect()
}

/// The scripted API: answers by method and path fragment, first route first, and keeps what it was asked.
struct Scripted {
    routes: Vec<Value>,
    requests: Mutex<Vec<Value>>,
}

impl ForgejoApi for Scripted {
    fn api<'a>(&'a self, input: ApiInput) -> BoxFuture<'a, Result<ApiOutput, ForgejoCliError>> {
        Box::pin(async move {
            let method = input.method.unwrap_or("GET");
            self.requests.lock().unwrap().push(json!({
                "repository": input.target.repository,
                "host": input.target.host,
                "path": input.path,
                "method": method,
                "body": input.body,
            }));
            let route = self.routes.iter().find(|route| route["method"].as_str().unwrap_or(method) == method && input.path.contains(route["match"].as_str().unwrap()));
            let failure = |detail: String, reason: Option<CliReason>| ForgejoCliError { command: "tea", cwd: "/repo".into(), detail, reason, http_status: None, cause: None };
            let Some(route) = route else { return Err(failure(format!("Unexpected Forgejo request: {method} {}", input.path), None)) };
            if let Some(error) = route["error"].as_object() {
                let reason = match error.get("reason").and_then(Value::as_str) {
                    Some("authentication") => Some(CliReason::Authentication),
                    Some("rate-limit") => Some(CliReason::RateLimit),
                    Some("not-found") => Some(CliReason::NotFound),
                    Some("missing-cli") => Some(CliReason::MissingCli),
                    Some("forbidden") => Some(CliReason::Forbidden),
                    _ => None,
                };
                return Err(failure(error["detail"].as_str().unwrap().to_string(), reason));
            }
            Ok(ApiOutput { stdout: route["stdout"].as_str().unwrap_or("").to_string(), stderr: route["stderr"].as_str().unwrap_or("").to_string() })
        })
    }
}

fn subject(input: &Value) -> Subject {
    Subject { cwd: "/repo".into(), host: input["host"].as_str().unwrap_or("forgejo.test").into(), repository: input["repository"].as_str().unwrap_or("").into(), number: input["number"].as_u64().unwrap_or(0) }
}

fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

fn answer<T: serde::Serialize>(result: Result<T, PrFailure>) -> Value {
    match result {
        Ok(value) => json!({"ok": value}),
        Err(failure) => {
            let reason = match failure.reason {
                Reason::MissingTool => "missing-tool",
                Reason::Unauthenticated => "unauthenticated",
                Reason::RateLimited => "rate-limited",
                Reason::NotFound => "not-found",
                Reason::Failed => "failed",
            };
            json!({"error": {"reason": reason, "detail": failure.detail}})
        }
    }
}

async fn call(provider: &ForgejoPullRequests, op: &str, input: &Value) -> Value {
    let s = subject(input);
    match op {
        "viewer" => answer(provider.viewer("/repo", "forgejo.test").await),
        "list" => {
            let request = ListRequest {
                cwd: "/repo".into(),
                host: input["host"].as_str().unwrap().into(),
                repositories: vec![input["repository"].as_str().unwrap().into()],
                state: parse(&input["state"]),
                involvement: parse(&input["involvement"]),
                viewer: input["viewer"].as_str().unwrap().into(),
                limit: input["limit"].as_u64().unwrap() as u32,
                query: input["query"].as_str().map(str::to_string),
                cursor: input["cursor"].is_object().then(|| ProviderCursor { updated_before: input["cursor"]["updatedBefore"].as_str().unwrap().into(), delivered: input["cursor"]["delivered"].as_u64().unwrap() as u32 }),
                filters: None,
            };
            answer(provider.list(&request).await)
        }
        "summary" => answer(provider.summary(&s).await.map(|summary| {
            // Only what the original's summary carries and the wire keeps.
            let mut object = json!({"number": summary.number, "title": summary.title, "url": summary.url, "headBranch": summary.head_branch, "baseBranch": summary.base_branch, "state": summary.state, "updatedAt": summary.updated_at, "closedAt": summary.closed_at, "mergedAt": summary.merged_at});
            let map = object.as_object_mut().unwrap();
            map.insert("isDraft".into(), json!(summary.is_draft.unwrap()));
            map.insert("author".into(), json!(summary.author.unwrap()));
            map.insert("additions".into(), json!(summary.additions.unwrap()));
            map.insert("deletions".into(), json!(summary.deletions.unwrap()));
            map.insert("mergeability".into(), json!(summary.mergeability.unwrap()));
            object
        })),
        "checks" => answer(provider.checks(&s).await),
        "detail" => answer(provider.detail(&s).await),
        "permissions" => answer(provider.viewer_permissions(&s, false).await),
        "activity" => answer(provider.activity(&s).await),
        "diff" => answer(provider.diff(&s, input["cursor"].as_str(), input["commit"].as_str()).await),
        "diffFile" => answer(provider.diff_file_contents(&s, input["commit"].as_str(), parse(&input["changeType"]), input["oldPath"].as_str().unwrap(), input["newPath"].as_str().unwrap()).await),
        "revisions" => {
            let paths: Vec<String> = parse(&input["paths"]);
            answer(provider.file_revisions(&s, &paths).await)
        }
        "action" => {
            let run = RunAction { action: parse(&input["action"]), merge_method: input["mergeMethod"].as_str().map(|method| parse(&json!(method))), update_method: input["updateMethod"].as_str().map(|method| parse(&json!(method))), remove_agent_credits: false };
            answer(provider.run_action(&s, &run).await)
        }
        "update" => answer(provider.update(&s, input["title"].as_str(), input["body"].as_str()).await),
        "comment" => answer(provider.comment(&s, input["body"].as_str().unwrap()).await),
        "updateComment" => answer(provider.update_comment(&s, input["commentId"].as_str().unwrap(), parse(&input["kind"]), input["body"].as_str().unwrap()).await),
        "review" => {
            let comments: Vec<PullRequestReviewCommentDraft> = parse(&input["comments"]);
            answer(provider.submit_review(&s, parse(&input["verdict"]), input["body"].as_str().unwrap(), &comments).await)
        }
        "candidates" => answer(provider.reviewer_candidates(&s).await),
        "request" => {
            let reviewers: Vec<PullRequestReviewerRequestTarget> = parse(&input["reviewers"]);
            answer(provider.set_reviewer_request(&s, &reviewers, input["requested"].as_bool().unwrap()).await)
        }
        "labelCandidates" => answer(provider.label_candidates(&s).await),
        "setLabels" => {
            let labels: Vec<String> = parse(&input["labels"]);
            let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
            answer(provider.set_labels(&s, &labels, input["applied"].as_bool().unwrap()).await)
        }
        "reaction" => answer(provider.set_reaction(&s, input["subjectId"].as_str(), parse(&input["content"]), input["reacted"].as_bool().unwrap()).await),
        "reply" => answer(provider.reply_to_thread(&s, input["threadId"].as_str().unwrap(), input["body"].as_str().unwrap()).await),
        "resolution" => answer(provider.set_thread_resolution(&s, input["threadId"].as_str().unwrap(), input["resolved"].as_bool().unwrap()).await),
        other => panic!("unknown op {other}"),
    }
}

fn sorted(mut requests: Vec<Value>) -> Vec<Value> {
    requests.sort_by_key(Value::to_string);
    requests
}

#[tokio::test]
async fn runs_provider_operations_like_the_original() {
    let scenarios = rows("op");
    assert!(scenarios.len() > 30, "{}", scenarios.len());
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let api = Arc::new(Scripted { routes: scenario["routes"].as_array().unwrap().clone(), requests: Mutex::new(vec![]) });
        let provider = ForgejoPullRequests::with_api(api.clone());
        for (index, planned) in scenario["calls"].as_array().unwrap().iter().enumerate() {
            let op = planned["op"].as_str().unwrap();
            let actual = Box::pin(call(&provider, op, &planned["input"])).await;
            assert_eq!(actual, scenario["results"][index], "{name} call {index} {op}");
            let (seen, expected) = (std::mem::take(&mut *api.requests.lock().unwrap()), scenario["requests"][index].as_array().unwrap().clone());
            if scenario["ordered"] == true {
                assert_eq!(seen, expected, "{name} call {index} {op}: requests");
            } else if scenario["results"][index]["error"].is_object() {
                // Reads run together and the first to fail ends the others, so which of them were
                // started is a race; the one that failed is among them.
                assert!(seen.iter().any(|request| expected.contains(request)), "{name} call {index} {op}: none of {expected:?} in {seen:?}");
            } else {
                assert_eq!(sorted(seen), sorted(expected), "{name} call {index} {op}: requests");
            }
        }
    }
}

#[test]
fn declares_what_the_original_declares() {
    let declared = &rows("declared")[0];
    assert_eq!(serde_json::to_value(capabilities()).unwrap(), declared["capabilities"]);
    assert_eq!(declared["kind"], "forgejo");
    let provider = ForgejoPullRequests::with_api(Arc::new(Scripted { routes: vec![], requests: Mutex::new(vec![]) }));
    assert_eq!(provider.has_checks(), declared["hasChecks"] == true);
    // What the page hides is what the capabilities say is not there.
    let capabilities = capabilities();
    assert!(!capabilities.review.reply && !capabilities.review.resolve && !capabilities.search);
    assert_eq!(capabilities.actions, vec![PullRequestAction::Merge, PullRequestAction::Close, PullRequestAction::Reopen, PullRequestAction::UpdateBranch]);
}
