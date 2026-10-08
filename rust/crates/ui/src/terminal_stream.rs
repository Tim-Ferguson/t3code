//! Destination-authorized Effect streams. Each reattach gets a fresh renderer
//! generation; a complete incoming chunk is decoded before the buffer changes.
use crate::runtime::{self, StreamRequest, TransportHandle, UiModel};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::terminal_session::{BufferState, apply_metadata};
use t3_contracts::{
    AuthEnvironmentScope, TerminalAttachInput, TerminalAttachStreamEvent,
    TerminalMetadataStreamEvent, TerminalSummary,
};

pub struct SessionStream {
    wire: StreamRequest,
    buffer: BufferState,
    thread: String,
    terminal: String,
}
impl SessionStream {
    pub fn subscribe(
        handle: &TransportHandle,
        state: Store<UiModel>,
        input: TerminalAttachInput,
    ) -> Result<Self, String> {
        let operate = {
            let model = state.peek();
            model.destination.as_ref().is_some_and(|destination| {
                model
                    .environments
                    .allows(destination, AuthEnvironmentScope::TerminalOperate)
            })
        };
        let thread = input.thread_id.to_string();
        let terminal = input.terminal_id.0.to_string();
        let (method, payload) = if operate {
            (
                "terminal.attach",
                serde_json::to_value(input).map_err(|error| error.to_string())?,
            )
        } else {
            (
                "terminal.observe",
                json!({"threadId":thread,"terminalId":terminal}),
            )
        };
        let wire = runtime::request_stream(handle, state, method, payload)?;
        Ok(Self {
            wire,
            buffer: BufferState::next_attach_seed(),
            thread,
            terminal,
        })
    }
    pub fn buffer(&self) -> &BufferState {
        &self.buffer
    }
    #[cfg(test)]
    pub(crate) async fn wait_ready(&mut self) -> bool {
        self.wire.wait_ready().await
    }
    pub async fn next(&mut self) -> Option<Result<BufferState, String>> {
        let values = match self.wire.next().await? {
            Ok(values) => values,
            Err(error) => return Some(Err(error)),
        };
        let decoded = values
            .into_iter()
            .map(serde_json::from_value::<TerminalAttachStreamEvent>)
            .collect::<Result<Vec<_>, _>>();
        let events = match decoded {
            Ok(events) => events,
            Err(error) => return Some(Err(format!("Invalid terminal stream: {error}"))),
        };
        for event in &events {
            let (thread, terminal) = match event {
                TerminalAttachStreamEvent::Snapshot { snapshot } => {
                    (&snapshot.thread_id, &snapshot.terminal_id)
                }
                TerminalAttachStreamEvent::Restarted { base, .. }
                | TerminalAttachStreamEvent::Output { base, .. }
                | TerminalAttachStreamEvent::Cleared { base }
                | TerminalAttachStreamEvent::Closed { base }
                | TerminalAttachStreamEvent::Exited { base, .. }
                | TerminalAttachStreamEvent::Error { base, .. }
                | TerminalAttachStreamEvent::Activity { base, .. } => {
                    (&base.thread_id, &base.terminal_id)
                }
            };
            if thread.as_str() != self.thread || terminal.as_str() != self.terminal {
                return Some(Err("Terminal update belongs to a different session.".into()));
            }
        }
        for event in events {
            self.buffer.apply(&event)
        }
        Some(Ok(self.buffer.clone()))
    }
}
pub struct MetadataStream {
    wire: StreamRequest,
    terminals: Vec<TerminalSummary>,
}
impl MetadataStream {
    pub fn subscribe(handle: &TransportHandle, state: Store<UiModel>) -> Result<Self, String> {
        Ok(Self {
            wire: runtime::request_stream(handle, state, "subscribeTerminalMetadata", json!({}))?,
            terminals: vec![],
        })
    }
    pub async fn next(&mut self) -> Option<Result<Vec<TerminalSummary>, String>> {
        let values = match self.wire.next().await? {
            Ok(values) => values,
            Err(error) => return Some(Err(error)),
        };
        let events = match values
            .into_iter()
            .map(serde_json::from_value::<TerminalMetadataStreamEvent>)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(events) => events,
            Err(error) => return Some(Err(format!("Invalid terminal metadata: {error}"))),
        };
        for event in events {
            apply_metadata(&mut self.terminals, &event)
        }
        Some(Ok(self.terminals.clone()))
    }
}
