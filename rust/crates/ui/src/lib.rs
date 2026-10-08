mod appearance;
mod client_settings;
mod draft_storage;
mod font_service;
mod model_controls;
mod new_thread;
mod runtime;
mod scroll_state;
mod terminal_bridge;
mod terminal_pane;
mod terminal_stream;
mod themes;
mod thread_controls;
mod timeline;
mod timeline_scroll;
use dioxus::prelude::*;
use runtime::{UiModel, UiModelStoreExt, View};
use serde_json::{Value, json};
use t3_client::{connection::ConnectionStatus, rpc::RequestKind};

const STYLES: &str = include_str!("../assets/app.css");

#[component]
pub fn App() -> Element {
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
    use_future(move || {
        let transport = startup_transport.clone();
        let terminal_catalog = terminal_catalog.clone();
        async move {
            terminal_catalog.hydrate().await;
            client_settings::hydrate(state).await;
            draft_storage::hydrate(state).await;
            runtime::connect(transport, state, runtime::default_address(), String::new()).await;
        }
    });
    rsx! { Application { state, transport } }
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
    let active_id = state.active_thread().read().clone();
    let active = threads
        .iter()
        .find(|thread| Some(thread.id.as_str()) == active_id.as_deref())
        .cloned();
    let connected = *state.status().read() == ConnectionStatus::Connected;
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
    let config = state.config().read().clone();
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
            let catalog = &themes.catalog;
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
    let layout_class = if *state.sidebar_open().read() {
        "app sidebar-open"
    } else {
        "app"
    };
    let status_label = match &*state.status().read() {
        ConnectionStatus::Connected => "Connected",
        ConnectionStatus::Connecting => "Connecting…",
        ConnectionStatus::Blocked(_) => "Connection needs attention",
        ConnectionStatus::Interrupted(_) => "Disconnected",
        ConnectionStatus::Disconnected => "Disconnected",
    };
    let error = state.error().read().clone();
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
        div { class: "{layout_class}", "data-theme": theme, "data-palette":palette,"data-word-wrap":state.client_settings().read().word_wrap,
            aside { class: "sidebar", "aria-label": "Main sidebar",
                header { class: "brand", Wordmark {} span { "Code" } }
                nav { class: "sidebar-actions",
                    button { onclick: {let transport=transport.clone();move |_| runtime::new_thread(&transport,state,None)}, "＋ New thread" }
                    button { onclick: move |_| add_project.set(true), "＋ Add project" }
                }
                div { class: "project-list",
                    for project in projects.iter() {
                        div { class: "project-group", key: "{project.id}",
                            button { class: "project-heading", title: "{project.workspace_root}", onclick: { let transport=transport.clone();let id = project.id.to_string(); move |_| runtime::new_thread(&transport,state,Some(id.clone())) },
                                span { class: "project-icon", "◇" } "{project.title}"
                            }
                            for thread in threads.iter().filter(|thread| thread.project_id == project.id && thread.deleted_at.is_none()) {
                                button { key: "{thread.id}", class: if active.as_ref().is_some_and(|active| active.id == thread.id) { "thread-row selected" } else { "thread-row" },
                                    onclick: { let transport=transport.clone(); let id=thread.id.to_string(); move |_| runtime::select_thread(&transport, state, id.clone()) },
                                    span { class: if thread.status.is_active() { "status-dot busy" } else { "status-dot" } }
                                    span { class: "thread-title", "{thread.title}" }
                                    if thread.pending_runtime_request.is_some() { span { class: "attention", title: "Needs your input", "!" } }
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
                    if let Some(active_thread) = active.clone() {
                        if active_thread.branch.is_some() { span { class: "branch-label", "⑂ {active_thread.branch.as_ref().unwrap()}" } }
                        button { class: "icon-button", title: "Archive thread", disabled: !can_operate, onclick: { let transport=transport.clone(); let id=active_thread.id.to_string(); move |_| { runtime::command(&transport,state,"thread.archive",json!({"threadId":id})); } }, "Archive" }
                    }
                }
                if let Some(error) = error {
                    div { class: "error-banner", role: "alert", span { "{error}" } button { "aria-label": "Dismiss error", onclick: move |_| state.error().set(None), "×" } }
                }
                match view {
                    View::Connections => rsx! {
                        section { class: "settings-page", h1 { "Connections" } p { "Connect to a T3 Code server to work on your projects from this device." }
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
                    View::Providers => rsx! {
                        section { class: "settings-page", div { class: "settings-tabs", button { class: "selected", "Providers" } button { onclick: move |_| state.view().set(View::Appearance), "Appearance" } button { onclick: move |_| state.view().set(View::Connections), "Connections" } }
                            h1 { "Providers" } p { "Agent runtimes available on this environment." }
                            for provider in config["providers"].as_array().into_iter().flatten() {
                                article { class: "provider-row",
                                    h2 { {provider["displayName"].as_str().or_else(|| provider["driver"].as_str()).unwrap_or("Provider")} }
                                    span { class: "muted", {provider["message"].as_str().unwrap_or("")} }
                                    span { {provider["status"].as_str().unwrap_or("Unknown")} }
                                }
                            }
                            if !connected { p { "Connect to your server to see its providers." } }
                        }
                    },
                    View::Appearance => rsx! {
                        section { class: "settings-page", div { class: "settings-tabs", button { onclick: move |_| state.view().set(View::Providers), "Providers" } button { class: "selected", "Appearance" } button { onclick: move |_| state.view().set(View::Connections), "Connections" } }
                            h1 { "Appearance" }
                            themes::ThemeControls {}
                            appearance::Typography {state}
                        }
                    },
                    View::Chat => rsx! {
                        if active.is_none() {
                            section { class: "empty-state",
                                h1 { "What should we work on?" }
                                if projects.is_empty() { p { "Add a project to start your first thread." } button { class: "primary", onclick: move |_| add_project.set(true), "＋ Add project" } }
                                else {
                                    p { "Start a new thread in your project." }
                                    new_thread::NewThreadControls {state,transport:transport.clone(),projects:projects.clone(),can_operate}

                                }
                            }
                        } else {
                            for current in [active.as_ref().unwrap().clone()] {
                                ThreadTimeline { key:"{timeline_key}",state, transport: transport.clone(),environment_key:environment_key.clone(), thread_id: current.id.to_string(), can_operate }
                            }
                            for current in [active.as_ref().unwrap().clone()] {
                                Composer { key:"{timeline_key}",state, transport: transport.clone(), thread: current, can_operate }
                            }
                            for current in [active.as_ref().unwrap().clone()] {
                                terminal_pane::Drawer {key:"terminal:{timeline_key}",state,transport:transport.clone(),environment:environment_key.clone(),thread_id:current.id.to_string(),cwd:projects.iter().find(|project|project.id==current.project_id).map(|p|p.workspace_root.to_string()).unwrap_or_default(),worktree:current.worktree_path.as_ref().map(ToString::to_string)}
                            }
                        }
                    },
                }
            }
            if add_project() {
                div { class: "modal-backdrop", div { class: "dialog", role: "dialog", "aria-modal": "true", "aria-labelledby": "add-project-heading",
                    h2 { id: "add-project-heading", "Add project" }
                    form { onsubmit: { let transport=transport.clone(); move |event| {
                        event.prevent_default();
                        if runtime::request(&transport,state,"projects.mutate",json!({"type":"project.create","commandId":uuid::Uuid::new_v4().to_string(),"projectId":uuid::Uuid::new_v4().to_string(),"title":project_title(),"workspaceRoot":project_path()}),RequestKind::Unary).is_some() { add_project.set(false); }
                    } },
                        label { r#for: "project-title", "Name" } input { id: "project-title", required: true, value: "{project_title}", oninput: move |event|project_title.set(event.value()) }
                        label { r#for: "project-path", "Project directory on the server" } input { id: "project-path", required: true, placeholder: "/path/to/project", value: "{project_path}", oninput: move |event|project_path.set(event.value()) }
                        div { class: "dialog-actions", button { r#type: "button", onclick: move |_|add_project.set(false), "Cancel" } button { class: "primary", r#type: "submit", disabled: !can_operate, "Add project" } }
                    }
                } }
            }
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
    let thread = state.thread();
    let mut history_loading = use_signal(|| None::<String>);
    let mut history_error = use_signal(|| None::<(String, String)>);
    let domain = thread.read();
    let projection = domain.projection.as_ref();
    let pending = projection
        .map(t3_client::requests::pending_requests)
        .unwrap_or_default();
    let rows: Vec<_> = projection
        .and_then(|projection| projection["visibleTurnItems"].as_array())
        .into_iter()
        .flatten()
        .map(|row| {
            (
                serde_json::to_string(&(
                    &environment_key,
                    &row["sourceThreadId"],
                    &row["sourceItemId"],
                ))
                .expect("scoped item identity"),
                row,
            )
        })
        .collect();
    rsx! {
        div { class: "timeline", id:scrolling.id.clone(), tabindex:"0", "aria-label": "Conversation", onmounted:move |_|mount_scroll.mount(),
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
                for (key,row) in rows { div { key:"{key}","data-timeline-row":key.clone(),"data-message-id":row["item"]["messageId"].as_str().unwrap_or_default(),
                    timeline::TimelineItem { environment_key:environment_key.clone(),item: row["item"].clone(),state,transport:transport.clone(),source_thread_id:row["sourceThreadId"].as_str().unwrap_or(&thread_id).to_owned(),source_item_id:row["sourceItemId"].as_str().unwrap_or_default().to_owned() }
                }}
                for approval in pending.approvals.iter() { ApprovalCard { key: "{approval.id}", approval:approval.clone(), state, transport:transport.clone(),thread_id:thread_id.clone(),can_operate } }
                for request in pending.user_inputs.iter() { UserInputCard { key: "{request.id}", request:request.clone(),state,transport:transport.clone(),thread_id:thread_id.clone(),can_operate } }
            } else { p { class: "muted", "Loading thread…" } }
            div {"data-timeline-spacer":"true","aria-hidden":"true"}
        }
        if let Some(detail)=&*scrolling.error.read() {p {class:"scroll-error",role:"alert",title:detail.clone(),"Automatic conversation scrolling is unavailable. Reload to retry."}}
        if *scrolling.show_latest.read() {button {class:"scroll-to-end","aria-label":"Scroll to end",onclick:move |_|latest_scroll.latest(),"↓ Latest"}}
    }
}

#[component]
fn ApprovalCard(
    approval: t3_client::requests::PendingApproval,
    state: Store<UiModel>,
    transport: runtime::TransportHandle,
    thread_id: String,
    can_operate: bool,
) -> Element {
    let options: Vec<_> = approval
        .options
        .iter()
        .filter_map(|option| {
            Some((
                option["decision"].as_str()?.to_owned(),
                option["label"].as_str()?.to_owned(),
                option["warning"].as_str().map(str::to_owned),
            ))
        })
        .collect();
    rsx! {
        section { class:"approval-card", "aria-label":"Approval required",
            h3 { "Approval required" }
            if let Some(app)=&approval.app_name { p { "{app}" } }
            if let Some(detail)=&approval.detail { pre { "{detail}" } }
            if !approval.live { p { class:"muted", "This request can no longer be answered." } }
            for (decision,label,warning) in options {
                div { class:"approval-option",
                    button { disabled:!can_operate || !approval.live, onclick:{let transport=transport.clone();let thread_id=thread_id.clone();let id=approval.id.clone();move |_|{runtime::command(&transport,state,"runtime-request.respond",json!({"threadId":thread_id,"requestId":id,"decision":decision}));}}, "{label}" }
                    if let Some(warning)=warning { span { class:"approval-warning", "{warning}" } }
                }
            }
        }
    }
}

#[component]
fn UserInputCard(
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
        form { class:"approval-card user-input", onsubmit:{let transport=transport.clone();let thread_id=thread_id.clone();let id=request.id.clone();let questions=request.questions.clone();move |event|{event.prevent_default();if let Some(answers)=t3_client::requests::build_answers(&questions,&answers.peek()){runtime::command(&transport,state,"runtime-request.respond",json!({"threadId":thread_id,"requestId":id,"answers":answers}));}}},
            h3 { "Your input is needed" }
            for (id,question) in questions { UserQuestion { key:"{id}", question,answers,state,disabled:!can_respond } }
            button { "aria-label":"Submit answers",class:"primary",r#type:"submit",disabled:!can_respond || !ready,"Submit answers" }
            if request.dismissible { button { r#type:"button",disabled:!can_operate,onclick:{let transport=transport.clone();let thread_id=thread_id.clone();let id=request.id.clone();move |_|{runtime::command(&transport,state,"thread.user-input.dismiss",json!({"threadId":thread_id,"requestId":id}));}},"Dismiss" } }
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
                label { class:"question-option",
                    input { "aria-label":"{label}",r#type:if question["multiSelect"]==true {"checkbox"} else {"radio"},name:"{id}",checked:draft.selected.contains(&value),onchange:{let question=question.clone();let id=id.clone();move |_| {
                        let displaced={let mut drafts=answers.write();t3_client::requests::toggle_option(&question,drafts.entry(id.clone()).or_default(),value.clone())};
                        if !displaced.is_empty(){let current=state.draft().peek().clone();draft_storage::edit_thread_prompt(state,if current.trim().is_empty(){displaced}else{format!("{}\n\n{}",current.trim_end(),displaced)});}
                    }} }
                    span { "{label}" small { "{description}" } }
                }
            }
            if question["allowCustomAnswer"]!=false { textarea { "aria-label":"Custom answer",placeholder:"Your answer",value:"{draft.custom}",oninput:{let id=id.clone();move |event|t3_client::requests::set_custom_answer(answers.write().entry(id.clone()).or_default(),event.value())} } }
        }
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
    let pending = state.pending_messages().read().values().any(|pending| {
        pending.thread_id == thread.id.as_str()
            && state.destination().peek().as_ref() == Some(&pending.destination)
    });
    let unsupported = state
        .destination()
        .read()
        .as_ref()
        .is_some_and(|destination| {
            draft_storage::has_unrendered(
                state,
                &t3_client::draft_storage::DraftTarget::thread(
                    destination.to_string(),
                    thread.id.to_string(),
                ),
            )
        });
    rsx! {
        form { class: "composer", onsubmit: { let transport=transport.clone(); move |event| { event.prevent_default(); runtime::send_message(&transport,state); } },
            textarea { "aria-label": "Message", placeholder: "Ask anything, or describe what to build…", value: "{draft}", oninput: move |event| draft_storage::edit_thread_prompt(state,event.value()) }
            if unsupported {p {class:"error-banner",role:"alert","This draft includes saved attachments or context. They are preserved; use the original app to send this complete draft."}}
            div { class: "composer-toolbar",
                crate::thread_controls::ThreadControls {state,thread:thread.clone()}
                if thread.status.is_active() {
                    button { r#type: "button", disabled: !can_operate, onclick: { let transport=transport.clone(); let id=thread.id.to_string(); move |_| { runtime::stop_thread(&transport,state,&id); } }, "Stop" }
                }
                button { class: "primary", r#type: "submit", disabled: !can_operate || unsupported || pending || draft.trim().is_empty(), if pending { "Sending…" } else { "Send ↑" } }
            }
        }
    }
}

#[component]
fn Wordmark() -> Element {
    rsx! { svg { view_box: "15.5309 37 94.3941 56.96", width: "25", height: "16", "aria-label": "T3",
        path { d: "M33.4509 93V47.56H15.5309V37H64.3309V47.56H46.4109V93H33.4509ZM86.7253 93.96C82.832 93.96 78.9653 93.4533 75.1253 92.44C71.2853 91.3733 68.032 89.88 65.3653 87.96L70.4053 78.04C72.5386 79.5867 75.0186 80.8133 77.8453 81.72C80.672 82.6267 83.5253 83.08 86.4053 83.08C89.6586 83.08 92.2186 82.44 94.0853 81.16C95.952 79.88 96.8853 78.12 96.8853 75.88C96.8853 73.7467 96.0586 72.0667 94.4053 70.84C92.752 69.6133 90.0853 69 86.4053 69H80.4853V60.44L96.0853 42.76L97.5253 47.4H68.1653V37H107.365V45.4L91.8453 63.08L85.2853 59.32H89.0453C95.9253 59.32 101.125 60.8667 104.645 63.96C108.165 67.0533 109.925 71.0267 109.925 75.88C109.925 79.0267 109.099 81.9867 107.445 84.76C105.792 87.48 103.259 89.6933 99.8453 91.4C96.432 93.1067 92.0586 93.96 86.7253 93.96Z", fill: "currentColor" }
    } }
}

#[component]
fn Message(role: String, text: String) -> Element {
    let html = safe_markdown(&text);
    rsx! { article { class: if role=="user" { "message user" } else { "message assistant" }, div { class: "markdown", dangerous_inner_html: html } } }
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
