use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    terminal_stream::{MetadataStream, SessionStream},
};
use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use t3_client::{
    connection::ConnectionStatus,
    terminal_output::{self, INITIAL_CURSOR, OutputUpdate},
    terminal_session::SessionStatus,
    terminal_ui::{Direction, ScopedPanes},
};
use t3_contracts::{AuthEnvironmentScope, TerminalAttachInput};
// Generate this directory with tools/build_terminal_surface.sh before UI builds.
// All WebViews use the same Rust surface; the JS file is wasm-bindgen ABI glue.
const SURFACE: Asset = asset!("/assets/terminal-surface");
#[derive(Clone)]
pub struct Catalog {
    pub panes: Signal<ScopedPanes>,
    pub error: Signal<Option<String>>,
    hydrated: Signal<bool>,
    write_allowed: Signal<bool>,
    writer: futures_channel::mpsc::UnboundedSender<String>,
}
pub fn use_catalog() -> Catalog {
    let panes = use_signal(ScopedPanes::default);
    let mut error = use_signal(|| None);
    let hydrated = use_signal(|| false);
    let write_allowed = use_signal(|| true);
    let (sender, receiver) = use_hook(|| {
        let (sender, receiver) = futures_channel::mpsc::unbounded::<String>();
        (sender, Rc::new(RefCell::new(Some(receiver))))
    });
    use_future(move || {
        let mut receiver = receiver
            .borrow_mut()
            .take()
            .expect("terminal writer mounts once");
        async move {
            while let Some(bytes) = receiver.next().await {
                match write_storage(bytes).await {
                    Ok(()) => error.set(None),
                    Err(cause) => error.set(Some(cause)),
                }
            }
        }
    });
    let catalog = Catalog {
        panes,
        error,
        hydrated,
        write_allowed,
        writer: sender,
    };
    use_context_provider(|| catalog.clone());
    catalog
}
impl Catalog {
    pub async fn hydrate(mut self) {
        let saved = read_storage().await;
        match saved {
            Ok(Some(raw)) => match decode_panes(&raw) {
                Ok(panes) => self.panes.set(panes),
                Err(error) => {
                    self.error.set(Some(error));
                    self.write_allowed.set(false);
                }
            },
            Ok(None) => {}
            Err(error) => {
                self.error.set(Some(error));
                self.write_allowed.set(false);
            }
        }
        self.hydrated.set(true);
    }
    fn change(&self, key: &str, action: impl FnOnce(&mut ScopedPanes, &str)) {
        if !*self.hydrated.peek() {
            return;
        }
        let mut pane_signal = self.panes;
        let mut panes = pane_signal.write();
        action(&mut panes, key);
        if *self.write_allowed.peek() {
            let bytes = json!({"state":&*panes,"version":4}).to_string();
            let _ = self.writer.unbounded_send(bytes);
        }
    }
    fn focus(&self, state: Store<UiModel>, owner: &crate::terminal_bridge::FocusOwner) {
        let model = state.peek();
        let mut next = self.panes.peek().clone();
        if owner.activate(
            &mut next,
            model.destination.as_ref().map(|id| id.as_str()),
            model.active_thread.as_deref(),
        ) {
            let key = ScopedPanes::key(&owner.environment, &owner.thread);
            self.change(&key, |panes, _| *panes = next);
        }
    }
}
fn decode_panes(raw: &str) -> Result<ScopedPanes, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| "Saved terminal layout is malformed; it was preserved.".to_owned())?;
    if value["version"].as_u64().is_some_and(|v| v > 4) {
        return Err("Saved terminal layout uses a newer version; it was preserved.".into());
    }
    let state = value["state"]
        .as_object()
        .ok_or("Saved terminal layout is malformed; it was preserved.")?;
    let entries = state
        .get("terminalUiStateByThreadKey")
        .or_else(|| state.get("terminalStateByThreadKey"))
        .and_then(Value::as_object);
    let mut recovered = ScopedPanes::default();
    if let Some(entries) = entries {
        for (key, value) in entries {
            let Some((environment, thread)) = key.split_once(':') else {
                continue;
            };
            if environment.is_empty() || thread.is_empty() {
                continue;
            }
            let pane = serde_json::from_value(value.clone()).map_err(|_| {
                "Saved terminal layout contains an invalid pane; it was preserved.".to_owned()
            })?;
            recovered.set(key, pane);
        }
    }
    Ok(recovered)
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn source_legacy_key_recovers_groups_and_discards_unscoped_records() {
        let pane = json!({"terminalOpen":true,"terminalHeight":300,"terminalIds":["term-1","term-2"],"activeTerminalId":"term-2"});
        let recovered=decode_panes(&json!({"version":1,"state":{"terminalStateByThreadKey":{"a:thread":pane,"bad":pane,"b:thread":pane}}}).to_string()).unwrap();
        let a = recovered.get("a:thread");
        assert_eq!(a.terminal_groups.len(), 2);
        assert_eq!(a.active_terminal_group_id, "group-term-2");
        assert_eq!(recovered.get("bad"), Default::default());
        assert_eq!(recovered.get("b:thread"), a);
    }
    #[test]
    fn malformed_and_future_envelopes_refuse_writer_hydration() {
        for raw in [
            "not json",
            r#"{"version":5,"state":{}}"#,
            r#"{"version":4,"state":null}"#,
            r#"{"version":4,"state":{"terminalUiStateByThreadKey":{"a:thread":{"terminalIds":false}}}}"#,
        ] {
            assert!(decode_panes(raw).is_err(), "must preserve {raw}");
        }
    }
}
async fn read_storage() -> Result<Option<String>, String> {
    #[cfg(target_arch = "wasm32")]
    {
        let storage = web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .ok_or("Terminal layout storage unavailable.")?;
        storage
            .get_item(t3_client::terminal_ui::STORAGE_KEY)
            .map_err(|_| "Terminal layout could not be read.".into())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        document::eval("return window.localStorage.getItem('t3code:terminal-state:v1');")
            .join()
            .await
            .map_err(|e| format!("Terminal layout could not be read: {e}"))
    }
}
async fn write_storage(bytes: String) -> Result<(), String> {
    #[cfg(target_arch = "wasm32")]
    {
        let storage = web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .ok_or("Terminal layout storage unavailable.")?;
        storage
            .set_item(t3_client::terminal_ui::STORAGE_KEY, &bytes)
            .map_err(|_| "Terminal layout could not be saved.".into())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let args = json!({"data":bytes});
        document::eval(&format!(
            "window.localStorage.setItem('t3code:terminal-state:v1',{args}.data); return true;"
        ))
        .join::<bool>()
        .await
        .map(|_| ())
        .map_err(|e| format!("Terminal layout could not be saved: {e}"))
    }
}
async fn delay(ms: u64) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(ms as u32).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}
#[component]
pub fn Drawer(
    state: Store<UiModel>,
    transport: TransportHandle,
    environment: String,
    thread_id: String,
    cwd: String,
    worktree: Option<String>,
) -> Element {
    let Some(catalog) = try_consume_context::<Catalog>() else {
        return rsx! {};
    };
    let key = ScopedPanes::key(&environment, &thread_id);
    let mut summaries = use_signal(|| None::<Vec<t3_contracts::TerminalSummary>>);
    let metadata_environment = environment.clone();
    let metadata_thread = thread_id.clone();
    let metadata_transport = transport.clone();
    let metadata_key = key.clone();
    let metadata_catalog = catalog.clone();
    use_future(move || {
        let environment = metadata_environment.clone();
        let thread = metadata_thread.clone();
        let transport = metadata_transport.clone();
        let key = metadata_key.clone();
        let catalog = metadata_catalog.clone();
        async move {
            loop {
                if !owns_pane(state, &environment, &thread) {
                    return;
                }
                if *state.status().peek() != ConnectionStatus::Connected || !reads(state) {
                    delay(250).await;
                    continue;
                }
                let mut stream = match MetadataStream::subscribe(&transport, state) {
                    Ok(stream) => stream,
                    Err(_) => {
                        delay(500).await;
                        continue;
                    }
                };
                while let Some(result) = stream.next().await {
                    if !owns_pane(state, &environment, &thread) {
                        return;
                    }
                    let Ok(all) = result else { break };
                    let rows = all
                        .into_iter()
                        .filter(|row| row.thread_id.as_str() == thread)
                        .collect::<Vec<_>>();
                    let ids = rows
                        .iter()
                        .map(|row| row.terminal_id.to_string())
                        .collect::<Vec<_>>();
                    let current = catalog.panes.peek().get(&key);
                    // Source preserves local split/new IDs while the server list lags an open.
                    if current.terminal_ids != ids
                        && !(ids.len() < current.terminal_ids.len()
                            && ids.iter().all(|id| current.terminal_ids.contains(id)))
                    {
                        catalog.change(&key, |panes, key| panes.reconcile(key, &ids));
                    }
                    summaries.set(Some(rows));
                }
                delay(250).await;
            }
        }
    });
    let mut panes = catalog.panes.read().get(&key);
    let connected = *state.status().read() == ConnectionStatus::Connected;
    let can_operate = connected
        && state.destination().read().as_ref().is_some_and(|id| {
            state
                .environments()
                .read()
                .allows(id, AuthEnvironmentScope::TerminalOperate)
        });
    let can_read = connected
        && state.destination().read().as_ref().is_some_and(|id| {
            state
                .environments()
                .read()
                .allows(id, AuthEnvironmentScope::TerminalRead)
        });
    if !can_operate {
        if let Some(rows) = summaries.read().as_ref() {
            panes = panes.reconcile(
                &rows
                    .iter()
                    .map(|row| row.terminal_id.to_string())
                    .collect::<Vec<_>>(),
            );
        }
    }
    let open = panes.terminal_open;
    let groups = t3_client::terminal_drawer::displayed_groups(&panes);
    let active_group = panes.active_terminal_group_id.clone();
    let sidebar = panes.terminal_ids.len() > 1;
    let split_limit = groups
        .iter()
        .find(|group| group.id == active_group)
        .is_some_and(|group| group.terminal_ids.len() >= t3_client::terminal_ui::MAX_PER_GROUP);
    let mut confirm = use_signal(|| None::<String>);
    let mut height = use_signal(|| panes.terminal_height);
    let mut viewport = use_signal(|| None::<f64>);
    let mut drag = use_signal(|| None::<(i32, f64, f64)>);
    let mut geometry = use_signal(|| None::<document::Eval>);
    let resize_id = use_hook(|| format!("terminal-resize-{}", uuid::Uuid::new_v4()));
    let window_catalog = catalog.clone();
    let window_key = key.clone();
    use_future(move || {
        let catalog = window_catalog.clone();
        let key = window_key.clone();
        async move {
            let mut eval = document::eval(
                "const resized=()=>dioxus.send(window.innerHeight); window.addEventListener('resize',resized); resized(); try{await dioxus.recv();}finally{window.removeEventListener('resize',resized);}",
            );
            geometry.set(Some(eval));
            while let Ok(window_height) = eval.recv::<f64>().await {
                viewport.set(Some(window_height));
                let next =
                    t3_client::terminal_drawer::clamp_height(*height.peek(), Some(window_height));
                if *height.peek() != next {
                    height.set(next);
                }
                if drag.peek().is_none() && catalog.panes.peek().get(&key).terminal_open {
                    catalog.change(&key, |panes, key| {
                        let next = panes.get(key).set_height(next);
                        panes.set(key, next);
                    });
                }
            }
        }
    });
    let drop_catalog = catalog.clone();
    let drop_key = key.clone();
    use_drop(move || {
        if let Some(eval) = *geometry.peek() {
            let _ = eval.send(true);
        }
        drop_catalog.change(&drop_key, |panes, key| {
            let next = panes.get(key).set_height(*height.peek());
            panes.set(key, next);
        });
    });
    let close = {
        let catalog = catalog.clone();
        let key = key.clone();
        let thread = thread_id.clone();
        let environment = environment.clone();
        let transport = transport.clone();
        move |id: String| {
            close_terminal(
                catalog.clone(),
                state,
                transport.clone(),
                environment.clone(),
                thread.clone(),
                key.clone(),
                id,
            )
        }
    };
    let labels = |id: &str| {
        t3_client::terminal_labels::label(
            id,
            summaries
                .read()
                .as_ref()
                .and_then(|rows| rows.iter().find(|row| row.terminal_id.as_str() == id))
                .map(|row| row.label.0.as_str()),
        )
    };
    rsx! {
     section{class:"terminal-drawer", "data-open":open, "data-thread-terminal-drawer":"", style:if open{format!("height:{}px",*height.read())}else{"height:22px".to_owned()},
      div{class:"terminal-toggle",button{disabled:!can_read,onclick:{let catalog=catalog.clone();let key=key.clone();move |_|catalog.change(&key,|panes,key|panes.set_open(key,!open))},if open{"Hide terminal"}else{"Terminal"}}}
      if open {
       div{id:"{resize_id}",class:"terminal-resize",role:"separator","aria-label":"Resize terminal drawer","aria-orientation":"horizontal",
        onpointerdown:{let resize_id=resize_id.clone();move |event:Event<PointerData>|{if event.trigger_button()!=Some(dioxus::html::input_data::MouseButton::Primary){return;}event.prevent_default();let pointer=event.pointer_id();drag.set(Some((pointer,event.client_coordinates().y,*height.peek())));let args=json!({"id":resize_id,"pointer":pointer});let _=document::eval(&format!("document.getElementById({args}.id)?.setPointerCapture({args}.pointer);"));}},
        onpointermove:move |event:Event<PointerData>|{if let Some((pointer,y,start))=*drag.peek(){if pointer==event.pointer_id(){event.prevent_default();height.set(t3_client::terminal_drawer::clamp_height(start+y-event.client_coordinates().y,*viewport.peek()));}}},
        onpointerup:{let catalog=catalog.clone();let key=key.clone();move |event:Event<PointerData>|{if drag.peek().is_some_and(|(pointer,_,_)|pointer==event.pointer_id()){drag.set(None);catalog.change(&key,|panes,key|{let next=panes.get(key).set_height(*height.peek());panes.set(key,next);});}}},
        onpointercancel:{let catalog=catalog.clone();let key=key.clone();move |_|{drag.set(None);catalog.change(&key,|panes,key|{let next=panes.get(key).set_height(*height.peek());panes.set(key,next);});}}
       }
      }
      if let Some(error)=&*catalog.error.read(){p{class:"terminal-error",role:"alert","{error}"}}
      div{class:"terminal-body",style:if open{"display:flex"}else{"display:none"},
       if groups.is_empty(){div{class:"terminal-empty",p{"No terminal sessions for this thread yet."}button{disabled:!can_operate,onclick:{let catalog=catalog.clone();let key=key.clone();move |_|{let id=allocate_terminal(&catalog,&key,summaries);catalog.change(&key,|panes,key|panes.upsert(key,&id,false,Direction::Horizontal));}},"New terminal"}}}
       for group in groups.iter().filter(|group|group.id==active_group) {
        div{class:"terminal-group",style:format!("flex-direction:{}",if group.split_direction==Some(Direction::Vertical){"column"}else{"row"}),
         for id in &group.terminal_ids {
          CanvasPane{environment:environment.clone(),key:"{environment}:{thread_id}:{id}",state,transport:transport.clone(),thread_id:thread_id.clone(),terminal_id:id.clone(),cwd:cwd.clone(),worktree:worktree.clone(),visible:open,can_operate,active:panes.active_terminal_id==*id}
         }
        }
       }
       if !groups.is_empty(){
        aside{class:if sidebar{"terminal-sidebar"}else{"terminal-floating-actions"},
         div{class:"terminal-toolbar",
          for (direction, label, icon) in [(Direction::Horizontal,"Split horizontally","horizontal"),(Direction::Vertical,"Split vertically","vertical")] {
           button{disabled:!can_operate||split_limit,"aria-label":label,title:if split_limit{"A split group can contain up to 4 terminals"}else{label},onclick:{let catalog=catalog.clone();let key=key.clone();move |_|{let id=allocate_terminal(&catalog,&key,summaries);catalog.change(&key,|panes,key|panes.upsert(key,&id,true,direction));}},TerminalIcon{kind:icon}}
          }
          button{disabled:!can_operate,"aria-label":"New terminal",title:"New terminal",onclick:{let catalog=catalog.clone();let key=key.clone();move |_|{let id=allocate_terminal(&catalog,&key,summaries);catalog.change(&key,|panes,key|panes.upsert(key,&id,false,Direction::Horizontal));}},TerminalIcon{kind:"plus"}}
          button{disabled:!can_operate,"aria-label":"Close terminal",title:"Close terminal",onclick:{let id=panes.active_terminal_id.clone();move |_|confirm.set(Some(id.clone()))},TerminalIcon{kind:"trash"}}
         }
         if sidebar{div{class:"terminal-session-list",
          for group in &groups {
           div{class:"terminal-session-group",
            div{class:"terminal-group-label","{t3_client::terminal_drawer::group_label(group)}"," ",span{"{group.terminal_ids.len()}"}}
            for id in &group.terminal_ids{
             div{class:if panes.active_terminal_id==*id{"terminal-session selected"}else{"terminal-session"},
              button{class:"terminal-session-close",disabled:!can_operate,"aria-label":format!("Close {}",labels(id)),title:format!("Close {}",labels(id)),onclick:{let id=id.clone();move |_|confirm.set(Some(id.clone()))},TerminalIcon{kind:"trash"}}
              button{class:"terminal-session-label",title:labels(id),onclick:{let catalog=catalog.clone();let key=key.clone();let id=id.clone();move |_|catalog.change(&key,|panes,key|{let next=panes.get(key).activate(&id);panes.set(key,next);})},"{labels(id)}"}
             }
            }
           }
         }}}
        }
       }
      }
      if let Some(id)=&*confirm.read(){
       div{class:"terminal-confirm-backdrop",onclick:move |_|confirm.set(None),
        div{class:"terminal-confirm",onkeydown:move |event|{if event.key()==Key::Escape{confirm.set(None);}},role:"alertdialog","aria-modal":"true","aria-labelledby":"terminal-close-title",onclick:move |event|event.stop_propagation(),
         h3{id:"terminal-close-title","Close terminal \"{labels(id)}\"?"}
         p{"This stops the running process and clears its history."}
         div{button{autofocus:true,onclick:move |_|confirm.set(None),"Cancel"}button{class:"terminal-destructive",disabled:!can_operate,onclick:{let id=id.clone();let close=close.clone();move |_|{confirm.set(None);close(id.clone());}},"Close terminal"}}
        }
       }
      }
     }
    }
}

fn close_terminal(
    catalog: Catalog,
    state: Store<UiModel>,
    transport: TransportHandle,
    environment: String,
    thread: String,
    key: String,
    id: String,
) {
    if !owns_pane(state, &environment, &thread) || !operates(state) {
        return;
    }
    let Some(owner) = runtime::response_owner(&transport, state) else {
        return;
    };
    catalog.change(&key, |panes, key| panes.close(key, &id));
    dioxus::dioxus_core::spawn_forever(async move {
        let check = transport.clone();
        let check_environment = environment.clone();
        let check_thread = thread.clone();
        let result = crate::terminal_bridge::close_with_fallback(
            move || {
                runtime::response_owner(&check, state).as_ref() == Some(&owner)
                    && owns_pane(state, &check_environment, &check_thread)
                    && operates(state)
            },
            move |attempt| {
                let transport = transport.clone();
                let thread = thread.clone();
                let id = id.clone();
                async move {
                    let (method, payload) = match attempt {
                        crate::terminal_bridge::CloseAttempt::Close => (
                            "terminal.close",
                            json!({"threadId":thread,"terminalId":id,"deleteHistory":true}),
                        ),
                        crate::terminal_bridge::CloseAttempt::Exit => (
                            "terminal.write",
                            json!({"threadId":thread,"terminalId":id,"data":"exit\n"}),
                        ),
                    };
                    runtime::request_value(transport, state, method, payload)
                        .await
                        .map(|_| ())
                }
            },
        )
        .await;
        if let Some(cause) = result {
            let mut errors = catalog.error;
            errors.set(Some(cause));
        }
    });
}

#[component]
fn CanvasPane(
    environment: String,
    state: Store<UiModel>,
    transport: TransportHandle,
    thread_id: String,
    terminal_id: String,
    cwd: String,
    worktree: Option<String>,
    visible: bool,
    can_operate: bool,
    active: bool,
) -> Element {
    let catalog = consume_context::<Catalog>();
    let focus_owner = crate::terminal_bridge::FocusOwner {
        environment: environment.clone(),
        thread: thread_id.clone(),
        terminal: terminal_id.clone(),
    };
    let event_catalog = catalog.clone();
    let event_focus_owner = focus_owner.clone();
    let id = use_hook(|| format!("terminal-{}", uuid::Uuid::new_v4()));
    let mut mounted = use_signal(|| false);
    let mut renderer = use_signal(|| None::<document::Eval>);
    let mut ready = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut running = use_signal(|| false);
    let mut advanced = use_signal(|| false);
    let mut font_preferences = use_signal(|| None::<document::Eval>);
    use_future(move || async move {
        let mut eval = document::eval(
            "const key='t3code:typography-advanced';const read=()=>{try{dioxus.send(window.localStorage.getItem(key));}catch{dioxus.send(null);}};const storage=e=>{if(e.key===key)read();};const local=e=>{if(e.detail?.key===key)read();};window.addEventListener('storage',storage);window.addEventListener('t3code:local_storage_change',local);read();try{await dioxus.recv();}finally{window.removeEventListener('storage',storage);window.removeEventListener('t3code:local_storage_change',local);}",
        );
        font_preferences.set(Some(eval));
        while let Ok(raw) = eval.recv::<Option<String>>().await {
            advanced.set(
                raw.as_deref()
                    .and_then(|raw| serde_json::from_str::<bool>(raw).ok())
                    .unwrap_or(false),
            );
        }
    });
    use_effect(move || {
        let settings = state.client_settings();
        let preferences = settings.read();
        let advanced = *advanced.read();
        let family = if advanced {
            preferences.font_family_terminal.0.clone()
        } else {
            preferences.font_family_code.0.clone()
        };
        let size = if advanced {
            preferences.font_size_terminal.0
        } else {
            preferences.font_size_code.0
        };
        if *ready.read() {
            if let Some(eval) = *renderer.read() {
                let _ = eval.send(json!({"type":"font","family":family,"size":size}));
            }
        }
    });
    let receipt = use_hook(crate::terminal_bridge::Receipts::default);
    let base = SURFACE.to_string();
    let launch_id = id.clone();
    let event_environment = environment.clone();
    let events_transport = transport.clone();
    let event_thread = thread_id.clone();
    let event_terminal = terminal_id.clone();
    let (actions, action_receiver) = use_hook(|| {
        let (tx, rx) = futures_channel::mpsc::unbounded::<
            crate::terminal_bridge::Action<runtime::ResponseOwner>,
        >();
        (tx, Rc::new(RefCell::new(Some(rx))))
    });
    let ordered_transport = transport.clone();
    use_future(move || {
        let receiver = action_receiver
            .borrow_mut()
            .take()
            .expect("terminal action writer mounts once");
        let check = ordered_transport.clone();
        let handle = ordered_transport.clone();
        async move {
            crate::terminal_bridge::pump(
                receiver,
                move |action| {
                    runtime::response_owner(&check, state).as_ref() == Some(&action.owner)
                        && operates(state)
                        && (action.method != "terminal.resize" || *running.peek())
                },
                move |action| {
                    let handle = handle.clone();
                    async move {
                        if let Err(cause) =
                            runtime::request_value(handle, state, action.method, action.payload)
                                .await
                        {
                            error.set(Some(cause));
                        }
                    }
                },
            )
            .await;
        }
    });
    let ack = receipt.clone();
    use_future(move || {
        let id = launch_id.clone();
        let base = base.clone();
        let transport = events_transport.clone();
        let thread = event_thread.clone();
        let terminal = event_terminal.clone();
        let ack = ack.clone();
        let actions = actions.clone();
        let environment = event_environment.clone();
        let catalog = event_catalog.clone();
        let focus_owner = event_focus_owner.clone();
        async move {
            while !*mounted.peek() {
                delay(20).await
            }
            let args = json!({"id":id,"base":base,"wasm":format!("{base}/t3_terminal_bg.wasm"),"options":{"readOnly":!can_operate}});
            let script = format!(
                "const args={args};\n{}",
                include_str!("../assets/terminal_abi.js")
            );
            let mut eval = document::eval(&script);
            renderer.set(Some(eval));
            loop {
                let event: Value = match eval.recv().await {
                    Ok(event) => event,
                    Err(cause) => {
                        error.set(Some(format!("Terminal renderer disconnected: {cause}")));
                        break;
                    }
                };
                match event["type"].as_str() {
                    Some("focus") => catalog.focus(state, &focus_owner),
                    Some("ready") => ready.set(true),
                    Some("error") => {
                        error.set(Some(
                            event["message"]
                                .as_str()
                                .unwrap_or("Terminal renderer failed.")
                                .to_owned(),
                        ));
                        break;
                    }
                    Some("applied") => {
                        if let Some(id) = event["receipt"].as_u64() {
                            ack.apply(id)
                        }
                    }
                    Some("write") | Some("resize") => {
                        if !owns_pane(state, &environment, &thread)
                            || !operates(state)
                            || (event["type"] == "resize" && !*running.peek())
                        {
                            continue;
                        }
                        let Some(owner) = runtime::response_owner(&transport, state) else {
                            continue;
                        };
                        let thread = thread.clone();
                        let terminal = terminal.clone();
                        let (method, payload) = if event["type"] == "write" {
                            (
                                "terminal.write",
                                json!({"threadId":thread,"terminalId":terminal,"data":event["data"]}),
                            )
                        } else {
                            (
                                "terminal.resize",
                                json!({"threadId":thread,"terminalId":terminal,"cols":event["cols"].as_u64().unwrap_or(1).min(1000),"rows":event["rows"].as_u64().unwrap_or(1).min(500)}),
                            )
                        };
                        let _ = actions.unbounded_send(crate::terminal_bridge::Action {
                            owner,
                            method,
                            payload,
                        });
                    }
                    _ => {}
                }
            }
            ack.close();
            ready.set(false);
            renderer.set(None);
            let _ = eval.send(json!({"type":"dispose"}));
        }
    });
    use_effect(use_reactive(
        (&visible, &can_operate, &active),
        move |(visible, can_operate, active)| {
            let loaded = *ready.read();
            let read_only = !can_operate;
            if let Some(eval) = *renderer.read() {
                let _ = eval.send(json!({"type":"visible","visible":visible}));
                let _ = eval.send(json!({"type":"readonly","readOnly":read_only}));
                if loaded && visible && can_operate && active {
                    let _ = eval.send(json!({"type":"focus"}));
                }
            }
        },
    ));
    let stream_transport = transport.clone();
    let stream_thread = thread_id.clone();
    let stream_terminal = terminal_id.clone();
    let stream_ack = receipt.clone();
    let exit_catalog = catalog.clone();
    use_future(move || {
        let handle = stream_transport.clone();
        let thread = stream_thread.clone();
        let terminal = stream_terminal.clone();
        let cwd = cwd.clone();
        let worktree = worktree.clone();
        let ack = stream_ack.clone();
        let environment = environment.clone();
        let catalog = exit_catalog.clone();
        async move {
            let mut exit = t3_client::terminal_drawer::ExitState::default();
            loop {
                if ack.is_closed() {
                    return;
                }
                if !owns_pane(state, &environment, &thread) {
                    return;
                }
                if *state.status().peek() != ConnectionStatus::Connected {
                    delay(200).await;
                    continue;
                }
                let input: TerminalAttachInput = match serde_json::from_value(
                    json!({"threadId":thread,"terminalId":terminal,"cwd":cwd,"worktreePath":worktree}),
                ) {
                    Ok(input) => input,
                    Err(cause) => {
                        error.set(Some(cause.to_string()));
                        return;
                    }
                };
                let mut session = match SessionStream::subscribe(&handle, state, input) {
                    Ok(stream) => stream,
                    Err(cause) => {
                        error.set(Some(cause));
                        delay(500).await;
                        continue;
                    }
                };
                let mut cursor = INITIAL_CURSOR;
                let mut size_epoch = crate::terminal_bridge::SizeEpoch::default();
                running.set(false);
                while let Some(next) = session.next().await {
                    let buffer = match next {
                        Ok(buffer) => buffer,
                        Err(cause) => {
                            running.set(false);
                            error.set(Some(cause));
                            break;
                        }
                    };
                    running.set(buffer.status == SessionStatus::Running);
                    while !*ready.peek() {
                        if ack.is_closed() {
                            return;
                        }
                        delay(20).await;
                    }
                    if size_epoch.observe(&buffer, operates(state)) {
                        if let Some(eval) = *renderer.peek() {
                            let _ = eval.send(json!({"type":"size"}));
                        }
                    }
                    let exit_message = exit.observe(buffer.status, buffer.version);
                    let update = terminal_output::read(&buffer.output, cursor);
                    cursor = update.cursor();
                    let (kind, data) = match update {
                        OutputUpdate::None { .. } if exit_message.is_none() => continue,
                        OutputUpdate::None { .. } => ("append", String::new()),
                        OutputUpdate::Reset { data, .. } => ("reset", data),
                        OutputUpdate::Append { data, .. } => ("append", data),
                    };
                    let data = if let Some(message) = exit_message {
                        format!("{data}\r\n[terminal] {message}\r\n")
                    } else {
                        data
                    };
                    let Some(eval) = *renderer.peek() else { break };
                    let Some((receipt_id, receiver)) = ack.begin() else {
                        return;
                    };
                    if let Err(cause) =
                        eval.send(json!({"type":kind,"data":data,"receipt":receipt_id}))
                    {
                        error.set(Some(format!("Terminal paint failed: {cause}")));
                        ack.close();
                        return;
                    }
                    // Only one decoded update crosses the WebView ABI at a time. A stalled
                    // renderer therefore stops stream consumption/ACKs rather than buffering.
                    if receiver.await.is_err() {
                        return;
                    }
                    error.set(None);
                    if exit_message.is_some()
                        && owns_pane(state, &environment, &thread)
                        && operates(state)
                    {
                        close_terminal(
                            catalog.clone(),
                            state,
                            handle.clone(),
                            environment.clone(),
                            thread.clone(),
                            ScopedPanes::key(&environment, &thread),
                            terminal.clone(),
                        );
                        return;
                    }
                }
                running.set(false);
                delay(250).await;
            }
        }
    });
    let cleanup_id = id.clone();
    let cleanup_receipts = receipt.clone();
    use_drop(move || {
        if let Some(eval) = *font_preferences.peek() {
            let _ = eval.send(true);
        }
        cleanup_receipts.close();
        if let Some(eval) = *renderer.peek() {
            let _ = eval.send(json!({"type":"dispose"}));
        }
        let args = json!({"id":cleanup_id});
        let _ = document::eval(&format!(
            "const slot=window.__t3RustTerminals?.get({args}.id);if(slot){{slot.disposed=true;}}"
        ));
    });
    rsx! {div{class:"terminal-pane","data-active":active,onmousedown:move |_|catalog.focus(state,&focus_owner),div{id:"{id}",class:"terminal-canvas",onmounted:move |_|mounted.set(true)},if let Some(message)=&*error.read(){p{class:"terminal-error",role:"alert","{message}"}}}}
}

fn owns_pane(state: Store<UiModel>, environment: &str, thread: &str) -> bool {
    let model = state.peek();
    model
        .destination
        .as_ref()
        .is_some_and(|id| id.as_str() == environment)
        && model.active_thread.as_deref() == Some(thread)
}

fn operates(state: Store<UiModel>) -> bool {
    let model = state.peek();
    model.status == ConnectionStatus::Connected
        && model.destination.as_ref().is_some_and(|id| {
            model
                .environments
                .allows(id, AuthEnvironmentScope::TerminalOperate)
        })
}
fn reads(state: Store<UiModel>) -> bool {
    let model = state.peek();
    model.destination.as_ref().is_some_and(|id| {
        model
            .environments
            .allows(id, AuthEnvironmentScope::TerminalRead)
    })
}

fn allocate_terminal(
    catalog: &Catalog,
    key: &str,
    summaries: Signal<Option<Vec<t3_contracts::TerminalSummary>>>,
) -> String {
    let mut ids = catalog.panes.peek().get(key).terminal_ids;
    let saved = summaries.peek();
    if let Some(rows) = saved.as_ref() {
        ids.extend(rows.iter().map(|row| row.terminal_id.to_string()));
    }
    let suffix = saved.is_none().then(|| uuid::Uuid::new_v4().to_string());
    t3_client::terminal_labels::next_id(&ids, suffix.as_deref())
}

// Lucide v0.564.0 ISC icon paths, matching the original drawer controls.
#[component]
fn TerminalIcon(kind: &'static str) -> Element {
    let paths: &[&str] = match kind {
        "horizontal" => &[
            "M8 19H5c-1 0-2-1-2-2V7c0-1 1-2 2-2h3",
            "M16 5h3c1 0 2 1 2 2v10c0 1-1 2-2 2h-3",
            "M12 4v16",
        ],
        "vertical" => &[
            "M5 8V5c0-1 1-2 2-2h10c1 0 2 1 2 2v3",
            "M19 16v3c0 1-1 2-2 2H7c-1 0-2-1-2-2v-3",
            "M4 12h16",
        ],
        "plus" => &["M12 5v14", "M5 12h14"],
        _ => &[
            "M10 11v6",
            "M14 11v6",
            "M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6",
            "M3 6h18",
            "M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2",
        ],
    };
    rsx! {svg{width:"13",height:"13",view_box:"0 0 24 24",fill:"none",stroke:"currentColor",stroke_width:"2",stroke_linecap:"round",stroke_linejoin:"round","aria-hidden":"true",for d in paths{path{d:*d}}}}
}
