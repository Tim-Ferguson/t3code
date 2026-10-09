//! The model picker: a button showing the model in use that opens a searchable list, grouped by
//! provider instance with a favourites tab (`ModelPickerContent.tsx`). Typing searches every
//! provider and ranks; Up and Down move, Enter picks, Tab and Shift+Tab change provider, Escape
//! closes. The shortcuts (`modelPicker.toggle`, `.jump.N`, `.previousProvider`, `.nextProvider`)
//! reach it through the shared `PickerCtl`.
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use serde_json::json;
use std::collections::BTreeSet;
use t3_client::{model_picker as logic, new_thread::InstanceModelOption};

/// What the shortcuts say to the picker.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct PickerCtl {
    pub open: Signal<bool>,
    /// Pick the Nth row of what is listed (1-based).
    pub jump: Signal<Option<usize>>,
    /// Move to the next or previous provider.
    pub step: Signal<Option<i32>>,
}

fn entries(rows: &[InstanceModelOption]) -> Vec<logic::Entry> {
    rows.iter()
        .map(|row| logic::Entry {
            instance_id: row.instance_id.to_string(),
            driver: row.driver.to_string(),
            provider_label: row.provider_label.clone(),
            slug: row.model.slug.clone(),
            name: row.model.name.clone(),
            short_name: row.model.short_name.clone(),
            sub_provider: row.model.sub_provider.clone(),
        })
        .collect()
}

#[component]
pub(crate) fn ModelPicker(state: Store<UiModel>, rows: Vec<InstanceModelOption>, selected_key: String, selected_label: String, selected_instance: String, disabled: bool, on_pick: EventHandler<usize>) -> Element {
    let local_open = use_signal(|| false);
    let (local_jump, local_step) = (use_signal(|| None::<usize>), use_signal(|| None::<i32>));
    let shared = use_hook(try_consume_context::<PickerCtl>);
    let mut open = shared.map_or(local_open, |ctl| ctl.open);
    let jump = shared.map_or(local_jump, |ctl| ctl.jump);
    let step = shared.map_or(local_step, |ctl| ctl.step);
    rsx! {
        div {class:"model-picker",
            button {r#type:"button",class:"model-picker-trigger","aria-label":"Model","aria-haspopup":"listbox","aria-expanded":"{open()}",value:"{selected_key}",disabled,
                onclick:move |_|{let now=*open.peek();open.set(!now);},
                "{selected_label}"}
            // Built when opened and gone when closed, so it opens fresh every time.
            if open() {Popup {state,rows,selected_instance,open,jump,step,on_pick}}
        }
    }
}

#[component]
fn Popup(state: Store<UiModel>, rows: Vec<InstanceModelOption>, selected_instance: String, mut open: Signal<bool>, jump: Signal<Option<usize>>, step: Signal<Option<i32>>, on_pick: EventHandler<usize>) -> Element {
    let writer = use_hook(try_consume_context::<crate::client_settings::Writer>);
    let mut query = use_signal(String::new);
    let mut highlighted = use_signal(|| 0usize);
    let favorites: BTreeSet<(String, String)> = state.client_settings().read().favorites.iter().map(|favorite| (favorite.provider.to_string(), favorite.model.to_string())).collect();
    let mut tab = use_signal(|| None::<logic::Tab>);
    let all = entries(&rows);
    let current_tab = tab().unwrap_or_else(|| logic::initial_tab(&favorites, &selected_instance));
    let listed = logic::visible(&all, &favorites, &query(), &current_tab);
    let mut providers: Vec<(String, String)> = Vec::new();
    for entry in &all {
        if !providers.iter().any(|(id, _)| id == &entry.instance_id) {
            providers.push((entry.instance_id.clone(), entry.provider_label.clone()));
        }
    }
    let mut tabs: Vec<logic::Tab> = Vec::new();
    if !favorites.is_empty() {
        tabs.push(logic::Tab::Favorites);
    }
    tabs.extend(providers.iter().map(|(id, _)| logic::Tab::Instance(id.clone())));
    let cursor = highlighted().min(listed.len().saturating_sub(1));
    // Shortcuts act on what is listed right now.
    {
        let listed = listed.clone();
        let tabs = tabs.clone();
        let current_tab = current_tab.clone();
        use_effect(move || {
            let (mut jump, mut step) = (jump, step);
            if !*open.peek() {
                return;
            }
            let wanted = *jump.read();
            if let Some(number) = wanted {
                jump.set(None);
                if let Some(index) = number.checked_sub(1).and_then(|at| listed.get(at)) {
                    on_pick.call(*index);
                    open.set(false);
                }
            }
            let moved = *step.read();
            if let Some(by) = moved {
                step.set(None);
                if let Some(at) = tabs.iter().position(|candidate| candidate == &current_tab) {
                    let next = (at as i32 + by).rem_euclid(tabs.len() as i32) as usize;
                    tab.set(Some(tabs[next].clone()));
                    highlighted.set(0);
                }
            }
        });
    }
    let label_of = |entry: &logic::Entry| format!("{} · {}", entry.provider_label, entry.name);
    let searching = !query().trim().is_empty();
    rsx! {
        div {class:"popover-backdrop",onclick:move |_|open.set(false)}
        div {class:"model-picker-popup",onkeydown:move |event| {if event.key()==Key::Escape {event.stop_propagation();open.set(false);}},
                    input {r#type:"search","aria-label":"Search models",placeholder:"Search models...",value:"{query}",autocomplete:"off",
                        onmounted:move |event| {spawn(async move {let _ = event.set_focus(true).await;});},
                        oninput:move |event| {query.set(event.value());highlighted.set(0);},
                        onkeydown:{let listed=listed.clone();let tabs=tabs.clone();let current_tab=current_tab.clone();move |event: KeyboardEvent| {
                            let shift = event.modifiers().shift();
                            match event.key() {
                                Key::ArrowDown if !listed.is_empty() => {event.prevent_default();highlighted.set((cursor+1)%listed.len());}
                                Key::ArrowUp if !listed.is_empty() => {event.prevent_default();highlighted.set((cursor+listed.len()-1)%listed.len());}
                                Key::Enter if !listed.is_empty() => {event.prevent_default();on_pick.call(listed[cursor]);open.set(false);}
                                Key::Tab if !tabs.is_empty() => {
                                    event.prevent_default();
                                    let at = tabs.iter().position(|candidate| candidate == &current_tab).unwrap_or(0) as i32;
                                    tab.set(Some(tabs[(at + if shift {-1} else {1}).rem_euclid(tabs.len() as i32) as usize].clone()));
                                    highlighted.set(0);
                                }
                                _ => {}
                            }
                        }}
                    }
                    if !searching {div {class:"model-picker-tabs",role:"tablist",
                        for candidate in tabs.iter() {{
                            let name = match candidate {logic::Tab::Favorites => "Favorites".to_owned(), logic::Tab::Instance(id) => providers.iter().find(|(known,_)|known==id).map(|(_,label)|label.clone()).unwrap_or_else(|| id.clone())};
                            let chosen = candidate == &current_tab;
                            rsx!{button {key:"{name}",r#type:"button",role:"tab","aria-selected":"{chosen}",class:if chosen {"selected"} else {""},
                                onclick:{let candidate=candidate.clone();move |_|{tab.set(Some(candidate.clone()));highlighted.set(0);}},"{name}"}}
                        }}
                    }}
                    ul {class:"model-picker-list",role:"listbox","aria-label":"Models",
                        if listed.is_empty() {li {class:"muted","No models found"}}
                        for (position,index) in listed.iter().copied().enumerate() {{
                            let entry = &all[index];
                            let favorite = favorites.contains(&entry.key());
                            let name = label_of(entry);
                            let (instance, slug) = entry.key();
                            let chosen = position == cursor;
                            rsx!{li {key:"{instance}:{slug}",role:"option","aria-selected":"{chosen}",class:if chosen {"selected"} else {""},
                                button {r#type:"button","aria-label":"{name}",class:"model-row",onmousedown:move |event| event.prevent_default(),onclick:move |_|{on_pick.call(index);open.set(false);},
                                    span {class:"model-name","{entry.name}"}
                                    if searching {span {class:"muted","{entry.provider_label}"}}
                                    if position < 9 {span {class:"muted model-jump",title:"Jump with the modelPicker.jump.{position+1} shortcut","{position+1}"}}
                                }
                                button {r#type:"button","aria-label":if favorite {"Remove from favorites"} else {"Add to favorites"},"aria-pressed":"{favorite}",class:"icon-button",
                                    onclick:{let favorites=favorites.clone();let writer=writer.clone();move |_|toggle_favorite(state,writer.as_ref(),&favorites,&instance,&slug)},
                                    if favorite {"★"} else {"☆"}}
                            }}
                        }}
                    }
        }
    }
}

fn toggle_favorite(state: Store<UiModel>, writer: Option<&crate::client_settings::Writer>, current: &BTreeSet<(String, String)>, instance: &str, model: &str) {
    let mut next = current.clone();
    let key = (instance.to_owned(), model.to_owned());
    if !next.remove(&key) {
        next.insert(key);
    }
    let list: Vec<_> = next.into_iter().map(|(provider, model)| json!({"provider":provider,"model":model})).collect();
    if let Some(writer) = writer {
        writer.patch(state, json!({"favorites":list}));
    }
}
