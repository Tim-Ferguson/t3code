//! GitLab pull requests against `fixtures/pull-request-gitlab.jsonl`, recorded from the original
//! by `rust/tools/generate_pull_request_gitlab_fixtures.mjs`: every decoder of
//! `gitLabMergeRequestJson.ts`, and the provider over a scripted fake `glab`. A scenario answers
//! each request by a fragment of its arguments; the test asserts on the answers the provider
//! gives and on every argument list and stdin the fake saw.
mod pull_request_support;
mod source_control_support;

use serde_json::{Value, json};
use pull_request_support::*;
use source_control_support::*;
use t3_contracts::*;
use t3_server::{
    pull_request_actions::RunAction,
    pull_request_github::{PrFailure, Reason},
    pull_request_gitlab::GitLabPullRequests,
    pull_request_gitlab_json as decode,
    pull_request_provider::*,
};

fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/pull-request-gitlab.jsonl")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == kind)
        .collect()
}

fn encoded<T: serde::Serialize>(result: Result<T, ()>) -> Value {
    match result {
        Ok(value) => json!({"ok": value}),
        Err(()) => json!({"error": true}),
    }
}

#[test]
fn decodes_every_payload_like_the_original() {
    let all = rows("decoder");
    assert!(all.len() > 200, "{}", all.len());
    let mut seen = std::collections::BTreeSet::new();
    for row in all {
        let raw = row["raw"].as_str().unwrap();
        let name = row["decoder"].as_str().unwrap();
        let actual = match name {
            "decodeMergeRequestDetailJson" => encoded(decode::decode_merge_request_detail_json(raw)),
            "decodeMergeRequestListJson" => encoded(decode::decode_merge_request_list_json(raw)),
            "decodeViewerJson" => encoded(decode::decode_viewer_json(raw)),
            "decodeProjectUsersJson" => encoded(decode::decode_project_users_json(raw)),
            "decodeProjectMergeCapabilitiesJson" => encoded(decode::decode_project_merge_capabilities_json(raw)),
            "decodeNotesJson" => encoded(decode::decode_notes_json(raw)),
            "decodeDiscussionsJson" => encoded(decode::decode_discussions_json(raw)),
            "decodeCommitsJson" => encoded(decode::decode_commits_json(raw)),
            "decodeCommitDiffRefsJson" => encoded(decode::decode_commit_diff_refs_json(raw)),
            "decodeDiffRefsJson" => encoded(decode::decode_diff_refs_json(raw)),
            "decodeMergeRequestDiffsJson" => encoded(decode::decode_merge_request_diffs_json(raw)),
            "decodeAwardEmojiJson" => encoded(decode::decode_award_emoji_json(raw)),
            "decodeRepositoryBlobsJson" => encoded(decode::decode_repository_blobs_json(raw)),
            "decodeOwnAwardIdJson" => {
                let content: PullRequestReactionContent = serde_json::from_value(row["args"][0]["content"].clone()).unwrap();
                encoded(decode::decode_own_award_id_json(raw, content, row["args"][0]["viewer"].as_str().unwrap()))
            }
            other => panic!("unknown decoder {other}"),
        };
        assert_eq!(actual, row["result"], "{name}: {raw}");
        seen.insert(name.to_string());
    }
    assert_eq!(seen.len(), 14);
}

// ---- the provider over a scripted fake glab ---------------------------------------------------

fn subject(input: &Value, cwd: &str) -> Subject {
    Subject {
        cwd: cwd.into(),
        host: input["host"].as_str().unwrap_or("gitlab.com").into(),
        repository: input["repository"].as_str().unwrap_or("").into(),
        number: input["number"].as_u64().unwrap_or(0),
    }
}

fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

fn reason_name(reason: Reason) -> &'static str {
    match reason {
        Reason::MissingTool => "missing-tool",
        Reason::Unauthenticated => "unauthenticated",
        Reason::RateLimited => "rate-limited",
        Reason::NotFound => "not-found",
        Reason::Failed => "failed",
    }
}

fn answer<T: serde::Serialize>(result: Result<T, PrFailure>) -> Value {
    match result {
        Ok(value) => json!({"ok": value}),
        Err(failure) => json!({"error": {"reason": reason_name(failure.reason), "detail": failure.detail}}),
    }
}

async fn call(provider: &GitLabPullRequests, op: &str, input: &Value, cwd: &str) -> Value {
    let s = subject(input, cwd);
    match op {
        "viewer" => answer(provider.viewer(cwd, "gitlab.com").await),
        "list" => {
            let request = ListRequest {
                cwd: cwd.into(),
                host: input["host"].as_str().unwrap().into(),
                repositories: vec![input["repository"].as_str().unwrap().into()],
                state: parse(&input["state"]),
                involvement: parse(&input["involvement"]),
                viewer: input["viewer"].as_str().unwrap().into(),
                limit: input["limit"].as_u64().unwrap() as u32,
                query: input["query"].as_str().map(str::to_string),
                cursor: input["cursor"].is_object().then(|| ProviderCursor {
                    updated_before: input["cursor"]["updatedBefore"].as_str().unwrap().into(),
                    delivered: input["cursor"]["delivered"].as_u64().unwrap() as u32,
                }),
                filters: None,
            };
            answer(provider.list(&request).await)
        }
        "checks" => answer(provider.checks(&s).await),
        "detail" => answer(provider.detail(&s).await),
        "activity" => answer(provider.activity(&s).await),
        "permissions" => answer(provider.viewer_permissions(&s, false).await),
        "diff" => answer(provider.diff(&s, input["cursor"].as_str(), input["commit"].as_str()).await),
        "revisions" => {
            let paths: Vec<String> = parse(&input["paths"]);
            answer(provider.file_revisions(&s, &paths).await)
        }
        "candidates" => answer(provider.reviewer_candidates(&s).await),
        "request" => {
            let reviewers: Vec<PullRequestReviewerRequestTarget> = parse(&input["reviewers"]);
            answer(provider.set_reviewer_request(&s, &reviewers, input["requested"].as_bool().unwrap()).await)
        }
        "action" => {
            let run = RunAction {
                action: parse(&input["action"]),
                merge_method: input["mergeMethod"].as_str().map(|method| parse(&json!(method))),
                update_method: input["updateMethod"].as_str().map(|method| parse(&json!(method))),
                remove_agent_credits: false,
            };
            answer(provider.run_action(&s, &run).await)
        }
        "update" => answer(provider.update(&s, input["title"].as_str(), input["body"].as_str()).await),
        "comment" => answer(provider.comment(&s, input["body"].as_str().unwrap()).await),
        "updateComment" => answer(provider.update_comment(&s, input["commentId"].as_str().unwrap(), parse(&input["kind"]), input["body"].as_str().unwrap()).await),
        "review" => {
            let comments: Vec<PullRequestReviewCommentDraft> = parse(&input["comments"]);
            answer(provider.submit_review(&s, parse(&input["verdict"]), input["body"].as_str().unwrap(), &comments).await)
        }
        "reply" => answer(provider.reply_to_thread(&s, input["threadId"].as_str().unwrap(), input["body"].as_str().unwrap()).await),
        "reaction" => answer(provider.set_reaction(&s, input["subjectId"].as_str(), parse(&input["content"]), input["reacted"].as_bool().unwrap()).await),
        "resolution" => answer(provider.set_thread_resolution(&s, input["threadId"].as_str().unwrap(), input["resolved"].as_bool().unwrap()).await),
        other => panic!("unknown op {other}"),
    }
}

fn sorted(mut calls: Vec<Value>) -> Vec<Value> {
    calls.sort_by_key(Value::to_string);
    calls
}

#[tokio::test]
async fn runs_provider_operations_like_the_original() {
    let scenarios = rows("op");
    assert!(scenarios.len() > 50, "{}", scenarios.len());
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let dir = FakeCliDir::new();
        install_glab(&dir, scenario["routes"].as_array().unwrap());
        let cwd = tempfile::tempdir().unwrap();
        let cwd_text = std::fs::canonicalize(cwd.path()).unwrap().to_string_lossy().into_owned();
        let test = test_host(&[], Some(dir.path()), None, cwd.path());
        let provider = GitLabPullRequests::new(test.host.clone());
        for (index, planned) in scenario["calls"].as_array().unwrap().iter().enumerate() {
            let op = planned["op"].as_str().unwrap();
            let actual = call(&provider, op, &planned["input"], &cwd_text).await;
            assert_eq!(actual, scenario["results"][index], "{name} call {index} {op}");
            // A `glab` that cannot be started leaves nothing to look at.
            if scenario["routes"].as_array().unwrap().iter().all(|route| route["spawn"] == true) {
                continue;
            }
            let (seen, expected) = (take_calls(&dir), scenario["processes"][index].as_array().unwrap().clone());
            if scenario["ordered"] == true {
                assert_eq!(seen, expected, "{name} call {index} {op}: requests");
            } else if scenario["results"][index]["error"].is_object() {
                // Two reads run together and the first to fail ends the other, so whether the
                // second was started before that is a race. Every request the original made was made.
                for request in &expected {
                    assert!(seen.contains(request), "{name} call {index} {op}: {request} not in {seen:?}");
                }
            } else {
                assert_eq!(sorted(seen), sorted(expected), "{name} call {index} {op}: requests");
            }
        }
    }
}
