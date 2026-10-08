use crate::runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{connection::ConnectionStatus, provider_settings as policy};
use t3_contracts::AuthEnvironmentScope;

// Completion owns only this mounted control's saving flag. Result/error writes
// remain fenced by the captured connection, while cancellation always releases
// the local flag even if the destination/socket changed without an unmount.
struct Saving(Signal<bool>);
impl Drop for Saving {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.0.try_write() {
            *pending = false;
        }
    }
}

fn label(row: &policy::InstanceRow) -> String {
    row.instance["displayName"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            policy::drivers()
                .iter()
                .find(|d| d["value"] == row.driver)
                .and_then(|d| d["label"].as_str())
        })
        .unwrap_or(&row.driver)
        .to_owned()
}

fn can_manage(state: Store<UiModel>, destination: &str) -> bool {
    let connected = *state.status().read() == ConnectionStatus::Connected;
    connected
        && state.destination().read().as_ref().is_some_and(|id| {
            id.as_str() == destination
                && state
                    .environments()
                    .read()
                    .allows(id, AuthEnvironmentScope::ProvidersManage)
        })
}

#[component]
pub fn Providers(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let current = state.destination().read().clone();
    let mut environment = use_signal(|| {
        current
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default()
    });
    let records: Vec<_> = state
        .environments()
        .read()
        .records
        .values()
        .map(|record| (record.id.to_string(), record.label.clone()))
        .collect();
    let ids: Vec<String> = records.iter().map(|(id, _)| id.clone()).collect();
    let selected = policy::selected_environment(
        &ids,
        Some(&environment.read()),
        current.as_ref().map(|id| id.as_str()),
    )
    .unwrap_or_default()
    .to_owned();
    let is_current = current.as_ref().is_some_and(|id| id.as_str() == selected);
    let connected = *state.status().read() == ConnectionStatus::Connected;
    rsx! {section {class:"settings-page",
        div {class:"settings-tabs",button {class:"selected","Providers"}button {onclick:move |_|state.view().set(View::Appearance),"Appearance"}button {onclick:move |_|state.view().set(View::Connections),"Connections"}}
        h1 {"Providers"}
        if !records.is_empty() {
            label {r#for:"provider-environment","Environment"}
            select {id:"provider-environment",value:selected.clone(),onchange:move |event|environment.set(event.value()),
                for (id,name) in records {option {key:"{id}",value:id.clone(),selected:selected==id,"{name}"}}
            }
        }
        if is_current&&connected {EnvironmentProviders {key:"{selected}",state,transport,destination:selected}}
        else {p {"Connect to this environment to manage its providers."}button {onclick:move |_|state.view().set(View::Connections),"Open connections"}}
    }}
}

#[component]
fn EnvironmentProviders(
    state: Store<UiModel>,
    transport: TransportHandle,
    destination: String,
) -> Element {
    let mut selected = use_signal(|| None::<String>);
    let mut pending = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let config_field = state.config();
    let config = config_field.read();
    let settings = config["settings"].clone();
    let live = config["providers"].as_array().cloned().unwrap_or_default();
    let rows = policy::instance_rows(&settings, &live, None);
    let row = policy::selected_instance(&rows, selected.read().as_deref(), None).cloned();
    let allowed = can_manage(state, &destination);
    drop(config);
    let update = EventHandler::new({
        let transport = transport.clone();
        let destination = destination.clone();
        move |input: Value| {
            if *pending.peek() || !can_manage(state, &destination) {
                return;
            }
            let Some(owner) = runtime::connection_owner(&transport, state) else {
                return;
            };
            pending.set(true);
            let saving = Saving(pending);
            error.set(None);
            let transport = transport.clone();
            spawn(async move {
                let _saving = saving;
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                let result = runtime::request_value(
                    transport.clone(),
                    state,
                    "server.updateSettings",
                    input,
                )
                .await;
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                match result {
                    Ok(_) if !runtime::live_config_active(&transport, state) => {
                        if let Err(cause) = runtime::request_value(
                            transport.clone(),
                            state,
                            "server.getConfig",
                            json!({}),
                        )
                        .await
                        {
                            if runtime::connection_owner(&transport, state).as_ref() == Some(&owner)
                            {
                                error.set(Some(cause));
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(cause) => error.set(Some(cause)),
                }
            });
        }
    });
    rsx! {
        if !allowed {p {class:"muted",role:"status","Provider settings are read-only. This connection needs providers:manage permission."}}
        if let Some(cause)=error.read().clone(){p {class:"error-banner",role:"alert","{cause}"}}
        if *pending.read(){p {role:"status","Saving provider settings…"}}
        div {class:"provider-settings-layout",
            nav {class:"provider-instance-list","aria-label":"Provider instances",
                for instance in &rows {button {key:"{instance.instance_id}","aria-label":format!("Select provider {}",label(instance)),class:if row.as_ref().is_some_and(|row|row.instance_id==instance.instance_id){"selected"}else{""},onclick:{let id=instance.instance_id.clone();move |_|selected.set(Some(id.clone()))},
                    strong {{label(instance)}}span {class:"muted",if policy::enabled(&instance.instance){"Enabled"}else{"Disabled"}}
                }}
            }
            if let Some(row)=row {
                ProviderEditor {key:"{row.instance_id}",state,transport:transport.clone(),environment:destination.clone(),row,live,pending:*pending.read(),allowed,onupdate:update}
            }else{p {"No configured provider instances."}}
        }
    }
}

#[component]
fn ProviderEditor(
    state: Store<UiModel>,
    transport: TransportHandle,
    environment: String,
    row: policy::InstanceRow,
    live: Vec<Value>,
    pending: bool,
    allowed: bool,
    onupdate: EventHandler<Value>,
) -> Element {
    let display = label(&row);
    let runtime = live
        .iter()
        .find(|provider| provider["instanceId"] == row.instance_id);
    let disabled = pending || !allowed;
    let fields = policy::fields(&row.driver, &row.instance["config"]);
    let modify = EventHandler::new({
        let row = row.clone();
        move |(key, value): (String, Value)| {
            let model = state.peek();
            let settings = &model.config["settings"];
            let live = model.config["providers"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let rows = policy::instance_rows(settings, live, None);
            let Some(latest) = rows
                .iter()
                .find(|latest| latest.instance_id == row.instance_id)
            else {
                return;
            };
            let mut instance = latest.instance.clone();
            if key == "enabled" {
                instance["enabled"] = value;
            } else {
                let Some(field) = policy::fields(&latest.driver, &instance["config"])
                    .iter()
                    .find(|field| field.key == key)
                else {
                    return;
                };
                if let Some(config) = policy::next_field_value(&instance["config"], field, &value) {
                    instance["config"] = config;
                } else {
                    instance
                        .as_object_mut()
                        .expect("provider instance object")
                        .remove("config");
                }
            }
            onupdate.call(policy::upsert_input(settings, latest, instance));
        }
    });
    rsx! {article {class:"provider-editor",
        header {h2 {"{display}"}label {input {r#type:"checkbox","aria-label":"Enable {display}",checked:policy::enabled(&row.instance),disabled,onchange:move |event|modify.call(("enabled".into(),json!(event.checked())))}"Enabled"}}
        if let Some(message)=runtime.and_then(|provider|provider["message"].as_str()).filter(|message|!message.is_empty()) {p {class:"muted","{message}"}}
        else if runtime.is_none() {p {class:"muted","This provider has not been checked."}}
        p {{runtime.and_then(|provider|provider["status"].as_str()).unwrap_or("unavailable")}}
        for field in fields {
            ProviderField {key:"{field.key}",field:field.clone(),config:row.instance["config"].clone(),disabled,onchange:modify}
        }
        if row.driver=="acpRegistry"&&row.instance["config"]["source"]=="local" {p {class:"muted","The executable runs on this environment."}}
        if let Some(provider)=runtime {crate::provider_auth::Authentication {state,transport:transport.clone(),provider:provider.clone(),environment:state.environments().read().records.values().find(|record|record.id.as_str()==environment).map(|record|record.label.clone()).unwrap_or(environment.clone()),allowed}}
        div {class:"provider-editor-actions",
            if !row.is_default {button {"aria-label":"Delete instance",disabled,onclick:{let id=row.instance_id.clone();move |_|onupdate.call(json!({"patch":{},"providerInstanceMutation":{"operation":"remove","instanceId":id}}))},"Delete instance"}}
            else if row.is_dirty==Some(true) {button {"aria-label":"Reset provider settings",disabled,onclick:{let id=row.instance_id.clone();let driver=row.driver.clone();move |_|{
                let mut providers=state.config().peek()["settings"]["providers"].clone();
                providers[&driver]=policy::default_providers()[&driver].clone();
                onupdate.call(json!({"patch":{"providers":providers},"providerInstanceMutation":{"operation":"remove","instanceId":id}}));
            }},"Reset provider settings"}}
        }
    }}
}

#[component]
fn ProviderField(
    field: policy::Field,
    config: Value,
    disabled: bool,
    onchange: EventHandler<(String, Value)>,
) -> Element {
    let incoming = config[&field.key].as_str().unwrap_or_default().to_owned();
    let mut draft = use_signal(|| incoming.clone());
    use_effect(use_reactive((&incoming,), move |(incoming,)| {
        draft.set(incoming)
    }));
    let key = field.key.clone();
    let control = match field.control.as_str() {
        "switch" => {
            rsx! {input {r#type:"checkbox","aria-label":field.label.clone(),disabled,checked:config[&field.key].as_bool().unwrap_or(field.default_boolean_value.unwrap_or(false)),onchange:move |event|onchange.call((key.clone(),json!(event.checked()))) }}
        }
        "select" => {
            let options = field.options.clone().unwrap_or_default();
            let fallback = options
                .first()
                .map(|option| option.value.clone())
                .unwrap_or_default();
            let selected = if incoming.is_empty() {
                fallback.clone()
            } else {
                incoming
            };
            rsx! {select {"aria-label":field.label.clone(),disabled,value:selected.clone(),onchange:move |event|{let value=event.value();onchange.call((key.clone(),json!(if value==fallback{String::new()}else{value})));},for option in options {option {value:option.value.clone(),selected:option.value==selected,"{option.label}"}}}}
        }
        "textarea" => {
            rsx! {textarea {"aria-label":field.label.clone(),disabled,value:draft.read().clone(),placeholder:field.placeholder.clone().unwrap_or_default(),oninput:move |event|{draft.set(event.value());onchange.call((key.clone(),json!(event.value())));}}}
        }
        _ => {
            rsx! {input {"aria-label":field.label.clone(),r#type:if field.control=="password"{"password"}else{"text"},autocomplete:"off",spellcheck:"false",disabled,value:draft.read().clone(),placeholder:field.placeholder.clone().unwrap_or_default(),oninput:move |event|draft.set(event.value()),onblur:move |_|{if *draft.peek()!=config[&key].as_str().unwrap_or_default(){onchange.call((key.clone(),json!(draft.peek().clone())));}}}}
        }
    };
    rsx! {div {class:"provider-field",div {strong {"{field.label}"}if let Some(description)=field.description{p {class:"muted","{description}"}}}{control}}}
}
