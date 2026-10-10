//! Native backend services. Services own behavior; transports only decode and dispatch.
pub mod acp_adapter;
mod acp_interaction;
pub mod acp_auth;
pub mod acp_registry_manage;
pub mod acp_authentication_state;
mod acp_health;
mod acp_health_jobs;
pub mod acp_client_policy;
pub mod acp_coordinator;
mod acp_mcp_callback;
pub mod acp_mcp_tools;
pub mod acp_model;
pub mod acp_peer;
pub mod acp_runtime;
pub mod acp_tools;
pub mod auth;
pub mod codex;
pub mod codex_runtime;
mod codex_fork;
mod codex_approvals;
mod codex_projection;
mod codex_subagents;
mod codex_tool_presentation;
mod codex_usage;
pub mod config;
pub mod execution;
pub(crate) mod dispatch_error;
pub(crate) mod composer_context;
pub mod answer_attachments;
pub mod workflow_script;
pub mod environment_capabilities;
pub mod history;
pub mod launch;
pub mod mcp_invocation;
pub mod mcp_sessions;
pub mod mcp_device;
pub mod mcp_http;
pub mod agent_device_launcher;
pub mod persistence;
pub mod project;
pub mod provider_auth_flow;
pub mod provider_auth_rpc;
pub mod provider_auth_service;
pub mod provider_install;
pub mod provider_maintenance;
pub mod provider_manage;
mod provider_manage_rpc;
pub mod provider_process;
pub mod provider_registry;
mod provider_selection_transition;
mod provider_turn_context;
mod provider_failure;
pub mod provider_mcp;
mod provider_instructions;
mod claude_protocol;
mod claude_models;
mod claude_launch;
pub mod claude_instance;
mod claude_projection;
mod claude_subagents;
mod claude_background;
mod notification;
mod provider_continuation;
pub mod provider_idle;
mod claude_adapter;
mod claude_usage_limits;
mod xai_extension;
mod grok_acp;
pub mod grok_instance;
mod grok_flavor;
mod acp_flavor;
mod text_diff;
mod devin_acp;
mod claude_reset_credits;
mod claude_skills;
mod usage_transcripts;
mod usage_pricing;
mod usage_aggregation;
mod usage_service;
pub mod usage_rpc;
pub mod agent_sessions;
#[cfg(test)]
mod usage_tests;
pub mod thread;
mod thread_fork;
mod context_handoff;
mod context_transfer;
mod claude_fork;
#[cfg(test)]
mod fork_parity_tests;
pub(crate) mod background_work;
pub(crate) mod queued_runs;
pub(crate) mod subagents;
pub(crate) mod delegated_tasks;
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

pub mod acp_mcp_bridge;

pub mod mcp_access;

pub mod mcp_control;
pub mod mcp_tool_input;
mod mcp_tool_functions;
pub mod rpc_admission;
#[cfg(any(test, feature = "outbound-check"))]
pub mod rpc_response;
pub mod pull_request_watch;
pub mod pull_request_watcher;
#[cfg(test)] mod rpc_decode_tests;
#[cfg(test)] mod rpc_handler_decoders;
#[cfg(test)] mod rpc_admission_tests;
#[cfg(test)] mod rpc_response_tests;
mod dispatch_decode;
mod service_commands;
mod provider_switch;
mod usage_limits;
mod restart_continuation;
mod thread_settlement;
pub mod pull_request_sync;
pub mod server_lifecycle;
pub mod delegated_completion;
mod mcp_tools;
mod mcp_delegate;
mod mcp_extra;
mod mcp_threads;
mod mcp_attachments;
mod mcp_environment;
mod mcp_pull_requests;
mod mcp_worktree;

mod message_queue;
mod message_steering;

pub mod asset_access;
pub mod asset_github_media;
mod asset_native_icon;
mod asset_response;
pub mod asset_workspace;
mod attachment_claims;
mod attachment_http;
mod attachment_persist;
pub mod attachment_prompt;
mod attachment_rpc;
pub mod attachment_service;
pub mod attachment_store;
pub mod attachment_upload;
mod image_dimensions;

mod checkpoint_capture;
mod checkpoint_service;
pub mod checkpoint_diff;

mod vcs_cache;
mod vcs_error;
mod vcs_input;
mod vcs_registry;
pub mod vcs_rpc;
mod vcs_service;
mod vcs_status;
mod vcs_worktree;
pub mod source_control_decode;
pub mod source_control_azure_devops;
pub mod source_control_bitbucket;
pub mod source_control_detect;
pub mod source_control_discovery;
pub mod source_control_github_api;
pub mod source_control_github_auth;
pub mod source_control_github_budget;
pub mod source_control_github_cli;
pub mod source_control_github_credentials;
pub mod source_control_github_prs;
pub mod source_control_github_provider;
pub mod source_control_github_state;
pub mod source_control_gitlab;
pub mod source_control_host;
pub mod source_control_provider;
pub mod source_control_rate_limit;
pub mod source_control_registry;
pub mod source_control_repository;
pub mod source_control_rpc;
pub mod source_control_uri;
mod checkpoint_rollback;

mod project_folders;
pub mod external_launcher;
pub mod mcp_apps;
mod codex_live;
pub mod provider_live;
pub mod secret_requests;
pub mod trace_diagnostics;
pub mod project_clone;
pub mod project_rpc;

mod keybindings;
pub mod keybindings_service;
pub mod keybindings_rpc;

mod thread_search;
pub mod thread_search_rpc;
pub mod scheduled_tasks_schedule;
pub mod scheduled_tasks_webhook;
pub mod scheduled_tasks;
pub mod scheduled_tasks_rpc;

mod thread_pull_requests;
pub mod pull_request_queries;
pub mod pull_request_decode;
pub mod pull_request_github;
pub mod pull_request_service;
pub mod pull_request_rpc;
pub mod scheduled_tasks_http;
pub mod git_workflow;
pub mod git_workflow_rpc;
pub mod git_workflow_status;
pub mod git_workflow_stacked;
pub mod git_workflow_followup;
pub mod text_generation;
pub mod text_generation_context;
pub mod text_generation_run;
pub mod text_generation_select;
pub mod text_generation_service;
pub mod text_generation_title;
pub mod review_service;
pub mod pull_request_list;
pub mod pull_request_write;
pub mod pull_request_actions;
pub mod pull_request_batch;
pub mod pull_request_provider;
pub mod pull_request_github_provider;
pub mod pull_request_registry;
pub mod pull_request_guard;
pub mod pull_request_viewed;
pub mod pull_request_gitlab;
pub mod pull_request_gitlab_json;
pub mod pull_request_bitbucket;
pub mod pull_request_bitbucket_json;
pub mod pull_request_bitbucket_revisions;
pub mod pull_request_azure;
pub mod pull_request_azure_diff;
pub mod pull_request_azure_json;

mod prepared_run;
mod project_setup_script;
pub mod worktree_setup_tracker;
pub mod workspace_prepare;
pub mod workspace_rpc;
mod thread_cleanup;
mod thread_deletion;
