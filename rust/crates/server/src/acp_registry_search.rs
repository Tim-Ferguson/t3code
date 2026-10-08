//! ACP Registry relevance ordering uses ECMAScript whitespace and UTF-16 order.
use crate::acp_registry_support::Agent;
fn whitespace(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}
pub(crate) fn rank(agent: &Agent, query: &str) -> Option<u8> {
    let normalized = t3_contracts::trim_wire_string(query).to_lowercase();
    if normalized.is_empty() {
        return Some(100);
    }
    let id = agent.id.to_lowercase();
    let name = agent.name.to_lowercase();
    let authors = agent.authors.join(" ").to_lowercase();
    let description = agent.description.to_lowercase();
    let terms = normalized
        .split(whitespace)
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    if id == normalized || name == normalized {
        return Some(0);
    }
    if id.starts_with(&normalized) || name.starts_with(&normalized) {
        return Some(10);
    }
    let identity = format!("{id} {name}");
    let tokens = identity
        .split(|character: char| !character.is_ascii_lowercase() && !character.is_ascii_digit())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if terms
        .iter()
        .all(|term| tokens.iter().any(|token| token.starts_with(term)))
    {
        return Some(20);
    }
    if terms
        .iter()
        .all(|term| id.contains(term) || name.contains(term))
    {
        return Some(30);
    }
    if terms.iter().all(|term| authors.contains(term)) {
        return Some(40);
    }
    let text = format!("{id} {name} {authors} {description}");
    terms.iter().all(|term| text.contains(term)).then_some(50)
}
pub(crate) fn compare(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    #[test]
    fn original_search_rank_and_utf16_order_witnesses() {
        for line in include_str!("../tests/fixtures/acp-registry-search.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let actual = if row["operation"] == "rank" {
                let value = &row["agent"];
                let agent = Agent {
                    id: value["id"].as_str().unwrap().into(),
                    name: value["name"].as_str().unwrap().into(),
                    version: "1".into(),
                    description: value["description"].as_str().unwrap().into(),
                    authors: serde_json::from_value(
                        value
                            .get("authors")
                            .cloned()
                            .unwrap_or(serde_json::json!([])),
                    )
                    .unwrap(),
                    license: None,
                    website: None,
                    repository: None,
                    icon: None,
                    binaries: Default::default(),
                    npx: None,
                    uvx: None,
                };
                serde_json::to_value(rank(&agent, row["query"].as_str().unwrap())).unwrap()
            } else {
                serde_json::json!(match compare(
                    row["left"].as_str().unwrap(),
                    row["right"].as_str().unwrap()
                ) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                })
            };
            assert_eq!(actual, row["output"], "{row}");
        }
    }
}
