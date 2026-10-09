//! The attachment tools: a signed upload URL, discarding a pending upload, and sending uploaded
//! files to a thread. They call `AttachmentService` and the claim step the socket's dispatch uses.
use crate::{
    attachment_claims::{self, ClaimedDispatchError},
    attachment_service::AttachmentService,
    mcp_access::{self, Access},
    mcp_control::{McpControlTools, delivery_mode, map_dispatch},
    mcp_invocation::{InvocationScope, McpFailure, refusal},
    mcp_threads::{command_id, rows},
    persistence::StoreError,
};
use serde_json::{Value, json};
use t3_contracts::{AttachmentCreateUploadUrlInput, McpThreadSendMode};

type Out = Result<Value, McpFailure>;

impl McpControlTools {
    fn attachments(&self) -> Result<AttachmentService, McpFailure> {
        AttachmentService::for_store(&self.store).ok_or_else(mcp_access::unavailable)
    }

    pub(crate) async fn attachment_prepare_upload(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::Writes, &[])?;
        let input: AttachmentCreateUploadUrlInput = serde_json::from_value(args["upload"].clone()).map_err(|_| mcp_access::unavailable())?;
        let (service, now_ms) = (self.attachments()?, (self.clock)().timestamp_millis());
        let issued = tokio::task::spawn_blocking(move || service.issue_upload_url(&input, now_ms)).await.map_err(|_| mcp_access::unavailable())?.map_err(|_| mcp_access::unavailable())?;
        serde_json::to_value(issued).map_err(|_| mcp_access::unavailable())
    }

    pub(crate) async fn attachment_discard(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::Writes, &[])?;
        let (service, id) = (self.attachments()?, args["attachmentId"].as_str().unwrap_or_default().to_owned());
        tokio::task::spawn_blocking(move || service.delete_pending(&id)).await.map_err(|_| mcp_access::unavailable())?;
        Ok(json!({}))
    }

    /// `resolveAttachmentReferences`: a pending upload, or a file the thread already holds.
    fn resolve_attachments(requested: &[Value], stored: &[Value]) -> Result<Vec<Value>, McpFailure> {
        requested
            .iter()
            .map(|attachment| {
                if attachment_claims::attachment_is_pending_upload(attachment) {
                    Some(attachment.clone())
                } else {
                    stored.iter().find(|held| held["id"] == attachment["id"]).cloned()
                }
                .ok_or_else(|| refusal("invalid_request", "Attachments must be pending uploads or already belong to the target thread."))
            })
            .collect()
    }

    /// `t3_thread_send_attachments`: a new message with the files, each call its own with no retry key.
    pub(crate) async fn send_attachments(&self, scope: &InvocationScope, args: Value) -> Out {
        let requested_thread = args["threadId"].as_str();
        let limits = self.writes_threads(scope, &[requested_thread])?;
        let (caller, projection) = self.read_thread(scope, requested_thread)?;
        let thread_id = projection["thread"]["id"].as_str().unwrap_or_default().to_owned();
        if !projection["thread"]["archivedAt"].is_null() {
            return Err(refusal("invalid_request", "Unarchive the target thread before sending attachments."));
        }
        let stored: Vec<Value> = rows(&projection, "messages").iter().flat_map(|message| rows(message, "attachments").iter().cloned()).collect();
        let attachments = Self::resolve_attachments(args["attachments"].as_array().map(Vec::as_slice).unwrap_or(&[]), &stored)?;
        let id = command_id();
        let mut command = json!({"type":"message.dispatch","commandId":id,"threadId":thread_id,"messageId":id,"text":args["message"].as_str().unwrap_or_default(),"attachments":attachments,"dispatchMode":delivery_mode(&projection, &thread_id, McpThreadSendMode::Auto)?,"createdBy":"agent","creationSource":"mcp"});
        if let Some(sender) = &caller.shell {
            command["senderThreadId"] = json!(sender.id);
        }
        let execution = self.execution.as_ref().ok_or_else(mcp_access::unavailable)?;
        let files = AttachmentService::for_store(&self.store);
        let refused = std::cell::RefCell::new(None);
        let now = (self.clock)();
        let outcome = attachment_claims::dispatch_with_claims(files.as_ref(), &self.store, &command, |claimed| {
            execution.dispatch_guarded(claimed, now, |_, current| {
                mcp_access::recheck_limits(limits, &current["thread"]).map_err(|error| {
                    *refused.borrow_mut() = Some(error);
                    StoreError::InvalidCommand("MCP admission refused".into())
                })
            })
        });
        match outcome {
            Err(ClaimedDispatchError::Claim(message)) => return Err(refusal("orchestration_error", &message)),
            Err(ClaimedDispatchError::Store(error)) => map_dispatch(Err(error), refused.into_inner())?,
            Ok(receipt) => map_dispatch(Ok(receipt), None)?,
        };
        let after = self.store.projection("thread", &thread_id).map_err(|_| mcp_access::unavailable())?.ok_or_else(mcp_access::unavailable)?;
        let message = rows(&after, "messages").iter().find(|message| message["id"] == id.as_str()).ok_or_else(mcp_access::unavailable)?;
        let run = rows(&after, "runs").iter().find(|run| run["id"] == message["runId"]).ok_or_else(mcp_access::unavailable)?;
        Ok(json!({"threadId":thread_id,"messageId":id,"runId":run["id"],"status":run["status"]}))
    }
}
