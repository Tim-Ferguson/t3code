//! The counters a thread shell is read from, kept up to date by the reducer so an event costs a few comparisons and no pass over
//! the thread's history (`thread::projection_after`; the shell is rebuilt for every event of a thread and sent to every client).
//!
//! They live in the stored projection under `shellState`, which no client is sent (`wire_projection::projection` removes it). A
//! projection stored by an earlier build has none: `of` scans the history for it once, and the reducer writes it with the next
//! event, so no rebuild of stored state is needed.
//!
//! What the original reads with SQL (`ProjectionStore.selectShellThreadRows`):
//! - `itemCount`: the turn items of the thread, less those of rolled-back runs.
//! - `visibleItemCount`: that count, and for a fork the items its source holds through the fork point and the fork's marker.
//! - `latestUserMessageAt` and `latestUserAuthoredMessageAt`: the newest `updatedAt` (then the greatest id) among the thread's
//!   user messages, and among those a person wrote (`createdBy: "user"`; wakes and agent messages use the user role too).
use serde_json::{Map, Value, json};

pub(crate) const KEY: &str = "shellState";

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}

pub(crate) fn empty() -> Value {
    json!({"items":0,"runItems":{},"userMessage":null,"authoredMessage":null,"inherited":0})
}

/// The counters from the history itself.
pub(crate) fn scan(projection: &Value) -> Value {
    let mut state = empty();
    state["items"] = json!(rows(projection, "turnItems").len());
    let mut per_run: Map<String, Value> = Map::new();
    for item in rows(projection, "turnItems") {
        if let Some(run) = item["runId"].as_str() {
            let count = per_run.get(run).and_then(Value::as_u64).unwrap_or(0) + 1;
            per_run.insert(run.to_owned(), json!(count));
        }
    }
    state["runItems"] = Value::Object(per_run);
    state["userMessage"] = newest_message(projection, false);
    state["authoredMessage"] = newest_message(projection, true);
    state["inherited"] = json!(projection[KEY]["inherited"].as_u64().unwrap_or(0));
    state
}

fn newest_message(projection: &Value, authored: bool) -> Value {
    rows(projection, "messages")
        .iter()
        .filter(|message| message["role"] == "user" && (!authored || message["createdBy"] == "user"))
        .filter_map(|message| Some((message["updatedAt"].as_str()?, message["id"].as_str()?)))
        .max()
        .map_or(Value::Null, |(at, id)| json!({"id":id,"at":at}))
}

/// The counters of a projection, from what is stored or else from the history.
pub(crate) fn of(projection: &Value) -> std::borrow::Cow<'_, Value> {
    match projection.get(KEY).filter(|state| state.is_object()) {
        Some(state) => std::borrow::Cow::Borrowed(state),
        None => std::borrow::Cow::Owned(scan(projection)),
    }
}

/// The projection of a fork after its visible items were built from `source`: what it inherits, in items, as the shell counts it.
pub(crate) fn inherit(projection: &mut Value, source: &Value) {
    let inherited = inherited_item_count(projection, source);
    if projection.get(KEY).is_none_or(|state| !state.is_object()) {
        projection[KEY] = scan(projection);
    }
    projection[KEY]["inherited"] = json!(inherited);
}

/// What the row an event replaces said, as far as the counters need it.
pub(crate) enum Before {
    Nothing,
    Item { run: Option<String> },
    Message { role: String, created_by: String, at: String },
}
pub(crate) fn before(field: &str, row: &Value) -> Before {
    match field {
        "turnItems" => Before::Item { run: row["runId"].as_str().map(str::to_owned) },
        "messages" => Before::Message { role: row["role"].as_str().unwrap_or("").to_owned(), created_by: row["createdBy"].as_str().unwrap_or("").to_owned(), at: row["updatedAt"].as_str().unwrap_or("").to_owned() },
        _ => Before::Nothing,
    }
}

/// An event upserted `row` into `field`, replacing what `before` describes (`None` when the row is new).
pub(crate) fn applied(projection: &mut Value, field: &str, row: &Value, replaced: Option<Before>) {
    if !matches!(field, "turnItems" | "messages") {
        return;
    }
    if projection.get(KEY).is_none_or(|state| !state.is_object()) {
        // First event since an earlier build stored this thread: the history already holds the row, so a scan is the state.
        projection[KEY] = scan(projection);
        return;
    }
    match field {
        "turnItems" => {
            let state = &mut projection[KEY];
            let previous_run = match &replaced {
                Some(Before::Item { run }) => Some(run.clone()),
                _ => None,
            };
            if previous_run.is_none() {
                state["items"] = json!(state["items"].as_u64().unwrap_or(0) + 1);
            }
            let run = row["runId"].as_str().map(str::to_owned);
            if previous_run.as_ref() != Some(&run) {
                if let Some(Some(old)) = previous_run {
                    let count = state["runItems"][&old].as_u64().unwrap_or(1).saturating_sub(1);
                    state["runItems"][&old] = json!(count);
                }
                if let Some(run) = run {
                    let count = state["runItems"][&run].as_u64().unwrap_or(0) + 1;
                    state["runItems"][&run] = json!(count);
                }
            }
        }
        _ => {
            if row["role"] != "user" && !matches!(&replaced, Some(Before::Message { role, .. }) if role == "user") {
                return;
            }
            for (slot, authored) in [("userMessage", false), ("authoredMessage", true)] {
                let qualifies = row["role"] == "user" && (!authored || row["createdBy"] == "user");
                let held = projection[KEY][slot]["id"].as_str().map(str::to_owned);
                let mine = row["id"].as_str().unwrap_or("");
                let at = row["updatedAt"].as_str().unwrap_or("");
                if held.as_deref() == Some(mine) {
                    // The newest message changed: a later time keeps it; an earlier time or a message that no longer
                    // counts may hand the place to another, which only the history knows.
                    if qualifies && projection[KEY][slot]["at"].as_str().is_some_and(|old| at >= old) {
                        projection[KEY][slot] = json!({"id":mine,"at":at});
                    } else {
                        projection[KEY][slot] = newest_message(projection, authored);
                    }
                } else if qualifies {
                    let better = match (held.as_deref(), projection[KEY][slot]["at"].as_str()) {
                        (Some(held), Some(held_at)) => (at, mine) > (held_at, held),
                        _ => true,
                    };
                    if better {
                        projection[KEY][slot] = json!({"id":mine,"at":at});
                    }
                }
            }
        }
    }
}

/// Items of the thread that are not those of a rolled-back run (`itemCount`).
pub(crate) fn item_count(projection: &Value) -> u64 {
    let state = of(projection);
    let rolled_back: u64 = rows(projection, "runs")
        .iter()
        .filter(|run| run["status"] == "rolled_back")
        .filter_map(|run| state["runItems"][run["id"].as_str()?].as_u64())
        .sum();
    state["items"].as_u64().unwrap_or(0).saturating_sub(rolled_back)
}

/// `visibleItemCount`: for a fork, what it inherits (`inherited`, set when its visible items were last built from the source) and
/// its own items.
pub(crate) fn visible_item_count(projection: &Value) -> u64 {
    item_count(projection) + of(projection)["inherited"].as_u64().unwrap_or(0)
}

/// What a fork inherits from `source` at its fork point, in items (`visibleItemCountForShell`): the source's own inherited part,
/// the items of every run of the source up to the fork point (rolled-back runs included, and the items that belong to no run for
/// an imported history), and the fork's marker.
pub(crate) fn inherited_item_count(projection: &Value, source: &Value) -> u64 {
    let forked = &projection["thread"]["forkedFrom"];
    let state = of(source);
    let ordinal_of = |run: &Value| run["ordinal"].as_u64();
    let boundary = rows(source, "runs").iter().find(|run| run["id"] == forked["runId"]).and_then(ordinal_of);
    let through: u64 = match boundary {
        None => 0,
        Some(boundary) => {
            let runless = if source["thread"]["historyOrigin"] == "v1_import" {
                state["items"].as_u64().unwrap_or(0).saturating_sub(state["runItems"].as_object().map_or(0, |runs| runs.values().filter_map(Value::as_u64).sum()))
            } else {
                0
            };
            runless + rows(source, "runs").iter().filter(|run| ordinal_of(run).is_some_and(|ordinal| ordinal <= boundary)).filter_map(|run| state["runItems"][run["id"].as_str()?].as_u64()).sum::<u64>()
        }
    };
    // A source that is itself a fork contributes the part it inherits; its own items are counted through the fork point above.
    let sources_own = if source["thread"]["forkedFrom"]["type"] == "run" { of(source)["inherited"].as_u64().unwrap_or(0) } else { 0 };
    sources_own + through + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, role: &str, by: &str, at: &str) -> Value {
        json!({"id":id,"role":role,"createdBy":by,"updatedAt":at})
    }
    fn item(id: &str, run: Option<&str>) -> Value {
        json!({"id":id,"runId":run})
    }
    fn apply(projection: &mut Value, field: &str, row: Value) {
        let replaced = projection[field].as_array().unwrap().iter().find(|existing| existing["id"] == row["id"]).map(|existing| before(field, existing));
        let rows = projection[field].as_array_mut().unwrap();
        match rows.iter_mut().find(|existing| existing["id"] == row["id"]) {
            Some(existing) => *existing = row.clone(),
            None => rows.push(row.clone()),
        }
        applied(projection, field, &row, replaced);
    }
    fn fresh() -> Value {
        json!({"thread":{},"runs":[],"turnItems":[],"messages":[],KEY:empty()})
    }

    /// Whatever the events were, the counters are what a scan of the history says.
    #[test]
    fn the_counters_follow_the_history_through_replacement_and_demotion() {
        let mut projection = fresh();
        let steps = [
            ("messages", message("a", "user", "user", "2026-01-01T00:00:01.000Z")),
            ("messages", message("b", "user", "agent", "2026-01-01T00:00:05.000Z")),
            ("messages", message("c", "assistant", "agent", "2026-01-01T00:00:09.000Z")),
            // The newest user message is replaced by an earlier time: the older one holds the place again.
            ("messages", message("b", "user", "agent", "2026-01-01T00:00:00.500Z")),
            ("messages", message("a", "user", "user", "2026-01-01T00:00:02.000Z")),
            // The holder stops being a person's message.
            ("messages", message("a", "user", "agent", "2026-01-01T00:00:03.000Z")),
            ("messages", message("d", "user", "user", "2026-01-01T00:00:03.000Z")),
            ("messages", message("e", "user", "user", "2026-01-01T00:00:03.000Z")),
            ("turnItems", item("i1", Some("r1"))),
            ("turnItems", item("i2", Some("r1"))),
            ("turnItems", item("i3", None)),
            ("turnItems", item("i2", Some("r2"))),
        ];
        for (field, row) in steps {
            apply(&mut projection, field, row);
            let mut scanned = scan(&projection);
            scanned["inherited"] = projection[KEY]["inherited"].clone();
            assert_eq!(projection[KEY], scanned, "after {field} {}", projection[field].as_array().unwrap().len());
        }
        assert_eq!(projection[KEY]["authoredMessage"]["id"], "e");
        assert_eq!(projection[KEY]["userMessage"]["id"], "e");
        assert_eq!(projection[KEY]["runItems"], json!({"r1":1,"r2":1}));
    }

    /// An imported history's items that belong to no run come before every run; a fork counts them with the source's run items.
    #[test]
    fn a_fork_of_an_imported_history_counts_the_items_that_belong_to_no_run() {
        let mut source = fresh();
        source["thread"] = json!({"historyOrigin":"v1_import"});
        source["runs"] = json!([{"id":"r1","ordinal":1,"status":"completed"}]);
        for (id, run) in [("one", None), ("two", None), ("native", Some("r1"))] {
            apply(&mut source, "turnItems", item(id, run));
        }
        let fork = json!({"thread":{"forkedFrom":{"type":"run","threadId":"s","runId":"r1"}}});
        assert_eq!(inherited_item_count(&fork, &source), 4);
        // A fork of that fork inherits the first fork's inheritance as well.
        let mut middle = fresh();
        middle["thread"] = json!({"forkedFrom":{"type":"run","threadId":"s","runId":"r1"}});
        middle["runs"] = json!([{"id":"m1","ordinal":1,"status":"completed"}]);
        middle[KEY]["inherited"] = json!(4);
        apply(&mut middle, "turnItems", item("own", Some("m1")));
        let grandchild = json!({"thread":{"forkedFrom":{"type":"run","threadId":"m","runId":"m1"}}});
        assert_eq!(inherited_item_count(&grandchild, &middle), 4 + 1 + 1);
    }

    #[test]
    fn a_rolled_back_run_leaves_the_item_count_and_a_fork_counts_its_source_through_the_fork_point() {
        let mut source = fresh();
        source["runs"] = json!([{"id":"r1","ordinal":1,"status":"completed"},{"id":"r2","ordinal":2,"status":"rolled_back"},{"id":"r3","ordinal":3,"status":"completed"}]);
        for (id, run) in [("a", "r1"), ("b", "r1"), ("c", "r2"), ("d", "r3")] {
            apply(&mut source, "turnItems", item(id, Some(run)));
        }
        assert_eq!(item_count(&source), 3);
        let fork = json!({"thread":{"forkedFrom":{"type":"run","threadId":"s","runId":"r2"}},"runs":[],"turnItems":[],"messages":[]});
        // Runs one and two, rolled-back or not, and the marker.
        assert_eq!(inherited_item_count(&fork, &source), 4);
        let unknown = json!({"thread":{"forkedFrom":{"type":"run","threadId":"s","runId":"missing"}}});
        assert_eq!(inherited_item_count(&unknown, &source), 1);
    }
}
