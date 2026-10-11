//! The desktop app's update, driven over the telemetry control channel (`desktopUpdate/DesktopAppUpdate.ts`). A server the T3 Code
//! desktop app started asks the app to check for and download its own update, streams the stages to the client, and hands back
//! a token; the client then commits, which has the app install it (and stop this server). The app's own feed decides what it
//! installs, so the version a client names is ignored. Nothing here downloads or installs anything: the app does, behind the
//! control channel.
use crate::desktop_telemetry::DesktopTelemetryReceiver;
use async_trait::async_trait;
use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use t3_contracts::{DesktopUpdateStatusReport, DesktopUpdateStatusSchema, DesktopUpdateRemoteOutcome};
use t3_remote::self_update::{DesktopAppUpdate, HandoffCallback, ProgressReporter, ServerSelfUpdateError, ServerSelfUpdateProgressStage, ServerSelfUpdateResult};

/// Backstop for a desktop updater that hangs without ever reporting a terminal outcome. Generous: it covers a slow download of a
/// full build.
pub const DESKTOP_UPDATE_TIMEOUT: Duration = Duration::from_secs(20 * 60);
pub const DESKTOP_INSTALL_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// The progress stage a desktop update state maps to, or none when the state carries no progress worth streaming.
fn progress_stage(status: DesktopUpdateStatusSchema) -> Option<ServerSelfUpdateProgressStage> {
    match status {
        DesktopUpdateStatusSchema::Checking | DesktopUpdateStatusSchema::Available | DesktopUpdateStatusSchema::Downloading => Some(ServerSelfUpdateProgressStage::Downloading),
        DesktopUpdateStatusSchema::Downloaded => Some(ServerSelfUpdateProgressStage::Installing),
        _ => None,
    }
}

pub struct DesktopUpdates {
    receiver: DesktopTelemetryReceiver,
    /// A server the desktop app started with the control channel open (`mode === "desktop"` and a control descriptor).
    available: bool,
    in_flight: AtomicBool,
}

impl DesktopUpdates {
    pub fn new(receiver: DesktopTelemetryReceiver, available: bool) -> Self {
        Self { receiver, available, in_flight: AtomicBool::new(false) }
    }

    async fn consume(&self, request_id: &str, changes: &mut crate::desktop_telemetry::DesktopUpdateSubscription, report: &ProgressReporter) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError> {
        let mut last: Option<ServerSelfUpdateProgressStage> = None;
        let mut emit = |stage: Option<ServerSelfUpdateProgressStage>| {
            let changed = stage.filter(|stage| last != Some(*stage));
            if changed.is_some() {
                last = changed;
            }
            changed
        };
        let terminal: DesktopUpdateStatusReport = loop {
            let Some(next) = changes.recv().await else {
                return Err(ServerSelfUpdateError::new("The desktop app stopped reporting its update."));
            };
            if next.request_id.as_ref().map(|id| id.as_str()) != Some(request_id) {
                continue;
            }
            match next.outcome {
                None => {
                    if let Some(stage) = emit(progress_stage(next.state.status)) {
                        report(stage).await?;
                    }
                }
                Some(_) => break next,
            }
        };
        match terminal.outcome {
            Some(DesktopUpdateRemoteOutcome::ReadyToInstall) => {
                if let Some(stage) = emit(Some(ServerSelfUpdateProgressStage::Installing)) {
                    report(stage).await?;
                }
                let target = terminal.state.downloaded_version.clone().or_else(|| terminal.state.available_version.clone()).unwrap_or_else(|| terminal.state.current_version.clone());
                tracing::info!(target_version = %target, "Desktop app update prepared for install.");
                Ok(ServerSelfUpdateResult { target_version: target, method: t3_contracts::ServerSelfUpdateMethod::DesktopApp, update_id: None, desktop_update_token: Some(request_id.to_owned()) })
            }
            Some(DesktopUpdateRemoteOutcome::UpToDate) => Err(ServerSelfUpdateError::new(format!("The T3 Code desktop app on this machine is already up to date on {}.", terminal.state.current_version))),
            _ => Err(ServerSelfUpdateError::new(terminal.reason.as_ref().map(|reason| reason.as_str().to_owned()).or(terminal.state.message.clone()).unwrap_or_else(|| "The desktop app update failed.".into()))),
        }
    }
}

/// Tells the app to stop the update it was asked for when the request is abandoned (the client left, or the backstop expired).
struct CancelOnDrop {
    receiver: DesktopTelemetryReceiver,
    request_id: String,
    armed: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let (receiver, id) = (self.receiver.clone(), self.request_id.clone());
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = receiver.cancel_update(&id).await;
                });
            }
        }
    }
}

/// Releases the single in-flight slot unless the update reached a state the app owns from here.
struct Slot<'a>(&'a AtomicBool);
impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl DesktopAppUpdate for DesktopUpdates {
    fn available(&self) -> bool {
        self.available
    }

    async fn run(&self, report: ProgressReporter) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError> {
        if !self.available {
            return Err(ServerSelfUpdateError::new("This server was not started by the T3 Code desktop app, so it cannot drive a desktop update."));
        }
        if self.in_flight.swap(true, Ordering::SeqCst) {
            return Err(ServerSelfUpdateError::new("A desktop app update is already in progress."));
        }
        let _slot = Slot(&self.in_flight);
        let request_id = uuid::Uuid::new_v4().to_string();
        // Subscribed before the request is sent, so a fast first report cannot be missed.
        let mut changes = self.receiver.updates();
        let mut cancel = CancelOnDrop { receiver: self.receiver.clone(), request_id: request_id.clone(), armed: true };
        self.receiver.request_update(&request_id).await.map_err(|_| ServerSelfUpdateError::new("Could not reach the T3 Code desktop app on this machine."))?;
        let outcome = tokio::time::timeout(DESKTOP_UPDATE_TIMEOUT, self.consume(&request_id, &mut changes, &report)).await;
        match outcome {
            // The timeout drops the work, and the guard then cancels the app's update as an interruption does.
            Err(_) => Err(ServerSelfUpdateError::new("The desktop app did not finish the update in time.")),
            Ok(result) => {
                // Finished on its own, with an answer or an error: the app has nothing left to cancel.
                cancel.armed = false;
                result
            }
        }
    }

    async fn commit(&self, request_id: &str, on_handoff_accepted: HandoffCallback) -> Result<Infallible, ServerSelfUpdateError> {
        if !self.available {
            return Err(ServerSelfUpdateError::new("This server cannot commit a desktop app update."));
        }
        let mut changes = self.receiver.updates();
        let latest = changes.latest.take();
        self.receiver.commit_update(request_id).await.map_err(|_| ServerSelfUpdateError::new("Could not reach the T3 Code desktop app."))?;
        on_handoff_accepted();
        let failed = |report: &DesktopUpdateStatusReport| report.request_id.as_ref().map(|id| id.as_str()) == Some(request_id) && report.outcome == Some(DesktopUpdateRemoteOutcome::Failed);
        let wait = async {
            if let Some(latest) = latest.filter(failed) {
                return Some(latest);
            }
            loop {
                let next = changes.recv().await?;
                if failed(&next) {
                    return Some(next);
                }
            }
        };
        let terminal = tokio::time::timeout(DESKTOP_INSTALL_TIMEOUT, wait).await.map_err(|_| ServerSelfUpdateError::new("The desktop app did not report an install result in time."))?;
        let Some(report) = terminal else {
            return Err(ServerSelfUpdateError::new("The desktop app stopped reporting the install."));
        };
        Err(ServerSelfUpdateError::new(report.reason.as_ref().map(|reason| reason.as_str().to_owned()).or(report.state.message.clone()).unwrap_or_else(|| "The desktop app failed to install the update.".into())))
    }
}
