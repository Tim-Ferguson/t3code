//! Orchestrator tools that start work, schedule it or ask the user for something. Each handler
//! decodes admitted input, runs the original's gate, and calls the service the RPC or the UI calls:
//! `ThreadLaunchService`, `ScheduledTasks`, `SecretRequests`.
use crate::{
    launch::{LaunchProvenance, ThreadLaunchService},
    mcp_access::{self, Access, Caller, CallerLimits},
    mcp_control::{McpControlTools, link, operation_id, rows},
    mcp_invocation::{InvocationScope, McpFailure, refusal},
    persistence::StoreError,
    scheduled_tasks::ScheduledTasks,
    secret_requests::SecretRequests,
};
use serde_json::{Value, json};
use t3_contracts::{ProviderInteractionMode, RuntimeMode, trim_wire_string};

type Out = Result<Value, McpFailure>;

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const MAX_WAIT_TIMEOUT_MS: u64 = 60 * 60 * 1000;

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args[key].as_str()
}
fn owned(args: &Value, key: &str) -> Option<String> {
    text(args, key).map(str::to_owned)
}
fn failure(code: &str, message: &str) -> McpFailure {
    refusal(code, message)
}
fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> Result<Option<T>, McpFailure> {
    if value.is_null() { Ok(None) } else { serde_json::from_value(value.clone()).map(Some).map_err(|_| mcp_access::unavailable()) }
}
fn interaction_rank(mode: ProviderInteractionMode) -> u8 {
    if mode == ProviderInteractionMode::Plan { 0 } else { 1 }
}
fn within(limits: CallerLimits, runtime: RuntimeMode, interaction: ProviderInteractionMode) -> bool {
    mcp_access::runtime_rank(runtime) <= mcp_access::runtime_rank(limits.runtime_mode) && interaction_rank(interaction) <= interaction_rank(limits.interaction_mode)
}

/// A scheduled task's workspace: a bound task posts into its thread, an unbound one gets a new
/// worktree per run.
fn task_workspace(bound: bool) -> Value {
    if bound { json!({"type":"root"}) } else { json!({"type":"worktree","baseRef":"main","startFromOrigin":true}) }
}

/// Keys that are non-blank trimmed strings in the original, with the length each is held to.
const TRIMMED: &[(&str, usize)] = &[
    ("projectId", usize::MAX),
    ("threadId", usize::MAX),
    ("scheduledTaskId", usize::MAX),
    ("taskId", usize::MAX),
    ("label", usize::MAX),
    ("reason", usize::MAX),
    ("placeholder", usize::MAX),
    ("title", 512),
    ("prompt", 120_000),
    ("task", 120_000),
    ("clientRequestId", 256),
];

/// A project as the tools return it: the shell the clients see, which a live project gives a null `deletedAt`.
fn live_project(mut shell: Value) -> Value {
    shell["deletedAt"] = Value::Null;
    shell
}

pub(crate) fn trim(value: &str) -> String {
    trim_wire_string(value).to_owned()
}

/// Trims the admitted strings the original decodes as `TrimmedNonEmptyString`, and refuses blank or
/// overlong ones with its wording.
pub(crate) fn refine(name: &str, args: &mut Value) -> Result<(), String> {
    let Some(object) = args.as_object_mut() else { return Ok(()) };
    let mut errors = Vec::new();
    for &(key, max) in TRIMMED {
        // `reason` is a free-form interrupt note, and `message` a send body: not trimmed ids.
        if key == "reason" && name != "request_secret" {
            continue;
        }
        if let Some(Value::String(found)) = object.get_mut(key) {
            let trimmed = trim_wire_string(found).to_owned();
            if trimmed.is_empty() {
                errors.push(format!("Expected a non-blank string\n  at [\"{key}\"]"));
            } else if trimmed.encode_utf16().count() > max {
                errors.push(format!("Expected a value with a length of at most {max}\n  at [\"{key}\"]"));
            }
            *found = trimmed;
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(format!("Invalid parameters for tool '{name}': {}", errors.join("\n"))) }
}

impl McpControlTools {
    fn projection_of(&self, id: &str) -> Result<Option<Value>, McpFailure> {
        self.store.projection("thread", id).map_err(|_| mcp_access::unavailable())
    }
    /// The caller's own thread projection, absent for an outside client.
    fn parent_projection(&self, scope: &InvocationScope) -> Result<Option<Value>, McpFailure> {
        scope
            .thread
            .as_ref()
            .map(|thread| {
                self.projection_of(thread.thread_id.as_str())?
                    .filter(|projection| projection["thread"]["deletedAt"].is_null())
                    .ok_or_else(|| failure("thread_not_found", "The calling thread was not found."))
            })
            .transpose()
    }
    /// The service's own live check: the caller's thread is not archived and its newest active run
    /// belongs to the provider instance that holds this credential.
    pub(crate) fn assert_live_parent(&self, scope: &InvocationScope, parent: &Value) -> Result<(), McpFailure> {
        let run = rows(parent, "runs")
            .iter()
            .filter(|run| matches!(run["status"].as_str(), Some("preparing" | "starting" | "running" | "waiting")))
            .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0));
        let owned = run.is_some_and(|run| scope.thread.as_ref().is_some_and(|thread| run["providerInstanceId"] == thread.provider_instance_id.as_str()));
        if !parent["thread"]["archivedAt"].is_null() || !owned {
            return Err(failure("parent_not_active", "The calling provider no longer owns an active thread run."));
        }
        Ok(())
    }
    /// A project's work from another project's thread needs the live run too.
    fn assert_live_for_project(&self, scope: &InvocationScope, parent: Option<&Value>, project_id: &str) -> Result<(), McpFailure> {
        match parent {
            Some(parent) if parent["thread"]["projectId"] != project_id => self.assert_live_parent(scope, parent),
            _ => Ok(()),
        }
    }
    fn require_project(&self, project_id: &str) -> Result<Value, McpFailure> {
        self.store
            .projection("project", project_id)
            .map_err(|error| failure("orchestration_error", &format!("Unable to read project {project_id}: {error}")))?
            .filter(|project| project["deletedAt"].is_null())
            .ok_or_else(|| failure("invalid_request", &format!("Project {project_id} was not found.")))
    }

    // ---- scheduled tasks --------------------------------------------------------------------

    /// The modes a task runs at: its own, and those of the thread it posts into.
    fn task_modes(&self, task: &Value) -> Result<Vec<(RuntimeMode, ProviderInteractionMode)>, McpFailure> {
        let own = |value: &Value| -> Option<(RuntimeMode, ProviderInteractionMode)> { Some((serde_json::from_value(value["runtimeMode"].clone()).ok()?, serde_json::from_value(value["interactionMode"].clone()).ok()?)) };
        let mut modes = vec![own(task).ok_or_else(mcp_access::unavailable)?];
        if let Some(bound) = task["threadId"].as_str() {
            if let Some(shell) = self.projection_of(bound)?.filter(|projection| projection["thread"]["deletedAt"].is_null()) {
                modes.push(own(&shell["thread"]).ok_or_else(mcp_access::unavailable)?);
            }
        }
        Ok(modes)
    }

    /// A task as the caller may see it. Its webhook URL starts runs, so only a caller that may
    /// start one sees it: never a read-only client, and a thread caller only with a live turn at
    /// modes covering every mode the task runs at.
    fn summarize(&self, scope: &InvocationScope, parent: Option<&Value>, limits: CallerLimits, task: &Value) -> Value {
        let live = match parent {
            None => scope.client.as_ref().is_none_or(|client| client.access != t3_contracts::AuthMcpClientAccess::ReadOnly),
            Some(parent) => self.assert_live_parent(scope, parent).is_ok(),
        };
        let may_run = live && self.task_modes(task).is_ok_and(|modes| modes.into_iter().all(|(runtime, interaction)| within(limits, runtime, interaction)));
        let mut summary = json!({"scheduledTaskId":task["id"],"title":task["title"],"prompt":task["prompt"],"enabled":task["enabled"],"projectId":task["projectId"],"boundThreadId":task["threadId"],"schedule":task["schedule"],"nextRunAt":task["nextRunAt"],"lastRunStatus":task["lastRunStatus"]});
        // A bare path is not a URL anyone can call, so agents never get one to share.
        if may_run && task["webhook"]["url"].is_string() {
            summary["webhookUrl"] = task["webhook"]["url"].clone();
        }
        if task["webhook"].is_object() {
            summary["webhookSignature"] = json!(if task["webhook"]["hasSecret"] == true { "set" } else { "none" });
        }
        summary
    }

    fn scheduler(&self) -> Result<ScheduledTasks, McpFailure> {
        let scheduler = ScheduledTasks::of(&self.store).map_err(|_| mcp_access::unavailable())?;
        scheduler.ensure_launcher(self.execution.as_ref());
        Ok(scheduler)
    }
    async fn all_tasks(&self, what: &str) -> Result<Vec<Value>, McpFailure> {
        let listed = self.scheduler()?.list().await.map_err(|error| failure("orchestration_error", &format!("Could not {what}: {}", error.message)))?;
        serde_json::to_value(&listed.tasks).ok().and_then(|tasks| tasks.as_array().cloned()).ok_or_else(mcp_access::unavailable)
    }
    /// A scheduled task the caller may change: one whose runs execute at modes no broader than its own.
    async fn changeable_task(&self, id: &str, limits: CallerLimits) -> Result<Value, McpFailure> {
        let task = self.all_tasks("load scheduled task").await?.into_iter().find(|task| task["id"] == id).ok_or_else(|| failure("task_not_found", &format!("Scheduled task {id} was not found.")))?;
        for (runtime, interaction) in self.task_modes(&task)? {
            mcp_access::resolve_runtime(limits.runtime_mode, Some(runtime))?;
            mcp_access::resolve_interaction(limits.interaction_mode, Some(interaction))?;
        }
        Ok(task)
    }
    /// The schedule, which some clients send as JSON text.
    fn schedule(value: &Value) -> Result<Value, McpFailure> {
        match value {
            Value::String(encoded) => serde_json::from_str(encoded).map_err(|_| failure("invalid_request", "schedule must be an object.")),
            other => Ok(other.clone()),
        }
    }

    pub(crate) async fn schedule_task(&self, scope: &InvocationScope, args: Value) -> Out {
        let caller = mcp_access::check_store(&self.store, scope, Access::StartsThreads, &[])?;
        let caller = self.caller(scope).map(|loaded| Caller { limits: caller.limits, ..loaded })?;
        let parent = self.parent_projection(scope)?;
        let project_id = caller.project_id(text(&args, "projectId"))?;
        self.assert_live_for_project(scope, parent.as_ref(), &project_id)?;
        let project = self.require_project(&project_id)?;
        let in_project = parent.as_ref().is_some_and(|parent| parent["thread"]["projectId"] == project_id.as_str());
        // Binding means "wake this thread", which only a thread caller in that project has.
        let bind = args["bindToCurrentThread"].as_bool().unwrap_or(in_project);
        if bind && !in_project {
            return Err(failure("invalid_request", if parent.is_none() { "bindToCurrentThread needs an agent running inside a T3 thread." } else { "bindToCurrentThread binds to this thread, which belongs to a different project." }));
        }
        let model = match &parent {
            Some(parent) => parent["thread"]["modelSelection"].clone(),
            None => Some(project["defaultModelSelection"].clone()).filter(|model| !model.is_null()).ok_or_else(|| failure("invalid_request", &format!("Project {project_id} has no default model, so this caller cannot pick one for it.")))?,
        };
        let prompt = owned(&args, "prompt").unwrap_or_default();
        let derived = prompt.split('\n').next().map(|line| trim_wire_string(line).to_owned()).unwrap_or_default();
        let title = owned(&args, "title").unwrap_or_else(|| if derived.is_empty() { "Scheduled task".into() } else { String::from_utf16_lossy(&derived.encode_utf16().take(80).collect::<Vec<_>>()) });
        let mut upsert = json!({"title":title,"prompt":prompt,"enabled":args["enabled"].as_bool().unwrap_or(true),"schedule":Self::schedule(&args["schedule"])?,"projectId":project_id,"threadId":if bind {parent.as_ref().map(|parent|parent["thread"]["id"].clone()).unwrap_or(Value::Null)}else{Value::Null},"workspaceStrategy":task_workspace(bind),"modelSelection":model,"runtimeMode":caller.limits.runtime_mode,"interactionMode":caller.limits.interaction_mode,"createdBy":"agent","creationSource":"mcp"});
        // The idempotency key is scoped by provider session, so two callers reusing one request id cannot collide.
        if let Some(key) = text(&args, "clientRequestId") {
            upsert["commandId"] = json!(operation_id("command", scope, "schedule-task", key));
        }
        let input = serde_json::from_value(upsert).map_err(|error| failure("orchestration_error", &format!("Could not schedule task: {error}")))?;
        let saved = self.scheduler()?.upsert(input).await.map_err(|error| failure("orchestration_error", &format!("Could not schedule task: {}", error.message)))?;
        Ok(self.summarize(scope, parent.as_ref(), caller.limits, &serde_json::to_value(saved.task).map_err(|_| mcp_access::unavailable())?))
    }

    pub(crate) async fn list_scheduled_tasks(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::Reads, &[])?;
        let caller = self.caller(scope)?;
        let parent = self.parent_projection(scope)?;
        let project = text(&args, "projectId").map(str::to_owned).or_else(|| caller.shell.as_ref().map(|shell| shell.project_id.to_string()));
        let tasks = self.all_tasks("list scheduled tasks").await?;
        let tasks: Vec<Value> = tasks.iter().filter(|task| project.as_deref().is_none_or(|project| task["projectId"] == project)).map(|task| self.summarize(scope, parent.as_ref(), caller.limits, task)).collect();
        Ok(json!({"tasks":tasks}))
    }

    pub(crate) async fn update_scheduled_task(&self, scope: &InvocationScope, args: Value) -> Out {
        let caller = mcp_access::check_store(&self.store, scope, Access::Writes, &[])?;
        let caller = self.caller(scope).map(|loaded| Caller { limits: caller.limits, ..loaded })?;
        let parent = self.parent_projection(scope)?;
        let id = text(&args, "scheduledTaskId").unwrap_or_default();
        let existing = self.changeable_task(id, caller.limits).await?;
        let project_id = existing["projectId"].as_str().unwrap_or_default().to_owned();
        self.assert_live_for_project(scope, parent.as_ref(), &project_id)?;
        let in_project = parent.as_ref().is_some_and(|parent| parent["thread"]["projectId"] == project_id.as_str());
        if args["bindToCurrentThread"] == true && !in_project {
            return Err(failure("invalid_request", if parent.is_none() { "bindToCurrentThread needs an agent running inside a T3 thread." } else { "bindToCurrentThread binds to this thread, which belongs to a different project." }));
        }
        let thread_id = match args["bindToCurrentThread"].as_bool() {
            None => existing["threadId"].clone(),
            Some(true) => parent.as_ref().map(|parent| parent["thread"]["id"].clone()).unwrap_or(Value::Null),
            Some(false) => Value::Null,
        };
        // Rebinding changes where runs execute, so the workspace follows: unbinding a root-strategy
        // task would otherwise run loose prompts in the shared project checkout.
        let workspace = args["bindToCurrentThread"].as_bool().map_or_else(|| existing["workspaceStrategy"].clone(), task_workspace);
        let pick = |key: &str| if args.get(key).is_some() { args[key].clone() } else { existing[key].clone() };
        let schedule = if args.get("schedule").is_some() { Self::schedule(&args["schedule"])? } else { existing["schedule"].clone() };
        let upsert = json!({"id":existing["id"],"title":pick("title"),"prompt":pick("prompt"),"enabled":pick("enabled"),"schedule":schedule,"projectId":project_id,"threadId":thread_id,"workspaceStrategy":workspace,"modelSelection":existing["modelSelection"],"runtimeMode":existing["runtimeMode"],"interactionMode":existing["interactionMode"],"createdBy":existing["createdBy"],"creationSource":existing["creationSource"]});
        let input = serde_json::from_value(upsert).map_err(|error| failure("orchestration_error", &format!("Could not update scheduled task: {error}")))?;
        let saved = self.scheduler()?.upsert(input).await.map_err(|error| failure("orchestration_error", &format!("Could not update scheduled task: {}", error.message)))?;
        Ok(self.summarize(scope, parent.as_ref(), caller.limits, &serde_json::to_value(saved.task).map_err(|_| mcp_access::unavailable())?))
    }

    pub(crate) async fn delete_scheduled_task(&self, scope: &InvocationScope, args: Value) -> Out {
        let caller = mcp_access::check_store(&self.store, scope, Access::Writes, &[])?;
        let caller = self.caller(scope).map(|loaded| Caller { limits: caller.limits, ..loaded })?;
        let parent = self.parent_projection(scope)?;
        let existing = self.changeable_task(text(&args, "scheduledTaskId").unwrap_or_default(), caller.limits).await?;
        self.assert_live_for_project(scope, parent.as_ref(), existing["projectId"].as_str().unwrap_or_default())?;
        let id = serde_json::from_value(existing["id"].clone()).map_err(|_| mcp_access::unavailable())?;
        self.scheduler()?.delete(t3_contracts::ScheduledTaskDeleteInput { id }).await.map_err(|error| failure("orchestration_error", &format!("Could not delete scheduled task: {}", error.message)))?;
        Ok(json!({"scheduledTaskId":existing["id"],"deleted":true}))
    }

    pub(crate) async fn run_scheduled_task_now(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::WritesEnvironment, &[])?;
        let id = text(&args, "taskId").unwrap_or_default();
        if !self.all_tasks("list scheduled tasks").await.map_err(|_| mcp_access::unavailable())?.iter().any(|task| task["id"] == id) {
            return Err(failure("invalid_request", "The scheduled task was not found."));
        }
        let input = serde_json::from_value(json!({"id":id})).map_err(|_| mcp_access::unavailable())?;
        let ran = self.scheduler()?.run_now(input).await.map_err(|_| mcp_access::unavailable())?;
        let task = serde_json::to_value(ran.task).map_err(|_| mcp_access::unavailable())?;
        Ok(json!({"taskId":task["id"],"threadId":task["threadId"],"lastRunStatus":task["lastRunStatus"],"runCount":task["runCount"],"nextRunAt":task["nextRunAt"]}))
    }

    // ---- projects -----------------------------------------------------------------------------

    /// The orchestration capability the project reads need, before anything is read.
    fn read_caller(&self, scope: &InvocationScope) -> Result<(), McpFailure> {
        mcp_access::check_store(&self.store, scope, Access::Reads, &[])?;
        if !scope.capabilities.contains(&crate::mcp_invocation::McpCapability::Orchestration) {
            return Err(failure("capability_denied", "This credential cannot control threads."));
        }
        self.caller(scope).map(|_| ())
    }

    pub(crate) fn project_list(&self, scope: &InvocationScope, args: Value) -> Out {
        self.read_caller(scope)?;
        let projects = crate::project::ProjectService::new(self.store.clone()).list().map_err(|_| mcp_access::unavailable())?;
        let start = args["cursor"].as_u64().unwrap_or(0) as usize;
        let end = start + args["limit"].as_u64().unwrap_or(20) as usize;
        let page: Vec<Value> = projects.iter().skip(start).take(end - start).cloned().map(live_project).collect();
        Ok(json!({"projects":page,"nextCursor":if end < projects.len() {json!(end)} else {Value::Null}}))
    }

    pub(crate) fn project_read(&self, scope: &InvocationScope, args: Value) -> Out {
        self.read_caller(scope)?;
        crate::project::ProjectService::new(self.store.clone()).get(text(&args, "projectId").unwrap_or_default()).map_err(|_| mcp_access::unavailable())?.map(live_project).ok_or_else(|| failure("invalid_request", "The project was not found."))
    }

    // ---- launching threads -------------------------------------------------------------------

    pub(crate) async fn launch_thread(&self, scope: &InvocationScope, args: Value) -> Out {
        let admitted = mcp_access::check_store(&self.store, scope, Access::StartsThreads, &[])?;
        let runtime = mcp_access::resolve_runtime(admitted.limits.runtime_mode, parse(&args["runtimeMode"])?)?;
        let interaction = mcp_access::resolve_interaction(admitted.limits.interaction_mode, parse(&args["interactionMode"])?)?;
        let caller = self.caller(scope).map(|loaded| Caller { limits: admitted.limits, ..loaded })?;
        let command_id = format!("mcp:{}", uuid::Uuid::new_v4());
        let attachments = args["attachments"].as_array().cloned().unwrap_or_default();
        if attachments.iter().any(|attachment| !crate::attachment_claims::attachment_is_pending_upload(attachment)) {
            return Err(failure("invalid_request", "A new thread accepts only pending attachment uploads."));
        }
        if args["scratch"] == true && (args.get("projectId").is_some() || args.get("workspaceStrategy").is_some()) {
            return Err(failure("invalid_request", "scratch:true picks its own project and folder; omit projectId and workspaceStrategy."));
        }
        let project_id = if args["scratch"] == true {
            let folders = crate::project_folders::ManagedProjectFolders::for_store(&self.store).ok_or_else(mcp_access::unavailable)?;
            folders.ensure_scratch_project().await.map_err(|error| failure("orchestration_error", error.message()))?
        } else {
            caller.project_id(text(&args, "projectId"))?
        };
        let project = self.store.projection("project", &project_id).map_err(|_| mcp_access::unavailable())?.filter(|project| project["deletedAt"].is_null());
        let strategy = if args.get("workspaceStrategy").is_some() { args["workspaceStrategy"].clone() } else { json!({"type":"root"}) };
        if strategy["type"] == "existing_worktree" {
            let project = project.as_ref().ok_or_else(|| failure("invalid_request", "The project was not found."))?;
            self.assert_project_worktree(project["workspaceRoot"].as_str().unwrap_or_default(), strategy["worktreePath"].as_str().unwrap_or_default()).await?;
        }
        let model = [args.get("modelSelection").cloned(), caller.shell.as_ref().and_then(|shell| serde_json::to_value(&shell.model_selection).ok()), project.as_ref().map(|project| project["defaultModelSelection"].clone())]
            .into_iter()
            .flatten()
            .find(|model| !model.is_null())
            .ok_or_else(|| failure("invalid_request", "Pass modelSelection: the project has no default model. orchestrator_capabilities lists providers and models."))?;
        let title = owned(&args, "title").unwrap_or_default();
        let mut launch = json!({"commandId":command_id,"threadId":command_id,"projectId":project_id,"title":title,"modelSelection":model,"runtimeMode":runtime,"interactionMode":interaction,"workspaceStrategy":strategy,"creationSource":"mcp"});
        let message = text(&args, "message");
        if message.is_some() || !attachments.is_empty() {
            launch["initialMessage"] = json!({"messageId":command_id,"text":message.unwrap_or_default(),"attachments":attachments});
        }
        if let Some(refusal) = crate::project_clone::refusal(&self.store, &project_id) {
            return Err(failure("orchestration_error", refusal));
        }
        let provenance = LaunchProvenance { created_by: Some("agent".into()), scheduled_task_id: None, sender_thread_id: scope.thread.as_ref().map(|thread| thread.thread_id.to_string()) };
        let providers = self.execution.as_ref().map(|execution| execution.providers.clone());
        let store = self.store.clone();
        let files = crate::attachment_service::AttachmentService::for_store(&store);
        let now = (self.clock)();
        let launched = tokio::task::spawn_blocking(move || {
            let service = match providers {
                Some(providers) => ThreadLaunchService::with_providers(store, providers),
                None => ThreadLaunchService::new(store),
            };
            crate::attachment_claims::launch_with_claims(files.as_ref(), &launch, |input| service.launch_as(input.clone(), provenance, now))
        })
        .await
        .map_err(|_| mcp_access::unavailable())?
        .map_err(|error| match error {
            crate::attachment_claims::ClaimedDispatchError::Claim(message) => failure("orchestration_error", &message),
            _ => mcp_access::unavailable(),
        })?;
        let projection = &launched["projection"];
        let thread = &projection["thread"];
        let run = rows(projection, "runs").iter().find(|run| run["userMessageId"] == command_id.as_str());
        Ok(json!({"threadId":thread["id"],"link":link(scope,thread["id"].as_str().unwrap_or_default(),thread["title"].as_str().unwrap_or_default()),"projectId":thread["projectId"],"modelSelection":thread["modelSelection"],"runId":run.map_or(Value::Null,|run|run["id"].clone()),"status":run.map_or(Value::Null,|run|run["status"].clone())}))
    }

    /// An existing checkout a launch may bind: one of the project's own git worktrees. Without
    /// this a launch could point an agent at any directory on the machine.
    async fn assert_project_worktree(&self, workspace_root: &str, worktree_path: &str) -> Result<(), McpFailure> {
        let real = |path: &std::path::Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let worktrees = crate::vcs_service::VcsService::shared().core().list_worktree_paths(std::path::Path::new(workspace_root)).await.unwrap_or_default();
        if worktrees.iter().any(|path| real(path) == real(std::path::Path::new(worktree_path))) {
            Ok(())
        } else {
            Err(failure("invalid_request", "worktreePath must be one of the project's git worktrees. t3_worktree_list shows them."))
        }
    }

    // ---- asking the user for a secret --------------------------------------------------------

    /// Asks the user for a secret through a private card in the caller's thread and waits for the
    /// answer. The value is kept by the secret store and never returned: a saved answer hands back
    /// a one-use `secretRef`, which a tool that accepts one (`schedule_task`'s signature) consumes.
    pub(crate) async fn request_secret(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::ActsAsCaller, &[])?;
        self.caller(scope)?;
        let thread = scope.require_thread("request_secret")?;
        let thread_id = thread.thread_id.to_string();
        let parent = self.projection_of(&thread_id)?.ok_or_else(|| failure("thread_not_found", "The calling thread was not found."))?;
        let run = rows(&parent, "runs")
            .iter()
            .filter(|run| matches!(run["status"].as_str(), Some("preparing" | "starting" | "running" | "waiting")))
            .max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0))
            .filter(|run| !run["rootNodeId"].is_null() && run["providerInstanceId"] == thread.provider_instance_id.as_str())
            .ok_or_else(|| failure("parent_not_active", "Asking for a secret requires an active run owned by this MCP provider session."))?;
        let (run_id, node_id) = (run["id"].as_str().unwrap_or_default().to_owned(), run["rootNodeId"].as_str().unwrap_or_default().to_owned());
        let secrets = SecretRequests::for_store(&self.store).ok_or_else(mcp_access::unavailable)?;
        let key = owned(&args, "clientRequestId").unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // Turn item ids are global; the key is scoped to this thread. A retry with the same
        // clientRequestId finds this card, answered or not.
        let item_id = format!("turn-item:secret-request:{}:{}", crate::mcp_control::encode(&thread_id), crate::mcp_control::encode(&key));
        let request = json!({"label":args["label"],"reason":args["reason"],"placeholder":args["placeholder"]});
        let record = {
            let (secrets, scope, key, thread_id, run_id, node_id, item_id, request) = (secrets.clone(), scope.clone(), key.clone(), thread_id.clone(), run_id.clone(), node_id.clone(), item_id.clone(), request.clone());
            move |status: &'static str| {
                let command_id = operation_id("command", &scope, &format!("secret-{status}"), &key);
                secrets.record_request(&command_id, &thread_id, &run_id, &node_id, &item_id, &request, status)
            }
        };
        // Subscribe before the card exists, so an answer cannot slip between the record and the wait.
        let mut updates = self.store.subscribe();
        let opening = record.clone();
        tokio::task::spawn_blocking(move || opening("pending"))
            .await
            .map_err(|_| mcp_access::unavailable())?
            .map_err(|error| failure("orchestration_error", &format!("Could not record the secret request: {error}")))?;
        // Only this call can hand the agent its ref, so the card must not outlive it: a timeout, a
        // failed wait or an aborted call closes it as cancelled. If even that fails, the server
        // still refuses an answer once the run ends, and an unused value expires.
        struct Closing<F: Fn(&'static str) -> Result<(), StoreError> + Send + Sync + 'static>(Option<std::sync::Arc<F>>);
        impl<F: Fn(&'static str) -> Result<(), StoreError> + Send + Sync + 'static> Drop for Closing<F> {
            fn drop(&mut self) {
                if let (Some(close), Ok(runtime)) = (self.0.take(), tokio::runtime::Handle::try_current()) {
                    runtime.spawn_blocking(move || {
                        if let Err(error) = close("cancelled") {
                            tracing::warn!(%error, "Could not close a secret request card");
                        }
                    });
                }
            }
        }
        let record = std::sync::Arc::new(record);
        let mut closing = Closing(Some(record.clone()));
        let limit = std::time::Duration::from_millis(args["timeoutMs"].as_u64().unwrap_or(DEFAULT_WAIT_TIMEOUT_MS).min(MAX_WAIT_TIMEOUT_MS));
        let deadline = tokio::time::sleep(limit);
        tokio::pin!(deadline);
        let card_status = |store: &crate::persistence::Store| -> Result<(Option<String>, bool), McpFailure> {
            let projection = store.projection("thread", &thread_id).map_err(|error| failure("orchestration_error", &format!("Unable to read the secret request: {error}")))?;
            let projection = projection.unwrap_or(Value::Null);
            let status = rows(&projection, "turnItems").iter().find(|item| item["id"] == item_id.as_str() && item["type"] == "secret_request").and_then(|item| item["secretStatus"].as_str()).map(str::to_owned);
            let ended = rows(&projection, "runs").iter().find(|candidate| candidate["id"] == run_id.as_str()).is_none_or(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "cancelled" | "interrupted" | "rolled_back")));
            Ok((status, ended))
        };
        let answered = loop {
            let (status, ended) = card_status(&self.store)?;
            match status.as_deref() {
                Some(status) if status != "pending" => break Some(status.to_owned()),
                _ if ended => {
                    let closing = record.clone();
                    let _ = tokio::task::spawn_blocking(move || closing("cancelled")).await;
                    break Some("cancelled".to_owned());
                }
                _ => {}
            }
            tokio::select! {
                _ = &mut deadline => break None,
                received = updates.recv() => {
                    if matches!(received, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        break None;
                    }
                }
            }
        };
        closing.0 = None;
        let status = match answered {
            Some(status) => status,
            None => {
                let closing = record.clone();
                let _ = tokio::task::spawn_blocking(move || closing("cancelled")).await;
                // An answer that raced the timeout still wins: the card is answered once.
                match card_status(&self.store).ok().and_then(|(status, _)| status) {
                    Some(status) if status == "saved" || status == "declined" => status,
                    _ => "timed_out".to_owned(),
                }
            }
        };
        if status != "saved" {
            return Ok(json!({"status":status}));
        }
        // Saved means the value was stored before the card said so; a missing value is a storage
        // fault, not an answer the agent can act on.
        match secrets.saved_ref(&thread_id, &item_id) {
            Some(reference) => Ok(json!({"status":"saved","secretRef":reference})),
            None => Err(failure("orchestration_error", "The user saved the secret, but it could not be read. Ask again with a new clientRequestId.")),
        }
    }
}
