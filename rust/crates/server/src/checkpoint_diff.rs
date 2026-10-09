//! `orchestration.getTurnDiff` and `orchestration.getFullThreadDiff` (port of
//! `checkpointing/CheckpointDiffQuery.ts` and its two handlers in `ws.ts`).
//!
//! A turn count is a run ordinal. Turn 0 is the thread's baseline, so `fromTurnCount: 0` diffs
//! against the root scope's ordinal-0 ref, which exists even before the first checkpoint.
use crate::{
    checkpoint_service::{CheckpointGit, GitFailure, ref_for_scope_ordinal, shared},
    persistence::Store,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use t3_contracts::{
    OrchestrationGetFullThreadDiffInput, OrchestrationGetTurnDiffInput, ThreadTurnDiff,
};
use t3_vcs::{DiffCheckpointsInput, DiffFormat};

pub const METHODS: [&str; 2] = ["orchestration.getTurnDiff", "orchestration.getFullThreadDiff"];
const TURN: &str = "CheckpointDiffQuery.getTurnDiff";
const FULL: &str = "CheckpointDiffQuery.getFullThreadDiff";

/// A typed checkpoint failure: the error's `name` is its wire tag, `message` its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffFailure {
    pub name: String,
    pub message: String,
}
impl From<GitFailure> for DiffFailure {
    fn from(error: GitFailure) -> Self {
        Self {
            name: error.name,
            message: error.message,
        }
    }
}
fn failure(name: &str, message: String) -> DiffFailure {
    DiffFailure {
        name: name.into(),
        message,
    }
}

fn rows<'a>(projection: &'a Value, field: &str) -> &'a [Value] {
    projection[field]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Source: `getCheckpointContext` reads scopes by `(ordinalWithinParent, id)` and checkpoints by
/// `(scopeId, ordinalWithinScope)`, which decides which row `find` returns on a tie.
fn sorted_scopes(projection: &Value) -> Vec<&Value> {
    let mut scopes: Vec<&Value> = rows(projection, "checkpointScopes").iter().collect();
    scopes.sort_by_key(|scope| (scope["ordinalWithinParent"].as_u64().unwrap_or(0), scope["id"].as_str().unwrap_or("")));
    scopes
}
fn sorted_checkpoints(projection: &Value) -> Vec<&Value> {
    let mut checkpoints: Vec<&Value> = rows(projection, "checkpoints").iter().collect();
    checkpoints.sort_by_key(|row| (row["scopeId"].as_str().unwrap_or(""), row["ordinalWithinScope"].as_u64().unwrap_or(0)));
    checkpoints
}

/// `getTurnDiff`. `operation` is the name the failures report; the full-thread diff reaches
/// this with the turn operation, as in the source.
pub(crate) async fn turn_diff<G: CheckpointGit>(
    git: &G,
    projection: Option<&Value>,
    thread_id: &str,
    from: u64,
    to: u64,
    ignore_whitespace: bool,
    operation: &str,
) -> Result<ThreadTurnDiff, DiffFailure> {
    let result = |diff: String| {
        serde_json::from_value::<ThreadTurnDiff>(
            json!({"threadId":thread_id,"fromTurnCount":from,"toTurnCount":to,"diff":diff}),
        )
        .map_err(|_| {
            failure(
                "CheckpointDiffResultInvalidError",
                format!("Checkpoint invariant violation in {operation}: Computed turn diff result does not satisfy contract schema."),
            )
        })
    };
    if from == to {
        return result(String::new());
    }
    let projection = projection.ok_or_else(|| {
        failure(
            "CheckpointThreadNotFoundError",
            format!("Checkpoint invariant violation in {operation}: Thread '{thread_id}' not found."),
        )
    })?;
    let completed: Vec<&Value> = rows(projection, "runs")
        .iter()
        .filter(|run| run["status"] == "completed")
        .collect();
    let ready: Vec<&Value> = sorted_checkpoints(projection)
        .into_iter()
        .filter(|checkpoint| {
            checkpoint["status"] == "ready"
                && checkpoint["appRunOrdinal"].is_u64()
                && completed.iter().any(|run| run["id"] == checkpoint["runId"])
        })
        .collect();
    let available = ready
        .iter()
        .filter_map(|checkpoint| checkpoint["appRunOrdinal"].as_u64())
        .max()
        .unwrap_or(0);
    if to > available {
        return Err(failure(
            "CheckpointTurnRangeUnavailableError",
            format!("Checkpoint unavailable for thread {thread_id} turn {to}: Turn diff range exceeds current turn count: requested {to}, current {available}."),
        ));
    }
    let unavailable = |turn: u64| {
        failure(
            "CheckpointRefUnavailableError",
            format!("Checkpoint unavailable for thread {thread_id} turn {turn}: Checkpoint ref is unavailable for turn {turn}."),
        )
    };
    let to_checkpoint = ready
        .iter()
        .find(|checkpoint| checkpoint["appRunOrdinal"].as_u64() == Some(to))
        .ok_or_else(|| unavailable(to))?;
    let scopes = sorted_scopes(projection);
    let to_scope = scopes
        .iter()
        .find(|scope| scope["id"] == to_checkpoint["scopeId"])
        .ok_or_else(|| {
            let diff = if operation == TURN { "turn diff" } else { "full thread diff" };
            failure(
                "CheckpointWorkspacePathMissingError",
                format!("Checkpoint invariant violation in {operation}: Workspace path missing for thread '{thread_id}' when computing {diff}."),
            )
        })?;
    // The root scope is shared by every run, so ordinal zero is the baseline for any turn count.
    let from_ref = if from == 0 {
        scopes
            .iter()
            .find(|scope| scope["kind"] == "root_run")
            .and_then(|scope| scope["id"].as_str())
            .map(|id| ref_for_scope_ordinal(id, 0))
    } else {
        ready
            .iter()
            .find(|checkpoint| checkpoint["appRunOrdinal"].as_u64() == Some(from))
            .and_then(|checkpoint| checkpoint["ref"].as_str().map(str::to_owned))
    }
    .ok_or_else(|| unavailable(from))?;
    let diff = git
        .diff(&DiffCheckpointsInput {
            cwd: PathBuf::from(to_scope["cwd"].as_str().unwrap_or("")),
            from_checkpoint_ref: from_ref,
            to_checkpoint_ref: to_checkpoint["ref"].as_str().unwrap_or("").to_owned(),
            fallback_from_to_head: false,
            ignore_whitespace,
            format: DiffFormat::Patch,
        })
        .await?;
    result(diff)
}

/// `getFullThreadDiff`: the turn diff from the baseline.
pub(crate) async fn full_thread_diff<G: CheckpointGit>(
    git: &G,
    projection: Option<&Value>,
    thread_id: &str,
    to: u64,
    ignore_whitespace: bool,
) -> Result<ThreadTurnDiff, DiffFailure> {
    if to == 0 {
        return turn_diff(git, projection, thread_id, 0, 0, ignore_whitespace, FULL).await;
    }
    turn_diff(git, projection, thread_id, 0, to, ignore_whitespace, TURN).await
}

fn wire_failure(tag: &str, message: &str, cause: &DiffFailure) -> Value {
    json!({"_tag":tag,"message":message,"cause":{"name":cause.name,"message":cause.message}})
}

/// The two diff RPCs. Each decodes its payload, makes one query and maps a failure to the
/// method's `_tag` envelope. The query reads one projection and runs git; it is not a command.
pub async fn command(store: &Store, tag: &str, payload: Value) -> Result<Value, Value> {
    let decode_error = |error: serde_json::Error| json!({"_tag":"SchemaDecodeError","message":error.to_string()});
    let read = |thread_id: &str| {
        store
            .projection("thread", thread_id)
            .unwrap_or(None)
    };
    let service = shared();
    let (outcome, error_tag, message) = if tag == "orchestration.getTurnDiff" {
        let input: OrchestrationGetTurnDiffInput = serde_json::from_value(payload).map_err(decode_error)?;
        let projection = read(&input.thread_id.as_str());
        (
            turn_diff(service.git(), projection.as_ref(), &input.thread_id.as_str(), input.from_turn_count.0, input.to_turn_count.0, input.ignore_whitespace.unwrap_or(true), TURN).await,
            "OrchestrationGetTurnDiffError",
            "Failed to load turn diff",
        )
    } else {
        let input: OrchestrationGetFullThreadDiffInput = serde_json::from_value(payload).map_err(decode_error)?;
        let projection = read(&input.thread_id.as_str());
        (
            full_thread_diff(service.git(), projection.as_ref(), &input.thread_id.as_str(), input.to_turn_count.0, input.ignore_whitespace.unwrap_or(true)).await,
            "OrchestrationGetFullThreadDiffError",
            "Failed to load full thread diff",
        )
    };
    match outcome {
        Ok(diff) => Ok(serde_json::to_value(diff).expect("typed turn diff")),
        Err(cause) => Err(wire_failure(error_tag, message, &cause)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_service::CheckpointGit;
    use std::{path::Path, sync::Mutex};

    /// Answers every diff with a label of its refs, or fails like a missing repository.
    struct Labels {
        fail: bool,
        calls: Mutex<Vec<Value>>,
    }
    impl CheckpointGit for Labels {
        async fn is_repository(&self, _: &Path) -> bool {
            true
        }
        async fn has_ref(&self, _: &Path, _: &str) -> Result<bool, GitFailure> {
            Ok(true)
        }
        async fn capture(&self, _: &Path, _: &str) -> Result<(), GitFailure> {
            Ok(())
        }
        async fn diff(&self, input: &DiffCheckpointsInput) -> Result<String, GitFailure> {
            self.calls.lock().unwrap().push(json!({"cwd":input.cwd,"fromCheckpointRef":input.from_checkpoint_ref,"toCheckpointRef":input.to_checkpoint_ref,"fallbackFromToHead":input.fallback_from_to_head,"ignoreWhitespace":input.ignore_whitespace}));
            if self.fail {
                return Err(GitFailure { name: "VcsUnsupportedOperationError".into(), message: "VCS operation is unsupported for unknown in CheckpointStore.diffCheckpoints: No supported VCS repository was detected.".into() });
            }
            let last = |text: &str| text.rsplit('/').next().unwrap().to_owned();
            Ok(format!("diff {}..{} ws={}", last(&input.from_checkpoint_ref), last(&input.to_checkpoint_ref), input.ignore_whitespace))
        }
        async fn restore(&self, _: &Path, _: &str) -> Result<bool, GitFailure> {
            Ok(true)
        }
        async fn delete(&self, _: &Path, _: &[String]) -> Result<(), GitFailure> {
            Ok(())
        }
    }

    fn fixtures() -> Vec<Value> {
        include_str!("../tests/fixtures/checkpoint-orchestration.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn refs_and_ids_match_the_original_derivation() {
        let mut count = 0;
        for row in fixtures() {
            match row["operation"].as_str().unwrap() {
                "ids" => {
                    let (scope, ordinal) = (row["scopeId"].as_str().unwrap(), row["ordinal"].as_u64().unwrap());
                    assert_eq!(crate::checkpoint_service::ref_for_scope_ordinal(scope, ordinal), row["output"]["ref"].as_str().unwrap());
                    assert_eq!(crate::checkpoint_service::checkpoint_id(scope, ordinal), row["output"]["checkpointId"].as_str().unwrap());
                    count += 1;
                }
                "rootScope" => {
                    assert_eq!(crate::checkpoint_service::root_scope_id(row["threadId"].as_str().unwrap()), row["output"].as_str().unwrap());
                    count += 1;
                }
                _ => {}
            }
        }
        assert_eq!(count, 18);
    }

    #[tokio::test]
    async fn diff_decisions_and_error_envelopes_match_the_original() {
        let rows = fixtures();
        let projections = rows.iter().find(|row| row["operation"] == "projections").unwrap()["projections"].clone();
        let mut count = 0;
        for row in rows.iter().filter(|row| matches!(row["operation"].as_str(), Some("turnDiff" | "fullDiff"))) {
            let compact = &projections[row["projection"].as_str().unwrap()];
            // The SQL projection also carries the sort keys the compact rows leave out.
            let mut projection = compact.clone();
            for scope in projection["checkpointScopes"].as_array_mut().unwrap() {
                scope["ordinalWithinParent"] = json!(0);
            }
            for checkpoint in projection["checkpoints"].as_array_mut().unwrap() {
                checkpoint["ordinalWithinScope"] = checkpoint["appRunOrdinal"].clone();
            }
            let git = Labels { fail: row["failDiff"] == true, calls: Mutex::new(vec![]) };
            let present = row["missingThread"] != true;
            let input = &row["input"];
            let thread = input["threadId"].as_str().unwrap();
            let ignore = input["ignoreWhitespace"].as_bool().unwrap_or(true);
            let result = if row["operation"] == "turnDiff" {
                turn_diff(&git, present.then_some(&projection), thread, input["fromTurnCount"].as_u64().unwrap(), input["toTurnCount"].as_u64().unwrap(), ignore, TURN).await
            } else {
                full_thread_diff(&git, present.then_some(&projection), thread, input["toTurnCount"].as_u64().unwrap(), ignore).await
            };
            let label = format!("{} {} {}", row["operation"], row["projection"], input);
            match result {
                Ok(diff) => assert_eq!(serde_json::to_value(diff).unwrap(), row["ok"], "{label}"),
                Err(cause) => {
                    assert_eq!(cause.name, row["error"]["tag"].as_str().unwrap(), "{label}");
                    assert_eq!(cause.message, row["error"]["message"].as_str().unwrap(), "{label}");
                    let (tag, message) = if row["operation"] == "turnDiff" { ("OrchestrationGetTurnDiffError", "Failed to load turn diff") } else { ("OrchestrationGetFullThreadDiffError", "Failed to load full thread diff") };
                    assert_eq!(wire_failure(tag, message, &cause), row["wire"], "{label}");
                }
            }
            assert_eq!(json!(*git.calls.lock().unwrap()), row["calls"], "{label}");
            count += 1;
        }
        assert_eq!(count, 731);
    }
}
