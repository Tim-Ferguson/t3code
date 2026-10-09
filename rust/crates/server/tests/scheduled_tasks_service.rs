//! The scheduled-task service against the original's: the same scripted scenarios the original service
//! ran (create, edit, enable, run, tick, delete, restart, webhooks) must give the same results, ask the
//! launcher for the same launches and leave the same task list after every step. Nothing here waits for
//! real time: the clock is manual and a tick is a call.
use chrono_tz::Tz;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Mutex},
};
use t3_server::{
    persistence::Store,
    scheduled_tasks::{LaunchRequest, ManualClock, RunLauncher, ScheduledTaskError, ScheduledTasks, WebhookTriggerRequest, WebhookTriggerResult},
    scheduled_tasks_schedule::ChronoZone,
    scheduled_tasks_webhook::WebhookRequest,
};

#[derive(Clone, Default)]
struct Recorder {
    launches: Arc<Mutex<Vec<Value>>>,
    mode: Arc<Mutex<String>>,
}
impl RunLauncher for Recorder {
    fn launch(&self, request: LaunchRequest) -> futures_util::future::BoxFuture<'static, Result<String, String>> {
        let task = serde_json::to_value(&request.task).unwrap();
        let record = match &request.task.thread_id {
            None => json!({
                "kind": "launch", "commandId": request.command_id, "messageId": request.message_id, "scheduledTaskId": task["id"], "text": request.prompt,
                "projectId": task["projectId"], "title": task["title"], "modelSelection": task["modelSelection"], "runtimeMode": task["runtimeMode"],
                "interactionMode": task["interactionMode"], "workspaceStrategy": task["workspaceStrategy"], "createdBy": task["createdBy"], "creationSource": task["creationSource"],
            }),
            Some(thread) => json!({
                "kind": "send", "commandId": request.command_id, "messageId": request.message_id, "scheduledTaskId": task["id"], "text": request.prompt,
                "projectId": task["projectId"], "threadId": thread, "modelSelection": task["modelSelection"], "mode": "queue", "createdBy": task["createdBy"], "creationSource": task["creationSource"],
            }),
        };
        self.launches.lock().unwrap().push(record);
        let fails = *self.mode.lock().unwrap() != "ok";
        Box::pin(async move { if fails { Err("boom".to_owned()) } else { Ok("thread-launched".to_owned()) } })
    }
}

fn error_json(error: &ScheduledTaskError) -> Value {
    json!({"message": error.message, "taskId": error.task_id, "cause": error.cause.as_ref().map(|cause| json!({"name": cause["name"], "message": cause["message"]}))})
}
fn summarize(text: &Value) -> Value {
    match text.as_str() {
        None => Value::Null,
        Some(text) if text.contains("boom") => json!("boom"),
        Some(text) => json!(text),
    }
}
fn normalize_task(task: &Value) -> Value {
    let mut task = task.clone();
    let webhook = task.as_object_mut().unwrap().remove("webhook");
    task["lastRunError"] = summarize(&task["lastRunError"]);
    if let Some(webhook) = webhook {
        let path = webhook["path"].as_str().unwrap();
        task["webhook"] = json!({"hasSecret": webhook["hasSecret"], "pathPrefix": &path[..path.rfind('/').unwrap()], "url": webhook["url"]});
    }
    task
}

struct Run {
    store: Store,
    clock: Arc<ManualClock>,
    zone: Tz,
    service: ScheduledTasks,
    recorder: Recorder,
    tokens: HashMap<String, String>,
    names: HashMap<String, String>,
}
impl Run {
    fn new(zone: Tz) -> Self {
        let store = Store::memory().unwrap();
        let clock = ManualClock::new(0);
        let recorder = Recorder { launches: Default::default(), mode: Arc::new(Mutex::new("ok".into())) };
        let service = Self::open(&store, &clock, zone, &recorder);
        Self { store, clock, zone, service, recorder, tokens: HashMap::new(), names: HashMap::new() }
    }
    fn open(store: &Store, clock: &Arc<ManualClock>, zone: Tz, recorder: &Recorder) -> ScheduledTasks {
        let service = ScheduledTasks::open(store.clone(), clock.clone(), Arc::new(ChronoZone(zone))).unwrap();
        service.set_launcher(Arc::new(recorder.clone()));
        service
    }
    fn name(&mut self, id: &str) -> String {
        // As the generator numbers them: a relay delivery keeps its id but takes a place in the count.
        let next = self.names.len() + 1;
        let relay = id.starts_with("delivery:relay:");
        self.names.entry(id.to_owned()).or_insert_with(|| if relay { id.to_owned() } else { format!("delivery:{next}") }).clone()
    }
    async fn learn_tokens(&mut self) {
        for task in self.service.list().await.map(|list| serde_json::to_value(list).unwrap()).unwrap()["tasks"].as_array().unwrap() {
            if let Some(webhook) = task.get("webhook") {
                self.tokens.insert(task["id"].as_str().unwrap().into(), webhook["path"].as_str().unwrap().rsplit('/').next().unwrap().into());
            }
        }
    }
    async fn snapshot(&self) -> Value {
        match self.service.list().await {
            Ok(list) => Value::Array(serde_json::to_value(list).unwrap()["tasks"].as_array().unwrap().iter().map(normalize_task).collect()),
            Err(error) => json!({"listError": error_json(&error)}),
        }
    }
    fn attempt<T: serde::Serialize>(&self, result: Result<T, ScheduledTaskError>, map: impl FnOnce(Value) -> Value) -> Value {
        match result {
            Ok(value) => json!({"ok": map(serde_json::to_value(value).unwrap())}),
            Err(error) => json!({"error": error_json(&error)}),
        }
    }
    fn request(&self, step: &Value) -> WebhookTriggerRequest {
        let body_text = step["body"].as_str().unwrap().to_owned();
        let mut headers: indexmap::IndexMap<String, String> = step["headers"].as_object().unwrap().iter().map(|(name, value)| (name.clone(), value.as_str().unwrap().to_owned())).collect();
        let sign = |secret: &str| {
            use hmac::{Hmac, Mac};
            let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
            mac.update(body_text.as_bytes());
            format!("sha256={}", mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>())
        };
        match step["signature"].as_str() {
            Some("sha256-good") => headers.insert("x-sig".into(), sign("s3cret")),
            Some("sha256-bad") => headers.insert("x-sig".into(), sign("other")),
            _ => None,
        };
        let task = step["taskId"].as_str().or(step["task"].as_str()).unwrap();
        WebhookTriggerRequest {
            hook_id: task.into(),
            token: if step["token"] == "wrong" { "wrong-token".into() } else { self.tokens.get(task).cloned().unwrap_or_else(|| "no-token".into()) },
            request: WebhookRequest { method: step["method"].as_str().unwrap().into(), path: format!("/api/hooks/{task}"), query: step["query"].as_str().unwrap().into(), headers, body_text: body_text.clone() },
            body: body_text.into_bytes(),
            relay_delivery_id: step["relayDeliveryId"].as_str().map(Into::into),
            received_at: step["receivedAt"].as_str().map(Into::into),
        }
    }

    async fn step(&mut self, step: &Value) -> Value {
        let id = step["id"].as_str().unwrap_or_default().to_owned();
        match step["op"].as_str().unwrap() {
            "setTime" => {
                self.clock.set(step["ms"].as_i64().unwrap());
                Value::Null
            }
            "launcher" => {
                *self.recorder.mode.lock().unwrap() = step["mode"].as_str().unwrap().into();
                Value::Null
            }
            "boot" => {
                self.service = Self::open(&self.store, &self.clock, self.zone, &self.recorder);
                Value::Null
            }
            "seed" => {
                seed(&self.store, &step["row"]);
                Value::Null
            }
            "upsert" => {
                let result = match serde_json::from_value(step["input"].clone()) {
                    Err(_) => json!({"decodeError": true}),
                    Ok(input) => self.attempt(self.service.upsert(input).await, |value| normalize_task(&value["task"])),
                };
                self.learn_tokens().await;
                result
            }
            "setEnabled" => {
                let input = serde_json::from_value(json!({"id": id, "enabled": step["enabled"]})).unwrap();
                self.attempt(self.service.set_enabled(input).await, |value| normalize_task(&value["task"]))
            }
            "delete" => self.attempt(self.service.delete(serde_json::from_value(json!({"id": id})).unwrap()).await, |value| value),
            "runNow" => self.attempt(self.service.run_now(serde_json::from_value(json!({"id": id})).unwrap()).await, |value| normalize_task(&value["task"])),
            "rotate" => {
                let previous = self.tokens.get(&id).cloned();
                let mut result = self.attempt(self.service.rotate_webhook_token(serde_json::from_value(json!({"id": id})).unwrap()).await, |value| normalize_task(&value["task"]));
                self.learn_tokens().await;
                if result.get("ok").is_some() {
                    result["tokenChanged"] = json!(previous != self.tokens.get(&id).cloned());
                }
                result
            }
            "tick" => {
                self.service.run_due_tasks().await.unwrap();
                self.service.wait_for_webhook_runs().await;
                Value::Null
            }
            "webhook" => {
                let outcome = self.service.trigger_webhook(self.request(step)).await;
                self.service.wait_for_webhook_runs().await;
                match outcome {
                    Err(error) => json!({"error": error_json(&error)}),
                    Ok(result) => {
                        let (tag, outcome, delivery) = match result {
                            WebhookTriggerResult::Accepted { delivery_id, outcome } => ("accepted", Some(outcome), Some(delivery_id)),
                            WebhookTriggerResult::NotFound => ("not_found", None, None),
                            WebhookTriggerResult::RejectedSignature => ("rejected_signature", None, None),
                            WebhookTriggerResult::Disabled => ("disabled", None, None),
                            WebhookTriggerResult::RateLimited { outcome } => ("rate_limited", Some(outcome), None),
                            WebhookTriggerResult::Expired => ("expired", None, None),
                        };
                        let mut value = json!({"_tag": tag});
                        if let Some(outcome) = outcome {
                            value["outcome"] = json!(outcome);
                        }
                        if let Some(delivery) = delivery {
                            value["deliveryId"] = json!(self.name(&delivery));
                        }
                        value
                    }
                }
            }
            "burst" => {
                let mut counts = serde_json::Map::new();
                for _ in 0..step["count"].as_u64().unwrap() {
                    let result = self.service.trigger_webhook(self.request(&step["request"])).await.unwrap();
                    let key = match result {
                        WebhookTriggerResult::Accepted { outcome, .. } => format!("accepted:{outcome}"),
                        WebhookTriggerResult::RateLimited { outcome } => format!("rate_limited:{outcome}"),
                        WebhookTriggerResult::NotFound => "not_found".into(),
                        WebhookTriggerResult::RejectedSignature => "rejected_signature".into(),
                        WebhookTriggerResult::Disabled => "disabled".into(),
                        WebhookTriggerResult::Expired => "expired".into(),
                    };
                    let count = counts.get(&key).and_then(Value::as_u64).unwrap_or(0) + 1;
                    counts.insert(key, json!(count));
                }
                self.service.wait_for_webhook_runs().await;
                Value::Object(counts)
            }
            "deliveries" => {
                let result = self.service.list_webhook_deliveries(serde_json::from_value(json!({"id": id})).unwrap()).await;
                let mut value = self.attempt(result, |value| value["deliveries"].clone());
                if let Some(deliveries) = value["ok"].as_array_mut() {
                    for delivery in deliveries {
                        let name = self.name(delivery["id"].as_str().unwrap());
                        delivery["id"] = json!(name);
                    }
                }
                value
            }
            "delivery" => {
                let listed = serde_json::to_value(self.service.list_webhook_deliveries(serde_json::from_value(json!({"id": id})).unwrap()).await.unwrap()).unwrap();
                let target = match &step["index"] {
                    Value::String(_) => "delivery:nope".to_owned(),
                    index => listed["deliveries"][index.as_u64().unwrap() as usize]["id"].as_str().unwrap().to_owned(),
                };
                let result = self.service.get_webhook_delivery(serde_json::from_value(json!({"id": id, "deliveryId": target})).unwrap()).await;
                let mut value = self.attempt(result, |value| value["delivery"].clone());
                if value.get("ok").is_some() {
                    let name = self.name(value["ok"]["id"].as_str().unwrap());
                    value["ok"]["id"] = json!(name);
                }
                value
            }
            other => panic!("unknown op {other}"),
        }
    }
}

/// Writes a task row as a previous process could have left it.
fn seed(store: &Store, row: &Value) {
    let schedule = row["schedule"].to_string();
    let created = "2026-09-09T12:00:00.000Z";
    store
        .transaction(|transaction| {
            transaction.execute(
                "INSERT INTO rust_scheduled_tasks (task_id, title, prompt, enabled, schedule_json, project_id, thread_id, workspace_strategy_json, model_selection_json, runtime_mode, interaction_mode, created_by, creation_source, created_at, updated_at, next_run_at, last_run_at, last_run_status, last_run_error, run_count) VALUES (?1, 'Seeded', 'Seeded prompt', ?2, ?3, 'project-1', NULL, '{\"type\":\"root\"}', '{\"instanceId\":\"codex\",\"model\":\"gpt-5.4\"}', 'full-access', 'default', 'user', 'web', ?4, ?4, ?5, ?6, ?7, NULL, ?8)",
                rusqlite::params![row["id"].as_str().unwrap(), row.get("enabled").and_then(Value::as_i64).unwrap_or(1), schedule, created, row["next"].as_str(), row.get("lastRunAt").and_then(Value::as_str), row["status"].as_str().unwrap(), row.get("runCount").and_then(Value::as_i64).unwrap_or(0)],
            )?;
            Ok(())
        })
        .unwrap();
}

fn scenarios() -> Vec<Value> {
    include_str!("fixtures/scheduled-task-service.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

/// Every step's recorded result, launches and task list, in every zone the scenario ran in.
async fn replay(row: &Value) {
    let zone = Tz::from_str(row["zone"].as_str().unwrap()).unwrap();
    let mut run = Run::new(zone);
    let (steps, trace) = (row["steps"].as_array().unwrap(), row["trace"].as_array().unwrap());
    assert_eq!(steps.len(), trace.len());
    for (index, (step, expected)) in steps.iter().zip(trace).enumerate() {
        let before = run.recorder.launches.lock().unwrap().len();
        let result = run.step(step).await;
        let context = format!("{} [{}] step {index} {}", row["scenario"], row["zone"], step);
        assert_eq!(&result, if expected["result"].is_null() { &Value::Null } else { &expected["result"] }, "result of {context}");
        let launches = run.recorder.launches.lock().unwrap()[before..].to_vec();
        let pattern = regex::Regex::new("delivery:[0-9a-f]{8}-[0-9a-f-]{27}").unwrap();
        let launches: Vec<Value> = launches.iter().map(|launch| serde_json::from_str(&pattern.replace_all(&launch.to_string(), |found: &regex::Captures| run.name(&found[0])).into_owned()).unwrap()).collect();
        assert_eq!(Value::Array(launches), expected["launches"], "launches of {context}");
        assert_eq!(run.snapshot().await, expected["list"], "list after {context}");
    }
}

#[tokio::test]
async fn scripted_scenarios_behave_as_the_original_service_does() {
    let rows = scenarios();
    assert_eq!(rows.len(), 25);
    for row in &rows {
        // Boxed: a long scenario's debug-build future is large.
        Box::pin(replay(row)).await;
    }
}
