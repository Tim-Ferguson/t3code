//! The message box and its suggestion menus: `/` for commands, `$` for skills, `@` for files.
//! The menu follows the end of the prompt (the box does not report its caret), reads the provider's
//! own commands and skills from the configuration, and searches files through the workspace
//! search the server already has. Arrow keys move, Enter or Tab picks, Escape closes.
use crate::{draft_storage, runtime, runtime::{UiModel, UiModelStoreExt}};
use dioxus::prelude::*;
use t3_client::prompt_history;
use serde_json::json;
use crate::limits_view;
use t3_client::{composer_context, composer_menu::{self as menu, SlashItem, Skill, TriggerKind}, pull_request_mention as mention, usage_limits as logic};

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
    /// Name this pull request in the text and attach it as a context chip (`#`).
    PullRequest(serde_json::Value),
}
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    id: String,
    label: String,
    description: String,
    apply: Apply,
    /// The state a pull request is in, which the original shows as an icon.
    badge: Option<&'static str>,
}

/// What the `#` menu has read from the host for the trigger it is open on.
#[derive(Debug, Clone, PartialEq, Default)]
struct Pulls {
    /// The project and the words searched for; the answer below is for this.
    key: String,
    entries: Vec<serde_json::Value>,
    providers: Vec<serde_json::Value>,
    failed: bool,
    loaded: bool,
    /// The pull request a typed number names when the list did not hold it.
    exact: Option<serde_json::Value>,
    exact_asked: Option<u64>,
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
                badge: None, apply: match command.as_str() {
                    "plan" => Apply::Mode("plan"),
                    "default" => Apply::Mode("default"),
                    _ => Apply::Model,
                },
            },
            SlashItem::Provider { name, description, .. } => Entry { id: format!("provider:{name}"), label: format!("/{name}"), description: description.clone(), badge: None, apply: if name == logic::USAGE_LIMITS_COMMAND_NAME { Apply::UsageLimits } else { Apply::Insert(format!("/{name} ")) } },
            SlashItem::Skill { skill, .. } => Entry { id: format!("skill:{}", skill.name), label: format!("${}", skill.name), description: skill.short_description.clone().or_else(|| skill.description.clone()).unwrap_or_default(), badge: None, apply: Apply::Insert(format!("${} ", skill.name)) },
        })
        .collect()
}

#[component]
pub(crate) fn PromptBox(state: Store<UiModel>, transport: runtime::TransportHandle, thread: t3_contracts::ThreadShell, draft: String, placeholder: String, attach_paste: bool, on_send: Callback<()>) -> Element {
    let composition = use_hook(Composition::default);
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
                    Some(Entry { id: format!("path:{path}"), label: format!("{path}{}", if directory { "/" } else { "" }), description: if directory { "Folder".into() } else { String::new() }, badge: None, apply: Apply::Insert(format!("{} ", t3_client::file_tree::mention(path))) })
                }).collect();
                found.set((query, entries, false));
            });
        }));
    }
    // Pull requests: the project's recent ones from the host, searched for the words after `#` once
    // typing pauses; a number the list does not hold is asked for by itself. An answer for an
    // older question is dropped.
    let mut pulls = use_signal(Pulls::default);
    // Where prompt recall is (ArrowUp on an empty box walks back through what was sent); a different thread starts again.
    let mut history = use_signal(|| None::<prompt_history::Position>);
    let project_id = thread.project_id.to_string();
    let pull_requests_supported = state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.pull_requests == Some(true)) && state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, t3_contracts::AuthEnvironmentScope::OrchestrationRead));
    let pull_trigger = trigger.as_ref().filter(|trigger| trigger.kind == TriggerKind::PullRequest).map(|trigger| trigger.query.clone());
    let pull_active = pull_requests_supported && pull_trigger.is_some();
    let pull_text = pull_trigger.as_deref().and_then(mention::text_query).map(str::to_owned);
    let pull_number = pull_trigger.as_deref().and_then(mention::number_query);
    {
        let transport = transport.clone();
        use_effect(use_reactive((&pull_active, &pull_text, &project_id), move |(active, text, project)| {
            // Opening the menu again asks again: pull requests change while a message is written.
            if !active {
                if !pulls.peek().key.is_empty() {
                    *pulls.write() = Pulls::default();
                }
                return;
            }
            let key = format!("{project}\u{0}{}", text.clone().unwrap_or_default());
            if pulls.peek().key == key {
                return;
            }
            *pulls.write() = Pulls { key: key.clone(), ..Pulls::default() };
            let transport = transport.clone();
            spawn(async move {
                if text.is_some() {
                    runtime::delay(mention::DEBOUNCE_MS).await;
                    if pulls.peek().key != key {
                        return;
                    }
                }
                let answer = runtime::request_value_reported(transport, state, t3_client::pull_request::LIST, mention::list_input(&project, text.as_deref())).await;
                if pulls.peek().key != key {
                    return;
                }
                let mut next = pulls.peek().clone();
                match answer {
                    Ok(value) => {
                        next.entries = value["entries"].as_array().cloned().unwrap_or_default();
                        next.providers = value["providers"].as_array().cloned().unwrap_or_default();
                        next.failed = value["errors"].as_array().is_some_and(|errors| errors.iter().any(|error| error["projectId"] == project.as_str()));
                    }
                    Err(_) => next.failed = true,
                }
                next.loaded = true;
                pulls.set(next);
            });
        }));
    }
    {
        let transport = transport.clone();
        use_effect(use_reactive((&pull_number, &project_id, &pulls.read().loaded), move |(number, project, loaded)| {
            let Some(number) = number.filter(|_| loaded) else { return };
            let current = pulls.peek().clone();
            let held = current.entries.iter().any(|entry| entry["projectId"] == project.as_str() && entry["number"] == number);
            let Some(repository) = current.entries.iter().find(|entry| entry["projectId"] == project.as_str()).and_then(|entry| entry["repository"].as_str()).map(str::to_owned) else { return };
            if held || current.exact_asked == Some(number) {
                return;
            }
            pulls.write().exact_asked = Some(number);
            let transport = transport.clone();
            spawn(async move {
                runtime::delay(mention::DEBOUNCE_MS).await;
                if pulls.peek().exact_asked != Some(number) {
                    return;
                }
                let answer = runtime::request_value_reported(transport, state, t3_client::pull_request::DETAIL, json!({"projectId":project,"repository":repository,"number":number})).await;
                if pulls.peek().exact_asked == Some(number) {
                    if let Some(detail) = answer.ok().filter(|detail| detail["number"] == number) {
                        pulls.write().exact = Some(detail);
                    }
                }
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
            menu::search_skills(&skills, &trigger.query).into_iter().map(|index| Entry { id: format!("skill:{}", skills[index].name), label: format!("${}", skills[index].name), description: skills[index].short_description.clone().or_else(|| skills[index].description.clone()).unwrap_or_default(), badge: None, apply: Apply::Insert(format!("${} ", skills[index].name)) }).collect()
        }
        Some(trigger) if can_search && trigger.kind == TriggerKind::Path && found.read().0 == trigger.query => found.read().1.clone(),
        Some(trigger) if pull_active && trigger.kind == TriggerKind::PullRequest => {
            let read = pulls.read();
            let exact = read.exact.as_ref().filter(|exact| Some(exact["number"].as_u64().unwrap_or(0)) == pull_number);
            let repository = read.entries.iter().find(|entry| entry["projectId"] == project_id.as_str()).and_then(|entry| entry["repository"].as_str());
            mention::matches(&read.entries, &read.providers, exact, &project_id, repository, &trigger.query).into_iter().map(|pull| Entry {
                id: format!("pull-request:{}:{}", pull["repository"].as_str().unwrap_or(""), pull["number"]),
                label: format!("#{}", pull["number"]),
                description: pull["title"].as_str().unwrap_or("").to_owned(),
                badge: Some(t3_client::pull_request::list_state_label(&pull)),
                apply: Apply::PullRequest(pull),
            }).collect()
        }
        _ => Vec::new(),
    };
    // The `#` menu always answers: while it reads, when nothing matches, and where pull requests cannot be read.
    let pull_status = trigger.as_ref().filter(|trigger| trigger.kind == TriggerKind::PullRequest && entries.is_empty()).map(|trigger| {
        let read = pulls.read();
        let provided = !read.providers.is_empty() || !read.entries.is_empty();
        if pull_active && !read.loaded || (pull_active && mention::text_query(&trigger.query).is_some() && read.key != format!("{project_id}\u{0}{}", trigger.query)) {
            mention::LOADING_TEXT.to_owned()
        } else {
            mention::empty_text(pull_active && (provided || read.failed), pull_active && read.failed, &trigger.query)
        }
    });
    let searching = can_search && trigger.as_ref().is_some_and(|trigger| trigger.kind == TriggerKind::Path) && (found.read().0 != path_query.clone().unwrap_or_default() || found.read().2);
    let open = trigger.is_some() && dismissed.read().as_deref() != Some(draft.as_str()) && (!entries.is_empty() || searching || pull_status.is_some());
    let current = selected().min(entries.len().saturating_sub(1));
    let pick = {
        let (transport, draft, trigger) = (transport.clone(), draft.clone(), trigger.clone());
        let thread_id = thread.id.to_string();
        let instance_id = thread.provider_instance_id.to_string();
        move |entry: Entry| {
            let Some(trigger) = trigger.clone() else { return };
            let mut range_end = trigger.range_end;
            let replacement = match &entry.apply {
                Apply::Insert(text) => text.clone(),
                // The chip is the reader's own; the text names it, and the space after it is not doubled.
                Apply::PullRequest(pull) => {
                    let record = composer_context::review_record(&mention::reference_comment(pull));
                    let reference = composer_context::reference("review-comment", record["contextId"].as_str().unwrap_or(""), record["label"].as_str().unwrap_or(""));
                    if draft[range_end..].starts_with(' ') {
                        range_end += 1;
                    }
                    if let Some(target) = crate::composer_chips::open_target(state) {
                        crate::composer_chips::add(state, target, record);
                    }
                    format!("{reference} ")
                }
                _ => String::new(),
            };
            let (text, _) = menu::replace_range(&draft, trigger.range_start, range_end, &replacement);
            draft_storage::edit_thread_prompt(state, text);
            match entry.apply {
                Apply::Mode(mode) => {
                    runtime::command(&transport, state, "thread.interaction-mode.set", json!({"threadId":thread_id,"interactionMode":mode}));
                }
                Apply::Model => {
                    let _ = document::eval("document.querySelector('.thread-controls select[aria-label=\"Model\"]')?.focus();");
                }
                Apply::UsageLimits => crate::limits_view::open_panel(state, &thread_id, &instance_id),
                Apply::Insert(_) | Apply::PullRequest(_) => {}
            }
            selected.set(0);
        }
    };
    let menu_label = "Suggestions";
    let keyed = entries.clone();
    let thread_id_key = thread.id.to_string();
    let pick_key = pick.clone();
    let draft_key = draft.clone();
    rsx! {
        div {class:"prompt-box",
            textarea { "aria-label": "Message", "aria-autocomplete":"list", "aria-expanded":"{open}", "data-attach-paste": if attach_paste {"on"} else {"off"}, placeholder:"{placeholder}", value: "{draft}",
                oninput: move |event| {selected.set(0);draft_storage::edit_thread_prompt(state,event.value())},
                oncompositionend: { let composition = composition.clone(); move |_| composition.ended() },
                onkeydown: move |event| {
                    if open {
                        match event.key() {
                            Key::ArrowDown if !keyed.is_empty() => {event.prevent_default();selected.set((current+1)%keyed.len());return;}
                            Key::ArrowUp if !keyed.is_empty() => {event.prevent_default();selected.set((current+keyed.len()-1)%keyed.len());return;}
                            Key::Enter | Key::Tab if !keyed.is_empty() && !event.modifiers().shift() => {event.prevent_default();let mut pick=pick_key.clone();pick(keyed[current].clone());return;}
                            Key::Escape => {event.stop_propagation();dismissed.set(Some(draft_key.clone()));return;}
                            _ => {}
                        }
                    }
                    // ArrowUp on an empty box recalls the last prompt sent, as a terminal does.
                    if let Some(direction) = history_direction(&event, &composition) {
                        let position = history.peek().clone();
                        if let Some(step) = recall(state, &thread_id_key, direction, position.as_ref(), &draft_key) {
                            event.prevent_default();
                            history.set(step.position);
                            draft_storage::edit_thread_prompt(state, step.prompt);
                            return;
                        }
                    }
                    // A menu with nothing to pick leaves Enter to the box, which sends or adds a line.
                    if enter_sends(state, &event, &draft_key, &composition) {
                        event.prevent_default();
                        on_send.call(());
                    }
                }
            }
            if open {
                ul {class:"composer-menu",role:"listbox","aria-label":menu_label,
                    if entries.is_empty() {li {class:"muted",role:"status",{pull_status.clone().unwrap_or_else(|| "Searching…".to_owned())}}}
                    for (index,entry) in entries.iter().enumerate() {
                        li {key:"{entry.id}",role:"option","aria-selected":"{index==current}",class:if index==current {"selected"} else {""},
                            // Picking must not take the focus from the box.
                            onmousedown:move |event| event.prevent_default(),
                            onclick:{let entry=entry.clone();let mut pick=pick.clone();move |_|pick(entry.clone())},
                            span {class:"menu-label","{entry.label}"}
                            if !entry.description.is_empty() {span {class:"muted","{entry.description}"}}
                            if let Some(badge) = entry.badge {span {class:"muted pr-state","data-state":"{badge}","{badge}"}}
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

/// When the last IME composition in a box ended. Safari reports the Enter that confirms a
/// candidate after the composition has ended (with `isComposing` already false), so an Enter
/// that follows the end at once still belongs to the IME.
#[derive(Clone, Default)]
pub(crate) struct Composition(std::rc::Rc<std::cell::Cell<i64>>);
impl Composition {
    pub(crate) fn ended(&self) {
        self.0.set(chrono::Utc::now().timestamp_millis());
    }
    fn just_ended(&self) -> bool {
        chrono::Utc::now().timestamp_millis() - self.0.get() < 100
    }
}

/// Whether the window is as narrow as a phone, where Enter is a new line.
fn mobile_viewport() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window().and_then(|window| window.inner_width().ok()).and_then(|width| width.as_f64()).is_some_and(|width| width < t3_client::composer_send::MOBILE_MAX_WIDTH)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

/// Whether this key press in a message box sends the message (see `t3_client::composer_send`).
/// The key is read from `keydown` by its `key` text, and a send cancels the default so the line
/// break the browser would insert is not inserted as well.
/// Which way ArrowUp or ArrowDown would walk through the prompts sent, when it may (no modifier, not composing).
fn history_direction(event: &KeyboardEvent, composition: &Composition) -> Option<prompt_history::Direction> {
    let modifiers = event.modifiers();
    if modifiers.shift() || modifiers.alt() || modifiers.meta() || modifiers.ctrl() || event.is_composing() || composition.just_ended() {
        return None;
    }
    match event.key() {
        Key::ArrowUp => Some(prompt_history::Direction::Backward),
        Key::ArrowDown => Some(prompt_history::Direction::Forward),
        _ => None,
    }
}
/// One step of recall for the open thread, or none when the key belongs to the caret: the box holds a draft
/// no recall made, an attachment or a chip, or the recalled text runs over several lines.
fn recall(state: Store<UiModel>, thread_id: &str, direction: prompt_history::Direction, position: Option<&prompt_history::Position>, current: &str) -> Option<prompt_history::Step> {
    if current.contains('\n') {
        return None;
    }
    let target = crate::composer_chips::open_target(state)?;
    if !crate::composer_chips::contexts_of(state, &target).is_empty() || state.draft_attachments().peek().get(&target).is_some_and(|held| !held.is_empty()) {
        return None;
    }
    let field = state.thread();
    let model = field.peek();
    let projection = model.projection.as_ref().filter(|projection| projection["thread"]["id"] == thread_id)?;
    let messages = projection["messages"].as_array()?;
    prompt_history::step(direction, &prompt_history::entries(messages), position, current)
}

pub(crate) fn enter_sends(state: Store<UiModel>, event: &KeyboardEvent, prompt: &str, composition: &Composition) -> bool {
    let key = crate::keybinding_page::key_event(event);
    key.key == "Enter"
        && !composition.just_ended()
        && t3_client::composer_send::enter_sends(&key, state.client_settings().peek().send_shortcut, prompt, mobile_viewport())
}
