//! The server's side of the launcher IPC (`apps/server/src/cloud/serviceLauncherClient.ts`). When a
//! background-service launcher starts the server it sets `T3_SERVICE_LAUNCHER_CONTEXT` and keeps an
//! IPC channel open to it; this client validates that context against the running version, and
//! exchanges the two request/reply pairs of [`crate::service_protocol`]:
//!
//! * `request_update`: ask the launcher to switch to a prepared version; it answers
//!   `update-accepted` (with the update id) or `update-rejected` (with a reason to show).
//! * `prepare_trial`: a child started on a new version tells the launcher it is `prepared` and waits
//!   for `committed`; the committed outcome is what `server lifecycle ready` reports.
//!
//! The IPC transport itself is the shell's ([`ServiceLauncherHost`]): in the original it is Node's
//! `process.send` channel. Each exchange subscribes before it sends, gives up after 30 seconds, and
//! fails at once when the channel disconnects.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::service_protocol::{
    SERVICE_LAUNCHER_CONTEXT_ENV, ServerSelfUpdateOutcome, ServiceLauncherChildMessage,
    ServiceLauncherContext, ServiceLauncherParentMessage, ServiceUpdateRecord,
    ServiceUpdateStatus, decode_service_launcher_context, decode_service_launcher_parent_message,
};

const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// What the launcher channel delivers.
#[derive(Debug, Clone)]
pub enum LauncherEvent {
    /// A message from the launcher (not yet decoded; unknown shapes are ignored).
    Message(Value),
    Disconnect,
}

/// `ServiceLauncherProcess`: the IPC channel to the launcher.
pub trait ServiceLauncherHost: Send + Sync {
    /// Whether this process was started with an open channel to a launcher.
    fn connected(&self) -> bool;
    /// Sends one message; an error is a failed send.
    fn send(&self, message: &ServiceLauncherChildMessage) -> Result<(), String>;
    /// Events from now on. Called before `send`, so a fast reply is never missed.
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LauncherEvent>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceLauncherOperation {
    DecodeContext,
    VersionMismatch,
    IpcUnavailable,
    Unmanaged,
    Send,
    Disconnect,
    Timeout,
}

/// `ServiceLauncherClientError`
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", operation_message(.operation))]
pub struct ServiceLauncherClientError {
    pub operation: ServiceLauncherOperation,
    pub cause: Option<String>,
}

fn operation_message(operation: &ServiceLauncherOperation) -> &'static str {
    match operation {
        ServiceLauncherOperation::DecodeContext => {
            "The service launcher supplied invalid startup context."
        }
        ServiceLauncherOperation::VersionMismatch => {
            "The service launcher started a different t3 version."
        }
        ServiceLauncherOperation::IpcUnavailable => "The service launcher IPC channel is unavailable.",
        ServiceLauncherOperation::Unmanaged => "This server is not managed by the launcher.",
        ServiceLauncherOperation::Send => "Could not send a request to the service launcher.",
        ServiceLauncherOperation::Disconnect => {
            "The service launcher disconnected before acknowledging the request."
        }
        ServiceLauncherOperation::Timeout => {
            "The service launcher did not respond within 30 seconds."
        }
    }
}

impl ServiceLauncherClientError {
    fn new(operation: ServiceLauncherOperation) -> Self {
        Self {
            operation,
            cause: None,
        }
    }
}

/// `ServiceLauncherRejectedError`
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct ServiceLauncherRejected {
    pub target_version: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestUpdateError {
    #[error(transparent)]
    Client(#[from] ServiceLauncherClientError),
    #[error(transparent)]
    Rejected(#[from] ServiceLauncherRejected),
}

/// The decoded startup context and whether this process is launcher managed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceLauncherStartup {
    pub context: Option<ServiceLauncherContext>,
    pub managed: bool,
}

/// `resolveStartup`: validates `T3_SERVICE_LAUNCHER_CONTEXT` (from `env`) against this build's
/// version and the channel. No context means not managed; a context that does not decode, names
/// another version, or has no open channel is an error rather than a silent unmanaged start.
pub fn resolve_service_launcher_startup(
    env: &BTreeMap<String, String>,
    channel_connected: bool,
    current_version: &str,
) -> Result<ServiceLauncherStartup, ServiceLauncherClientError> {
    let raw = env.get(SERVICE_LAUNCHER_CONTEXT_ENV);
    let context = match raw {
        None => None,
        Some(raw) => Some(decode_service_launcher_context(raw).ok_or_else(|| {
            ServiceLauncherClientError::new(ServiceLauncherOperation::DecodeContext)
        })?),
    };
    if let Some(context) = &context {
        if context.child_version != current_version {
            return Err(ServiceLauncherClientError::new(
                ServiceLauncherOperation::VersionMismatch,
            ));
        }
    }
    let managed = context.is_some() && channel_connected;
    if context.is_some() && !managed {
        return Err(ServiceLauncherClientError::new(
            ServiceLauncherOperation::IpcUnavailable,
        ));
    }
    Ok(ServiceLauncherStartup { context, managed })
}

/// `resolveServiceLauncherMode`
pub fn resolve_service_launcher_mode(
    env: &BTreeMap<String, String>,
    channel_connected: bool,
    current_version: &str,
) -> Result<bool, ServiceLauncherClientError> {
    resolve_service_launcher_startup(env, channel_connected, current_version).map(|s| s.managed)
}

/// `ServiceLauncherClient`
pub struct ServiceLauncherClient {
    host: Arc<dyn ServiceLauncherHost>,
    context: Option<ServiceLauncherContext>,
    managed: bool,
}

impl ServiceLauncherClient {
    pub fn new(
        host: Arc<dyn ServiceLauncherHost>,
        env: &BTreeMap<String, String>,
        current_version: &str,
    ) -> Result<Self, ServiceLauncherClientError> {
        let startup =
            resolve_service_launcher_startup(env, host.connected(), current_version)?;
        Ok(Self {
            host,
            context: startup.context,
            managed: startup.managed,
        })
    }

    /// Whether a launcher manages this server (it can then update itself).
    pub fn managed(&self) -> bool {
        self.managed
    }

    async fn exchange(
        &self,
        message: ServiceLauncherChildMessage,
        accept: impl Fn(&ServiceLauncherParentMessage) -> bool,
    ) -> Result<ServiceLauncherParentMessage, ServiceLauncherClientError> {
        if !self.managed {
            return Err(ServiceLauncherClientError::new(
                ServiceLauncherOperation::Unmanaged,
            ));
        }
        let mut events = self.host.subscribe();
        if let Err(cause) = self.host.send(&message) {
            return Err(ServiceLauncherClientError {
                operation: ServiceLauncherOperation::Send,
                cause: Some(cause),
            });
        }
        let wait = async {
            loop {
                match events.recv().await {
                    Ok(LauncherEvent::Message(value)) => {
                        if let Some(reply) = decode_service_launcher_parent_message(&value) {
                            if accept(&reply) {
                                return Ok(reply);
                            }
                        }
                    }
                    Ok(LauncherEvent::Disconnect)
                    | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(ServiceLauncherClientError::new(
                            ServiceLauncherOperation::Disconnect,
                        ));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        };
        tokio::time::timeout(EXCHANGE_TIMEOUT, wait)
            .await
            .unwrap_or_else(|_| {
                Err(ServiceLauncherClientError::new(
                    ServiceLauncherOperation::Timeout,
                ))
            })
    }

    /// `requestUpdate`: the launcher's update id once it has accepted the switch.
    pub async fn request_update(
        &self,
        target_version: &str,
        db_path: &str,
    ) -> Result<String, RequestUpdateError> {
        let reply = self
            .exchange(
                ServiceLauncherChildMessage::RequestUpdate {
                    target_version: target_version.to_owned(),
                    db_path: db_path.to_owned(),
                },
                |reply| {
                    matches!(
                        reply,
                        ServiceLauncherParentMessage::UpdateAccepted { .. }
                            | ServiceLauncherParentMessage::UpdateRejected { .. }
                    )
                },
            )
            .await?;
        match reply {
            ServiceLauncherParentMessage::UpdateAccepted { update_id } => Ok(update_id),
            ServiceLauncherParentMessage::UpdateRejected { reason } => {
                Err(ServiceLauncherRejected {
                    target_version: target_version.to_owned(),
                    reason,
                }
                .into())
            }
            ServiceLauncherParentMessage::Committed { .. } => {
                unreachable!("the exchange only accepts accepted and rejected replies")
            }
        }
    }

    /// `prepareTrial`: when this child was started to try a pending update, confirm to the launcher
    /// and wait for its commit, returning the committed outcome; otherwise the last recorded outcome,
    /// if any.
    pub async fn prepare_trial(
        &self,
    ) -> Result<Option<ServerSelfUpdateOutcome>, ServiceLauncherClientError> {
        match self.context.as_ref().and_then(|c| c.update.as_ref()) {
            Some(ServiceUpdateRecord::Pending(pending)) => {
                let update_id = pending.id.clone();
                self.exchange(
                    ServiceLauncherChildMessage::Prepared {
                        update_id: update_id.clone(),
                    },
                    |reply| {
                        matches!(reply, ServiceLauncherParentMessage::Committed { update_id: id } if *id == update_id)
                    },
                )
                .await?;
                Ok(Some(ServerSelfUpdateOutcome {
                    id: pending.id.clone(),
                    from_version: pending.from_version.clone(),
                    target_version: pending.target_version.clone(),
                    status: ServiceUpdateStatus::Committed,
                    reason: None,
                }))
            }
            Some(ServiceUpdateRecord::Finished(outcome)) => Ok(Some(outcome.clone())),
            None => Ok(None),
        }
    }
}
