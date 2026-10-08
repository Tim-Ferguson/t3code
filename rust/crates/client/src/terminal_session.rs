//! Source terminalSession.ts reducers. Wire decoding happens before mutation.
use crate::terminal_output::{self, DEFAULT_MAX_BUFFER_BYTES, OutputState};
use serde::{Deserialize, Serialize};
use t3_contracts::{
    TerminalAttachStreamEvent, TerminalMetadataStreamEvent, TerminalSessionStatus, TerminalSummary,
};
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Starting,
    Running,
    Exited,
    Error,
    #[default]
    Closed,
}
impl From<TerminalSessionStatus> for SessionStatus {
    fn from(value: TerminalSessionStatus) -> Self {
        match value {
            TerminalSessionStatus::Starting => Self::Starting,
            TerminalSessionStatus::Running => Self::Running,
            TerminalSessionStatus::Exited => Self::Exited,
            TerminalSessionStatus::Error => Self::Error,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BufferState {
    pub output: OutputState,
    pub status: SessionStatus,
    pub error: Option<String>,
    pub updated_at: Option<String>,
    pub version: u64,
    pub lifecycle_version: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    pub summary: Option<TerminalSummary>,
    #[serde(flatten)]
    pub buffer: BufferState,
    pub has_running_subprocess: bool,
}
impl BufferState {
    pub fn next_attach_seed() -> Self {
        static GENERATION: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
        Self::seed(GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1)
    }
    pub fn seed(generation: i64) -> Self {
        let mut state = Self::default();
        state.output.generation = generation;
        state
    }
    pub fn apply(&mut self, event: &TerminalAttachStreamEvent) {
        self.apply_with_budget(event, DEFAULT_MAX_BUFFER_BYTES)
    }
    pub fn apply_with_budget(&mut self, event: &TerminalAttachStreamEvent, max_bytes: i64) {
        match event {
            TerminalAttachStreamEvent::Snapshot { snapshot }
            | TerminalAttachStreamEvent::Restarted { snapshot, .. } => {
                let restarted = matches!(event, TerminalAttachStreamEvent::Restarted { .. });
                if restarted || self.version > 0 {
                    self.lifecycle_version += 1;
                }
                self.output = terminal_output::reset(&self.output, &snapshot.history, max_bytes);
                self.status = snapshot.status.into();
                self.error = None;
                self.updated_at = Some(snapshot.updated_at.clone());
                self.version += 1;
            }
            TerminalAttachStreamEvent::Output { data, .. } => {
                self.output = terminal_output::append(&self.output, data, max_bytes);
                if self.status == SessionStatus::Closed {
                    self.status = SessionStatus::Running;
                }
                self.error = None;
                self.version += 1;
            }
            TerminalAttachStreamEvent::Cleared { .. } => {
                self.output = terminal_output::reset(&self.output, "", max_bytes);
                self.error = None;
                self.version += 1;
            }
            TerminalAttachStreamEvent::Exited { .. } => {
                self.status = SessionStatus::Exited;
                self.error = None;
                self.version += 1;
            }
            TerminalAttachStreamEvent::Closed { .. } => {
                self.status = SessionStatus::Closed;
                self.error = None;
                self.version += 1;
            }
            TerminalAttachStreamEvent::Error { message, .. } => {
                self.status = SessionStatus::Error;
                self.error = Some(message.to_string());
                self.version += 1;
            }
            TerminalAttachStreamEvent::Activity { .. } => {}
        }
    }
}
pub fn combine(summary: Option<&TerminalSummary>, buffer: &BufferState) -> SessionState {
    let mut combined = buffer.clone();
    if buffer.version == 0 {
        if let Some(summary) = summary {
            combined.status = summary.status.into();
        }
    }
    combined.updated_at = match (
        summary.map(|summary| summary.updated_at.as_str()),
        buffer.updated_at.as_deref(),
    ) {
        (None, right) => right.map(str::to_owned),
        (left, None) => left.map(str::to_owned),
        (Some(left), Some(right)) => {
            let left_date = chrono::DateTime::parse_from_rfc3339(left);
            let right_date = chrono::DateTime::parse_from_rfc3339(right);
            Some(
                if left_date
                    .ok()
                    .zip(right_date.ok())
                    .is_some_and(|(left, right)| left >= right)
                {
                    left
                } else {
                    right
                }
                .to_owned(),
            )
        }
    };
    SessionState {
        summary: summary.cloned(),
        buffer: combined,
        has_running_subprocess: summary.is_some_and(|summary| summary.has_running_subprocess),
    }
}
pub fn apply_metadata(current: &mut Vec<TerminalSummary>, event: &TerminalMetadataStreamEvent) {
    match event {
        TerminalMetadataStreamEvent::Snapshot { terminals } => *current = terminals.clone(),
        TerminalMetadataStreamEvent::Remove {
            thread_id,
            terminal_id,
        } => current.retain(|terminal| {
            terminal.thread_id != *thread_id || terminal.terminal_id != *terminal_id
        }),
        TerminalMetadataStreamEvent::Upsert { terminal } => {
            current.retain(|previous| {
                previous.thread_id != terminal.thread_id
                    || previous.terminal_id != terminal.terminal_id
            });
            current.push(terminal.clone());
        }
    }
}
