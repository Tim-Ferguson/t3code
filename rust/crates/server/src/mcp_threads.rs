//! The thread-management tools: search, configuration, organization, forks, the queue and the
//! pending questions. Each handler runs the original's gate, then calls the service the socket
//! calls (`thread_search`, `ThreadService`, `ExecutionService`) with the command the original builds.
use crate::{
    mcp_access::{self, Access, Caller, CallerLimits},
    mcp_control::McpControlTools,
    mcp_invocation::{InvocationScope, McpFailure, refusal},
};
use serde_json::{Value, json};

type Out = Result<Value, McpFailure>;

pub(crate) fn rows<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args[key].as_str()
}
/// `newCommandId`: every call is its own command; none of these tools has a retry key.
pub(crate) fn command_id() -> String {
    format!("mcp:{}", uuid::Uuid::new_v4())
}
fn code_points(text: &str, limit: usize) -> (String, bool) {
    let mut taken = text.chars();
    let head: String = taken.by_ref().take(limit).collect();
    (head, taken.next().is_some())
}

impl McpControlTools {
    /// `McpToolAccess.writesThreads`: the calling thread, or each named thread within the caller's modes.
    pub(crate) fn writes_threads(&self, scope: &InvocationScope, targets: &[Option<&str>]) -> Result<CallerLimits, McpFailure> {
        let ids: Vec<&str> = targets.iter().flatten().copied().collect();
        Ok(mcp_access::check_store(&self.store, scope, Access::WritesThreads, &ids)?.limits)
    }
    /// `readCaller`: a credential that controls threads, and the caller it names.
    pub(crate) fn reading_caller(&self, scope: &InvocationScope) -> Result<Caller, McpFailure> {
        self.store.read(|connection| Ok(mcp_access::load_caller(connection, scope, true))).map_err(|_| mcp_access::unavailable())?
    }
    /// `readThread`: any thread of the environment (the caller's own when none is named) that is not deleted.
    pub(crate) fn read_thread(&self, scope: &InvocationScope, requested: Option<&str>) -> Result<(Caller, Value), McpFailure> {
        let caller = self.reading_caller(scope)?;
        let id = caller.thread_id(requested)?;
        let projection = self
            .store
            .projection("thread", &id)
            .map_err(|_| mcp_access::unavailable())?
            .filter(|projection| projection["thread"]["deletedAt"].is_null())
            .ok_or_else(|| refusal("thread_not_found", "The thread was not found."))?;
        Ok((caller, projection))
    }
    /// A command on the thread with the common fields the original gives each: a fresh id and the thread.
    fn thread_command(&self, limits: CallerLimits, projection: &Value, mut command: Value) -> Out {
        command["commandId"] = json!(command_id());
        command["threadId"] = projection["thread"]["id"].clone();
        let receipt = self.dispatch_command(&command, limits)?;
        Ok(json!({"sequence":receipt.result_sequence}))
    }

    pub(crate) async fn thread_search(&self, scope: &InvocationScope, args: Value) -> Out {
        let caller = self.reading_caller(scope)?;
        let mut query = args.clone();
        query.as_object_mut().map(|object| object.remove("projectId"));
        let input: t3_contracts::OrchestrationSearchThreadsInput = serde_json::from_value(query).map_err(|_| mcp_access::unavailable())?;
        // Like the other project tools, an omitted project means the caller's own; a client
        // outside a thread searches every project.
        let project = text(&args, "projectId").map(str::to_owned).or_else(|| caller.shell.as_ref().map(|shell| shell.project_id.to_string()));
        let (store, words, limit) = (self.store.clone(), input.query.as_str().to_owned(), input.limit.map(|limit| limit.0 as u32));
        let found = tokio::task::spawn_blocking(move || crate::thread_search::search(&store, &words, limit)).await.map_err(|_| mcp_access::unavailable())?.map_err(|_| mcp_access::unavailable())?;
        let mut result = crate::thread_search::result_json(&found);
        if let Some(project) = project {
            result["matches"] = json!(rows(&result, "matches").iter().filter(|found| found["projectId"] == project.as_str()).cloned().collect::<Vec<_>>());
        }
        Ok(result)
    }

    pub(crate) fn thread_transfers(&self, scope: &InvocationScope, args: Value) -> Out {
        let (_, projection) = self.read_thread(scope, text(&args, "threadId"))?;
        let transfers: Vec<Value> = rows(&projection, "contextTransfers").iter().map(|transfer| json!({"id":transfer["id"],"sourceThreadId":transfer["sourceThreadId"],"targetThreadId":transfer["targetThreadId"],"status":transfer["status"]})).collect();
        Ok(json!({"transfers":transfers}))
    }

    pub(crate) fn thread_configuration(&self, scope: &InvocationScope, args: Value) -> Out {
        let (_, projection) = self.read_thread(scope, text(&args, "threadId"))?;
        let thread = &projection["thread"];
        Ok(json!({"threadId":thread["id"],"modelSelection":thread["modelSelection"],"runtimeMode":thread["runtimeMode"],"interactionMode":thread["interactionMode"]}))
    }

    /// `t3_thread_configure`: the selection command, which a thread's own provider instance accepts.
    /// Switching to another instance is the original's `provider.switch`, which this server has not
    /// ported; the same command is sent so the refusal is the thread service's own.
    pub(crate) fn thread_configure(&self, scope: &InvocationScope, args: Value) -> Out {
        let limits = self.writes_threads(scope, &[text(&args, "threadId")])?;
        let (_, projection) = self.read_thread(scope, text(&args, "threadId"))?;
        self.thread_command(limits, &projection, json!({"type":"thread.model-selection.set","modelSelection":args["modelSelection"]}))
    }

    pub(crate) fn thread_organize(&self, scope: &InvocationScope, args: Value) -> Out {
        let limits = self.writes_threads(scope, &[text(&args, "threadId")])?;
        let (_, projection) = self.read_thread(scope, text(&args, "threadId"))?;
        let action = text(&args, "action").unwrap_or_default();
        let command = match action {
            "snooze" => {
                let until = args.get("snoozedUntil").ok_or_else(|| refusal("invalid_request", "snooze requires snoozedUntil."))?;
                json!({"type":"thread.snooze","snoozedUntil":until})
            }
            "unsnooze" | "unsettle" => json!({"type":format!("thread.{action}"),"reason":"user"}),
            "mark_unread" => json!({"type":"thread.mark-unread"}),
            other => json!({"type":format!("thread.{other}")}),
        };
        self.thread_command(limits, &projection, command)
    }

    pub(crate) fn thread_fork(&self, scope: &InvocationScope, args: Value) -> Out {
        let limits = self.writes_threads(scope, &[text(&args, "threadId")])?;
        let (_, projection) = self.read_thread(scope, text(&args, "threadId"))?;
        let id = command_id();
        let target = format!("{id}:fork");
        let mut command = json!({"type":"thread.fork","commandId":id,"sourceThreadId":projection["thread"]["id"],"targetThreadId":target,"sourcePoint":args["sourcePoint"],"createdBy":"agent","creationSource":"mcp"});
        if let Some(title) = args.get("title") {
            command["title"] = title.clone();
        }
        let receipt = self.dispatch_command(&command, limits)?;
        Ok(json!({"sequence":receipt.result_sequence,"targetThreadId":target}))
    }

    pub(crate) fn thread_merge_back(&self, scope: &InvocationScope, args: Value) -> Out {
        let limits = self.writes_threads(scope, &[text(&args, "targetThreadId"), text(&args, "sourceThreadId")])?;
        let (_, target) = self.read_thread(scope, text(&args, "targetThreadId"))?;
        let (_, source) = self.read_thread(scope, text(&args, "sourceThreadId"))?;
        let command = json!({"type":"thread.merge_back","commandId":command_id(),"sourceThreadId":source["thread"]["id"],"targetThreadId":args["targetThreadId"],"sourcePoint":args["sourcePoint"],"createdBy":"agent","creationSource":"mcp"});
        let receipt = self.dispatch_command(&command, limits)?;
        Ok(json!({"sequence":receipt.result_sequence,"targetThreadId":target["thread"]["id"]}))
    }

    // ---- the queue ---------------------------------------------------------------------------

    /// A queued run with its message, the text cut to `limit` characters.
    fn queue_entry(projection: &Value, run_id: &str, limit: usize) -> Option<Value> {
        let run = rows(projection, "runs").iter().find(|run| run["id"] == run_id && run["status"] == "queued")?;
        let message = rows(projection, "messages").iter().find(|message| message["id"] == run["userMessageId"])?;
        let (head, truncated) = code_points(message["text"].as_str().unwrap_or_default(), limit);
        Some(json!({"queuedRunId":run["id"],"text":head,"truncated":truncated}))
    }

    pub(crate) fn queue_list(&self, scope: &InvocationScope, args: Value) -> Out {
        let (_, projection) = self.read_thread(scope, text(&args, "threadId"))?;
        let runs = crate::queued_runs::delivery_order(&projection);
        let cursor = args["cursor"].as_u64().unwrap_or(0) as usize;
        let end = cursor + args["limit"].as_u64().unwrap_or(20) as usize;
        let items: Vec<Value> = runs.iter().skip(cursor).take(end - cursor).filter_map(|run| Self::queue_entry(&projection, run["id"].as_str().unwrap_or_default(), 1000)).collect();
        Ok(json!({"items":items,"nextCursor":if end < runs.len() {json!(end)} else {Value::Null}}))
    }

    pub(crate) fn queue_read(&self, scope: &InvocationScope, args: Value) -> Out {
        let (_, projection) = self.read_thread(scope, text(&args, "threadId"))?;
        Self::queue_entry(&projection, text(&args, "queuedRunId").unwrap_or_default(), 16000).ok_or_else(|| refusal("invalid_request", "The queued message was not found."))
    }

    /// `t3_queue_edit`, `_cancel`, `_reorder` and `_promote_to_steer`: the existing queue command.
    pub(crate) fn queue_command(&self, scope: &InvocationScope, name: &str, args: Value) -> Out {
        let limits = self.writes_threads(scope, &[text(&args, "threadId")])?;
        let (_, projection) = self.read_thread(scope, text(&args, "threadId"))?;
        let run = args["queuedRunId"].clone();
        let command = match name {
            "t3_queue_edit" => json!({"type":"queued-run.edit","runId":run,"text":args["text"]}),
            "t3_queue_cancel" => json!({"type":"queued-run.cancel","runId":run}),
            "t3_queue_reorder" => json!({"type":"queued-run.reorder","runId":run,"beforeRunId":args["beforeRunId"]}),
            _ => json!({"type":"queued-message.promote-to-steer","queuedRunId":run,"targetRunId":args["targetRunId"]}),
        };
        self.thread_command(limits, &projection, command)
    }

    // ---- pending user questions ---------------------------------------------------------------

    /// A pending user-input question and its card. Approvals are never read or answered here.
    fn pending_question(&self, scope: &InvocationScope, thread: Option<&str>, request_id: &str) -> Result<(Value, Value), McpFailure> {
        let (_, projection) = self.read_thread(scope, thread)?;
        let request = rows(&projection, "runtimeRequests").iter().find(|request| request["id"] == request_id && request["kind"] == "user_input" && request["status"] == "pending");
        let item = rows(&projection, "turnItems").iter().find(|item| item["type"] == "user_input_request" && item["requestId"] == request_id);
        match (request, item) {
            (Some(_), Some(item)) => Ok((projection.clone(), item.clone())),
            _ => Err(refusal("invalid_request", "The pending user-input request was not found.")),
        }
    }

    pub(crate) fn pending_request_list(&self, scope: &InvocationScope, args: Value) -> Out {
        let (_, projection) = self.read_thread(scope, text(&args, "threadId"))?;
        let ids: Vec<&Value> = rows(&projection, "runtimeRequests").iter().filter(|request| request["kind"] == "user_input" && request["status"] == "pending").map(|request| &request["id"]).collect();
        Ok(json!({"requestIds":ids}))
    }

    pub(crate) fn pending_request_read(&self, scope: &InvocationScope, args: Value) -> Out {
        let (_, item) = self.pending_question(scope, text(&args, "threadId"), text(&args, "requestId").unwrap_or_default())?;
        Ok(json!({"requestId":args["requestId"],"questions":item["questions"]}))
    }

    /// Answering a question for the agent that asked it is a write to that thread: the target's modes
    /// may not exceed the caller's, and only user-input questions can be answered, never an approval.
    pub(crate) fn pending_request_respond(&self, scope: &InvocationScope, args: Value) -> Out {
        let limits = self.writes_threads(scope, &[text(&args, "threadId")])?;
        let (projection, _) = self.pending_question(scope, text(&args, "threadId"), text(&args, "requestId").unwrap_or_default())?;
        self.thread_command(limits, &projection, json!({"type":"runtime-request.respond","requestId":args["requestId"],"answers":args["answers"]}))
    }
}
