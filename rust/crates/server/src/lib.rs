//! Native backend services. Services own behavior; transports only decode and dispatch.
pub mod auth;
pub mod codex;
pub mod codex_runtime;
pub mod config;
pub mod execution;
pub mod launch;
pub mod persistence;
pub mod project;
pub mod provider_process;
pub mod provider_registry;
pub mod thread;
pub mod transport;
