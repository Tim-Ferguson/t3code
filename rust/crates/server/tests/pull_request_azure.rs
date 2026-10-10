//! Azure DevOps pull requests against `fixtures/pull-request-azure.jsonl`, recorded from the
//! original by `rust/tools/generate_pull_request_azure_fixtures.mjs`: every decoder of
//! `azureDevOpsPullRequestJson.ts`, the diff builder of `azureDevOpsDiff.ts`, what the provider
//! declares, and the provider over a scripted fake `az`. A scenario answers each request by a
//! fragment of its arguments; the test asserts on the answers the provider gives and on every
//! argument list the fake saw.
mod pull_request_support;
mod source_control_support;

use pull_request_support::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use source_control_support::*;
use t3_contracts::*;
use t3_server::{
    pull_request_actions::RunAction,
    pull_request_azure::{AzurePullRequests, LOCATION_CACHE_CAPACITY, MAX_DIFF_SPAWNS, capabilities, viewer_permissions},
    pull_request_azure_diff as diff,
    pull_request_azure_json::{self as decode, ChangeEntry, ChangeKind},
    pull_request_github::{PrFailure, Reason},
    pull_request_provider::*,
};

fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/pull-request-azure.jsonl")
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
            "decodePullRequestJson" => encoded(decode::decode_pull_request_json(raw)),
            "decodePullRequestListJson" => encoded(decode::decode_pull_request_list_json(raw)),
            "decodeViewerJson" => encoded(decode::decode_viewer_json(raw)),
            "decodeThreadsJson" => encoded(decode::decode_threads_json(raw)),
            "decodeIterationsJson" => encoded(decode::decode_iterations_json(raw)),
            "decodeIterationChangesJson" => encoded(decode::decode_iteration_changes_json(raw)),
            "decodeItemContentJson" => encoded(decode::decode_item_content_json(raw)),
            other => panic!("unknown decoder {other}"),
        };
        assert_eq!(actual, row["result"], "{name}: {raw}");
        seen.insert(name.to_string());
    }
    assert_eq!(seen.len(), 7);
}

// ---- the diff builder ---------------------------------------------------------------------------

/// A text as the generator wrote it: a string, `{repeat: [unit, n]}`, `{parts: [...]}`, or `n`
/// unique lines.
fn text_of(spec: &Value) -> String {
    match spec {
        Value::String(text) => text.clone(),
        Value::Object(object) if object.contains_key("parts") => object["parts"].as_array().unwrap().iter().map(text_of).collect(),
        Value::Object(object) if object.contains_key("repeat") => object["repeat"][0].as_str().unwrap().repeat(object["repeat"][1].as_u64().unwrap() as usize),
        Value::Object(object) => {
            let pad = "z".repeat(object.get("width").and_then(Value::as_u64).unwrap_or(0) as usize);
            let prefix = object["prefix"].as_str().unwrap();
            let lines: Vec<String> = (0..object["lines"].as_u64().unwrap()).map(|line| format!("{prefix} {line} {pad}")).collect();
            format!("{}{}", lines.join("\n"), if object.get("noFinalNewline") == Some(&json!(true)) { "" } else { "\n" })
        }
        other => panic!("unknown text {other}"),
    }
}

fn digest(text: &str) -> Value {
    json!({"length": text.len(), "sha256": Sha256::digest(text.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>()})
}

fn change_of(value: &Value) -> ChangeEntry {
    ChangeEntry {
        path: value["path"].as_str().unwrap().into(),
        old_path: value["oldPath"].as_str().unwrap().into(),
        change_kind: match value["changeKind"].as_str().unwrap() {
            "new" => ChangeKind::New,
            "deleted" => ChangeKind::Deleted,
            "change" => ChangeKind::Change,
            "rename-pure" => ChangeKind::RenamePure,
            _ => ChangeKind::RenameChanged,
        },
        object_id: value["objectId"].as_str().map(str::to_string),
        original_object_id: value["originalObjectId"].as_str().map(str::to_string),
    }
}

#[test]
fn builds_every_file_patch_like_the_original() {
    let all = rows("patch");
    assert!(all.len() > 40, "{}", all.len());
    for row in all {
        let texts = diff::FileTexts { old_contents: text_of(&row["old"]), new_contents: text_of(&row["new"]), binary: row["binary"] == true };
        let patch = diff::file_patch(&change_of(&row["change"]), &texts);
        let actual = json!({
            "section": if patch.section.len() <= 4096 { json!(patch.section) } else { digest(&patch.section) },
            "truncated": patch.truncated,
            "abandoned": patch.abandoned,
            "edits": patch.edits,
        });
        assert_eq!(actual, row["result"], "{} / {}", row["change"], row["old"]);
    }
    for row in rows("unreadable") {
        let patch = diff::unreadable_file_patch(&change_of(&row["change"]));
        assert_eq!(json!({"section": patch.section, "truncated": patch.truncated, "abandoned": patch.abandoned, "edits": patch.edits}), row["result"]);
    }
}

#[test]
fn reads_the_cursor_like_the_original() {
    let all = rows("cursor");
    assert!(all.len() > 20);
    for row in all {
        let actual = diff::parse_cursor(row["raw"].as_str()).map_or(Value::Null, |cursor| json!({"iterationId": cursor.iteration_id, "fileIndex": cursor.file_index}));
        assert_eq!(actual, row["result"], "{}", row["raw"]);
    }
    for row in rows("format") {
        let cursor = diff::DiffCursor { iteration_id: row["cursor"]["iterationId"].as_i64().unwrap(), file_index: row["cursor"]["fileIndex"].as_u64().unwrap() as usize };
        assert_eq!(json!(diff::format_cursor(cursor)), row["result"]);
    }
}

#[test]
fn declares_what_the_original_declares() {
    let declared = &rows("declared")[0];
    assert_eq!(serde_json::to_value(capabilities()).unwrap(), declared["capabilities"]);
    assert_eq!(serde_json::to_value(viewer_permissions()).unwrap(), declared["permissions"]);
    assert_eq!(declared["kind"], "azure-devops");
    assert_eq!((MAX_DIFF_SPAWNS as u64, LOCATION_CACHE_CAPACITY as u64), (declared["spawns"].as_u64().unwrap(), declared["locations"].as_u64().unwrap()));
    assert_eq!(declared["limits"], json!({"files": diff::MAX_DIFF_SLICE_FILES, "bytes": diff::MAX_DIFF_SLICE_BYTES, "edits": diff::MAX_DIFF_SLICE_EDITS, "fileEdits": diff::MAX_FILE_DIFF_EDITS}));
    // What the page hides is what the capabilities say is not there.
    let capabilities = capabilities();
    assert!(!capabilities.comment && !capabilities.search && capabilities.reactions == Some(false));
    assert!(capabilities.review.verdicts.is_empty() && !capabilities.review.reply && !capabilities.review.resolve && !capabilities.review.inline_comment);
    assert!(capabilities.reviewers.request && !capabilities.reviewers.list_candidates);
    assert_eq!(capabilities.edit, Some(PullRequestEditCapabilities { change_request: true, comment: false }));
}

// ---- the provider over a scripted fake az ---------------------------------------------------------

fn subject(input: &Value, cwd: &str) -> Subject {
    Subject { cwd: cwd.into(), host: input["host"].as_str().unwrap_or("dev.azure.com").into(), repository: input["repository"].as_str().unwrap_or("").into(), number: input["number"].as_u64().unwrap_or(0) }
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

async fn call(provider: &AzurePullRequests, op: &str, input: &Value, cwd: &str) -> Value {
    let s = subject(input, cwd);
    match op {
        "viewer" => answer(provider.viewer(cwd, "dev.azure.com").await),
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
        "summary" => answer(provider.summary(&s).await.map(|summary| {
            // Only what the original's summary carries; the rest is absent, not null.
            let mut object = json!({"number": summary.number, "title": summary.title, "url": summary.url, "headBranch": summary.head_branch, "baseBranch": summary.base_branch, "state": summary.state, "updatedAt": summary.updated_at, "closedAt": summary.closed_at, "mergedAt": summary.merged_at});
            let map = object.as_object_mut().unwrap();
            map.insert("isDraft".into(), json!(summary.is_draft.unwrap()));
            map.insert("author".into(), json!(summary.author.unwrap()));
            map.insert("mergeability".into(), json!(summary.mergeability.unwrap()));
            assert!(summary.additions.is_none() && summary.deletions.is_none() && summary.changed_files.is_none() && summary.review_decision.is_none() && summary.checks_state.is_none() && summary.stack.is_none());
            object
        })),
        "preview" => answer(provider.preview(&s).await.map(|preview| {
            json!({"number": preview.number, "title": preview.title, "url": preview.url, "author": preview.author, "state": preview.state, "isDraft": preview.is_draft, "createdAt": preview.created_at})
        })),
        "detail" => answer(provider.detail(&s).await),
        "hasChecks" => json!({"ok": provider.has_checks()}),
        "permissions" => answer(provider.viewer_permissions(&s, false).await),
        "activity" => answer(provider.activity(&s).await),
        "diff" => {
            let result = provider.diff(&s, input["cursor"].as_str(), input["commit"].as_str()).await;
            let mut value = answer(result);
            if let Some(patch) = value["ok"]["patch"].as_str().filter(|patch| patch.len() > 4096).map(digest) {
                value["ok"]["patch"] = patch;
            }
            value
        }
        "diffFile" => answer(provider.diff_file_contents(&s, None, parse(&input["changeType"]), input["oldPath"].as_str().unwrap(), input["newPath"].as_str().unwrap()).await),
        "revisions" => {
            let paths: Vec<String> = parse(&input["paths"]);
            answer(provider.file_revisions(&s, &paths).await)
        }
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
        "review" => {
            let comments: Vec<PullRequestReviewCommentDraft> = parse(&input["comments"]);
            answer(provider.submit_review(&s, parse(&input["verdict"]), input["body"].as_str().unwrap(), &comments).await)
        }
        "reply" => answer(provider.reply_to_thread(&s, input["threadId"].as_str().unwrap(), input["body"].as_str().unwrap()).await),
        "reaction" => answer(provider.set_reaction(&s, input["subjectId"].as_str(), parse(&input["content"]), input["reacted"].as_bool().unwrap()).await),
        "resolution" => answer(provider.set_thread_resolution(&s, input["threadId"].as_str().unwrap(), input["resolved"].as_bool().unwrap()).await),
        "candidates" => answer(provider.reviewer_candidates(&s).await),
        other => panic!("unknown op {other}"),
    }
}

fn sorted(mut calls: Vec<Value>) -> Vec<Value> {
    calls.sort_by_key(Value::to_string);
    calls
}

/// A route whose answer the generator left as a recipe: a file's JSON envelope.
fn expanded(route: &Value) -> Value {
    let mut route = route.clone();
    if let Some(item) = route.get("stdoutItem").filter(|item| item.is_object()).cloned() {
        route["stdout"] = json!(json!({"content": text_of(&item["content"]), "contentMetadata": {"isBinary": item["binary"] == true}}).to_string());
    }
    route
}

#[tokio::test]
async fn runs_provider_operations_like_the_original() {
    let scenarios = rows("op");
    assert!(scenarios.len() > 60, "{}", scenarios.len());
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let routes: Vec<Value> = scenario["routes"].as_array().unwrap().iter().map(expanded).collect();
        let dir = FakeCliDir::new();
        install_az(&dir, &routes);
        let cwd = tempfile::tempdir().unwrap();
        let cwd_text = std::fs::canonicalize(cwd.path()).unwrap().to_string_lossy().into_owned();
        let test = test_host(&[], Some(dir.path()), None, cwd.path());
        let provider = AzurePullRequests::new(test.host.clone());
        for (index, planned) in scenario["calls"].as_array().unwrap().iter().enumerate() {
            let op = planned["op"].as_str().unwrap();
            let actual = Box::pin(call(&provider, op, &planned["input"], &cwd_text)).await;
            assert_eq!(actual, scenario["results"][index], "{name} call {index} {op}");
            // An `az` that cannot be started leaves nothing to look at.
            if routes.iter().all(|route| route["spawn"] == true) {
                continue;
            }
            let (seen, expected) = (take_calls(&dir), scenario["processes"][index].as_array().unwrap().clone());
            if scenario["ordered"] == true {
                assert_eq!(seen, expected, "{name} call {index} {op}: requests");
            } else if scenario["racy"] == true {
                // The original starts the second side of a pair only if the first has not already
                // failed; this port always asks for both. Everything the original asked for was asked.
                assert!(expected.iter().all(|request| seen.contains(request)), "{name} call {index} {op}: {expected:?} not all in {seen:?}");
            } else if scenario["results"][index]["error"].is_object() {
                // Reads run together and the first to fail ends the others. The fake records a call
                // only as it finishes, so whether a sibling is seen is a race; the read that failed
                // is the one thing both made, and it is recorded.
                assert!(seen.iter().any(|request| expected.contains(request)), "{name} call {index} {op}: none of {expected:?} in {seen:?}");
            } else {
                assert_eq!(sorted(seen), sorted(expected), "{name} call {index} {op}: requests");
            }
        }
    }
}
