//! Source typography controls. Font measurements live in the shared Rust WASM utility.
use crate::{
    client_settings::Writer,
    font_service::{Enumeration, Fonts},
    runtime::{UiModel, UiModelStoreExt},
};
use dioxus::dioxus_core::Task;
use dioxus::prelude::*;
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
const ADVANCED: &str = "t3code:typography-advanced";
fn trim(value: &str) -> &str {
    value.trim_matches(|ch|matches!(ch,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Interface,
    Prompt,
    Code,
    Terminal,
}
impl Kind {
    fn keys(self) -> (&'static str, &'static str) {
        match self {
            Self::Interface => ("fontFamilySans", "fontSizeInterface"),
            Self::Prompt => ("fontFamilyComposer", "fontSizePrompt"),
            Self::Code => ("fontFamilyCode", "fontSizeCode"),
            Self::Terminal => ("fontFamilyTerminal", "fontSizeTerminal"),
        }
    }
    fn size(self) -> (i64, i64, i64) {
        match self {
            Self::Interface => (12, 20, 16),
            Self::Prompt => (12, 20, 14),
            Self::Code => (10, 18, 13),
            Self::Terminal => (8, 20, 12),
        }
    }
    fn mono(self) -> bool {
        matches!(self, Self::Code | Self::Terminal)
    }
    fn family(self, state: Store<UiModel>) -> String {
        let settings = state.client_settings();
        let settings = settings.peek();
        match self {
            Self::Interface => &settings.font_family_sans.0,
            Self::Prompt => &settings.font_family_composer.0,
            Self::Code => &settings.font_family_code.0,
            Self::Terminal => &settings.font_family_terminal.0,
        }
        .clone()
    }
}
#[component]
pub fn Typography(state: Store<UiModel>) -> Element {
    let fonts = use_context::<Fonts>();
    let writer = use_context::<Writer>();
    let mut advanced = use_signal(|| false);
    let mut listener = use_signal(|| None::<document::Eval>);
    use_future(move || async move {
        let mut eval = document::eval(
            "const key='t3code:typography-advanced';const read=()=>{try{dioxus.send(window.localStorage.getItem(key));}catch{dioxus.send(null);}};const storage=e=>{if(e.key===key)read();};const local=e=>{if(e.detail?.key===key)read();};window.addEventListener('storage',storage);window.addEventListener('t3code:local_storage_change',local);read();try{await dioxus.recv();}finally{window.removeEventListener('storage',storage);window.removeEventListener('t3code:local_storage_change',local);}",
        );
        listener.set(Some(eval));
        while let Ok(raw) = eval.recv::<Option<String>>().await {
            advanced.set(
                raw.as_deref()
                    .and_then(|raw| serde_json::from_str::<bool>(raw).ok())
                    .unwrap_or(false),
            );
        }
    });
    use_drop(move || {
        if let Some(eval) = *listener.peek() {
            let _ = eval.send(json!({"type":"dispose"}));
        }
    });
    let settings = state.client_settings();
    let settings = settings.read();
    let is_advanced = *advanced.read();
    let wrap = settings.word_wrap;
    let smoothing = settings.font_smoothing;
    let change_fonts = fonts.clone();
    let wrap_writer = writer.clone();
    let smoothing_writer = writer;
    rsx! {
        section{class:"typography",h2{"Typography"}
            label{class:"typography-advanced",input{r#type:"checkbox","aria-label":"Show advanced typography settings",checked:is_advanced,onchange:move|event|{
                let value=event.checked();advanced.set(value);let service=change_fonts.clone();let mut errors=service.clone();service.spawn(async move{
                    let args=json!({"key":ADVANCED,"value":value.to_string()});let result=document::eval(&format!("window.localStorage.setItem({args}.key,{args}.value);window.dispatchEvent(new CustomEvent('t3code:local_storage_change',{{detail:{{key:{args}.key}}}}));return true;")).join::<bool>().await;
                    if let Err(cause)=result{errors.error.set(Some(format!("Could not save advanced typography: {cause}")));}
                });
            }}"Advanced"}
            if let Some(error)=fonts.error.read().as_ref(){p{class:"error-banner",role:"alert","{error}"}}
            if !*fonts.ready.read(){p{class:"muted","Loading font controls…"}}
            FontRow{state,kind:Kind::Interface,title:"Interface font",description:"Everything outside code blocks and the terminal."}
            if is_advanced{FontRow{state,kind:Kind::Prompt,title:"Prompt font",description:"Only the box you write prompts in. Mono works well here."}}
            FontRow{state,kind:Kind::Code,title:if is_advanced{"Code font"}else{"Monospace font"},description:if is_advanced{"Code blocks, diffs, and file previews."}else{"Code blocks, diffs, file previews, and the terminal."}}
            if is_advanced{FontRow{state,kind:Kind::Terminal,title:"Terminal font",description:"Terminal output, independent from code blocks and diffs."}
                TerminalPreview{family:settings.font_family_terminal.0.clone(),size:settings.font_size_terminal.0}
                if fonts.defaults.read()["mac"]==true{div{class:"font-setting",div{strong{"Font smoothing"}p{class:"muted","Use thinner grayscale text smoothing instead of the macOS default."}}input{r#type:"checkbox","aria-label":"Font smoothing",checked:smoothing,onchange:move|event|smoothing_writer.patch(state,json!({"fontSmoothing":event.checked()}))}}}
            }
            if !is_advanced{TerminalPreview{family:settings.font_family_code.0.clone(),size:settings.font_size_code.0}}
            div{class:"font-setting",div{strong{"Word wrap"}p{class:"muted","Wrap long lines in code blocks, tables, diffs, and file previews by default."}}input{r#type:"checkbox","aria-label":"Wrap code, tables, diffs, and file previews by default",checked:wrap,onchange:move|event|wrap_writer.patch(state,json!({"wordWrap":event.checked()}))}}
        }
    }
}
/// A blur commit belongs to the persistent App rather than the departing row.
/// Epoch/base checks prevent delayed measurements from overwriting reset/external edits.
fn commit(
    fonts: &Fonts,
    writer: Writer,
    state: Store<UiModel>,
    kind: Kind,
    candidate: String,
    base: String,
    epoch: Rc<Cell<u64>>,
    ticket: u64,
    picker: bool,
) {
    let mut service = fonts.clone();
    fonts.commit(async move{
        let accepted=if trim(&candidate).is_empty(){true}else{
            match service.request(json!({"type":"probe","family":candidate})).await{
                Ok(value)=>value["available"]==true&&(!kind.mono()||value["monospace"]==true),
                Err(error)=>{if epoch.get()==ticket{service.error.set(Some(error));}return;}
            }
        };
        if epoch.get()!=ticket||kind.family(state)!=base{return;}
        if accepted{writer.patch(state,json!({(kind.keys().0):candidate}));}
        else if picker{service.error.set(Some(format!("\"{candidate}\" isn't monospace. Code and terminal need a fixed-width font, so the current font was kept.")));}
    });
}
#[component]
fn FontRow(state: Store<UiModel>, kind: Kind, title: String, description: String) -> Element {
    let fonts = use_context::<Fonts>();
    let writer = use_context::<Writer>();
    let settings = state.client_settings();
    let settings = settings.read();
    let (value, size) = match kind {
        Kind::Interface => (
            settings.font_family_sans.0.clone(),
            settings.font_size_interface.0,
        ),
        Kind::Prompt => (
            settings.font_family_composer.0.clone(),
            settings.font_size_prompt.0,
        ),
        Kind::Code => (
            settings.font_family_code.0.clone(),
            settings.font_size_code.0,
        ),
        Kind::Terminal => (
            settings.font_family_terminal.0.clone(),
            settings.font_size_terminal.0,
        ),
    };
    let mut draft = use_signal(|| value.clone());
    let mut settled = use_signal(|| true);
    let mut timer = use_signal(|| None::<Task>);
    let mut focused = use_signal(|| false);
    let epoch = use_hook(|| Rc::new(Cell::new(0)));
    let previous = use_hook(|| Rc::new(RefCell::new(value.clone())));
    if *previous.borrow() != value {
        *previous.borrow_mut() = value.clone();
        epoch.set(epoch.get() + 1);
        if let Some(task) = timer.write().take() {
            task.cancel();
        }
        draft.set(value.clone());
        settled.set(true);
    }
    let defaults = fonts.defaults.read();
    let default = match kind {
        Kind::Interface => defaults["sans"]
            .as_str()
            .unwrap_or("System default")
            .to_owned(),
        Kind::Prompt => {
            let custom = trim(&settings.font_family_sans.0);
            if custom.is_empty() {
                defaults["sans"].as_str().unwrap_or("System default").into()
            } else {
                custom.into()
            }
        }
        _ => defaults["code"]
            .as_str()
            .unwrap_or("System monospace")
            .into(),
    };
    let invalid =
        *settled.read() && !trim(&draft.read()).is_empty() && trim(&draft.read()) != trim(&value);
    let reset_writer = writer.clone();
    let reset_epoch = epoch.clone();
    let (min, max, default_size) = kind.size();
    let family_label = format!("{title} family");
    let size_label = match kind {
        Kind::Interface => "Interface font size",
        Kind::Prompt => "Prompt font size",
        Kind::Code => "Code font size",
        Kind::Terminal => "Terminal font size",
    };
    let input_fonts = fonts.clone();
    let input_writer = writer.clone();
    let input_epoch = epoch.clone();
    let input_base = value.clone();
    let blur_fonts = fonts.clone();
    let blur_writer = writer.clone();
    let blur_epoch = epoch.clone();
    let blur_base = value.clone();
    let key_fonts = fonts.clone();
    let key_writer = writer.clone();
    let key_epoch = epoch.clone();
    let key_base = value.clone();
    let focus_fonts = fonts.clone();
    let size_writer = writer;
    let ready = *fonts.ready.read();
    rsx! {div{class:"font-setting",div{class:"font-description",strong{"{title}"}p{class:"muted","{description}"}}
        div{class:"font-controls",
            if matches!(&*fonts.enumeration.read(),Enumeration::Granted(_)){
                FontPicker{state,kind,title:title.clone(),default_family:default.clone(),selected:value.clone(),initial_open:*focused.peek(),epoch:epoch.clone()}
            }else{
                input{r#type:"text","aria-label":family_label,"aria-invalid":invalid,disabled:!ready,autocapitalize:"off",autocomplete:"off",maxlength:200,spellcheck:false,placeholder:default,value:draft.read().clone(),
                    onfocus:move|_|{focused.set(true);focus_fonts.discover();},
                    oninput:move|event|{
                        let candidate=event.value();draft.set(candidate.clone());settled.set(false);if let Some(task)=timer.write().take(){task.cancel();}
                        input_epoch.set(input_epoch.get()+1);let ticket=input_epoch.get();let epoch=input_epoch.clone();let service=input_fonts.clone();let writer=input_writer.clone();let base=input_base.clone();
                        timer.set(Some(spawn(async move{crate::font_service::delay(400).await;timer.set(None);settled.set(true);commit(&service,writer,state,kind,candidate,base,epoch,ticket,false);})));},
                    onblur:move|_|{focused.set(false);if let Some(task)=timer.write().take(){task.cancel();settled.set(true);commit(&blur_fonts,blur_writer.clone(),state,kind,draft.peek().clone(),blur_base.clone(),blur_epoch.clone(),blur_epoch.get(),false);}},
                    onkeydown:move|event|{if event.key()==Key::Enter{if let Some(task)=timer.write().take(){task.cancel();settled.set(true);commit(&key_fonts,key_writer.clone(),state,kind,draft.peek().clone(),key_base.clone(),key_epoch.clone(),key_epoch.get(),false);}}
                        if event.key()==Key::Escape{event.prevent_default();event.stop_propagation();key_epoch.set(key_epoch.get()+1);if let Some(task)=timer.write().take(){task.cancel();}draft.set(key_base.clone());settled.set(true);}}
                }
            }
            select{"aria-label":size_label,value:size.to_string(),onchange:move|event|{if let Ok(size)=event.value().parse::<i64>(){if (min..=max).contains(&size){size_writer.patch(state,json!({(kind.keys().1):size}));}}},for px in min..=max{option{value:px.to_string(),selected:px==size,"{px} px"}}}
            if !value.is_empty()||size!=default_size{button{class:"subtle","aria-label":format!("Reset {}",title.to_lowercase()),onclick:move|_|{reset_epoch.set(reset_epoch.get()+1);if let Some(task)=timer.write().take(){task.cancel();}draft.set(String::new());settled.set(true);reset_writer.patch(state,json!({(kind.keys().0):"",(kind.keys().1):default_size}));},"Reset"}}
        }
        if invalid{p{class:"font-invalid",role:"status","Font unavailable or not fixed-width; the current font was kept."}}
    }}
}
#[component]
fn FontPicker(
    state: Store<UiModel>,
    kind: Kind,
    title: String,
    default_family: String,
    selected: String,
    initial_open: bool,
    epoch: Rc<Cell<u64>>,
) -> Element {
    let fonts = use_context::<Fonts>();
    let writer = use_context::<Writer>();
    let mut open = use_signal(|| initial_open);
    let mut query = use_signal(String::new);
    let mut scroll = use_signal(|| 0_f64);
    let mut highlight = use_signal(|| 0_usize);
    let enumeration = fonts.enumeration.read();
    let families = match &*enumeration {
        Enumeration::Granted(families) => families,
        _ => return rsx! {},
    };
    let search = trim(&query.read()).to_lowercase();
    let mut indices = Vec::new();
    if search.is_empty() {
        indices.push(None);
    }
    indices.extend(
        families
            .iter()
            .enumerate()
            .filter(|(_, family)| search.is_empty() || family.to_lowercase().contains(&search))
            .map(|(index, _)| Some(index)),
    );
    let first = (((*scroll.read() / 30.).floor() as usize).saturating_sub(4)).min(indices.len());
    let end = (first + 22).min(indices.len());
    let total = indices.len() * 30;
    let highlighted = (*highlight.read()).min(indices.len().saturating_sub(1));
    let key_candidate = indices.get(highlighted).map(|index| {
        index
            .map(|index| families[index].clone())
            .unwrap_or_default()
    });
    let key_fonts = fonts.clone();
    let key_writer = writer.clone();
    let key_epoch = epoch.clone();
    let key_base = selected.clone();
    let count = indices.len();
    let key_title = title.clone();
    let trigger = if trim(&selected).is_empty() {
        default_family.clone()
    } else {
        selected.clone()
    };
    rsx! {div{class:"font-picker",button{r#type:"button","aria-label":format!("{title} family"),"aria-haspopup":"listbox","aria-expanded":*open.read(),onclick:move|_|{let next=!*open.peek();open.set(next);if next{query.set(String::new());scroll.set(0.);highlight.set(0);}},"{trigger} ▾"}
        if *open.read(){div{class:"font-popup",input{r#type:"search","aria-label":format!("Search {title} families"),placeholder:"Search fonts…",value:query.read().clone(),autofocus:true,oninput:move|event|{query.set(event.value());scroll.set(0.);highlight.set(0);},onkeydown:move|event|{
            if event.key()==Key::Escape{event.prevent_default();event.stop_propagation();open.set(false);}
            if event.key()==Key::ArrowDown||event.key()==Key::ArrowUp{event.prevent_default();let index=if event.key()==Key::ArrowDown{(highlighted+1).min(count.saturating_sub(1))}else{highlighted.saturating_sub(1)};highlight.set(index);let top=*scroll.peek();let target=if index as f64*30.<top{index as f64*30.}else if (index+1) as f64*30.>top+288.{((index+1)as f64*30.-288.).max(0.)}else{top};scroll.set(target);let args=json!({"label":format!("{key_title} font list"),"top":target});document::eval(&format!("document.querySelector('[aria-label='+JSON.stringify({args}.label)+']')?.scrollTo({{top:{args}.top}});"));}
            if event.key()==Key::Enter{event.prevent_default();if let Some(candidate)=key_candidate.clone(){open.set(false);key_epoch.set(key_epoch.get()+1);commit(&key_fonts,key_writer.clone(),state,kind,candidate,key_base.clone(),key_epoch.clone(),key_epoch.get(),true);}}
        }}
            div{class:"font-list",role:"listbox","aria-label":format!("{title} font list"),onscroll:move|event|scroll.set(event.data().scroll_top() as f64),
                div{style:"height:{total}px;position:relative;",
                    for position in first..end{ {let candidate=indices[position].map(|index|families[index].clone()).unwrap_or_default();let label=if candidate.is_empty(){format!("{default_family} (default)")}else{candidate.clone()};let is_selected=trim(&candidate)==trim(&selected);let service=fonts.clone();let writer=writer.clone();let epoch=epoch.clone();let base=selected.clone();rsx!{button{key:"{position}",class:if position==highlighted{"font-choice highlighted"}else{"font-choice"},role:"option","aria-selected":is_selected,style:"position:absolute;top:{position*30}px;height:30px;width:100%;",onclick:move|_|{open.set(false);epoch.set(epoch.get()+1);commit(&service,writer.clone(),state,kind,candidate.clone(),base.clone(),epoch.clone(),epoch.get(),true);},"{label}",if is_selected{" ✓"}}}} }
                }
                if indices.is_empty(){p{"No fonts found."}}
            }
        }}
    }}
}

#[component]
fn TerminalPreview(family: String, size: i64) -> Element {
    let id = use_hook(|| format!("font-preview-{}", uuid::Uuid::new_v4()));
    let launch_id = id.clone();
    let (start, receive) = use_hook(|| {
        let (tx, rx) = futures_channel::oneshot::channel();
        (
            Rc::new(RefCell::new(Some(tx))),
            Rc::new(RefCell::new(Some(rx))),
        )
    });
    let mut renderer = use_signal(|| None::<document::Eval>);
    let mut ready = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let current_font = use_signal(|| (family.clone(), size));
    let mut current_font = current_font;
    if *current_font.peek() != (family.clone(), size) {
        current_font.set((family.clone(), size));
    }
    use_future(move || {
        let launch_id = launch_id.clone();
        let receive = receive.borrow_mut().take().unwrap();
        async move {
            if receive.await.is_err() {
                return;
            }
            let base = crate::terminal_pane::SURFACE.to_string();
            let font = current_font.peek();
            let args = json!({"id":launch_id,"base":base,"wasm":format!("{base}/t3_terminal_bg.wasm"),"options":{"fontFamily":trim(&font.0),"fontSize":font.1,"tabNavigates":true}});
            drop(font);
            let mut bridge = document::eval(&format!(
                "const args={args};\n{}",
                include_str!("../assets/terminal_abi.js")
            ));
            renderer.set(Some(bridge));
            let preview: serde_json::Value =
                serde_json::from_str(include_str!("../assets/terminal-preview.json")).unwrap();
            let mut echo = t3_client::terminal_preview::Echo::default();
            loop {
                let event: serde_json::Value = match bridge.recv().await {
                    Ok(event) => event,
                    Err(cause) => {
                        error.set(Some(format!("Font preview disconnected: {cause}")));
                        break;
                    }
                };
                match event["type"].as_str() {
                    Some("ready") => {
                        ready.set(true);
                        let font = current_font.peek();
                        let _ = bridge.send(json!({"type":"font","family":font.0,"size":font.1}));
                        let _ = bridge.send(json!({"type":"append","data":preview["transcript"]}));
                    }
                    Some("write") => {
                        if let Some(data) = event["data"].as_str() {
                            if let Some(data) =
                                echo.write(data, preview["prompt"].as_str().unwrap())
                            {
                                let _ = bridge.send(json!({"type":"append","data":data}));
                            }
                        }
                    }
                    Some("error") => {
                        error.set(Some(
                            event["message"]
                                .as_str()
                                .unwrap_or("Font preview failed.")
                                .into(),
                        ));
                        break;
                    }
                    _ => {}
                }
            }
            ready.set(false);
            renderer.set(None);
            let _ = bridge.send(json!({"type":"dispose"}));
        }
    });
    use_effect(move || {
        let font = current_font.read();
        if *ready.read() {
            if let Some(bridge) = *renderer.read() {
                let _ = bridge.send(json!({"type":"font","family":font.0,"size":font.1}));
            }
        }
    });
    let dispose_id = id.clone();
    use_drop(move || {
        if let Some(bridge) = *renderer.peek() {
            let _ = bridge.send(json!({"type":"dispose"}));
        }
        let args = json!({"id":dispose_id});
        document::eval(&format!(
            "const slot=window.__t3RustTerminals?.get({args}.id);if(slot){{slot.disposed=true;slot.surface?.dispose();}}"
        ));
    });
    rsx! {div{class:"font-terminal-preview",id,"aria-label":"Terminal font preview",onmounted:move|_|{if let Some(start)=start.borrow_mut().take(){let _=start.send(());}}}if let Some(error)=error.read().as_ref(){p{role:"alert","{error}"}}}
}
