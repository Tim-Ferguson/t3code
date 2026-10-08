use crate::runtime::{self, UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::{models, new_thread};
use t3_contracts::*;

// The select value can be assigned before dynamic options are mounted. Mark
// the selected option too so first paint and later controlled updates agree.

#[component]
pub fn NewThreadControls(
    state: Store<UiModel>,
    transport: runtime::TransportHandle,
    projects: Vec<ProjectShell>,
    can_operate: bool,
) -> Element {
    let selected = state.selected_project().read().clone();
    let project = projects
        .iter()
        .find(|p| Some(p.id.as_str()) == selected.as_deref())
        .or_else(|| projects.first())
        .cloned();
    let Some(project) = project else {
        return rsx! {};
    };
    let destination = state
        .destination()
        .read()
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let key = serde_json::to_string(&(&destination, &project.id)).expect("identity");
    rsx! {
        div { class:"new-thread-controls",
            label { "Project"
                select { "aria-label":"Project",value:"{project.id}",onchange:move|e|state.selected_project().set(Some(e.value())),
                    for p in &projects { option {value:"{p.id}",selected:p.id==project.id,"{p.title}"} }
                }
            }
            for project in [project.clone()] {NewThreadOptions { key:"{key}",state,transport:transport.clone(),project,can_operate }}
        }
    }
}
type DraftKey = (EnvironmentId, String);
fn update_choices(
    state: Store<UiModel>,
    key: Option<&DraftKey>,
    update: impl FnOnce(&mut runtime::NewThreadChoices),
) {
    if let Some(key) = key {
        update(
            state
                .new_thread_choices()
                .write()
                .entry(key.clone())
                .or_default(),
        );
    }
}
fn remember_selection(
    state: Store<UiModel>,
    draft_key: Option<&DraftKey>,
    selection: &ModelSelection,
) {
    update_choices(state, draft_key, |choices| {
        choices.model_selection = Some(selection.clone())
    });
    let destination = state.destination().peek().clone();
    if let Some(destination) = destination {
        state
            .sticky_models()
            .write()
            .insert(destination, selection.clone());
    }
}
fn key(selection: &ModelSelection) -> String {
    serde_json::to_string(&(&selection.instance_id, &selection.model)).expect("model identity")
}
#[component]
fn NewThreadOptions(
    state: Store<UiModel>,
    transport: runtime::TransportHandle,
    project: ProjectShell,
    can_operate: bool,
) -> Element {
    // Component ownership includes both destination and project; asynchronous
    // catalog arrival resolves defaults without writing them into an explicit draft.
    let config_signal = state.typed_config();
    let config = config_signal.read();
    let Some(config) = config.as_ref() else {
        return rsx! {p {class:"muted","Loading provider catalog…"}};
    };
    let client_signal = state.client_settings();
    let client = client_signal.read();
    let defaults = new_thread::project_defaults(&config.settings, Some(&project));
    let destination = state.destination().read().clone();
    let draft_key = destination
        .as_ref()
        .map(|destination| (destination.clone(), project.id.to_string()));
    let prompt = draft_key
        .as_ref()
        .and_then(|key| state.new_thread_drafts().read().get(key).cloned())
        .unwrap_or_default();
    let choices = draft_key
        .as_ref()
        .and_then(|key| state.new_thread_choices().read().get(key).cloned())
        .unwrap_or_default();
    let base_ref = choices.base_ref.clone();
    let pending = destination.as_ref().is_some_and(|destination| {
        state.pending_launches().read().values().any(|pending| {
            &pending.destination == destination && pending.project_id == project.id.as_str()
        })
    });
    let sticky = destination
        .as_ref()
        .and_then(|id| state.sticky_models().peek().get(id).cloned());
    let selected = new_thread::resolve_new_thread_selection(
        config,
        &client,
        runtime::ui_surface(),
        choices.model_selection.as_ref(),
        &defaults,
        sticky.as_ref(),
    );
    let rows =
        new_thread::available_models(config, &client, runtime::ui_surface(), selected.as_ref());
    let selected_key = selected.as_ref().map(key).unwrap_or_default();
    let provider = selected.as_ref().and_then(|s| {
        config
            .providers
            .0
            .iter()
            .find(|p| p.instance_id == s.instance_id)
    });
    let supported = provider
        .and_then(|p| p.supported_runtime_modes.as_ref().and_then(Option::as_ref))
        .map(|s| s.0.as_slice());
    let modes = models::runtime_modes(supported);
    let mode = models::compatible_runtime_mode(
        choices.runtime_mode.unwrap_or(defaults.runtime_mode),
        supported,
    );
    let caps = match (provider, selected.as_ref()) {
        (Some(provider), Some(selected)) => models::model_capabilities(
            provider.driver.as_str(),
            &provider.models,
            selected.model.as_str(),
            client.plan_mode_enabled,
        ),
        _ => ModelCapabilities {
            option_descriptors: None,
        },
    };
    let descriptors =
        models::option_descriptors(&caps, selected.as_ref().and_then(|s| s.options.as_deref()));
    let environment = choices
        .environment_mode
        .or(defaults.environment_mode)
        .unwrap_or(ThreadEnvMode::Local);
    let environment_value = if environment == ThreadEnvMode::Worktree {
        "worktree"
    } else {
        "local"
    };
    let runtime_value = serde_json::to_value(mode)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let preserved_missing = selected.as_ref().is_some_and(|s| {
        !rows
            .iter()
            .any(|r| r.instance_id == s.instance_id && r.model.slug == s.model.as_str())
    });
    let disabled = pending
        || !can_operate
        || selected.is_none()
        || (environment == ThreadEnvMode::Worktree && base_ref.trim().is_empty())
        || selected.as_ref().is_some_and(|s| {
            rows.iter()
                .find(|r| r.instance_id == s.instance_id && r.model.slug == s.model.as_str())
                .is_some_and(|r| r.model.is_unavailable)
        });
    let launch_selection = selected.clone();
    let project_id = project.id.to_string();
    rsx! {
        label { "Model"
            select {"aria-label":"Model",value:selected_key,onchange:{let rows=rows.clone();let draft_key=draft_key.clone();move|event| {
                if let Some(row)=rows.iter().find(|r|key(&r.selection())==event.value()) {
                    let selection=row.selection();
                    remember_selection(state,draft_key.as_ref(),&selection);
                }
            }},
                if preserved_missing {if let Some(selection)=selected.as_ref() {option {value:key(selection),selected:true,"{selection.instance_id} · {selection.model}"}}}
                if rows.is_empty() && selected.is_none() {option {value:"",selected:true,"No models available"}}
                for row in &rows {option {value:key(&row.selection()),selected:key(&row.selection())==selected_key,"{row.provider_label} · {row.model.name}"}}
            }
        }
        div {class:"model-traits",
            for descriptor in descriptors {
                match descriptor {
                    ProviderOptionDescriptor::Select {fields} => {
                        let id=fields.id.clone();
                        let value=models::option_current_value(&ProviderOptionDescriptor::Select {fields:fields.clone()},selected.as_ref(),None).and_then(|v|match v {ProviderOptionSelectionValue::String(v)=>Some(v.to_string()),_=>None}).unwrap_or_default();
                        let explicit=selected.as_ref().and_then(|s|s.options.as_ref()).is_some_and(|options|options.iter().any(|option|option.id==id));
                        let picker_value=if explicit {value.clone()}else{String::new()};
                        let default_value=models::option_current_value(&ProviderOptionDescriptor::Select {fields:fields.clone()},None,None).and_then(|v|match v {ProviderOptionSelectionValue::String(v)=>Some(v),_=>None});
                        let default_label=default_value.as_ref().and_then(|value|fields.options.iter().find(|option|&option.id==value)).map(|option|format!("Provider default ({})",option.label)).unwrap_or_else(||"Provider default".into());
                        let choices=if runtime::ui_surface()==new_thread::Surface::Mobile {models::mobile_option_choices(&ProviderOptionDescriptor::Select {fields:fields.clone()})}else{fields.options.iter().filter(|o|!fields.prompt_injected_values.as_ref().and_then(Option::as_ref).is_some_and(|values|values.contains(&o.id))).cloned().collect()};
                        let selection=selected.clone();
                        let draft_key=draft_key.clone();
                        rsx!{label {"{fields.label}",select {"aria-label":"{fields.label}",value:"{picker_value}",onchange:move|e|{
                            if let Some(mut selection)=selection.clone() {
                                let options=selection.options.get_or_insert_default(); options.retain(|o|o.id!=id);
                                if let Ok(value)=TrimmedNonEmptyString::new(e.value()) {options.push(ProviderOptionSelection {id:id.clone(),value:ProviderOptionSelectionValue::String(value)});}
                                remember_selection(state,draft_key.as_ref(),&selection);
                            }
                        },
                            option {value:"",selected:picker_value.is_empty(),"{default_label}"}
                            for choice in choices {option {value:"{choice.id}",selected:choice.id.as_str()==picker_value,"{choice.label}"}}
                        }}}
                    },
                    ProviderOptionDescriptor::Boolean {fields} => {
                        let id=fields.id.clone();
                        let selection=selected.clone();
                        let draft_key=draft_key.clone();
                        let checked=selected.as_ref().and_then(|s|s.options.as_ref()).and_then(|o|o.iter().find(|o|o.id==id)).and_then(|o|match o.value {ProviderOptionSelectionValue::Boolean(v)=>Some(v),_=>None}).unwrap_or(if id.as_str()=="fastMode"{false}else{fields.current_value.flatten().unwrap_or(false)});
                        rsx!{label {class:"trait-toggle",input {r#type:"checkbox",checked,onchange:move|e|{
                            if let Some(mut selection)=selection.clone() {
                                let options=selection.options.get_or_insert_default(); options.retain(|o|o.id!=id);
                                options.push(ProviderOptionSelection {id:id.clone(),value:ProviderOptionSelectionValue::Boolean(e.checked())}); remember_selection(state,draft_key.as_ref(),&selection);
                            }
                        }},"{fields.label}"}}
                    }
                }
            }
        }
        label {"Permissions",select {"aria-label":"Permissions",value:runtime_value,onchange:{let draft_key=draft_key.clone();move|e|{if let Ok(mode)=serde_json::from_value(json!(e.value())) {update_choices(state,draft_key.as_ref(),|choices|choices.runtime_mode=Some(mode));}}},
            for choice in modes {option {value:serde_json::to_value(choice).unwrap().as_str().unwrap(),selected:choice==mode,"{models::runtime_mode_label(choice)}"}}
        }}
        label {"Workspace",select {"aria-label":"Workspace",value:environment_value,onchange:{let draft_key=draft_key.clone();move|e|update_choices(state,draft_key.as_ref(),|choices|choices.environment_mode=Some(if e.value()=="worktree"{ThreadEnvMode::Worktree}else{ThreadEnvMode::Local}))},option {value:"local",selected:environment==ThreadEnvMode::Local,"Local"} option {value:"worktree",selected:environment==ThreadEnvMode::Worktree,"New worktree"}}}
        if environment==ThreadEnvMode::Worktree {label {"Base branch",input {"aria-label":"Base branch",value:"{base_ref}",placeholder:"Branch or revision",oninput:{let draft_key=draft_key.clone();move|e|update_choices(state,draft_key.as_ref(),|choices|choices.base_ref=e.value())}}}}
        label {class:"new-thread-prompt","First message",textarea {"aria-label":"First message",placeholder:"What would you like to work on?",value:"{prompt}",oninput:move|e|{
            if let Some(key)=draft_key.clone() {state.new_thread_drafts().write().insert(key,e.value());}
        }}}
        button {class:"primary","aria-label":"Create new thread",disabled,onclick:move|_|{
            if let Some(selection)=launch_selection.as_ref() {
                let workspace=if environment==ThreadEnvMode::Worktree {
                    let Ok(base_ref)=TrimmedNonEmptyString::new(&base_ref) else{return;};
                    ThreadLaunchWorkspaceStrategy::Worktree(WorkspaceWorktree {base_ref,branch:None,start_from_origin:None})
                } else {ThreadLaunchWorkspaceStrategy::Root(WorkspaceRoot {branch:None})};
                runtime::launch_thread(&transport,state,&project_id,selection,mode,workspace,Some(&prompt));
            }
        },if pending {"Creating…"} else if prompt.trim().is_empty() {"New thread"} else {"Start chat"}}
        if rows.is_empty() {p {class:"muted","No ready providers. Check provider setup in Settings."}}
    }
}
