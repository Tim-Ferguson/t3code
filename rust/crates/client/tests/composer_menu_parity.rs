//! The composer's menus replayed against what the original answered
//! (`rust/tools/generate_client_composer_menu_fixtures.mjs`): which menu a prompt asks for, and
//! the order of slash commands and skills for a query.
use serde_json::Value;
use t3_client::composer_menu::{self as menu, SlashItem, Skill};

fn rows(function: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/composer_menu.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["fn"] == function).collect();
    assert!(!rows.is_empty(), "no rows for {function}");
    rows
}
/// The original counts UTF-16 units; every prompt here is in the basic plane, so a unit is a char.
fn byte(text: &str, units: u64) -> usize {
    text.char_indices().nth(units as usize).map_or(text.len(), |(at, _)| at)
}
fn unit(text: &str, byte: usize) -> u64 {
    text[..byte].chars().count() as u64
}

#[test]
fn the_menu_a_prompt_asks_for_is_the_one_the_original_opens() {
    for row in rows("trigger") {
        let text = row["input"]["text"].as_str().unwrap();
        let cursor = row["input"]["cursor"].as_u64().unwrap();
        let found = menu::detect_trigger(text, byte(text, cursor));
        let shown = found.as_ref().map(|trigger| serde_json::json!({"kind":trigger.kind.name(),"query":trigger.query,"rangeStart":unit(text,trigger.range_start),"rangeEnd":unit(text,trigger.range_end)}));
        assert_eq!(shown.unwrap_or(Value::Null), row["output"], "{}", row["input"]);
    }
}

fn items() -> Vec<(String, SlashItem)> {
    let command = |name: &str, description: &str| (format!("slash:{name}"), SlashItem::Command { command: name.into(), description: description.into() });
    let provider = |name: &str, description: &str| (format!("provider:{name}"), SlashItem::Provider { name: name.into(), description: description.into(), provider: "codex".into() });
    let skill = |name: &str, display: Option<&str>, short: Option<&str>, description: Option<&str>, scope: Option<&str>| {
        (format!("skill:{name}"), SlashItem::Skill { provider: "codex".into(), skill: Skill { name: name.into(), display_name: display.map(Into::into), short_description: short.map(Into::into), description: description.map(Into::into), scope: scope.map(Into::into), enabled: true, user_invocable: None } })
    };
    vec![
        command("model", "Switch response model for this thread"),
        command("plan", "Switch this thread into plan mode"),
        command("default", "Switch this thread back to normal build mode"),
        provider("compact", "Summarize the conversation to free context"),
        provider("review", "Review the current changes"),
        provider("init", "Create an AGENTS.md file"),
        provider("model-config", "Configure models"),
        skill("deploy-app", Some("Deploy App"), Some("Ship to production"), None, None),
        skill("review_pr", None, None, Some("Review a pull request thoroughly"), Some("personal")),
    ]
}

#[test]
fn slash_commands_and_skills_rank_as_the_original_ranks_them() {
    let items = items();
    let list: Vec<SlashItem> = items.iter().map(|(_, item)| item.clone()).collect();
    for row in rows("slash") {
        let found: Vec<&str> = menu::search_slash(&list, row["input"]["query"].as_str().unwrap()).into_iter().map(|index| items[index].0.as_str()).collect();
        let expected: Vec<&str> = row["output"].as_array().unwrap().iter().map(|id| id.as_str().unwrap()).collect();
        assert_eq!(found, expected, "{}", row["input"]);
    }
    for row in rows("position") {
        let found: Vec<&str> = menu::items_for_position(&list, row["input"]["atStart"].as_bool().unwrap()).into_iter().map(|index| items[index].0.as_str()).collect();
        let expected: Vec<&str> = row["output"].as_array().unwrap().iter().map(|id| id.as_str().unwrap()).collect();
        assert_eq!(found, expected, "{}", row["input"]);
    }
}

#[test]
fn skills_are_searched_as_the_original_searches_them() {
    for row in rows("skills") {
        let skills: Vec<Skill> = row["input"]["skills"].as_array().unwrap().iter().map(|skill| Skill::from_json(skill).unwrap()).collect();
        let paths: Vec<&str> = row["input"]["skills"].as_array().unwrap().iter().map(|skill| skill["path"].as_str().unwrap()).collect();
        let found: Vec<&str> = menu::search_skills(&skills, row["input"]["query"].as_str().unwrap()).into_iter().map(|index| paths[index]).collect();
        let expected: Vec<&str> = row["output"].as_array().unwrap().iter().map(|path| path.as_str().unwrap()).collect();
        assert_eq!(found, expected, "{}", row["input"]["query"]);
    }
}

#[test]
fn picking_replaces_the_token_and_standalone_mode_commands_are_recognised() {
    assert_eq!(menu::replace_range("see @src/li", 4, 11, "@src/lib.rs "), ("see @src/lib.rs ".to_owned(), 16));
    assert_eq!(menu::replace_range("abc", 9, 1, "X"), ("abcX".to_owned(), 4));
    assert_eq!(menu::standalone_command(" /Plan "), Some("plan"));
    assert_eq!(menu::standalone_command("/default"), Some("default"));
    assert_eq!(menu::standalone_command("/plan now"), None);
}

#[test]
fn model_search_scores_as_the_original_scores() {
    use t3_client::model_picker::{Searchable, score_search};
    for row in rows("pickerScore") {
        let model = &row["input"]["model"];
        let text = |key: &str| model[key].as_str();
        let searchable = Searchable { driver: text("driverKind").unwrap(), provider_display_name: text("providerDisplayName").unwrap(), name: text("name").unwrap(), short_name: text("shortName"), sub_provider: text("subProvider"), favorite: model["isFavorite"] == true };
        assert_eq!(score_search(&searchable, row["input"]["query"].as_str().unwrap()), row["output"].as_f64(), "{}", row["input"]);
    }
}
