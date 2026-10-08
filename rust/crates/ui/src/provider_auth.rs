//! Sign-in flows are owned by the destination connection. Secrets are ephemeral
//! component state; no composer, settings or local-storage writer sees them.
use crate::runtime::{self, ConnectionOwner, TransportHandle, UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use t3_client::provider_auth as policy;
use t3_contracts::{AuthEnvironmentScope, ProviderAuthState};

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct AuthProbe {
    pub stream_id: Rc<RefCell<Option<String>>>,
    pub subscriptions: Rc<std::cell::Cell<usize>>,
}
fn manages(state: Store<UiModel>) -> bool {
    let model = state.peek();
    model.status == t3_client::connection::ConnectionStatus::Connected
        && model.destination.as_ref().is_some_and(|id| {
            model
                .environments
                .allows(id, AuthEnvironmentScope::ProvidersManage)
        })
}
#[derive(Clone, Default)]
struct Draft {
    id: String,
    values: BTreeMap<String, String>,
}
struct Pending(Signal<bool>);
impl Drop for Pending {
    fn drop(&mut self) {
        if let Ok(mut value) = self.0.try_write() {
            *value = false;
        }
    }
}
struct Popup(Option<document::Eval>);
impl Popup {
    fn finish(&mut self, accepted: bool) {
        if let Some(eval) = self.0.take() {
            let _ = eval.send(accepted);
        }
    }
}
impl Drop for Popup {
    fn drop(&mut self) {
        self.finish(false);
    }
}
#[derive(Clone)]
struct Action {
    method: &'static str,
    payload: Value,
    clear: bool,
    popup: Option<document::Eval>,
}

#[component]
pub(crate) fn Authentication(
    state: Store<UiModel>,
    transport: TransportHandle,
    provider: Value,
    environment: String,
    allowed: bool,
) -> Element {
    // Subscribe to status so reconnect replaces the captured owner even when the
    // provider catalog itself has not changed.
    let _status = state.status().read().clone();
    let owner = runtime::connection_owner(&transport, state);
    let instance = provider["instanceId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let mut auth = use_signal(|| None::<Value>);
    let mut query_error = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut method = use_signal(String::new);
    let mut draft = use_signal(Draft::default);
    let mut pending = use_signal(|| false);
    let mut logout = use_signal(|| false);
    let mut revealed = use_signal(|| false);
    #[cfg(test)]
    let probe = try_consume_context::<AuthProbe>();
    let subscription_transport = transport.clone();
    use_resource(use_reactive(
        (&owner, &allowed, &instance),
        move |(owner, allowed, instance)| {
            let transport = subscription_transport.clone();
            #[cfg(test)]
            let probe = probe.clone();
            async move {
                auth.set(None);
                query_error.set(None);
                draft.set(Draft::default());
                error.set(None);
                logout.set(false);
                let Some(owner) = owner.filter(|_| allowed) else {
                    return;
                };
                let mut stream = match runtime::request_stream(
                    &transport,
                    state,
                    "provider.auth.subscribe",
                    json!({"instanceId":instance}),
                ) {
                    Ok(stream) => stream,
                    Err(cause) => {
                        query_error.set(Some(cause));
                        return;
                    }
                };
                #[cfg(test)]
                if let Some(probe) = probe {
                    *probe.stream_id.borrow_mut() = Some(stream.request_id());
                    probe.subscriptions.set(probe.subscriptions.get() + 1);
                }
                while let Some(chunk) = stream.next().await {
                    if runtime::connection_owner(&transport, state).as_ref() != Some(&owner)
                        || !manages(state)
                    {
                        return;
                    }
                    let values = match chunk {
                        Ok(values) => values,
                        Err(cause) => {
                            query_error.set(Some(cause));
                            return;
                        }
                    };
                    // Validate a complete chunk before painting any state. Privacy
                    // and method capabilities come from the authenticated server.
                    let decoded: Result<Vec<ProviderAuthState>, _> =
                        values.iter().cloned().map(serde_json::from_value).collect();
                    match decoded {
                        Ok(states)
                            if states
                                .iter()
                                .all(|value| value.instance_id.as_str() == instance) =>
                        {
                            if let Some(value) = states.last() {
                                auth.set(Some(
                                    serde_json::to_value(value).expect("validated auth state"),
                                ));
                            }
                        }
                        Ok(_) => {
                            query_error.set(Some(
                                "Provider sign-in stream targeted another instance.".into(),
                            ));
                            return;
                        }
                        Err(cause) => {
                            query_error
                                .set(Some(format!("Invalid provider sign-in state: {cause}")));
                            return;
                        }
                    }
                }
            }
        },
    ));
    let dispatch = EventHandler::new({
        let transport = transport.clone();
        let instance = instance.clone();
        move |action: Action| {
            let mut popup = Popup(action.popup);
            if *pending.peek() || !manages(state) {
                return;
            }
            let Some(owner) = runtime::connection_owner(&transport, state) else {
                return;
            };
            if action.payload["instanceId"] != instance {
                return;
            }
            if action.payload.get("flowId").is_some()
                && auth.peek().as_ref().map(|value| &value["flowId"])
                    != Some(&action.payload["flowId"])
            {
                return;
            }
            if action.payload.get("interactionId").is_some()
                && auth
                    .peek()
                    .as_ref()
                    .map(|value| &value["interaction"]["id"])
                    != Some(&action.payload["interactionId"])
            {
                return;
            }
            pending.set(true);
            error.set(None);
            let receipt = Pending(pending);
            let transport = transport.clone();
            let draft_id = draft.peek().id.clone();
            let target_flow = action.payload.get("flowId").cloned();
            let target_interaction = action.payload.get("interactionId").cloned();
            spawn(async move {
                let _receipt = receipt;
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner)
                    || !manages(state)
                {
                    return;
                }
                if target_flow.as_ref().is_some_and(|flow| {
                    auth.peek()
                        .as_ref()
                        .is_none_or(|current| &current["flowId"] != flow)
                }) || target_interaction.as_ref().is_some_and(|id| {
                    auth.peek()
                        .as_ref()
                        .is_none_or(|current| &current["interaction"]["id"] != id)
                }) {
                    return;
                }
                let result =
                    runtime::request_value(transport.clone(), state, action.method, action.payload)
                        .await;
                let current = runtime::connection_owner(&transport, state).as_ref() == Some(&owner)
                    && manages(state)
                    && target_flow.as_ref().is_none_or(|flow| {
                        auth.peek()
                            .as_ref()
                            .is_some_and(|current| &current["flowId"] == flow)
                    });
                popup.finish(current && result.is_ok());
                if !current {
                    return;
                }
                match result {
                    Ok(_) => {
                        if action.clear && draft.peek().id == draft_id {
                            draft.set(Draft::default());
                        }
                        logout.set(false);
                    }
                    Err(cause) => {
                        if target_interaction.as_ref().is_none_or(|id| {
                            auth.peek()
                                .as_ref()
                                .is_some_and(|current| &current["interaction"]["id"] == id)
                        }) {
                            error.set(Some(cause));
                        }
                    }
                }
            });
        }
    });
    let auth_value = auth.read().clone();
    let account = policy::account(
        &provider,
        auth_value.as_ref(),
        query_error.read().is_some(),
        !allowed,
        *pending.read(),
        &method.read(),
        &environment,
    );
    let flow = auth_value
        .as_ref()
        .map(|value| value["flowId"].clone())
        .unwrap_or(Value::Null);
    let interaction = auth_value
        .as_ref()
        .map(|value| value["interaction"].clone())
        .unwrap_or(Value::Null);
    let methods = auth_value
        .as_ref()
        .and_then(|value| value["methods"].as_array())
        .cloned()
        .unwrap_or_default();
    use_effect(use_reactive((&account.draft_id,), move |(id,)| {
        if draft.peek().id != id {
            draft.set(Draft {
                id,
                values: BTreeMap::new(),
            });
        }
    }));
    let values = if draft.read().id == account.draft_id {
        draft.read().values.clone()
    } else {
        BTreeMap::new()
    };
    let change = EventHandler::new({
        let id = account.draft_id.clone();
        move |(name, value): (String, String)| {
            let mut next = if draft.peek().id == id {
                draft.peek().clone()
            } else {
                Draft {
                    id: id.clone(),
                    values: BTreeMap::new(),
                }
            };
            next.values.insert(name, value);
            draft.set(next);
        }
    });
    let email = provider["auth"]["email"].as_str().unwrap_or_default();
    let email = policy::trim(email).to_owned();
    use_effect(use_reactive((&email,), move |_| revealed.set(false)));
    let display = provider["displayName"]
        .as_str()
        .or(provider["driver"].as_str())
        .unwrap_or("provider")
        .to_owned();
    let disabled = account.disabled;
    let response = EventHandler::new({
        let instance = instance.clone();
        let flow = flow.clone();
        let interaction = interaction.clone();
        move |(response, clear, popup): (Value, bool, Option<document::Eval>)| {
            dispatch.call(Action{method:"provider.auth.respond",payload:json!({"instanceId":instance,"flowId":flow,"interactionId":interaction["id"],"response":response}),clear,popup});
        }
    });
    rsx! {section {class:"provider-account","aria-label":"Provider account",
        h3 {"Account"}
        if account.signed_in&&!account.active&&!policy::trim(&email).is_empty(){p {"Signed in as ",button {class:if *revealed.read(){"redacted-account"}else{"redacted-account obscured"},title:if *revealed.read(){"Click to hide email"}else{"Click to reveal email"},"aria-label":"Toggle account email visibility",onclick:move |_|revealed.toggle(),if *revealed.read(){"{email}"}else{{policy::redacted_placeholder(&email)}}}}}
        else {p {role:"status","{account.description}"}}
        if let Some(message)=account.status_message {p {role:"status","{message}"}}
        div {class:"provider-editor-actions",
            if account.method_picker {select {"aria-label":"Sign-in method",disabled,value:account.selected_method.clone(),onchange:move |event|method.set(event.value()),
                option {value:"",selected:account.selected_method.is_empty(),"Provider default"}
                for entry in methods {{let id=entry["id"].as_str().unwrap_or_default(); rsx!{option {key:"{id}",value:id,selected:id==account.selected_method,{entry["name"].as_str().unwrap_or_default()}}}}}
            }}
            if let Some(url)=account.url.clone().filter(|url|!url.is_empty()) {
                button {"aria-label":"Open browser",disabled,onclick:{let url=url.clone();let interaction=interaction.clone();let transport=transport.clone();move |_|{
                    let consent=interaction["type"]=="browser"&&interaction["requiresConsent"]==true;
                    let args=json!({"url":url,"consent":consent,"native":cfg!(any(feature="desktop",feature="mobile"))});
                    let popup=document::eval(&format!("const args={args};\n{}",include_str!("../assets/provider_auth_browser.js")));
                    let captured=runtime::connection_owner(&transport,state);let popup_transport=transport.clone();
                    spawn(async move {if !matches!(popup.join::<bool>().await,Ok(true))&&runtime::connection_owner(&popup_transport,state)==captured&&manages(state){error.set(Some("Could not open the sign-in page. Copy the link and open it in your browser.".into()));}});
                    if consent{response.call((json!({"type":"browser","action":"accept"}),false,Some(popup)));}

                }},"Open browser"}
                button {"aria-label":"Copy sign-in link",disabled,onclick:{let url=url.clone();let interaction=interaction.clone();let transport=transport.clone();let flow=flow.clone();let instance=instance.clone();move |_|{
                    let consent=interaction["type"]=="browser"&&interaction["requiresConsent"]==true;
                    let payload=json!({"instanceId":instance,"flowId":flow,"interactionId":interaction["id"],"response":{"type":"browser","action":"accept"}});let captured=runtime::connection_owner(&transport,state);let clipboard_transport=transport.clone();let args=json!({"url":url});let clipboard=document::eval(&format!("const args={args};await navigator.clipboard.writeText(args.url);return true;"));
                    spawn(async move {let result=clipboard.join::<bool>().await;if runtime::connection_owner(&clipboard_transport,state)!=captured||!manages(state){return;}match result{Ok(true)=>{if consent{dispatch.call(Action{method:"provider.auth.respond",payload,clear:false,popup:None});}},_=>error.set(Some("Could not copy the sign-in link. Try opening it in your browser.".into()))}});
                }},"Copy sign-in link"}
            }
            if account.active&&!flow.is_null(){button {"aria-label":"Cancel sign-in",disabled,onclick:{let instance=instance.clone();let flow=flow.clone();move |_|dispatch.call(Action{method:"provider.auth.cancel",payload:json!({"instanceId":instance,"flowId":flow}),clear:false,popup:None})},"Cancel sign-in"}}
            if !account.active&&!account.needs_external_setup&&provider["setup"]["canAuthenticate"]!=false {button {"aria-label":account.start_label.clone(),disabled:account.start_disabled,onclick:{let instance=instance.clone();let selected=account.selected_method.clone();move |_|{
                let mut payload=json!({"instanceId":instance});if !selected.is_empty(){payload["methodId"]=json!(selected);}dispatch.call(Action{method:"provider.auth.start",payload,clear:false,popup:None});
            }},"{account.start_label}"}}
            if account.can_logout {button {"aria-label":"Sign out",disabled:disabled||auth_value.is_none(),onclick:move |_|logout.set(true),"Sign out"}}
            if account.needs_external_setup {if let Some(url)=provider["setup"]["documentationUrl"].as_str(){a {href:url,target:"_blank",rel:"noopener noreferrer","Provider documentation"}}}
        }
        if *logout.read(){div {role:"dialog","aria-label":"Sign out of provider",p {"Sign out of {display} on {environment}? This stops running threads that share this sign-in. Thread history is kept."}button {"aria-label":"Cancel sign out",onclick:move |_|logout.set(false),"Cancel"}button {"aria-label":"Confirm sign out",disabled,onclick:{let instance=instance.clone();move |_|dispatch.call(Action{method:"provider.auth.logout",payload:json!({"instanceId":instance}),clear:true,popup:None})},"Confirm sign out"}}}
        if interaction["type"]=="deviceCode" {p {"Enter code "code {{interaction["userCode"].as_str().unwrap_or_default()}}" in your browser."}}
        if interaction["type"]=="credentials" {form {"aria-label":"Provider credentials",onsubmit:{let values=values.clone();move |event|{event.prevent_default();response.call((json!({"type":"credentials","values":values}),true,None));}},
            for field in interaction["fields"].as_array().cloned().unwrap_or_default(){{let field_id=field["name"].as_str().unwrap_or_default();rsx!{label {key:"{field_id}",{field["label"].as_str().unwrap_or_default()}input {"aria-label":field["label"].as_str().unwrap_or_default(),r#type:if field["secret"]==true{"password"}else{"text"},autocomplete:"off",maxlength:16384,disabled,value:values.get(field["name"].as_str().unwrap_or_default()).cloned().unwrap_or_default(),oninput:{let name=field["name"].as_str().unwrap_or_default().to_owned();move |event|change.call((name.clone(),event.value()))}}}}}}
            button {r#type:"submit",disabled,"Connect"}
        }}
        if account.callback {form {"aria-label":"Provider callback",onsubmit:{let instance=instance.clone();let flow=flow.clone();let callback=values.get("callback").cloned().unwrap_or_default();move |event|{event.prevent_default();if !policy::trim(&callback).is_empty(){dispatch.call(Action{method:"provider.auth.complete",payload:json!({"instanceId":instance,"flowId":flow,"callbackUrl":callback}),clear:true,popup:None});}}},
            label {"If the final localhost page does not load, paste its full URL here.",input {r#type:"url","aria-label":"Sign-in callback URL",autocomplete:"off",maxlength:16384,disabled,value:values.get("callback").cloned().unwrap_or_default(),oninput:move |event|change.call(("callback".into(),event.value()))}}
            button {r#type:"submit",disabled:disabled||values.get("callback").is_none_or(|value|policy::trim(value).is_empty()),"Continue"}
        }}
        if interaction["type"]=="terminal" {AuthTerminal {key:"{account.draft_id}",state,transport:transport.clone(),owner:owner.clone(),instance:instance.clone(),flow:flow.clone(),interaction:interaction.clone(),read_only:!allowed,onerror:move |cause|error.set(Some(cause))}}
        if let Some(cause)=error.read().clone().or_else(||query_error.read().clone()){p {role:"alert",class:"error-banner","{cause}"}}
    }}
}

#[component]
fn AuthTerminal(
    state: Store<UiModel>,
    transport: TransportHandle,
    owner: Option<ConnectionOwner>,
    instance: String,
    flow: Value,
    interaction: Value,
    read_only: bool,
    onerror: EventHandler<String>,
) -> Element {
    let id = use_hook(|| format!("provider-auth-terminal-{}", uuid::Uuid::new_v4()));
    let mut mounted = use_signal(|| false);
    let mut renderer = use_signal(|| None::<document::Eval>);
    let mut written = use_signal(|| 0i64);
    let latest = use_signal(|| interaction.clone());
    let mut latest = latest;
    use_effect(use_reactive((&interaction,), move |(interaction,)| {
        latest.set(interaction)
    }));
    let (sender, receiver) = use_hook(|| {
        let (sender, receiver) = futures_channel::mpsc::unbounded::<Value>();
        (sender, Rc::new(RefCell::new(Some(receiver))))
    });
    let writer_transport = transport.clone();
    let writer_owner = owner.clone();
    let writer_flow = flow.clone();
    let writer_instance = instance.clone();
    use_future(move || {
        let mut receiver = receiver
            .borrow_mut()
            .take()
            .expect("auth terminal mounts once");
        let transport = writer_transport.clone();
        let owner = writer_owner.clone();
        let flow = writer_flow.clone();
        let instance = writer_instance.clone();
        async move {
            while let Some(response) = receiver.next().await {
                for data in policy::terminal_chunks(response["data"].as_str().unwrap_or_default()) {
                    if owner.is_none()
                        || runtime::connection_owner(&transport, state) != owner
                        || !manages(state)
                    {
                        return;
                    }
                    let mut response = response.clone();
                    response["data"] = json!(data);
                    let payload = json!({"instanceId":instance,"flowId":flow,"interactionId":latest.peek()["id"],"response":response});
                    if let Err(_) = runtime::request_value(
                        transport.clone(),
                        state,
                        "provider.auth.respond",
                        payload,
                    )
                    .await
                    {
                        if runtime::connection_owner(&transport, state) == owner {
                            onerror.call(
                                "The provider sign-in terminal is no longer available.".into(),
                            );
                        }
                        return;
                    }
                }
            }
        }
    });
    let start = EventHandler::new({
        let id = id.clone();
        let owner = owner.clone();
        let transport = transport.clone();
        let sender = sender.clone();
        move |_| {
            if *mounted.peek() {
                return;
            }
            mounted.set(true);
            let id = id.clone();
            let transport = transport.clone();
            let owner = owner.clone();
            let sender = sender.clone();
            spawn(async move {
                let base = crate::terminal_pane::SURFACE.to_string();
                let args = json!({"id":id,"base":base,"wasm":format!("{base}/t3_terminal_bg.wasm"),"options":{"fontSize":13,"tabNavigates":true,"readOnly":read_only}});
                let mut eval = document::eval(&format!(
                    "const args={args};\n{}",
                    include_str!("../assets/terminal_abi.js")
                ));
                renderer.set(Some(eval));
                loop {
                    let event: Value = match eval.recv().await {
                        Ok(event) => event,
                        Err(cause) => {
                            onerror.call(format!("Sign-in terminal disconnected: {cause}"));
                            break;
                        }
                    };
                    match event["type"].as_str() {
                        Some("ready") => {
                            let current = latest.peek();
                            let _=eval.send(json!({"type":"reset","data":current["output"].as_str().unwrap_or_default()}));
                            written.set(current["outputOffset"].as_i64().unwrap_or_else(|| {
                                current["output"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .encode_utf16()
                                    .count() as i64
                            }));
                        }
                        Some("write" | "resize") => {
                            if runtime::connection_owner(&transport, state) != owner
                                || !manages(state)
                            {
                                continue;
                            }
                            let response = if event["type"] == "write" {
                                json!({"type":"terminal","data":event["data"]})
                            } else {
                                json!({"type":"terminal","data":"","size":{"cols":event["cols"].as_u64().unwrap_or(1).clamp(1,500),"rows":event["rows"].as_u64().unwrap_or(1).clamp(1,200)}})
                            };
                            let _ = sender.unbounded_send(response);
                        }
                        Some("link") => {
                            if let Some(url) = event["url"].as_str().filter(|url| {
                                url.to_ascii_lowercase().starts_with("https://")
                                    || url.to_ascii_lowercase().starts_with("http://")
                            }) {
                                let url = json!(url);
                                let _ = document::eval(&format!(
                                    "window.open({url},'_blank','noopener');"
                                ));
                            }
                        }
                        Some("error") => {
                            onerror.call(
                                event["message"]
                                    .as_str()
                                    .unwrap_or("Sign-in terminal failed.")
                                    .into(),
                            );
                            break;
                        }
                        _ => {}
                    }
                }
                renderer.set(None);
                let _ = eval.send(json!({"type":"dispose"}));
            });
        }
    });
    use_effect(use_reactive(
        (&interaction, &read_only),
        move |(interaction, read_only)| {
            if let Some(eval) = *renderer.read() {
                let _ = eval.send(json!({"type":"readonly","readOnly":read_only}));
                let paint = policy::terminal_paint(
                    &mut written.write(),
                    interaction["output"].as_str().unwrap_or_default(),
                    interaction["outputOffset"].as_i64(),
                );
                match paint {
                    policy::Paint::None => {}
                    policy::Paint::Append(data) => {
                        let _ = eval.send(json!({"type":"append","data":data}));
                    }
                    policy::Paint::Reset(data) => {
                        let _ = eval.send(json!({"type":"reset","data":data}));
                    }
                }
            }
        },
    ));
    use_drop({
        let id = id.clone();
        move || {
            if let Some(eval) = *renderer.peek() {
                let _ = eval.send(json!({"type":"dispose"}));
            }
            let args = json!({"id":id});
            let _ = document::eval(&format!(
                "const slot=window.__t3RustTerminals?.get({args}.id);if(slot)slot.disposed=true;"
            ));
        }
    });
    rsx! {div {id,"aria-label":"Provider sign-in terminal",class:"provider-auth-terminal",style:"height:256px;",onmounted:move |_|start.call(())}}
}
