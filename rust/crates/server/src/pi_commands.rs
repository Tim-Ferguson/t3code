//! What the Pi coding agent tells T3 Code about itself, and how T3 Code's inputs reach it: `/compact`,
//! the commands and skills `get_commands` lists, `$skill` references, model slugs and the thinking ladder.
//! Mirrors `PiCommands.ts`, `piThinkingCapabilities.ts` and `parsePiModelSlug` of `PiRpc.ts`.
use serde_json::{Value, json};

/// Pi's `get_commands` omits the TUI builtins. `/compact` is advertised so T3 can map it to the RPC call.
pub fn compact_slash_command() -> Value {
    json!({"name":"compact","description":"Summarize the conversation and reduce context usage","input":{"hint":"Optional instructions"}})
}

/// `parsePiCompactCommand`: `None` when the text is not `/compact`, else its custom instructions, if any.
pub fn parse_compact_command(text: &str) -> Option<Option<String>> {
    let trimmed = text.trim();
    if trimmed == "/compact" {
        return Some(None);
    }
    let rest = trimmed.strip_prefix("/compact")?;
    if rest.is_empty() {
        return Some(None);
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let instructions = rest.trim();
    Some((!instructions.is_empty()).then(|| instructions.to_owned()))
}

/// `withPiBuiltinSlashCommands`.
pub fn with_builtin_slash_commands(commands: Vec<Value>) -> Vec<Value> {
    std::iter::once(compact_slash_command()).chain(commands.into_iter().filter(|command| command["name"] != "compact")).collect()
}

/// A trimmed, non-empty string field (`recordString` of `PiCommands.ts`).
fn trimmed(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned)
}
fn normalize_skill_scope(scope: Option<String>) -> Option<String> {
    let scope = scope?;
    Some(match scope.trim().to_lowercase().as_str() {
        "global" | "personal" => "user".to_owned(),
        "workspace" | "local" => "project".to_owned(),
        _ => scope,
    })
}
/// `parsePiDiscoveredCommands`: the slash commands and skills of a `get_commands` payload.
pub fn parse_discovered_commands(data: &Value) -> (Vec<Value>, Vec<Value>) {
    let (mut slash_commands, mut skills) = (vec![], vec![]);
    let Some(commands) = data.get("commands").and_then(Value::as_array) else { return (slash_commands, skills) };
    for command in commands {
        let Some(command_name) = trimmed(command, "name") else { continue };
        let description = trimmed(command, "description");
        if trimmed(command, "source").as_deref() == Some("skill") {
            let name = command_name.strip_prefix("skill:").unwrap_or(&command_name).to_owned();
            if name.is_empty() {
                continue;
            }
            let (source_info, interface) = (command.get("sourceInfo").cloned().unwrap_or(Value::Null), command.get("interface").cloned().unwrap_or(Value::Null));
            let path = trimmed(&source_info, "path").or_else(|| trimmed(command, "path")).unwrap_or_else(|| format!("pi:skill:{name}"));
            let scope = normalize_skill_scope(trimmed(&source_info, "scope").or_else(|| trimmed(command, "location")));
            let display_name = trimmed(command, "displayName").or_else(|| trimmed(&source_info, "displayName")).or_else(|| trimmed(&interface, "displayName"));
            let short_description = trimmed(command, "shortDescription").or_else(|| trimmed(&source_info, "shortDescription")).or_else(|| trimmed(&interface, "shortDescription"));
            let mut skill = json!({"name":name,"path":path,"enabled":true});
            for (key, value) in [("description", description), ("scope", scope), ("displayName", display_name), ("shortDescription", short_description)] {
                if let Some(value) = value {
                    skill[key] = json!(value);
                }
            }
            skills.push(skill);
            continue;
        }
        let mut slash = json!({"name":command_name});
        if let Some(description) = description {
            slash["description"] = json!(description);
        }
        slash_commands.push(slash);
    }
    (slash_commands, skills)
}

/// `expandPiSkillReference`. Pi expands skills only through leading `/skill:name` commands. T3 stores skill
/// chips as `$name`, so every known `$skill` is hoisted to that position and the rest of the prompt is kept.
pub fn expand_skill_reference(text: &str, skill_names: &std::collections::HashSet<String>) -> String {
    let mut found: Vec<(String, usize, usize)> = vec![];
    let mut previous_whitespace = true;
    let mut chars = text.char_indices().peekable();
    while let Some((start, character)) = chars.next() {
        if character == '$' && previous_whitespace {
            let mut end = start + 1;
            while let Some((at, next)) = chars.peek().copied() {
                if next.is_whitespace() {
                    break;
                }
                end = at + next.len_utf8();
                chars.next();
            }
            let name = &text[start + 1..end];
            if !name.is_empty() && skill_names.contains(name) {
                found.push((name.to_owned(), start, end));
            }
            previous_whitespace = false;
            continue;
        }
        previous_whitespace = character.is_whitespace();
    }
    if found.is_empty() {
        return text.to_owned();
    }
    let mut ordered: Vec<&str> = vec![];
    for (name, _, _) in &found {
        if !ordered.contains(&name.as_str()) {
            ordered.push(name);
        }
    }
    let mut body = text.to_owned();
    for (_, start, end) in found.iter().rev() {
        body.replace_range(*start..*end, "");
    }
    let body = body.trim();
    let prefix = ordered.iter().map(|name| format!("/skill:{name}")).collect::<Vec<_>>().join(" ");
    if body.is_empty() { prefix } else { format!("{prefix} {body}") }
}

/// `parsePiModelSlug`: `provider/model`, split at the first separator. `None` for a slug without a usable
/// one, so a caller can reject the selection instead of leaving Pi on its configured default.
pub fn parse_model_slug(slug: &str) -> Option<(&str, &str)> {
    let separator = slug.find('/')?;
    (separator > 0 && separator < slug.len() - 1).then(|| (&slug[..separator], &slug[separator + 1..]))
}

const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
pub fn is_thinking_level(level: &str) -> bool {
    THINKING_LEVELS.contains(&level)
}
fn thinking_label(level: &str) -> &'static str {
    match level {
        "off" => "Off",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        _ => "Max",
    }
}
/// `getSupportedThinkingLevels` of `@earendil-works/pi-ai`. A reasoning model always exposes off through high unless
/// a map entry is null. Extra High and Max appear only when the map has a non-null entry.
fn supported_thinking_levels(model: &Value) -> Vec<&'static str> {
    if model.get("reasoning") != Some(&Value::Bool(true)) {
        return vec![];
    }
    let map = model.get("thinkingLevelMap").and_then(Value::as_object);
    THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| match map.and_then(|map| map.get(*level)) {
            Some(Value::Null) => false,
            mapped if *level == "xhigh" || *level == "max" => mapped.is_some(),
            _ => true,
        })
        .collect()
}
/// `clampThinkingLevel` of `@earendil-works/pi-ai`: the requested level, else the nearest higher one the model has, else the nearest lower.
fn clamp_thinking_level(input: &Value, available: &[&'static str]) -> Option<&'static str> {
    let requested = THINKING_LEVELS.iter().position(|level| Some(*level) == input.as_str())?;
    if let Some(exact) = available.iter().copied().find(|level| *level == THINKING_LEVELS[requested]) {
        return Some(exact);
    }
    (requested + 1..THINKING_LEVELS.len())
        .find_map(|index| available.iter().copied().find(|level| *level == THINKING_LEVELS[index]))
        .or_else(|| (0..requested).rev().find_map(|index| available.iter().copied().find(|level| *level == THINKING_LEVELS[index])))
        .or_else(|| available.first().copied())
}
/// `thinkingCapabilitiesForPiModel`.
pub fn thinking_capabilities(model: &Value, default_level: &Value) -> Value {
    let levels = supported_thinking_levels(model);
    if levels.is_empty() {
        return json!({"optionDescriptors":[]});
    }
    let default = clamp_thinking_level(default_level, &levels);
    let options: Vec<Value> = levels
        .iter()
        .map(|level| {
            let mut option = json!({"id":level,"label":thinking_label(level)});
            if Some(*level) == default {
                option["isDefault"] = json!(true);
            }
            option
        })
        .collect();
    json!({"optionDescriptors":[{"id":"thinking","label":"Thinking","type":"select","options":options}]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    fn fixture() -> Vec<Value> {
        serde_json::from_str(include_str!("../tests/fixtures/pi-functions.json")).unwrap()
    }
    #[test]
    fn every_pure_function_agrees_with_the_original_on_its_recorded_cases() {
        let mut checked = std::collections::HashMap::<String, usize>::new();
        for row in fixture() {
            let operation = row["operation"].as_str().unwrap().to_owned();
            let input = &row["input"];
            let actual = match operation.as_str() {
                "compact" => match parse_compact_command(input["text"].as_str().unwrap()) {
                    None => Value::Null,
                    Some(None) => json!({}),
                    Some(Some(instructions)) => json!({"customInstructions":instructions}),
                },
                "discovered" => {
                    let (slash, skills) = parse_discovered_commands(&input["data"]);
                    json!({"slashCommands":slash,"skills":skills})
                }
                "builtins" => json!(with_builtin_slash_commands(input["commands"].as_array().unwrap().clone())),
                "expand" => json!(expand_skill_reference(input["text"].as_str().unwrap(), &input["skills"].as_array().unwrap().iter().map(|name| name.as_str().unwrap().to_owned()).collect::<HashSet<_>>())),
                "slug" => parse_model_slug(input["slug"].as_str().unwrap()).map_or(Value::Null, |(provider, model)| json!({"provider":provider,"modelId":model})),
                "thinking" => thinking_capabilities(&input["model"], &input["default"]),
                "launch" => continue,
                other => panic!("unknown operation {other}"),
            };
            assert_eq!(actual, row["output"], "{row}");
            *checked.entry(operation).or_default() += 1;
        }
        for (operation, minimum) in [("compact", 10), ("discovered", 4), ("builtins", 2), ("expand", 10), ("slug", 6), ("thinking", 8)] {
            assert!(checked.get(operation).copied().unwrap_or(0) >= minimum, "{operation}: {checked:?}");
        }
    }
}
