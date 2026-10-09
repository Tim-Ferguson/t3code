//! Plan and build interaction modes on an ACP session, and the form elicitations an agent asks.
//!
//! An agent has no notion of T3 Code's plan mode, so a plan turn switches the session's own mode
//! (and any mode-like select option) to the one named `plan` or `architect`, and the next build turn
//! puts back what the session had before (`configureSession` in AcpAdapterV2).
use serde_json::{Map, Value, json};

const PLAN_CHOICES: [&str; 2] = ["plan", "architect"];

/// What the session was set to before a plan turn, restored by the next build turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NativeBuild {
    mode: Option<String>,
    options: Vec<(String, Value)>,
}

/// The values a select option offers, groups flattened.
pub(crate) fn option_choices(option: &Value) -> Vec<&str> {
    option["options"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| match entry["options"].as_array() {
            Some(group) => group.iter().filter_map(|choice| choice["value"].as_str()).collect::<Vec<_>>(),
            None => entry["value"].as_str().into_iter().collect(),
        })
        .collect()
}

fn plan_sensitive(setup: &Value) -> impl Iterator<Item = &Value> {
    setup["configOptions"].as_array().into_iter().flatten().filter(|option| option["type"] == "select" && matches!(option["category"].as_str(), Some("mode" | "collaboration_mode")))
}

fn mode_ids(setup: &Value) -> Vec<&str> {
    setup["modes"]["availableModes"].as_array().into_iter().flatten().filter_map(|mode| mode["id"].as_str()).collect()
}

/// The session mode (`_t3/session-mode`) and select options to set so the session matches the turn's
/// interaction mode, given what the session reports now. `remembered` holds the pre-plan state.
pub(crate) fn interaction_changes(setup: &Value, plan: bool, remembered: &mut Option<NativeBuild>) -> Map<String, Value> {
    let mut changes = Map::new();
    let current_mode = setup["modes"]["currentModeId"].as_str();
    if plan {
        if remembered.is_none() {
            *remembered = Some(NativeBuild {
                mode: current_mode.map(ToOwned::to_owned),
                options: plan_sensitive(setup).filter_map(|option| Some((option["id"].as_str()?.to_owned(), option["currentValue"].clone()))).collect(),
            });
        }
        if let Some(plan_mode) = mode_ids(setup).into_iter().find(|id| PLAN_CHOICES.contains(id)) {
            if current_mode != Some(plan_mode) {
                changes.insert("_t3/session-mode".into(), json!(plan_mode));
            }
        }
        for option in plan_sensitive(setup) {
            if let Some(requested) = option_choices(option).into_iter().find(|choice| PLAN_CHOICES.contains(choice)) {
                if option["currentValue"] != requested {
                    changes.insert(option["id"].as_str().unwrap().to_owned(), json!(requested));
                }
            }
        }
    } else if let Some(build) = remembered.take() {
        if let Some(mode) = build.mode.as_deref() {
            if current_mode != Some(mode) && mode_ids(setup).contains(&mode) {
                changes.insert("_t3/session-mode".into(), json!(mode));
            }
        }
        for (id, value) in build.options {
            let option = setup["configOptions"].as_array().into_iter().flatten().find(|option| option["type"] == "select" && option["id"] == id);
            if let (Some(option), Some(saved)) = (option, value.as_str()) {
                if option["currentValue"] != value && option_choices(option).contains(&saved) {
                    changes.insert(id, value);
                }
            }
        }
    }
    changes
}

/// The questions of a form elicitation: one per property of the requested schema.
pub(crate) fn elicitation_questions(request: &Value) -> Vec<Value> {
    let text = |value: &Value, fallback: &str| value.as_str().map(str::trim).filter(|text| !text.is_empty()).unwrap_or(fallback).to_owned();
    request["requestedSchema"]["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, (id, property))| {
            let values: Vec<&str> = property["enum"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let options: Vec<Value> = if !values.is_empty() {
                values.into_iter().map(|value| json!({"label":value,"description":value})).collect()
            } else if property["type"] == "boolean" {
                vec![json!({"label":"true","description":"Yes"}), json!({"label":"false","description":"No"})]
            } else {
                vec![]
            };
            json!({"id":id,"header":text(&property["title"],&format!("Question {}",index + 1)),"question":text(&property["description"],request["message"].as_str().unwrap_or("")),"options":options})
        })
        .collect()
}

/// The answers the agent's schema asks for, as `content` of an accepted elicitation.
pub(crate) fn elicitation_content(answers: &Value, allowed: &[String]) -> Value {
    let mut content = Map::new();
    for (key, value) in answers.as_object().into_iter().flatten() {
        if !allowed.contains(key) {
            continue;
        }
        match value {
            Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                content.insert(key.clone(), value.clone());
            }
            Value::Array(items) => {
                content.insert(key.clone(), Value::Array(items.iter().filter(|item| item.is_string()).cloned().collect()));
            }
            _ => {}
        }
    }
    Value::Object(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(mode: &str, effort: &str) -> Value {
        json!({"modes":{"currentModeId":mode,"availableModes":[{"id":"code"},{"id":"plan"}]},
               "configOptions":[{"id":"collab","type":"select","category":"collaboration_mode","currentValue":effort,"options":[{"value":"default"},{"group":"g","options":[{"value":"plan"}]}]},
                                {"id":"other","type":"select","category":"model","currentValue":"a","options":[{"value":"a"}]}]})
    }
    #[test]
    fn plan_turns_switch_the_session_and_the_next_build_turn_restores_it() {
        let mut remembered = None;
        let into_plan = interaction_changes(&setup("code", "default"), true, &mut remembered);
        assert_eq!(Value::Object(into_plan), json!({"_t3/session-mode":"plan","collab":"plan"}));
        // A second plan turn on the already switched session changes nothing and keeps what was saved.
        assert!(interaction_changes(&setup("plan", "plan"), true, &mut remembered).is_empty());
        let back = interaction_changes(&setup("plan", "plan"), false, &mut remembered);
        assert_eq!(Value::Object(back), json!({"_t3/session-mode":"code","collab":"default"}));
        assert!(remembered.is_none());
        assert!(interaction_changes(&setup("code", "default"), false, &mut remembered).is_empty());
    }
    #[test]
    fn elicitation_properties_become_questions_and_only_asked_keys_are_answered() {
        let request = json!({"message":"Pick","requestedSchema":{"properties":{"a":{"type":"string","title":"Color","description":"Which?","enum":["red","blue"]},"b":{"type":"boolean"},"c":{"type":"string","title":"  "}}}});
        let questions = elicitation_questions(&request);
        assert_eq!(questions[0], json!({"id":"a","header":"Color","question":"Which?","options":[{"label":"red","description":"red"},{"label":"blue","description":"blue"}]}));
        assert_eq!(questions[1], json!({"id":"b","header":"Question 2","question":"Pick","options":[{"label":"true","description":"Yes"},{"label":"false","description":"No"}]}));
        assert_eq!(questions[2]["header"], "Question 3");
        let content = elicitation_content(&json!({"a":"red","b":true,"x":"dropped","c":["u",1]}), &["a".into(), "b".into(), "c".into()]);
        assert_eq!(content, json!({"a":"red","b":true,"c":["u"]}));
    }
}
