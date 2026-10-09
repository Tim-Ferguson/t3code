//! The pull request tools: link, unlink, list, watch and unwatch. They need the `pull-requests`
//! capability, resolve the pull request the agent names (a URL, or a repository and number under the
//! thread's project host) and send the thread commands the socket sends. Their failures are the
//! original's own error classes, which the transport writes as the failure's message.
use crate::{
    mcp_access::{self, Access},
    mcp_control::McpControlTools,
    mcp_invocation::{InvocationScope, McpCapability, McpFailure},
    pull_request_service::{Env, PullRequestService},
    thread_pull_requests::{self, Chain},
};
use serde_json::{Value, json};

type Out = Result<Value, McpFailure>;

/// One of the original's pull request error classes: its tag, and the message the agent reads.
fn error(tag: &str, message: impl Into<String>) -> McpFailure {
    McpFailure(json!({"_tag":tag,"message":message.into()}))
}
fn failed(tag: &str, message: &str, cause: McpFailure) -> McpFailure {
    // A mode refusal recorded at commit is the caller's to hear; anything else is this tool's failure.
    if matches!(cause.0["code"].as_str(), Some("runtime_mode_escalation_denied" | "interaction_mode_escalation_denied")) { cause } else { error(tag, message) }
}

/// The identity a thread's project resolves to: its host and provider supply defaults for a bare repository and number.
struct ProjectHost {
    host: String,
    kind: &'static str,
    remote_url: String,
}

impl McpControlTools {
    /// `requireThread`: the pull-requests capability, then the thread named or the calling one.
    fn pr_thread(&self, scope: &InvocationScope, requested: Option<&str>) -> Result<Value, McpFailure> {
        scope.require_capability(McpCapability::PullRequests, false)?;
        let id = requested.or(scope.thread.as_ref().map(|thread| thread.thread_id.as_str())).ok_or_else(|| error("PullRequestThreadRequiredError", "Pass threadId: this MCP client is not running inside a T3 thread."))?;
        self.store
            .projection("thread", id)
            .map_err(|_| mcp_access::unavailable())?
            .filter(|projection| projection["thread"]["deletedAt"].is_null())
            .map(|projection| crate::thread::shell(&projection))
            .ok_or_else(|| error("PullRequestThreadNotFoundError", format!("Thread {id} was not found.")))
    }

    async fn project_host(&self, project_id: &str) -> Option<ProjectHost> {
        let root = self.store.projection("project", project_id).ok().flatten().filter(|project| project["deletedAt"].is_null())?["workspaceRoot"].as_str()?.to_owned();
        let env = Env { host: crate::source_control_host::Host::process_default().with_settings(self.services.settings.clone()), store: self.store.clone() };
        let identity = PullRequestService::shared().identity(&env, &root).await?;
        Some(ProjectHost { host: identity.host(), kind: identity.kind.as_str(), remote_url: identity.remote_url.clone() })
    }

    /// `resolveTarget`: whichever shape the agent passed, as one host-level identity and a URL.
    async fn pr_target(&self, args: &Value, thread: &Value) -> Result<Value, McpFailure> {
        if let Some(url) = args["url"].as_str() {
            let mut parsed = thread_pull_requests::parse_change_request_url(url).ok_or_else(|| error("PullRequestUrlInvalidError", "This is not a recognised pull request URL. Pass repository and number instead."))?;
            parsed["url"] = json!(url);
            let mut target = thread_pull_requests::normalize(&parsed);
            target["url"] = json!(url);
            return Ok(target);
        }
        let (Some(repository), Some(number)) = (args["repository"].as_str(), args["number"].as_u64()) else {
            return Err(error("PullRequestTargetIncompleteError", "Pass either url, or both repository and number."));
        };
        let project = self.project_host(thread["projectId"].as_str().unwrap_or_default()).await;
        let host = args["host"].as_str().map(str::to_lowercase).or_else(|| project.as_ref().map(|project| project.host.to_lowercase())).ok_or_else(|| error("PullRequestHostRequiredError", "This thread's project has no recognised remote. Pass host or url."))?;
        let repository = repository.to_lowercase();
        // The project's kind only describes its own host; another host gets no URL guess.
        let own = project.as_ref().filter(|project| project.host.to_lowercase() == host);
        let url = thread_pull_requests::change_request_url_for(own.map(|project| project.kind), &host, &repository, number, project.as_ref().map(|project| project.remote_url.as_str())).unwrap_or_else(|| format!("https://{host}/{repository}/pull/{number}"));
        let mut target = thread_pull_requests::normalize(&json!({"host":host,"repository":repository,"number":number,"url":url}));
        target["url"] = json!(url);
        Ok(target)
    }

    fn pr_link(thread: &Value, target: &Value) -> Option<Value> {
        let key = thread_pull_requests::normalize(target);
        thread_pull_requests::links(thread).into_iter().find(|link| link["source"] != "stack-dismissed" && thread_pull_requests::normalize(link) == key)
    }

    fn entry_of(link: &Value, chains: &[Chain]) -> Value {
        let key = thread_pull_requests::normalize(link);
        let stack = chains.iter().filter(|chain| chain.layers.len() >= 2).find_map(|chain| chain.layers.iter().position(|layer| thread_pull_requests::normalize(layer) == key).map(|index| json!({"kind":chain.kind,"position":index + 1,"size":chain.layers.len()}))).unwrap_or(Value::Null);
        let snapshot = &link["snapshot"];
        let field = |name: &str| if snapshot.is_null() { Value::Null } else { snapshot[name].clone() };
        json!({"host":key["host"],"repository":link["repository"],"number":link["number"],"url":link["url"],"source":link["source"],"watching":link.get("watch").is_some_and(|watch| !watch.is_null()),"state":field("state"),"title":field("title"),"headBranch":field("headBranch"),"baseBranch":field("baseBranch"),"isDraft":field("isDraft"),"stack":stack})
    }

    pub(crate) async fn pull_request_list(&self, scope: &InvocationScope, args: Value) -> Out {
        let thread = self.pr_thread(scope, args["threadId"].as_str())?;
        let links = thread_pull_requests::links(&thread);
        let chains = thread_pull_requests::chains(&links);
        let entries: Vec<Value> = thread_pull_requests::visible(&links).iter().map(|link| Self::entry_of(link, &chains)).collect();
        Ok(json!({"pullRequests":entries,"chains":chains.iter().map(|chain| json!({"kind":chain.kind,"numbers":chain.layers.iter().map(|layer| layer["number"].clone()).collect::<Vec<_>>()})).collect::<Vec<_>>()}))
    }

    /// `link_pull_request`, `unlink_pull_request`, `watch_pull_request` and `unwatch_pull_request`.
    pub(crate) async fn pull_request_write(&self, scope: &InvocationScope, name: &str, args: Value) -> Out {
        let requested = args["threadId"].as_str();
        let limits = mcp_access::check_store(&self.store, scope, Access::WritesThreads, &requested.into_iter().collect::<Vec<_>>())?.limits;
        let (failure_tag, failure) = match name {
            "link_pull_request" => ("PullRequestLinkFailedError", "Could not link the pull request."),
            "unlink_pull_request" => ("PullRequestUnlinkFailedError", "Could not unlink the pull request."),
            _ => ("PullRequestWatchFailedError", "Could not change whether the pull request is watched."),
        };
        let thread = self.pr_thread(scope, requested)?;
        let thread_id = thread["id"].as_str().unwrap_or_default().to_owned();
        let target = self.pr_target(&args, &thread).await?;
        let command = |kind: &str, fields: Value| {
            let mut command = json!({"type":kind,"commandId":format!("server:{}:{thread_id}:{}",match name {"link_pull_request"=>"mcp-pr-link","unlink_pull_request"=>"mcp-pr-unlink",_=>"mcp-pr-watch"},uuid::Uuid::new_v4()),"threadId":thread_id,"host":target["host"],"repository":target["repository"],"number":target["number"]});
            if let (Some(base), Some(extra)) = (command.as_object_mut(), fields.as_object()) {
                base.extend(extra.clone());
            }
            command
        };
        let identity = |wired: Value| json!({"host":target["host"],"repository":target["repository"],"number":target["number"],"url":target["url"],"watching":wired["watching"],"wasWatching":wired["wasWatching"]});
        match name {
            "link_pull_request" => {
                if Self::pr_link(&thread, &target).is_some() {
                    return Ok(json!({"host":target["host"],"repository":target["repository"],"number":target["number"],"url":target["url"],"alreadyLinked":true}));
                }
                self.dispatch_command(&command("thread.pull-request.link", json!({"url":target["url"],"source":"agent"})), limits).map_err(|cause| failed(failure_tag, failure, cause))?;
                Ok(json!({"host":target["host"],"repository":target["repository"],"number":target["number"],"url":target["url"],"alreadyLinked":false}))
            }
            "unlink_pull_request" => {
                let linked = thread_pull_requests::links(&thread).iter().any(|link| thread_pull_requests::normalize(link) == thread_pull_requests::normalize(&target));
                if !linked {
                    return Ok(json!({"host":target["host"],"repository":target["repository"],"number":target["number"],"wasLinked":false}));
                }
                self.dispatch_command(&command("thread.pull-request.unlink", json!({})), limits).map_err(|cause| failed(failure_tag, failure, cause))?;
                Ok(json!({"host":target["host"],"repository":target["repository"],"number":target["number"],"wasLinked":true}))
            }
            _ => {
                let watching = name == "watch_pull_request";
                if watching && thread["lineage"]["relationshipToParent"] == "subagent" {
                    return Err(error("PullRequestWatchFromSubagentError", "This thread is a subagent, so it cannot watch pull requests. Its parent thread owns the pull request: finish your task and report back instead."));
                }
                let before = Self::pr_link(&thread, &target);
                let was_watching = before.as_ref().is_some_and(|link| link.get("watch").is_some_and(|watch| !watch.is_null()));
                // A merged pull request cannot reopen. A closed one can, and its saved state may be stale,
                // so the watch starts and its first read ends it if the host still says closed.
                if watching && before.as_ref().is_some_and(|link| link["snapshot"]["state"] == "merged") {
                    return Err(error("PullRequestNotOpenError", "The pull request is merged, so there is nothing to watch."));
                }
                let fields = if watching { json!({"watching":true,"link":{"url":target["url"],"source":"agent"}}) } else { json!({"watching":false}) };
                self.dispatch_command(&command("thread.pull-request.watch", fields), limits).map_err(|cause| failed(failure_tag, failure, cause))?;
                let after = self.pr_thread(scope, Some(&thread_id))?;
                let now_watching = Self::pr_link(&after, &target).is_some_and(|link| link.get("watch").is_some_and(|watch| !watch.is_null()));
                Ok(identity(json!({"watching":now_watching,"wasWatching":was_watching})))
            }
        }
    }
}
