//! The shortcut matcher against the original's decisions for thousands of events, contexts and
//! rule sets (recorded by `tools/generate_client_keybinding_fixtures.mjs`).
use serde_json::Value;
use std::collections::BTreeMap;
use t3_client::keybindings::{Context, KeyEvent, Platform, Rule, effective_shortcuts, from_wire, resolve_rule, shortcut_label};
use t3_contracts::ResolvedKeybindingsConfig;

fn context(value: &Value) -> Context {
    value.as_object().unwrap().iter().fold(Context::default(), |context, (name, on)| context.with(name, on.as_bool().unwrap()))
}
fn event(value: &Value) -> KeyEvent {
    KeyEvent {
        key: value["key"].as_str().unwrap().into(),
        code: value["code"].as_str().unwrap().into(),
        meta: value["metaKey"].as_bool().unwrap(),
        ctrl: value["ctrlKey"].as_bool().unwrap(),
        shift: value["shiftKey"].as_bool().unwrap(),
        alt: value["altKey"].as_bool().unwrap(),
        ..Default::default()
    }
}

#[test]
fn matching_precedence_labels_and_effective_shortcuts_agree_with_the_original() {
    let (mut events, mut contexts, mut platforms): (Vec<KeyEvent>, Vec<Context>, Vec<Platform>) = Default::default();
    let mut sets: BTreeMap<String, Vec<Rule>> = BTreeMap::new();
    let (mut resolved, mut labels) = (0, 0);
    for line in include_str!("fixtures/keybinding-matcher.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        if let Some(row) = row.as_array() {
            let (name, platform, ctx, ev) = (row[0].as_str().unwrap(), row[1].as_u64().unwrap() as usize, row[2].as_u64().unwrap() as usize, row[3].as_u64().unwrap() as usize);
            let actual = resolve_rule(&events[ev], &sets[name], platforms[platform], &contexts[ctx]).map(|rule| rule.command.as_str());
            assert_eq!(actual, row[4].as_str(), "{name} platform {platform} context {ctx} event {:?}", events[ev]);
            resolved += 1;
            continue;
        }
        match row["type"].as_str().unwrap() {
            "events" => events = row["events"].as_array().unwrap().iter().map(event).collect(),
            "contexts" => contexts = row["contexts"].as_array().unwrap().iter().map(context).collect(),
            "platforms" => platforms = row["platforms"].as_array().unwrap().iter().map(|name| Platform::parse(name.as_str().unwrap())).collect(),
            "rules" => {
                let config: ResolvedKeybindingsConfig = serde_json::from_value(row["rules"].clone()).unwrap();
                sets.insert(row["name"].as_str().unwrap().into(), from_wire(&config));
            }
            "label" => {
                let (rules, platform, ctx) = (&sets[row["rules"].as_str().unwrap()], platforms[row["platform"].as_u64().unwrap() as usize], &contexts[row["context"].as_u64().unwrap() as usize]);
                let command = row["command"].as_str().unwrap();
                assert_eq!(shortcut_label(rules, command, platform, ctx).as_deref(), row["label"].as_str(), "label of {command}");
                let effective: Vec<(String, bool, bool, bool, bool, bool)> = row["effective"].as_array().unwrap().iter().map(|s| (s["key"].as_str().unwrap().into(), s["metaKey"].as_bool().unwrap(), s["ctrlKey"].as_bool().unwrap(), s["shiftKey"].as_bool().unwrap(), s["altKey"].as_bool().unwrap(), s["modKey"].as_bool().unwrap())).collect();
                let actual: Vec<_> = effective_shortcuts(rules, command, platform, ctx).iter().map(|s| (s.key.clone(), s.meta, s.ctrl, s.shift, s.alt, s.modifier)).collect();
                assert_eq!(actual, effective, "effective shortcuts of {command}");
                labels += 1;
            }
            other => panic!("unknown row {other}"),
        }
    }
    assert!(resolved > 5000 && labels > 500, "{resolved} {labels}");
}
