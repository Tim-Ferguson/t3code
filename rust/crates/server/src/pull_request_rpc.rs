//! The `pullRequests.*` socket methods. Each handler decodes its payload, calls one service
//! method (boxed, so its future never sits inside the dispatcher's), and maps the failure to
//! the original's `_tag` envelope. Writes reach the host only after the transport has checked
//! the source-control write scope; a method this server has not ported answers with the
//! explicit unsupported error rather than a half-working handler. Every method is ported on
//! GitHub; the one case that is not is a native stack action, which the capabilities this
//! server states do not offer.
use crate::{
    pull_request_service::{Env, PrError, PrResult, PullRequestService},
    source_control_host::Host,
    transport::ApiState,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use t3_contracts::*;

pub const METHODS: [&str; 29] = [
    "pullRequests.list",
    "pullRequests.listStats",
    "pullRequests.summary",
    "pullRequests.routing",
    "pullRequests.routingIdentity",
    "pullRequests.stack",
    "pullRequests.linkedThreads",
    "pullRequests.detail",
    "pullRequests.preview",
    "pullRequests.checks",
    "pullRequests.activity",
    "pullRequests.threadComments",
    "pullRequests.diffFileContents",
    "pullRequests.filesViewed",
    "pullRequests.setFilesViewed",
    "pullRequests.runAction",
    "pullRequests.update",
    "pullRequests.comment",
    "pullRequests.updateComment",
    "pullRequests.submitReview",
    "pullRequests.replyToThread",
    "pullRequests.setThreadResolution",
    "pullRequests.setReaction",
    "pullRequests.invalidate",
    "pullRequests.subscribeRefreshes",
    "pullRequests.reviewerCandidates",
    "pullRequests.requestReviewers",
    "pullRequests.labelCandidates",
    "pullRequests.setLabels",
];

fn decode_failure(message: impl ToString) -> Value {
    json!({"_tag":"SchemaDecodeError","message":message.to_string()})
}

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, Value> {
    serde_json::from_value(payload).map_err(decode_failure)
}

fn encode<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("typed pull request result")
}

fn done<T: Serialize>(result: PrResult<T>) -> Result<Value, Value> {
    result.map(|value| encode(&value)).map_err(|error| error.wire())
}

/// `pullRequestSync.requestSync` of the pull request a socket method acted on: the linked threads' snapshots are read again.
async fn request_sync(state: &ApiState, env: &Env, reference: &PullRequestRef) {
    if let (Some(sync), Some(key)) = (crate::pull_request_sync::PullRequestSync::current(&state.store), PullRequestService::shared().sync_key(env, reference).await) {
        sync.request_sync(&key);
    }
}

fn done_void(result: PrResult<()>) -> Result<Value, Value> {
    result.map(|()| Value::Null).map_err(|error| error.wire())
}

/// A reference names a pull request by a positive safe integer.
fn check_reference(reference: &PullRequestRef) -> Result<(), Value> {
    if reference.number == 0 || reference.number > 9_007_199_254_740_991 {
        return Err(decode_failure("Expected a positive integer for number."));
    }
    Ok(())
}

/// A remark's body: markdown, kept verbatim, at most 65,536 characters (the service refuses one
/// that is only whitespace).
fn check_body(body: &str, allow_empty: bool) -> Result<(), Value> {
    if (!allow_empty && body.is_empty()) || body.encode_utf16().count() > 65_536 {
        return Err(decode_failure("Expected a body of 1 to 65536 characters."));
    }
    Ok(())
}

/// `PullRequestCommentInput`, decoded and bounded the way the handler does before anything is posted.
pub(crate) fn decode_comment(payload: Value) -> Result<PullRequestCommentInput, Value> {
    let input: PullRequestCommentInput = decode(payload)?;
    check_reference(&input.reference)?;
    check_body(&input.body, false)?;
    Ok(input)
}

/// The bounds the contract puts on a listing: each is written into a host's own search query.
fn check_list(input: &PullRequestListInput) -> Result<(), Value> {
    let bad = |what: &str| Err(decode_failure(format!("Expected {what}.")));
    if input.limit.is_some_and(|limit| !(1..=500).contains(&limit)) {
        return bad("a limit between 1 and 500");
    }
    if input.query.as_ref().is_some_and(|query| query.as_str().chars().count() > 200) {
        return bad("a query of at most 200 characters");
    }
    if input.project_ids.as_ref().is_some_and(|ids| ids.len() > 100) {
        return bad("at most 100 project ids");
    }
    if input.cursors.as_ref().is_some_and(|cursors| cursors.values().any(|cursor| cursor.is_empty() || cursor.chars().count() > 4096)) {
        return bad("cursors of 1 to 4096 characters");
    }
    if let Some(filters) = &input.filters {
        let value_ok = |value: &TrimmedNonEmptyString| value.as_str().chars().count() <= 200;
        let groups_ok = filters.labels.as_ref().is_none_or(|groups| groups.len() <= 10 && groups.iter().all(|group| group.len() <= 10 && group.iter().all(value_ok)));
        let excluded_ok = filters.excluded_labels.as_ref().is_none_or(|excluded| excluded.len() <= 10 && excluded.iter().all(value_ok));
        if !groups_ok || !excluded_ok || !filters.author.as_ref().is_none_or(value_ok) {
            return bad("filters within their bounds");
        }
    }
    Ok(())
}

pub fn env(state: &ApiState) -> Env {
    Env { host: Host::process_default().with_settings(state.settings.clone()), store: state.store.clone() }
}

pub async fn command(state: &ApiState, tag: &str, payload: Value) -> Result<Value, Value> {
    let env = env(state);
    let service = PullRequestService::shared();
    match tag {
        "pullRequests.list" => {
            let input: PullRequestListInput = decode(payload)?;
            check_list(&input)?;
            done(Box::pin(service.list(&env, &input)).await)
        }
        "pullRequests.listStats" => {
            let input: PullRequestListStatsInput = decode(payload)?;
            if input.refs.len() > 500 {
                return Err(decode_failure("Expected at most 500 references."));
            }
            for reference in &input.refs {
                check_reference(reference)?;
            }
            done(Box::pin(service.list_stats(&env, &input)).await)
        }
        "pullRequests.routingIdentity" => {
            let input: PullRequestRoutingIdentityInput = decode(payload)?;
            done(Box::pin(service.routing_identity(&env, &input)).await)
        }
        "pullRequests.routing" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            done(Box::pin(service.routing(&env, &reference)).await)
        }
        "pullRequests.summary" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            let result = Box::pin(service.routed(&env, &reference, |namespace| {
                Box::pin(service.summary(&env, &reference, namespace, true))
            }))
            .await;
            done(result)
        }
        "pullRequests.stack" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            let result = Box::pin(service.routed(&env, &reference, |namespace| {
                Box::pin(service.stack(&env, &reference, namespace, true))
            }))
            .await;
            done(result)
        }
        "pullRequests.linkedThreads" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            done(Box::pin(service.linked_threads(&env, &reference)).await)
        }
        "pullRequests.detail" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            let result = Box::pin(service.routed(&env, &reference, |namespace| {
                Box::pin(service.detail(&env, &reference, namespace))
            }))
            .await;
            done(result)
        }
        "pullRequests.preview" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            let result = Box::pin(service.routed(&env, &reference, |namespace| {
                Box::pin(service.preview(&env, &reference, namespace))
            }))
            .await;
            done(result)
        }
        "pullRequests.checks" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            let result = Box::pin(service.routed(&env, &reference, |namespace| {
                Box::pin(service.checks(&env, &reference, namespace))
            }))
            .await;
            done(result)
        }
        "pullRequests.activity" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            let result = Box::pin(service.routed(&env, &reference, |namespace| {
                Box::pin(service.activity(&env, &reference, namespace))
            }))
            .await;
            done(result)
        }
        "pullRequests.threadComments" => {
            let input: PullRequestThreadCommentsInput = decode(payload)?;
            check_reference(&input.reference)?;
            done(Box::pin(service.thread_comments(&env, &input)).await)
        }
        "pullRequests.diffFileContents" => {
            let input: PullRequestDiffFileContentsInput = decode(payload)?;
            check_reference(&input.reference)?;
            done(Box::pin(service.diff_file_contents(&env, &input)).await)
        }
        "pullRequests.filesViewed" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            let result = Box::pin(service.routed(&env, &reference, |namespace| {
                Box::pin(service.files_viewed(&env, &reference, namespace))
            }))
            .await;
            done(result)
        }
        "pullRequests.comment" => {
            let input = decode_comment(payload)?;
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.comment(&env, &input, namespace)))).await;
            done_void(result)
        }
        "pullRequests.update" => {
            let input: PullRequestUpdateInput = decode(payload)?;
            check_reference(&input.reference)?;
            if input.title.as_ref().is_some_and(|title| title.as_str().chars().count() > 1024) {
                return Err(decode_failure("Expected a title of at most 1024 characters."));
            }
            check_body(input.body.as_deref().unwrap_or(""), true)?;
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.update(&env, &input, namespace)))).await;
            done_void(result)
        }
        "pullRequests.updateComment" => {
            let input: PullRequestCommentUpdateInput = decode(payload)?;
            check_reference(&input.reference)?;
            check_body(&input.body, false)?;
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.update_comment(&env, &input, namespace)))).await;
            done_void(result)
        }
        "pullRequests.submitReview" => {
            let input: PullRequestSubmitReviewInput = decode(payload)?;
            check_reference(&input.reference)?;
            check_body(&input.body, true)?;
            for comment in &input.comments {
                check_body(&comment.body, false)?;
            }
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.submit_review(&env, &input, namespace)))).await;
            done_void(result)
        }
        "pullRequests.replyToThread" => {
            let input: PullRequestThreadReplyInput = decode(payload)?;
            check_reference(&input.reference)?;
            check_body(&input.body, false)?;
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.reply_to_thread(&env, &input, namespace)))).await;
            done_void(result)
        }
        "pullRequests.setThreadResolution" => {
            let input: PullRequestThreadResolutionInput = decode(payload)?;
            check_reference(&input.reference)?;
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.set_thread_resolution(&env, &input, namespace)))).await;
            done_void(result)
        }
        "pullRequests.setReaction" => {
            let input: PullRequestReactionInput = decode(payload)?;
            check_reference(&input.reference)?;
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.set_reaction(&env, &input, namespace)))).await;
            done_void(result)
        }
        "pullRequests.setFilesViewed" => {
            let input: PullRequestSetFilesViewedInput = decode(payload)?;
            check_reference(&input.reference)?;
            if input.files.len() > 500 || input.files.iter().any(|file| file.path.is_empty() || file.path.chars().count() > 4096) {
                return Err(decode_failure("Expected at most 500 files with paths of 1 to 4096 characters."));
            }
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.set_files_viewed(&env, &input, namespace)))).await;
            done_void(result)
        }
        "pullRequests.runAction" => {
            let input: PullRequestActionInput = decode(payload)?;
            check_reference(&input.reference)?;
            if input.stack_number == Some(0) || input.expected_stack_heads.as_ref().is_some_and(|heads| heads.iter().any(|head| head.number == 0)) {
                return Err(decode_failure("Expected a positive integer for stackNumber."));
            }
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.run_action(&env, &input, namespace)))).await;
            if result.is_ok() {
                request_sync(state, &env, &input.reference).await;
            }
            done_void(result)
        }
        "pullRequests.reviewerCandidates" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            let result = Box::pin(service.routed(&env, &reference, |_| Box::pin(service.reviewer_candidates(&env, &reference)))).await;
            done(result)
        }
        "pullRequests.requestReviewers" => {
            let input: PullRequestReviewerRequestInput = decode(payload)?;
            check_reference(&input.reference)?;
            if input.reviewers.is_empty() || input.reviewers.len() > 25 {
                return Err(decode_failure("Expected 1 to 25 reviewers."));
            }
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.request_reviewers(&env, &input, namespace)))).await;
            done_void(result)
        }
        "pullRequests.labelCandidates" => {
            let reference: PullRequestRef = decode(payload)?;
            check_reference(&reference)?;
            let result = Box::pin(service.routed(&env, &reference, |_| Box::pin(service.label_candidates(&env, &reference)))).await;
            done(result)
        }
        "pullRequests.setLabels" => {
            let input: PullRequestLabelChangeInput = decode(payload)?;
            check_reference(&input.reference)?;
            if input.labels.is_empty() || input.labels.len() > 25 {
                return Err(decode_failure("Expected 1 to 25 labels."));
            }
            let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.set_labels(&env, &input, namespace)))).await;
            done_void(result)
        }
        "pullRequests.invalidate" => {
            let input: PullRequestInvalidateInput = decode(payload)?;
            if let Some(reference) = &input.reference {
                check_reference(reference)?;
            }
            Box::pin(service.invalidate(&env, &input, true)).await;
            // A reader asking for fresh host state also wants the thread badges it feeds to catch up, including a merged link the sweep
            // would otherwise never revisit.
            if let (Some(reference), false) = (&input.reference, input.files_viewed_only == Some(true)) {
                request_sync(state, &env, reference).await;
            }
            Ok(Value::Null)
        }
        _ => Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    }
}

/// `POST /api/pull-requests/diff`: the patch travels over HTTP, where it compresses and is
/// flow-controlled. The error carries the status the original's endpoint declares.
pub async fn diff_http(state: &ApiState, payload: Value) -> Result<Value, (u16, Value)> {
    let input: PullRequestDiffInput = serde_json::from_value(payload).map_err(|error| (400, decode_failure(error)))?;
    check_reference(&input.reference).map_err(|error| (400, error))?;
    let (env, service) = (env(state), PullRequestService::shared());
    let result = Box::pin(service.routed(&env, &input.reference, |namespace| Box::pin(service.diff(&env, &input, namespace)))).await;
    result.map(|diff| encode(&diff)).map_err(|error| {
        let status = match &error {
            PrError::Unavailable(_) => 503,
            PrError::Operation { .. } => 502,
        };
        (status, error.wire())
    })
}

/// A `pullRequests.subscribeRefreshes` stream: a revision whenever something held may have
/// changed. Dropping it is the whole clean-up.
pub struct RefreshSubscription {
    receiver: tokio::sync::watch::Receiver<u64>,
    first: bool,
}

impl RefreshSubscription {
    pub async fn next(&mut self) -> Option<Value> {
        loop {
            if self.first {
                self.first = false;
                let revision = *self.receiver.borrow_and_update();
                if revision > 0 {
                    return Some(json!(revision));
                }
                continue;
            }
            self.receiver.changed().await.ok()?;
            let revision = *self.receiver.borrow_and_update();
            if revision > 0 {
                return Some(json!(revision));
            }
        }
    }
}

pub fn open_refreshes(payload: &Value) -> Result<RefreshSubscription, Value> {
    if payload.is_null() {
        return Err(decode_failure("Expected object | array"));
    }
    Ok(RefreshSubscription { receiver: PullRequestService::shared().subscribe_refreshes(), first: true })
}
