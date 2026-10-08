use serde_json::{Value, json};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadState {
    pub sequence: u64,
    pub projection: Option<Value>,
    pub history_cursor: Option<String>,
    pub has_more_history: bool,
    pub latest_local_turn_ordinal: Option<u64>,
    pub synchronized: bool,
}

impl ThreadState {
    /// A socket snapshot or another history page can change the cursor while
    /// an HTTP page is in flight. Only the page owning this cursor may merge.
    pub fn merge_history_for_cursor(
        &mut self,
        request_cursor: &str,
        page: &Value,
    ) -> Result<bool, &'static str> {
        if self.history_cursor.as_deref() != Some(request_cursor) {
            return Ok(false);
        }
        self.merge_history(page)?;
        Ok(true)
    }

    pub fn apply(&mut self, item: &Value) -> Result<bool, &'static str> {
        match item["kind"].as_str() {
            Some("synchronized") => {
                self.synchronized = true;
                Ok(false)
            }
            Some("snapshot") => {
                if !item["projection"]["thread"]["id"].is_string() {
                    return Err("snapshot has no thread");
                }
                let sequence = item["snapshotSequence"]
                    .as_u64()
                    .ok_or("missing snapshot sequence")?;
                let cursor = match item.get("historyCursor") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(cursor)) => Some(cursor.clone()),
                    _ => return Err("invalid history cursor"),
                };
                let has_more = match item.get("hasMoreHistory") {
                    None => false,
                    Some(Value::Bool(value)) => *value,
                    _ => return Err("invalid history state"),
                };
                let watermark = match item.get("latestLocalTurnOrdinal") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(value.as_u64().ok_or("invalid turn watermark")?),
                };
                self.sequence = sequence;
                self.projection = Some(item["projection"].clone());
                self.history_cursor = cursor;
                self.has_more_history = has_more;
                self.latest_local_turn_ordinal = watermark;
                self.synchronized = false;
                Ok(true)
            }
            Some("event") | Some("unknown-event") => {
                let sequence = item["sequence"].as_u64().ok_or("missing event sequence")?;
                if sequence <= self.sequence {
                    return Ok(false);
                }
                let changed = if let Some(projection) = self.projection.as_mut() {
                    apply_projection_event(
                        projection,
                        &item["event"],
                        self.has_more_history,
                        self.latest_local_turn_ordinal,
                    )?
                } else {
                    false
                };
                // Unknown future event kinds still advance resume position.
                self.sequence = sequence;
                Ok(changed)
            }
            _ => Err("unknown thread stream kind"),
        }
    }

    /// Pages are chronological and may overlap with live updates. Current
    /// entities win over stale page rows, and positions remain contiguous.
    pub fn merge_history(&mut self, page: &Value) -> Result<(), &'static str> {
        // Reject malformed pages before updating rows or resume metadata.
        let has_more = page["hasMoreHistory"]
            .as_bool()
            .ok_or("missing history state")?;
        let cursor = match page.get("nextCursor") {
            Some(Value::Null) => None,
            Some(Value::String(cursor)) => Some(cursor.clone()),
            _ => return Err("invalid history cursor"),
        };
        let incoming = page["items"].as_array().ok_or("missing history items")?;
        if incoming
            .iter()
            .any(|row| !row["sourceItemId"].is_string() || !row["sourceThreadId"].is_string())
        {
            return Err("invalid history item identity");
        }
        let projection = self.projection.as_mut().ok_or("thread not loaded")?;
        let current = projection["visibleTurnItems"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut rows = incoming.clone();
        rows.retain(|row| {
            !current.iter().any(|existing| {
                existing["sourceItemId"] == row["sourceItemId"]
                    && existing["sourceThreadId"] == row["sourceThreadId"]
            })
        });
        rows.extend(current);
        renumber(&mut rows);
        projection["visibleTurnItems"] = Value::Array(rows);
        self.history_cursor = cursor;
        self.has_more_history = has_more;
        Ok(())
    }
}

fn upsert(array: &mut Value, item: Value) -> Result<(), &'static str> {
    let id = item
        .get("id")
        .filter(|id| id.is_string())
        .ok_or("entity has no id")?;
    let rows = array.as_array_mut().ok_or("projection array missing")?;
    if let Some(index) = rows.iter().position(|row| row.get("id") == Some(id)) {
        rows[index] = item;
    } else {
        rows.push(item);
    }
    Ok(())
}

fn renumber(rows: &mut [Value]) {
    for (index, row) in rows.iter_mut().enumerate() {
        row["position"] = json!(index);
    }
}

fn item_visible(projection: &Value, item: &Value) -> bool {
    let run_id = &item["runId"];
    let run_status = projection["runs"]
        .as_array()
        .and_then(|runs| {
            runs.iter()
                .find(|run| !run_id.is_null() && run["id"] == *run_id)
        })
        .and_then(|run| run["status"].as_str());
    if run_status == Some("rolled_back") {
        return false;
    }
    if run_status == Some("cancelled")
        && item["type"] == "user_message"
        && item["inputIntent"] == "queued_turn"
    {
        return false;
    }
    if item["type"] == "run_interrupt_result" && !run_id.is_null() && !item["nodeId"].is_null() {
        let superseded = projection["attempts"].as_array().is_some_and(|attempts| {
            attempts.iter().any(|attempt| {
                attempt["runId"] == *run_id
                    && attempt["rootNodeId"] == item["nodeId"]
                    && attempt["status"] == "superseded"
            })
        });
        let matching = projection["turnItems"].as_array().is_some_and(|items| {
            items.iter().any(|candidate| {
                candidate["type"] == "run_interrupt_request" && candidate["runId"] == *run_id
            })
        });
        if superseded && !matching {
            return false;
        }
    }
    true
}

fn filter_visible(projection: &mut Value) {
    let mut rows = projection["visibleTurnItems"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    rows.retain(|row| row["visibility"] != "local" || item_visible(projection, &row["item"]));
    renumber(&mut rows);
    projection["visibleTurnItems"] = Value::Array(rows);
}

fn update_visible(projection: &mut Value, item: Value, partial: bool, watermark: Option<u64>) {
    let mut rows = projection["visibleTurnItems"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let index = rows
        .iter()
        .position(|row| row["sourceItemId"] == item["id"]);
    if !item_visible(projection, &item) {
        if let Some(index) = index {
            rows.remove(index);
        }
    } else {
        let ordinal = item["ordinal"].as_u64().unwrap_or(0);
        let oldest = rows
            .iter()
            .filter(|row| row["visibility"] == "local")
            .filter_map(|row| row["item"]["ordinal"].as_u64())
            .min();
        if index.is_none()
            && partial
            && (watermark.is_some_and(|max| ordinal <= max)
                || oldest.is_some_and(|min| ordinal < min))
        {
            return;
        }
        if let Some(index) = index {
            rows.remove(index);
        }
        let insertion = rows
            .iter()
            .position(|row| {
                row["visibility"] == "local"
                    && (row["item"]["ordinal"].as_u64().unwrap_or(0) > ordinal
                        || (row["item"]["ordinal"] == item["ordinal"]
                            && row["item"]["id"].as_str() > item["id"].as_str()))
            })
            .unwrap_or(rows.len());
        rows.insert(insertion, json!({"position":0,"visibility":"local","sourceThreadId":item["threadId"],"sourceItemId":item["id"],"item":item}));
    }
    renumber(&mut rows);
    projection["visibleTurnItems"] = Value::Array(rows);
}

/// The payload stays lossless until every provider/tool variant is ported.
/// Events for other threads and future event kinds cannot corrupt this cache.
pub fn apply_projection_event(
    projection: &mut Value,
    event: &Value,
    partial: bool,
    watermark: Option<u64>,
) -> Result<bool, &'static str> {
    if event["threadId"] != projection["thread"]["id"] {
        return Ok(false);
    }
    let Some(kind) = event["type"].as_str() else {
        return Ok(false);
    };
    let payload = event["payload"].clone();
    let array = match kind {
        "run.created" | "run.updated" => Some("runs"),
        "run-attempt.created" | "run-attempt.updated" => Some("attempts"),
        "node.updated" => Some("nodes"),
        "subagent.updated" => Some("subagents"),
        "provider-session.attached" | "provider-session.updated" => Some("providerSessions"),
        "provider-thread.updated" => Some("providerThreads"),
        "provider-turn.updated" => Some("providerTurns"),
        "runtime-request.updated" => Some("runtimeRequests"),
        "message.updated" => Some("messages"),
        "plan.updated" => Some("plans"),
        "checkpoint-scope.created" => Some("checkpointScopes"),
        "checkpoint.captured" => Some("checkpoints"),
        "context-handoff.updated" => Some("contextHandoffs"),
        "context-transfer.created" | "context-transfer.updated" => Some("contextTransfers"),
        _ => None,
    };
    if let Some(array) = array {
        let mut payload = payload;
        if array == "providerTurns" && payload["tokenUsage"].is_null() {
            if let Some(usage) = projection[array]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"] == payload["id"]))
                .and_then(|row| row.get("tokenUsage"))
            {
                payload["tokenUsage"] = usage.clone();
            }
        }
        upsert(&mut projection[array], payload)?;
        if matches!(array, "runs" | "attempts") {
            filter_visible(projection);
        }
    } else if matches!(
        kind,
        "thread.created"
            | "thread.archived"
            | "thread.unarchived"
            | "thread.deleted"
            | "thread.settled"
            | "thread.unsettled"
            | "thread.snoozed"
            | "thread.unsnoozed"
            | "thread.auto-settle-set"
            | "thread.pinned"
            | "thread.unpinned"
            | "thread.pin-reordered"
            | "thread.active-reordered"
            | "thread.metadata-updated"
            | "thread.pull-request-synced"
            | "thread.runtime-mode-updated"
            | "thread.interaction-mode-updated"
            | "thread.model-selection-updated"
            | "thread.provider-switched"
            | "thread.visited"
            | "thread.marked-unread"
    ) {
        projection["thread"] = payload;
        if matches!(kind, "thread.visited" | "thread.marked-unread") {
            return Ok(true);
        }
    } else if kind == "provider-session.detached" {
        projection["providerSessions"]
            .as_array_mut()
            .ok_or("missing provider sessions")?
            .retain(|row| row["id"] != payload["providerSessionId"]);
    } else if kind == "turn-item.updated" {
        let existing = projection["turnItems"]
            .as_array()
            .ok_or("missing turn items")?
            .iter()
            .any(|row| row["id"] == payload["id"]);
        let visible = projection["visibleTurnItems"]
            .as_array()
            .ok_or("missing visible items")?;
        let oldest = visible
            .iter()
            .filter(|row| row["visibility"] == "local")
            .filter_map(|row| row["item"]["ordinal"].as_u64())
            .min();
        let ordinal = payload["ordinal"].as_u64().ok_or("missing ordinal")?;
        if partial
            && !existing
            && (watermark.is_some_and(|max| ordinal <= max)
                || oldest.is_some_and(|min| ordinal < min))
        {
            return Ok(false);
        }
        let changed_interrupt = payload["type"] == "run_interrupt_request"
            || projection["turnItems"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["id"] == payload["id"] && item["type"] == "run_interrupt_request"
                })
            });
        upsert(&mut projection["turnItems"], payload.clone())?;
        if changed_interrupt {
            filter_visible(projection);
        }
        update_visible(projection, payload, partial, watermark);
    } else if kind == "run.background-work-cancelled" {
        if let Some(run) = projection["runs"]
            .as_array_mut()
            .and_then(|rows| rows.iter_mut().find(|row| row["id"] == payload["runId"]))
        {
            run["restartCancelledBackgroundWork"] =
                payload["restartCancelledBackgroundWork"].clone();
        }
    } else if kind != "checkpoint.rollback-requested" {
        return Ok(false);
    }
    projection["updatedAt"] = event["occurredAt"].clone();
    Ok(true)
}
