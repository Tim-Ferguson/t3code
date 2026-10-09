//! What the thread projection says each turn changed, and the diff request a selection means.
//! `deriveThreadCheckpointSummaries` plus the selection rules of `DiffPanel.tsx`.
use serde_json::{Value, json};

pub const TURN_DIFF: &str = "orchestration.getTurnDiff";
pub const FULL_THREAD_DIFF: &str = "orchestration.getFullThreadDiff";

/// One entry of a checkpoint's `files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub kind: String,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
}

impl ChangedFile {
    pub fn from_value(file: &Value) -> Option<Self> {
        Some(Self {
            path: file["path"].as_str().or_else(|| file["fileName"].as_str())?.to_owned(),
            kind: file["kind"].as_str().unwrap_or("modified").to_owned(),
            additions: file["additions"].as_u64(),
            deletions: file["deletions"].as_u64(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ready,
    /// The project is not a git repository, so there is nothing to snapshot.
    Missing,
    Error,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub run_id: String,
    /// The run ordinal the diff RPCs call a turn count.
    pub ordinal: u64,
    pub status: Status,
    /// A checkpoint exists a moment before its run settles, and only a completed run has a diff.
    pub run_completed: bool,
    pub files: Vec<ChangedFile>,
    pub captured_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Turns {
    /// The projection of this thread has arrived; before that nothing can be said about turns.
    pub loaded: bool,
    /// Newest first.
    pub turns: Vec<TurnSummary>,
}

/// Turns with a checkpoint, newest first. Cheap enough to run on every thread event: it reads
/// two small arrays and the caller only reacts when the result changes.
pub fn summarize(projection: Option<&Value>, thread_id: &str) -> Turns {
    let Some(projection) = projection.filter(|projection| projection["thread"]["id"] == thread_id) else {
        return Turns::default();
    };
    let runs = projection["runs"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut turns: Vec<TurnSummary> = Vec::new();
    for checkpoint in projection["checkpoints"].as_array().into_iter().flatten() {
        let (Some(run_id), Some(ordinal)) = (checkpoint["runId"].as_str(), checkpoint["appRunOrdinal"].as_u64()) else {
            continue;
        };
        // A rewound turn is no longer part of the conversation: it has no diff to offer, and the
        // server refuses to produce one (`CheckpointRefUnavailableError`).
        if runs.iter().any(|run| run["id"] == run_id && run["status"] == "rolled_back") {
            continue;
        }
        let status = match checkpoint["status"].as_str() {
            Some("ready") => Status::Ready,
            Some("missing") => Status::Missing,
            Some("error") => Status::Error,
            _ => Status::Stale,
        };
        let summary = TurnSummary {
            run_id: run_id.to_owned(),
            ordinal,
            status,
            run_completed: runs.iter().any(|run| run["id"] == run_id && run["status"] == "completed"),
            files: checkpoint["files"].as_array().into_iter().flatten().filter_map(ChangedFile::from_value).collect(),
            captured_at: checkpoint["capturedAt"].as_str().unwrap_or_default().to_owned(),
        };
        // A run has one checkpoint here; if a replay delivers two, the later row wins.
        match turns.iter_mut().find(|turn| turn.run_id == summary.run_id) {
            Some(existing) => *existing = summary,
            None => turns.push(summary),
        }
    }
    turns.sort_by(|a, b| b.ordinal.cmp(&a.ordinal).then_with(|| b.captured_at.cmp(&a.captured_at)));
    Turns { loaded: true, turns }
}

/// What the diff view is asked to show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Scope {
    /// "Changes": the checkout's branch against its base. The original's default scope.
    #[default]
    Changes,
    /// "Uncommitted": the working tree.
    Uncommitted,
    /// The newest turn. The panel pins it to a turn as soon as it knows one, so a diff does not
    /// change under the reader when another turn finishes.
    Latest,
    Turn(String),
    /// Everything since the thread began.
    Thread,
}

/// A request for the server, with the identity that makes a late answer recognizable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: &'static str,
    pub payload: Value,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The scope reads the checkout (`review.getDiffPreview`), not a checkpoint.
    Review,
    Loading,
    NoTurns,
    NotGit,
    CaptureFailed,
    Waiting,
    Fetch(Request),
}

impl Plan {
    /// The panel's text for a plan with nothing to fetch, in the original's words where it has them.
    pub fn message(&self) -> Option<&'static str> {
        match self {
            Plan::Loading => Some("Loading thread…"),
            Plan::NoTurns => Some("No completed turns yet."),
            Plan::NotGit => Some("Turn diffs are unavailable because this project is not a git repository."),
            Plan::CaptureFailed => Some("Turn diffs are unavailable because the checkpoint for this turn could not be captured."),
            Plan::Waiting => Some("Waiting for this turn to finish…"),
            Plan::Fetch(_) | Plan::Review => None,
        }
    }
}

impl Turns {
    pub fn latest(&self) -> Option<&TurnSummary> {
        self.turns.first()
    }

    /// The turn a scope names. A turn that is gone (rolled back, thread reloaded) falls back to
    /// the newest, as `reconcileTurnSelection` does.
    pub fn turn(&self, scope: &Scope) -> Option<&TurnSummary> {
        match scope {
            Scope::Turn(run_id) => self.turns.iter().find(|turn| &turn.run_id == run_id).or_else(|| self.latest()),
            _ => self.latest(),
        }
    }

    /// The label on the scope control.
    pub fn label(&self, scope: &Scope) -> String {
        match scope {
            Scope::Thread => "Whole thread".into(),
            Scope::Changes => "Changes".into(),
            Scope::Uncommitted => "Uncommitted".into(),
            Scope::Latest => "Latest turn".into(),
            Scope::Turn(_) => match (self.turn(scope), self.latest()) {
                (Some(turn), Some(latest)) if turn.run_id != latest.run_id => format!("Turn {}", turn.ordinal),
                _ => "Latest turn".into(),
            },
        }
    }
}

fn plan_for(turn: &TurnSummary) -> Option<Plan> {
    match turn.status {
        Status::Missing => Some(Plan::NotGit),
        Status::Error | Status::Stale => Some(Plan::CaptureFailed),
        Status::Ready if !turn.run_completed => Some(Plan::Waiting),
        Status::Ready => None,
    }
}

/// `useCheckpointDiff`'s request for a selection: a range starting at turn zero is the
/// whole-thread RPC, any other range the turn RPC.
pub fn plan(turns: &Turns, scope: &Scope, thread_id: &str, ignore_whitespace: bool) -> Plan {
    if matches!(scope, Scope::Changes | Scope::Uncommitted) {
        return Plan::Review;
    }
    if !turns.loaded {
        return Plan::Loading;
    }
    let Some(selected) = turns.turn(scope) else {
        return Plan::NoTurns;
    };
    let (target, from) = match scope {
        Scope::Thread => {
            // The server serves ranges that end at a completed run with a ready checkpoint.
            match turns.turns.iter().find(|turn| turn.status == Status::Ready && turn.run_completed) {
                Some(newest) => (newest, 0),
                None => return plan_for(selected).unwrap_or(Plan::Waiting),
            }
        }
        _ => {
            if let Some(blocked) = plan_for(selected) {
                return blocked;
            }
            (selected, selected.ordinal.saturating_sub(1))
        }
    };
    let (method, payload) = if from == 0 {
        (FULL_THREAD_DIFF, json!({"threadId":thread_id,"toTurnCount":target.ordinal,"ignoreWhitespace":ignore_whitespace}))
    } else {
        (TURN_DIFF, json!({"threadId":thread_id,"fromTurnCount":from,"toTurnCount":target.ordinal,"ignoreWhitespace":ignore_whitespace}))
    };
    let key = json!([thread_id, from, target.ordinal, ignore_whitespace]).to_string();
    Plan::Fetch(Request { method, payload, key })
}

/// The text to show for a failed diff request. `cause` is the RPC failure's cause list; the
/// server's checkpoint errors wrap the useful sentence in `error.cause.message`.
pub fn failure_message(cause: &Value) -> String {
    let errors: Vec<&Value> = match cause.as_array() {
        Some(causes) => causes.iter().map(|cause| &cause["error"]).collect(),
        None => vec![cause],
    };
    for error in errors {
        for message in [&error["cause"]["message"], &error["message"]] {
            if let Some(message) = message.as_str().filter(|message| !message.trim().is_empty()) {
                return message.to_owned();
            }
        }
    }
    "The server could not complete this request.".into()
}

/// One request's progress. Each request carries the key of what it was for, so an answer that
/// arrives after the selection moved on is dropped instead of shown.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Load<T> {
    #[default]
    Idle,
    Loading { key: String },
    Ready { key: String, value: T },
    Failed { key: String, message: String },
}

/// What a view should draw for the key it currently wants.
#[derive(Debug, PartialEq, Eq)]
pub enum Shown<'a, T> {
    Loading,
    Ready(&'a T),
    Failed(&'a str),
}

impl<T> Load<T> {
    pub fn start(&mut self, key: String) {
        *self = Load::Loading { key };
    }

    /// Records an answer if, and only if, `key` is still the request in flight.
    pub fn finish(&mut self, key: &str, outcome: Result<T, String>) -> bool {
        if !matches!(self, Load::Loading { key: current } if current == key) {
            return false;
        }
        *self = match outcome {
            Ok(value) => Load::Ready { key: key.to_owned(), value },
            Err(message) => Load::Failed { key: key.to_owned(), message },
        };
        true
    }

    /// Anything held for another key is not an answer to this one, so it reads as loading.
    pub fn shown(&self, wanted: &str) -> Shown<'_, T> {
        match self {
            Load::Ready { key, value } if key == wanted => Shown::Ready(value),
            Load::Failed { key, message } if key == wanted => Shown::Failed(message),
            _ => Shown::Loading,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection() -> Value {
        json!({
            "thread": {"id": "t"},
            "runs": [{"id": "r1", "status": "completed"}, {"id": "r2", "status": "completed"}, {"id": "r3", "status": "waiting"}],
            "checkpoints": [
                {"runId": null, "appRunOrdinal": null, "status": "ready", "files": []},
                {"runId": "r2", "appRunOrdinal": 2, "status": "ready", "capturedAt": "2026-01-02T00:00:00Z", "files": [{"path": "b.rs", "kind": "modified", "additions": 2, "deletions": 1}]},
                {"runId": "r1", "appRunOrdinal": 1, "status": "ready", "capturedAt": "2026-01-01T00:00:00Z", "files": [{"path": "a.rs", "kind": "added", "additions": 5, "deletions": 0}, {"kind": "no path"}]},
                {"runId": "r3", "appRunOrdinal": 3, "status": "ready", "capturedAt": "2026-01-03T00:00:00Z", "files": []},
            ],
        })
    }

    #[test]
    fn summaries_list_turns_newest_first_with_their_files() {
        let turns = summarize(Some(&projection()), "t");
        assert!(turns.loaded);
        assert_eq!(turns.turns.iter().map(|turn| (turn.run_id.as_str(), turn.ordinal, turn.run_completed)).collect::<Vec<_>>(), [("r3", 3, false), ("r2", 2, true), ("r1", 1, true)]);
        assert_eq!(turns.turns[2].files, [ChangedFile { path: "a.rs".into(), kind: "added".into(), additions: Some(5), deletions: Some(0) }]);
    }

    #[test]
    fn rolled_back_turns_are_not_listed_and_the_latest_moves_back() {
        let mut value = projection();
        value["runs"][1]["status"] = json!("rolled_back");
        value["runs"][2]["status"] = json!("rolled_back");
        value["checkpoints"][1]["status"] = json!("stale");
        value["checkpoints"][3]["status"] = json!("stale");
        let turns = summarize(Some(&value), "t");
        assert_eq!(turns.turns.iter().map(|turn| turn.run_id.as_str()).collect::<Vec<_>>(), ["r1"]);
        // The scope that named a rewound turn falls back to what is left, with a diff to fetch.
        let Plan::Fetch(request) = plan(&turns, &Scope::Turn("r2".into()), "t", true) else { panic!() };
        assert_eq!(request.payload["toTurnCount"], 1);
        // Nothing left to show is the plain no-turns state, not a failed capture.
        value["runs"][0]["status"] = json!("rolled_back");
        assert_eq!(plan(&summarize(Some(&value), "t"), &Scope::Latest, "t", true), Plan::NoTurns);
    }

    #[test]
    fn another_threads_projection_and_no_projection_are_not_loaded() {
        assert_eq!(summarize(None, "t"), Turns::default());
        assert_eq!(summarize(Some(&projection()), "other"), Turns::default());
        assert_eq!(plan(&Turns::default(), &Scope::Latest, "t", true), Plan::Loading);
    }

    #[test]
    fn a_replayed_checkpoint_replaces_the_row_for_its_run() {
        let mut value = projection();
        value["checkpoints"].as_array_mut().unwrap().push(json!({"runId": "r1", "appRunOrdinal": 1, "status": "error", "files": []}));
        let turns = summarize(Some(&value), "t");
        assert_eq!(turns.turns.len(), 3);
        assert_eq!(turns.turns[2].status, Status::Error);
    }

    fn settled() -> Turns {
        let mut value = projection();
        value["runs"][2]["status"] = json!("completed");
        summarize(Some(&value), "t")
    }

    #[test]
    fn the_first_turn_and_the_whole_thread_use_the_full_thread_rpc() {
        let turns = settled();
        let Plan::Fetch(first) = plan(&turns, &Scope::Turn("r1".into()), "t", true) else { panic!() };
        assert_eq!(first.method, FULL_THREAD_DIFF);
        assert_eq!(first.payload, json!({"threadId": "t", "toTurnCount": 1, "ignoreWhitespace": true}));
        let Plan::Fetch(second) = plan(&turns, &Scope::Turn("r2".into()), "t", false) else { panic!() };
        assert_eq!(second.method, TURN_DIFF);
        assert_eq!(second.payload, json!({"threadId": "t", "fromTurnCount": 1, "toTurnCount": 2, "ignoreWhitespace": false}));
        let Plan::Fetch(whole) = plan(&turns, &Scope::Thread, "t", true) else { panic!() };
        assert_eq!((whole.method, whole.payload["toTurnCount"].clone()), (FULL_THREAD_DIFF, json!(3)));
        let Plan::Fetch(latest) = plan(&turns, &Scope::Latest, "t", true) else { panic!() };
        assert_eq!(latest.payload["toTurnCount"], 3);
    }

    #[test]
    fn request_keys_differ_by_range_and_whitespace_setting() {
        let turns = settled();
        let key = |scope: Scope, ignore| match plan(&turns, &scope, "t", ignore) {
            Plan::Fetch(request) => request.key,
            other => panic!("{other:?}"),
        };
        let keys = [key(Scope::Latest, true), key(Scope::Latest, false), key(Scope::Turn("r2".into()), true), key(Scope::Thread, true)];
        for (index, left) in keys.iter().enumerate() {
            for right in &keys[index + 1..] {
                assert_ne!(left, right);
            }
        }
        assert_eq!(key(Scope::Latest, true), key(Scope::Turn("r3".into()), true));
    }

    #[test]
    fn a_turn_still_settling_is_not_requested() {
        let turns = summarize(Some(&projection()), "t");
        assert_eq!(plan(&turns, &Scope::Latest, "t", true), Plan::Waiting);
        // The whole thread ends at the newest completed turn instead.
        let Plan::Fetch(request) = plan(&turns, &Scope::Thread, "t", true) else { panic!() };
        assert_eq!(request.payload["toTurnCount"], 2);
    }

    #[test]
    fn missing_and_failed_checkpoints_are_states_not_requests() {
        let mut value = projection();
        value["runs"][2]["status"] = json!("completed");
        value["checkpoints"][3]["status"] = json!("missing");
        let turns = summarize(Some(&value), "t");
        assert_eq!(plan(&turns, &Scope::Latest, "t", true), Plan::NotGit);
        assert!(matches!(plan(&turns, &Scope::Turn("r2".into()), "t", true), Plan::Fetch(_)));
        value["checkpoints"][3]["status"] = json!("error");
        assert_eq!(plan(&summarize(Some(&value), "t"), &Scope::Latest, "t", true), Plan::CaptureFailed);
        for status in ["missing", "error"] {
            for checkpoint in value["checkpoints"].as_array_mut().unwrap() {
                checkpoint["status"] = json!(status);
            }
            let plan = plan(&summarize(Some(&value), "t"), &Scope::Thread, "t", true);
            assert_eq!(plan, if status == "missing" { Plan::NotGit } else { Plan::CaptureFailed });
        }
    }

    #[test]
    fn a_thread_without_checkpoints_has_no_turns() {
        let value = json!({"thread": {"id": "t"}, "runs": [], "checkpoints": []});
        assert_eq!(plan(&summarize(Some(&value), "t"), &Scope::Latest, "t", true), Plan::NoTurns);
        assert_eq!(Plan::NoTurns.message(), Some("No completed turns yet."));
    }

    #[test]
    fn a_turn_that_no_longer_exists_falls_back_to_the_newest() {
        let turns = settled();
        assert_eq!(turns.turn(&Scope::Turn("gone".into())).unwrap().run_id, "r3");
    }

    #[test]
    fn scope_labels_follow_the_original() {
        let turns = settled();
        assert_eq!(turns.label(&Scope::Latest), "Latest turn");
        assert_eq!(turns.label(&Scope::Turn("r3".into())), "Latest turn");
        assert_eq!(turns.label(&Scope::Turn("r1".into())), "Turn 1");
        assert_eq!(turns.label(&Scope::Thread), "Whole thread");
    }

    #[test]
    fn failures_prefer_the_checkpoint_cause_over_the_generic_message() {
        let cause = json!([{"error": {"_tag": "OrchestrationGetTurnDiffError", "message": "Failed to load turn diff", "cause": {"name": "CheckpointTurnRangeUnavailableError", "message": "Turn diff range exceeds current turn count: requested 9, current 3."}}}]);
        assert_eq!(failure_message(&cause), "Turn diff range exceeds current turn count: requested 9, current 3.");
        let denied = json!([{"error": {"_tag": "EnvironmentAuthorizationError", "message": "Missing orchestration:read"}}]);
        assert_eq!(failure_message(&denied), "Missing orchestration:read");
        let bare = json!({"message": "plain"});
        assert_eq!(failure_message(&bare), "plain");
        assert_eq!(failure_message(&json!(null)), "The server could not complete this request.");
        assert_eq!(failure_message(&json!([{"error": {"message": "  ", "cause": {"message": ""}}}])), "The server could not complete this request.");
    }

    #[test]
    fn a_late_answer_for_an_old_key_is_dropped() {
        let mut load: Load<&str> = Load::Idle;
        load.start("turn-1".into());
        load.start("turn-2".into());
        assert!(!load.finish("turn-1", Ok("stale")));
        assert_eq!(load.shown("turn-2"), Shown::Loading);
        assert!(load.finish("turn-2", Ok("fresh")));
        assert_eq!(load.shown("turn-2"), Shown::Ready(&"fresh"));
        // Even a repeat of the old answer cannot overwrite a settled one.
        assert!(!load.finish("turn-1", Err("late".into())));
        assert!(!load.finish("turn-2", Err("again".into())));
        assert_eq!(load.shown("turn-2"), Shown::Ready(&"fresh"));
    }

    #[test]
    fn what_was_loaded_for_another_key_reads_as_loading() {
        let mut load: Load<u8> = Load::Idle;
        load.start("a".into());
        load.finish("a", Err("boom".into()));
        assert_eq!(load.shown("a"), Shown::Failed("boom"));
        assert_eq!(load.shown("b"), Shown::Loading);
        load.start("a".into());
        assert_eq!(load.shown("a"), Shown::Loading);
        assert_eq!(Load::<u8>::Idle.shown("a"), Shown::Loading);
    }
}
