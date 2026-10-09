//! `orchestrator_capabilities`, `delegate_task`, `task_status` and `task_cancel`: the app-owned form
//! of subagents. Each decodes admitted input, applies the original's gate and calls
//! `DelegatedTasks` (spawn, wake policy, acknowledge, dispose) or the thread Stop the UI uses.
//!
//! The original puts no depth or fan-out limit on delegation. What bounds it is the credential's
//! thread: the parent run must be live and owned by the session, and a child's runtime and
//! interaction modes may not exceed the parent's. This port adds no other limit.
use crate::{
    delegated_completion::DelegatedTasks,
    mcp_access::{self, Access, CallerLimits},
    mcp_control::{McpControlTools, operation_id, rows},
    mcp_invocation::{InvocationScope, McpFailure, refusal},
    subagents,
};
use serde_json::{Value, json};
use t3_contracts::{ProviderInteractionMode, RuntimeMode};

type Out = Result<Value, McpFailure>;

const DEFAULT_WAIT_TIMEOUT_MS: f64 = 600_000.0;
const MAX_WAIT_TIMEOUT_MS: f64 = 3_600_000.0;

fn failure(code: &str, message: &str) -> McpFailure {
    refusal(code, message)
}
fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args[key].as_str()
}
fn active(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("preparing" | "starting" | "running" | "waiting"))
}
fn latest_active(projection: &Value) -> Option<&Value> {
    rows(projection, "runs").iter().filter(|run| active(run)).max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0))
}
/// `providerConstraints`: why a provider instance cannot run a child task, in the model's words.
fn constraints(provider: &Value, supports_orchestration: bool) -> Vec<String> {
    let mut found = vec![];
    if !supports_orchestration {
        found.push("No V2 provider adapter is registered.".to_owned());
    }
    if provider["enabled"] == false {
        found.push("Provider instance is disabled.".into());
    }
    if provider["installed"] == false {
        found.push("Provider executable is not installed.".into());
    }
    if provider["availability"] == "unavailable" {
        found.push(provider["unavailableReason"].as_str().unwrap_or("Provider driver is unavailable.").to_owned());
    }
    if matches!(provider["status"].as_str(), Some("error" | "disabled")) {
        found.push(provider["message"].as_str().map_or_else(|| format!("Provider status is {}.", provider["status"].as_str().unwrap_or_default()), str::to_owned));
    }
    if provider["auth"]["status"] == "unauthenticated" {
        found.push("Provider is not authenticated.".into());
    }
    found
}
/// `invalidOptionSelections`: duplicates always fail; the rest is checked when the model advertises descriptors.
fn invalid_options(selections: &[Value], descriptors: Option<&Vec<Value>>) -> Vec<String> {
    let mut problems = vec![];
    let mut seen: Vec<&str> = vec![];
    for selection in selections {
        let id = selection["id"].as_str().unwrap_or_default();
        if seen.contains(&id) {
            problems.push(format!("Option {id} was specified more than once."));
            continue;
        }
        seen.push(id);
        let Some(descriptors) = descriptors else { continue };
        let Some(descriptor) = descriptors.iter().find(|candidate| candidate["id"] == id) else {
            let known = descriptors.iter().filter_map(|candidate| candidate["id"].as_str()).collect::<Vec<_>>().join(", ");
            problems.push(format!("Unknown option {id}; supported options: {}.", if known.is_empty() { "none" } else { &known }));
            continue;
        };
        if descriptor["type"] == "boolean" && !selection["value"].is_boolean() {
            problems.push(format!("Option {id} expects a boolean value."));
            continue;
        }
        if descriptor["type"] == "select" && !descriptor["options"].as_array().is_some_and(|choices| choices.iter().any(|choice| choice["id"] == selection["value"])) {
            let choices = descriptor["options"].as_array().into_iter().flatten().filter_map(|choice| choice["id"].as_str()).collect::<Vec<_>>().join(", ");
            problems.push(format!("Option {id} must be one of: {choices}."));
        }
    }
    problems
}
/// The options a target names, as `[{id, value}]`: some clients send an object keyed by option id.
fn option_list(options: &Value) -> Option<Vec<Value>> {
    match options {
        Value::Array(list) => Some(list.clone()),
        Value::Object(map) => Some(map.iter().map(|(id, value)| json!({"id":id,"value":value})).collect()),
        _ => None,
    }
}
fn resolved_modes(parent: &Value, requested_runtime: &Value, requested_interaction: &Value) -> Result<(RuntimeMode, ProviderInteractionMode), McpFailure> {
    fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> Result<Option<T>, McpFailure> {
        if value.is_null() || value == "inherit" { Ok(None) } else { serde_json::from_value(value.clone()).map(Some).map_err(|_| mcp_access::unavailable()) }
    }
    let parent_runtime: RuntimeMode = serde_json::from_value(parent["thread"]["runtimeMode"].clone()).map_err(|_| mcp_access::unavailable())?;
    let parent_interaction: ProviderInteractionMode = serde_json::from_value(parent["thread"]["interactionMode"].clone()).map_err(|_| mcp_access::unavailable())?;
    Ok((mcp_access::resolve_runtime(parent_runtime, decode(requested_runtime)?)?, mcp_access::resolve_interaction(parent_interaction, decode(requested_interaction)?)?))
}
fn task_status_for_run(run: Option<&Value>) -> &'static str {
    match run.and_then(|run| run["status"].as_str()) {
        Some("queued") => "queued",
        Some("waiting") => "waiting",
        Some("completed") => "completed",
        Some("failed") => "failed",
        Some("cancelled" | "rolled_back") => "cancelled",
        Some("interrupted") => "interrupted",
        _ => "running",
    }
}
fn terminal_task(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "interrupted")
}
/// `delegatedTaskRun`: the run the spawn transfer names, else the child's first.
fn delegated_run<'a>(child: &'a Value, task: &Value) -> Option<&'a Value> {
    let spawn = rows(child, "contextTransfers").iter().find(|transfer| transfer["type"] == "subagent_spawn" && transfer["sourceThreadId"] == task["threadId"] && transfer["targetThreadId"] == task["childThreadId"]);
    match spawn {
        None => rows(child, "runs").first(),
        Some(spawn) => rows(child, "runs").iter().find(|run| run["id"] == spawn["targetRunId"]),
    }
}
fn exposable(run: Option<&Value>) -> bool {
    run.is_some_and(|run| run["status"] != "rolled_back" && terminal_task(task_status_for_run(Some(run))))
}
/// `latestTerminalResultRun`: the newest ended run that is not a monitor's, apart from rolled-back ones.
fn latest_terminal<'a>(child: &'a Value, delegated: Option<&Value>) -> Option<&'a Value> {
    let monitors: Vec<&Value> = rows(child, "messages").iter().filter(|message| message["notification"]["source"]["kind"] == "monitor").map(|message| &message["runId"]).collect();
    rows(child, "runs")
        .iter()
        .filter(|run| subagents::terminal(run["status"].as_str().unwrap_or_default()) && !monitors.contains(&&run["id"]) && run["status"] != "rolled_back" && (delegated.is_some_and(|delegated| delegated["id"] == run["id"]) || !run["startedAt"].is_null()))
        .fold(None, |latest: Option<&Value>, run| match latest {
            Some(current) if !subagents::ran_after(run, current) => Some(current),
            _ => Some(run),
        })
}

impl McpControlTools {
    fn providers(&self) -> Result<&crate::provider_registry::ProviderRegistry, McpFailure> {
        self.execution.as_ref().map(|execution| &execution.providers).ok_or_else(mcp_access::unavailable)
    }
    fn delegated(&self) -> Result<DelegatedTasks, McpFailure> {
        self.execution.clone().map(DelegatedTasks::new).ok_or_else(mcp_access::unavailable)
    }
    fn thread_projection(&self, id: &str) -> Result<Value, McpFailure> {
        self.store.projection("thread", id).map_err(|error| failure("orchestration_error", &format!("Unable to read thread {id}: {error}")))?.ok_or_else(|| failure("thread_not_found", &format!("Thread {id} was not found.")))
    }

    pub(crate) fn capabilities(&self, scope: &InvocationScope) -> Out {
        mcp_access::check_store(&self.store, scope, Access::Reads, &[])?;
        let caller = self.caller(scope)?;
        let providers = self.providers()?;
        let snapshots = providers.snapshots();
        let list = snapshots.iter().map(|provider| {
            let id = provider["instanceId"].as_str().unwrap_or_default();
            let found = constraints(provider, providers.driver(id).is_ok());
            let models: Vec<Value> = provider["models"].as_array().into_iter().flatten().map(|model| {
                let mut entry = json!({"id":model["slug"],"label":model["name"]});
                if let Some(options) = model["capabilities"]["optionDescriptors"].as_array() {
                    entry["options"] = json!(options);
                }
                entry
            }).collect();
            json!({"providerInstanceId":id,"driverKind":provider["driver"],"displayName":provider["displayName"].as_str().map_or(Value::Null,|name|json!(name)),"models":models,"canRunChildTask":found.is_empty(),"canRunCrossProviderChildTask":found.is_empty(),"constraints":found})
        }).collect::<Vec<_>>();
        let shell = caller.shell.as_ref();
        Ok(json!({"parentThreadId":shell.map_or(Value::Null,|shell|json!(shell.id)),"inheritedProviderInstanceId":shell.map_or(Value::Null,|shell|json!(shell.model_selection.instance_id)),"inheritedModel":shell.map_or(Value::Null,|shell|json!(shell.model_selection.model)),"runtimeMode":caller.limits.runtime_mode,"interactionMode":caller.limits.interaction_mode,"providers":list,"features":{"appOwnedSubagents":true,"asyncPolling":true,"cancellation":true,"batchThreadCreation":true,"threadManagement":true,"incrementalThreadRead":true,"scheduledTasks":true,"maxBatchThreads":20}}))
    }

    /// `resolveTarget`: the model a child runs on, from the target the agent named or the parent's.
    async fn resolve_target(&self, parent: &Value, target: &Value) -> Result<Value, McpFailure> {
        let providers = self.providers()?;
        let instance = text(target, "providerInstanceId").map(str::to_owned).or_else(|| text(target, "driverKind").is_none().then(|| parent["thread"]["modelSelection"]["instanceId"].as_str().unwrap_or_default().to_owned()));
        let first = self.resolve_with(parent, target, &providers.snapshots());
        match (&first, instance) {
            // Provider snapshots only re-probe while a client is in the foreground, so an unattended
            // agent can see a provider as unavailable after it was fixed: look once more before refusing.
            (Err(error), Some(instance)) if error.0["code"] == "provider_unavailable" => match providers.refresh_instance(&instance).await {
                Ok(refreshed) => self.resolve_with(parent, target, &refreshed),
                Err(_) => first,
            },
            _ => first,
        }
    }
    fn resolve_with(&self, parent: &Value, target: &Value, snapshots: &[Value]) -> Result<Value, McpFailure> {
        let providers = self.providers()?;
        let capable = |id: &str| providers.driver(id).is_ok();
        let requested_driver = text(target, "driverKind");
        let mut instance = text(target, "providerInstanceId").map(str::to_owned);
        let inherited = &parent["thread"]["modelSelection"];
        if instance.is_none() {
            if let Some(driver) = requested_driver {
                let candidates: Vec<&Value> = snapshots.iter().filter(|provider| provider["driver"] == driver && capable(provider["instanceId"].as_str().unwrap_or_default())).collect();
                if candidates.is_empty() {
                    return Err(failure("provider_unavailable", &format!("No V2 provider adapter is registered for driver {driver}.")));
                }
                // Inherit the parent's instance only when it can serve the child.
                let usable = |provider: &&&Value| constraints(provider, true).is_empty();
                let chosen = candidates.iter().find(|provider| provider["instanceId"] == inherited["instanceId"] && usable(provider)).or_else(|| candidates.iter().find(|provider| usable(provider)));
                instance = Some(chosen.ok_or_else(|| failure("provider_unavailable", &format!("No available V2 provider instance for driver {driver}.")))?["instanceId"].as_str().unwrap_or_default().to_owned());
            }
        }
        let instance = instance.unwrap_or_else(|| inherited["instanceId"].as_str().unwrap_or_default().to_owned());
        let provider = snapshots.iter().find(|candidate| candidate["instanceId"] == instance.as_str()).ok_or_else(|| failure("provider_unavailable", &format!("Provider instance {instance} is not registered.")))?;
        if let Some(driver) = requested_driver.filter(|driver| provider["driver"] != *driver) {
            return Err(failure("invalid_request", &format!("Provider instance {instance} uses driver {}, not {driver}.", provider["driver"].as_str().unwrap_or_default())));
        }
        let found = constraints(provider, capable(&instance));
        if !found.is_empty() {
            return Err(failure("provider_unavailable", &format!("Provider {instance} cannot run a child task: {}", found.join(" "))));
        }
        let models = provider["models"].as_array().cloned().unwrap_or_default();
        let requested_model = text(target, "model");
        let model = requested_model.map(str::to_owned).or_else(|| if inherited["instanceId"] == instance.as_str() { inherited["model"].as_str().map(str::to_owned) } else { models.first().and_then(|model| model["slug"].as_str()).map(str::to_owned) }).ok_or_else(|| failure("model_unavailable", &format!("Provider {instance} has no model available for inheritance.")))?;
        if requested_model.is_some() && !models.is_empty() && !models.iter().any(|candidate| candidate["slug"] == model.as_str()) {
            return Err(failure("model_unavailable", &format!("Model {model} is not advertised by provider {instance}.")));
        }
        let options = target.get("options").and_then(option_list);
        if let Some(options) = &options {
            let descriptors = models.iter().find(|candidate| candidate["slug"] == model.as_str()).and_then(|model| model["capabilities"]["optionDescriptors"].as_array());
            let problems = invalid_options(options, descriptors);
            if !problems.is_empty() {
                return Err(failure("invalid_request", &format!("Model {model} on provider {instance} rejected options: {}", problems.join(" "))));
            }
        }
        Ok(match options {
            None if inherited["instanceId"] == instance.as_str() && inherited["model"] == model.as_str() => inherited.clone(),
            None => json!({"instanceId":instance,"model":model}),
            Some(options) => json!({"instanceId":instance,"model":model,"options":options}),
        })
    }

    pub(crate) async fn delegate_task(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::ActsAsCaller, &[])?;
        self.caller(scope)?;
        let thread = scope.require_thread("delegate_task")?;
        let parent = self.thread_projection(thread.thread_id.as_str())?;
        let parent_run = latest_active(&parent)
            .filter(|run| !run["rootNodeId"].is_null() && run["providerInstanceId"] == thread.provider_instance_id.as_str())
            .ok_or_else(|| failure("parent_not_active", "Delegated tasks require an active run owned by this MCP provider session."))?
            .clone();
        let target = self.resolve_target(&parent, &args["target"]).await?;
        let (runtime, interaction) = resolved_modes(&parent, &args["runtimeMode"], &args["interactionMode"])?;
        let key = text(&args, "clientRequestId").map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let command_id = operation_id("command", scope, "delegate-task", &key);
        let wait = args["mode"] == "wait";
        let task_text = match text(&args, "role") {
            None | Some("general") => text(&args, "task").unwrap_or_default().to_owned(),
            Some(role) => format!("Act as the {role} sub-agent for this task.\n\n{}", text(&args, "task").unwrap_or_default()),
        };
        let mut request = json!({"type":"delegated_task.request","createdBy":"agent","creationSource":"mcp","commandId":command_id,"parentThreadId":thread.thread_id,"parentRunId":parent_run["id"],"parentNodeId":parent_run["rootNodeId"],"task":task_text,"modelSelection":target,"runtimeMode":runtime,"interactionMode":interaction,
            // Async delegations wake the parent on every child terminal; wait delegations deliver
            // through the blocking tool call, so a wake is only needed if the parent settled first.
            "completionWake":if wait {"settled_only"} else {"always"}});
        if let Some(title) = text(&args, "title") {
            request["title"] = json!(title);
        }
        let service = self.delegated()?;
        let now = (self.clock)();
        let dispatched = tokio::task::spawn_blocking({
            let (service, request) = (service.clone(), request.clone());
            move || service.request(&request, now)
        })
        .await
        .map_err(|_| mcp_access::unavailable())?;
        let receipt = dispatched.map_err(|error| failure("orchestration_error", &format!("Unable to create delegated task: {}", error.to_string().replace("Invalid command: ", ""))))?;
        if receipt.status != "accepted" {
            return Err(failure("orchestration_error", &format!("Unable to create delegated task: {}", receipt.error.map_or_else(String::new, |error| error["cause"].as_str().map_or_else(|| error.to_string(), str::to_owned)))));
        }
        let task_id = format!("node:delegated-task:{}", crate::mcp_control::encode(&command_id));
        if !wait {
            return self.read_task(scope, &task_id, false, true).await;
        }
        let timeout = args["timeoutMs"].as_f64().unwrap_or(DEFAULT_WAIT_TIMEOUT_MS).clamp(1.0, MAX_WAIT_TIMEOUT_MS);
        if let Some(done) = self.wait_for_task(scope, &task_id, timeout).await? {
            return Ok(done);
        }
        // The blocking wait timed out, so it no longer owns delivery: upgrade the task so a later
        // terminal wakes the parent even mid-turn. Best effort; settled_only still wakes a settled parent.
        let policy = json!({"type":"delegated_task.wake-policy","commandId":operation_id("command", scope, "delegate-task-wake-policy", &key),"parentThreadId":thread.thread_id,"taskId":task_id,"completionWake":"always"});
        let upgrading = service.clone();
        match tokio::task::spawn_blocking(move || upgrading.wake_policy(&policy, now)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(task_id, %error, "orchestrator-mcp.delegate-task.wake-policy-failed"),
            Err(error) => tracing::warn!(task_id, %error, "orchestrator-mcp.delegate-task.wake-policy-failed"),
        }
        self.read_task(scope, &task_id, true, true).await
    }

    /// Reading an untruncated terminal result from this thread's direct app-owned child acknowledges
    /// the child's automatic completion delivery (`t3_thread_read`, which stays a read).
    pub(crate) fn acknowledge_read_result(&self, scope: &InvocationScope, target: &Value, page: &[&Value], max_chars: usize, text_offset: usize) -> Result<(), McpFailure> {
        let Some(caller) = scope.thread.as_ref().filter(|_| text_offset == 0) else { return Ok(()) };
        let parent_id = caller.thread_id.as_str();
        let Some(parent) = self.store.projection("thread", parent_id).map_err(|_| mcp_access::unavailable())? else { return Ok(()) };
        let target_id = target["thread"]["id"].as_str().unwrap_or_default();
        if target["thread"]["lineage"]["parentThreadId"] != parent_id || target["thread"]["lineage"]["relationshipToParent"] != "subagent" {
            return Ok(());
        }
        let Some(task) = rows(&parent, "subagents").iter().find(|task| task["origin"] == "app_owned" && task["threadId"] == parent_id && task["childThreadId"] == target_id) else { return Ok(()) };
        let transfer = rows(&parent, "contextTransfers").iter().find(|transfer| transfer["type"] == "subagent_result" && transfer["sourceThreadId"] == target_id && transfer["targetThreadId"] == parent_id);
        let Some(transfer) = transfer else { return Ok(()) };
        let run = match transfer["sourcePoint"]["runId"].as_str() {
            Some(id) => rows(target, "runs").iter().find(|run| run["id"] == id),
            None => delegated_run(target, task),
        };
        if !exposable(run) {
            return Ok(());
        }
        let result = subagents::result_for_run(target, run.expect("checked above"));
        if result["messageId"].is_null() && result["turnItemId"].is_null() {
            return Ok(());
        }
        let included = page.iter().any(|row| {
            row["sourceThreadId"] == target_id
                && ((!result["turnItemId"].is_null() && row["sourceItemId"] == result["turnItemId"]) || (!result["messageId"].is_null() && row["item"]["type"] == "assistant_message" && row["item"]["messageId"] == result["messageId"]))
                && crate::mcp_control::turn_item_text(&row["item"]).is_some_and(|text| text.encode_utf16().count() <= max_chars)
        });
        let state = task["completionDelivery"]["state"].as_str();
        if !included || state == Some("acknowledged") || state == Some("disposed") {
            return Ok(());
        }
        let observing = latest_active(&parent).filter(|run| run["providerInstanceId"] == caller.provider_instance_id.as_str());
        let command = json!({"type":"delegated_task.completion-delivery.acknowledge","commandId":operation_id("command", scope, "thread-read-acknowledge", &uuid::Uuid::new_v4().to_string()),"parentThreadId":parent_id,"taskId":task["id"],"observedByRunId":observing.map_or(Value::Null,|run|run["id"].clone())});
        self.delegated()?.resolve_delivery(&command, (self.clock)()).map(|_| ()).map_err(|error| failure("orchestration_error", &format!("Unable to acknowledge delegated task {}: {error}", task["id"].as_str().unwrap_or_default())))
    }

    /// Creates between one and twenty ordinary top-level threads in the caller's project, branch and
    /// worktree, each started with its prompt, and records each in the caller's timeline.
    pub(crate) async fn create_threads(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::ActsAsCaller, &[])?;
        self.caller(scope)?;
        let thread = scope.require_thread("create_threads")?;
        let parent = self.thread_projection(thread.thread_id.as_str())?;
        let parent_run = latest_active(&parent)
            .filter(|run| !run["rootNodeId"].is_null() && run["providerInstanceId"] == thread.provider_instance_id.as_str())
            .ok_or_else(|| failure("parent_not_active", "Thread creation requires an active run owned by this MCP provider session."))?
            .clone();
        let key = text(&args, "clientRequestId").map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let (service, now) = (self.delegated()?, (self.clock)());
        let mut created = vec![];
        for (index, request) in args["threads"].as_array().cloned().unwrap_or_default().iter().enumerate() {
            let target = self.resolve_target(&parent, &request["target"]).await?;
            let (runtime, interaction) = resolved_modes(&parent, &request["runtimeMode"], &request["interactionMode"])?;
            let thread_id = format!("thread:mcp:{}:{}:{index}", crate::mcp_control::encode(&scope.request_namespace), crate::mcp_control::encode(&key));
            let prompt = request["prompt"].as_str().map(|prompt| crate::mcp_tools::trim(prompt)).filter(|prompt| !prompt.is_empty());
            let detail = request["title"].as_str().map(|title| crate::mcp_tools::trim(title)).filter(|title| !title.is_empty()).or_else(|| prompt.clone());
            // The title is the request's or its prompt, clipped to 80 UTF-16 units.
            let title = match detail {
                None => format!("{} thread {}", parent["thread"]["title"].as_str().unwrap_or_default(), index + 1),
                Some(detail) => {
                    let units: Vec<u16> = detail.encode_utf16().collect();
                    if units.len() > 80 { format!("{}...", String::from_utf16_lossy(&units[..77])) } else { detail }
                }
            };
            let create = json!({"type":"thread.create","createdBy":"agent","creationSource":"mcp","commandId":operation_id("command", scope, "create-thread", &format!("{key}:{index}")),"threadId":thread_id,"projectId":parent["thread"]["projectId"],"title":title,"modelSelection":target,"runtimeMode":runtime,"interactionMode":interaction,"branch":parent["thread"]["branch"],"worktreePath":parent["thread"]["worktreePath"]});
            let creating = crate::thread::ThreadService::new(self.store.clone());
            let receipt = tokio::task::spawn_blocking(move || creating.dispatch(&create, now)).await.map_err(|_| mcp_access::unavailable())?.map_err(|error| failure("orchestration_error", &format!("Unable to create thread {}: {error}", index + 1)))?;
            if receipt.status != "accepted" {
                return Err(failure("orchestration_error", &format!("Unable to create thread {}: {}", index + 1, receipt.error.map_or_else(String::new, |error| error.to_string()))));
            }
            if let Some(prompt) = &prompt {
                let start = json!({"type":"message.dispatch","createdBy":"agent","creationSource":"mcp","commandId":operation_id("command", scope, "dispatch-thread", &format!("{key}:{index}")),"threadId":thread_id,"senderThreadId":thread.thread_id,"messageId":format!("message:mcp:{}:{}:{index}", crate::mcp_control::encode(&scope.request_namespace), crate::mcp_control::encode(&key)),"text":prompt,"attachments":[],"modelSelection":target,"dispatchMode":{"type":"start_immediately"}});
                let execution = self.execution.clone().ok_or_else(mcp_access::unavailable)?;
                let started = tokio::task::spawn_blocking(move || execution.dispatch(&start, now)).await.map_err(|_| mcp_access::unavailable())?.map_err(|error| failure("orchestration_error", &format!("Unable to start thread {}: {error}", index + 1)))?;
                if started.status != "accepted" {
                    return Err(failure("orchestration_error", &format!("Unable to start thread {}: {}", index + 1, started.error.map_or_else(String::new, |error| error.to_string()))));
                }
            }
            let projection = self.thread_projection(&thread_id)?;
            let run = rows(&projection, "runs").last();
            let record = json!({"type":"thread.created.record","commandId":operation_id("command", scope, "record-created-thread", &format!("{key}:{index}")),"parentThreadId":thread.thread_id,"parentRunId":parent_run["id"],"parentNodeId":parent_run["rootNodeId"],"targetThreadId":thread_id,"targetRunId":run.map_or(Value::Null,|run|run["id"].clone())});
            let (recording, position) = (service.clone(), index + 1);
            tokio::task::spawn_blocking(move || recording.record_created_thread(&record, now)).await.map_err(|_| mcp_access::unavailable())?.map_err(|error| failure("orchestration_error", &format!("Unable to record thread {position} in the parent timeline: {error}")))?;
            created.push(json!({"threadId":thread_id,"runId":run.map_or(Value::Null,|run|run["id"].clone()),"status":run.map_or(json!("idle"),|run|run["status"].clone()),"title":projection["thread"]["title"],"createdBy":projection["thread"]["createdBy"],"creationSource":projection["thread"]["creationSource"],"providerInstanceId":target["instanceId"],"model":target["model"]}));
        }
        Ok(json!({"threads":created}))
    }

    pub(crate) async fn task_status(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::ActsAsCaller, &[])?;
        scope.require_thread("task_status")?;
        self.read_task(scope, text(&args, "taskId").unwrap_or_default(), false, true).await
    }

    /// `readTask`: a delegated task of the calling thread, from its durable projections.
    pub(crate) async fn read_task(&self, scope: &InvocationScope, task_id: &str, wait_timed_out: bool, acknowledge: bool) -> Out {
        self.caller(scope)?;
        let thread = scope.require_thread("task_status")?;
        let parent_id = thread.thread_id.to_string();
        let parent = self.thread_projection(&parent_id)?;
        let task = rows(&parent, "subagents").iter().find(|candidate| candidate["id"] == task_id && candidate["origin"] == "app_owned" && candidate["threadId"] == parent_id.as_str()).filter(|task| !task["childThreadId"].is_null()).ok_or_else(|| failure("task_not_found", &format!("Delegated task {task_id} does not belong to thread {parent_id}.")))?;
        let child_id = task["childThreadId"].as_str().unwrap_or_default();
        let child = self.thread_projection(child_id)?;
        let child_run = delegated_run(&child, task);
        let terminal_run = latest_terminal(&child, child_run);
        let (progress_state, result_run_id) = subagents::delegated_task_progress(&child);
        let result_run = result_run_id.as_ref().and_then(|id| rows(&child, "runs").iter().find(|run| run["id"] == *id));
        let work_state = if task["result"].is_null() { progress_state } else { "result_available" };
        let status = if !task["result"].is_null() {
            match task["status"].as_str() {
                Some(status @ ("completed" | "failed" | "cancelled" | "interrupted")) => status,
                _ => task_status_for_run(child_run),
            }
        } else if work_state == "result_available" {
            task_status_for_run(result_run.or(child_run))
        } else if task_status_for_run(child_run) == "queued" {
            "queued"
        } else {
            "running"
        };
        let result_text = |run: &Value| subagents::result_for_run(&child, run)["text"].clone();
        let derived = if !task["result"].is_null() {
            task["result"].clone()
        } else if let (Some(run), true) = (result_run, terminal_task(status)) {
            result_text(run)
        } else {
            Value::Null
        };
        let transfers: Vec<&Value> = rows(&parent, "contextTransfers").iter().filter(|transfer| transfer["type"] == "subagent_result" && transfer["sourceThreadId"] == child_id && transfer["targetThreadId"] == parent_id.as_str()).collect();
        let transfer_for = |run: Option<&Value>| -> Value {
            let Some(run) = run.filter(|run| exposable(Some(run))) else { return Value::Null };
            transfers.iter().find(|transfer| transfer["sourcePoint"]["runId"] == run["id"]).or_else(|| (child_run.is_some_and(|child_run| child_run["id"] == run["id"])).then(|| transfers.iter().find(|transfer| transfer["sourcePoint"]["runId"].is_null())).flatten()).map_or(Value::Null, |transfer| transfer["id"].clone())
        };
        let terminal_status = terminal_run.map(|run| task_status_for_run(Some(run)));
        let pending_children = rows(&child, "runs").iter().any(|run| !subagents::terminal(run["status"].as_str().unwrap_or_default()) && child_run.is_none_or(|delegated| run["ordinal"].as_u64() > delegated["ordinal"].as_u64()));
        let response = json!({"taskId":task["id"],"childThreadId":child_id,"childRunId":child_run.map_or(Value::Null,|run|run["id"].clone()),"childNodeId":task["id"],"status":status,"workState":work_state,"hasPendingChildRuns":pending_children,"providerInstanceId":task["providerInstanceId"],"model":task["model"],"summary":derived,"resultContextTransferId":transfers.first().map_or(Value::Null,|transfer|transfer["id"].clone()),"latestTerminalRunId":terminal_run.map_or(Value::Null,|run|run["id"].clone()),"latestTerminalStatus":terminal_status.filter(|status|terminal_task(status)).map_or(Value::Null,|status|json!(status)),"latestTerminalSummary":if exposable(terminal_run){terminal_run.map_or(Value::Null,|run|if child_run.is_some_and(|child_run|child_run["id"]==run["id"]){derived.clone()}else{result_text(run)})}else{Value::Null},"latestTerminalResultContextTransferId":transfer_for(terminal_run),"waitTimedOut":wait_timed_out});
        let state = task["completionDelivery"]["state"].as_str();
        if acknowledge && terminal_task(status) && state != Some("acknowledged") && state != Some("disposed") {
            let observing = latest_active(&parent).filter(|run| run["providerInstanceId"] == thread.provider_instance_id.as_str());
            let command = json!({"type":"delegated_task.completion-delivery.acknowledge","commandId":operation_id("command", scope, "task-status-acknowledge", &uuid::Uuid::new_v4().to_string()),"parentThreadId":parent_id,"taskId":task_id,"observedByRunId":observing.map_or(Value::Null,|run|run["id"].clone())});
            let service = self.delegated()?;
            let now = (self.clock)();
            tokio::task::spawn_blocking(move || service.resolve_delivery(&command, now))
                .await
                .map_err(|_| mcp_access::unavailable())?
                .map_err(|error| failure("orchestration_error", &format!("Unable to acknowledge delegated task {task_id}: {error}")))?;
        }
        Ok(response)
    }

    /// Re-reads the task when an event on the parent or the child can make it terminal, instead of polling.
    async fn wait_for_task(&self, scope: &InvocationScope, task_id: &str, timeout_ms: f64) -> Result<Option<Value>, McpFailure> {
        let mut updates = self.store.subscribe();
        let initial = self.read_task(scope, task_id, false, true).await?;
        if terminal_task(initial["status"].as_str().unwrap_or_default()) {
            return Ok(Some(initial));
        }
        let parent = scope.require_thread("task_status")?.thread_id.to_string();
        let child = initial["childThreadId"].as_str().unwrap_or_default().to_owned();
        let deadline = tokio::time::sleep(std::time::Duration::from_secs_f64(timeout_ms / 1000.0));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return Ok(None),
                received = updates.recv() => {
                    match received {
                        Ok(events) if !events.iter().any(|stored| {
                            let (thread, kind) = (stored.event.aggregate_id.as_str(), stored.event.event_type.as_str());
                            (thread == parent && kind == "subagent.updated") || (thread == child && matches!(kind, "run.updated" | "subagent.updated" | "provider-thread.updated"))
                        }) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err(mcp_access::unavailable()),
                        _ => {}
                    }
                }
            }
            let current = self.read_task(scope, task_id, false, true).await?;
            if terminal_task(current["status"].as_str().unwrap_or_default()) {
                return Ok(Some(current));
            }
        }
    }

    pub(crate) async fn task_cancel(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::ActsAsCaller, &[])?;
        self.caller(scope)?;
        let thread = scope.require_thread("task_cancel")?;
        let task_id = text(&args, "taskId").unwrap_or_default().to_owned();
        let current = self.read_task(scope, &task_id, false, false).await?;
        let key = text(&args, "clientRequestId").map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let parent_id = thread.thread_id.to_string();
        let parent = self.thread_projection(&parent_id)?;
        let limits = CallerLimits { runtime_mode: serde_json::from_value(parent["thread"]["runtimeMode"].clone()).map_err(|_| mcp_access::unavailable())?, interaction_mode: serde_json::from_value(parent["thread"]["interactionMode"].clone()).map_err(|_| mcp_access::unavailable())? };
        let child_id = current["childThreadId"].as_str().unwrap_or_default().to_owned();
        // Cancelling stops the child and every task under it, each a write to a thread its user may
        // have raised above the parent's modes since it was delegated: all are checked before any is stopped.
        let mut tree = vec![];
        self.stoppable_tree(&child_id, limits, &mut tree)?;
        let dispose = rows(&parent, "subagents").iter().find(|task| task["id"] == task_id.as_str() && task["origin"] == "app_owned").is_some_and(|task| task["completionDelivery"]["state"] != "disposed");
        let service = self.delegated()?;
        let now = (self.clock)();
        let dispose_command = json!({"type":"delegated_task.completion-delivery.dispose","commandId":operation_id("command", scope, "cancel-task-completion-delivery", &key),"parentThreadId":parent_id,"taskId":task_id});
        let reason = text(&args, "reason").map(str::to_owned);
        let stop = |this: &Self, tree: &[String]| -> Result<(), McpFailure> {
            for (index, thread_id) in tree.iter().enumerate() {
                this.stop_thread(scope, thread_id, limits, &key, index, reason.as_deref()).map_err(|error| if error.0["code"] == "orchestration_error" { failure("task_not_cancellable", &format!("Unable to stop delegated task {task_id}: {}", error.0["message"].as_str().unwrap_or_default())) } else { error })?;
            }
            Ok(())
        };
        if terminal_task(current["status"].as_str().unwrap_or_default()) {
            stop(self, &tree)?;
            if dispose {
                let disposing = service.clone();
                tokio::task::spawn_blocking(move || disposing.resolve_delivery(&dispose_command, now)).await.map_err(|_| mcp_access::unavailable())?.map_err(|error| failure("orchestration_error", &format!("Unable to dispose delegated task {task_id} completion delivery: {error}")))?;
            }
            return Ok(json!({"taskId":task_id,"status":current["status"]}));
        }
        // A task waiting on its own delegates has work to stop below it. One with neither a running
        // turn nor delegates keeps its delivery.
        let child = self.thread_projection(&child_id)?;
        if current["workState"] != "waiting_for_children" && latest_active(&child).is_none() {
            return Err(failure("task_not_cancellable", &format!("Delegated task {task_id} has no interruptible child run.")));
        }
        stop(self, &tree)?;
        if dispose {
            let disposing = service.clone();
            match tokio::task::spawn_blocking(move || disposing.resolve_delivery(&dispose_command, now)).await {
                Ok(Ok(_)) => {}
                other => tracing::warn!(task_id, ?other, "orchestrator-mcp.cancel-task.delivery-dispose-failed"),
            }
        }
        Ok(json!({"taskId":task_id,"status":"cancel_requested"}))
    }

    /// The child thread and every thread of the app-owned tasks under it, checked against `limits`.
    fn stoppable_tree(&self, thread_id: &str, limits: CallerLimits, found: &mut Vec<String>) -> Result<(), McpFailure> {
        if found.iter().any(|known| known == thread_id) {
            return Ok(());
        }
        found.push(thread_id.to_owned());
        let Some(view) = self.store.projection("thread", thread_id).map_err(|_| mcp_access::unavailable())? else { return Ok(()) };
        // A deleted thread takes no stop, but the tasks under it still do.
        if view["thread"]["deletedAt"].is_null() {
            mcp_access::recheck_limits(limits, &view["thread"])?;
        }
        for task in rows(&view, "subagents") {
            if let (true, Some(child)) = (task["origin"] == "app_owned", task["childThreadId"].as_str()) {
                self.stoppable_tree(child, limits, found)?;
            }
        }
        Ok(())
    }

    /// The Stop the UI gives a thread: interrupts its active run and holds its queued turns.
    fn stop_thread(&self, scope: &InvocationScope, thread_id: &str, limits: CallerLimits, key: &str, index: usize, reason: Option<&str>) -> Result<(), McpFailure> {
        let Some(view) = self.store.projection("thread", thread_id).map_err(|_| mcp_access::unavailable())? else { return Ok(()) };
        if !view["thread"]["deletedAt"].is_null() {
            return Ok(());
        }
        let Some(run) = latest_active(&view) else { return Ok(()) };
        let mut command = json!({"type":"run.interrupt","commandId":operation_id("command", scope, "cancel-task", &format!("{key}:{index}")),"threadId":thread_id,"runId":run["id"],"holdQueue":true});
        if let Some(reason) = reason {
            command["reason"] = json!(reason);
        }
        self.execute(&command, limits).map(|_| ())
    }
}
