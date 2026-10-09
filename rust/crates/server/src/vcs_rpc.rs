//! The `vcs.*` socket methods and the `subscribeVcsStatus` stream. Each handler decodes its
//! payload, calls one `VcsService` method, and maps the failure to its wire `_tag`.
use crate::{
    transport::ApiState,
    vcs_input,
    vcs_service::{VcsContext, VcsService},
    vcs_status::{GitStatusSource, StatusStream},
};
use serde_json::{Value, json};

pub const METHODS: [&str; 8] = [
    "vcs.pull",
    "vcs.refreshStatus",
    "vcs.listRefs",
    "vcs.createWorktree",
    "vcs.removeWorktree",
    "vcs.createRef",
    "vcs.switchRef",
    "vcs.init",
];

pub fn context(state: &ApiState) -> VcsContext {
    // The status the checkout shows reads pull requests with the same credentials as the rest.
    VcsService::shared().status().source().use_settings(state.settings.clone());
    VcsContext {
        store: state.store.clone(),
        settings: state.settings.clone(),
        background: state.background.clone(),
    }
}

fn decode_failure(message: String) -> Value {
    json!({"_tag":"SchemaDecodeError","message":message})
}

fn encode<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("typed vcs result")
}

pub async fn command(state: &ApiState, tag: &str, payload: Value) -> Result<Value, Value> {
    let (service, context) = (VcsService::shared(), context(state));
    match tag {
        "vcs.refreshStatus" => {
            let input = vcs_input::status_input(&payload).map_err(decode_failure)?;
            let status = Box::pin(service.refresh_status(&input.cwd, &context)).await;
            status.map(|status| encode(&status)).map_err(|e| e.wire())
        }
        "vcs.pull" => {
            let input = vcs_input::status_input(&payload).map_err(decode_failure)?;
            let result = Box::pin(service.pull(&input.cwd, &context)).await;
            result.map(|result| encode(&result)).map_err(|e| e.wire())
        }
        "vcs.listRefs" => {
            let input = vcs_input::list_refs_input(&payload).map_err(decode_failure)?;
            let result = Box::pin(service.list_refs(&input)).await;
            result.map(|result| encode(&result)).map_err(|e| e.wire())
        }
        "vcs.createWorktree" => {
            let input = vcs_input::create_worktree_input(&payload).map_err(decode_failure)?;
            Box::pin(service.create_worktree(&input, &context))
                .await
                .map_err(|e| e.wire())
        }
        "vcs.removeWorktree" => {
            let input = vcs_input::remove_worktree_input(&payload).map_err(decode_failure)?;
            let result = Box::pin(service.remove_worktree(&input, &context)).await;
            result.map(|()| Value::Null).map_err(|e| e.wire())
        }
        "vcs.createRef" => {
            let input = vcs_input::create_ref_input(&payload).map_err(decode_failure)?;
            let result = Box::pin(service.create_ref(&input, &context)).await;
            result
                .map(|ref_name| json!({"refName":ref_name}))
                .map_err(|e| e.wire())
        }
        "vcs.switchRef" => {
            let input = vcs_input::switch_ref_input(&payload).map_err(decode_failure)?;
            let result = Box::pin(service.switch_ref(&input, &context)).await;
            result
                .map(|ref_name| json!({"refName":ref_name}))
                .map_err(|e| e.wire())
        }
        "vcs.init" => {
            let input = vcs_input::init_input(&payload).map_err(decode_failure)?;
            let result = Box::pin(service.init(&input, &context)).await;
            result.map(|()| Value::Null).map_err(|e| e.wire())
        }
        _ => Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    }
}

/// A `subscribeVcsStatus` subscription: the snapshot, then every change as its wire event.
/// Dropping it releases the poller it holds, so ending the request's task is enough to clean up.
pub struct StatusSubscription(StatusStream<GitStatusSource>);

impl StatusSubscription {
    pub async fn next(&mut self) -> Option<Value> {
        self.0.next().await.map(|event| event.wire())
    }
}

pub async fn open_status(state: &ApiState, payload: &Value) -> Result<StatusSubscription, Value> {
    let input = vcs_input::status_subscription_input(payload).map_err(decode_failure)?;
    let hooks = context(state).hooks();
    let stream = VcsService::shared()
        .status()
        .open_stream(&input.cwd, input.include_remote != Some(false), &hooks)
        .await
        .map_err(|e| e.wire())?;
    Ok(StatusSubscription(stream))
}

#[cfg(test)]
#[path = "vcs_parity_tests.rs"]
mod parity_tests;

/// How many live status subscriptions hold the remote-refresh poller of `cwd` (a canonical
/// path); `None` when no poller runs. For tests of subscription cleanup.
#[doc(hidden)]
pub fn status_poller_subscribers(cwd: &str) -> Option<usize> {
    VcsService::shared().status().poller_subscribers(cwd)
}

/// Returns once no status subscription holds the poller of `cwd` (a canonical path).
#[doc(hidden)]
pub async fn wait_for_no_status_poller(cwd: &str) {
    VcsService::shared().status().wait_for_no_poller(cwd).await
}
