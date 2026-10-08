//! Native backend services. Services own behavior; transports only decode and dispatch.
pub mod acp_peer;
pub mod auth;
pub mod codex;
pub mod codex_runtime;
pub mod config;
pub mod execution;
pub mod history;
pub mod launch;
pub mod persistence;
pub mod project;
pub mod provider_process;
pub mod provider_registry;
pub mod thread;
pub mod transport;
pub mod wire_projection;
pub mod workspace_entries;
pub mod workspace_files;

pub mod terminal_history;

pub mod terminal_utf8;

pub mod terminal_environment;
pub mod terminal_io;
pub mod terminal_manager;
pub mod terminal_process;
pub mod terminal_store;
