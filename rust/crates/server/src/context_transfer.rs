//! Context transfers resolved when a run is planned: the first message to a fork continues from its
//! source (natively when the provider can fork its own thread, otherwise from a portable handoff),
//! and the next message to a thread with a merge-back waiting carries what the fork did
//! (`Orchestrator.dispatchMessage`, `CommandPolicy`). Provider-agnostic: an adapter is told only
//! which provider thread the run belongs to and which handoffs wait to be delivered with it.
use crate::{context_handoff, execution::at, persistence::{StoreError, read_projection}, thread_fork};
use chrono::{DateTime, Utc};
use rusqlite::Transaction;
use serde_json::{Value, json};

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}

/// What an adapter declares it can do, as the port's capability files state it.
pub(crate) fn capabilities(driver: &str) -> Option<Value> {
    let encoded = match driver {
        "codex" => include_str!("codex-capabilities.json"),
        "claudeAgent" => include_str!("claude-capabilities.json"),
        "acpRegistry" | "antigravity" => include_str!("acp-capabilities.json"),
        "grok" => include_str!("grok-capabilities.json"),
        _ => return None,
    };
    serde_json::from_str(encoded).ok()
}

fn unsupported(instance: &str, command_id: &str, capability: &str, detail: &str) -> StoreError {
    StoreError::InvalidCommand(format!("{instance} cannot satisfy {capability} for command {command_id}: {detail}"))
}

/// `ensureContextHandoff`: the target must take synthetic context, and delta or full history.
pub(crate) fn ensure_context_handoff(capabilities: &Value, instance: &str, command_id: &str, strategy: &str) -> Result<(), StoreError> {
    let context = &capabilities["context"];
    let fail = |detail: &str| Err(unsupported(instance, command_id, "context_handoff", detail));
    if context["canConsumeHandoffSummaries"] != true {
        return fail("providerInstanceId cannot consume handoff summaries");
    }
    if context["acceptsSyntheticUserContext"] != true {
        return fail("providerInstanceId cannot receive synthetic user context");
    }
    if matches!(strategy, "fork_delta_context" | "delta_context") && context["supportsDeltaHandoff"] != true {
        return fail("providerInstanceId does not support delta handoff");
    }
    if strategy == "full_thread_summary" && context["supportsFullThreadHandoff"] != true {
        return fail("providerInstanceId does not support full-thread handoff");
    }
    Ok(())
}

/// `ensureNativeFork`.
pub(crate) fn ensure_native_fork(capabilities: &Value, instance: &str, command_id: &str, from_specific_turn: bool) -> Result<(), StoreError> {
    if capabilities["threads"]["canForkThread"] != true {
        return Err(unsupported(instance, command_id, "native_fork", "providerInstanceId does not support native thread forks"));
    }
    if from_specific_turn && capabilities["threads"]["canForkFromTurn"] != true {
        return Err(unsupported(instance, command_id, "fork_from_turn", "providerInstanceId cannot fork from a specific completed turn"));
    }
    if capabilities["identity"]["nativeThreadIds"] != "strong" {
        return Err(unsupported(instance, command_id, "native_fork", "providerInstanceId does not expose strong native thread ids"));
    }
    Ok(())
}

/// `decideForkExecution`: fork the provider's own thread when it is the same provider, the source
/// finished and the provider can; otherwise carry the conversation as a portable handoff. A failed
/// or interrupted source may have no native turn to cut at, so it is carried as text.
pub(crate) fn decide_fork_execution(capabilities: &Value, instance: &str, command_id: &str, same_provider: bool, strong_native_source: bool, source_run_status: &str, from_specific_turn: bool) -> Result<&'static str, StoreError> {
    let native = matches!(source_run_status, "completed" | "waiting")
        && same_provider
        && strong_native_source
        && capabilities["threads"]["canForkThread"] == true
        && (!from_specific_turn || capabilities["threads"]["canForkFromTurn"] == true)
        && capabilities["identity"]["nativeThreadIds"] == "strong";
    if native {
        return Ok("native_fork");
    }
    ensure_context_handoff(capabilities, instance, command_id, "full_thread_summary")?;
    Ok("portable_context")
}

/// The events and rows a run planned with a transfer adds to the ordinary run.
pub(crate) struct Transfer {
    /// The run's provider thread forks from this native thread.
    pub forked_from: Value,
    /// Handoffs the run's provider thread now owns.
    pub handoff_ids: Vec<Value>,
    /// `Run.contextHandoffId`.
    pub run_handoff_id: Value,
    /// Events before the run is created.
    pub early: Vec<(&'static str, Value)>,
    /// The visible record of the handoff, ordered before the user's message.
    pub handoff_item: Option<Value>,
    /// Events after the user's message.
    pub late: Vec<(&'static str, Value)>,
}

/// A run that starts on another provider than the thread's active provider thread was on: the
/// thread's earlier runs go to the new provider as a handoff.
pub(crate) struct Switch<'a> {
    /// The provider thread the target provider already has for this thread; none when it is new.
    pub target_provider_thread: Option<&'a Value>,
}

pub(crate) struct Planning<'a> {
    /// Set when the run is a provider switch.
    pub switch: Option<Switch<'a>>,
    pub command: &'a Value,
    pub projection: &'a Value,
    pub transaction: Option<&'a Transaction<'a>>,
    pub driver: &'a str,
    pub model: &'a Value,
    pub run_id: &'a str,
    pub node_id: &'a str,
    pub provider_thread_id: &'a str,
    /// The ordinal the handoff item takes; the user's message follows it.
    pub item_ordinal: u64,
    pub now: DateTime<Utc>,
}

fn read(transaction: Option<&Transaction<'_>>, thread_id: &str, missing: &str) -> Result<Value, StoreError> {
    let transaction = transaction.ok_or_else(|| StoreError::InvalidCommand(missing.into()))?;
    read_projection(transaction, "thread", thread_id)?.ok_or_else(|| StoreError::InvalidCommand(missing.into()))
}

/// The unique values of `rows`, by `key`, in first-seen order.
fn unique_by(values: impl Iterator<Item = Value>, key: impl Fn(&Value) -> String) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    values.filter(|value| seen.insert(key(value))).collect()
}

/// Resolves the fork or merge-back transfers a thread waits on for the run being planned. `None`
/// when the thread waits on none.
pub(crate) fn resolve(input: &Planning<'_>) -> Result<Option<Transfer>, StoreError> {
    let projection = input.projection;
    let fork = thread_fork::pending_fork_transfer(projection);
    let merges = thread_fork::pending_merge_back_transfers(projection);
    if fork.is_none() && merges.is_empty() && input.switch.is_none() {
        return Ok(None);
    }
    let thread_id = projection["thread"]["id"].as_str().unwrap_or("");
    let command_id = input.command["commandId"].as_str().unwrap_or("");
    let instance = input.model["instanceId"].as_str().unwrap_or("");
    if unique_by(merges.iter().map(|transfer| transfer["sourceThreadId"].clone()), Value::to_string).len() > 1 {
        return Err(StoreError::InvalidCommand(format!("Thread {thread_id} has pending merge-back transfers from multiple forks.")));
    }
    let merge = thread_fork::latest_context_transfer(&merges);
    let superseded: Vec<Value> = merges.iter().filter(|transfer| Some(**transfer) != merge).map(|transfer| (*transfer).clone()).collect();
    let capabilities = capabilities(input.driver).ok_or_else(|| StoreError::InvalidCommand(format!("Provider instance {instance} has no declared capabilities.")))?;
    let timestamp = at(input.now);
    let mut early: Vec<(&'static str, Value)> = vec![];
    let mut late: Vec<(&'static str, Value)> = vec![];
    let mut forked_from = Value::Null;
    let mut handoff_ids = vec![];
    // --- fork ---
    let mut fork_handoff: Option<Value> = None;
    let mut source_run_row: Option<Value> = None;
    if let Some(transfer) = fork {
        let source = read(input.transaction, transfer["sourceThreadId"].as_str().unwrap_or(""), &format!("Pending fork transfer {} has no resolvable source provider thread.", transfer["id"].as_str().unwrap_or("")))?;
        let source_run = rows(&source, "runs").iter().find(|run| run["id"] == transfer["sourcePoint"]["runId"]).cloned();
        let source_provider_thread = source_run.as_ref().and_then(|run| thread_fork::provider_thread_for_run(&source, run)).cloned();
        let (Some(source_run), Some(source_provider_thread)) = (source_run, source_provider_thread) else {
            return Err(StoreError::InvalidCommand(format!("Pending fork transfer {} has no resolvable source provider thread.", transfer["id"].as_str().unwrap_or(""))));
        };
        if transfer["sourceProviderInstanceId"].is_null() {
            return Err(StoreError::InvalidCommand(format!("Pending fork transfer {} has no source provider.", transfer["id"].as_str().unwrap_or(""))));
        }
        let source_turn_id = (!source_run["activeAttemptId"].is_null())
            .then(|| {
                thread_fork::latest_provider_turn_for_attempt(rows(&source, "providerTurns"), &source_run["activeAttemptId"]).map(|turn| turn["id"].clone()).or_else(|| {
                    rows(&source, "attempts").iter().find(|attempt| attempt["id"] == source_run["activeAttemptId"]).map(|attempt| attempt["providerTurnId"].clone()).filter(|id| !id.is_null())
                })
            })
            .flatten();
        let execution = decide_fork_execution(
            &capabilities,
            instance,
            command_id,
            transfer["sourceProviderInstanceId"] == *instance,
            source_provider_thread["nativeThreadRef"]["strength"] == "strong",
            source_run["status"].as_str().unwrap_or(""),
            true,
        )?;
        if execution == "native_fork" {
            ensure_native_fork(&capabilities, instance, command_id, true)?;
            forked_from = json!({"providerThreadId":source_provider_thread["id"]});
            if let Some(turn) = source_turn_id {
                forked_from["providerTurnId"] = turn;
            }
            let mut pending = transfer.clone();
            pending["targetProviderInstanceId"] = json!(instance);
            pending["targetRunId"] = json!(input.run_id);
            pending["status"] = json!("pending");
            pending["resolution"] = Value::Null;
            pending["error"] = Value::Null;
            pending["updatedAt"] = json!(timestamp);
            early.push(("context-transfer.updated", pending));
        } else {
            let items = handoff_items(&source, rows(&source, "runs").iter().filter(|run| run["ordinal"].as_u64() <= source_run["ordinal"].as_u64()).map(|run| json!(run["id"])).chain((source["thread"]["historyOrigin"] == "v1_import").then_some(Value::Null)).collect::<Vec<_>>().as_slice());
            let handoff = context_handoff::prepare_provider_handoff(&context_handoff::ProviderHandoff {
                thread_id,
                target_run_id: input.run_id,
                transfer_id: transfer["id"].as_str(),
                from_provider_thread_ids: vec![source_provider_thread["id"].clone()],
                to_provider_thread_id: input.provider_thread_id,
                from_provider_instance_id: source_run["providerInstanceId"].as_str().unwrap_or(""),
                to_provider_instance_id: instance,
                covered_run_ordinals: context_handoff::visible_delta_run_ordinals(rows(&source, "runs"), &items),
                strategy: "full_thread_summary",
                items: &items,
                runs: rows(&source, "runs"),
                created_at: &timestamp,
            });
            let mut resolved = transfer.clone();
            resolved["targetProviderInstanceId"] = json!(instance);
            resolved["targetRunId"] = json!(input.run_id);
            resolved["status"] = json!("resolved_portable");
            resolved["resolution"] = json!({"strategy":"portable_context","contextHandoffId":handoff["id"]});
            resolved["error"] = Value::Null;
            resolved["updatedAt"] = json!(timestamp);
            early.push(("context-transfer.updated", resolved.clone()));
            let mut consumed = resolved;
            consumed["status"] = json!("consumed");
            consumed["consumedAt"] = json!(timestamp);
            late.push(("context-transfer.updated", consumed));
            handoff_ids.push(handoff["id"].clone());
            early.push(("context-handoff.updated", handoff.clone()));
            fork_handoff = Some(handoff);
        }
        source_run_row = Some(source_run);
    }
    // --- provider switch ---
    let mut switch_handoff: Option<Value> = None;
    let mut switch_runs: Vec<Value> = vec![];
    if let Some(switch) = &input.switch {
        let runs = rows(projection, "runs");
        let latest = runs.iter().rfind(|run| is_handoff_source_run(run));
        // A merge-back waiting makes the new provider read the whole conversation, not what it missed.
        let full = switch.target_provider_thread.is_none() || merge.is_some();
        let delivered = switch.target_provider_thread.and_then(|thread| last_delivered_run(projection, thread));
        let after = if merge.is_some() { 0 } else { delivered.and_then(|run| run["ordinal"].as_u64()).unwrap_or(0) };
        if let Some(latest) = latest {
            let covered: Vec<Value> = runs.iter().filter(|run| is_handoff_source_run(run) && run["ordinal"].as_u64() > Some(after) && run["ordinal"].as_u64() <= latest["ordinal"].as_u64()).cloned().collect();
            if !covered.is_empty() {
                ensure_context_handoff(&capabilities, instance, command_id, if full { "full_thread_summary" } else { "delta_context" })?;
                let items = handoff_items(projection, &covered.iter().map(|run| run["id"].clone()).collect::<Vec<_>>());
                let id = thread_fork::transfer_id("provider_handoff", thread_id, thread_id);
                let mut from_threads: Vec<Value> = vec![];
                for run in &covered {
                    if !run["providerThreadId"].is_null() && !from_threads.contains(&run["providerThreadId"]) {
                        from_threads.push(run["providerThreadId"].clone());
                    }
                }
                let handoff = context_handoff::prepare_provider_handoff(&context_handoff::ProviderHandoff {
                    thread_id,
                    target_run_id: input.run_id,
                    transfer_id: Some(&id),
                    from_provider_thread_ids: from_threads,
                    to_provider_thread_id: input.provider_thread_id,
                    from_provider_instance_id: latest["providerInstanceId"].as_str().unwrap_or(""),
                    to_provider_instance_id: instance,
                    covered_run_ordinals: json!({"from":covered[0]["ordinal"],"to":covered[covered.len() - 1]["ordinal"]}),
                    strategy: if full { "full_thread_summary" } else { "delta_since_target_last_seen" },
                    items: &items,
                    runs,
                    created_at: &timestamp,
                });
                let transfer = json!({
                    "id":id,"type":"provider_handoff","sourceThreadId":thread_id,"targetThreadId":thread_id,
                    "sourcePoint":thread_fork::source_point_for_run(projection, latest),
                    "basePoint":match (full, delivered) { (false, Some(run)) => thread_fork::source_point_for_run(projection, run), _ => Value::Null },
                    "sourceProviderInstanceId":latest["providerInstanceId"],"targetProviderInstanceId":instance,"targetRunId":input.run_id,
                    "status":"consumed","resolution":{"strategy":if full { "portable_context" } else { "delta_context" },"contextHandoffId":handoff["id"]},
                    "createdBy":input.command["createdBy"],"error":null,"createdAt":timestamp,"updatedAt":timestamp,"consumedAt":timestamp,
                });
                early.push(("context-transfer.created", transfer));
                early.push(("context-handoff.updated", handoff.clone()));
                handoff_ids.push(handoff["id"].clone());
                switch_handoff = Some(handoff);
                switch_runs = covered;
            }
        }
    }
    // --- merge-back ---
    let mut merge_handoff: Option<Value> = None;
    let mut merge_run: Option<Value> = None;
    if let Some(transfer) = merge {
        let id = transfer["id"].as_str().unwrap_or("");
        let source = read(input.transaction, transfer["sourceThreadId"].as_str().unwrap_or(""), &format!("Pending merge-back transfer {id} has no resolvable source run."))?;
        let Some(source_run) = rows(&source, "runs").iter().find(|run| run["id"] == transfer["sourcePoint"]["runId"]).cloned() else {
            return Err(StoreError::InvalidCommand(format!("Pending merge-back transfer {id} has no resolvable source run.")));
        };
        let Some(source_provider_thread) = thread_fork::provider_thread_for_run(&source, &source_run).cloned() else {
            return Err(StoreError::InvalidCommand(format!("Pending merge-back transfer {id} has no resolvable source provider thread.")));
        };
        ensure_context_handoff(&capabilities, instance, command_id, "fork_delta_context")?;
        let items = handoff_items(&source, &rows(&source, "runs").iter().filter(|run| run["ordinal"].as_u64() <= source_run["ordinal"].as_u64()).map(|run| json!(run["id"])).collect::<Vec<_>>());
        let handoff = context_handoff::prepare_fork_delta(&context_handoff::ForkDelta {
            source_thread_id: transfer["sourceThreadId"].as_str().unwrap_or(""),
            target_thread_id: thread_id,
            target_run_id: input.run_id,
            transfer_id: id,
            from_provider_thread_ids: vec![source_provider_thread["id"].clone()],
            to_provider_thread_id: input.provider_thread_id,
            from_provider_instance_id: source_run["providerInstanceId"].as_str().unwrap_or(""),
            to_provider_instance_id: instance,
            covered_run_ordinals: context_handoff::visible_delta_run_ordinals(rows(&source, "runs"), &items),
            delta_items: &items,
            created_at: &timestamp,
        });
        // Unlike a portable fork's or a provider switch's, a merge-back's handoff is not listed on the provider thread.
        early.push(("context-handoff.updated", handoff.clone()));
        merge_handoff = Some(handoff);
        merge_run = Some(source_run);
    }
    for mut row in superseded {
        row["status"] = json!("superseded");
        row["error"] = json!(match merge {
            None => "Superseded while consuming merge-back transfer.".to_owned(),
            Some(latest) => format!("Superseded by merge-back transfer {}.", latest["id"].as_str().unwrap_or("")),
        });
        row["updatedAt"] = json!(timestamp);
        early.push(("context-transfer.updated", row));
    }
    if let (Some(transfer), Some(handoff)) = (merge, &merge_handoff) {
        let mut consumed = transfer.clone();
        consumed["targetProviderInstanceId"] = json!(instance);
        consumed["targetRunId"] = json!(input.run_id);
        consumed["status"] = json!("consumed");
        consumed["resolution"] = json!({"strategy":"fork_delta_context","contextHandoffId":handoff["id"]});
        consumed["error"] = Value::Null;
        consumed["updatedAt"] = json!(timestamp);
        consumed["consumedAt"] = json!(timestamp);
        early.push(("context-transfer.updated", consumed));
    }
    // The handoff the run shows: the portable fork's, else the merge-back's, else the provider switch's.
    let active = fork_handoff.as_ref().or(merge_handoff.as_ref()).or(switch_handoff.as_ref());
    let sources: Vec<&Value> = if fork_handoff.is_some() {
        source_run_row.iter().collect()
    } else if switch_handoff.is_some() {
        switch_runs.iter().collect()
    } else {
        merge_run.iter().collect()
    };
    let handoff_item = active.map(|handoff| {
        let from_instances = unique_by(sources.iter().map(|run| run["providerInstanceId"].clone()), Value::to_string);
        let from_models = unique_by(sources.iter().map(|run| run["modelSelection"].clone()), |model| format!("{}\0{}", model["instanceId"], model["model"]));
        json!({
            "id":crate::execution::run_signal_item_id(input.run_id, &format!("context-handoff:{}", handoff["id"].as_str().unwrap_or(""))),
            "threadId":thread_id,"runId":input.run_id,"nodeId":input.node_id,"providerThreadId":input.provider_thread_id,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,
            "ordinal":input.item_ordinal,"status":"completed","title":if fork_handoff.is_some() { "Fork context" } else if switch_handoff.is_some() { "Provider handoff" } else { "Merge-back context" },
            "startedAt":timestamp,"completedAt":timestamp,"updatedAt":timestamp,"type":"handoff","contextHandoffId":handoff["id"],
            "fromProviderThreadIds":handoff["fromProviderThreadIds"],"toProviderThreadId":handoff["toProviderThreadId"],
            "fromProviderInstanceIds":from_instances,"toProviderInstanceId":instance,"fromModelSelections":from_models,
            "toModel":input.model["model"],"strategy":handoff["strategy"],"summary":handoff["summaryText"],
        })
    });
    let run_handoff_id = active.map_or(Value::Null, |handoff| handoff["id"].clone());
    Ok(Some(Transfer { forked_from, handoff_ids, run_handoff_id, early, handoff_item, late }))
}

/// `isHandoffSourceRun`: failed and interrupted turns still hold conversation the next provider needs;
/// queued, cancelled and rolled-back runs are not replayed.
fn is_handoff_source_run(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted"))
}

/// `lastDeliveredRunForProviderThread`: the newest run the provider thread took, one that finished
/// or at least started a provider turn.
fn last_delivered_run<'a>(projection: &'a Value, provider_thread: &Value) -> Option<&'a Value> {
    rows(projection, "runs").iter().rfind(|run| {
        is_handoff_source_run(run)
            && run["providerThreadId"] == provider_thread["id"]
            && (run["status"] == "completed" || rows(projection, "providerTurns").iter().any(|turn| turn["runAttemptId"] == run["activeAttemptId"]))
    })
}

/// `readHandoffItems`: the turn items of the given runs (`null` is an import's runless history),
/// in the order of the conversation.
pub(crate) fn handoff_items(projection: &Value, run_ids: &[Value]) -> Vec<Value> {
    let mut items: Vec<&Value> = rows(projection, "turnItems").iter().filter(|item| run_ids.contains(&item["runId"])).collect();
    items.sort_by(|left, right| left["ordinal"].as_u64().cmp(&right["ordinal"].as_u64()).then_with(|| left["id"].as_str().cmp(&right["id"].as_str())));
    items.into_iter().cloned().collect()
}

/// The fork transfer waiting for its run's native thread to be forked: planned with a target run
/// and no resolution yet.
pub(crate) fn native_fork_transfer<'a>(projection: &'a Value, run_id: &Value) -> Option<&'a Value> {
    rows(projection, "contextTransfers").iter().find(|transfer| {
        transfer["type"] == "fork" && transfer["targetThreadId"] == projection["thread"]["id"] && transfer["targetRunId"] == *run_id && transfer["status"] == "pending" && transfer["resolution"].is_null()
    })
}

/// The transfer once its native thread exists: consumed, resolved by the provider's own fork.
pub(crate) fn consume_native_fork(transfer: &Value, instance: &str, run_id: &str, native: &Value, now: DateTime<Utc>) -> Value {
    let mut row = transfer.clone();
    row["targetProviderInstanceId"] = json!(instance);
    row["targetRunId"] = json!(run_id);
    row["status"] = json!("consumed");
    row["resolution"] = json!({"strategy":"native_fork","providerThreadRef":native});
    row["error"] = Value::Null;
    row["updatedAt"] = json!(at(now));
    row["consumedAt"] = json!(at(now));
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fork_goes_native_only_for_a_same_provider_source_that_finished_with_a_strong_native_thread() {
        let capabilities = json!({"threads":{"canForkThread":true,"canForkFromTurn":true},"identity":{"nativeThreadIds":"strong"},"context":{}});
        assert_eq!(decide_fork_execution(&capabilities, "codex", "c", true, true, "completed", true).unwrap(), "native_fork");
        assert_eq!(decide_fork_execution(&capabilities, "codex", "c", true, true, "waiting", true).unwrap(), "native_fork");
        for (same, strong, status) in [(false, true, "completed"), (true, false, "completed"), (true, true, "failed"), (true, true, "interrupted")] {
            let refusal = decide_fork_execution(&capabilities, "codex", "c", same, strong, status, true).unwrap_err().to_string();
            assert!(refusal.contains("cannot consume handoff summaries"), "{same} {strong} {status}: {refusal}");
        }
    }

    #[test]
    fn handoff_strategies_need_the_matching_capability() {
        let mut capabilities = json!({"context":{"canConsumeHandoffSummaries":true,"acceptsSyntheticUserContext":true,"supportsDeltaHandoff":true,"supportsFullThreadHandoff":false}});
        assert!(ensure_context_handoff(&capabilities, "codex", "c", "fork_delta_context").is_ok());
        assert!(ensure_context_handoff(&capabilities, "codex", "c", "full_thread_summary").unwrap_err().to_string().contains("does not support full-thread handoff"));
        capabilities["context"]["supportsDeltaHandoff"] = json!(false);
        assert!(ensure_context_handoff(&capabilities, "codex", "c", "delta_context").unwrap_err().to_string().contains("does not support delta handoff"));
        capabilities["context"]["acceptsSyntheticUserContext"] = json!(false);
        assert!(ensure_context_handoff(&capabilities, "codex", "c", "delta_context").unwrap_err().to_string().contains("synthetic user context"));
    }
}
