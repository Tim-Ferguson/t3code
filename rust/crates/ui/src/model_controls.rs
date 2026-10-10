use crate::runtime;
use dioxus::prelude::*;
use t3_client::{models, new_thread};
use t3_contracts::*;

/// How the prompt takes over the primary effort control of a model whose effort can be asked for
/// in words (Claude's "ultrathink"): the values that are, whether the prompt asks for one now, and
/// whether it does in its own text rather than in the prefix this control adds.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PromptEffort {
    pub injected: Vec<String>,
    pub controlled: bool,
    pub in_body: bool,
}
impl PromptEffort {
    /// For the descriptors shown and the prompt now written; none when no effort is asked for in words.
    pub fn of(descriptors: &[ProviderOptionDescriptor], prompt: &str) -> Option<Self> {
        let injected: Vec<String> = descriptors.iter().find_map(|descriptor| match descriptor {
            ProviderOptionDescriptor::Select { fields } => Some(fields.prompt_injected_values.as_ref().and_then(Option::as_ref).map(|values| values.iter().map(ToString::to_string).collect::<Vec<_>>()).unwrap_or_default()),
            _ => None,
        })?;
        if injected.is_empty() || runtime::ui_surface() == new_thread::Surface::Mobile {
            return None;
        }
        let (controlled, in_body) = models::ultrathink_control(true, prompt);
        Some(Self { injected, controlled, in_body })
    }
}

#[component]
pub fn ModelOptions(
    selected: Option<ModelSelection>,
    #[props(default)] reported: Option<ModelSelection>,
    descriptors: Vec<ProviderOptionDescriptor>,
    onchange: EventHandler<ModelSelection>,
    /// Set where the prompt can carry the effort; `on_effort` is told the value chosen for it.
    #[props(default)] prompt_effort: Option<PromptEffort>,
    #[props(default)] on_effort: Option<EventHandler<String>>,
) -> Element {
    let primary = descriptors.iter().find_map(|descriptor| match descriptor { ProviderOptionDescriptor::Select { fields } => Some(fields.id.clone()), _ => None });
    rsx! {
        div {class:"model-traits",
            for descriptor in descriptors {
                match descriptor {
                    ProviderOptionDescriptor::Select {fields} => {
                        let id=fields.id.clone();
                        // The primary effort may be asked for in the prompt: ultrathink is then a choice
                        // here, shown as chosen while the prompt asks for it, and fixed while the word is in the text.
                        let words=prompt_effort.clone().filter(|_|primary.as_ref()==Some(&id));
                        let in_body=words.as_ref().is_some_and(|words|words.in_body);
                        let value=models::option_current_value(&ProviderOptionDescriptor::Select {fields:fields.clone()},selected.as_ref(),reported.as_ref()).and_then(|v|match v {ProviderOptionSelectionValue::String(v)=>Some(v.to_string()),_=>None}).unwrap_or_default();
                        let explicit=selected.as_ref().and_then(|s|s.options.as_ref()).is_some_and(|options|options.iter().any(|option|option.id==id));
                        let picker_value=match words.as_ref().filter(|words|words.controlled) {Some(words)=>words.injected.first().cloned().unwrap_or_default(),None=>if explicit {value.clone()}else{String::new()}};
                        let default_value=models::option_current_value(&ProviderOptionDescriptor::Select {fields:fields.clone()},None,None).and_then(|v|match v {ProviderOptionSelectionValue::String(v)=>Some(v),_=>None});
                        let default_label=default_value.as_ref().and_then(|value|fields.options.iter().find(|option|&option.id==value)).map(|option|format!("Provider default ({})",option.label)).unwrap_or_else(||"Provider default".into());
                        let choices=if runtime::ui_surface()==new_thread::Surface::Mobile {models::mobile_option_choices(&ProviderOptionDescriptor::Select {fields:fields.clone()})}else{fields.options.iter().filter(|o|words.is_some()||!fields.prompt_injected_values.as_ref().and_then(Option::as_ref).is_some_and(|values|values.contains(&o.id))).cloned().collect()};
                        let described=fields.options.iter().find(|option|option.id.as_str()==picker_value.as_str()||(picker_value.is_empty()&&option.id.as_str()==value.as_str())).and_then(|option|option.description.clone().flatten()).map(|text|text.to_string());
                        let selection=selected.clone();
                        let on_effort=on_effort.filter(|_|words.is_some());
                        rsx!{label {"{fields.label}",select {"aria-label":"{fields.label}",value:"{picker_value}",disabled:in_body,onchange:move|e|{
                            // A value the prompt carries is written to the prompt, not sent as an option.
                            if let Some(handler)=on_effort {
                                handler.call(e.value());
                                if words.as_ref().is_some_and(|words|words.injected.contains(&e.value())) {return;}
                            }
                            if let Some(mut selection)=selection.clone() {
                                let options=selection.options.get_or_insert_default(); options.retain(|o|o.id!=id);
                                if let Ok(value)=TrimmedNonEmptyString::new(e.value()) {options.push(ProviderOptionSelection {id:id.clone(),value:ProviderOptionSelectionValue::String(value)});}
                                onchange.call(selection);
                            }
                        },
                            option {value:"",selected:picker_value.is_empty(),"{default_label}"}
                            for choice in choices {option {value:"{choice.id}",selected:choice.id.as_str()==picker_value,title:choice.description.clone().flatten().map(|text|text.to_string()),"{choice.label}"}}
                        }
                        if in_body {span {class:"muted trait-note",role:"note","Your prompt contains \"ultrathink\" in the text. Remove it to change this option."}}
                        else if let Some(text)=described {span {class:"muted trait-note","{text}"}}
                        }}
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
