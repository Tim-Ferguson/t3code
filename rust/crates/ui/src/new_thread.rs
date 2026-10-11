use crate::runtime::{self, UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::{lifecycle, models, new_thread};
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
    // The form has its draft in the render that shows it: this does nothing when it has one already.
    crate::draft_sessions::ensure_for_form(state);
    let selected = state.selected_project().read().clone();
    let project = projects
        .iter()
        .find(|p| Some(p.id.as_str()) == selected.as_deref())
        .or_else(|| projects.first())
        .cloned();
    let Some(project) = project else {
        return rsx! {};
    };
    let draft = state.active_draft().read().clone();
    let Some(draft) = draft.filter(|draft| crate::draft_sessions::project_of(state, draft).as_deref() == Some(project.id.as_str())) else {
        return rsx! {};
    };
    let destination = state
        .destination()
        .read()
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let key = serde_json::to_string(&(&destination, &draft)).expect("identity");
    rsx! {
        div { class:"new-thread-controls",
            for project in [project.clone()] {NewThreadOptions { key:"{key}",state,transport:transport.clone(),projects:projects.clone(),project,draft:draft.clone(),can_operate }}
        }
    }
}
/// The project the new thread is for, among the pickers under the message box.
#[component]
fn ProjectPicker(state: Store<UiModel>, projects: Vec<ProjectShell>, current: String) -> Element {
    rsx! {
        label { "Project"
            select { "aria-label":"Project",value:"{current}",onchange:move|e|state.selected_project().set(Some(e.value())),
                for p in &projects { option {value:"{p.id}",selected:p.id.as_str()==current,"{p.title}"} }
            }
        }
    }
}
type DraftKey = (EnvironmentId, String);
fn previous_for(state: Store<UiModel>, project: &ProjectShell, current: Option<&str>) -> Option<(Option<String>, String)> {
    let field = state.shell();
    let shell = field.read();
    let snapshot = shell.snapshot.as_ref()?;
    let threads: Vec<lifecycle::WorktreeThread<'_>> = snapshot
        .threads
        .iter()
        .filter(|thread| thread.project_id == project.id && thread.deleted_at.is_none())
        .map(|thread| lifecycle::WorktreeThread { branch: thread.branch.as_ref().map(|branch| branch.as_str()), worktree_path: thread.worktree_path.as_ref().map(|path| path.as_str()), updated_at: thread.updated_at.0.timestamp_millis(), archived: thread.archived_at.is_some() })
        .collect();
    lifecycle::previous_worktree(&threads, current)
}
/// The project the new-thread form is for, when the form is on screen.
fn form_project(state: Store<UiModel>) -> Option<(ProjectShell, DraftKey)> {
    let draft = state.active_draft().read().clone()?;
    if *state.view().read() != crate::runtime::View::Chat || state.active_thread().read().is_some() {
        return None;
    }
    let field = state.shell();
    let shell = field.read();
    let snapshot = shell.snapshot.as_ref()?;
    let selected = state.selected_project().read().clone();
    let project = snapshot.projects.iter().find(|project| Some(project.id.as_str()) == selected.as_deref()).or_else(|| snapshot.projects.first())?.clone();
    let destination = state.destination().read().clone()?;
    let key = (destination, draft);
    Some((project, key))
}
/// Whether `composer.workspace` has a form to act on.
pub(crate) fn workspace_form_open(state: Store<UiModel>) -> bool {
    form_project(state).is_some()
}
/// Whether `composer.previousWorktree` has a worktree to hop into.
pub(crate) fn previous_worktree_available(state: Store<UiModel>) -> bool {
    form_project(state).is_some_and(|(project, key)| {
        let current = state.new_thread_choices().read().get(&key).and_then(|choices| choices.worktree_path.clone());
        previous_for(state, &project, current.as_deref()).is_some()
    })
}
/// `composer.previousWorktree`: the form starts in the project's previous worktree.
pub(crate) fn use_previous_worktree(state: Store<UiModel>) {
    let Some((project, key)) = form_project(state) else { return };
    let current = state.new_thread_choices().peek().get(&key).and_then(|choices| choices.worktree_path.clone());
    if let Some((branch, path)) = previous_for(state, &project, current.as_deref()) {
        update_choices(state, Some(&key), |choices| use_worktree(choices, path, branch));
    }
}
/// A pull request was prepared for a thread: the form starts on its branch, in the worktree that
/// holds it or in the project's own checkout.
pub(crate) fn apply_prepared(state: Store<UiModel>, branch: &str, worktree_path: Option<String>) {
    let Some(destination) = state.destination().peek().clone() else { return };
    let Some(draft) = state.active_draft().peek().clone() else { return };
    let key = (destination, draft);
    update_choices(state, Some(&key), |choices| match worktree_path {
        Some(path) => use_worktree(choices, path, Some(branch.to_owned())),
        None => {
            choices.environment_mode = Some(ThreadEnvMode::Local);
            choices.worktree_path = None;
            choices.base_ref = branch.to_owned();
        }
    });
}
/// The project the new thread form is for, and its folder: what a pull request checkout needs.
pub(crate) fn form_workspace(state: Store<UiModel>) -> Option<(String, String)> {
    form_project(state).map(|(project, _)| (project.id.to_string(), project.workspace_root.to_string()))
}
/// `composer.workspace`: the workspace choice takes the keyboard.
pub(crate) fn focus_workspace() {
    dioxus::document::eval("document.getElementById('new-thread-workspace')?.focus();");
}
/// Points the form at an existing worktree, remembered with the rest of the draft's choices.
fn use_worktree(choices: &mut runtime::NewThreadChoices, path: String, branch: Option<String>) {
    choices.environment_mode = Some(ThreadEnvMode::Worktree);
    choices.worktree_path = Some(path);
    if let Some(branch) = branch {
        choices.base_ref = branch;
    }
}
fn update_choices(
    state: Store<UiModel>,
    key: Option<&DraftKey>,
    update: impl FnOnce(&mut runtime::NewThreadChoices),
) {
    if let Some(key) = key {
        {
            let mut field = state.new_thread_choices();
            let mut choices = field.write();
            update(choices.entry(key.clone()).or_default());
        }
        let choices = state
            .new_thread_choices()
            .peek()
            .get(key)
            .cloned()
            .unwrap_or_default();
        crate::draft_storage::save_choices(state, key, &choices);
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
    projects: Vec<ProjectShell>,
    project: ProjectShell,
    draft: String,
    can_operate: bool,
) -> Element {
    // Component ownership includes both destination and project; asynchronous
    // catalog arrival resolves defaults without writing them into an explicit draft.
    let config_signal = state.typed_config();
    let config = config_signal.read();
    let Some(config) = config.as_ref() else {
        return rsx! {div {class:"new-thread-pickers",ProjectPicker {state,projects:projects.clone(),current:project.id.to_string()} p {class:"muted","Loading provider catalog…"}}};
    };
    let client_signal = state.client_settings();
    let client = client_signal.read();
    let defaults = new_thread::project_defaults(&config.settings, Some(&project));
    let destination = state.destination().read().clone();
    let draft_key = destination
        .as_ref()
        .map(|destination| (destination.clone(), draft.clone()));
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
            &pending.destination == destination && pending.draft_id == draft
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
    let worktree_path = choices.worktree_path.clone().filter(|_| environment == ThreadEnvMode::Worktree);
    let environment_value = if worktree_path.is_some() {
        "existing"
    } else if environment == ThreadEnvMode::Worktree {
        "worktree"
    } else {
        "local"
    };
    // The worktree of the project's most recent thread that this form is not already using.
    let previous = previous_for(state, &project, worktree_path.as_deref());
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
    let unsupported = draft_key.as_ref().is_some_and(|key| {
        crate::draft_storage::has_unrendered(
            state,
            &t3_client::draft_storage::DraftTarget::draft(key.0.to_string(), key.1.clone()),
        )
    });
    let attachment_target = destination.as_ref().map(|destination| {
        t3_client::draft_storage::DraftTarget::draft(destination.to_string(), draft.clone())
    });
    let (attachment_count, attachment_block) = attachment_target
        .as_ref()
        .and_then(|target| {
            state.draft_attachments().read().get(target).map(|list| {
                (list.len(), t3_client::attachments::block_reason_for(list, &crate::attachments::limits(state)))
            })
        })
        .unwrap_or_default();
    let mut drop_files = attachment_target
        .clone()
        .map(|target| crate::attachments::on_drop(&transport, state, target));
    let disabled = unsupported
        || attachment_block.is_some()
        || pending
        || !can_operate
        || selected.is_none()
        || (environment == ThreadEnvMode::Worktree && worktree_path.is_none() && base_ref.trim().is_empty())
        || selected.as_ref().is_some_and(|s| {
            rows.iter()
                .find(|r| r.instance_id == s.instance_id && r.model.slug == s.model.as_str())
                .is_some_and(|r| r.model.is_unavailable)
        });
    let launch_selection = selected.clone();
    let project_id = project.id.to_string();
    let chip_count = attachment_target.as_ref().map_or(0, |target| crate::composer_chips::count(state, target));
    // A project whose clone has not landed starts no thread; the reason repaints this form only when it changes.
    let clone_block = use_memo(use_reactive((&project_id,), move |(project_id,)| crate::clone_view::block_reason(state, &project_id)));
    let clone_blocked = clone_block.read().is_some();
    let prompt_is_empty = prompt.trim().is_empty();
    // Starting the thread from the button or `composer.sendBackground`; the background form leaves the person here.
    let launch = {
        let (transport, project_id, draft, prompt, launch_selection, worktree_path, base_ref) = (transport.clone(), project_id.clone(), draft.clone(), prompt.clone(), launch_selection.clone(), worktree_path.clone(), base_ref.clone());
        move |background: bool| {
            if let Some(selection)=launch_selection.as_ref() {
                let workspace=if let Some(path)=worktree_path.clone() {
                    let Ok(worktree_path)=TrimmedNonEmptyString::new(&path) else{return;};
                    ThreadLaunchWorkspaceStrategy::ExistingWorktree(WorkspaceExistingWorktree {worktree_path,branch:TrimmedNonEmptyString::new(&base_ref).ok().map(Some)})
                } else if environment==ThreadEnvMode::Worktree {
                    let Ok(base_ref)=TrimmedNonEmptyString::new(&base_ref) else{return;};
                    ThreadLaunchWorkspaceStrategy::Worktree(WorkspaceWorktree {base_ref,branch:None,start_from_origin:None})
                } else {ThreadLaunchWorkspaceStrategy::Root(WorkspaceRoot {branch:None})};
                if let Some(id)=runtime::launch_thread(&transport,state,&project_id,&draft,selection,mode,workspace,Some(&prompt)) {
                    if background {
                        if let Some(pending)=state.pending_launches().write().get_mut(&id) {pending.background=true;}
                    }
                }
            }
        }
    };
    let composition = use_hook(crate::composer_menu::Composition::default);
    // `composer.sendBackground` asks this form to start its thread without opening it.
    {
        let launch=launch.clone();
        let mut seen=use_signal(|| *state.background_sends().peek());
        use_effect(move || {
            let requested=*state.background_sends().read();
            if requested != *seen.peek() {
                seen.set(requested);
                if !disabled && !clone_blocked && (!prompt_is_empty || attachment_count>0 || chip_count>0) {launch(true);}
            }
        });
    }
    rsx! {
        crate::clone_view::Banner { state, transport: transport.clone(), project_id: project_id.clone() }
        if unsupported {p {class:"error-banner",role:"alert","This draft includes saved context. It is preserved; use the original app to send this complete draft."}}
        // The message box is the main thing on the screen, laid out as the thread's composer is; the
        // choices that shape the thread sit beneath it.
        div {class:"new-thread-card new-thread-attachments",ondragover:move|event|event.prevent_default(),ondrop:move|event|{if let Some(handler)=drop_files.as_mut(){handler(event);}},
            crate::prompt_stash::Dock {state,transport:transport.clone()}
            if let Some(target)=attachment_target.clone() { crate::composer_chips::Tray {state,target} }
            div {class:"new-thread-prompt",textarea {"aria-label":"First message","data-attach-paste":if can_operate{"on"}else{"off"},placeholder:"What would you like to work on?",value:"{prompt}",oninput:{let draft_key=draft_key.clone();move|e|{
                if let Some(key)=draft_key.clone() {crate::draft_storage::edit_draft_prompt(state,key,e.value());}
            }},
            oncompositionend:{let composition=composition.clone();move|_|composition.ended()},
            // Enter starts the thread, as the Start button does; Shift-Enter and IME confirmation add a line or confirm.
            onkeydown:{let mut launch=launch.clone();let prompt=prompt.clone();let composition=composition.clone();move|event|{
                if crate::composer_menu::enter_sends(state,&event,&prompt,&composition){
                    event.prevent_default();
                    if !disabled && !clone_blocked && (!prompt_is_empty||attachment_count>0||chip_count>0){launch(false);}
                }
            }}}}
            if let Some(target)=attachment_target.clone() {
                crate::attachments::Shelf {state,transport:transport.clone(),target:target.clone(),locked:pending,can_operate,paste_target:".new-thread-prompt textarea"}
            }
            crate::composer_panels::PromptLength {state,text:Some(prompt.clone())}
            div {class:"composer-toolbar",
                if can_operate { if let Some(target)=attachment_target.clone() { crate::attachments::AttachButton {state,transport:transport.clone(),target} } }
                span {}
                button {"aria-label":"Start in background",disabled:disabled || clone_blocked || (prompt.trim().is_empty() && attachment_count==0 && chip_count==0),title:"Start the thread without leaving this form",onclick:{let mut launch=launch.clone();move|_|launch(true)},"Start in background"}
                button {class:"primary","aria-label":"Create new thread",disabled:disabled || clone_blocked,title:clone_block.read().unwrap_or(""),onclick:{let mut launch=launch.clone();move|_|launch(false)},if pending {"Creating…"} else if prompt.trim().is_empty() && attachment_count==0 && chip_count==0 {"New thread"} else {"Start chat"}}
            }
        }
        div {class:"new-thread-pickers",
            ProjectPicker {state,projects:projects.clone(),current:project_id.clone()}
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
        crate::model_controls::ModelOptions {selected:selected.clone(),prompt_effort:crate::model_controls::PromptEffort::of(&descriptors,&prompt),on_effort:{let draft_key=draft_key.clone();let prompt=prompt.clone();let injected:Vec<String>=crate::model_controls::PromptEffort::of(&descriptors,&prompt).map(|effort|effort.injected).unwrap_or_default();move|value:String|{
            let injected:Vec<&str>=injected.iter().map(String::as_str).collect();
            if let (Some(key),Some(next))=(draft_key.clone(),t3_client::models::prompt_after_effort(&prompt,&value,&injected)) {if next!=prompt {crate::draft_storage::edit_draft_prompt(state,key,next);}}
        }},descriptors,onchange:{let draft_key=draft_key.clone();move|selection|remember_selection(state,draft_key.as_ref(),&selection)}}
        label {"Permissions",select {"aria-label":"Permissions",value:runtime_value,onchange:{let draft_key=draft_key.clone();move|e|{if let Ok(mode)=serde_json::from_value(json!(e.value())) {update_choices(state,draft_key.as_ref(),|choices|choices.runtime_mode=Some(mode));}}},
            for choice in modes {option {value:serde_json::to_value(choice).unwrap().as_str().unwrap(),selected:choice==mode,"{models::runtime_mode_label(choice)}"}}
        }}
        label {"Workspace",select {"aria-label":"Workspace",id:"new-thread-workspace",value:environment_value,onchange:{let draft_key=draft_key.clone();let previous=previous.clone();move|e|{
            let chosen=e.value();
            update_choices(state,draft_key.as_ref(),|choices|match chosen.as_str() {
                "local"=>{choices.environment_mode=Some(ThreadEnvMode::Local);choices.worktree_path=None;}
                "worktree"=>{choices.environment_mode=Some(ThreadEnvMode::Worktree);choices.worktree_path=None;}
                "previous"=>if let Some((branch,path))=previous.clone() {use_worktree(choices,path,branch);},
                _=>{}
            });
        }},
            option {value:"local",selected:environment_value=="local","Current checkout"}
            option {value:"worktree",selected:environment_value=="worktree","New worktree"}
            if worktree_path.is_some() {option {value:"existing",selected:environment_value=="existing","Existing worktree"}}
            if let Some((branch,_))=previous.as_ref() {option {value:"previous","{lifecycle::previous_worktree_label(branch.as_deref())}"}}
        }}
        if let Some(path)=worktree_path.clone() {
            p {class:"muted",role:"status","Starts in the existing worktree {path}"}
            button {r#type:"button","aria-label":"Use a new worktree instead",onclick:{let draft_key=draft_key.clone();move|_|update_choices(state,draft_key.as_ref(),|choices|choices.worktree_path=None)},"Use a new worktree instead"}
        }
        if environment==ThreadEnvMode::Worktree && worktree_path.is_none() {label {"Base branch",input {"aria-label":"Base branch",value:"{base_ref}",placeholder:"Branch or revision",oninput:{let draft_key=draft_key.clone();move|e|update_choices(state,draft_key.as_ref(),|choices|choices.base_ref=e.value())}}}}
        if environment==ThreadEnvMode::Worktree {crate::branch::BaseBranch {state,transport:transport.clone(),cwd:project.workspace_root.to_string(),on_pick:{let draft_key=draft_key.clone();let root=project.workspace_root.to_string();move|chosen:t3_client::vcs::Ref|update_choices(state,draft_key.as_ref(),|choices|{
            // A branch that already lives in another worktree starts the thread there.
            match chosen.worktree_path.filter(|path|*path!=root) {
                Some(path)=>use_worktree(choices,path,Some(chosen.name)),
                None=>{choices.base_ref=chosen.name;choices.worktree_path=None;}
            }
        })}}}
        }
        if rows.is_empty() {p {class:"muted","No ready providers. Check provider setup in Settings."}}
    }
}
