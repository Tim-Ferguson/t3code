//! Custom/environment library controls use the persistent App storage actor.
use crate::themes::Themes;
use dioxus::html::FileData;
use dioxus::prelude::*;
use t3_client::themes::{Appearance, Colors, Definition, import, library, vivid};
#[derive(Clone, PartialEq)]
pub struct EditorSession {
    pub editing_theme_id: Option<String>,
    pub seed_theme_id: Option<String>,
    pub seed_name: Option<String>,
    pub initial_appearance: Appearance,
    pub id: String,
}
#[derive(Clone, PartialEq)]
struct Editing {
    theme: Definition,
    existing: bool,
    initial_appearance: Appearance,
}
#[component]
pub fn ThemeEditorHost() -> Element {
    let service = use_context::<Themes>();
    let session = service.editor.read().clone();
    let catalog = service.catalog.read().clone();
    let mut editor = service.editor;
    let Some(session) = session else {
        return rsx! {};
    };
    let existing = session
        .editing_theme_id
        .as_deref()
        .and_then(|id| catalog.definition(id));
    let seed = existing.or_else(|| {
        session
            .seed_theme_id
            .as_deref()
            .and_then(|id| catalog.definition(id))
    });
    let mut theme = seed.cloned().unwrap_or(Definition {
        id: String::new(),
        label: String::new(),
        appearance: session.initial_appearance,
        colors: catalog.data.standard[&session.initial_appearance].clone(),
        variants: None,
        collection: None,
        managed: Some(true),
        sidebar_artwork: None,
    });
    if existing.is_none() {
        theme.id.clear();
        theme.label = session.seed_name.clone().unwrap_or_default();
    }
    let edit = Editing {
        theme,
        existing: existing.is_some(),
        initial_appearance: session.initial_appearance,
    };
    rsx! {ThemeEditor {key:"{session.id}",edit,onclose:move |_|editor.set(None)}}
}
fn open_editor(
    mut service: Themes,
    editing_theme_id: Option<String>,
    seed_theme_id: Option<String>,
    seed_name: Option<String>,
    initial_appearance: Appearance,
) {
    service.editor.set(Some(EditorSession {
        editing_theme_id,
        seed_theme_id,
        seed_name,
        initial_appearance,
        id: uuid::Uuid::new_v4().to_string(),
    }));
}
#[component]
pub fn ThemeLibrary() -> Element {
    let service = use_context::<Themes>();
    let catalog = service.catalog.read().clone();
    let mut importing = use_signal(|| false);
    let mut exported = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut pending = use_signal(|| false);
    let mut removal = use_signal(|| None::<Vec<Definition>>);
    let mut selected_removals = use_signal(Vec::<String>::new);
    let mode = service.snapshot.read().resolved_theme;
    let create = service.clone();
    let retry = service.clone();
    let unavailable = matches!(
        &*service.library.read(),
        Some(library::Library::Unavailable { .. })
    );
    let mut cards: Vec<_> = catalog
        .data
        .builtin
        .iter()
        .cloned()
        .map(|t| (t, false, false))
        .collect();
    cards.extend(
        catalog
            .environment
            .iter()
            .filter(|t| !catalog.custom.iter().any(|c| c.id == t.id))
            .cloned()
            .map(|t| (t, false, true)),
    );
    cards.extend(catalog.custom.iter().cloned().map(|t| (t, true, false)));
    rsx! {
        section {class:"theme-library","aria-label":"Theme library",
            div {class:"theme-library-heading",h3 {"Themes"}
                button {"aria-label":"Create theme",disabled:unavailable||*pending.read(),onclick:move |_|{
                    let seed={let catalog=create.catalog.peek();let snapshot=create.snapshot.peek();catalog.definition(catalog.half(&snapshot.theme,snapshot.theme_halves.as_ref(),mode)).map(|t|t.id.clone())};
                    open_editor(create.clone(),None,seed,None,mode);
                },"Create theme"}
                button {"aria-label":"Add theme",disabled:unavailable||*pending.read(),onclick:move |_|importing.set(true),"Add theme"}
            }
            if unavailable {div {class:"error-banner",role:"alert","The theme library could not be read. Saved records have been kept."
                button {onclick:move |_|retry.retry_library(),"Retry"}
            }}
            if let Some(message)=error.read().as_ref(){p {role:"alert",class:"error-banner","{message}"}}
            div {class:"theme-library-grid",
                for (theme,custom,environment) in cards {
                    {
                        let use_service=service.clone();let export_service=service.clone();let name=theme.label.clone();let id=theme.id.clone();let use_id=id.clone();let use_modes=theme.modes();let duplicate=theme.clone();let edit=theme.clone();let duplicate_service=service.clone();let edit_service=service.clone();let export=theme.clone();let delete=id.clone();let removal_theme=theme.clone();let removal_service=service.clone();
                        let colors=theme.colors(mode).unwrap_or(&theme.colors);let style=format!("background:{};color:{};border-color:{}",colors["canvas"],colors["text"],colors["border"]);
                        rsx!{article {key:"{id}",class:"theme-library-card",style,
                            div {class:"theme-card-preview",span {class:"theme-preview-sidebar",style:format!("background:{}",colors["sidebar"])},span {style:format!("background:{}",colors["surfaceRaised"]),"Aa"},span {style:format!("background:{}",colors["accent"])," "}}
                            h4 {"{name}"}
                            if environment {small {"Environment"}}
                            div {class:"theme-card-actions",
                                button {onclick:move |_|{if use_modes.len()==1{drop(use_service.choice(crate::themes::Choice::AssignHalf(use_modes[0],Some(use_id.clone()))))}else{use_service.change(t3_client::themes::storage::Action::Theme(use_id.clone()))}},"Use"}
                                button {disabled:unavailable,onclick:move |_|open_editor(duplicate_service.clone(),None,Some(duplicate.id.clone()),Some(format!("{} copy",duplicate.label)),mode),"Duplicate"}
                                if custom {
                                    button {onclick:move |_|open_editor(edit_service.clone(),Some(edit.id.clone()),None,None,mode),"Edit"}
                                    button {onclick:move |_|match library::export(&export_service.catalog.peek(),&export){Ok(text)=>exported.set(Some(text)),Err(cause)=>error.set(Some(cause))},"Download"}
                                    button {disabled:*pending.read(),onclick:move |_|{
                                        let members:Vec<_>=if let Some(collection)=&removal_theme.collection {let catalog=removal_service.catalog.peek();catalog.custom.iter().filter(|t|t.collection.as_ref().and_then(|v|v.get("id"))==collection.get("id")).cloned().collect()}else{vec![removal_theme.clone()]};
                                        selected_removals.set(if members.len()>1{vec![]}else{vec![delete.clone()]});removal.set(Some(members));
                                    },"Remove"}
                                }
                            }
                        }}
                    }
                }
            }
        }
        if let Some(members)=removal.read().clone(){
            div {class:"theme-dialog",role:"dialog","aria-label":"Remove themes",h3 {"Remove theme"}p {"Choose the themes to remove. This cannot be undone."}
                for theme in members {label {key:"{theme.id}",input {r#type:"checkbox",checked:selected_removals.read().contains(&theme.id),onchange:move|event|{let mut ids=selected_removals.write();if event.checked(){if !ids.contains(&theme.id){ids.push(theme.id.clone());}}else{ids.retain(|id|id!=&theme.id);}}},"{theme.label}"}}
                button {"aria-label":"Confirm remove themes",disabled:*pending.read()||selected_removals.read().is_empty(),onclick:{let service=service.clone();move |_|{let save=service.remove_themes(selected_removals.peek().clone());pending.set(true);spawn(async move{let result=save.await;pending.set(false);match result{Ok(_)=>removal.set(None),Err(cause)=>error.set(Some(cause))}});}},"Remove"}
                button {disabled:*pending.read(),onclick:move|_|removal.set(None),"Cancel"}
            }
        }
        if *importing.read(){ThemeImport {onclose:move |_|importing.set(false)}}
        if let Some(text)=exported.read().clone(){div {class:"theme-dialog",role:"dialog","aria-label":"Download theme",h3 {"Download theme"}textarea {readonly:true,value:text.clone()}a {download:"theme.json",href:format!("data:application/json;charset=utf-8,{}",urlencoding(&text)),"Download JSON"}button {onclick:move |_|exported.set(None),"Close"}}}
    }
}
fn urlencoding(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
#[derive(Clone, Copy)]
struct ImportState {
    text: Signal<String>,
    file: Signal<Option<String>>,
    error: Signal<Option<String>>,
    pending: Signal<bool>,
    conflicts: Signal<Option<Vec<Definition>>>,
}
async fn read_theme_files(
    service: Themes,
    files: Vec<FileData>,
    mut state: ImportState,
    onclose: EventHandler<()>,
) {
    if files.len() == 1 {
        let file = &files[0];
        if let Some(error) = import::oversized(file.size().min(usize::MAX as u64) as usize) {
            state.error.set(Some(error));
        } else {
            match file.read_string().await {
                Ok(text) => {
                    state.text.set(text);
                    state.file.set(Some(file.name()));
                    state.error.set(None);
                }
                Err(_) => state.error.set(Some(
                    "Could not read that file. Paste the JSON below instead.".into(),
                )),
            }
        }
        state.pending.set(false);
        return;
    }
    let mut failures = Vec::new();
    let mut entries = Vec::new();
    for file in files {
        if import::oversized(file.size().min(usize::MAX as u64) as usize).is_some() {
            failures.push(format!("{}: too large", file.name()));
            continue;
        }
        match file.read_string().await {
            Ok(text) => match import::parse(&service.catalog.peek(), &text) {
                Ok(theme) => entries.push(t3_client::themes::vscode::Entry {
                    theme,
                    source_name: Some(file.name()),
                }),
                Err(error) => failures.push(format!("{}: {error}", file.name())),
            },
            Err(error) => failures.push(format!("{}: {error}", file.name())),
        }
    }
    let themes = {
        let catalog = service.catalog.peek();
        t3_client::themes::vscode::pair(
            &catalog,
            &t3_client::themes::vscode::resolve_collisions(&catalog, &entries),
            None,
        )
    };
    let result = service
        .import_many(themes, crate::themes::ImportMode::New, None)
        .await;
    state.pending.set(false);
    match result {
        Err(error) => state.error.set(Some(error)),
        Ok(result) => {
            failures.extend(
                result["failures"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned()),
            );
            let conflicts: Vec<Definition> =
                serde_json::from_value(result["conflicts"].clone()).unwrap();
            if !failures.is_empty() {
                state.error.set(Some(failures.join(" — ")));
            } else if !conflicts.is_empty() {
                state.error.set(None);
                state.conflicts.set(Some(conflicts));
            } else if !result["saved"].as_array().unwrap().is_empty() {
                onclose.call(());
            }
        }
    }
}
#[component]
fn ThemeImport(onclose: EventHandler<()>) -> Element {
    let service = use_context::<Themes>();
    let mut text = use_signal(String::new);
    let file = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut pending = use_signal(|| false);
    let mut conflicts = use_signal(|| None::<Vec<Definition>>);
    let state = ImportState {
        text,
        file,
        error,
        pending,
        conflicts,
    };
    let read = service.clone();
    rsx! {div {class:"theme-dialog",role:"dialog","aria-label":"Add theme",h3 {"Add theme"}p {"Choose or paste T3 Code or VS Code theme JSON files. Multiple files install together without changing your current theme."}
        input {r#type:"file",accept:".json,application/json",multiple:true,"aria-label":"Choose theme files",disabled:*pending.read(),onchange:move|event|{
            let files=event.files();if files.is_empty(){return;}pending.set(true);let read=read.clone();spawn(async move{read_theme_files(read,files,state,onclose).await;});
        }}
        if let Some(name)=file.read().as_ref(){small {"{name}"}}
        textarea {"aria-label":"Theme JSON",value:text.read().clone(),oninput:move |event|text.set(event.value())}
        if let Some(cause)=error.read().as_ref(){p {role:"alert",class:"error-banner","{cause}"}}
        if let Some(themes)=conflicts.read().clone(){
            p {"Some themes are already installed. Update them or add separate copies."}
            for theme in &themes {p {"{theme.label}"}}
            for copy in [false,true]{
                button {"aria-label":if copy{"Add a copy"}else{"Update theme"},disabled:*pending.read(),onclick:{let service=service.clone();let themes=themes.clone();move |_|{
                    let preferred=if themes.len()==1{file.peek().as_deref().map(import::preferred_name)}else{None};
                    let save=service.import_many(themes.clone(),if copy{crate::themes::ImportMode::Copy}else{crate::themes::ImportMode::Update},preferred);
                    pending.set(true);
                    spawn(async move{
                        let result=save.await;pending.set(false);conflicts.set(None);
                        match result{Err(cause)=>error.set(Some(cause)),Ok(result)=>{
                            let failures=result["failures"].as_array().unwrap();
                            if failures.is_empty(){onclose.call(())}else{error.set(Some(failures.iter().map(|v|v.as_str().unwrap()).collect::<Vec<_>>().join(" — ")))}
                        }}
                    });
                }},if copy{"Add a copy"}else{"Update theme"}}
            }
        } else {button {"aria-label":"Import theme",disabled:*pending.read(),onclick:move |_|{
            let candidate=import::parse(&service.catalog.peek(),&text.peek());
            match candidate {Err(cause)=>error.set(Some(cause)),Ok(theme)=>{
                if service.catalog.peek().custom.iter().any(|t|t.id==theme.id){error.set(None);conflicts.set(Some(vec![theme]));return;}
                let save=service.import_theme(theme);pending.set(true);
                spawn(async move{let result=save.await;pending.set(false);match result{Err(cause)=>error.set(Some(cause)),Ok(_)=>onclose.call(())}});
            }}
        },"Add theme"}}button {disabled:*pending.read(),onclick:move |_|onclose.call(()),"Cancel"}
    }}
}
#[component]
fn ThemeEditor(edit: Editing, onclose: EventHandler<()>) -> Element {
    let service = use_context::<Themes>();
    let catalog = service.catalog.read().clone();
    let mut name = use_signal(|| edit.theme.label.clone());
    let mut mode = use_signal(|| {
        if edit.theme.colors(edit.initial_appearance).is_some() {
            edit.initial_appearance
        } else {
            edit.theme.appearance
        }
    });
    let mut advanced = use_signal(|| edit.theme.managed != Some(true));
    let mut regenerate = use_signal(|| edit.theme.managed != Some(true));
    let mut dirty = use_signal(std::collections::BTreeSet::<Appearance>::new);
    let mut colors = use_signal(|| {
        [
            (
                Appearance::Light,
                edit.theme
                    .colors(Appearance::Light)
                    .cloned()
                    .unwrap_or_else(|| catalog.data.standard[&Appearance::Light].clone()),
            ),
            (
                Appearance::Dark,
                edit.theme
                    .colors(Appearance::Dark)
                    .cloned()
                    .unwrap_or_else(|| catalog.data.standard[&Appearance::Dark].clone()),
            ),
        ]
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>()
    });
    let mut error = use_signal(|| None::<String>);
    let mut pending = use_signal(|| false);
    let merge_service = service.clone();
    let editing_id = edit.existing.then(|| edit.theme.id.clone());
    use_effect(move || {
        if editing_id.is_none() {
            let catalog = merge_service.catalog.read();
            if let Some(target) =
                t3_client::themes::editor::merge_target(&catalog, &name.read(), None)
            {
                let taken = target.modes();
                if taken.len() == 1 && taken.contains(&*mode.peek()) {
                    mode.set(if taken[0] == Appearance::Light {
                        Appearance::Dark
                    } else {
                        Appearance::Light
                    });
                }
            }
        }
    });
    let preview = service.clone();
    use_effect(move || preview.preview(Some((colors.read()[&*mode.read()].clone(), *mode.read()))));
    let restore = service.clone();
    use_drop(move || restore.preview(None));
    let active = *mode.read();
    let current = colors.read()[&active].clone();
    let roles = if *advanced.read() {
        catalog.data.roles.clone()
    } else {
        vec!["canvas".into(), "accent".into()]
    };
    let original = edit.theme.clone();
    let target = t3_client::themes::editor::merge_target(
        &catalog,
        &name.read(),
        edit.existing.then_some(original.id.as_str()),
    )
    .cloned();
    let advanced_service = service.clone();
    let editable = if edit.existing {
        original.modes()
    } else {
        vec![]
    };
    rsx! {div {class:"theme-editor",role:"dialog","aria-label":"Theme editor",
        header {h3 {if edit.existing {"Edit theme"}else{"Create theme"}}button {disabled:*pending.read(),onclick:move |_|onclose.call(()),"Close"}}
        label {"Theme name"}input {"aria-label":"Theme name",value:name.read().clone(),oninput:move |event|name.set(event.value())}
        div {class:"theme-editor-modes",for appearance in [Appearance::Light,Appearance::Dark]{button {disabled:(edit.existing&&original.colors(appearance).is_none())||(!edit.existing&&target.as_ref().is_some_and(|t|t.colors(appearance).is_some())),"aria-pressed":*mode.read()==appearance,onclick:move |_|mode.set(appearance),"{appearance.key()}"}}}
        label {input {r#type:"checkbox",checked:*advanced.read(),onchange:move |event|{
            let checked=event.checked();advanced.set(checked);
            if !checked&&*regenerate.peek(){
                let modes=if editable.len()>1{editable.clone()}else{vec![*mode.peek()]};
                let catalog=advanced_service.catalog.peek();
                for mode in modes {let next=managed(&catalog,mode,&colors.peek()[&mode]);colors.write().insert(mode,next);dirty.write().insert(mode);}
                regenerate.set(false);
            }
        }}"Advanced"}
        div {class:"theme-editor-colors",for role in roles {
            {let value=current[&role].clone();let label=role.clone();let color_service=service.clone();rsx!{label {key:"{role}","{label}",input {"aria-label":"Theme color {label}",value,oninput:move |event|{
                let active=*mode.peek();let catalog=color_service.catalog.peek();let current=colors.peek()[&active].clone();let value=event.value();
                let next=if *advanced.peek(){regenerate.set(true);vivid::update(active,&current,&role,&value)}else{let mut seeds=current;seeds.insert(role.clone(),value.clone());if t3_client::themes::color::canonical(&value).is_some(){dirty.write().insert(active);managed(&catalog,active,&seeds)}else{seeds}};
                colors.write().insert(active,next);
            }}}}}
        }}
        if let Some(cause)=error.read().as_ref(){p {role:"alert",class:"error-banner","{cause}"}}
        button {"aria-label":"Save theme",disabled:*pending.read(),onclick:move |_|{
            if !*advanced.peek(){let modes=dirty.peek().clone();let catalog=service.catalog.peek();for mode in modes{let next=managed(&catalog,mode,&colors.peek()[&mode]);colors.write().insert(mode,next);}}
            let draft=t3_client::themes::editor::Draft{editing_id:edit.existing.then(||original.id.clone()),name:name.peek().clone(),appearance:*mode.peek(),colors:colors.peek().clone(),advanced:*advanced.peek()};
            let save=service.save_editor(draft);pending.set(true);
            spawn(async move{let result=save.await;pending.set(false);match result{Err(cause)=>error.set(Some(cause)),Ok(_)=>onclose.call(())}});
        },"Save theme"}
    }}
}

fn managed(catalog: &t3_client::themes::Catalog, mode: Appearance, colors: &Colors) -> Colors {
    let valid = |role: &str| {
        colors
            .get(role)
            .filter(|value| t3_client::themes::color::canonical(value).is_some())
            .unwrap_or(&catalog.data.standard[&mode][role])
    };
    vivid::create(catalog, mode, valid("canvas"), valid("accent"))
}
