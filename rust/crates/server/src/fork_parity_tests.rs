//! The handoff builders, the fork planner and the capability checks, against what the original's
//! own code returned for the same inputs (`tests/fixtures/context-handoff.jsonl`, written by
//! `tools/generate_context_handoff_fixtures.mjs`). Ids the original allocates are `<id>`.
use crate::{context_handoff as handoff, context_transfer as transfer, thread_fork};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::HashMap;

fn rows() -> Vec<Value> {
    include_str!("../tests/fixtures/context-handoff.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn now() -> DateTime<Utc> {
    "2026-09-04T00:00:00.000Z".parse().unwrap()
}

fn without_id(mut value: Value) -> Value {
    value["id"] = json!("<id>");
    value
}

fn selection(selected: &handoff::Selection) -> Value {
    json!({"messages":selected.messages,"omittedItemIds":selected.omitted_item_ids,"context":selected.context,"omittedItems":selected.omitted_items})
}

#[test]
fn history_selection_budget_and_rendering_match_the_original() {
    let mut count = 0;
    for row in rows() {
        let name = row["name"].as_str().unwrap_or("");
        match row["kind"].as_str().unwrap() {
            "historical_messages" => {
                let got: Vec<Value> = row["items"].as_array().unwrap().iter().filter_map(handoff::historical_message).collect();
                assert_eq!(Value::Array(got), row["output"], "{name}");
            }
            "select_history" => {
                let messages = row["messages"].as_array().unwrap();
                let selected = handoff::select_history(messages, row["coverage"].as_str().unwrap(), row["omittedItems"].as_u64().unwrap(), row["budget"].as_u64().unwrap());
                let mut got = selection(&selected);
                if row["output"].get("cost").is_some() {
                    got["cost"] = json!(handoff::history_cost(&selected.messages, &selected.context));
                    got["rendered"] = json!(handoff::render_history(&selected.messages, &selected.context));
                    got["responseItems"] = handoff::history_response_items(&selected.messages, &selected.context);
                }
                assert_eq!(got, row["output"], "{name}");
            }
            "coverage" => {
                assert_eq!(handoff::handoff_coverage(row["threadId"].as_str().unwrap(), &row["covered"], row["items"].as_array().unwrap()), row["output"].as_str().unwrap(), "{name}");
            }
            "budget" => {
                let window = row["modelContextWindow"].as_u64();
                let got = handoff::handoff_budget(row["tokenCap"].as_u64().unwrap(), row["userText"].as_str().unwrap(), &row["attachments"], &row["providerThread"], row["nativeContextEstimate"].as_u64().unwrap(), window);
                assert_eq!(json!(got), row["output"], "{name}");
            }
            _ => continue,
        }
        count += 1;
    }
    assert!(count > 60, "{count}");
}

#[test]
fn prepared_handoffs_match_the_original() {
    let mut count = 0;
    for row in rows() {
        let name = row["name"].as_str().unwrap_or("");
        let items = row["items"].as_array();
        match row["kind"].as_str().unwrap() {
            "fork_delta" => {
                let got = handoff::prepare_fork_delta(&handoff::ForkDelta {
                    source_thread_id: "thread:fork",
                    target_thread_id: "thread:source",
                    target_run_id: "run:target",
                    transfer_id: "context-transfer:1",
                    from_provider_thread_ids: vec![json!("provider-thread:1")],
                    to_provider_thread_id: "provider-thread:2",
                    from_provider_instance_id: "codex",
                    to_provider_instance_id: "claudeAgent",
                    covered_run_ordinals: row["covered"].clone(),
                    delta_items: items.unwrap(),
                    created_at: "2026-09-04T00:00:00.000Z",
                });
                assert_eq!(without_id(got), row["output"], "{name}");
            }
            "provider_handoff" => {
                let got = handoff::prepare_provider_handoff(&handoff::ProviderHandoff {
                    thread_id: "thread:source",
                    target_run_id: "run:target",
                    transfer_id: Some("context-transfer:1"),
                    from_provider_thread_ids: vec![json!("provider-thread:1")],
                    to_provider_thread_id: "provider-thread:2",
                    from_provider_instance_id: "codex",
                    to_provider_instance_id: "claudeAgent",
                    covered_run_ordinals: row["covered"].clone(),
                    strategy: row["strategy"].as_str().unwrap(),
                    items: items.unwrap(),
                    runs: row["runs"].as_array().unwrap(),
                    created_at: "2026-09-04T00:00:00.000Z",
                });
                assert_eq!(without_id(got), row["output"], "{name}");
            }
            _ => continue,
        }
        count += 1;
    }
    assert_eq!(count, 15);
}

#[test]
fn fork_plans_match_the_original() {
    let mut count = 0;
    for row in rows().into_iter().filter(|row| row["kind"] == "fork_plan") {
        let name = row["name"].as_str().unwrap();
        let input = &row["input"];
        let source_thread = input["sourceProjection"]["thread"].clone();
        let source_run = input["sourceRun"].clone();
        let mut source = json!({"thread":source_thread,"runs":[source_run],"providerThreads":[],"providerTurns":[],"attempts":[],"checkpoints":[],"contextTransfers":[]});
        if !input["sourceProviderThread"].is_null() {
            source["providerThreads"] = json!([input["sourceProviderThread"]]);
        }
        let mut command = json!({"type":"thread.fork","commandId":"command:fork","sourceThreadId":source_thread["id"],"targetThreadId":input["targetThreadId"],"sourcePoint":{"type":"run","runId":source_run["id"]},"createdBy":input["createdBy"],"creationSource":input["creationSource"],"createdAt":input["createdAt"]});
        if let Some(title) = input.get("title") {
            command["title"] = title.clone();
        }
        let planned = thread_fork::plan_fork(&command, None, Some(&source), now());
        match (&row["output"]["ok"], planned) {
            (Value::Null, Err(refusal)) => assert_eq!(refusal["cause"], row["output"]["error"], "{name}"),
            (ok, Ok(events)) if !ok.is_null() => {
                let created: Vec<Value> = events.iter().map(|event| event.payload.clone()).collect();
                assert_eq!(created[0], ok["targetThread"], "{name}: the new thread");
                let mut got = created[1].clone();
                let mut want = ok["transfer"].clone();
                assert_eq!(got["sourcePoint"]["runId"], want["sourcePoint"]["runId"], "{name}");
                // The canonical point also names the provider thread the fork cuts, which the original
                // test input leaves out.
                for row in [&mut got, &mut want] {
                    row["id"] = json!("<id>");
                    row["sourcePoint"] = Value::Null;
                }
                assert_eq!(got, want, "{name}: the transfer");
            }
            (_, other) => panic!("{name}: {other:?} against {}", row["output"]),
        }
        count += 1;
    }
    assert_eq!(count, 24);
}

#[test]
fn capability_checks_match_the_original() {
    let all = rows();
    let capabilities: HashMap<String, Value> = all.iter().filter(|row| row["kind"] == "capabilities").map(|row| (row["name"].as_str().unwrap().to_owned(), row["capabilities"].clone())).collect();
    let verdict = |result: Result<(), crate::persistence::StoreError>| match result {
        Ok(()) => json!({"ok":null}),
        Err(error) => json!({"error":error.to_string().trim_start_matches("Invalid command: ")}),
    };
    let mut count = 0;
    for row in &all {
        let name = row["name"].as_str().unwrap_or("");
        let caps = row.get("capabilities").and_then(Value::as_str).map(|key| &capabilities[key]);
        let got = match row["kind"].as_str().unwrap() {
            "ensure_context_handoff" => verdict(transfer::ensure_context_handoff(caps.unwrap(), "codex", "command:1", row["strategy"].as_str().unwrap())),
            "ensure_native_fork" => verdict(transfer::ensure_native_fork(caps.unwrap(), "codex", "command:1", row["fromSpecificTurn"] == true)),
            "decide_fork_execution" => match transfer::decide_fork_execution(
                caps.unwrap(),
                "codex",
                "command:1",
                row["sameProvider"] == true,
                row["hasStrongNativeSource"] == true,
                row["sourceRunStatus"].as_str().unwrap(),
                row["fromSpecificTurn"] == true,
            ) {
                Ok(strategy) => json!({"ok":strategy}),
                Err(error) => json!({"error":error.to_string().trim_start_matches("Invalid command: ")}),
            },
            _ => continue,
        };
        assert_eq!(got, row["output"], "{name}");
        count += 1;
    }
    assert_eq!(count, 48 + 32 + 640);
}
