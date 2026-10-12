//! Rust wire contracts. Names and serialized fields follow packages/contracts.
//! Unported nested payloads are retained verbatim rather than silently discarded.

pub mod auth;
pub mod base;
pub mod orchestration;
pub mod permissions;
pub mod provider;
pub mod rpc;
pub mod thread_command;

pub use auth::*;
pub use base::*;
pub use orchestration::*;
pub use permissions::*;
pub use provider::*;
pub use rpc::*;
pub use thread_command::*;

pub mod server_config;
pub use server_config::*;

pub mod settings;
pub use settings::*;

pub mod api_config;
pub use api_config::*;

pub mod execution;
pub mod messages;
pub mod turn_items;
pub use execution::*;
pub use messages::*;
pub use turn_items::*;

pub mod client_settings;
pub use client_settings::*;

pub mod history;
pub use history::*;

pub mod filesystem;
pub use filesystem::*;

pub mod terminal;
pub use terminal::*;

pub mod provider_runtime;
pub use provider_runtime::*;

pub mod preview;
pub use preview::*;
pub mod preview_session;
pub use preview_session::*;
pub mod preview_automation;
pub use preview_automation::*;
pub mod preview_automation_inputs;
pub use preview_automation_inputs::*;
pub mod resource_telemetry;
pub use resource_telemetry::*;

pub mod resource_discovery;
pub use resource_discovery::*;

pub mod diagnostics;
pub use diagnostics::*;

pub mod desktop_bootstrap;
pub use desktop_bootstrap::*;

pub mod acp_registry;
pub use acp_registry::*;
pub mod provider_setup;
pub use provider_setup::*;
pub mod provider_manage;
pub use provider_manage::*;

pub mod settings_rpc;
pub use settings_rpc::*;

pub mod background;
pub use background::*;

pub mod device;
pub use device::*;

pub mod state_import;
pub use state_import::*;

pub mod mcp_control;
pub use mcp_control::*;

pub mod assets;
pub use assets::*;

pub mod checkpoint_diff;
pub use checkpoint_diff::*;

pub mod source_control;
pub use source_control::*;

pub mod keybindings;
pub use keybindings::*;

pub mod thread_search;
pub use thread_search::*;

pub mod scheduled_task;
pub use scheduled_task::*;

pub mod pull_request;
pub use pull_request::*;

pub mod worktree_setup;
pub use worktree_setup::*;

pub mod project_clone;
pub use project_clone::*;

pub mod project_icon;
pub use project_icon::*;

pub mod desktop_app_activation;
pub use desktop_app_activation::*;
