#![cfg_attr(test, recursion_limit = "512")]
mod appearance;
mod crash;
mod attachments;
mod background_activity;
mod branch;
mod changed_files;
mod client_settings;
mod command_palette;
mod composer_banners;
mod composer_menu;
mod composer_panels;
mod composer_rest;
mod item_cards;
mod base_ref_picker;
mod git_control;
mod pull_request_edit;
mod pull_request_glyph;
mod provider_glyph;
mod pull_request_handoff;
mod pull_request_links;
mod pull_request_list;
mod pull_request_menus;
mod pull_request_panel;
mod pull_request_review;
mod pull_request_summary;
mod pull_request_thread;
mod pull_request_files;
mod diff_panel;
mod right_panel;
#[cfg(feature = "desktop")]
mod desktop_updates;
#[cfg(feature = "desktop")]
mod cli_command_row;
mod file_media;
mod file_panel;
mod file_tree;
mod project_defaults;
mod projects_page;
mod pull_request_stack;
mod thread_pull_requests_panel;
mod thread_selection;
mod access_page;
mod acp_registry_step;
mod connect_page;
mod import_notice;
mod notifications;
mod project_search;
mod publish_dialog;
mod review_diff;
mod draft_sessions;
mod welcome;
mod draft_storage;
mod empty_state;
mod header_more;
mod environment_icon;
mod error_text;
mod file_preview;
mod font_service;
mod general_page;
mod keybinding_page;
mod lifecycle;
mod keys;
mod markdown;
mod model_controls;
mod model_picker;
mod new_thread;
mod notice;
mod open_in;
mod plan_save;
mod proactive_panels;
mod project_actions;
mod project_create;
mod provider_auth;
mod provider_auth_local;
mod provider_manage;
mod provider_settings;
mod rollback;
mod resource_section;
mod routing;
mod runtime;
mod scheduled_tasks_page;
mod scroll_state;
mod secret_card;
mod plan_thread;
mod prompt_stash;
mod project_menu;
mod sidebar_groups;
mod sidebar_rows;
mod sidebar_sections;
mod stage_art;
mod search;
mod settings_common;
mod settings_rows;

/// A setting's wire spelling, for an attribute the stylesheet selects on.
fn wire_text(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default()
}
mod server_update;
mod source_control_page;
mod storage_page;
mod subagent_rows;
mod thread_status_line;
mod syntax_highlighting;
mod terminal_bridge;
mod terminal_pane;
mod terminal_stream;
mod text_generation_page;
mod diagnostics_page;
mod theme_download;
mod theme_inspector;
mod theme_library;
mod theme_search;
mod themes;
mod thread_controls;
mod thread_lifecycle;
mod thread_relations;
mod timeline;
mod limits_view;
mod clone_view;
mod workspace_files;
mod composer_chips;
mod context_menu;
mod link_menus;
mod import_projects;
mod usage_hubs;
mod usage_page;
mod timeline_scroll;
mod work_rows;
mod worktree_setup;
mod connect_agent;
mod ssh_environments;
#[cfg(any(feature = "desktop", test))]
mod ssh_gateway;
#[cfg(feature = "desktop")]
mod desktop_shell;
#[cfg(feature = "desktop")]
pub use desktop_shell::launch as launch_desktop;
use dioxus::prelude::*;
use runtime::{UiModel, UiModelStoreExt, View};
use serde_json::{Value, json};
use t3_client::{connection::ConnectionStatus, rpc::RequestKind, timeline_items as words};

const STYLES: &str = include_str!("../assets/app.css");

/// Installs the web target's panic hook (the static error screen); see `crash`.
#[cfg(target_arch = "wasm32")]
pub use crash::install as install_panic_screen;

/// Lets the command palette open the "Add project" dialog the application shell owns.
#[derive(Clone, Copy)]
pub(crate) struct AddProject(pub Signal<bool>);

#[component]
pub fn App() -> Element {
    // `/connect-agent` is a document of its own, shown before anyone is signed in: the application, with its connection, is not started.
    if let Some((origin, query)) = connect_agent::requested() {
        return rsx! { connect_agent::Page { origin, query } };
    }
    rsx! { Main {} }
}

#[component]
fn Main() -> Element {
    // The web runtime sets its own panic hook when it starts; this puts the error screen back on top.
    #[cfg(target_arch = "wasm32")]
    use_hook(crash::install);
    use_context_provider(timeline_scroll::Positions::default);
    let state = use_store(UiModel::default);
    let transport = use_hook(runtime::TransportHandle::default);
    let startup_transport = transport.clone();
    let terminal_catalog = terminal_pane::use_catalog();
    themes::use_themes(state);
    client_settings::use_writer(state);
    font_service::use_fonts(state);
    draft_storage::use_writer(state);
    draft_storage::use_native_close_flush(state);
    draft_storage::use_flush_on_unload(state);
    #[cfg(feature = "desktop")]
    desktop_shell::use_shell(state);
    use_future(move || {
        let transport = startup_transport.clone();
        let terminal_catalog = terminal_catalog.clone();
        async move {
            terminal_catalog.hydrate().await;
            client_settings::hydrate(state).await;
            draft_storage::hydrate(state).await;
            prompt_stash::hydrate(state).await;
            right_panel::hydrate(state).await;
            #[cfg(feature = "desktop")]
            desktop_shell::connect(transport, state).await;
            #[cfg(not(feature = "desktop"))]
            match runtime::take_pairing_link() {
                Some(credential) => runtime::pair_from_link(transport, state, runtime::default_address(), credential).await,
                None => runtime::connect(transport, state, runtime::default_address(), String::new()).await,
            }
        }
    });
    let failed_import = import_notice::use_failed_import(state);
    let mut failed_import_again = failed_import;
    let failed = failed_import.read().clone().flatten();
    if let Some(status) = failed {
        return rsx! { style { "{STYLES}" } style { {include_str!("../assets/theme-tokens.css")} } import_notice::FailedScreen { status, retry: move |()| { #[cfg(feature = "desktop")] desktop_shell::restart_server(); failed_import_again.restart() } } };
    }
    rsx! { Application { state, transport } }
}

/// The About rows of the settings (version, update track), on the desktop app only.
#[cfg(feature = "desktop")]
pub(crate) fn desktop_update_rows() -> Element {
    rsx! { desktop_updates::Rows {} }
}
#[cfg(not(feature = "desktop"))]
pub(crate) fn desktop_update_rows() -> Element {
    rsx! {}
}

/// The `t3` command row of the settings, on the desktop app only.
#[cfg(feature = "desktop")]
pub(crate) fn desktop_cli_row() -> Element {
    rsx! { cli_command_row::Row {} }
}
#[cfg(not(feature = "desktop"))]
pub(crate) fn desktop_cli_row() -> Element {
    rsx! {}
}

/// The sidebar's update pill, on the desktop app only.
#[cfg(feature = "desktop")]
fn desktop_update_pill() -> Element {
    rsx! { desktop_updates::Pill {} }
}
#[cfg(not(feature = "desktop"))]
fn desktop_update_pill() -> Element {
    rsx! {}
}

/// The desktop app's strips over the page (the server starting, the quit hint). The web build has none.
#[cfg(feature = "desktop")]
fn desktop_overlays(state: Store<UiModel>) -> Element {
    rsx! { desktop_shell::Overlays { state } }
}
#[cfg(not(feature = "desktop"))]
fn desktop_overlays(_state: Store<UiModel>) -> Element {
    rsx! {}
}

/// "Browse…" beside the project directory when the server runs on this computer and the app can ask the system
/// for a folder (`pickFolder`). A folder is also the name for a project that has none yet.
#[cfg(feature = "desktop")]
fn browse_button(transport: &runtime::TransportHandle, mut title: Signal<String>, mut path: Signal<String>) -> Element {
    if !desktop_shell::server_is_local(transport) {
        return rsx! {};
    }
    rsx! {
        button { class: "browse-folder", r#type: "button", "aria-label": "Browse for a folder",
            onclick: move |_| {
                spawn(async move {
                    if let Some(folder) = desktop_shell::pick_folder().await {
                        if title.peek().trim().is_empty() {
                            if let Some(name) = std::path::Path::new(&folder).file_name() { title.set(name.to_string_lossy().into_owned()); }
                        }
                        path.set(folder);
                    }
                });
            },
            "Browse…"
        }
    }
}
#[cfg(not(feature = "desktop"))]
fn browse_button(_transport: &runtime::TransportHandle, _title: Signal<String>, _path: Signal<String>) -> Element {
    rsx! {}
}

#[component]
fn Application(state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<RenderProbe>() {
        probe.application.set(probe.application.get() + 1);
    }
    let mut address = use_signal(runtime::default_address);
    let mut token = use_signal(String::new);
    let mut credential_kind = use_signal(runtime::default_credential_kind);
    let mut add_project = use_signal(|| false);
    use_context_provider(|| AddProject(add_project));
    sidebar_sections::use_shelves(state);
    sidebar_groups::use_scope(state);
    // A chosen row that left the sidebar leaves the choice, so a bulk menu never counts rows nobody can see.
    use_effect(move || {
        let _ = state.shell().read().snapshot.as_ref().map(|snapshot| snapshot.snapshot_sequence);
        thread_selection::prune(state);
    });
    right_panel::use_persist(state);
    let runner = keys::use_keys(state, transport.clone());
    clone_view::use_stream(state, transport.clone());
    let new_thread_hint = runner.keys.label("chat.new");
    let mut project_title = use_signal(String::new);
    let mut project_path = use_signal(String::new);
    let shell = state.shell();
    let snapshot = shell.read();
    let projects = snapshot
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.projects.clone())
        .unwrap_or_default();
    let threads = snapshot
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.threads.clone())
        .unwrap_or_default();
    drop(snapshot);
    let (_, groups) = sidebar_groups::groups_of(&projects, &state.client_settings().read());
    let scope_items: Vec<(String, String)> = groups.iter().map(|group| (group.key.clone(), group.label.clone())).collect();
    let scoped = sidebar_groups::scoped_key(state, groups.iter().map(|group| group.key.as_str()));
    let row_caps = sidebar_rows::caps(state);
    // A snoozed row wakes when its time passes with nothing else changing. One timer is set for the
    // earliest wake time (never a clock that ticks), and the sidebar is built again when it fires.
    let mut wake_tick = use_signal(|| 0u64);
    let next_wake = use_memo(move || {
        let _ = wake_tick();
        let now = chrono::Utc::now();
        state.shell().read().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().filter_map(|thread| t3_client::sidebar::snoozed_until(thread, now)).min())
    });
    use_resource(move || async move {
        let Some(at) = next_wake() else { return };
        // A long snooze waits in day-long steps; the margin keeps the row from waking a moment early.
        let wait = (at - chrono::Utc::now()).num_milliseconds().clamp(0, 86_400_000) as u64 + 50;
        runtime::delay(wait).await;
        let next = *wake_tick.peek() + 1;
        wake_tick.set(next);
    });
    let _ = wake_tick();
    let active_id = state.active_thread().read().clone();
    let active = threads
        .iter()
        .find(|thread| Some(thread.id.as_str()) == active_id.as_deref())
        .cloned();
    let connected = *state.status().read() == ConnectionStatus::Connected;
    let scratch_available = project_create::scratch_available(state);
    let can_operate = connected
        && state
            .destination()
            .read()
            .as_ref()
            .is_some_and(|destination| {
                state.environments().read().allows(
                    destination,
                    t3_contracts::AuthEnvironmentScope::OrchestrationOperate,
                )
            });
    // Opening a thread that finished while the reader was away marks it visited, on the server, so
    // every device agrees. It is asked once: the answer clears the unread state this reads.
    let unseen = active.as_ref().filter(|_| can_operate).filter(|thread| t3_client::sidebar::has_unseen_completion(thread)).map(|thread| thread.id.to_string());
    use_effect(use_reactive(&unseen, {
        let transport = transport.clone();
        move |unseen| {
            // Against a server that does not track visits (`threadVisitedTracking`) there is nothing to tell it.
            let tracked = state.typed_config().peek().as_ref().is_some_and(|config| config.environment.capabilities.thread_visited_tracking == Some(true));
            if let Some(id) = unseen.filter(|_| tracked && state.status().peek().clone() == ConnectionStatus::Connected) {
                runtime::command(&transport, state, "thread.visit", json!({"threadId":id,"visitedAt":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)}));
            }
        }
    }));
    let title = active
        .as_ref()
        .map(|thread| thread.title.clone())
        .unwrap_or_default();
    let view = state.view().read().clone();
    let theme = if *state.dark().read() {
        "dark"
    } else {
        "light"
    };
    let palette = try_consume_context::<themes::Themes>()
        .map(|themes| {
            let snapshot = themes.snapshot.read();
            let catalog = themes.catalog.read();
            catalog
                .definition(catalog.half(
                    &snapshot.theme,
                    snapshot.theme_halves.as_ref(),
                    snapshot.resolved_theme,
                ))
                .map(|theme| theme.id.clone())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let layout_class = match (*state.sidebar_open().read(), cfg!(all(feature = "desktop", target_os = "macos"))) {
        (true, true) => "app sidebar-open desktop-inset",
        (true, false) => "app sidebar-open",
        (false, true) => "app desktop-inset",
        (false, false) => "app",
    };
    let status_label = match &*state.status().read() {
        ConnectionStatus::Connected => "Connected",
        ConnectionStatus::Connecting => "Connecting…",
        ConnectionStatus::Blocked(_) => "Connection needs attention",
        ConnectionStatus::Interrupted(_) => "Disconnected",
        ConnectionStatus::Disconnected => "Disconnected",
    };
    let error = state.error().read().clone();
    let stage = stage_art::artwork(state);
    // With "Composer context" on, the branch control sits under the composer of an open thread instead of in its header.
    let persist_context_strip = state.client_settings().read().persist_composer_context_strip;
    let saved_connections: Vec<_> = state
        .environments()
        .read()
        .records
        .values()
        .map(|record| {
            (
                record.id.clone(),
                record.label.clone(),
                record.address.clone(),
            )
        })
        .collect();
    let environment_key = state
        .destination()
        .read()
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let timeline_key =
        serde_json::to_string(&(&environment_key, &active_id)).expect("string identity");
    rsx! {
        style { "{STYLES}" }
        style { {include_str!("../assets/theme-tokens.css")} }
        import_notice::Notice { state, transport: transport.clone() }
        background_activity::Reporter { state, transport: transport.clone() }
        proactive_panels::Host { state }
        div { class: "{layout_class}", "data-theme": theme, "data-palette":palette,"data-contrast":if state.client_settings().read().appearance_contrast.0 == 100 {"default"} else {"custom"},style:format!("--ui-contrast:{};--panel-ms:{}ms;--glass-opacity:{}%{}",state.client_settings().read().appearance_contrast.0 as f64 / 100.0,state.client_settings().read().panel_animation_duration_ms.0,state.client_settings().read().glass_opacity.0,if state.client_settings().read().glass_opacity.0 == 100 {";--glass-blur:0px"} else {""}),"data-word-wrap":state.client_settings().read().word_wrap,"data-chat-width":wire_text(&state.client_settings().read().chat_width),"data-diff-color-scheme":wire_text(&state.client_settings().read().diff_color_scheme),
            aside { class: if stage.is_some() { "sidebar has-stage-art" } else { "sidebar" }, "aria-label": "Main sidebar",
                if let Some(variant) = stage { stage_art::Backdrop { key: "{variant:?}", variant } }
                header { class: "brand", Wordmark {} span { "Code" } stage_art::Pill { state } }
                {desktop_update_pill()}
                nav { class: "sidebar-actions",
                    button { title: match &new_thread_hint { Some(hint) => format!("New thread ({hint})"), None => "New thread".to_owned() }, onclick: {let transport=transport.clone();move |_| runtime::new_thread(&transport,state,None)}, "＋ New thread" }
                    button { onclick: move |_| add_project.set(true), "＋ Add project" }
                    button { "aria-label": "Import projects", disabled: !can_operate, title: "Import projects and conversations from Claude Code and Codex", onclick: move |_| import_projects::open(state), "＋ Import projects" }
                    button { "aria-label": "New project", disabled: !can_operate || project_create::new_projects_root(state).is_none(), onclick: move |_| command_palette::open(state, command_palette::Intent::NewProject), "＋ New project" }
                }
                search::SidebarSearch { state, transport: transport.clone() }
                if !scope_items.is_empty() { sidebar_groups::ScopeFilter { state, items: scope_items.clone() } }
                draft_sessions::Rows { state, transport: transport.clone() }
                div { class: "project-list",
                    sidebar_sections::DragBridge { state, transport: transport.clone() }
                    for group in groups.iter().filter(|group| scoped.as_ref().is_none_or(|key| *key == group.key)) {
                        {
                            let representative = &projects[group.representative];
                            let members: Vec<project_menu::Member> = group.members.iter().map(|member| { let project = &projects[member.index]; project_menu::Member { id: project.id.to_string(), title: project.title.to_string(), workspace_root: project.workspace_root.to_string() } }).collect();
                            let scope = sidebar_groups::Scope { key: representative.id.to_string(), ids: group.project_ids() };
                            let grouped = group.members.len();
                            rsx! {
                                div { class: "project-group", key: "{group.key}",
                                    div { class: "project-header",
                                        button { class: "project-heading", title: "{representative.workspace_root}", onclick: { let transport=transport.clone();let id = representative.id.to_string(); move |_| runtime::new_thread(&transport,state,Some(id.clone())) },
                                            file_preview::ProjectFavicon { key: "favicon:{representative.workspace_root}", state, transport: transport.clone(), environment_key: environment_key.clone(), cwd: representative.workspace_root.to_string(), title: representative.title.to_string() } "{group.label}"
                                            if grouped > 1 { span { class: "project-group-count", "aria-label": "{grouped} projects grouped", title: "{grouped} projects grouped", "{grouped}" } }
                                        }
                                        project_menu::ProjectMenu { key: "menu:{group.key}", state, transport: transport.clone(), label: group.label.clone(), members, can_operate }
                                    }
                                    for id in scope.ids.iter() { clone_view::ProjectState { key: "clone:{id}", state, project_id: id.clone() } }
                                    sidebar_sections::ProjectThreads { key: "threads:{group.key}", scope: scope.clone(), state, transport: transport.clone(), caps: row_caps, can_operate, tick: wake_tick() }
                                    for id in scope.ids.iter() { thread_lifecycle::Archived { key: "archived:{id}", state, transport: transport.clone(), project_id: id.clone() } }
                                }
                            }
                        }
                    }
                }
                footer { class: "sidebar-footer",
                    button { onclick: move |_| state.view().set(View::Connections), span { class: if connected { "status-dot online" } else { "status-dot" } } "{status_label}" }
                    button { onclick: move |_| state.view().set(View::Providers), "⚙ Settings" }
                }
            }
            main { class: "workspace", "data-slot":"sidebar-inset",
                if let Some(error)=state.draft_storage_error().read().clone() {div {class:"error-banner",role:"alert","{error}",button {onclick:move|_|{spawn(draft_storage::hydrate(state));},"Retry reading drafts"}}}
                if let Some(error)=state.client_settings_error().read().clone() {div {class:"error-banner",role:"alert","{error}",button {onclick:move|_|{spawn(client_settings::hydrate(state));},{if try_consume_context::<client_settings::Writer>().is_some_and(|writer|writer.document.peek().read_error.is_none()){"Retry saving preferences"}else{"Retry reading preferences"}}}}}
                header { class: "workspace-header",
                    button { class: "icon-button", "aria-label": "Toggle main sidebar", onclick: move |_| { let open=state.peek().sidebar_open; state.sidebar_open().set(!open); }, "☰" }
                    span { class: "workspace-title", "{title}" }
                    command_palette::Button { state }
                    if view == View::Chat && (active.is_some() || state.selected_project().read().is_some()) { right_panel::Toggle { state, hint: runner.keys.label("rightPanel.toggle") } }
                    if let Some(active_thread) = active.clone() {
                      div { class: "header-actions",
                        {
                            let project_cwd = projects.iter().find(|project| project.id == active_thread.project_id).map(|project| project.workspace_root.to_string()).unwrap_or_default();
                            let worktree = active_thread.worktree_path.as_ref().map(ToString::to_string);
                            let cwd = worktree.clone().unwrap_or_else(|| project_cwd.clone());
                            rsx! { if !cwd.is_empty() { open_in::OpenIn { key: "open-in:{timeline_key}:{cwd}", state, transport: transport.clone(), cwd: cwd.clone() } } if !cwd.is_empty() && !persist_context_strip { branch::Control { key: "branch:{timeline_key}:{cwd}", state, transport: transport.clone(), thread_id: active_thread.id.to_string(), project_id: active_thread.project_id.to_string(), cwd, project_cwd, worktree } } }
                        }
                        project_actions::RunControl { key: "run:{timeline_key}", state, thread_id: active_thread.id.to_string() }
                        diff_panel::Toggle { state, environment_key: environment_key.clone(), thread_id: active_thread.id.to_string() }
                        thread_pull_requests_panel::Toggle { state, thread_id: active_thread.id.to_string() }
                        thread_relations::HeaderButton { state, transport: transport.clone(), thread_id: active_thread.id.to_string() }
                        header_more::Group {
                        {
                            let supported = state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.thread_title_regeneration == Some(true));
                            let regenerating = matches!(active_thread.title_regeneration, Some(Some(_)));
                            let id = active_thread.id.to_string();
                            let transport = transport.clone();
                            rsx! {
                                if supported {
                                    button { class: "icon-button", "aria-label": "Regenerate title", "aria-busy": "{regenerating}", title: "Write a new title for this thread from its messages", disabled: !can_operate || regenerating,
                                        onclick: move |_| { runtime::command(&transport, state, "thread.metadata.update", json!({"threadId":id,"regenerateTitle":true})); },
                                        if regenerating { "Regenerating\u{2026}" } else { "Regenerate title" } }
                                }
                            }
                        }
                            thread_lifecycle::HeaderButtons { state, transport: transport.clone(), thread_id: active_thread.id.to_string() }
                        }
                        pull_request_links::HeaderLinks { state, transport: transport.clone(), thread: active_thread.clone() }
                      }
                    }
                }
                // A failed git action of the open thread's workspace hangs from here, inside this column.
                if let Some(active_thread) = active.as_ref().filter(|_| view == View::Chat) {
                    {
                        let project_cwd = projects.iter().find(|project| project.id == active_thread.project_id).map(|project| project.workspace_root.to_string()).unwrap_or_default();
                        let cwd = active_thread.worktree_path.as_ref().map(ToString::to_string).unwrap_or(project_cwd);
                        rsx! { if !cwd.is_empty() { git_control::FailureCard { key: "git-failure:{timeline_key}:{cwd}", state, cwd } } }
                    }
                }
                if let Some(error) = error {
                    div { class: "error-banner", role: "alert", error_text::ErrorText { text: error } button { "aria-label": "Dismiss error", onclick: move |_| state.error().set(None), "×" } }
                }
                match view {
                    View::Connections => rsx! {
                        section { class: "settings-page", settings_common::Tabs { state, current: View::Connections } h1 { "Connections" } p { "Connect to a T3 Code server to work on your projects from this device." }
                            form { onsubmit: { let transport=transport.clone(); move |event| { event.prevent_default(); let transport=transport.clone(); let address=address(); let credential=token();let pairing=credential_kind()=="pairing";token.set(String::new());spawn(async move { if pairing {runtime::pair_and_connect(transport,state,address,credential).await;}else{runtime::connect(transport,state,address,credential).await;} }); } },
                                label { r#for: "server-address", "Server address" }
                                input { id: "server-address", r#type: "url", required: true, placeholder: "https://your-server", value: "{address}", oninput: move |event| address.set(event.value()) }
                                label {r#for:"credential-kind","Authentication"}
                                select {id:"credential-kind",value:"{credential_kind}",onchange:move |event|{credential_kind.set(event.value());token.set(String::new());},option {value:"session",selected:credential_kind()=="session","Paired browser session"}option {value:"pairing",selected:credential_kind()=="pairing","Pairing credential"}option {value:"bearer",selected:credential_kind()=="bearer","Access token"}}
                                if credential_kind()!="session" {
                                    label { r#for: "access-token",if credential_kind()=="pairing"{"Pairing credential"}else{"Access token"} }
                                    input { id: "access-token", r#type: "password",required:true, autocomplete: "off", placeholder:if credential_kind()=="pairing"{"Credential generated by this server"}else{"Bearer access token"}, value: "{token}", oninput: move |event| token.set(event.value()) }
                                }
                                button { class: "primary", r#type: "submit", "Connect" }
                            }
                            p { class: "muted", "{status_label}" }
                            environment_icon::Picker { state, transport: transport.clone() }
                            connect_page::Section { state, transport: transport.clone() }
                            access_page::Authorized { state, transport: transport.clone() }
                            ssh_environments::Section { state, transport: transport.clone() }
                            if !saved_connections.is_empty() { h2 { "Saved connections" }
                                for (id,label,saved_address) in saved_connections {
                                    article {class:"connection-row",key:"{id}",
                                        div {strong {"{label}"} p {class:"muted","{saved_address}"}}
                                        button {onclick:{let saved_address=saved_address.clone();move |_|{address.set(saved_address.clone());token.set(String::new());credential_kind.set(runtime::default_credential_kind());}},"Use address"}
                                        button {onclick:{let transport=transport.clone();move |_|{runtime::forget_environment(&transport,state,&id);token.set(String::new());}},"Forget"}
                                    }
                                }
                            }
                        }
                    },
                    View::Providers => rsx! {provider_settings::Providers {state,transport:transport.clone()}},
                    View::Keybindings => rsx! {keybinding_page::Page {state,transport:transport.clone()}},
                    View::ScheduledTasks => rsx! {scheduled_tasks_page::Page {state,transport:transport.clone()}},
                    View::SourceControl => rsx! {source_control_page::Page {state,transport:transport.clone()}},
                    View::General => rsx! {general_page::Page {state,transport:transport.clone()}},
                    View::Projects => rsx! {projects_page::Page {state,transport:transport.clone()}},
                    View::TextGeneration => rsx! {text_generation_page::Page {state,transport:transport.clone()}},
                    View::Diagnostics => rsx! {diagnostics_page::Page {state,transport:transport.clone()}},
                    View::Storage => rsx! {storage_page::Page {state,transport:transport.clone()}},
                    View::Archived => rsx! {thread_lifecycle::ArchivedPage {state,transport:transport.clone()}},
                    View::PullRequests => rsx! {
                        pull_request_list::Page {state,transport:transport.clone()}
                        pull_request_panel::Host {key:"pr:list",state,transport:transport.clone()}
                    },
                    View::Usage => rsx! {usage_page::Page {state,transport:transport.clone()}},
                    View::Appearance => rsx! {
                        section { class: "settings-page", settings_common::Tabs { state, current: View::Appearance }
                            h1 { "Appearance" }
                            themes::ThemeControls {}
                            section { class: "sc-section", "aria-label": "Layout", h2 { "Layout" } settings_rows::ClientRows { state, rows: &settings_rows::LAYOUT } if stage_art::offered(state) { settings_rows::ClientRows { state, rows: &settings_rows::ENVIRONMENT_IDENTIFICATION } } }
                            // The font service is the app's (`use_fonts`); a harness that mounts the page without it has no fonts to set.
                            if try_consume_context::<font_service::Fonts>().is_some() { appearance::Typography {state} }
                        }
                    },
                    View::Chat => rsx! {
                        {
                            // Beside a thread the panel works in its checkout; with none open, in the project the new-thread form is on.
                            let selected = state.selected_project().read().clone();
                            let panel_cwd = match active.as_ref() {
                                Some(thread) => thread.worktree_path.as_ref().map(ToString::to_string).or_else(|| projects.iter().find(|project| project.id == thread.project_id).map(|project| project.workspace_root.to_string())).unwrap_or_default(),
                                None => projects.iter().find(|project| Some(project.id.as_str()) == selected.as_deref()).map(|project| project.workspace_root.to_string()).unwrap_or_default(),
                            };
                            rsx! { right_panel::Shell { key: "panel:{timeline_key}", state, transport: transport.clone(), environment_key: environment_key.clone(), thread_id: active_id.clone(), cwd: panel_cwd } }
                        }
                        if active.is_none() {
                            section { class: "empty-state",
                                h1 { "What should we work on?" }
                                if projects.is_empty() { p { "Add a project to start your first thread." } button { class: "primary", onclick: move |_| add_project.set(true), "＋ Add project" } if scratch_available { button { disabled: !can_operate, onclick: {let transport=transport.clone();move |_| project_create::start_scratch(&transport,state)}, "Start without a project" } } }
                                else {
                                    p { "Start a new thread in your project." }
                                    new_thread::NewThreadControls {state,transport:transport.clone(),projects:projects.clone(),can_operate}
                                    if scratch_available { button { disabled: !can_operate, onclick: {let transport=transport.clone();move |_| project_create::start_scratch(&transport,state)}, "Start without a project" } }
                                }
                            }
                        } else {
                            // A failure inside the open conversation shows a card in its place; the sidebar and
                            // the header stay usable. A new thread (a new key) starts a fresh boundary.
                            ErrorBoundary { key: "boundary:{timeline_key}", handle_error: crash::thread_failed,
                            for current in [active.as_ref().unwrap().clone()] {
                                ThreadTimeline { key:"{timeline_key}",state, transport: transport.clone(),environment_key:environment_key.clone(), thread_id: current.id.to_string(), can_operate }
                            }
                            for current in [active.as_ref().unwrap().clone()] {
                                worktree_setup::RetryBar {key:"retry:{timeline_key}",state,transport:transport.clone(),thread_id:current.id.to_string()}
                            }
                            for current in [active.as_ref().unwrap().clone()] {
                                rollback::Host {key:"rewind:{timeline_key}",state,transport:transport.clone(),environment_key:environment_key.clone(),thread_id:current.id.to_string()}
                            }
                            for current in [active.as_ref().unwrap().clone()] {
                                if is_subagent_thread(&current) { ReadOnlyThread { key:"{timeline_key}", state, transport: transport.clone(), parent: current.lineage.parent_thread_id.as_ref().map(ToString::to_string) } } else { Composer { key:"{timeline_key}",state, transport: transport.clone(), thread: current, can_operate } }
                            }
                            for current in [active.as_ref().unwrap().clone()] {
                                terminal_pane::Drawer {key:"terminal:{timeline_key}",state,transport:transport.clone(),environment:environment_key.clone(),thread_id:current.id.to_string(),cwd:projects.iter().find(|project|project.id==current.project_id).map(|p|p.workspace_root.to_string()).unwrap_or_default(),worktree:current.worktree_path.as_ref().map(ToString::to_string)}
                            }
                            }
                        }
                    },
                }
            }
            {desktop_overlays(state)}
            ssh_environments::Host { state, transport: transport.clone() }
            context_menu::ContextMenuHost {}
            if add_project() {
                div { class: "modal-backdrop", div { class: "dialog", role: "dialog", "aria-modal": "true", "aria-labelledby": "add-project-heading",
                    h2 { id: "add-project-heading", "Add project" }
                    form { onsubmit: { let transport=transport.clone(); move |event| {
                        event.prevent_default();
                        if runtime::request(&transport,state,"projects.mutate",json!({"type":"project.create","commandId":uuid::Uuid::new_v4().to_string(),"projectId":uuid::Uuid::new_v4().to_string(),"title":project_title(),"workspaceRoot":project_path()}),RequestKind::Unary).is_some() { add_project.set(false); }
                    } },
                        label { r#for: "project-title", "Name" } input { id: "project-title", required: true, value: "{project_title}", oninput: move |event|project_title.set(event.value()) }
                        label { r#for: "project-path", "Project directory on the server" } input { id: "project-path", required: true, placeholder: "/path/to/project", value: "{project_path}", oninput: move |event|project_path.set(event.value()) } {browse_button(&transport, project_title, project_path)}
                        div { class: "dialog-actions", button { r#type: "button", onclick: move |_|add_project.set(false), "Cancel" } button { class: "primary", r#type: "submit", disabled: !can_operate, "Add project" } }
                    }
                } }
            }
            // Inside `.app`, which is where the theme tokens (`--card`, `--border`, `--foreground`) are
            // defined: outside it a dialog or notice has no surface of its own.
            if routing::enabled() { routing::Router { state, transport: transport.clone() } }
            theme_library::ThemeEditorHost {}
            command_palette::Host { state, transport: transport.clone() }
            pull_request_thread::Host { state, transport: transport.clone() }
            notice::Host { state, transport: transport.clone() }
            thread_selection::CustomSnoozeHost { state, transport: transport.clone() }
            notifications::Coordinator { state, transport: transport.clone() }
            clone_view::Notices { state, transport: transport.clone() }
            import_projects::Host { state, transport: transport.clone() }
            thread_lifecycle::Host { state, transport: transport.clone() }
            draft_sessions::UndoNotice { state }
            welcome::Gate { state }
            welcome::Recovery { state }
            welcome::Host { state, transport: transport.clone() }
            thread_relations::Host { state, transport: transport.clone() }
            thread_pull_requests_panel::LinkDialog { state, transport: transport.clone() }
        }
    }
}

#[component]
fn ThreadTimeline(
    state: Store<UiModel>,
    transport: runtime::TransportHandle,
    environment_key: String,
    thread_id: String,
    can_operate: bool,
) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<RenderProbe>() {
        probe.timeline.set(probe.timeline.get() + 1);
    }
    let owner =
        serde_json::to_string(&(&environment_key, &thread_id)).expect("scoped timeline identity");
    let scrolling = timeline_scroll::use_scroll(state, owner);
    let mount_scroll = scrolling.clone();
    let latest_scroll = scrolling.clone();
    rollback::provide_busy(state, (environment_key.clone(), thread_id.clone()));
    // Files named in the conversation resolve against this thread's workspace.
    let file_preview = use_signal(|| None::<String>);
    use_context_provider(|| workspace_files::Host { state, transport: transport.clone(), environment_key: environment_key.clone(), thread_id: thread_id.clone(), preview: file_preview });
    let thread = state.thread();
    let supports_rewind = use_memo({
        let thread_id = thread_id.clone();
        move || {
            let provider = state.shell().read().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)).map(|thread| thread.provider_instance_id.clone());
            let field = state.typed_config();
            let config = field.read();
            let provider = provider.and_then(|id| config.as_ref().and_then(|config| config.providers.0.iter().find(|candidate| candidate.instance_id == id)));
            t3_client::rollback::provider_supports_rollback(provider)
        }
    });
    let mut history_loading = use_signal(|| None::<String>);
    let mut history_error = use_signal(|| None::<(String, String)>);
    let domain = thread.read();
    let projection = domain.projection.as_ref();
    let active_run = projection.and_then(|projection| projection["runs"].as_array()).and_then(|runs| runs.iter().rev().find(|run| matches!(run["status"].as_str(), Some("preparing" | "starting" | "running" | "waiting")))).cloned();
    let active_run_id = active_run.as_ref().and_then(|run| run["id"].as_str()).map(str::to_owned);
    let working_since = active_run.as_ref().and_then(|run| run["startedAt"].as_str().or_else(|| run["requestedAt"].as_str())).map(str::to_owned);
    let offers = if can_operate && supports_rewind() { projection.map(t3_client::rollback::targets).unwrap_or_default() } else { Default::default() };
    let disclosures = work_rows::Disclosures::new();
    // The conversation's rows: tool activity grouped, settled turns folded.
    let empty = Vec::new();
    let items = projection.and_then(|projection| projection["visibleTurnItems"].as_array()).unwrap_or(&empty);
    let entries = t3_client::timeline_rows::entries(items, projection.and_then(|projection| projection["attempts"].as_array()).unwrap_or(&empty), projection.and_then(|projection| projection["nodes"].as_array()).unwrap_or(&empty));
    let latest_run = projection.and_then(|projection| projection["runs"].as_array()).and_then(|runs| runs.last()).map(|run| t3_client::timeline_rows::LatestRun {
        id: run["id"].as_str().unwrap_or_default().to_owned(),
        status: run["status"].as_str().unwrap_or_default().to_owned(),
        started_at: run["startedAt"].as_str().map(str::to_owned),
        completed_at: run["completedAt"].as_str().map(str::to_owned),
    });
    // A provider's own subagent thread has no run: its work is the root turn's.
    let runless = projection.and_then(work_rows::runless_work_started_at);
    let is_working = active_run_id.is_some() || runless.is_some();
    // An interrupt in this session leaves its turn open, so the reader keeps their place; the next
    // turn folds it again.
    let seen = use_hook(|| std::rc::Rc::new(std::cell::RefCell::new(None::<(String, String)>)));
    let latest_key = latest_run.as_ref().map(|run| (run.id.clone(), run.status.clone()));
    use_effect(use_reactive(&latest_key, move |latest| {
        let previous = seen.replace(latest.clone());
        if let (Some((before, before_status)), Some((now, now_status))) = (previous, latest) {
            let mut runs = disclosures.runs;
            if before == now && before_status == "running" && now_status == "interrupted" {
                runs.with_mut(|set| set.insert(now));
            } else if before != now {
                runs.with_mut(|set| set.remove(&before));
            }
        }
    }));
    let mut derived = t3_client::timeline_rows::derive(&t3_client::timeline_rows::Input {
        entries: &entries,
        latest_run: latest_run.as_ref(),
        running_run_id: active_run_id.as_deref(),
        expanded_runs: &disclosures.runs.read(),
        expanded_attempts: &disclosures.attempts.read(),
        expanded_groups: &disclosures.groups.read(),
        is_working,
        runless_work_active: runless.is_some(),
        active_turn_started_at: working_since.as_deref().or(runless.as_deref()),
    });
    t3_client::timeline_rows::place_left_out(&mut derived, &entries, items);
    let mut rendered = work_rows::Rows { state, transport: transport.clone(), environment_key: &environment_key, thread_id: &thread_id, projection: projection.unwrap_or(&Value::Null), items, entries: &entries, offers: &offers, active_run_id: active_run_id.as_deref(), working_since: working_since.as_deref(), disclosures, edit_target, markers: Default::default() }.render(&derived);
    // The worktree setup sits under the message that started the thread, or at the end before any.
    let setup_at = derived.iter().position(|row| matches!(row, t3_client::timeline_rows::Row::Entry { entry } if matches!(entries[*entry].kind, t3_client::timeline_rows::Kind::Message { role_user: true, .. }))).map_or(rendered.len(), |first| first + 1).min(rendered.len());
    rendered.insert(setup_at, rsx! { worktree_setup::Card {key:"setup:{thread_id}",state,transport:transport.clone(),thread_id:thread_id.clone(),turn_live:latest_run.as_ref().is_some_and(|run| run.started_at.is_some())} });
    rsx! {
        div { class: "timeline", id:scrolling.id.clone(), tabindex:"0", "aria-label": "Conversation", onmounted:move |_|mount_scroll.mount(),
            workspace_files::PreviewDialog {}
            if projection.is_some() {
                if domain.has_more_history || history_error.read().as_ref().is_some_and(|(id,_)|id==&thread_id) {
                    button { "aria-label":"Load earlier messages",class:"load-earlier",disabled:history_loading.read().as_ref()==Some(&thread_id),onclick:{let transport=transport.clone();let thread_id=thread_id.clone();move |_|{
                        if history_loading.peek().as_ref()==Some(&thread_id){return;}
                        history_loading.set(Some(thread_id.clone()));history_error.set(None);
                        let transport=transport.clone();let thread_id=thread_id.clone();spawn(async move{
                            let result=runtime::load_earlier(transport,state).await;
                            if history_loading.peek().as_ref()==Some(&thread_id){history_loading.set(None);if let Err(error)=result{history_error.set(Some((thread_id,error)));}}
                        });
                    }},if history_loading.read().as_ref()==Some(&thread_id){"Loading earlier messages…"}else{"Load earlier messages"} }
                    if let Some((id,error))=&*history_error.read(){if id==&thread_id {p {class:"history-error",role:"alert","{error}"}}}
                }
                {rendered.into_iter()}
                thread_status_line::StatusLine { state, transport: transport.clone(), thread_id: thread_id.clone(), can_operate }
            } else { p { class: "muted", "Loading thread…" } }
            div {"data-timeline-spacer":"true","aria-hidden":"true"}
        }
        if let Some(detail)=&*scrolling.error.read() {p {class:"scroll-error",role:"alert",title:detail.clone(),"Automatic conversation scrolling is unavailable. Reload to retry."}}
        if *scrolling.show_latest.read() {button {class:"scroll-to-end","aria-label":"Scroll to end",onclick:move |_|latest_scroll.latest(),"↓ Latest"}}
    }
}

/// A child thread a provider's subagent runs in: reached from its row in the parent, not listed
/// in the sidebar, and read-only.
fn is_subagent_thread(thread: &t3_contracts::ThreadShell) -> bool {
    thread.lineage.relationship_to_parent == Some(t3_contracts::ThreadRelationship::Subagent)
}

/// The checkpoint "Edit from here" rewinds to for a row: a message that started a turn of this
/// thread, whose turn has a checkpoint to go back to.
fn edit_target(offers: &std::collections::BTreeMap<String, t3_client::rollback::Target>, thread_id: &str, row: &Value) -> Option<t3_client::rollback::Target> {
    let item = &row["item"];
    if item["type"] != "user_message" || row["sourceThreadId"] != thread_id || !matches!(item["inputIntent"].as_str(), Some("turn_start" | "queued_turn")) {
        return None;
    }
    offers.get(item["runId"].as_str()?).cloned()
}

#[component]
pub(crate) fn UserInputCard(
    request: t3_client::requests::PendingUserInput,
    state: Store<UiModel>,
    transport: runtime::TransportHandle,
    thread_id: String,
    can_operate: bool,
) -> Element {
    let answers =
        use_signal(std::collections::BTreeMap::<String, t3_client::requests::DraftAnswer>::new);
    let ready = t3_client::requests::build_answers(&request.questions, &answers.read()).is_some();
    let can_respond = can_operate && request.response_capability != "not_resumable";
    let questions: Vec<_> = request
        .questions
        .iter()
        .map(|question| {
            (
                question["id"].as_str().unwrap_or("").to_owned(),
                question.clone(),
            )
        })
        .collect();
    rsx! {
        form { class:"composer-panel approval-card user-input", onsubmit:{let transport=transport.clone();let thread_id=thread_id.clone();let id=request.id.clone();let questions=request.questions.clone();move |event|{event.prevent_default();if let Some(answers)=t3_client::requests::build_answers(&questions,&answers.peek()){runtime::command(&transport,state,"runtime-request.respond",json!({"threadId":thread_id,"requestId":id,"answers":answers}));}}},
            h3 { "Your input is needed" }
            for (id,question) in questions { UserQuestion { key:"{id}", question,answers,state,disabled:!can_respond } }
            div { class:"user-input-actions",
                button { "aria-label":"Submit answers",class:"primary",r#type:"submit",disabled:!can_respond || !ready,"Submit answers" }
                if request.dismissible { button { r#type:"button",disabled:!can_operate,onclick:{let transport=transport.clone();let thread_id=thread_id.clone();let id=request.id.clone();move |_|{runtime::command(&transport,state,"thread.user-input.dismiss",json!({"threadId":thread_id,"requestId":id}));}},"Dismiss" } }
            }
        }
    }
}

#[component]
fn UserQuestion(
    question: Value,
    mut answers: Signal<std::collections::BTreeMap<String, t3_client::requests::DraftAnswer>>,
    state: Store<UiModel>,
    disabled: bool,
) -> Element {
    let id = question["id"].as_str().unwrap_or("").to_owned();
    let draft = answers.read().get(&id).cloned().unwrap_or_default();
    let options: Vec<_> = question["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|option| {
            Some((
                option["value"]
                    .as_str()
                    .or_else(|| option["label"].as_str())?
                    .to_owned(),
                option["label"].as_str()?.to_owned(),
                option["description"].as_str().unwrap_or("").to_owned(),
            ))
        })
        .collect();
    rsx! {
        fieldset { disabled,
            legend { {question["header"].as_str().unwrap_or("Question")} }
            p { {question["question"].as_str().unwrap_or("")} }
            for (value,label,description) in options {
                {
                    // The whole option is the control: a click on its text or description selects it
                    // as a click on the circle does. The label asks for the choice itself rather than
                    // leaving it to the browser's forwarding, and cancels that forwarding so the
                    // input cannot also report a change of its own.
                    let pick={let question=question.clone();let id=id.clone();let value=value.clone();move || {
                        let displaced={let mut drafts=answers.write();t3_client::requests::toggle_option(&question,drafts.entry(id.clone()).or_default(),value.clone())};
                        if !displaced.is_empty(){let current=state.draft().peek().clone();draft_storage::edit_thread_prompt(state,if current.trim().is_empty(){displaced}else{format!("{}\n\n{}",current.trim_end(),displaced)});}
                    }};
                    let mut pick_change=pick.clone();let mut pick_click=pick;
                    rsx! {
                        label { key:"{value}",class:"question-option",onclick:move |event| { event.prevent_default(); if !disabled { pick_click(); } },
                            input { "aria-label":"{label}",r#type:if question["multiSelect"]==true {"checkbox"} else {"radio"},name:"{id}",checked:draft.selected.contains(&value),onchange:move |_| pick_change() }
                            span { class:"question-option-text","{label}" small { "{description}" } }
                        }
                    }
                }
            }
            if question["allowCustomAnswer"]!=false { textarea { "aria-label":"Custom answer",placeholder:"Your answer",value:"{draft.custom}",oninput:{let id=id.clone();move |event|t3_client::requests::set_custom_answer(answers.write().entry(id.clone()).or_default(),event.value())} } }
        }
    }
}

/// What the shell says of a thread's usage-limit recovery: the reset it reports, when it is
/// snoozed until, the recovery already scheduled, and whether the shell itself says a limit
/// stopped the latest run.
fn recovery_props(thread: &t3_contracts::ThreadShell) -> limits_view::ShellRecovery {
    let iso = |at: &t3_contracts::UtcDateTime| at.0.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    limits_view::ShellRecovery {
        limited_run: (t3_client::sidebar::status(thread) == t3_client::sidebar::Status::Limited).then(|| thread.latest_run_id.as_ref().map(ToString::to_string)).flatten(),
        reset_at: thread.usage_limit_reset_at.clone().flatten().map(|at| at.to_string()),
        stopped_at: thread.latest_run_completed_at.flatten().map(|at| iso(&at)).unwrap_or_else(|| iso(&thread.updated_at)),
        snoozed_until: thread.snoozed_until.flatten().map(|at| iso(&at)),
        recovery: thread.limit_recovery.clone().flatten().map(|recovery| limits_view::RecoveryState { run_id: recovery.run_id.to_string(), reset_at: recovery.reset_at.to_string(), auto_resume: recovery.auto_resume, snooze: recovery.snooze.flatten().unwrap_or(false) }),
    }
}

#[component]
fn Composer(
    state: Store<UiModel>,
    transport: runtime::TransportHandle,
    thread: t3_contracts::ThreadShell,
    can_operate: bool,
) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<RenderProbe>() {
        probe.composer.set(probe.composer.get() + 1);
    }
    let draft = state.draft().read().clone();
    // A rewind in flight owns the composer: its message is about to return to it.
    let rewinding = use_memo({
        let thread_id = thread.id.to_string();
        move || {
            let destination = state.destination().read().as_ref().map(ToString::to_string);
            state.rewind().read().active.as_ref().is_some_and(|active| Some(&active.owner.0) == destination.as_ref() && active.owner.1 == thread_id)
        }
    });
    let pending = state.pending_messages().read().values().any(|pending| {
        pending.thread_id == thread.id.as_str()
            && state.destination().peek().as_ref() == Some(&pending.destination)
    });
    let destination = state.destination().read().clone();
    let target = destination.as_ref().map(|destination| {
        t3_client::draft_storage::DraftTarget::thread(destination.to_string(), thread.id.to_string())
    });
    let unsupported = target
        .as_ref()
        .is_some_and(|target| draft_storage::has_unrendered(state, target));
    let (attachment_count, attachment_block) = target
        .as_ref()
        .and_then(|target| {
            state.draft_attachments().read().get(target).map(|list| {
                (list.len(), t3_client::attachments::block_reason_for(list, &attachments::limits(state)))
            })
        })
        .unwrap_or_default();
    let mut drop_files = target
        .clone()
        .map(|target| attachments::on_drop(&transport, state, target));
    let thread_key = thread.id.to_string();
    let running = thread.status.is_active();
    let mut editing = use_context::<keys::QueueEdit>().0;
    let mut restore = use_signal(|| None::<String>);
    // The prompt that was set aside while a queued message was edited returns once the edit is
    // saved (the box was emptied by the acknowledgement) and the send has finished.
    use_effect(move || {
        let sending = state.pending_messages().read().values().any(|pending| pending.thread_id == thread_key);
        if !sending {
            let previous = restore.peek().clone();
            if let Some(previous) = previous {
                restore.set(None);
                if state.draft().peek().is_empty() {
                    draft_storage::edit_thread_prompt(state, previous);
                }
            }
        }
    });
    let mode_name = serde_json::to_value(thread.interaction_mode).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default();
    let follow_up = use_memo(use_reactive((&mode_name, &(attachment_count > 0)), move |(mode, has_attachments)| {
        let field = state.thread();
        let model = field.read();
        let projection = model.projection.as_ref()?;
        let run = composer_panels::latest_run_settled(projection)?;
        let plan = words::latest_proposed_plan(projection, Some(&run));
        let inputs = t3_client::requests::pending_requests(projection).user_inputs.len();
        words::show_plan_follow_up(inputs, &mode, true, plan.as_ref(), has_attachments).then_some(plan).flatten()
    }));
    // "Resume thread" continues the run a stop interrupted, or a usage limit failed (the shell and the
    // run's own failure agree). A narrow memo: streaming text into the conversation repaints nothing here.
    let shell_limited = thread.last_error_class.clone().flatten() == Some(t3_contracts::ProviderFailureClass::UsageLimit);
    let resumable = use_memo(use_reactive((&thread.id.to_string(), &shell_limited), move |(thread_id, shell_limited)| {
        let field = state.thread();
        let model = field.read();
        let projection = model.projection.as_ref()?;
        if projection["thread"]["id"] != thread_id.as_str() {
            return None;
        }
        let runs = projection["runs"].as_array()?;
        // A held queue has its own "Resume thread" above the composer, which delivers what is queued.
        if runs.iter().any(|run| run["status"] == "queued" && run["queueHeld"] == true) {
            return None;
        }
        let items: Vec<Value> = match projection["turnItems"].as_array() {
            Some(items) => items.clone(),
            None => projection["visibleTurnItems"].as_array().into_iter().flatten().map(|row| row["item"].clone()).collect(),
        };
        t3_client::usage_limits::resumable_run(runs, &items, shell_limited)
    }));
    let queue_by_default = state.client_settings().read().follow_up_behavior == t3_contracts::FollowUpBehavior::Queue;
    let chip_count = target.as_ref().map_or(0, |target| composer_chips::count(state, target));
    let has_content = !draft.trim().is_empty() || attachment_count > 0 || chip_count > 0;
    let following_up = follow_up.read().clone();
    let resume_run = resumable.read().clone().filter(|_| !has_content && editing.read().is_none() && following_up.is_none() && !running);
    let submit_label = if following_up.is_some() { if has_content { "Refine" } else { "Implement" } } else if resume_run.is_some() { "Resume thread" } else if editing.read().is_some() { "Update queued message" } else if running { if queue_by_default { "Queue message" } else { "Steer message" } } else { "Submit message" };
    let submit = {
        let transport = transport.clone();
        let following_up = following_up.clone();
        let resume_run = resume_run.clone();
        let mode_thread = thread.id.to_string();
        let thread_provider = thread.provider_instance_id.to_string();
        move |alternate: bool| {
            let current = editing.peek().clone();
            if let Some(run_id) = resume_run.clone() {
                runtime::send_with(&transport, state, runtime::SendOptions { text: Some(t3_client::usage_limits::CONTINUE_TEXT.to_owned()), manual_continuation: Some(run_id), ..Default::default() });
            } else if let Some(current) = current {
                restore.set(Some(current.previous_draft.clone()));
                editing.set(None);
                runtime::send_with(&transport, state, runtime::SendOptions { edit_run: Some(current.run_id), ..Default::default() });
            } else if let Some(plan) = following_up.clone() {
                let options = if state.draft().peek().trim().is_empty() && attachment_count == 0 {
                    runtime::SendOptions { text: Some(words::plan_implementation_prompt(&plan.markdown)), interaction_mode: Some("default"), source_plan: Some(json!({"threadId":plan.thread_id,"planId":plan.id})), ..Default::default() }
                } else {
                    runtime::SendOptions { interaction_mode: Some("plan"), ..Default::default() }
                };
                runtime::send_with(&transport, state, options);
            } else if t3_client::usage_limits::is_usage_limits_command(&state.draft().peek()) && limits_view::offers_command(state, thread_provider.as_str()) {
                // `/usage-limits` on its own shows the report; it is not a message.
                draft_storage::edit_thread_prompt(state, String::new());
                limits_view::open_panel(state, &mode_thread, &thread_provider);
            } else if let Some(mode) = composer_menu::standalone_mode(&state.draft().peek()) {
                // `/plan` and `/default` on their own switch the mode; they are not a message.
                draft_storage::edit_thread_prompt(state, String::new());
                runtime::command(&transport, state, "thread.interaction-mode.set", json!({"threadId":mode_thread,"interactionMode":mode}));
            } else {
                runtime::send_with(&transport, state, runtime::SendOptions { mode: Some(words::dispatch_mode(running, alternate, queue_by_default)), ..Default::default() });
            }
        }
    };
    let blocked = !can_operate || unsupported || pending || rewinding() || attachment_block.is_some() || (!has_content && following_up.is_none() && resume_run.is_none());
    // Enter in the box does what the Send button does, and nothing when that is not possible.
    let enter_send = use_callback({ let mut submit = submit.clone(); move |()| { if !blocked { submit(false); } } });
    let show_stop = running && !has_content && editing.read().is_none() && following_up.is_none();
    let send_art = stage_art::artwork(state);
    let persist_context_strip = state.client_settings().read().persist_composer_context_strip;
    // "Collapse composer on scroll": a conversation scrolled up rests this composer into one line until it is used again.
    let resting = {
        let owner = composer_rest::owner(destination.as_ref().map(|id| id.as_str()).unwrap_or_default(), thread.id.as_str());
        let attachment_problem = target.as_ref().is_some_and(|target| state.attachment_error().read().as_ref().is_some_and(|(owner, _)| owner == target));
        let needs_room = unsupported || pending || attachment_block.is_some() || attachment_problem || state.prompt_length_error().read().is_some();
        composer_rest::use_resting(state, owner, draft.contains('\n'), needs_room)
    };
    let edit_hint = try_consume_context::<keys::Keys>().and_then(|keys| keys.label("thread.editQueuedMessage"));
    let steer_hint = try_consume_context::<keys::Keys>().and_then(|keys| keys.label("thread.steerQueuedMessage"));
    rsx! {
        limits_view::RecoveryHost { state, transport: transport.clone(), thread_id: thread.id.to_string(), shell: recovery_props(&thread) }
        limits_view::UsageLimitsPanel { state, thread_id: thread.id.to_string(), instance_id: thread.provider_instance_id.to_string() }
        composer_banners::Stack { server_update::Banners { state, transport: transport.clone() } }
        composer_panels::PendingRequests {state,transport:transport.clone(),thread_id:thread.id.to_string(),can_operate}
        composer_panels::QueuedRuns {state,transport:transport.clone(),thread_id:thread.id.to_string(),can_operate,editing,edit_hint,steer_hint:steer_hint.clone()}
        composer_panels::GoalStrip {state,transport:transport.clone(),thread_id:thread.id.to_string(),can_operate}
        composer_panels::BackgroundStrip {state,transport:transport.clone(),thread_id:thread.id.to_string(),can_operate}
        if let Some(plan)=following_up.clone() { composer_panels::PlanReady {plan} }
        form { class: if resting { "composer resting" } else { "composer" }, "aria-label": "Message composer", "data-resting": "{resting}",
            // Focusing, clicking or typing in the composer lifts the resting layout.
            onfocusin: move |_| composer_rest::lift(state), onmousedown: move |_| composer_rest::lift(state),
            onsubmit: { let mut submit=submit.clone(); move |event| { event.prevent_default(); submit(false); } },
            ondragover: move |event| event.prevent_default(),
            ondrop: move |event| { if let Some(handler)=drop_files.as_mut() { handler(event); } },
            prompt_stash::Dock {state,transport:transport.clone()}
            if let Some(target)=target.clone() { composer_chips::Tray {state,target} }
            composer_menu::PromptBox { state, transport: transport.clone(), thread: thread.clone(), draft: draft.clone(), attach_paste: can_operate, on_send: enter_send, placeholder: if following_up.is_some() {"Add feedback to refine the plan, or leave this blank to implement it".to_owned()} else {"Ask anything, or describe what to build…".to_owned()} }
            if let Some(target)=target.clone() { attachments::Shelf {state,transport:transport.clone(),target,locked:pending,can_operate,paste_target:".composer textarea"} }
            if unsupported {p {class:"error-banner",role:"alert","This draft includes saved context. It is preserved; use the original app to send this complete draft."}}
            composer_panels::PromptLength { state, text: None }
            div { class: "composer-toolbar",
                if can_operate { if let Some(target)=target.clone() { attachments::AttachButton {state,transport:transport.clone(),target} } }
                crate::thread_controls::ThreadControls {state,transport:transport.clone(),thread:thread.clone()}
                composer_panels::ContextMeter {state}
                if show_stop {
                    button { r#type: "button", "aria-label":"Stop generation", disabled: !can_operate, onclick: { let transport=transport.clone(); let id=thread.id.to_string(); move |_| { runtime::stop_thread(&transport,state,&id); } }, "■ Stop" }
                } else {
                    if let Some(plan)=following_up.clone().filter(|_| !has_content && editing.read().is_none()) { plan_thread::ImplementActions {state,transport:transport.clone(),thread_id:thread.id.to_string(),plan,disabled:blocked} }
                    button { class: if send_art.is_some() { "primary stage" } else { "primary" }, r#type: "button", "aria-label":"{submit_label}", title:if rewinding() {"Rewinding the conversation\u{2026}".to_owned()} else if let Some(reason)=attachment_block.clone() {reason} else if running && editing.read().is_none() && following_up.is_none() {format!("Click to {}, Ctrl/\u{2318}-click to {}",if queue_by_default{"queue"}else{"steer"},if queue_by_default{"steer"}else{"queue"})} else {submit_label.to_owned()}, disabled: blocked,
                        onclick:{let mut submit=submit.clone();move |event: MouseEvent| {let modifiers=event.modifiers();if !blocked {submit(modifiers.ctrl()||modifiers.meta());}}},
                        if let Some(variant) = send_art { stage_art::ButtonArt { key: "{variant:?}", variant } }
                        if pending { "Sending…" } else if following_up.is_some() || resume_run.is_some() { "{submit_label}" } else if editing.read().is_some() { "✓ Save" } else { "Send ↑" } }
                }
            }
        }
        if persist_context_strip { branch::ComposerStrip { state, transport: transport.clone(), thread: thread.clone() } }
    }
}

/// Where the message box would be for a subagent's thread: its provider runs it, so messages go to the parent.
#[component]
fn ReadOnlyThread(state: Store<UiModel>, transport: runtime::TransportHandle, parent: Option<String>) -> Element {
    rsx! {section {class:"composer-panel read-only-thread",role:"status",
        span {class:"muted","This subagent is run by its provider and cannot take messages. Message the parent thread instead."}
        if let Some(parent)=parent {button {"aria-label":"Open parent thread",onclick:move |_|runtime::select_thread(&transport,state,parent.clone()),"Open parent thread"}}
    }}
}

#[component]
fn Wordmark() -> Element {
    rsx! { svg { view_box: "15.5309 37 94.3941 56.96", width: "25", height: "16", "aria-label": "T3",
        path { d: "M33.4509 93V47.56H15.5309V37H64.3309V47.56H46.4109V93H33.4509ZM86.7253 93.96C82.832 93.96 78.9653 93.4533 75.1253 92.44C71.2853 91.3733 68.032 89.88 65.3653 87.96L70.4053 78.04C72.5386 79.5867 75.0186 80.8133 77.8453 81.72C80.672 82.6267 83.5253 83.08 86.4053 83.08C89.6586 83.08 92.2186 82.44 94.0853 81.16C95.952 79.88 96.8853 78.12 96.8853 75.88C96.8853 73.7467 96.0586 72.0667 94.4053 70.84C92.752 69.6133 90.0853 69 86.4053 69H80.4853V60.44L96.0853 42.76L97.5253 47.4H68.1653V37H107.365V45.4L91.8453 63.08L85.2853 59.32H89.0453C95.9253 59.32 101.125 60.8667 104.645 63.96C108.165 67.0533 109.925 71.0267 109.925 75.88C109.925 79.0267 109.099 81.9867 107.445 84.76C105.792 87.48 103.259 89.6933 99.8453 91.4C96.432 93.1067 92.0586 93.96 86.7253 93.96Z", fill: "currentColor" }
    } }
}

#[component]
fn Message(role: String, text: String, children: Element) -> Element {
    rsx! { article { class: if role=="user" { "message user" } else { "message assistant" }, markdown::Markdown {text} {children} } }
}

/// Raw HTML and unsafe link/image schemes cannot cross into the webview. Rich
/// media and trusted provider directives get their own adapters in later batches.
fn safe_markdown(text: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let events = Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH).map(
        |event| match event {
            Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) if !safe_url(&dest_url) => Event::Start(Tag::Link {
                link_type,
                dest_url: "#".into(),
                title,
                id,
            }),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) if !safe_url(&dest_url) => Event::Start(Tag::Image {
                link_type,
                dest_url: "".into(),
                title,
                id,
            }),
            Event::End(TagEnd::Image) => Event::End(TagEnd::Image),
            event => event,
        },
    );
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    html
}
fn safe_url(url: &str) -> bool {
    let url = url.trim().to_ascii_lowercase();
    !url.contains(':')
        || url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("mailto:")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_markdown_cannot_execute_html_or_script_links() {
        let html = safe_markdown(
            "<script>alert(1)</script>\n\n[bad](javascript:alert%281%29)\n\n**Safe**",
        );
        assert!(!html.contains("<script>"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(html.contains("<strong>Safe</strong>"));
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
struct RenderProbe {
    application: std::rc::Rc<std::cell::Cell<usize>>,
    composer: std::rc::Rc<std::cell::Cell<usize>>,
    timeline: std::rc::Rc<std::cell::Cell<usize>>,
    /// How many times a conversation row was built.
    item: std::rc::Rc<std::cell::Cell<usize>>,
    diff: std::rc::Rc<std::cell::Cell<usize>>,
    /// How many times a group's summary line (or the live line) was built.
    summary: std::rc::Rc<std::cell::Cell<usize>>,
    /// How many times a sidebar thread row was built.
    sidebar_row: std::rc::Rc<std::cell::Cell<usize>>,
    /// How many times the header's lineage button and a response's fork button were built.
    lineage_button: std::rc::Rc<std::cell::Cell<usize>>,
    fork_button: std::rc::Rc<std::cell::Cell<usize>>,
    /// How many times the status line (snoozed, settled, woke) was built.
    status_line: std::rc::Rc<std::cell::Cell<usize>>,
}

#[cfg(test)]
mod reactive_tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    #[derive(Clone)]
    struct Harness {
        state: Rc<RefCell<Option<Store<UiModel>>>>,
        probe: RenderProbe,
    }
    pub(super) fn large_model() -> UiModel {
        let mut model = UiModel::default();
        let thread: t3_contracts::ThreadShell=serde_json::from_value(json!({
            "createdBy":"user","creationSource":"web","id":"thread","projectId":"p0","title":"Work",
            "providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"model"},
            "runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null,
            "lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"thread"},
            "forkedFrom":null,"activeProviderThreadId":null,"latestRunId":null,"activeRunId":null,"status":"idle",
            "pendingRuntimeRequest":null,"latestVisibleMessage":null,"latestUserMessageAt":null,
            "hasActionableProposedPlan":false,"itemCount":2000,"visibleItemCount":2000,
            "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","archivedAt":null,
            "settledOverride":null,"settledAt":null,"deletedAt":null
        })).unwrap();
        let projects=(0..1000).map(|index|serde_json::from_value(json!({"id":format!("p{index}"),"title":format!("Project {index}"),"workspaceRoot":format!("/projects/{index}"),"defaultModelSelection":null,"scripts":[],"createdAt":"now","updatedAt":"now"})).unwrap()).collect();
        model.shell.snapshot = Some(t3_contracts::ShellSnapshot {
            schema_version: 2,
            snapshot_sequence: 1,
            projects,
            threads: vec![thread],
            archived_threads: vec![],
        });
        model.active_thread = Some("thread".into());
        let rows:Vec<_>=(0..2000).map(|index|json!({"sourceThreadId":"thread","sourceItemId":format!("item{index}"),"position":index,"item":{"id":format!("item{index}"),"type":"assistant_message","text":"A long conversation that should never be cloned or rendered again merely because the user types a character."}})).collect();
        model.thread.projection =
            Some(json!({"thread":{"id":"thread"},"visibleTurnItems":rows,"runtimeRequests":[]}));
        model
    }
    fn harness(props: Harness) -> Element {
        use_context_provider(|| props.probe.clone());
        let state = use_store(large_model);
        *props.state.borrow_mut() = Some(state);
        let transport = use_hook(runtime::TransportHandle::default);
        rsx! { Application { state, transport } }
    }
    #[test]
    fn typing_in_large_conversation_updates_composer_without_rebuilding_shell_or_history() {
        let props = Harness {
            state: Rc::new(RefCell::new(None)),
            probe: RenderProbe::default(),
        };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let root_renders = props.probe.application.get();
        let timeline_renders = props.probe.timeline.get();
        let composer_renders = props.probe.composer.get();
        assert_eq!(timeline_renders, 1);
        let state = props.state.borrow().unwrap();
        for text in ["h", "he", "hel", "hello"] {
            state.draft().set(text.to_owned());
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
        assert_eq!(props.probe.application.get(), root_renders);
        assert_eq!(props.probe.timeline.get(), timeline_renders);
        assert_eq!(props.probe.composer.get(), composer_renders + 4);
        assert_eq!(
            state.thread().peek().projection.as_ref().unwrap()["visibleTurnItems"]
                .as_array()
                .unwrap()
                .len(),
            2000
        );
    }

    /// The Usage page keeps everything it shows in its own field of the model: a scan starting,
    /// answering or failing, a limit refresh and a reset credit in flight repaint nothing else.
    #[test]
    fn usage_updates_repaint_neither_the_shell_nor_the_conversation_nor_the_composer() {
        let props = Harness { state: Rc::new(RefCell::new(None)), probe: RenderProbe::default() };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        let counts = || (props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get());
        let before = counts();
        state.usage().write().scanning = true;
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        state.usage().write().error = Some("The scan failed.".into());
        state.usage().write().limits_refreshing = true;
        state.redeem().write().busy.insert("credit".into());
        state.redeem().write().status.insert("credit".into(), "Reset applied.".into());
        state.usage().write().scanning = false;
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert_eq!(counts(), before);
    }

    /// Adding and removing a context chip rebuilds the composer that shows it, and neither the
    /// application, the sidebar nor the open conversation.
    #[test]
    fn adding_and_removing_a_context_chip_rebuilds_only_the_composer() {
        let props = Harness { state: Rc::new(RefCell::new(None)), probe: RenderProbe::default() };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        state.destination().set(Some(serde_json::from_value(json!("environment-a")).unwrap()));
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        let (root, timeline, composer) = (props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get());
        let target = t3_client::draft_storage::DraftTarget::thread("environment-a", "thread");
        let record = t3_client::composer_context::thread_record("environment-a", "other", "Other thread");
        composer_chips::add(state, target.clone(), record.clone());
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert_eq!((props.probe.application.get(), props.probe.timeline.get()), (root, timeline));
        assert!(props.probe.composer.get() > composer, "the composer shows the chip");
        let with_chip = props.probe.composer.get();
        composer_chips::remove(state, target, record["contextId"].as_str().unwrap());
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert_eq!((props.probe.application.get(), props.probe.timeline.get()), (root, timeline));
        assert!(props.probe.composer.get() > with_chip);
    }

    /// A clone reporting progress repaints its own notice and its own project's line, and
    /// nothing else of a thousand-project sidebar or the open conversation.
    #[test]
    fn clone_progress_repaints_neither_the_sidebar_nor_the_conversation_nor_the_composer() {
        let props = Harness { state: Rc::new(RefCell::new(None)), probe: RenderProbe::default() };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        let counts = || (props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get());
        let snapshot = |percent: f64| t3_client::project_clone::Snapshot {
            project_id: "p7".into(), remote_url: "git://example/repo.git".into(), destination_path: "/projects/7".into(), repository: None,
            phase: "running".into(), stage: "receiving".into(), percent: Some(percent), detail: None, error: None, sequence: percent,
        };
        state.clones().write().live = true;
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        let before = counts();
        for percent in [1.0, 20.0, 55.0, 99.0] {
            state.clones().write().list = vec![snapshot(percent)];
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
        assert_eq!(counts(), before);
    }

    #[test]
    fn adding_and_removing_attachments_rebuilds_only_the_composer() {
        use t3_client::attachments::{Admitted, Attachment, Kind, Upload};
        let props = Harness {
            state: Rc::new(RefCell::new(None)),
            probe: RenderProbe::default(),
        };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        state
            .destination()
            .set(Some(serde_json::from_value(json!("environment-a")).unwrap()));
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        let (root, timeline, composer) = (
            props.probe.application.get(),
            props.probe.timeline.get(),
            props.probe.composer.get(),
        );
        let target = t3_client::draft_storage::DraftTarget::thread("environment-a", "thread");
        let mut attachment = Attachment::new(
            "local".into(),
            &Admitted {
                index: 0,
                kind: Kind::File,
                name: "notes.pdf".into(),
                mime_type: "application/pdf".into(),
            },
            9,
        );
        attachment.upload = Upload::Ready;
        state
            .draft_attachments()
            .write()
            .entry(target.clone())
            .or_default()
            .push(attachment);
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(crate::runtime::transport_tests::rendered_text(&dom).contains("notes.pdf"));
        state.draft_attachments().write().get_mut(&target).unwrap().clear();
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert_eq!(props.probe.application.get(), root);
        assert_eq!(props.probe.timeline.get(), timeline);
        assert_eq!(props.probe.composer.get(), composer + 2);
    }

    /// A long conversation whose thread has two completed turns, with the diff panel mounted.
    fn diff_model() -> UiModel {
        let mut model = large_model();
        model.destination = Some(serde_json::from_value(json!("environment-a")).unwrap());
        let projection = model.thread.projection.as_mut().unwrap();
        projection["runs"] = json!([{"id":"run-1","status":"completed"},{"id":"run-2","status":"completed"}]);
        projection["checkpoints"] = json!([
            {"runId":"run-1","appRunOrdinal":1,"status":"ready","files":[],"capturedAt":"2026-01-01T00:00:00Z"},
            {"runId":"run-2","appRunOrdinal":2,"status":"ready","files":[{"path":"a.rs","kind":"modified","additions":1,"deletions":0}],"capturedAt":"2026-01-02T00:00:00Z"},
        ]);
        model
    }
    fn diff_harness(props: Harness) -> Element {
        use_context_provider(|| props.probe.clone());
        let state = use_store(diff_model);
        *props.state.borrow_mut() = Some(state);
        let transport = use_hook(runtime::TransportHandle::default);
        rsx! { Application { state, transport } }
    }
    #[test]
    fn opening_using_and_closing_the_diff_never_rebuilds_the_conversation_or_sidebar() {
        use crate::runtime::transport_tests::{click_control, control};
        let props = Harness {
            state: Rc::new(RefCell::new(None)),
            probe: RenderProbe::default(),
        };
        let mut dom = VirtualDom::new_with_props(diff_harness, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        let (root, timeline, composer) = (props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get());
        assert_eq!(props.probe.diff.get(), 0, "a closed panel is not mounted");

        // This model has no connection, so the button is disabled; the panel is opened through its handler
        // directly, since what is measured is what mounting and using the panel repaints.
        crate::runtime::transport_tests::force_click_control(&mut dom, "Diff");
        assert!(control(&dom, "Diff scope").is_some());
        assert_eq!(props.probe.diff.get(), 1);
        // Using the panel's controls re-renders the panel alone.
        for label in ["Hide whitespace changes", "Show whitespace changes", "Disable diff line wrapping", "Show file tree"] {
            if control(&dom, label).is_some() {
                click_control(&mut dom, label);
            }
        }
        let panel = props.probe.diff.get();
        assert!(panel > 1);
        assert_eq!((props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get()), (root, timeline, composer), "the panel's own state repaints only the panel");

        // Streaming into the thread repaints the conversation, not the diff.
        for text in ["Hel", "Hello", "Hello wor", "Hello world"] {
            state.thread().write().projection.as_mut().unwrap()["visibleTurnItems"][0]["item"]["text"] = json!(text);
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
        assert_eq!(props.probe.diff.get(), panel, "a streaming turn must not re-render an open diff");
        // A new completed turn changes what the panel is about, so it may repaint once.
        let before_turn = props.probe.diff.get();
        state.thread().write().projection.as_mut().unwrap()["checkpoints"].as_array_mut().unwrap().push(json!({"runId":"run-3","appRunOrdinal":3,"status":"ready","files":[],"capturedAt":"2026-01-03T00:00:00Z"}));
        state.thread().write().projection.as_mut().unwrap()["runs"].as_array_mut().unwrap().push(json!({"id":"run-3","status":"completed"}));
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(props.probe.diff.get() <= before_turn + 1);

        click_control(&mut dom, "Close diff");
        assert!(control(&dom, "Diff scope").is_none());
        // Application and the sidebar inside it were never rebuilt by the panel opening, working or closing.
        assert_eq!(props.probe.application.get(), root);
        assert_eq!(props.probe.composer.get(), composer, "the composer is untouched by the panel");
    }

    #[test]
    fn the_rewind_dialog_over_a_large_conversation_never_rebuilds_it_or_the_sidebar() {
        let props = Harness {
            state: Rc::new(RefCell::new(None)),
            probe: RenderProbe::default(),
        };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        state.destination().set(Some(serde_json::from_value(json!("environment-a")).unwrap()));
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        let (root, timeline, composer) = (props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get());
        let confirm = || rollback::Confirm {
            owner: ("environment-a".into(), "thread".into()),
            run_id: "run".into(),
            text: "first".into(),
            attachments: vec![],
            target: t3_client::rollback::Target { checkpoint_id: "c".into(), scope_id: "s".into() },
        };
        for _ in 0..3 {
            state.rewind().write().confirm = Some(confirm());
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
            assert!(crate::runtime::transport_tests::rendered_text(&dom).contains("Edit from here?"));
            state.rewind().write().confirm = None;
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
        // A refused rewind and a rewind in flight are the dialog's own state too.
        state.rewind().write().error = Some((("environment-a".into(), "thread".into()), "File restore requires an isolated worktree.".into()));
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(crate::runtime::transport_tests::rendered_text(&dom).contains("File restore requires"));
        assert_eq!((props.probe.application.get(), props.probe.timeline.get()), (root, timeline));
        // Only the composer's send button follows a rewind in flight, once each way.
        state.rewind().write().active = Some(rollback::Active { owner: ("environment-a".into(), "thread".into()), command_id: "c".into(), run_id: "run".into(), restore_files: false, accepted: false, text: String::new(), files: Default::default() });
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(crate::runtime::transport_tests::rendered_text(&dom).contains("Rewinding the conversation"));
        state.rewind().write().active = None;
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert_eq!((props.probe.application.get(), props.probe.timeline.get()), (root, timeline));
        assert_eq!(props.probe.composer.get(), composer + 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_rewind_the_server_never_finishes_ends_with_a_timeout_instead_of_waiting_forever() {
        crate::rollback::TIMEOUT_OVERRIDE.with(|timeout| timeout.set(Some(20)));
        let props = Harness {
            state: Rc::new(RefCell::new(None)),
            probe: RenderProbe::default(),
        };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        state.destination().set(Some(serde_json::from_value(json!("environment-a")).unwrap()));
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        let owner = ("environment-a".to_owned(), "thread".to_owned());
        state.rewind().write().active = Some(rollback::Active { owner: owner.clone(), command_id: "c".into(), run_id: "run".into(), restore_files: true, accepted: true, text: "first".into(), files: Default::default() });
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(crate::runtime::transport_tests::rendered_text(&dom).contains("Rewinding the conversation and restoring files"));
        // The wait ends on its own, with the reason, and the thread can be used again.
        tokio::time::timeout(std::time::Duration::from_secs(45), async {
            while !crate::runtime::transport_tests::rendered_text(&dom).contains("Timed out waiting for the thread to rewind.") {
                dom.wait_for_work().await;
                dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
            }
        })
        .await
        .expect("the rewind never gave up");
        assert!(state.peek().rewind.active.is_none());
        assert!(!crate::runtime::transport_tests::rendered_text(&dom).contains("Rewinding the conversation"));
        // Nothing came back to the composer: the rewind did not finish.
        assert_eq!(state.peek().draft, "");
        crate::rollback::TIMEOUT_OVERRIDE.with(|timeout| timeout.set(None));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opening_typing_in_and_closing_the_palette_never_rebuilds_the_conversation_the_sidebar_or_the_composer() {
        use crate::runtime::keys_tests::{PALETTE, chord, key_on, poll_until, press};
        use crate::runtime::transport_tests::{control, input_control};
        let props = Harness { state: Rc::new(RefCell::new(None)), probe: RenderProbe::default() };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let (root, timeline, composer) = (props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get());
        assert_eq!(timeline, 1);
        // The shortcut opens it; typing, moving and running a filter repaint the palette alone.
        assert_eq!(press(&mut dom, chord("k", false, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("commandPalette.toggle"));
        poll_until(&mut dom, "the palette", |dom| control(dom, PALETTE).is_some()).await;
        for text in [">", ">o", ">op", "settings", "zzz", ""] {
            input_control(&mut dom, PALETTE, text);
        }
        key_on(&mut dom, PALETTE, Key::ArrowDown);
        key_on(&mut dom, PALETTE, Key::ArrowUp);
        key_on(&mut dom, PALETTE, Key::Escape);
        assert!(control(&dom, PALETTE).is_none());
        // Opening and closing again, by the shortcut and by the header button.
        press(&mut dom, chord("k", false, false), t3_client::keybindings::Dom::default()).await;
        poll_until(&mut dom, "the palette", |dom| control(dom, PALETTE).is_some()).await;
        press(&mut dom, chord("k", false, false), t3_client::keybindings::Dom::default()).await;
        poll_until(&mut dom, "closed", |dom| control(dom, PALETTE).is_none()).await;
        crate::runtime::transport_tests::click_control(&mut dom, "Command palette");
        assert!(control(&dom, PALETTE).is_some());
        crate::runtime::transport_tests::click_control(&mut dom, "Command palette");
        assert!(control(&dom, PALETTE).is_none());
        assert_eq!((props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get()), (root, timeline, composer), "the palette is its own island");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_key_press_repaints_only_what_its_command_changes_over_a_two_thousand_item_conversation() {
        use crate::runtime::keys_tests::{chord, poll_until, press, typed};
        use t3_client::keybindings::{Dom, KeyEvent};
        let props = Harness { state: Rc::new(RefCell::new(None)), probe: RenderProbe::default() };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        let (root, timeline, composer) = (props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get());
        // Keys nothing names, keys typed in the message box, keys while composing, and keys the
        // app has no feature for are all left alone, and repaint nothing.
        let composing = KeyEvent { composing: true, ..chord("k", false, false) };
        for (event, focus) in [(chord("q", true, true), Dom::default()), (typed("h"), Dom::default()), (typed("h"), Dom { editable_focus: true, composer_focus: true, ..Default::default() }), (composing, Dom::default()), (chord("d", false, false), Dom::default())] {
            assert_eq!(press(&mut dom, event, focus).await, None);
        }
        assert_eq!((props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get()), (root, timeline, composer));
        // A command that changes the shell repaints the shell, and still not the conversation.
        assert!(!*state.sidebar_open().peek());
        assert_eq!(press(&mut dom, chord("b", false, false), Dom::default()).await.as_deref(), Some("sidebar.toggle"));
        poll_until(&mut dom, "the sidebar", |_| *state.sidebar_open().peek()).await;
        assert_eq!((props.probe.timeline.get(), props.probe.composer.get()), (timeline, composer));
    }

    #[derive(Clone, Default)]
    struct IsolationHarness {
        state: Rc<RefCell<Option<Store<UiModel>>>>,
        detail: crate::timeline::DetailProbe,
    }
    fn isolation_harness(props: IsolationHarness) -> Element {
        use_context_provider(|| props.detail.clone());
        let state = use_store(|| {
            let mut model = large_model();
            model.destination = Some(serde_json::from_value(json!("environment-a")).unwrap());
            model.thread.projection.as_mut().unwrap()["visibleTurnItems"] = json!([{"sourceThreadId":"thread","sourceItemId":"same-item","item":{"id":"same-item","type":"command_execution","status":"completed","updatedAt":"same-revision","input":"pwd","outputOmitted":true}}]);
            model
        });
        *props.state.borrow_mut() = Some(state);
        let transport = use_hook(runtime::TransportHandle::default);
        rsx! {Application {state,transport}}
    }
    #[test]
    fn identical_thread_item_and_revision_on_another_environment_get_fresh_detail_state() {
        let props = IsolationHarness::default();
        let mut dom = VirtualDom::new_with_props(isolation_harness, props.clone());
        dom.rebuild_in_place();
        assert_eq!(props.detail.0.borrow().len(), 1);
        let mut detail = props.detail.0.borrow()[0].1;
        detail.set(Some((serde_json::to_string(&("environment-a","thread","same-item","same-revision")).unwrap(),json!({"id":"same-item","type":"command_execution","output":"private environment-a output"}))));
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        props.state.borrow().unwrap().destination().set(Some(
            serde_json::from_value(json!("environment-b")).unwrap(),
        ));
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        let registrations = props.detail.0.borrow();
        assert_eq!(registrations.len(), 2);
        assert_eq!(registrations[1].0, "environment-b");
        assert!(registrations[1].1.peek().is_none());
    }
}

#[cfg(all(test, unix))]
#[path = "item_tests.rs"]
mod item_tests;
