//! Scoped Open VSX search/download; committed collections use the App storage actor.
use crate::{
    theme_download::Source,
    themes::{LibraryAction, Themes},
};
use dioxus::dioxus_core::Task;
use dioxus::prelude::*;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use t3_client::themes::{
    library,
    openvsx::{self, Extension},
};
#[derive(Clone)]
struct Update {
    extension: Extension,
}
#[derive(Clone)]
struct Controls {
    loading: Signal<bool>,
    pending: Signal<bool>,
    error: Signal<Option<String>>,
    update: Signal<Option<Update>>,
    search_task: Signal<Option<Task>>,
    epoch: Rc<Cell<u64>>,
}
fn install(
    service: Themes,
    source: Source,
    extension: Extension,
    mut controls: Controls,
    onclose: EventHandler<()>,
    allow_update: bool,
) {
    if *controls.pending.peek() {
        return;
    }
    if let Some(task) = controls.search_task.write().take() {
        task.cancel();
    }
    controls.epoch.set(controls.epoch.get() + 1);
    controls.loading.set(false);
    controls.pending.set(true);
    controls.error.set(None);
    spawn(async move {
        let outcome = async {
            let previous = service.collection(extension.collection_id.clone()).await?;
            if !previous.is_empty() && !allow_update {
                controls.update.set(Some(Update {
                    extension: extension.clone(),
                }));
                return Ok(false);
            }
            let catalog = service.catalog.peek().clone();
            let themes = source.package(&catalog, &extension).await?;
            let themes = themes
                .iter()
                .map(|t| library::definition_value(&catalog, t))
                .collect();
            // Accepted writes are queued synchronously to the persistent actor;
            // closing the dialog cannot cancel a storage receipt already queued.
            service
                .library_change(LibraryAction::Replace {
                    id: extension.collection_id,
                    themes,
                    expected: Some(previous),
                })
                .await?;
            Ok::<_, String>(true)
        }
        .await;
        controls.pending.set(false);
        match outcome {
            Ok(true) => onclose.call(()),
            Ok(false) => {}
            Err(cause) => controls.error.set(Some(cause)),
        }
    });
}
#[component]
pub fn ThemeSearch(onclose: EventHandler<()>) -> Element {
    let service = use_context::<Themes>();
    let source = use_hook(|| try_consume_context::<Source>().unwrap_or_default());
    let mut query = use_signal(String::new);
    let mut retry = use_signal(|| 0_u64);
    let mut sort = use_signal(|| "downloadCount".to_owned());
    let mut results = use_signal(|| None::<Vec<Extension>>);
    let mut loading = use_signal(|| false);
    let pending = use_signal(|| false);
    let error = use_signal(|| None::<String>);
    let update = use_signal(|| None::<Update>);
    let search_task = use_signal(|| None::<Task>);
    let epoch = use_hook(|| Rc::new(Cell::new(0)));
    let mut controls = Controls {
        loading,
        pending,
        error,
        update,
        search_task,
        epoch: epoch.clone(),
    };
    let last = use_hook(|| Rc::new(RefCell::new(None::<String>)));
    let keyboard_last = last.clone();
    let effect_source = source.clone();
    let mut effect_controls = controls.clone();
    use_effect(move || {
        let immediate = *retry.read() != 0;
        let query = openvsx::search_text(&query.read()).to_owned();
        let sort = sort.read().clone();
        if *effect_controls.pending.read() {
            return;
        }
        let key = format!("{query}\0{sort}");
        if last.borrow().as_ref() == Some(&key) {
            return;
        }
        *last.borrow_mut() = Some(key);
        if let Some(task) = effect_controls.search_task.write().take() {
            task.cancel();
        }
        effect_controls.epoch.set(effect_controls.epoch.get() + 1);
        let generation = effect_controls.epoch.get();
        effect_controls.error.set(None);
        results.set(None);
        if query.is_empty() {
            loading.set(false);
            return;
        }
        loading.set(true);
        let source = effect_source.clone();
        let mut controls = effect_controls.clone();
        let task = spawn(async move {
            if !immediate {
                #[cfg(target_arch = "wasm32")]
                gloo_timers::future::TimeoutFuture::new(350).await;
                #[cfg(not(target_arch = "wasm32"))]
                tokio::time::sleep(std::time::Duration::from_millis(350)).await;
            }
            let result = source.search(&query, &sort).await;
            if controls.epoch.get() != generation {
                return;
            }
            loading.set(false);
            match result {
                Ok(items) => results.set(Some(items)),
                Err(cause) => controls.error.set(Some(cause)),
            }
        });
        effect_controls.search_task.set(Some(task));
    });
    let status = if *pending.read() {
        "Downloading theme extension…"
    } else if *loading.read() {
        "Searching Open VSX…"
    } else {
        ""
    };
    rsx! {section {class:"theme-search","aria-label":"Open VSX themes",h4 {"Search community themes"}
        p {"Find open-source themes from Open VSX."}
        input {"aria-label":"Search Open VSX themes",placeholder:"Search themes",value:query.read().clone(),oninput:move|event|{retry.set(0);query.set(event.value());},onkeydown:move|event|{if event.key()==Key::Enter&&!event.is_composing()&&!*pending.peek()&&!*loading.peek(){event.prevent_default();*keyboard_last.borrow_mut()=None;let next=*retry.peek()+1;retry.set(next);}}}
        label {"Sort",select {"aria-label":"Sort Open VSX themes",value:sort.read().clone(),onchange:move|event|sort.set(event.value()),
            for (value,label) in [("downloadCount","Most downloaded"),("rating","Best rated"),("timestamp","Newest"),("relevance","Most relevant")]{option{value,selected:sort.read().as_str()==value,"{label}"}}
        }}
        if !status.is_empty(){p {role:"status","{status}"}}
        if let Some(cause)=error.read().as_ref(){p {role:"alert",class:"error-banner","{cause}"}}
        if let Some(items)=results.read().clone(){
            if items.is_empty(){p {"No matching theme extensions found."}}
            for extension in items {{let installed=service.catalog.read().custom.iter().any(|t|t.collection.as_ref().and_then(|v|v.get("id")).and_then(serde_json::Value::as_str)==Some(&extension.collection_id));let action=if installed{"Update"}else{"Install"};rsx!{article {key:"{extension.id}",class:"theme-search-result",if let Some(icon)=extension.icon_url.as_ref(){img {src:icon.clone(),alt:"",width:32,height:32,loading:"lazy"}}h5 {"{extension.name}"}small {"{extension.publisher} · {extension.license} · {extension.download_count} downloads"}p {"{extension.description}"}
                if let Some(url)=extension.source_url.as_ref(){a {href:url.clone(),target:"_blank",rel:"noopener noreferrer","Source"}}
                button {"aria-label":format!("{} {}",action,extension.name),disabled:*pending.read(),onclick:{let service=service.clone();let source=source.clone();let controls=controls.clone();move |_|install(service.clone(),source.clone(),extension.clone(),controls.clone(),onclose,false)},"{action}"}
            }}}}
        }
        if let Some(next)=update.read().clone(){div {role:"alertdialog","aria-label":"Update theme collection",h4 {"Update installed collection?"}p {"This replaces the installed themes from this extension. Themes removed by its publisher will also be removed."}
            button {"aria-label":"Update collection",disabled:*pending.read(),onclick:{let service=service.clone();let source=source.clone();let controls=controls.clone();move |_|install(service.clone(),source.clone(),next.extension.clone(),controls.clone(),onclose,true)},"Update collection"}
            button {"aria-label":"Cancel update",disabled:*pending.read(),onclick:move |_|controls.update.set(None),"Cancel update"}
        }}
    }}
}
