//! Reactions and rewriting words on a pull request: which reactions exist and how a pill reads, what
//! a press in flight shows before the host answers, and who may rewrite the title, the description
//! or a remark. Port of `pullRequestReactions.logic.ts` and `pullRequestEditing.logic.ts`.
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The picker's order, which is GitHub's: the two verdicts first, then the rest as it lists them.
pub const REACTION_ORDER: [&str; 8] = ["thumbs-up", "thumbs-down", "laugh", "hooray", "confused", "heart", "rocket", "eyes"];

pub fn reaction_emoji(content: &str) -> &'static str {
    match content {
        "thumbs-up" => "\u{1f44d}",
        "thumbs-down" => "\u{1f44e}",
        "laugh" => "\u{1f604}",
        "hooray" => "\u{1f389}",
        "confused" => "\u{1f615}",
        "heart" => "\u{2764}\u{fe0f}",
        "rocket" => "\u{1f680}",
        "eyes" => "\u{1f440}",
        _ => "",
    }
}
/// The spoken names GitHub uses in its own hover text.
pub fn reaction_name(content: &str) -> &'static str {
    match content {
        "thumbs-up" => "thumbs up",
        "thumbs-down" => "thumbs down",
        "laugh" => "laugh",
        "hooray" => "hooray",
        "confused" => "confused",
        "heart" => "heart",
        "rocket" => "rocket",
        "eyes" => "eyes",
        _ => "",
    }
}

const NAMED_ACTOR_LIMIT: usize = 3;
fn join_names(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [one, two] => format!("{one} and {two}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}
fn count_remainder(count: u64, named: bool) -> String {
    let (one, many) = if named { ("other", "others") } else { ("person", "people") };
    format!("{count} {}", if count == 1 { one } else { many })
}
/// Who reacted, in GitHub's sentence: the viewer reads as "You" and comes first while there is room
/// for them among the count, and the sentence never names more people than the count claims.
pub fn reaction_tooltip(reaction: &Value) -> String {
    let count = reaction["count"].as_u64().unwrap_or(0);
    let actors: Vec<String> = reaction["actors"].as_array().into_iter().flatten().filter_map(|actor| actor.as_str().map(str::to_owned)).collect();
    let room = (actors.len() as u64) < count;
    let names: Vec<String> = if reaction["viewerHasReacted"] == true && room { std::iter::once("You".to_owned()).chain(actors).collect() } else { actors };
    let shown: Vec<String> = names.into_iter().take((NAMED_ACTOR_LIMIT as u64).min(count) as usize).collect();
    let others = count.saturating_sub(shown.len() as u64);
    let mut parts = shown.clone();
    if others > 0 {
        parts.push(count_remainder(others, !shown.is_empty()));
    }
    format!("{} reacted with {} emoji", join_names(&parts), reaction_name(reaction["content"].as_str().unwrap_or_default()))
}

/// What the host last said, so a press in flight is forgotten the moment the real counts land.
pub fn signature(reactions: &[Value]) -> String {
    reactions.iter().map(|reaction| format!("{}:{}:{}", reaction["content"].as_str().unwrap_or_default(), reaction["count"], u8::from(reaction["viewerHasReacted"] == true))).collect::<Vec<_>>().join(" ")
}

/// The list as it is drawn while a press is still in flight: reading a change back from the host
/// takes seconds, and a pill that does not move until then reads as a press that did nothing.
pub fn apply_pending(reactions: &[Value], pending: &BTreeMap<String, bool>) -> Vec<Value> {
    if pending.is_empty() {
        return reactions.to_vec();
    }
    let mut by_content: BTreeMap<String, Value> = reactions.iter().filter_map(|reaction| Some((reaction["content"].as_str()?.to_owned(), reaction.clone()))).collect();
    for (content, reacted) in pending {
        match by_content.get(content).cloned() {
            None => {
                if *reacted {
                    by_content.insert(content.clone(), json!({"content":content,"count":1,"actors":[],"viewerHasReacted":true}));
                }
            }
            Some(current) => {
                if (current["viewerHasReacted"] == true) == *reacted {
                    continue;
                }
                let count = current["count"].as_i64().unwrap_or(0) + if *reacted { 1 } else { -1 };
                if count <= 0 {
                    by_content.remove(content);
                } else {
                    let mut next = current;
                    next["count"] = json!(count);
                    next["viewerHasReacted"] = json!(reacted);
                    by_content.insert(content.clone(), next);
                }
            }
        }
    }
    REACTION_ORDER.iter().filter_map(|content| by_content.remove(*content)).collect()
}

/// Hosts disagree about the case of a login and none of them treats two casings as two people.
fn same_login(one: Option<&str>, other: Option<&str>) -> bool {
    matches!((one, other), (Some(one), Some(other)) if one.trim().to_lowercase() == other.trim().to_lowercase())
}
/// Whether the title and description can be rewritten from here: the host can, and the reader wrote
/// the change request or may merge it.
pub fn can_edit_change_request(detail: &Value) -> bool {
    if detail["capabilities"]["edit"]["changeRequest"] != true {
        return false;
    }
    same_login(detail["viewer"].as_str(), detail["author"]["login"].as_str()) || detail["viewerPermissions"]["actions"].as_array().is_some_and(|actions| actions.iter().any(|action| action == "merge"))
}
/// Whether this remark can be rewritten from here. A review's own summary is left out: the hosts
/// disagree about what one even is.
pub fn can_edit_comment(detail: &Value, comment: &Value) -> bool {
    if detail["capabilities"]["edit"]["comment"] != true {
        return false;
    }
    if !matches!(comment["kind"].as_str(), Some("issue-comment" | "review-comment")) {
        return false;
    }
    same_login(detail["viewer"].as_str(), comment["author"]["login"].as_str())
}

pub const REACTION_FAILED: &str = "The reaction could not be saved";
pub const TITLE_FAILED: &str = "The title could not be saved";
pub const TITLE_REFUSED: &str = "The host refused the new title.";
pub const BODY_FAILED: &str = "Could not save the description";
pub const COMMENT_FAILED: &str = "Could not save the comment";
