//! Source-compatible conversation windows over the authoritative persisted timeline.
use crate::persistence::{Store, StoreError, read_projection};
use base64::{
    Engine, alphabet,
    engine::{
        DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig, general_purpose::URL_SAFE_NO_PAD,
    },
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;

pub const MAX_ENCODED_BYTES: usize = 1_048_576;
#[derive(Clone, Copy)]
pub struct PagePolicy {
    pub user_turns: Option<usize>,
    pub items: usize,
    pub bytes: usize,
}
impl Default for PagePolicy {
    fn default() -> Self {
        Self {
            user_turns: Some(10),
            items: 75,
            bytes: MAX_ENCODED_BYTES,
        }
    }
}
#[derive(Serialize)]
struct Cursor {
    v: u8,
    seq: u64,
    st: String,
    si: String,
    p: usize,
}
fn invalid_cursor() -> StoreError {
    StoreError::InvalidCommand("Invalid thread history cursor.".into())
}
fn decode_cursor(value: &str) -> Result<Cursor, StoreError> {
    if value.is_empty() || value.encode_utf16().count() > 4096 {
        return Err(invalid_cursor());
    }
    // Node Buffer's base64url decoder also accepts standard alphabet, whitespace,
    // ignored punctuation, omitted padding and a dangling final six-bit group.
    let mut encoded: Vec<u8> = value
        .bytes()
        .take_while(|byte| *byte != b'=')
        .filter_map(|byte| match byte {
            b'+' => Some(b'-'),
            b'/' => Some(b'_'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => Some(byte),
            _ => None,
        })
        .collect();
    if encoded.len() % 4 == 1 {
        encoded.pop();
    }
    let engine = GeneralPurpose::new(
        &alphabet::URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let bytes = engine.decode(encoded).map_err(|_| invalid_cursor())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
    let integer = |key: &str| {
        value[key]
            .as_f64()
            .filter(|value| value.is_finite() && *value >= 0. && value.fract() == 0.)
    };
    if value["v"].as_f64() != Some(1.) {
        return Err(invalid_cursor());
    }
    let seq = integer("seq").ok_or_else(invalid_cursor)?;
    let p = integer("p").ok_or_else(invalid_cursor)?;
    let st = value["st"]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(invalid_cursor)?;
    let si = value["si"]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(invalid_cursor)?;
    Ok(Cursor {
        v: 1,
        seq: seq as u64,
        st: st.into(),
        si: si.into(),
        p: p as usize,
    })
}
fn bytes(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("JSON value serializes")
        .len()
}
fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn identity(value: &Value, field: &str) -> String {
    value[field].as_str().unwrap_or("").to_owned()
}
fn turn_start(item: &Value) -> bool {
    item["type"] == "user_message"
        && matches!(
            item["inputIntent"].as_str(),
            Some("turn_start" | "queued_turn")
        )
}
fn user_turn(item: &Value) -> bool {
    turn_start(item) && item["createdBy"] == "user"
}
fn local(row: &Value, thread: &Value) -> bool {
    row["visibility"] == "local" || row["sourceThreadId"] == thread["id"]
}

pub fn select_page(
    items: &[Value],
    end: usize,
    sequence: u64,
    policy: PagePolicy,
    local_thread: Option<&Value>,
) -> Value {
    let end = end.min(items.len());
    let turn_limit = if items[..end].iter().any(|row| user_turn(&row["item"])) {
        policy.user_turns
    } else {
        None
    };
    let mut selected = Vec::new();
    let (mut encoded, mut users, mut raw_turns) = (0usize, 0usize, 0usize);
    for row in items[..end].iter().rev() {
        let cost = if turn_limit.is_some() {
            0
        } else {
            bytes(row)
                + local_thread
                    .filter(|thread| local(row, thread))
                    .map(|_| bytes(&row["item"]))
                    .unwrap_or(0)
        };
        // Ordinary turns may exceed both budgets; cutting a turn hides its user input.
        if !selected.is_empty()
            && match turn_limit {
                Some(limit) => users >= limit || raw_turns >= 150,
                None => {
                    selected.len() >= policy.items || encoded.saturating_add(cost) > policy.bytes
                }
            }
        {
            break;
        }
        selected.push(row.clone());
        encoded += cost;
        if user_turn(&row["item"]) {
            users += 1;
        }
        if turn_start(&row["item"]) {
            raw_turns += 1;
        }
    }
    selected.reverse();
    let oldest = selected.first();
    let has_more = oldest
        .and_then(|oldest| {
            items.iter().position(|row| {
                row["sourceThreadId"] == oldest["sourceThreadId"]
                    && row["sourceItemId"] == oldest["sourceItemId"]
            })
        })
        .is_some_and(|index| index > 0);
    let cursor = if has_more {
        let row = oldest.unwrap();
        Some(
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&Cursor {
                    v: 1,
                    seq: sequence,
                    st: identity(row, "sourceThreadId"),
                    si: identity(row, "sourceItemId"),
                    p: row["position"].as_u64().unwrap_or(0) as usize,
                })
                .unwrap(),
            ),
        )
    } else {
        None
    };
    for (position, row) in selected.iter_mut().enumerate() {
        row["position"] = json!(position);
    }
    json!({"items":selected,"nextCursor":cursor,"hasMoreHistory":has_more})
}

pub fn bounded_projection(projection: &Value, sequence: u64, policy: PagePolicy) -> Value {
    let mut control = projection.clone();
    control["plans"] = json!(
        rows(projection, "plans")
            .iter()
            .filter(|plan| plan["status"] == "active")
            .collect::<Vec<_>>()
    );
    control["contextHandoffs"] = json!(
        rows(projection, "contextHandoffs")
            .iter()
            .filter(|handoff| matches!(handoff["status"].as_str(), Some("pending" | "ready")))
            .collect::<Vec<_>>()
    );
    let latest_local = rows(projection, "turnItems")
        .iter()
        .filter_map(|item| item["ordinal"].as_u64())
        .max();
    let reserve: usize = rows(projection, "turnItems")
        .iter()
        .filter(|item| item["type"] == "run_interrupt_request")
        .map(bytes)
        .sum();
    let mut empty = control.clone();
    for field in ["messages", "turnItems", "visibleTurnItems"] {
        empty[field] = json!([]);
    }
    let window_policy = PagePolicy {
        bytes: policy
            .bytes
            .saturating_sub(reserve.saturating_add(bytes(&empty)).saturating_add(1024)),
        ..policy
    };
    let visible = rows(projection, "visibleTurnItems");
    let window = select_page(
        visible,
        visible.len(),
        sequence,
        window_policy,
        Some(&projection["thread"]),
    );
    let local_ids: HashSet<String> = rows(&window, "items")
        .iter()
        .filter(|row| local(row, &projection["thread"]))
        .map(|row| identity(row, "sourceItemId"))
        .collect();
    let mut items: Vec<Value> = rows(projection, "turnItems")
        .iter()
        .filter(|item| local_ids.contains(&identity(item, "id")))
        .cloned()
        .collect();
    items.extend(
        rows(projection, "turnItems")
            .iter()
            .filter(|item| {
                item["type"] == "run_interrupt_request"
                    && !local_ids.contains(&identity(item, "id"))
            })
            .cloned(),
    );
    let plan_ids: HashSet<String> = items
        .iter()
        .filter(|item| matches!(item["type"].as_str(), Some("proposed_plan" | "todo_list")))
        .map(|item| identity(item, "planId"))
        .collect();
    let handoff_ids: HashSet<String> = items
        .iter()
        .filter(|item| item["type"] == "handoff")
        .map(|item| identity(item, "contextHandoffId"))
        .collect();
    control["plans"] = json!(
        rows(projection, "plans")
            .iter()
            .filter(|plan| plan["status"] == "active" || plan_ids.contains(&identity(plan, "id")))
            .map(|plan| {
                let mut plan = plan.clone();
                if plan["status"] != "active" {
                    if plan["kind"] == "proposed_plan" {
                        plan["markdown"] = json!("");
                    } else {
                        plan.as_object_mut().unwrap().remove("explanation");
                    }
                    plan["detailInTurnItem"] = json!(true);
                }
                plan
            })
            .collect::<Vec<_>>()
    );
    control["contextHandoffs"] = json!(
        rows(projection, "contextHandoffs")
            .iter()
            .filter(
                |handoff| matches!(handoff["status"].as_str(), Some("pending" | "ready"))
                    || handoff_ids.contains(&identity(handoff, "id"))
            )
            .map(|handoff| {
                let mut handoff = handoff.clone();
                if !matches!(handoff["status"].as_str(), Some("pending" | "ready")) {
                    handoff["summaryText"] = json!("");
                    handoff["detailInTurnItem"] = json!(true);
                }
                handoff
            })
            .collect::<Vec<_>>()
    );
    let latest_run = rows(projection, "runs")
        .iter()
        .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0));
    let retained_runs: Vec<&Value> = rows(projection, "runs")
        .iter()
        .filter(|run| {
            latest_run.is_some_and(|latest| latest["id"] == run["id"])
                || matches!(
                    run["status"].as_str(),
                    Some("preparing" | "starting" | "running" | "waiting" | "queued")
                )
        })
        .collect();
    let run_ids: HashSet<String> = retained_runs
        .iter()
        .map(|run| identity(run, "id"))
        .collect();
    let mut message_ids: HashSet<String> = items
        .iter()
        .filter(|item| item.get("messageId").is_some())
        .map(|item| identity(item, "messageId"))
        .collect();
    message_ids.extend(
        retained_runs
            .iter()
            .map(|run| identity(run, "userMessageId")),
    );
    control["messages"] = json!(
        rows(projection, "messages")
            .iter()
            .filter(|message| message_ids.contains(&identity(message, "id"))
                || (!message["runId"].is_null() && run_ids.contains(&identity(message, "runId")))
                || message.get("delegatedCompletion").is_some())
            .collect::<Vec<_>>()
    );
    control["turnItems"] = json!(items);
    control["visibleTurnItems"] = window["items"].clone();
    let exceeded = bytes(&control) > policy.bytes;
    json!({"snapshotSequence":sequence,"projection":control,"historyCursor":window["nextCursor"],"hasMoreHistory":window["hasMoreHistory"],"latestLocalTurnOrdinal":latest_local,"payloadBudgetExceeded":exceeded})
}

#[derive(Clone)]
pub struct HistoryService {
    store: Store,
}
impl HistoryService {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
    pub fn snapshot(&self, id: &str, bounded: bool) -> Result<Value, StoreError> {
        self.store.read(|connection| {
            let projection=read_projection(connection,"thread",id)?.ok_or_else(||StoreError::InvalidCommand("Thread not found.".into()))?;
            let sequence:u64=connection.query_row("SELECT COALESCE(MAX(sequence),0) FROM rust_application_events WHERE aggregate_kind='thread' AND aggregate_id=?",[id],|row|row.get(0))?;
            let projection=crate::wire_projection::projection(&projection);
            let value=if bounded {bounded_projection(&projection,sequence,PagePolicy::default())} else {json!({"snapshotSequence":sequence,"projection":projection})};
            if bounded {crate::execution::checked::<t3_contracts::ThreadBoundedSnapshot>(value)} else {crate::execution::checked::<t3_contracts::ThreadDetailSnapshot>(value)}
        })
    }
    pub fn page(&self, id: &str, cursor: &str) -> Result<Value, StoreError> {
        let cursor = decode_cursor(cursor)?;
        self.store.read(|connection| {
            let projection=read_projection(connection,"thread",id)?.ok_or_else(||StoreError::InvalidCommand("Thread not found.".into()))?;
            let sequence:u64=connection.query_row("SELECT COALESCE(MAX(sequence),0) FROM rust_application_events WHERE aggregate_kind='thread' AND aggregate_id=?",[id],|row|row.get(0))?;
            let projection=crate::wire_projection::projection(&projection);
            let visible=rows(&projection,"visibleTurnItems");
            let end=visible.iter().position(|row|row["sourceThreadId"]==cursor.st && row["sourceItemId"]==cursor.si).unwrap_or(cursor.p.min(visible.len()));
            let mut page=select_page(visible,end,sequence,PagePolicy{user_turns:Some(20),..PagePolicy::default()},None);
            page["snapshotSequence"]=json!(sequence);
            crate::execution::checked::<t3_contracts::ThreadHistoryPage>(page)
        })
    }
    pub fn detail(&self, input: Value) -> Result<Value, StoreError> {
        let input: t3_contracts::GetTurnItemInput = serde_json::from_value(input)?;
        self.store.read(|connection| {
            // The source ignores the optional revision and reads the current stored item.
            let projection = read_projection(connection, "thread", input.thread_id.as_str())?;
            let item = projection
                .as_ref()
                .and_then(|projection| {
                    rows(projection, "turnItems")
                        .iter()
                        .find(|item| item["id"] == input.item_id.as_str())
                })
                .map(crate::wire_projection::detail);
            crate::execution::checked::<t3_contracts::GetTurnItemResult>(json!({"item":item}))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(index: usize, thread: &str) -> Value {
        json!({"position":index,"visibility":if thread=="thread"{"local"}else{"inherited"},"sourceThreadId":thread,"sourceItemId":format!("item-{index}"),"item":{"id":format!("item-{index}"),"threadId":thread,"runId":null,"nodeId":null,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":index,"status":"completed","title":null,"startedAt":null,"completedAt":null,"updatedAt":"2026-01-01T00:00:00.000Z","type":"command_execution","input":"echo example","output":"output"}})
    }
    fn user(mut row: Value, actor: &str, intent: &str) -> Value {
        let message_id = format!("message-{}", row["position"]);
        let object = row["item"].as_object_mut().unwrap();
        object.remove("input");
        object.remove("output");
        object.insert("type".into(), json!("user_message"));
        object.insert("createdBy".into(), json!(actor));
        object.insert("creationSource".into(), json!("web"));
        object.insert("inputIntent".into(), json!(intent));
        object.insert("messageId".into(), json!(message_id));
        row["item"]["text"] = json!("User prompt");
        row["item"]["attachments"] = json!([]);
        row
    }
    #[test]
    fn conversation_pages_preserve_complete_turns_and_twenty_older_user_inputs() {
        let mut timeline = vec![];
        for turn in 0..33 {
            timeline.push(user(row(timeline.len(), "thread"), "user", "turn_start"));
            for _ in 0..80 {
                timeline.push(row(timeline.len(), "thread"));
            }
            if turn == 32 {
                timeline.push(user(row(timeline.len(), "thread"), "user", "steer"));
            }
        }
        let recent = select_page(&timeline, timeline.len(), 7, PagePolicy::default(), None);
        assert_eq!(rows(&recent, "items").len(), 811);
        assert_eq!(rows(&recent, "items")[0]["item"]["type"], "user_message");
        let cursor = decode_cursor(recent["nextCursor"].as_str().unwrap()).unwrap();
        assert_eq!(cursor.p, 23 * 81);
        let older = select_page(
            &timeline,
            cursor.p,
            8,
            PagePolicy {
                user_turns: Some(20),
                ..PagePolicy::default()
            },
            None,
        );
        assert_eq!(rows(&older, "items").len(), 20 * 81);
        assert_eq!(
            decode_cursor(older["nextCursor"].as_str().unwrap())
                .unwrap()
                .p,
            3 * 81
        );
        let mut huge = timeline[timeline.len() - 82..].to_vec();
        huge.last_mut().unwrap()["item"]["text"] = json!("x".repeat(MAX_ENCODED_BYTES + 1));
        let page = select_page(&huge, huge.len(), 8, PagePolicy::default(), None);
        assert_eq!(rows(&page, "items").len(), 82);
    }
    #[test]
    fn histories_without_user_turns_obey_budgets_and_agent_raw_turns_are_capped() {
        let mut timeline: Vec<_> = (0..120).map(|index| row(index, "thread")).collect();
        timeline[0] = user(timeline[0].clone(), "agent", "turn_start");
        let recent = select_page(&timeline, 120, 1, PagePolicy::default(), None);
        assert_eq!(rows(&recent, "items").len(), 75);
        let mut huge = vec![row(0, "thread"), row(1, "thread")];
        huge[0]["item"]["output"] = json!("x".repeat(2_000_000));
        let policy = PagePolicy {
            user_turns: None,
            items: 50,
            bytes: 1024,
        };
        let recent = select_page(&huge, 2, 1, policy, None);
        assert_eq!(rows(&recent, "items").len(), 1);
        let older = select_page(&huge, 1, 1, policy, None);
        assert_eq!(rows(&older, "items").len(), 1);
        assert_eq!(older["hasMoreHistory"], false);
        let mut turns = vec![user(row(0, "thread"), "user", "turn_start")];
        turns.extend((1..201).map(|index| user(row(index, "thread"), "agent", "turn_start")));
        assert_eq!(
            rows(
                &select_page(&turns, 201, 1, PagePolicy::default(), None),
                "items"
            )
            .len(),
            150
        );
    }
    #[test]
    fn cursor_codec_matches_node_permissive_base64_and_integer_json_numbers() {
        let text = r#"{"v":1.0,"seq":9.0,"st":"source","si":"item","p":3.0}"#;
        let encoded = URL_SAFE_NO_PAD.encode(text);
        for value in [
            encoded.clone(),
            format!("{encoded}=="),
            format!("{}\n{}", &encoded[..5], &encoded[5..]),
            format!("{}!{}", &encoded[..5], &encoded[5..]),
        ] {
            let cursor = decode_cursor(&value).unwrap();
            assert_eq!((cursor.seq, cursor.p), (9, 3));
        }
        for text in [
            r#"{"v":1,"seq":-1,"st":"s","si":"i","p":0}"#,
            r#"{"v":1,"seq":1,"st":"s","si":"i","p":0.5}"#,
            r#"{"v":1,"seq":1,"st":"","si":"i","p":0}"#,
            "{}",
        ] {
            assert!(decode_cursor(&URL_SAFE_NO_PAD.encode(text)).is_err());
        }
        assert!(decode_cursor(&"A".repeat(4097)).is_err());
    }
    #[test]
    fn bounded_projection_preserves_dependencies_control_messages_and_hidden_local_watermark() {
        let visible: Vec<_> = (0..100).map(|index| row(index, "ancestor")).collect();
        let interrupt =
            json!({"id":"interrupt","type":"run_interrupt_request","ordinal":123,"message":"Stop"});
        let hidden = json!({"id":"hidden","type":"command_execution","ordinal":999});
        let projection = json!({"thread":{"id":"thread"},"visibleTurnItems":visible,"turnItems":[interrupt,hidden],"runs":[{"id":"old","ordinal":1,"status":"completed","userMessageId":"old-user"},{"id":"live","ordinal":2,"status":"running","userMessageId":"live-user"},{"id":"latest","ordinal":3,"status":"completed","userMessageId":"latest-user"}],"messages":[{"id":"old-user","runId":"old"},{"id":"live-user","runId":"live"},{"id":"live-assistant","runId":"live"},{"id":"latest-user","runId":"latest"},{"id":"delegated","runId":null,"delegatedCompletion":{}}],"plans":[{"id":"active","status":"active"},{"id":"old","status":"completed"}],"contextHandoffs":[{"id":"pending","status":"pending"},{"id":"ready","status":"ready"},{"id":"old","status":"completed"}]});
        let bounded = bounded_projection(&projection, 4, PagePolicy::default());
        let projection = &bounded["projection"];
        assert_eq!(bounded["latestLocalTurnOrdinal"], 999);
        assert_eq!(projection["turnItems"], json!([interrupt]));
        assert_eq!(rows(projection, "visibleTurnItems").len(), 75);
        assert_eq!(rows(projection, "messages").len(), 4);
        assert_eq!(rows(projection, "plans").len(), 1);
        assert_eq!(rows(projection, "contextHandoffs").len(), 2);
        let row = row(0, "thread");
        assert!(bytes(&row) + bytes(&row["item"]) > bytes(&row));
        let no_turn = PagePolicy {
            user_turns: None,
            items: 75,
            bytes: bytes(&row) * 2 + bytes(&row["item"]) - 1,
        };
        assert_eq!(
            rows(
                &select_page(
                    &[row.clone(), row.clone()],
                    2,
                    4,
                    no_turn,
                    Some(&json!({"id":"thread"}))
                ),
                "items"
            )
            .len(),
            1
        );
        assert_eq!(
            rows(
                &select_page(&[row.clone(), row], 2, 4, no_turn, None),
                "items"
            )
            .len(),
            2
        );
    }
    #[test]
    fn persisted_windows_survive_reopen_cursor_movement_and_keep_full_detail_private() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let store = Store::open(&path).unwrap();
        crate::project::ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":directory.path()}),chrono::Utc::now()).unwrap();
        let result=crate::launch::ThreadLaunchService::new(store.clone()).launch(json!({"commandId":"launch","threadId":"thread","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}),chrono::Utc::now()).unwrap();
        let mut projection = result["projection"].clone();
        let mut visible: Vec<_> = (0..120)
            .map(|index| row(index, if index < 90 { "ancestor" } else { "thread" }))
            .collect();
        visible[100]["item"]["output"] = json!("kept in persistence ".repeat(30_000));
        projection["visibleTurnItems"] = json!(visible);
        projection["turnItems"] = json!(
            visible[90..]
                .iter()
                .map(|row| row["item"].clone())
                .collect::<Vec<_>>()
        );
        let projection =
            crate::execution::checked::<t3_contracts::ThreadProjection>(projection).unwrap();
        store.read(|connection|{connection.execute("UPDATE rust_projections SET state_json=? WHERE aggregate_kind='thread' AND aggregate_id='thread'",[projection.to_string()])?;Ok(())}).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let service = HistoryService::new(store.clone());
        let bounded = service.snapshot("thread", true).unwrap();
        assert_eq!(rows(&bounded["projection"], "visibleTurnItems").len(), 75);
        assert_eq!(bounded["latestLocalTurnOrdinal"], 119);
        assert!(bounded.to_string().len() < 100_000);
        assert_eq!(
            bounded["projection"]["visibleTurnItems"][55]["item"]["outputOmitted"],
            true
        );
        let cursor = bounded["historyCursor"].as_str().unwrap();
        let page = service.page("thread", cursor).unwrap();
        assert_eq!(rows(&page, "items").len(), 45);
        assert_eq!(page["hasMoreHistory"], false);
        let detail = service
            .detail(json!({"threadId":"thread","itemId":"item-100","revision":"old"}))
            .unwrap();
        assert!(detail["item"]["output"].as_str().unwrap().len() >= ON_DEMAND_TEST);
        assert!(
            detail["item"]["output"]
                .as_str()
                .unwrap()
                .ends_with("output truncated for transport")
        );
        assert_eq!(
            service
                .detail(json!({"threadId":"thread","itemId":"item-1"}))
                .unwrap()["item"],
            Value::Null
        );
        let mut moved = projection.clone();
        let list = moved["visibleTurnItems"].as_array_mut().unwrap();
        list.insert(0, row(400, "ancestor"));
        for (position, row) in list.iter_mut().enumerate() {
            row["position"] = json!(position);
        }
        store.read(|connection|{connection.execute("UPDATE rust_projections SET state_json=? WHERE aggregate_kind='thread' AND aggregate_id='thread'",[moved.to_string()])?;Ok(())}).unwrap();
        assert_eq!(
            rows(&service.page("thread", cursor).unwrap(), "items").len(),
            46
        );
        moved["visibleTurnItems"]
            .as_array_mut()
            .unwrap()
            .retain(|row| row["sourceItemId"] != "item-45");
        store.read(|connection|{connection.execute("UPDATE rust_projections SET state_json=? WHERE aggregate_kind='thread' AND aggregate_id='thread'",[moved.to_string()])?;Ok(())}).unwrap();
        assert_eq!(
            rows(&service.page("thread", cursor).unwrap(), "items").len(),
            45
        );
    }
    const ON_DEMAND_TEST: usize = 256 * 1024;
}
