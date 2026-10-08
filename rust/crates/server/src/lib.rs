//! Native backend services. Services own behavior; transports only decode and dispatch.
pub mod acp_adapter;
pub mod acp_auth;
pub mod acp_authentication_state;
pub mod acp_client_policy;
pub mod acp_coordinator;
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
pub mod provider_auth_flow;
pub mod provider_auth_rpc;
pub mod provider_auth_service;
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

mod acp_registry_archives;
mod acp_registry_commands;
mod acp_registry_packages;
pub mod acp_registry_path;
mod acp_registry_search;
mod acp_registry_spawn;
pub mod acp_registry_support;
mod acp_registry_uninstall;

#[cfg(test)]
mod acp_registry_rpc_tests;

pub mod server_settings_migrations;

pub mod background_policy;
pub mod device_host_resolver;
#[cfg(test)]
mod device_host_rpc_tests;

#[cfg(test)]
mod background_rpc_tests;

pub mod device_toolchain;

pub mod device_platform;

pub mod local_device_host;

pub mod device_commands;
pub mod device_detail;
pub mod device_hub_proxy;
pub mod device_service;

#[cfg(all(test, unix))]
mod device_rpc_tests;

pub mod device_actions;

pub mod device_agent_daemon;

pub mod device_agent_target;

#[cfg(all(test, unix))]
mod provider_auth_rpc_tests;
