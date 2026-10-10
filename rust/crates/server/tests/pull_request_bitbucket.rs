//! Bitbucket pull requests against `fixtures/pull-request-bitbucket.jsonl`, recorded from the
//! original by `rust/tools/generate_pull_request_bitbucket_fixtures.mjs`: every decoder of
//! `bitbucketPullRequestJson.ts`, the patch reader, the capabilities, and the provider over a
//! scripted HTTP host on loopback. A scenario answers each request by the method and a fragment of
//! its url; the test asserts on the answers the provider gives and on every request the host saw.
//! Then the raw request path of the source-control module: credentials, redirects and bounds.
mod pull_request_support;
mod source_control_support;

use pull_request_support::*;
use serde_json::{Value, json};
use source_control_support::*;
use t3_contracts::*;
use t3_server::{
    pull_request_actions::RunAction,
    pull_request_bitbucket::{self as bitbucket, BitbucketPullRequests},
    pull_request_bitbucket_json as decode,
    pull_request_bitbucket_revisions::parse_diff_file_revisions,
    pull_request_github::{PrFailure, Reason},
    pull_request_provider::*,
};

fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/pull-request-bitbucket.jsonl")
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
    assert!(all.len() > 190, "{}", all.len());
    let mut seen = std::collections::BTreeSet::new();
    for row in all {
        let raw = row["raw"].as_str().unwrap();
        let name = row["decoder"].as_str().unwrap();
        let actual = match name {
            "decodePullRequestJson" => encoded(decode::decode_pull_request_json(raw)),
            "decodePullRequestPageJson" => encoded(decode::decode_pull_request_page_json(raw)),
            "decodeViewerJson" => encoded(decode::decode_viewer_json(raw)),
            "decodeRepositoryPermissionJson" => encoded(decode::decode_repository_permission_json(raw)),
            "decodeWorkspaceMembersJson" => encoded(decode::decode_workspace_members_json(raw)),
            "decodeCommentsJson" => encoded(decode::decode_comments_json(raw).map(|comments| {
                json!({"comments": comments.comments, "next": comments.next, "threads": decode::build_review_threads(&comments.entries)})
            })),
            "decodeCommitsJson" => encoded(decode::decode_commits_json(raw)),
            "decodeStatusesJson" => encoded(decode::decode_statuses_json(raw)),
            "decodeDiffstatJson" => encoded(decode::decode_diffstat_json(raw).map(|(stat, next)| json!({"additions": stat.additions, "deletions": stat.deletions, "changedFiles": stat.changed_files, "next": next}))),
            "decodeConflictsJson" => encoded(decode::decode_conflicts_json(raw)),
            other => panic!("unknown decoder {other}"),
        };
        assert_eq!(actual, row["result"], "{name}: {raw}");
        seen.insert(name.to_string());
    }
    assert_eq!(seen.len(), 10);
}

#[test]
fn reads_the_head_version_of_each_file_off_a_patch() {
    let all = rows("revisions");
    assert!(all.len() > 20, "{}", all.len());
    for row in all {
        let patch = row["patch"].as_str().unwrap();
        assert_eq!(json!(parse_diff_file_revisions(patch)), row["result"], "{patch:?}");
    }
}

#[test]
fn declares_what_bitbucket_can_do_and_nothing_else() {
    let row = &rows("capabilities")[0];
    assert_eq!(serde_json::to_value(bitbucket::capabilities()).unwrap(), row["capabilities"]);
    assert_eq!(serde_json::to_value(bitbucket::viewer_permissions(true)).unwrap(), row["permissions"]["write"]);
    assert_eq!(serde_json::to_value(bitbucket::viewer_permissions(false)).unwrap(), row["permissions"]["read"]);
}

// ---- the provider over a scripted host --------------------------------------------------------

fn subject(input: &Value, cwd: &str) -> Subject {
    Subject {
        cwd: cwd.into(),
        host: input["host"].as_str().unwrap_or("bitbucket.org").into(),
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
        Err(failure) => {
            let mut error = json!({"reason": reason_name(failure.reason), "detail": failure.detail});
            if let Some(at) = failure.retry_at {
                error["retryAt"] = json!(at);
            }
            json!({"error": error})
        }
    }
}

async fn call(provider: &BitbucketPullRequests, op: &str, input: &Value, cwd: &str) -> Value {
    let s = subject(input, cwd);
    match op {
        "viewer" => answer(provider.viewer(cwd, "bitbucket.org").await),
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
                update_method: None,
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

/// A request as the original's mock recorded it, with the body read as JSON.
fn expected_request(request: &Value) -> Value {
    json!({
        "method": request["method"],
        "url": request["url"],
        "body": request["body"].as_str().map_or(Value::Null, |body| serde_json::from_str(body).unwrap_or(Value::Null)),
    })
}

fn sorted(mut calls: Vec<Value>) -> Vec<Value> {
    calls.sort_by_key(Value::to_string);
    calls
}

/// A patch the server padded past the byte ceiling comes back cut at it; the test compares the part
/// the fixture states, and that nothing but padding followed.
fn unpadded(mut result: Value, expected: &Value) -> Value {
    if let (Some(patch), Some(wanted)) = (result["ok"]["patch"].as_str(), expected["ok"]["patch"].as_str())
        && patch.len() > wanted.len()
        && patch.starts_with(wanted)
        && patch[wanted.len()..].bytes().all(|byte| byte == b'\n')
    {
        result["ok"]["patch"] = json!(wanted);
    }
    result
}

#[tokio::test]
async fn runs_provider_operations_like_the_original() {
    let scenarios = rows("op");
    assert!(scenarios.len() > 85, "{}", scenarios.len());
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let server = RouteServer::start(scenario["routes"].as_array().unwrap()).await;
        let cwd = tempfile::tempdir().unwrap();
        let cwd_text = std::fs::canonicalize(cwd.path()).unwrap().to_string_lossy().into_owned();
        let test = test_host(&[("T3CODE_BITBUCKET_API_BASE_URL", server.base.as_str()), ("T3CODE_BITBUCKET_ACCESS_TOKEN", "fixture-token")], None, None, cwd.path());
        let provider = BitbucketPullRequests::new(test.host.clone());
        for (index, planned) in scenario["calls"].as_array().unwrap().iter().enumerate() {
            let op = planned["op"].as_str().unwrap();
            // `complete: false` is how the original says a read did not see the whole patch; the Rust
            // type leaves the field out until it is true.
            let mut wanted = scenario["results"][index].clone();
            if wanted["ok"]["complete"] == false {
                wanted["ok"].as_object_mut().unwrap().remove("complete");
            }
            let actual = unpadded(call(&provider, op, &planned["input"], &cwd_text).await, &wanted);
            assert_eq!(actual, wanted, "{name} call {index} {op}");
            let seen = server.take();
            let expected: Vec<Value> = scenario["requests"][index].as_array().unwrap().iter().map(expected_request).collect();
            if scenario["ordered"] == true {
                assert_eq!(seen, expected, "{name} call {index} {op}: requests");
            } else if scenario["results"][index]["error"].is_object() {
                // Reads that run together end each other when one fails, so whether a sibling reached
                // the host is a race, and the original interrupts one before it starts where this one
                // may already have sent it. The read that failed is the one thing both made.
                assert!(seen.iter().any(|request| expected.contains(request)), "{name} call {index} {op}: none of {expected:?} in {seen:?}");
            } else {
                assert_eq!(sorted(seen), sorted(expected), "{name} call {index} {op}: requests");
            }
        }
    }
}

// ---- the raw request path ---------------------------------------------------------------------

use t3_server::source_control_bitbucket::BitbucketApi;

fn api_for(base: &str, env: &[(&str, &str)]) -> (BitbucketApi, tempfile::TempDir, TestHost) {
    let cwd = tempfile::tempdir().unwrap();
    let mut vars = vec![("T3CODE_BITBUCKET_API_BASE_URL", base)];
    vars.extend_from_slice(env);
    let test = test_host(&vars, None, None, cwd.path());
    (BitbucketApi::new(test.host.clone()), cwd, test)
}

#[tokio::test]
async fn sends_the_credential_to_the_configured_host_only() {
    let server = RouteServer::start(&[json!({"match": "/repositories", "body": "hello"}), json!({"match": "/redirect", "status": 302, "location": "{origin}/repositories/x"})]).await;
    let (api, _cwd, _test) = api_for(&server.base, &[("T3CODE_BITBUCKET_ACCESS_TOKEN", "secret-token")]);
    let get = |url: String| {
        let api = &api;
        async move { api.request(reqwest::Method::GET, &url, None, None).await }
    };
    // a path is below the base; a whole url is taken only if it is the base's own origin
    assert_eq!(get("/repositories/a".into()).await.unwrap().body, "hello");
    assert_eq!(get(format!("{}/repositories/b", server.base)).await.unwrap().body, "hello");
    assert_eq!(server.authorizations().iter().map(|(_, header)| header.clone()).collect::<Vec<_>>(), vec![Some("Bearer secret-token".to_string()); 2]);
    for foreign in ["https://evil.example/2.0/repositories/a", "http://127.0.0.1:1/2.0/x"] {
        let error = get(foreign.into()).await.unwrap_err();
        assert_eq!(error.tag, "BitbucketUntrustedUrlError", "{foreign}");
        assert!(!error.message.contains("secret-token") && !error.message.contains("/2.0/"), "only the host is named: {}", error.message);
    }
    assert_eq!(server.authorizations().len(), 2, "nothing was sent to a url outside the configured host");
    // an API token travels as basic credentials
    let (basic, _cwd, _test) = api_for(&server.base, &[("T3CODE_BITBUCKET_EMAIL", "me@example.com"), ("T3CODE_BITBUCKET_API_TOKEN", "api-secret")]);
    basic.request(reqwest::Method::GET, "/repositories/c", None, None).await.unwrap();
    let header = server.authorizations().pop().unwrap().1.unwrap();
    assert!(header.starts_with("Basic "), "{header}");
    // no credential, no header
    let (anonymous, _cwd, _test) = api_for(&server.base, &[]);
    anonymous.request(reqwest::Method::GET, "/repositories/d", None, None).await.unwrap();
    assert_eq!(server.authorizations().pop().unwrap().1, None);
}

#[tokio::test]
async fn follows_a_redirect_back_to_the_same_host_and_refuses_one_that_leaves() {
    let mut routes = vec![
        json!({"match": "/to-same", "status": 302, "location": "/2.0/final?x=1"}),
        json!({"match": "/to-absolute", "status": 301, "location": "{origin}/final"}),
        json!({"match": "/to-elsewhere", "status": 302, "location": "https://evil.example/steal"}),
        json!({"match": "/to-other-port", "status": 302, "location": "http://127.0.0.1:1/steal"}),
        json!({"match": "/loop", "status": 302, "location": "/2.0/loop"}),
        json!({"match": "/final", "body": "arrived"}),
    ];
    let server = RouteServer::start(&routes).await;
    let (api, _cwd, _test) = api_for(&server.base, &[("T3CODE_BITBUCKET_ACCESS_TOKEN", "secret-token")]);
    assert_eq!(api.request(reqwest::Method::GET, "/to-same", None, None).await.unwrap().body, "arrived");
    assert_eq!(api.request(reqwest::Method::GET, "/to-absolute", None, None).await.unwrap().body, "arrived");
    for target in ["/to-elsewhere", "/to-other-port"] {
        let error = api.request(reqwest::Method::GET, target, None, None).await.unwrap_err();
        assert_eq!(error.tag, "BitbucketUntrustedUrlError", "{target}");
        assert!(!error.message.contains("secret-token") && !error.message.contains("steal"), "{}", error.message);
    }
    // a chain is followed three times and then the redirect itself is the answer
    let error = api.request(reqwest::Method::GET, "/loop", None, None).await.unwrap_err();
    assert_eq!((error.tag, error.status), ("BitbucketResponseError", Some(302)));
    assert_eq!(server.take().iter().filter(|request| request["url"] == "/loop").count(), 4);
    // a body travels again with the redirected request
    routes.push(json!({"method": "POST", "match": "/final", "body": "posted"}));
    server.replace_routes(&routes);
    let sent = api.request(reqwest::Method::POST, "/to-same", Some(r#"{"a":1}"#), None).await;
    assert!(sent.is_ok());
    assert!(server.take().iter().any(|request| request["method"] == "POST" && request["url"] == "/final?x=1" && request["body"] == json!({"a": 1})));
}

#[tokio::test]
async fn bounds_the_body_and_reports_failures_without_the_credential() {
    let server = RouteServer::start(&[
        json!({"match": "/big", "body": "x", "truncated": true}),
        json!({"match": "/limited", "status": 429, "retryAfter": 30}),
        json!({"match": "/missing", "status": 404}),
        json!({"match": "/gone", "error": "request"}),
        json!({"match": "/ok", "body": "0123456789"}),
    ])
    .await;
    let (api, _cwd, _test) = api_for(&server.base, &[("T3CODE_BITBUCKET_ACCESS_TOKEN", "secret-token")]);
    let response = api.request(reqwest::Method::GET, "/big", None, Some(1024)).await.unwrap();
    assert_eq!((response.body.len(), response.truncated), (1024, true));
    let exact = api.request(reqwest::Method::GET, "/ok", None, Some(10)).await.unwrap();
    assert_eq!((exact.body.as_str(), exact.truncated), ("0123456789", false));
    let cut = api.request(reqwest::Method::GET, "/ok", None, Some(4)).await.unwrap();
    assert_eq!((cut.body.as_str(), cut.truncated), ("0123", true));
    let limited = api.request(reqwest::Method::GET, "/limited", None, None).await.unwrap_err();
    assert_eq!((limited.tag, limited.status, limited.retry_at), ("BitbucketResponseError", Some(429), Some(NOW_MS + 30_000)));
    assert_eq!(limited.message, "Bitbucket API failed in request: Bitbucket returned HTTP 429.");
    let missing = api.request(reqwest::Method::GET, "/missing", None, None).await.unwrap_err();
    assert_eq!((missing.status, missing.detail()), (Some(404), "Bitbucket returned HTTP 404."));
    let gone = api.request(reqwest::Method::GET, "/gone", None, None).await.unwrap_err();
    assert_eq!((gone.tag, gone.message.as_str()), ("BitbucketRequestError", "Bitbucket API failed in request: Failed to send the Bitbucket request."));
    for error in [&limited, &missing, &gone] {
        assert!(!format!("{error:?}").contains("secret-token"));
    }
}

#[test]
fn a_credential_never_prints() {
    use t3_server::source_control_bitbucket::BitbucketCredential;
    for credential in [
        BitbucketCredential::AccessToken("secret-token".into()),
        BitbucketCredential::ApiToken { email: "me@example.com".into(), api_token: "api-secret".into() },
    ] {
        let printed = format!("{credential:?}");
        assert!(!printed.contains("secret") && !printed.contains("me@example.com"), "{printed}");
    }
}

#[tokio::test]
async fn reads_the_patch_once_for_a_run_of_ticks_and_afresh_once_it_has_aged_out() {
    let server = RouteServer::start(&[json!({"match": "/diff", "body": "diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n@@ -1 +1 @@\n"})]).await;
    let (_api, _cwd, test) = api_for(&server.base, &[("T3CODE_BITBUCKET_ACCESS_TOKEN", "fixture-token")]);
    let provider = BitbucketPullRequests::new(test.host.clone());
    let subject = Subject { cwd: "/repo".into(), host: "bitbucket.org".into(), repository: "acme/web".into(), number: 7 };
    let paths = ["a.ts".to_string()];
    let revisions = || async { provider.file_revisions(&subject, &paths).await.unwrap().revisions };
    assert_eq!(revisions().await["a.ts"], "2222222");
    assert_eq!(server.take().len(), 1);
    // a run of ticks shares one read, and a patch for another pull request is another read
    assert_eq!(revisions().await["a.ts"], "2222222");
    test.clock.advance(std::time::Duration::from_secs(4));
    assert_eq!(revisions().await["a.ts"], "2222222");
    assert!(server.take().is_empty(), "the patch is held for five seconds");
    test.clock.advance(std::time::Duration::from_secs(2));
    assert_eq!(revisions().await["a.ts"], "2222222");
    assert_eq!(server.take().len(), 1, "past five seconds the next tick reaches Bitbucket");
    let other = Subject { number: 8, ..subject.clone() };
    provider.file_revisions(&other, &paths).await.unwrap();
    assert_eq!(server.take().len(), 1, "another pull request is another patch");
}
