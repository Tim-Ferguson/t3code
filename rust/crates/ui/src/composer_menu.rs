//! The message box and its suggestion menus: `/` for commands, `$` for skills, `@` for files.
//! The menu follows the end of the prompt (the box does not report its caret), reads the provider's
//! own commands and skills from the configuration, and searches files through the workspace
//! search the server already has. Arrow keys move, Enter or Tab picks, Escape closes.
use crate::{draft_storage, runtime, runtime::{UiModel, UiModelStoreExt}};
use dioxus::prelude::*;
use serde_json::json;
use crate::limits_view;
use t3_client::{composer_menu::{self as menu, SlashItem, Skill, TriggerKind}, usage_limits as logic};

#[derive(Debug, Clone, PartialEq)]
enum Apply {
    /// Put this where the trigger text was.
    Insert(String),
    /// Switch the thread's interaction mode (`/plan`, `/default`).
    Mode(&'static str),
    /// Move to the model control (`/model`).
    Model,
    /// Show the provider's limits (`/usage-limits`); the draft keeps nothing of it.
    UsageLimits,
}
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    id: String,
    label: String,
    description: String,
    apply: Apply,
}

fn provider_entries(state: Store<UiModel>, thread: &t3_contracts::ThreadShell, plan_mode: bool, show_skills: bool) -> (Vec<SlashItem>, Vec<Skill>) {
    let config = state.typed_config();
    let config = config.read();
    let provider = config.as_ref().and_then(|config| config.providers.0.iter().find(|provider| provider.instance_id == thread.provider_instance_id));
    let mut items = vec![SlashItem::Command { command: "model".into(), description: "Switch response model for this thread".into() }];
    if plan_mode {
        items.push(SlashItem::Command { command: "plan".into(), description: "Switch this thread into plan mode".into() });
        items.push(SlashItem::Command { command: "default".into(), description: "Switch this thread back to normal build mode".into() });
    }
    let mut skills = Vec::new();
    if let Some(provider) = provider {
        for command in &provider.slash_commands {
            let description = command.description.as_ref().and_then(Option::as_ref).map(|text| text.to_string()).unwrap_or_default();
            items.push(SlashItem::Provider { name: command.name.to_string(), description, provider: provider.instance_id.to_string() });
        }
        if limits_view::offers_command(state, provider.instance_id.as_str()) {
            // Served by T3 from the same snapshots as Usage, Limits; a provider's own command of that name gives way.
            items.retain(|item| !matches!(item, SlashItem::Provider { name, .. } if name == logic::USAGE_LIMITS_COMMAND_NAME));
            items.push(SlashItem::Provider { name: logic::USAGE_LIMITS_COMMAND_NAME.into(), description: logic::USAGE_LIMITS_COMMAND_DESCRIPTION.into(), provider: provider.instance_id.to_string() });
        }
        if show_skills {
            skills = provider.skills.iter().filter_map(|skill| Skill::from_json(&serde_json::to_value(skill).ok()?)).collect();
        }
    }
    (items, skills)
}

fn slash_entries(items: &[SlashItem], skills: &[Skill], query: &str, at_start: bool) -> Vec<Entry> {
    let mut all: Vec<SlashItem> = items.to_vec();
    all.extend(menu::search_skills(skills, "").into_iter().map(|index| SlashItem::Skill { skill: skills[index].clone(), provider: String::new() }));
    let allowed = menu::items_for_position(&all, at_start);
    let visible: Vec<SlashItem> = allowed.iter().map(|index| all[*index].clone()).collect();
    menu::search_slash(&visible, query)
        .into_iter()
        .map(|index| match &visible[index] {
            SlashItem::Command { command, description } => Entry {
                id: format!("slash:{command}"),
                label: format!("/{command}"),
                description: description.clone(),
                apply: match command.as_str() {
                    "plan" => Apply::Mode("plan"),
                    "default" => Apply::Mode("default"),
                    _ => Apply::Model,
                },
            },
            SlashItem::Provider { name, description, .. } => Entry { id: format!("provider:{name}"), label: format!("/{name}"), description: description.clone(), apply: if name == logic::USAGE_LIMITS_COMMAND_NAME { Apply::UsageLimits } else { Apply::Insert(format!("/{name} ")) } },
            SlashItem::Skill { skill, .. } => Entry { id: format!("skill:{}", skill.name), label: format!("${}", skill.name), description: skill.short_description.clone().or_else(|| skill.description.clone()).unwrap_or_default(), apply: Apply::Insert(format!("${} ", skill.name)) },
        })
        .collect()
}

#[component]
pub(crate) fn PromptBox(state: Store<UiModel>, transport: runtime::TransportHandle, thread: t3_contracts::ThreadShell, draft: String, placeholder: String, attach_paste: bool) -> Element {
    let mut selected = use_signal(|| 0usize);
    let mut dismissed = use_signal(|| None::<String>);
    let mut found = use_signal(|| (String::new(), Vec::<Entry>::new(), false));
    let trigger = menu::detect_trigger(&draft, draft.len());
    // Without the grant the server would only refuse, so `@` stays plain text.
    let can_search = state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, t3_contracts::AuthEnvironmentScope::FilesystemRead));
    let path_query = trigger.as_ref().filter(|trigger| can_search && trigger.kind == TriggerKind::Path).map(|trigger| trigger.query.clone());
    let cwd = {
        let shell = state.shell();
        let shell = shell.read();
        thread.worktree_path.as_ref().map(ToString::to_string).or_else(|| shell.snapshot.as_ref().and_then(|snapshot| snapshot.projects.iter().find(|project| project.id == thread.project_id)).map(|project| project.workspace_root.to_string()))
    };
    // File search: one request per query, and an answer for an older query is dropped.
    {
        let transport = transport.clone();
        let cwd = cwd.clone();
        use_effect(use_reactive((&path_query, &cwd), move |(query, cwd)| {
            let (Some(query), Some(cwd)) = (query, cwd) else {
                return;
            };
            let (known_query, known_entries, in_flight) = found.peek().clone();
            if known_query == query && !in_flight {
                return;
            }
            found.set((query.clone(), known_entries, true));
            let transport = transport.clone();
            spawn(async move {
                let result = runtime::request_value(transport, state, "projects.searchEntries", json!({"cwd":cwd,"query":query,"limit":20})).await;
                if found.peek().0 != query {
                    return;
                }
                let entries = result.ok().and_then(|value| value["entries"].as_array().cloned()).unwrap_or_default().iter().filter_map(|entry| {
                    let path = entry["path"].as_str()?;
                    let directory = entry["kind"] == "directory";
                    Some(Entry { id: format!("path:{path}"), label: format!("{path}{}", if directory { "/" } else { "" }), description: if directory { "Folder".into() } else { String::new() }, apply: Apply::Insert(format!("@{path} ")) })
                }).collect();
                found.set((query, entries, false));
            });
        }));
    }
    let (plan_mode, show_skills) = {
        let settings = state.client_settings();
        let settings = settings.read();
        (settings.plan_mode_enabled, settings.show_skills_in_slash_menu)
    };
    let entries: Vec<Entry> = match &trigger {
        Some(trigger) if trigger.kind == TriggerKind::SlashCommand => {
            let (items, skills) = provider_entries(state, &thread, plan_mode, show_skills);
            slash_entries(&items, &skills, &trigger.query, trigger.range_start == 0)
        }
        Some(trigger) if trigger.kind == TriggerKind::Skill => {
            let (_, skills) = provider_entries(state, &thread, plan_mode, true);
            menu::search_skills(&skills, &trigger.query).into_iter().map(|index| Entry { id: format!("skill:{}", skills[index].name), label: format!("${}", skills[index].name), description: skills[index].short_description.clone().or_else(|| skills[index].description.clone()).unwrap_or_default(), apply: Apply::Insert(format!("${} ", skills[index].name)) }).collect()
        }
        Some(trigger) if can_search && trigger.kind == TriggerKind::Path && found.read().0 == trigger.query => found.read().1.clone(),
        _ => Vec::new(),
    };
    let searching = can_search && trigger.as_ref().is_some_and(|trigger| trigger.kind == TriggerKind::Path) && (found.read().0 != path_query.clone().unwrap_or_default() || found.read().2);
    let open = trigger.is_some() && dismissed.read().as_deref() != Some(draft.as_str()) && (!entries.is_empty() || searching);
    let current = selected().min(entries.len().saturating_sub(1));
    let pick = {
        let (transport, draft, trigger) = (transport.clone(), draft.clone(), trigger.clone());
        let thread_id = thread.id.to_string();
        let instance_id = thread.provider_instance_id.to_string();
        move |entry: Entry| {
            let Some(trigger) = trigger.clone() else { return };
            let replacement = match &entry.apply {
                Apply::Insert(text) => text.clone(),
                _ => String::new(),
            };
            let (text, _) = menu::replace_range(&draft, trigger.range_start, trigger.range_end, &replacement);
            draft_storage::edit_thread_prompt(state, text);
            match entry.apply {
                Apply::Mode(mode) => {
                    runtime::command(&transport, state, "thread.interaction-mode.set", json!({"threadId":thread_id,"interactionMode":mode}));
                }
                Apply::Model => {
                    let _ = document::eval("document.querySelector('.thread-controls select[aria-label=\"Model\"]')?.focus();");
                }
                Apply::UsageLimits => crate::limits_view::open_panel(state, &thread_id, &instance_id),
                Apply::Insert(_) => {}
            }
            selected.set(0);
        }
    };
    let menu_label = "Suggestions";
    let keyed = entries.clone();
    let pick_key = pick.clone();
    let draft_key = draft.clone();
    rsx! {
        div {class:"prompt-box",
            textarea { "aria-label": "Message", "aria-autocomplete":"list", "aria-expanded":"{open}", "data-attach-paste": if attach_paste {"on"} else {"off"}, placeholder:"{placeholder}", value: "{draft}",
                oninput: move |event| {selected.set(0);draft_storage::edit_thread_prompt(state,event.value())},
                onkeydown: move |event| {
                    if !open {return;}
                    match event.key() {
                        Key::ArrowDown if !keyed.is_empty() => {event.prevent_default();selected.set((current+1)%keyed.len());}
                        Key::ArrowUp if !keyed.is_empty() => {event.prevent_default();selected.set((current+keyed.len()-1)%keyed.len());}
                        Key::Enter | Key::Tab if !keyed.is_empty() && !event.modifiers().shift() => {event.prevent_default();let mut pick=pick_key.clone();pick(keyed[current].clone());}
                        Key::Escape => {event.stop_propagation();dismissed.set(Some(draft_key.clone()));}
                        _ => {}
                    }
                }
            }
            if open {
                ul {class:"composer-menu",role:"listbox","aria-label":menu_label,
                    if entries.is_empty() {li {class:"muted","Searching…"}}
                    for (index,entry) in entries.iter().enumerate() {
                        li {key:"{entry.id}",role:"option","aria-selected":"{index==current}",class:if index==current {"selected"} else {""},
                            // Picking must not take the focus from the box.
                            onmousedown:move |event| event.prevent_default(),
                            onclick:{let entry=entry.clone();let mut pick=pick.clone();move |_|pick(entry.clone())},
                            span {class:"menu-label","{entry.label}"}
                            if !entry.description.is_empty() {span {class:"muted","{entry.description}"}}
                        }
                    }
                }
            }
        }
    }
}

/// A prompt that is only `/plan` or `/default` switches the mode instead of being sent.
pub(crate) fn standalone_mode(draft: &str) -> Option<&'static str> {
    menu::standalone_command(draft)
}
