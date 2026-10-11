//! Whether this server is going down only to be replaced by an update
//! (`apps/server/src/cloud/updateHandoff.ts`). Shutdown keeps the managed T3 Connect tunnel across
//! those restarts instead of releasing it ([`crate::cloud_link::UpdateHandoff`]).
//!
//! Two independent signals, both under `<home>/runtime`:
//!
//! * The service launcher's state file has a *pending* update and no `.service-stopping` marker next
//!   to it. A pending update alone is not proof a replacement is coming: an explicit stop
//!   (`t3 service uninstall`, `systemctl stop`, `launchctl bootout`) during the pending window tears
//!   the server down too, and the launcher marks that case just before it signals the child.
//! * The desktop app's `desktop-update-restart` marker, written right before it stops this server to
//!   install an update. Reading it consumes it, and only a marker younger than a minute counts, so a
//!   marker no server read (a hard kill) cannot keep the tunnel on a later quit.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use t3_contracts::DESKTOP_UPDATE_RESTART_MARKER_FILE;

use crate::clock::Clock;
use crate::service_protocol::{
    SERVICE_STATE_FILE, SERVICE_STOP_MARKER_FILE, service_state_has_pending_update,
};

/// The desktop app stops its backends within seconds of writing the marker.
pub const DESKTOP_UPDATE_RESTART_MARKER_TTL: Duration = Duration::from_secs(60);

fn runtime_dir(base_dir: &Path) -> PathBuf {
    base_dir.join("runtime")
}

/// `pendingServiceUpdateExists`: the launcher owns this durable state, so it is read directly.
pub fn pending_service_update_exists(base_dir: &Path) -> bool {
    std::fs::read_to_string(runtime_dir(base_dir).join(SERVICE_STATE_FILE))
        .is_ok_and(|text| service_state_has_pending_update(&text))
}

/// `pendingUpdateHandoffExists`: a pending update that is not an explicit stop.
pub fn pending_update_handoff_exists(base_dir: &Path) -> bool {
    pending_service_update_exists(base_dir)
        && !runtime_dir(base_dir)
            .join(SERVICE_STOP_MARKER_FILE)
            .exists()
}

/// `desktopUpdateRestartPending`: consumes the marker and says whether it was fresh.
pub fn desktop_update_restart_pending(base_dir: &Path, now_ms: i64) -> bool {
    let marker = runtime_dir(base_dir).join(DESKTOP_UPDATE_RESTART_MARKER_FILE);
    let Ok(metadata) = std::fs::metadata(&marker) else {
        return false;
    };
    let _ = std::fs::remove_file(&marker);
    let Some(written_at) = metadata
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
    else {
        return false;
    };
    now_ms - (written_at.as_millis() as i64) < DESKTOP_UPDATE_RESTART_MARKER_TTL.as_millis() as i64
}

/// The server's [`crate::cloud_link::UpdateHandoff`], reading the files above.
pub struct FsUpdateHandoff {
    pub base_dir: PathBuf,
    pub clock: Arc<dyn Clock>,
}

#[async_trait]
impl crate::cloud_link::UpdateHandoff for FsUpdateHandoff {
    async fn desktop_update_restart_pending(&self) -> bool {
        desktop_update_restart_pending(&self.base_dir, self.clock.now_ms())
    }

    async fn pending_update_handoff_exists(&self) -> bool {
        pending_update_handoff_exists(&self.base_dir)
    }
}
