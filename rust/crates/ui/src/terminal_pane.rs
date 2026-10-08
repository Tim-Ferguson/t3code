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
    let active_group = panes.active_terminal_group_id.clone();
    rsx! {
     section{class:"terminal-drawer", "data-open":open,
      div{class:"terminal-toolbar",
       button{disabled:!can_read,onclick:{let catalog=catalog.clone();let key=key.clone();move |_|catalog.change(&key,|panes,key|panes.set_open(key,!open))},if open{"Hide terminal"}else{"Terminal"}}
       if open{
        for group in &panes.terminal_groups {
         button{class:if group.id==active_group{"selected"}else{""},onclick:{let catalog=catalog.clone();let key=key.clone();let id=group.terminal_ids[0].clone();move |_|catalog.change(&key,|panes,key|{let state=panes.get(key).activate(&id);panes.set(key,state)})},"{t3_client::terminal_labels::label(&group.terminal_ids[0], summaries.read().as_ref().and_then(|rows| rows.iter().find(|row|row.terminal_id.as_str()==group.terminal_ids[0])).map(|row|row.label.0.as_str()))}"}
        }
        button{disabled:!can_operate,onclick:{let catalog=catalog.clone();let key=key.clone();move |_|{let id=allocate_terminal(&catalog,&key,summaries);catalog.change(&key,|panes,key|panes.upsert(key,&id,false,Direction::Horizontal));}},"＋"}
        button{disabled:!can_operate||panes.terminal_groups.iter().find(|group|group.id==active_group).is_some_and(|group|group.terminal_ids.len()>=4),onclick:{let catalog=catalog.clone();let key=key.clone();move |_|{let id=allocate_terminal(&catalog,&key,summaries);catalog.change(&key,|panes,key|panes.upsert(key,&id,true,Direction::Horizontal));}},"Split"}
        button{disabled:!can_operate,onclick:{let catalog=catalog.clone();let key=key.clone();let id=panes.active_terminal_id.clone();let thread=thread_id.clone();let transport=transport.clone();move |_|{if runtime::request(&transport,state,"terminal.close",json!({"threadId":thread,"terminalId":id,"deleteHistory":true}),t3_client::rpc::RequestKind::Unary).is_some(){catalog.change(&key,|panes,key|panes.close(key,&id));}}},"Close"}
       }
      }
      if let Some(error)=&*catalog.error.read(){p{class:"terminal-error",role:"alert","{error}"}}
      for group in panes.terminal_groups.iter().filter(|group|group.id==active_group) {
       div{class:"terminal-group",style:format!("height:{}px;display:{};flex-direction:{}",panes.terminal_height,if open&&group.id==active_group{"flex"}else{"none"},if group.split_direction==Some(Direction::Vertical){"column"}else{"row"}),
        for id in &group.terminal_ids {
         CanvasPane{environment:environment.clone(),key:"{environment}:{thread_id}:{id}",state,transport:transport.clone(),thread_id:thread_id.clone(),terminal_id:id.clone(),cwd:cwd.clone(),worktree:worktree.clone(),visible:open&&group.id==active_group,can_operate,active:panes.active_terminal_id==*id}
        }
       }
      }
     }
    }
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
    use_future(move || {
        let handle = stream_transport.clone();
        let thread = stream_thread.clone();
        let terminal = stream_terminal.clone();
        let cwd = cwd.clone();
        let worktree = worktree.clone();
        let ack = stream_ack.clone();
        let environment = environment.clone();
        async move {
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
                    let update = terminal_output::read(&buffer.output, cursor);
                    cursor = update.cursor();
                    let (kind, data) = match update {
                        OutputUpdate::None { .. } => continue,
                        OutputUpdate::Reset { data, .. } => ("reset", data),
                        OutputUpdate::Append { data, .. } => ("append", data),
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
                }
                running.set(false);
                delay(250).await;
            }
        }
    });
    let cleanup_id = id.clone();
    let cleanup_receipts = receipt.clone();
    use_drop(move || {
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
