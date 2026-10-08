//! Drawer geometry and exit transitions from ThreadTerminalDrawer.tsx.
use crate::{
    terminal_session::SessionStatus,
    terminal_ui::{DEFAULT_HEIGHT, Direction, Group, PaneState},
};

pub fn clamp_height(height: f64, viewport_height: Option<f64>) -> f64 {
    let maximum = viewport_height
        .map(|height| (height * 0.75).floor().max(180.0))
        .unwrap_or(DEFAULT_HEIGHT);
    let safe = if height.is_finite() {
        height
    } else {
        DEFAULT_HEIGHT
    };
    (safe + 0.5).floor().max(180.0).min(maximum)
}
pub fn displayed_groups(pane: &PaneState) -> Vec<Group> {
    let pane = pane.normalized();
    let mut groups = pane.terminal_groups;
    groups.sort_by_key(|group| {
        group
            .terminal_ids
            .iter()
            .filter_map(|id| {
                pane.terminal_ids
                    .iter()
                    .position(|candidate| candidate == id)
            })
            .min()
            .unwrap_or(usize::MAX)
    });
    groups
}
pub fn group_label(group: &Group) -> &'static str {
    if group.terminal_ids.len() <= 1 {
        "Single"
    } else if group.split_direction == Some(Direction::Vertical) {
        "Stacked"
    } else {
        "Side by side"
    }
}
pub fn should_handle_exit(
    current: SessionStatus,
    synchronized: SessionStatus,
    handled: bool,
    version: u64,
) -> bool {
    version > 0
        && matches!(current, SessionStatus::Closed | SessionStatus::Exited)
        && current != synchronized
        && !handled
}
#[derive(Default)]
pub struct ExitState {
    synchronized: SessionStatus,
    handled: bool,
}
impl ExitState {
    pub fn observe(&mut self, status: SessionStatus, version: u64) -> Option<&'static str> {
        if version > 0 && status == SessionStatus::Running {
            self.handled = false;
        }
        let message = if should_handle_exit(status, self.synchronized, self.handled, version) {
            self.handled = true;
            Some(if status == SessionStatus::Closed {
                "Terminal closed"
            } else {
                "Process exited"
            })
        } else {
            None
        };
        if version > 0 {
            self.synchronized = status;
        }
        message
    }
}
