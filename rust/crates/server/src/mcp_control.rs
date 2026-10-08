//! Cooperative thread tools over the authoritative application store.
#[path = "mcp_control_parameters.rs"]
mod parameters;
use crate::{
    execution::ExecutionService,
    mcp_access::{self, Access, Caller, CallerLimits},
    mcp_invocation::{InvocationScope, McpCapability, McpFailure, refusal},
    persistence::{Receipt, Store, StoreError},
    thread,
};
use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::sync::Arc;
use t3_contracts::*;
#[derive(Clone)]
pub struct McpControlTools {
    pub store: Store,
    pub execution: Option<ExecutionService>,
    pub clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}
fn rows<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn active(run: &Value) -> bool {
    matches!(
        run["status"].as_str(),
        Some("preparing" | "starting" | "running" | "waiting")
    )
}
fn terminal(run: &Value) -> bool {
    matches!(
        run["status"].as_str(),
        Some("completed" | "failed" | "cancelled" | "interrupted" | "rolled_back")
    )
}
fn latest(projection: &Value) -> Option<&Value> {
    rows(projection, "runs")
        .iter()
        .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0))
}
fn live(projection: &Value) -> Option<&Value> {
    rows(projection, "runs")
        .iter()
        .filter(|run| active(run))
        .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0))
}
fn valid<T: DeserializeOwned + Serialize>(value: Value) -> Result<Value, McpFailure> {
    serde_json::from_value::<T>(value)
        .and_then(serde_json::to_value)
        .map_err(|_| mcp_access::unavailable())
}
fn opt<T>(value: Option<Option<T>>) -> Option<T> {
    value.flatten()
}
fn encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            output.push(byte as char)
        } else {
            output.push_str(&format!("%{byte:02X}"))
        }
    }
    output
}
pub fn operation_id(prefix: &str, scope: &InvocationScope, operation: &str, key: &str) -> String {
    format!(
        "{prefix}:mcp:{}:{}:{}",
        encode(&scope.request_namespace),
        encode(operation),
        encode(key)
    )
}
fn link(scope: &InvocationScope, id: &str, title: &str) -> String {
    let cleaned = title
        .chars()
        .map(|character| {
            if matches!(character, '[' | ']' | '\\' | '\r' | '\n') {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let mut label = String::new();
    let mut whitespace = false;
    for character in cleaned.chars() {
        let mut buffer = [0; 4];
        let blank = trim_wire_string(character.encode_utf8(&mut buffer)).is_empty();
        if blank {
            whitespace = true;
        } else {
            if whitespace && !label.is_empty() {
                label.push(' ');
            }
            whitespace = false;
            label.push(character);
        }
    }
    // serde strings cannot represent half a surrogate; preserve complete scalars.
    let mut units = 0;
    label = label
        .chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= 120
        })
        .collect();
    if label.is_empty() {
        label = "Untitled thread".into();
    }
    let segment = |text: &str| encode(text).replace('(', "%28").replace(')', "%29");
    format!(
        "[{label}](t3-thread://v1/{}/{})",
        segment(scope.environment_id.as_str()),
        segment(id)
    )
}
fn date(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str().and_then(|value| value.parse().ok())
}
fn snoozed(shell: &Value, now: DateTime<Utc>) -> bool {
    date(&shell["snoozedUntil"]).is_some_and(|until| until > now)
}
fn view_item(scope: &InvocationScope, shell: &Value, now: DateTime<Utc>) -> Value {
    let settled = shell["settledOverride"] == "settled";
    let snooze = snoozed(shell, now);
    json!({"threadId":shell["id"],"link":link(scope,shell["id"].as_str().unwrap(),shell["title"].as_str().unwrap_or("")),"title":shell["title"],"createdBy":shell["createdBy"],"creationSource":shell["creationSource"],"status":shell.get("activityRunStatus").filter(|status|!status.is_null()).unwrap_or(&shell["status"]),"latestRunId":shell["latestRunId"],"providerInstanceId":shell["modelSelection"]["instanceId"],"model":shell["modelSelection"]["model"],"runtimeMode":shell["runtimeMode"],"interactionMode":shell["interactionMode"],"linkedPullRequest":shell["linkedPullRequest"],"settled":settled,"settledAt":if settled{shell["settledAt"].clone()}else{Value::Null},"snoozed":snooze,"snoozedUntil":if snooze{shell["snoozedUntil"].clone()}else{Value::Null},"parentThreadId":shell["lineage"]["parentThreadId"],"relationshipToParent":shell["lineage"]["relationshipToParent"],"itemCount":shell["visibleItemCount"],"createdAt":shell["createdAt"],"updatedAt":shell["updatedAt"]})
}
impl McpControlTools {
    pub fn registered(name: &str) -> bool {
        parameters::NAMES.contains(&name)
    }
    pub fn catalog() -> Vec<Value> {
        serde_json::from_str::<Vec<Value>>(include_str!("mcp_control_catalog.json"))
            .expect("source-generated tool catalog")
            .into_iter()
            .filter(|tool| Self::registered(tool["name"].as_str().unwrap()))
            .collect()
    }
    pub async fn call(
        &self,
        scope: &InvocationScope,
        name: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        let arguments = parameters::decode(name, arguments)?;
        // Raw schema admission above rejects null; JSON codecs preserve null separately.
        let decode_error = |error: serde_json::Error| error.to_string();
        let result = match name {
            "t3_thread_list" => self.list(
                scope,
                serde_json::from_value(arguments).map_err(decode_error)?,
            ),
            "t3_thread_read" => self.read(
                scope,
                serde_json::from_value(arguments).map_err(decode_error)?,
            ),
            "t3_thread_wait" => {
                self.wait(
                    scope,
                    serde_json::from_value(arguments).map_err(decode_error)?,
                )
                .await
            }
            "t3_thread_interrupt" => self.interrupt(
                scope,
                serde_json::from_value(arguments).map_err(decode_error)?,
            ),
            _ => unreachable!(),
        };
        Ok(match result {
            Ok(value) => {
                json!({"isError":false,"structuredContent":value,"content":[{"type":"text","text":value.to_string()}]})
            }
            Err(error) => {
                json!({"isError":true,"content":[{"type":"text","text":error.0.to_string()}]})
            }
        })
    }
    fn caller(&self, scope: &InvocationScope) -> Result<Caller, McpFailure> {
        if !scope.capabilities.contains(&McpCapability::Orchestration) {
            return Err(refusal(
                "capability_denied",
                "This MCP credential does not grant orchestration capabilities.",
            ));
        }
        self.store
            .read(|connection| Ok(mcp_access::load_caller(connection, scope, false)))
            .map_err(|_| mcp_access::unavailable())?
    }
    fn target(&self, id: &str) -> Result<Value, McpFailure> {
        self.store
            .projection("thread", id)
            .map_err(|_| mcp_access::unavailable())?
            .filter(|projection| projection["thread"]["deletedAt"].is_null())
            .ok_or_else(|| refusal("thread_not_found", &format!("Thread {id} was not found.")))
    }
    fn writable(&self, scope: &InvocationScope, id: &str) -> Result<CallerLimits, McpFailure> {
        let caller = mcp_access::check_store(&self.store, scope, Access::WritesThreads, &[id])?;
        self.caller(scope)?;
        Ok(caller.limits)
    }
    pub fn list(
        &self,
        scope: &InvocationScope,
        input: OrchestratorMcpThreadListInput,
    ) -> Result<Value, McpFailure> {
        let caller = self.caller(scope)?;
        let project = caller.project_id(opt(input.project_id).as_ref().map(ProjectId::as_str))?;
        let now = (self.clock)();
        let statuses = opt(input.statuses);
        let title = opt(input.title_contains).map(|value| value.0.as_str().to_lowercase());
        let include = opt(input.include_subagents) != Some(false);
        let settled = opt(input.settled);
        let snooze = opt(input.snoozed);
        let mut shells = self
            .store
            .projections("thread")
            .map_err(|_| mcp_access::unavailable())?
            .into_iter()
            .filter(|projection| {
                projection["thread"]["projectId"] == project
                    && projection["thread"]["deletedAt"].is_null()
                    && projection["thread"]["archivedAt"].is_null()
            })
            .map(|projection| thread::shell(&projection))
            .filter(|shell| include || shell["lineage"]["relationshipToParent"] != "subagent")
            .filter(|shell| {
                statuses.as_ref().is_none_or(|statuses| {
                    statuses.0.iter().any(|status| {
                        json!(status)
                            == shell
                                .get("activityRunStatus")
                                .filter(|status| !status.is_null())
                                .unwrap_or(&shell["status"])
                                .clone()
                    })
                })
            })
            .filter(|shell| {
                title.as_ref().is_none_or(|title| {
                    shell["title"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(title)
                })
            })
            .filter(|shell| {
                settled.is_none_or(|value| (shell["settledOverride"] == "settled") == value)
            })
            .filter(|shell| snooze.is_none_or(|value| snoozed(shell, now) == value))
            .collect::<Vec<_>>();
        shells.sort_by(|left, right| {
            date(&right["updatedAt"])
                .cmp(&date(&left["updatedAt"]))
                .then_with(|| {
                    crate::workspace_entries::collate(
                        right["id"].as_str().unwrap(),
                        left["id"].as_str().unwrap(),
                    )
                })
        });
        let cursor = opt(input.cursor).map(|value| value.0 as usize).unwrap_or(0);
        let limit = opt(input.limit).map(|value| value.0 as usize).unwrap_or(50);
        let total = shells.len();
        let page = shells
            .into_iter()
            .skip(cursor)
            .take(limit)
            .map(|shell| view_item(scope, &shell, now))
            .collect::<Vec<_>>();
        let next = if cursor.saturating_add(page.len()) < total {
            Some(cursor + page.len())
        } else {
            None
        };
        valid::<OrchestratorMcpThreadListResult>(
            json!({"projectId":project,"currentThreadId":scope.thread.as_ref().map(|thread|thread.thread_id.as_str()),"threads":page,"nextCursor":next,"total":total}),
        )
    }
    pub async fn wait(
        &self,
        scope: &InvocationScope,
        input: OrchestratorMcpThreadWaitInput,
    ) -> Result<Value, McpFailure> {
        self.caller(scope)?;
        let id = input.thread_id.as_str();
        let mut updates = self.store.subscribe();
        let projection = self.target(id)?;
        let requested = opt(input.run_id);
        let run = match &requested {
            Some(run_id) => rows(&projection, "runs")
                .iter()
                .find(|run| run["id"] == run_id.as_str())
                .ok_or_else(|| {
                    refusal(
                        "run_not_found",
                        &format!("Run {run_id} does not belong to thread {id}."),
                    )
                })?
                .clone(),
            None => match latest(&projection) {
                Some(run) => run.clone(),
                None => {
                    return valid::<OrchestratorMcpThreadWaitResult>(
                        json!({"threadId":id,"runId":null,"status":"idle","timedOut":false}),
                    );
                }
            },
        };
        let selected = run["id"]
            .as_str()
            .ok_or_else(mcp_access::unavailable)?
            .to_owned();
        let millis = opt(input.timeout_ms)
            .and_then(|number| number.as_f64())
            .unwrap_or(600_000.0)
            .clamp(1.0, 3_600_000.0);
        let deadline = tokio::time::sleep(std::time::Duration::from_secs_f64(millis / 1000.0));
        tokio::pin!(deadline);
        let mut current = run;
        loop {
            if terminal(&current) {
                return valid::<OrchestratorMcpThreadWaitResult>(
                    json!({"threadId":id,"runId":selected,"status":current["status"],"timedOut":false}),
                );
            }
            let expired = tokio::select! {_=&mut deadline=>true,result=updates.recv()=>{match result{Ok(events) if !events.iter().any(|event|event.event.aggregate_id==id&&(event.event.event_type=="thread.deleted"||event.event.event_type=="run.updated"&&event.event.payload["id"]==selected))=>continue,Err(tokio::sync::broadcast::error::RecvError::Closed)=>return Err(mcp_access::unavailable()),_=>{}}false}};
            let projection = self.target(id)?;
            current = rows(&projection, "runs")
                .iter()
                .find(|run| run["id"] == selected)
                .cloned()
                .ok_or_else(|| {
                    refusal(
                        "run_not_found",
                        &format!("Run {selected} does not belong to thread {id}."),
                    )
                })?;
            if expired {
                return valid::<OrchestratorMcpThreadWaitResult>(
                    json!({"threadId":id,"runId":selected,"status":current["status"],"timedOut":!terminal(&current)}),
                );
            }
        }
    }
    fn execute(&self, input: &Value, limits: CallerLimits) -> Result<Receipt, McpFailure> {
        let refused = std::cell::RefCell::new(None);
        let result = self
            .execution
            .as_ref()
            .ok_or_else(mcp_access::unavailable)?
            .dispatch_guarded(input, (self.clock)(), |_, projection| {
                mcp_access::recheck_limits(limits, &projection["thread"]).map_err(|error| {
                    *refused.borrow_mut() = Some(error);
                    StoreError::InvalidCommand("MCP admission refused".into())
                })
            });
        match result {
            Ok(receipt) if receipt.status == "accepted" => Ok(receipt),
            Ok(receipt) => Err(receipt
                .error
                .as_ref()
                .map(mcp_access::dispatch_failure)
                .unwrap_or_else(mcp_access::unavailable)),
            Err(error) => Err(refused.into_inner().unwrap_or_else(|| match error {
                StoreError::InvalidCommand(message) => refusal(
                    "orchestration_error",
                    &message.chars().take(1000).collect::<String>(),
                ),
                _ => mcp_access::unavailable(),
            })),
        }
    }
    pub fn send(
        &self,
        scope: &InvocationScope,
        input: OrchestratorMcpThreadSendInput,
    ) -> Result<Value, McpFailure> {
        let id = input.thread_id.as_str();
        let limits = self.writable(scope, id)?;
        let target = self.target(id)?;
        if !target["thread"]["archivedAt"].is_null() {
            return Err(refusal(
                "thread_not_sendable",
                &format!("Thread {id} is archived and cannot receive messages."),
            ));
        }
        let mode = opt(input.mode).unwrap_or(McpThreadSendMode::Auto);
        let steerable = rows(&target, "runs")
            .iter()
            .filter(|run| {
                run["status"] == "running"
                    && !run["activeAttemptId"].is_null()
                    && rows(&target, "providerTurns").iter().any(|turn| {
                        turn["runAttemptId"] == run["activeAttemptId"]
                            && turn["status"] == "running"
                    })
            })
            .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0));
        let dispatch_mode = match mode {
            McpThreadSendMode::Steer | McpThreadSendMode::Restart => {
                let run = steerable.ok_or_else(|| {
                    refusal(
                        "thread_not_sendable",
                        &format!(
                            "Thread {id} has no running turn that can be {}.",
                            if mode == McpThreadSendMode::Steer {
                                "steered"
                            } else {
                                "restarted"
                            }
                        ),
                    )
                })?;
                json!({"type":if mode == McpThreadSendMode::Steer {"steer_active"}else{"restart_active"},"targetRunId":run["id"]})
            }
            McpThreadSendMode::Auto if steerable.is_some() => {
                json!({"type":"steer_active","targetRunId":steerable.unwrap()["id"]})
            }
            McpThreadSendMode::Auto => json!({"type":"start_immediately"}),
            McpThreadSendMode::Queue => json!({"type":"queue_after_active"}),
        };
        let key = opt(input.client_request_id)
            .map(|value| value.0.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let message_id = operation_id("message", scope, "thread-send", &key);
        let mut command = json!({"type":"message.dispatch","commandId":operation_id("command",scope,"thread-send",&key),"threadId":id,"messageId":message_id,"text":input.message.0,"attachments":[],"dispatchMode":dispatch_mode,"createdBy":"agent","creationSource":"mcp"});
        if let Some(parent) = &scope.thread {
            command["senderThreadId"] = json!(parent.thread_id);
        }
        self.execute(&command, limits)?;
        let projection = self.target(id)?;
        let missing = || {
            refusal(
                "orchestration_error",
                &format!(
                    "Message {message_id} was accepted on thread {id} without a durable run projection."
                ),
            )
        };
        let message = rows(&projection, "messages")
            .iter()
            .find(|message| message["id"] == message_id)
            .ok_or_else(missing)?;
        let run = rows(&projection, "runs")
            .iter()
            .find(|run| run["id"] == message["runId"])
            .ok_or_else(missing)?;
        let item = rows(&projection, "turnItems")
            .iter()
            .find(|item| item["type"] == "user_message" && item["messageId"] == message_id);
        if item.is_none() && run["status"] != "queued" {
            return Err(missing());
        }
        let delivery = match item {
            None => "queued",
            Some(item) if item["inputIntent"] == "queued_turn" => "queued",
            Some(item) if item["inputIntent"] == "turn_start" => "started",
            Some(_) if mode == McpThreadSendMode::Restart => "restarted",
            Some(_) => "steered",
        };
        valid::<OrchestratorMcpThreadSendResult>(
            json!({"threadId":id,"messageId":message_id,"runId":run["id"],"status":run["status"],"delivery":delivery}),
        )
    }
    pub fn interrupt(
        &self,
        scope: &InvocationScope,
        input: OrchestratorMcpThreadInterruptInput,
    ) -> Result<Value, McpFailure> {
        let id = input.thread_id.as_str();
        let limits = self.writable(scope, id)?;
        let target = self.target(id)?;
        let specified = opt(input.run_id);
        let selected = match &specified {
            Some(run_id) => Some(
                rows(&target, "runs")
                    .iter()
                    .find(|run| run["id"] == run_id.as_str())
                    .ok_or_else(|| {
                        refusal(
                            "run_not_found",
                            &format!("Run {run_id} does not belong to thread {id}."),
                        )
                    })?,
            ),
            None => live(&target),
        };
        let Some(run) = selected else {
            return valid::<OrchestratorMcpThreadInterruptResult>(
                json!({"threadId":id,"runId":null,"status":"no_active_run"}),
            );
        };
        if terminal(run) {
            return valid::<OrchestratorMcpThreadInterruptResult>(
                json!({"threadId":id,"runId":run["id"],"status":run["status"]}),
            );
        }
        if !active(run) || live(&target).is_none_or(|active| active["id"] != run["id"]) {
            return Err(refusal(
                "thread_not_interruptible",
                &format!(
                    "Run {} is not currently interruptible.",
                    run["id"].as_str().unwrap()
                ),
            ));
        }
        let key = opt(input.client_request_id)
            .map(|value| value.0.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut command = json!({"type":"run.interrupt","commandId":operation_id("command",scope,"thread-interrupt",&key),"threadId":id,"runId":run["id"]});
        if let Some(reason) = opt(input.reason) {
            command["reason"] = json!(reason.0);
        }
        self.execute(&command, limits)?;
        valid::<OrchestratorMcpThreadInterruptResult>(
            json!({"threadId":id,"runId":run["id"],"status":"interrupt_requested"}),
        )
    }
    pub fn update(
        &self,
        scope: &InvocationScope,
        input: ThreadMetadataMcpUpdateInput,
    ) -> Result<Value, McpFailure> {
        let input = input.0;
        let caller = mcp_access::check_store(
            &self.store,
            scope,
            Access::WritesThreads,
            &opt(input.thread_id.clone())
                .as_ref()
                .map(ThreadId::as_str)
                .into_iter()
                .collect::<Vec<_>>(),
        )?;
        self.caller(scope)?;
        let id = caller.thread_id(opt(input.thread_id).as_ref().map(ThreadId::as_str))?;
        let target = self.target(&id)?;
        let key = opt(input.client_request_id)
            .map(|value| value.0.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let action = json!(input.action).as_str().unwrap().to_owned();
        let command_id = format!(
            "command:mcp:{}:thread-update:{}:{}:{}",
            encode(&scope.request_namespace),
            encode(&id),
            encode(&action),
            encode(&key)
        );
        let mut command =
            json!({"type":"thread.metadata.update","commandId":command_id,"threadId":id});
        match input.action {
            ThreadMetadataMcpAction::Rename => {
                command["title"] = json!(opt(input.title).unwrap().0)
            }
            ThreadMetadataMcpAction::RegenerateTitle => command["regenerateTitle"] = json!(true),
            ThreadMetadataMcpAction::LinkPullRequest => {
                let request = opt(input.pull_request).unwrap();
                command["linkedPullRequest"] = json!({"projectId":target["thread"]["projectId"],"repository":request.repository,"number":request.number,"url":request.url});
            }
            ThreadMetadataMcpAction::UnlinkPullRequest => {
                command["linkedPullRequest"] = Value::Null
            }
        }
        let refused = std::cell::RefCell::new(None);
        let receipt = thread::ThreadService::new(self.store.clone())
            .dispatch_guarded(&command, (self.clock)(), |_, projection| {
                let target = projection
                    .ok_or_else(|| StoreError::InvalidCommand("Thread not found".into()))?;
                mcp_access::recheck_limits(caller.limits, &target["thread"]).map_err(|error| {
                    *refused.borrow_mut() = Some(error);
                    StoreError::InvalidCommand("MCP admission refused".into())
                })
            })
            .map_err(|_| refused.into_inner().unwrap_or_else(mcp_access::unavailable))?;
        if receipt.status != "accepted" {
            return Err(receipt
                .error
                .as_ref()
                .map(mcp_access::dispatch_failure)
                .unwrap_or_else(mcp_access::unavailable));
        }
        // Receipt replay returns the original event, not today's thread title.
        let event=self.store.read(|connection|{let payload:String=connection.query_row("SELECT event_json FROM rust_application_events WHERE command_id=?1 AND event_type='thread.metadata-updated' ORDER BY sequence LIMIT 1",[&command_id],|row|row.get(0))?;Ok(serde_json::from_str::<crate::persistence::Event>(&payload)?.payload)}).map_err(|_|mcp_access::unavailable())?;
        valid::<ThreadMetadataMcpUpdateResult>(
            json!({"threadId":id,"action":input.action,"commandId":command_id,"sequence":receipt.result_sequence,"title":event["title"],"titleRegeneration":event["titleRegeneration"],"linkedPullRequest":event["linkedPullRequest"],"updatedAt":event["updatedAt"]}),
        )
    }
}
fn pretty(value: &Value) -> String {
    let normalized: Value = serde_json::from_str(&crate::device_actions::stringify(value))
        .unwrap_or_else(|_| value.clone());
    serde_json::to_string_pretty(&normalized).unwrap_or_default()
}
fn pick(item: &Value, keys: &[&str]) -> Value {
    let mut object = serde_json::Map::new();
    for key in keys {
        if let Some(value) = item.get(*key) {
            object.insert((*key).into(), value.clone());
        }
    }
    Value::Object(object)
}
fn fragment(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        value => crate::device_actions::stringify(value),
    }
}
pub(crate) fn turn_item_text(item: &Value) -> Option<String> {
    let join =
        |values: Vec<Option<String>>| values.into_iter().flatten().collect::<Vec<_>>().join("\n");
    let text = |key: &str| item.get(key).map(fragment);
    match item["type"].as_str().unwrap_or("") {
        "notification" => Some(join(vec![text("summary"), text("detail")])),
        "user_message" | "assistant_message" | "reasoning" => text("text"),
        "proposed_plan" => text("markdown"),
        "todo_list" => {
            let mut lines = vec![text("explanation")];
            lines.extend(rows(item, "steps").iter().map(|step| {
                Some(format!(
                    "[{}] {}",
                    fragment(&step["status"]),
                    fragment(&step["text"])
                ))
            }));
            Some(join(lines))
        }
        "user_input_request" => Some(pretty(&item["questions"])),
        "file_change" => Some(join(vec![
            text("fileName"),
            if item.get("additions").is_none() && item.get("deletions").is_none() {
                None
            } else {
                Some(format!(
                    "+{} -{}",
                    fragment(
                        item.get("additions")
                            .filter(|v| !v.is_null())
                            .unwrap_or(&json!(0))
                    ),
                    fragment(
                        item.get("deletions")
                            .filter(|v| !v.is_null())
                            .unwrap_or(&json!(0))
                    )
                ))
            },
            item.get("diffStr")
                .filter(|value| !value.is_null())
                .or_else(|| item.get("newStr"))
                .map(fragment),
        ])),
        "command_execution" => Some(join(vec![
            Some(format!(
                "$ {}",
                item.get("input")
                    .map(fragment)
                    .unwrap_or_else(|| "undefined".into())
            )),
            text("output"),
        ])),
        "file_search" => Some(pretty(&pick(item, &["pattern", "results"]))),
        "web_search" => Some(pretty(&pick(item, &["patterns", "results"]))),
        "approval_request" => item
            .get("prompt")
            .filter(|value| !value.is_null())
            .or_else(|| item.get("requestKind"))
            .map(fragment),
        "checkpoint" => Some(pretty(&item["files"])),
        "run_interrupt_request" | "run_interrupt_result" | "system_notice" => text("message"),
        "error" => item["failure"].get("message").map(fragment),
        "compaction" => item
            .get("summary")
            .filter(|value| !value.is_null())
            .map(fragment),
        "handoff" => Some(
            item.get("summary")
                .filter(|value| !value.is_null())
                .map(fragment)
                .unwrap_or_else(|| {
                    format!(
                        "{} handoff to {}",
                        fragment(&item["strategy"]),
                        fragment(&item["toProviderInstanceId"])
                    )
                }),
        ),
        "fork" => Some(format!(
            "Forked to thread {}.",
            fragment(&item["targetThreadId"])
        )),
        "thread_created" => Some(format!(
            "Created thread {} with {} ({}).",
            fragment(&item["targetThreadId"]),
            fragment(&item["targetProviderInstanceId"]),
            fragment(&item["targetModel"])
        )),
        "secret_request" => Some(format!(
            "Asked the user for {}: {}.",
            fragment(&item["label"]),
            fragment(&item["secretStatus"])
        )),
        "subagent" => item
            .get("result")
            .filter(|value| !value.is_null())
            .or_else(|| item.get("progress").filter(|value| !value.is_null()))
            .or_else(|| item.get("prompt"))
            .map(fragment),
        "dynamic_tool" => Some(pretty(&pick(item, &["toolName", "input", "output"]))),
        _ => None,
    }
}
impl McpControlTools {
    pub fn read(
        &self,
        scope: &InvocationScope,
        input: OrchestratorMcpThreadReadInput,
    ) -> Result<Value, McpFailure> {
        self.caller(scope)?;
        let id = input.thread_id.as_str();
        let target = self.target(id)?;
        let shell = thread::shell(&target);
        let now = (self.clock)();
        let selected = opt(input.item_id);
        let after = opt(input.after_position).map(|value| value.0);
        let activity = opt(input.view) == Some(McpThreadView::Activity);
        let limit = opt(input.limit).map(|value| value.0 as usize).unwrap_or(50);
        let run_limit = opt(input.run_limit)
            .map(|value| value.0 as usize)
            .unwrap_or(10);
        let max_chars = opt(input.max_chars_per_item)
            .map(|value| value.0 as usize)
            .unwrap_or(20000);
        let offset = if selected.is_some() {
            opt(input.text_offset)
                .map(|value| value.0 as usize)
                .unwrap_or(0)
        } else {
            0
        };
        let all = rows(&target, "visibleTurnItems");
        let matching = all
            .iter()
            .filter(|row| {
                if let Some(selected) = &selected {
                    row["sourceItemId"] == selected.as_str()
                } else {
                    after.is_none_or(|after| {
                        row["position"]
                            .as_u64()
                            .is_some_and(|position| position > after)
                    }) && (activity
                        || matches!(
                            row["item"]["type"].as_str(),
                            Some("user_message" | "assistant_message" | "proposed_plan")
                        ))
                }
            })
            .collect::<Vec<_>>();
        let page = matching.iter().take(limit).copied().collect::<Vec<_>>();
        let mut items = Vec::new();
        let mut sources = std::collections::HashMap::<String, Value>::new();
        for row in &page {
            let item = &row["item"];
            let full = turn_item_text(item);
            let end = offset.saturating_add(max_chars);
            let truncated = full
                .as_ref()
                .is_some_and(|text| text.encode_utf16().count() > end);
            let text = full
                .as_ref()
                .map(|text| {
                    let units = text
                        .encode_utf16()
                        .skip(offset)
                        .take(max_chars)
                        .collect::<Vec<_>>();
                    String::from_utf16(&units).map_err(|_| {
                        refusal(
                            "orchestration_error",
                            "The requested text boundary splits a Unicode surrogate pair.",
                        )
                    })
                })
                .transpose()?;
            let message_id = if matches!(
                item["type"].as_str(),
                Some("user_message" | "assistant_message")
            ) {
                item["messageId"].clone()
            } else {
                Value::Null
            };
            let source = if row["sourceThreadId"] == id {
                &target
            } else {
                let source_id = row["sourceThreadId"]
                    .as_str()
                    .ok_or_else(mcp_access::unavailable)?;
                if !sources.contains_key(source_id) {
                    sources.insert(source_id.to_owned(), self.target(source_id)?);
                }
                sources.get(source_id).unwrap()
            };
            let message = rows(source, "messages")
                .iter()
                .find(|message| message["id"] == message_id);
            items.push(json!({"position":row["position"],"visibility":row["visibility"],"sourceThreadId":row["sourceThreadId"],"itemId":row["sourceItemId"],"runId":item["runId"],"messageId":message_id,"createdBy":message.map(|message|message["createdBy"].clone()),"creationSource":message.map(|message|message["creationSource"].clone()),"type":item["type"],"status":item["status"],"title":item["title"],"text":text,"textTruncated":truncated,"nextTextOffset":if truncated{Some(end)}else{None},"updatedAt":item["updatedAt"]}));
        }
        let mut detail = view_item(scope, &shell, now);
        detail["projectId"] = target["thread"]["projectId"].clone();
        detail["activeRunId"] = live(&target)
            .map(|run| run["id"].clone())
            .unwrap_or(Value::Null);
        detail["latestRunId"] = latest(&target)
            .map(|run| run["id"].clone())
            .unwrap_or(Value::Null);
        detail["status"] = live(&target)
            .or_else(|| latest(&target))
            .map(|run| run["status"].clone())
            .unwrap_or(json!("idle"));
        detail["titleRegeneration"] = target["thread"]["titleRegeneration"].clone();
        detail["branch"] = target["thread"]["branch"].clone();
        detail["worktreePath"] = target["thread"]["worktreePath"].clone();
        detail["runCount"] = json!(rows(&target, "runs").len());
        detail["itemCount"] = json!(all.len());
        detail["pendingRequestCount"] = json!(
            rows(&target, "runtimeRequests")
                .iter()
                .filter(|request| request["status"] == "pending")
                .count()
        );
        detail["archived"] = json!(!target["thread"]["archivedAt"].is_null());
        let mut runs = rows(&target, "runs").iter().collect::<Vec<_>>();
        runs.sort_by_key(|run| std::cmp::Reverse(run["ordinal"].as_u64().unwrap_or(0)));
        let runs=runs.into_iter().take(run_limit).map(|run|json!({"runId":run["id"],"ordinal":run["ordinal"],"status":run["status"],"providerInstanceId":run["modelSelection"]["instanceId"],"model":run["modelSelection"]["model"],"requestedAt":run["requestedAt"],"startedAt":run["startedAt"],"completedAt":run["completedAt"]})).collect::<Vec<_>>();
        valid::<OrchestratorMcpThreadReadResult>(
            json!({"thread":detail,"recentRuns":runs,"items":items,"nextPosition":page.last().map(|row|row["position"].clone()),"hasMore":matching.len()>page.len()}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (McpControlTools, InvocationScope) {
        let (store, mut scope) = crate::mcp_invocation::tests::fixture();
        scope.capabilities.insert(McpCapability::Orchestration);
        scope.thread = None;
        scope.client = Some(crate::mcp_invocation::ClientCaller {
            session_id: "client".into(),
            label: "Client".into(),
            access: AuthMcpClientAccess::FullAccess,
        });
        (
            McpControlTools {
                store,
                execution: None,
                clock: Arc::new(|| "2026-01-01T00:00:00Z".parse().unwrap()),
            },
            scope,
        )
    }
    fn run(id: &str, ordinal: u64, status: &str) -> Value {
        json!({"id":id,"ordinal":ordinal,"status":status,"modelSelection":{"instanceId":"codex","model":"fixture"},"requestedAt":"2026-01-01T00:00:00.000Z","startedAt":null,"completedAt":null})
    }
    fn publish(tools: &McpControlTools, kind: &str, payload: Value) {
        let command = uuid::Uuid::new_v4().to_string();
        let event = crate::execution::event("thread:1", &command, kind, payload, (tools.clock)());
        tools
            .store
            .dispatch(
                &command,
                "thread",
                "thread:1",
                "test.publish",
                (tools.clock)(),
                |_| {
                    Ok(crate::persistence::Decision::Accepted {
                        events: vec![event],
                        effects: vec![],
                    })
                },
                thread::reduce,
            )
            .unwrap();
    }
    fn wait_input(run: Option<&str>, timeout: u64) -> OrchestratorMcpThreadWaitInput {
        let mut input = json!({"threadId":"thread:1","timeoutMs":timeout});
        if let Some(run) = run {
            input["runId"] = json!(run);
        }
        serde_json::from_value(input).unwrap()
    }
    #[tokio::test(start_paused = true)]
    async fn wait_keeps_selected_run_and_recovers_lag_without_polling_or_interrupt() {
        let (tools, scope) = fixture();
        publish(&tools, "run.updated", run("first", 1, "running"));
        let wait = tools.wait(&scope, wait_input(None, 600000));
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        publish(&tools, "run.updated", run("newer", 2, "queued"));
        for index in 0..300 {
            publish(
                &tools,
                "message.updated",
                json!({"id":format!("unrelated-{index}"),"text":"transcript"}),
            );
        }
        publish(&tools, "run.updated", run("first", 1, "completed"));
        let result = wait.await.unwrap();
        assert_eq!(result["runId"], "first");
        assert_eq!(result["status"], "completed");
        assert_eq!(result["timedOut"], false);
        let count = tools
            .store
            .read(|connection| {
                Ok(
                    connection.query_row("SELECT count(*) FROM rust_effect_outbox", [], |row| {
                        row.get::<_, u64>(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(count, 0);
    }
    #[tokio::test(start_paused = true)]
    async fn wait_final_projection_wins_timeout_and_deletion_is_reported() {
        let (tools, scope) = fixture();
        publish(&tools, "run.updated", run("first", 1, "waiting"));
        let wait = tools.wait(&scope, wait_input(Some("first"), 1));
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        publish(&tools, "run.updated", run("first", 1, "completed"));
        let result = wait.await.unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["timedOut"], false);
        publish(&tools, "run.updated", run("first", 1, "running"));
        let wait = tools.wait(&scope, wait_input(Some("first"), 1000));
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        let mut thread = tools.target("thread:1").unwrap()["thread"].clone();
        thread["deletedAt"] = json!("2026-01-01T00:00:00.000Z");
        publish(&tools, "thread.deleted", thread);
        assert_eq!(wait.await.unwrap_err().0["code"], "thread_not_found");
    }
    #[tokio::test(start_paused = true)]
    async fn wait_timeout_returns_current_status_and_never_stops_the_run() {
        let (tools, scope) = fixture();
        publish(&tools, "run.updated", run("first", 1, "running"));
        let wait = tools.wait(&scope, wait_input(None, 1));
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        let result = wait.await.unwrap();
        assert_eq!(
            result,
            json!({"threadId":"thread:1","runId":"first","status":"running","timedOut":true})
        );
        assert_eq!(
            tools.target("thread:1").unwrap()["runs"][0]["status"],
            "running"
        );
        assert!(
            tools.target("thread:1").unwrap()["turnItems"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn list_and_read_views_are_typed_and_metadata_replay_uses_original_event() {
        let (tools, scope) = fixture();
        let list = tools
            .list(
                &scope,
                serde_json::from_value(json!({"projectId":"project:1"})).unwrap(),
            )
            .unwrap();
        assert_eq!(list["threads"][0]["threadId"], "thread:1");
        assert_eq!(
            list["threads"][0]["link"],
            "[Thread](t3-thread://v1/environment-1/thread%3A1)"
        );
        let update = |title: &str, key: &str| {
            tools.update(&scope,serde_json::from_value(json!({"threadId":"thread:1","action":"rename","title":title,"clientRequestId":key})).unwrap()).unwrap()
        };
        let first = update("First", "once");
        let second = update("Second", "other");
        assert_ne!(first["sequence"], second["sequence"]);
        assert_eq!(update("Ignored on replay", "once"), first);
        assert_eq!(
            tools.target("thread:1").unwrap()["thread"]["title"],
            "Second"
        );
        let now = "2026-01-01T00:00:00.000Z";
        publish(
            &tools,
            "message.updated",
            json!({"id":"message","createdBy":"agent","creationSource":"mcp"}),
        );
        for (id, kind, text) in [
            ("u", "user_message", "A😀B"),
            ("activity", "reasoning", "hidden"),
            ("a", "assistant_message", "done"),
        ] {
            let mut item = json!({"id":id,"threadId":"thread:1","type":kind,"status":"completed","text":text,"title":null,"runId":null,"messageId":"message","updatedAt":now});
            if kind == "reasoning" {
                item.as_object_mut().unwrap().remove("messageId");
            }
            publish(&tools, "turn-item.updated", item);
        }
        let read = tools
            .read(
                &scope,
                serde_json::from_value(
                    json!({"threadId":"thread:1","limit":1,"maxCharsPerItem":3,"textOffset":100}),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(read["items"][0]["text"], "A😀");
        assert_eq!(read["items"][0]["nextTextOffset"], 3);
        assert_eq!(read["items"][0]["createdBy"], "agent");
        assert_eq!(read["nextPosition"], 0);
        assert_eq!(read["hasMore"], true);
        let item = tools
            .read(
                &scope,
                serde_json::from_value(
                    json!({"threadId":"thread:1","itemId":"u","textOffset":3,"maxCharsPerItem":1}),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(item["items"][0]["text"], "B");
        assert_eq!(item["items"][0]["textTruncated"], false);
        let activity = tools
            .read(
                &scope,
                serde_json::from_value(json!({"threadId":"thread:1","view":"activity"})).unwrap(),
            )
            .unwrap();
        assert_eq!(activity["items"].as_array().unwrap().len(), 3);
    }
}
