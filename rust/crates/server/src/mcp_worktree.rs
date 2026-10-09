//! The worktree tools: the refs of a thread's checkout, the thread's worktree binding, and the
//! handoff that moves a thread into a new git worktree. They need the `worktree` capability. The
//! git work is the VCS service's (the socket's `vcs.listRefs` and `vcs.createWorktree`), the
//! binding a `thread.metadata.update`, the continuation a queued message and the setup script the
//! project's.
use crate::{
    mcp_access::{self, Access},
    mcp_control::McpControlTools,
    mcp_invocation::{InvocationScope, McpCapability, McpFailure, refusal},
    mcp_threads::rows,
    project_setup_script::{self, CompletionShell},
    vcs_input::{CreateWorktreeInput, list_refs_input},
    vcs_service::{VcsContext, VcsService},
};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, sync::Mutex};

type Out = Result<Value, McpFailure>;

/// `WorktreeMcpFailure`: its code and the message the agent reads.
fn failure(code: &str, message: impl Into<String>) -> McpFailure {
    McpFailure(json!({"_tag":"WorktreeMcpFailure","code":code,"message":message.into()}))
}
fn operation(prefix: &str, error: impl std::fmt::Display) -> McpFailure {
    failure("operation_failed", format!("{prefix}: {error}"))
}

/// Serializes handoffs per thread: two concurrent calls could otherwise both pass the "not yet in
/// a worktree" check and each create one, leaving one untracked on disk.
static IN_FLIGHT: Mutex<Option<HashSet<String>>> = Mutex::new(None);
struct Handoff(String);
impl Handoff {
    fn begin(thread: &str) -> Option<Self> {
        IN_FLIGHT.lock().unwrap().get_or_insert_with(HashSet::new).insert(thread.to_owned()).then(|| Self(thread.to_owned()))
    }
}
impl Drop for Handoff {
    fn drop(&mut self) {
        if let Some(threads) = IN_FLIGHT.lock().unwrap().as_mut() {
            threads.remove(&self.0);
        }
    }
}

impl McpControlTools {
    fn vcs_context(&self) -> VcsContext {
        VcsContext { store: self.store.clone(), settings: self.services.settings.clone(), background: self.services.background.clone() }
    }
    fn worktree_capability(scope: &InvocationScope) -> Result<(), McpFailure> {
        scope.capabilities.contains(&McpCapability::Worktree).then_some(()).ok_or_else(|| failure("capability_denied", "This MCP credential does not grant worktree capabilities."))
    }
    /// The calling thread and its project, as the worktree service loads them.
    fn worktree_thread(&self, scope: &InvocationScope) -> Result<(Value, Value), McpFailure> {
        let id = scope.require_thread("Worktree handoff and status")?.thread_id.to_string();
        let thread = self.store.projection("thread", &id).map_err(|error| operation(&format!("Unable to read thread {id}"), error))?.filter(|projection| projection["thread"]["deletedAt"].is_null()).ok_or_else(|| failure("thread_not_found", format!("Thread '{id}' was not found.")))?;
        let project_id = thread["thread"]["projectId"].as_str().unwrap_or_default();
        let project = self.store.projection("project", project_id).map_err(|error| operation(&format!("Unable to read project {project_id}"), error))?.filter(|project| project["deletedAt"].is_null()).ok_or_else(|| failure("project_not_found", format!("Project '{project_id}' was not found for thread '{id}'.")))?;
        Ok((thread, project))
    }
    async fn default_start_from_origin(&self) -> Result<bool, McpFailure> {
        let settings = self.services.settings.as_ref().ok_or_else(|| failure("operation_failed", "Unable to read server settings: the settings service is unavailable."))?;
        Ok(settings.snapshot().await.map_err(|error| operation("Unable to read server settings", error))?.new_worktrees_start_from_origin)
    }

    pub(crate) async fn worktree_list(&self, scope: &InvocationScope, args: Value) -> Out {
        if !scope.capabilities.contains(&McpCapability::Worktree) {
            return Err(refusal("capability_denied", "This credential cannot inspect worktrees."));
        }
        let (_, projection) = self.read_thread(scope, args["threadId"].as_str())?;
        let project = self.store.projection("project", projection["thread"]["projectId"].as_str().unwrap_or_default()).map_err(|_| mcp_access::unavailable())?.filter(|project| project["deletedAt"].is_null()).ok_or_else(|| refusal("invalid_request", "The project was not found."))?;
        let cwd = projection["thread"]["worktreePath"].as_str().or(project["workspaceRoot"].as_str()).unwrap_or_default();
        let mut payload = json!({"cwd":cwd});
        for key in ["query", "cursor", "limit", "refKind", "includeMatchingRemoteRefs"] {
            if let Some(value) = args.get(key) {
                payload[key] = value.clone();
            }
        }
        let input = list_refs_input(&payload).map_err(|_| mcp_access::unavailable())?;
        let listed = Box::pin(VcsService::shared().list_refs(&input)).await.map_err(|_| mcp_access::unavailable())?;
        serde_json::to_value(listed).map_err(|_| mcp_access::unavailable())
    }

    pub(crate) async fn worktree_status(&self, scope: &InvocationScope) -> Out {
        mcp_access::check_store(&self.store, scope, Access::ReadsAsCaller, &[])?;
        Self::worktree_capability(scope)?;
        let (thread, project) = self.worktree_thread(scope)?;
        let default = self.default_start_from_origin().await?;
        let thread = &thread["thread"];
        Ok(json!({"attached":!thread["worktreePath"].is_null(),"worktreePath":thread["worktreePath"],"branch":thread["branch"],"projectWorkspaceRoot":project["workspaceRoot"],"defaultStartFromOrigin":default}))
    }

    /// `t3_worktree_handoff`: a new branch in a new worktree, the thread re-pointed at it, the
    /// continuation queued and the setup script started. Changing the workspace detaches the live
    /// provider session, so the current turn ends shortly after.
    pub(crate) async fn worktree_handoff(&self, scope: &InvocationScope, args: Value) -> Out {
        let limits = mcp_access::check_store(&self.store, scope, Access::ActsAsCaller, &[])?.limits;
        Self::worktree_capability(scope)?;
        let thread_id = scope.require_thread("Worktree handoff and status")?.thread_id.to_string();
        let Some(_guard) = Handoff::begin(&thread_id) else {
            return Err(failure("handoff_in_progress", format!("A worktree handoff is already in progress for thread '{thread_id}'.")));
        };
        let already = |path: &str| failure("already_in_worktree", format!("Thread '{thread_id}' is already attached to worktree '{path}'."));
        let (projection, project) = self.worktree_thread(scope)?;
        if let Some(path) = projection["thread"]["worktreePath"].as_str() {
            return Err(already(path));
        }
        // An archived thread would accept the binding but refuse the continuation message.
        if !projection["thread"]["archivedAt"].is_null() {
            return Err(failure("invalid_request", format!("Thread '{thread_id}' is archived and cannot be handed off to a worktree.")));
        }
        let project_cwd = project["workspaceRoot"].as_str().unwrap_or_default().to_owned();
        if let Some(path) = args["path"].as_str().filter(|path| !Path::new(path).is_absolute()) {
            return Err(failure("invalid_request", format!("path must be an absolute filesystem path, got '{path}'. A relative path would be created relative to the project workspace but stored verbatim as the thread's worktree binding.")));
        }
        let core = VcsService::shared().core();
        let cwd = Path::new(&project_cwd);
        let local = core.status_details_local(cwd, Default::default()).await.map_err(|error| operation("Unable to read git status", error))?;
        if !local.is_repo {
            return Err(failure("invalid_request", format!("Project workspace '{project_cwd}' is not a git repository.")));
        }
        let branch = args["branch"].as_str().unwrap_or_default().to_owned();
        // Fail fast, with where the branch is checked out, instead of letting `git worktree add` fail opaquely.
        let names = core.list_local_branch_names(cwd).await.map_err(|error| operation("Unable to list branches", error))?;
        if names.contains(&branch) {
            let listed = list_refs_input(&json!({"cwd":project_cwd,"query":branch,"refKind":"local"})).ok();
            let checkout = match listed {
                Some(input) => Box::pin(VcsService::shared().list_refs(&input)).await.ok().and_then(|result| result.refs.into_iter().find(|candidate| candidate.name == branch && !candidate.is_remote).and_then(|candidate| candidate.worktree_path)),
                None => None,
            };
            return Err(failure("invalid_request", format!("Branch '{branch}' already exists{} Choose a different branch name, or delete the existing branch{} first.", checkout.as_ref().map_or(".".to_owned(), |path| format!(" and is checked out at '{path}'.")), if checkout.is_some() { " and its worktree" } else { "" })));
        }
        let base = match args["baseRef"].as_str() {
            Some(base) => base.to_owned(),
            None => local.branch.clone().ok_or_else(|| failure("invalid_request", "Could not determine the current branch of the project workspace (detached HEAD?). Pass baseRef explicitly."))?,
        };
        let start_from_origin = match args["startFromOrigin"].as_bool() {
            Some(value) => value,
            None => self.default_start_from_origin().await?,
        };
        let mut start = base.clone();
        if start_from_origin {
            core.fetch_remote(cwd, "origin", None).await.map_err(|error| operation("Unable to fetch origin", error))?;
            start = core.resolve_remote_tracking_commit(cwd, &base, "origin").await.map_err(|error| operation(&format!("Unable to resolve the remote-tracking commit of '{base}'"), error))?.0;
        }
        let ids = uuid::Uuid::new_v4().to_string();
        let input = CreateWorktreeInput { cwd: project_cwd.clone(), ref_name: start, new_ref_name: Some(branch.clone()), base_ref_name: Some(base.clone()), path: args["path"].as_str().map(str::to_owned) };
        let created = Box::pin(VcsService::shared().create_worktree(&input, &self.vcs_context())).await.map_err(|error| operation("Unable to create the worktree", error))?;
        let (worktree, ref_name) = (created["worktree"]["path"].as_str().unwrap_or_default().to_owned(), created["worktree"]["refName"].as_str().unwrap_or_default().to_owned());
        // Whatever fails between here and the committed binding removes the worktree again, so a
        // failed handoff leaves nothing behind on disk.
        let remove_created = || async {
            if core.remove_worktree(cwd, Path::new(&worktree), true).await.is_ok() {
                let _ = core.delete_local_branch(cwd, &ref_name, true).await;
            }
        };
        // The thread was read before the slow git work; a concurrent binding could have attached it since.
        let recheck = match self.worktree_thread(scope) {
            Ok((recheck, _)) => recheck,
            Err(error) => {
                remove_created().await;
                return Err(error);
            }
        };
        if let Some(path) = recheck["thread"]["worktreePath"].as_str() {
            remove_created().await;
            return Err(already(path));
        }
        if !recheck["thread"]["archivedAt"].is_null() {
            remove_created().await;
            return Err(failure("invalid_request", format!("Thread '{thread_id}' was archived while the worktree was being created; the handoff was rolled back.")));
        }
        let bind = json!({"type":"thread.metadata.update","commandId":format!("command:mcp:{}:worktree-handoff:{ids}",crate::mcp_control::encode(&scope.request_namespace)),"threadId":thread_id,"branch":ref_name,"worktreePath":worktree,"expectedWorktreePath":null});
        if let Err(error) = self.dispatch_command(&bind, limits) {
            remove_created().await;
            return Err(failure("operation_failed", format!("Unable to re-point the thread at the worktree: {}", error)));
        }
        // Queue the continuation right after the binding commits: the detach it schedules ends the calling
        // session, and a durably queued message is what makes the thread start again inside the worktree.
        let continuation = match args["continuationPrompt"].as_str() {
            None => json!({"status":"skipped"}),
            Some(prompt) => match self.queue_continuation(&thread_id, prompt, limits, &ids, &scope.request_namespace) {
                Ok(delivery) => json!({"status":"scheduled","delivery":delivery}),
                Err(error) => json!({"status":"failed","detail":error.to_string()}),
            },
        };
        let (status_context, status_path) = (self.vcs_context(), worktree.clone());
        tokio::spawn(async move {
            if let Err(error) = VcsService::shared().refresh_status(&status_path, &status_context).await {
                tracing::warn!(%error, "Status refresh after a worktree handoff failed");
            }
        });
        let setup_script = if args["runSetupScript"].as_bool().unwrap_or(true) { self.run_setup_script(&thread_id, &projection, &project, &worktree).await } else { json!({"status":"skipped"}) };
        let scheduled = continuation["status"] == "scheduled";
        Ok(json!({"worktreePath":worktree,"branch":ref_name,"baseRef":base,"startedFromOrigin":start_from_origin,"setupScript":setup_script,"continuation":continuation,"note":if scheduled {
            "Handoff recorded. Changing the workspace detaches this provider session, so the current turn ends shortly after this call; the queued continuation prompt then starts the next turn inside the worktree with the conversation preserved. The worktree is not removed automatically when the thread is deleted."
        } else {
            "Handoff recorded. Changing the workspace detaches this provider session, so the current turn ends shortly after this call; the conversation continues inside the worktree when the thread receives its next message. Pass continuationPrompt to resume automatically. The worktree is not removed automatically when the thread is deleted."
        }}))
    }

    /// The continuation as the thread's next queued message; returns how it was delivered.
    fn queue_continuation(&self, thread_id: &str, prompt: &str, limits: mcp_access::CallerLimits, ids: &str, namespace: &str) -> Result<&'static str, McpFailure> {
        let message_id = format!("message:mcp:{}:worktree-continuation:{ids}", crate::mcp_control::encode(namespace));
        let command = json!({"type":"message.dispatch","commandId":format!("command:mcp:{}:worktree-continuation:{ids}",crate::mcp_control::encode(namespace)),"threadId":thread_id,"messageId":message_id,"text":prompt,"attachments":[],"dispatchMode":{"type":"queue_after_active"},"createdBy":"agent","creationSource":"mcp"});
        self.execute(&command, limits)?;
        let projection = self.store.projection("thread", thread_id).map_err(|_| mcp_access::unavailable())?.ok_or_else(mcp_access::unavailable)?;
        let item = rows(&projection, "turnItems").iter().find(|item| item["type"] == "user_message" && item["messageId"] == message_id.as_str());
        Ok(match item {
            None => "queued",
            Some(item) if item["inputIntent"] == "queued_turn" => "queued",
            Some(item) if item["inputIntent"] == "turn_start" => "started",
            Some(_) => "steered",
        })
    }

    /// The project's setup script, started in the new worktree. The thread is already bound, so a
    /// failure is reported in the result instead of failing the handoff.
    async fn run_setup_script(&self, thread_id: &str, projection: &Value, project: &Value, worktree: &str) -> Value {
        let failed = |detail: String| json!({"status":"failed","detail":detail});
        let Some(settings) = &self.services.settings else { return failed("The settings service is unavailable.".into()) };
        let settings = match settings.snapshot().await {
            Ok(settings) => settings,
            Err(error) => return failed(error.to_string()),
        };
        let Ok(project_id) = project["projectId"].as_str().unwrap_or_default().parse::<t3_contracts::ProjectId>() else { return failed("The project id is invalid.".into()) };
        let scripts = match crate::launch::resolved_scripts(&settings, &project_id, project) {
            Ok(scripts) => scripts,
            Err(error) => return failed(error.to_string()),
        };
        let Some(script) = project_setup_script::setup_script(&scripts) else { return json!({"status":"no-script"}) };
        let Some(terminals) = &self.services.terminals else { return failed("The terminal service is unavailable, so the setup script cannot run.".into()) };
        let shell: CompletionShell = project_setup_script::resolve_completion_shell(cfg!(windows), &std::env::var("SHELL").unwrap_or_default());
        let _ = projection;
        match Box::pin(project_setup_script::run(terminals, project_setup_script::Input { thread_id, project_root: project["workspaceRoot"].as_str().unwrap_or_default(), cwd: worktree, script, shell, observe: None })).await {
            Ok(started) => json!({"status":"started","scriptName":started.name,"terminalId":started.terminal_id}),
            Err((step, detail)) => failed(format!("{step}: {detail}")),
        }
    }
}
