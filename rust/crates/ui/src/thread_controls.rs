use crate::runtime::{self, NewThreadChoices, UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use t3_client::{models, new_thread};
use t3_contracts::*;

fn model_key(selection: &ModelSelection) -> String {
    serde_json::to_string(&(&selection.instance_id, &selection.model)).expect("model identity")
}

pub fn update_choices(
    state: Store<UiModel>,
    key: &(EnvironmentId, String),
    update: impl FnOnce(&mut NewThreadChoices),
) {
    {
        let mut choices = state.thread_choices();
        update(choices.write().entry(key.clone()).or_default());
    }
    let choices = state
        .thread_choices()
        .peek()
        .get(key)
        .cloned()
        .unwrap_or_default();
    crate::draft_storage::save_thread_choices(state, key, &choices);
}
fn select_model(
    state: Store<UiModel>,
    key: &(EnvironmentId, String),
    thread: &ThreadShell,
    selection: ModelSelection,
) {
    if state.destination().peek().as_ref() != Some(&key.0) {
        return;
    }
    if let Some(error) = runtime::model_change_error(state, thread, &selection) {
        state.error().set(Some(error));
        return;
    }
    update_choices(state, key, |choices| {
        choices.model_selection = Some(selection.clone())
    });
    state.thread_model_options().write().insert(
        (
            key.0.clone(),
            selection.instance_id.clone(),
            selection.model.to_string(),
        ),
        selection.options.clone().unwrap_or_default(),
    );
    state
        .sticky_models()
        .write()
        .insert(key.0.clone(), selection);
}

#[component]
pub fn ThreadControls(state: Store<UiModel>, transport: runtime::TransportHandle, thread: ThreadShell) -> Element {
    // Text/tool streaming does not change model controls. Subscribe only to
    // the narrow reported selection, without copying the thread projection.
    let thread_id = thread.id.to_string();
    let reported = use_memo(move || {
        let field = state.thread();
        let projection = field.read();
        projection
            .projection
            .as_ref()
            .filter(|projection| projection["thread"]["id"] == thread_id)
            .and_then(t3_client::started_thread::reported_model_selection)
    });
    let destination = state.destination().read().clone();
    let Some(destination) = destination else {
        return rsx! {};
    };
    let key = (destination, thread.id.to_string());
    let choices = state
        .thread_choices()
        .read()
        .get(&key)
        .cloned()
        .unwrap_or_default();
    let selected = choices
        .model_selection
        .as_ref()
        .unwrap_or(&thread.model_selection)
        .clone();
    let config_signal = state.typed_config();
    let config = config_signal.read();
    let Some(config) = config.as_ref() else {
        return rsx! {span {class:"muted","{selected.instance_id} · {selected.model}"}};
    };
    let client_signal = state.client_settings();
    let client = client_signal.read();
    let rows =
        new_thread::available_models(config, &client, runtime::ui_surface(), Some(&selected));
    let selected_key = model_key(&selected);
    let selected_label = rows.iter().find(|row| model_key(&row.selection()) == selected_key).map(|row| format!("{} · {}", row.provider_label, row.model.name)).unwrap_or_else(|| format!("{} · {}", selected.instance_id, selected.model));
    let _missing = !rows
        .iter()
        .any(|row| model_key(&row.selection()) == selected_key);
    let provider = config
        .providers
        .0
        .iter()
        .find(|p| p.instance_id == selected.instance_id);
    let supported = provider
        .and_then(|p| p.supported_runtime_modes.as_ref().and_then(Option::as_ref))
        .map(|m| m.0.as_slice());
    let mode = models::compatible_runtime_mode(
        choices.runtime_mode.unwrap_or(thread.runtime_mode),
        supported,
    );
    let modes = models::runtime_modes(supported);
    let runtime_value = serde_json::to_value(mode)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let caps = provider
        .map(|p| {
            models::model_capabilities(
                p.driver.as_str(),
                &p.models,
                selected.model.as_str(),
                client.plan_mode_enabled,
            )
        })
        .unwrap_or(ModelCapabilities {
            option_descriptors: None,
        });
    let descriptors = models::option_descriptors(&caps, selected.options.as_deref());
    rsx! {
        div {class:"thread-controls",
            crate::model_picker::ModelPicker {state,rows:rows.clone(),selected_key:selected_key.clone(),selected_label:selected_label.clone(),selected_instance:selected.instance_id.to_string(),disabled:false,on_pick:{let key=key.clone();let thread=thread.clone();let rows=rows.clone();move|index:usize| {
                if let Some(row)=rows.get(index) {
                    let mut selection=row.selection();
                    let remembered=state.thread_model_options().peek().get(&(key.0.clone(),selection.instance_id.clone(),selection.model.to_string())).cloned().or_else(|| {
                        let handle=state.peek().draft_storage.clone();
                        let document=handle.document.borrow();
                        document.source_state.as_deref().and_then(|source|source["stickyOptionsByModelByProvider"][selection.instance_id.as_str()].get(selection.model.as_str())).and_then(|options|serde_json::from_value::<Vec<ProviderOptionSelection>>(options.clone()).ok())
                    });
                    selection.options=remembered.filter(|options|!options.is_empty());
                    select_model(state,&key,&thread,selection);
                }
            }}}
            crate::model_controls::ModelOptions {selected:Some(selected),descriptors,reported:reported(),onchange:{let key=key.clone();let thread=thread.clone();move|selection|select_model(state,&key,&thread,selection)}}
            if client.plan_mode_enabled {
                {let planning=thread.interaction_mode==ProviderInteractionMode::Plan;
                let tooltip=if planning{"Plan mode — click to return to normal build mode"}else{"Default mode — click to enter plan mode"};
                let id=thread.id.to_string();let transport=transport.clone();
                rsx!{button {r#type:"button",class:"mode-toggle","aria-pressed":"{planning}","aria-label":"{tooltip}",title:"{tooltip}",onclick:move |_|{runtime::command(&transport,state,"thread.interaction-mode.set",serde_json::json!({"threadId":id,"interactionMode":if planning{"default"}else{"plan"}}));},if planning{"Plan"}else{"Build"}}}}
            }
            label {"Permissions",select {"aria-label":"Permissions",value:"{runtime_value}",onchange:move|e| {
                if let Ok(mode)=serde_json::from_value(serde_json::json!(e.value())) {update_choices(state,&key,|choices|choices.runtime_mode=Some(mode));}
            },for choice in modes {option {value:serde_json::to_value(choice).unwrap().as_str().unwrap(),selected:choice==mode,"{models::runtime_mode_label(choice)}"}}}}
        }
    }
}
