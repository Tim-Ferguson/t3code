//! Native backend services. Services own behavior; transports only decode and dispatch.
pub mod acp_adapter;
pub mod acp_client_policy;
pub mod acp_mcp_tools;
pub mod acp_model;
pub mod acp_peer;
pub mod acp_runtime;
pub mod acp_tools;
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

pub mod terminal_activity;
pub mod terminal_environment;
pub mod terminal_inspector;
pub mod terminal_io;
pub mod terminal_manager;
pub mod terminal_process;
pub mod terminal_store;

pub mod native_telemetry;
pub mod resource_binary;
pub mod resource_policy;

pub mod resource_ports;

pub mod resource_discovery;

pub mod resource_model;

pub mod resource_history;

pub mod desktop_telemetry;

pub mod resource_attribution;
pub mod resource_telemetry_service;

pub mod desktop_telemetry_bootstrap;

pub mod host_resources;
mod host_system;

pub mod process_diagnostics;

pub mod bootstrap;

mod acp_client_terminals;

mod acp_client_callbacks;

pub mod server_secret_store;

pub mod background_settings;

pub mod server_settings_model;

pub mod server_settings_secrets;

pub mod server_settings;

pub mod server_settings_runtime;

pub mod acp_registry_support;

#[cfg(test)]
mod acp_registry_rpc_tests;
