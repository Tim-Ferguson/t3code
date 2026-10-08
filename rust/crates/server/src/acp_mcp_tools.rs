//! MCP identity recovery at the generic ACP tool boundary.
use regex::Regex;
use serde_json::{Value, json};
use std::sync::OnceLock;

fn trimmed(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(t3_contracts::trim_wire_string)
        .filter(|value| !value.is_empty())
}
fn names() -> &'static Vec<String> {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| serde_json::from_str(include_str!("acp-mcp-tool-names.json")).unwrap())
}
fn known(tool: &str) -> bool {
    names().iter().any(|name| name == tool)
}
fn t3_server(server: &str) -> bool {
    matches!(
        server.to_ascii_lowercase().as_str(),
        "t3code" | "t3-code" | "t3_code" | "t3 code"
    )
}
struct Patterns {
    qualified: Regex,
    prefix: Regex,
    suffix: Regex,
    bare: Regex,
    fallback: Regex,
}
fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(||Patterns {
        qualified:Regex::new(r"^(?i:mcp)__([^\n\r\u{2028}\u{2029}]+?)__([^\n\r\u{2028}\u{2029}]+)$").unwrap(),
        prefix:Regex::new(r"^(?i:(?:mcp[-_]{1,2})?t3[-_ ]?code)[-_.:/ ]{1,3}(?<tool>[A-Za-z0-9][A-Za-z0-9_.-]*)(?::[^\n\r\u{2028}\u{2029}]*)?$").unwrap(),
        suffix:Regex::new(r"^(?<tool>[A-Za-z0-9][A-Za-z0-9_.-]*?)(?: (?i:\(t3[-_ ]?code MCP Server\))(?::|$)|[-_.](?i:t3[-_ ]?code)$)").unwrap(),
        bare:Regex::new(r"^(?<tool>[A-Za-z0-9_]+)(?::[\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]|$)").unwrap(),
        fallback:Regex::new(r#"(?:^|[\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}"'=])acp-mcp-call[\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}"']+([A-Za-z0-9_.-]+)(?:[\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]+([^\n\r\u{2028}\u{2029}]+?))?[\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]*$"#).unwrap(),
    })
}
fn fallback_input(value: &str) -> Option<Value> {
    let value = t3_contracts::trim_wire_string(value);
    for candidate in std::iter::once(value).chain(
        value
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\'')),
    ) {
        if let Ok(mut result) = serde_json::from_str::<Value>(candidate) {
            if let Some(text) = result.as_str() {
                result = serde_json::from_str(text).ok()?;
            }
            if result.is_object() {
                return Some(result);
            }
        }
    }
    None
}
pub(crate) fn identity(tool: &Value, embedded: &[String]) -> Option<Value> {
    let input = &tool["data"]["rawInput"];
    let meta = &tool["data"]["meta"];
    let server = trimmed(&input["server"]);
    let name = trimmed(&input["tool"]);
    if meta["is_mcp_tool_call"] == true {
        if let (Some(server), Some(name)) = (server, name) {
            return Some(json!({"server":server,"tool":name}));
        }
    }
    let server = trimmed(&meta["serverId"]);
    let meta_name = trimmed(&meta["toolName"]);
    if server.is_some_and(t3_server) {
        if let Some(candidate) = meta_name {
            for name in names() {
                let boundary = candidate.len().checked_sub(name.len() + 1);
                if candidate == name
                    || (candidate.ends_with(name)
                        && boundary.is_some_and(|index| {
                            !candidate.as_bytes()[index].is_ascii_alphanumeric()
                        }))
                {
                    return Some(json!({"server":"t3-code","tool":name}));
                }
            }
        }
    }
    let goose = &meta["goose"]["toolCall"];
    if server.is_some_and(|server| !t3_server(server))
        || trimmed(&goose["extensionName"]).is_some_and(|server| !t3_server(server))
    {
        let (Some(server), Some(name)) = (server, meta_name) else {
            return None;
        };
        let name = name
            .strip_prefix(&format!("mcp__{server}__"))
            .or_else(|| name.strip_prefix(&format!("mcp::{server}::")))
            .unwrap_or(name);
        return (!name.is_empty()).then(|| json!({"server":server,"tool":name}));
    }
    for candidate in [
        &meta["toolName"],
        &meta["claudeCode"]["toolName"],
        &goose["toolName"],
        &tool["data"]["title"],
    ] {
        let Some(candidate) = trimmed(candidate) else {
            continue;
        };
        if let Some(capture) = patterns().qualified.captures(candidate) {
            if !t3_server(&capture[1]) {
                return Some(json!({"server":&capture[1],"tool":&capture[2]}));
            }
        }
        if let Some(capture) = patterns()
            .prefix
            .captures(candidate)
            .or_else(|| patterns().suffix.captures(candidate))
            .or_else(|| patterns().bare.captures(candidate))
        {
            let name = &capture["tool"];
            if known(name) {
                return Some(json!({"server":"t3-code","tool":name}));
            }
        }
    }
    for command in [tool["command"].as_str(), tool["data"]["title"].as_str()]
        .into_iter()
        .flatten()
        .chain(embedded.iter().map(String::as_str))
    {
        if let Some(capture) = patterns().fallback.captures(command) {
            let mut identity = json!({"server":"t3-code","tool":&capture[1]});
            if let Some(input) = capture
                .get(2)
                .and_then(|value| fallback_input(value.as_str()))
            {
                identity["input"] = input;
            }
            return Some(identity);
        }
    }
    None
}
pub(crate) fn output(value: &Value) -> Value {
    let Some(record) = value.as_object() else {
        return value.clone();
    };
    if !record.contains_key("result") && !record.contains_key("error") {
        return value.clone();
    }
    let result = record
        .get("result")
        .and_then(Value::as_object)
        .and_then(|result| {
            result
                .get("structuredContent")
                .filter(|value| !value.is_null())
                .or_else(|| result.get("content").filter(|value| !value.is_null()))
        });
    match value["error"]["message"].as_str() {
        Some(error) => {
            let mut result_value = json!({"error":error});
            if let Some(result) = result {
                result_value["result"] = result.clone();
            }
            result_value
        }
        None => result.unwrap_or(value).clone(),
    }
}
fn normalize_text(value: &Value) -> Option<String> {
    let value = trimmed(value)?;
    let mut result = String::new();
    let mut whitespace = false;
    for character in value.chars() {
        if t3_contracts::trim_wire_string(&character.to_string()).is_empty() {
            whitespace = true;
        } else {
            if whitespace && !result.is_empty() {
                result.push(' ');
            }
            whitespace = false;
            result.push(character);
        }
    }
    (!result.is_empty() && result.encode_utf16().count() <= 160).then_some(result)
}
fn normalize_url(value: &Value) -> Option<String> {
    let value = value.as_str()?;
    if value.encode_utf16().count() > 4096 {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    let value = url.as_str();
    (matches!(url.scheme(), "http" | "https") && value.encode_utf16().count() <= 4096)
        .then(|| value.to_owned())
}
fn known_qualified(server: &str, tool: &str) -> bool {
    // Only the canonical qualified T3 tool spelling is used by this presentation
    // boundary; generic provider recovery performs broader title matching above.
    t3_server(server) && known(tool)
}
pub(crate) fn presentation(input: &Value) -> Value {
    let qualified = input["toolName"]
        .as_str()
        .and_then(|value| patterns().qualified.captures(value));
    // Choose the nullish input before normalization: an explicit invalid value
    // must not fall through to a different asserted server identity.
    let server_value = input
        .get("serverName")
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| qualified.as_ref().map(|capture| json!(&capture[1])))
        .unwrap_or_else(|| input["serverDisplayName"].clone());
    let server = normalize_text(&server_value);
    let tool = qualified
        .as_ref()
        .map(|capture| json!(&capture[2]))
        .as_ref()
        .unwrap_or(&input["toolName"])
        .clone();
    let tool = normalize_text(&tool);
    if let (Some(server), Some(tool)) = (&server, &tool) {
        if known_qualified(server, tool) {
            return json!({});
        }
    }
    let title = normalize_text(&input["title"]).or_else(|| {
        if server.is_some() {
            tool.as_ref().and_then(|tool| {
                normalize_text(&json!(
                    tool.split(['_', '-'])
                        .filter(|value| !value.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                ))
            })
        } else {
            None
        }
    });
    let Some(server) = server else {
        return title
            .map(|title| json!({"title":title}))
            .unwrap_or(json!({}));
    };
    let source = &input["source"];
    let name = normalize_text(&input["serverDisplayName"])
        .or_else(|| normalize_text(&source["name"]))
        .or_else(|| {
            normalize_text(&json!(
                server
                    .split(['_', '-'])
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            ))
        })
        .unwrap_or_else(|| server.clone());
    let logo = normalize_url(&input["iconUrl"]).or_else(|| normalize_url(&source["logoUrl"]));
    let dark =
        normalize_url(&input["iconUrlDark"]).or_else(|| normalize_url(&source["logoUrlDark"]));
    let mut result = json!({});
    if let Some(title) = title {
        result["title"] = json!(title);
    }
    let mut source =
        json!({"key":format!("mcp:{}",server.to_lowercase()),"name":name,"kind":"integration"});
    if let Some(logo) = logo {
        let mut icon = json!({"_tag":"themed-logo","logoUrl":logo});
        if let Some(dark) = dark {
            icon["logoUrlDark"] = json!(dark);
        }
        result["toolIcon"] = icon.clone();
        source["icon"] = icon;
    }
    result["toolSource"] = source;
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_mcp_identity_output_and_presentation() {
        for (index, line) in include_str!("../tests/fixtures/acp-mcp-tools.jsonl")
            .lines()
            .enumerate()
        {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let input = &fixture["input"];
            let actual = match fixture["operation"].as_str().unwrap() {
                "identity" => json!(identity(
                    &input["tool"],
                    &input["embedded"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|value| value.as_str().unwrap().to_owned())
                        .collect::<Vec<_>>()
                )),
                "output" => output(input),
                "presentation" => presentation(input),
                operation => panic!("Unknown MCP oracle operation {operation}"),
            };
            assert_eq!(
                actual, fixture["output"],
                "source MCP fixture {index}: {}",
                fixture["operation"]
            );
        }
    }
}
