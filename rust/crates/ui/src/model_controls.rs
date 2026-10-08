use crate::runtime;
use dioxus::prelude::*;
use t3_client::{models, new_thread};
use t3_contracts::*;

#[component]
pub fn ModelOptions(
    selected: Option<ModelSelection>,
    #[props(default)] reported: Option<ModelSelection>,
    descriptors: Vec<ProviderOptionDescriptor>,
    onchange: EventHandler<ModelSelection>,
) -> Element {
    rsx! {
        div {class:"model-traits",
            for descriptor in descriptors {
                match descriptor {
                    ProviderOptionDescriptor::Select {fields} => {
                        let id=fields.id.clone();
                        let value=models::option_current_value(&ProviderOptionDescriptor::Select {fields:fields.clone()},selected.as_ref(),reported.as_ref()).and_then(|v|match v {ProviderOptionSelectionValue::String(v)=>Some(v.to_string()),_=>None}).unwrap_or_default();
                        let explicit=selected.as_ref().and_then(|s|s.options.as_ref()).is_some_and(|options|options.iter().any(|option|option.id==id));
                        let picker_value=if explicit {value.clone()}else{String::new()};
                        let default_value=models::option_current_value(&ProviderOptionDescriptor::Select {fields:fields.clone()},None,None).and_then(|v|match v {ProviderOptionSelectionValue::String(v)=>Some(v),_=>None});
                        let default_label=default_value.as_ref().and_then(|value|fields.options.iter().find(|option|&option.id==value)).map(|option|format!("Provider default ({})",option.label)).unwrap_or_else(||"Provider default".into());
                        let choices=if runtime::ui_surface()==new_thread::Surface::Mobile {models::mobile_option_choices(&ProviderOptionDescriptor::Select {fields:fields.clone()})}else{fields.options.iter().filter(|o|!fields.prompt_injected_values.as_ref().and_then(Option::as_ref).is_some_and(|values|values.contains(&o.id))).cloned().collect()};
                        let selection=selected.clone();
                        rsx!{label {"{fields.label}",select {"aria-label":"{fields.label}",value:"{picker_value}",onchange:move|e|{
                            if let Some(mut selection)=selection.clone() {
                                let options=selection.options.get_or_insert_default(); options.retain(|o|o.id!=id);
                                if let Ok(value)=TrimmedNonEmptyString::new(e.value()) {options.push(ProviderOptionSelection {id:id.clone(),value:ProviderOptionSelectionValue::String(value)});}
                                onchange.call(selection);
                            }
                        },
                            option {value:"",selected:picker_value.is_empty(),"{default_label}"}
                            for choice in choices {option {value:"{choice.id}",selected:choice.id.as_str()==picker_value,"{choice.label}"}}
                        }}}
                    },
                    ProviderOptionDescriptor::Boolean {fields} => {
                        let id=fields.id.clone();
                        let selection=selected.clone();
                        let checked=selected.as_ref().and_then(|s|s.options.as_ref()).and_then(|o|o.iter().find(|o|o.id==id)).and_then(|o|match o.value {ProviderOptionSelectionValue::Boolean(v)=>Some(v),_=>None}).unwrap_or(if id.as_str()=="fastMode"{false}else{fields.current_value.flatten().unwrap_or(false)});
                        rsx!{label {class:"trait-toggle",input {r#type:"checkbox",checked,onchange:move|e|{
                            if let Some(mut selection)=selection.clone() {
                                let options=selection.options.get_or_insert_default(); options.retain(|o|o.id!=id);
                                options.push(ProviderOptionSelection {id:id.clone(),value:ProviderOptionSelectionValue::Boolean(e.checked())}); onchange.call(selection);
                            }
                        }},"{fields.label}"}}
                    }
                }
            }
        }
    }
}
