//! How a Codex MCP or dynamic tool call is presented (`CodexToolPresentation.ts`,
//! `McpToolPresentation.ts`): the source integration, the browser or computer surface, icons and
//! the activity title. Pure functions of the native item.
use serde_json::{Map, Value, json};
use std::collections::HashSet;

/// `normalizeMcpText`: whitespace collapsed, empty and over-long text rejected.
fn mcp_text(value: &Value, max: usize) -> Option<String> {
    let text = value.as_str()?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty() && text.chars().count() <= max).then_some(text)
}
fn display(value: &Value) -> Option<String> {
    mcp_text(value, 160)
}
fn squash(text: &str) -> Option<String> {
    display(&json!(text.split(['_', '-']).filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ")))
}
fn http_url(value: &Value) -> Option<String> {
    let text = value.as_str().filter(|text| text.len() <= 4096)?;
    (text.starts_with("https://") || text.starts_with("http://")).then(|| text.to_owned())
}
fn image_url(value: &Value) -> Option<String> {
    let text = value.as_str().filter(|text| text.len() <= 4096)?;
    (text.starts_with("https://") || text.starts_with("http://") || text.starts_with("data:")).then(|| text.to_owned())
}
fn t3_mcp_tool(server: &str, tool: &str) -> bool {
    static NAMES: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    let names = NAMES.get_or_init(|| serde_json::from_str::<Vec<String>>(include_str!("claude-t3-mcp-tools.json")).expect("generated T3 MCP tool names").into_iter().collect());
    matches!(server.to_lowercase().as_str(), "t3-code" | "t3_code" | "t3code") && names.contains(tool)
}
fn themed_logo(records: &[&Value]) -> Option<Value> {
    for record in records {
        let Some(logo) = image_url(&record["logoUrl"]) else { continue };
        let dark = image_url(if record["logoUrlDark"].is_null() { &record["logoDarkUrl"] } else { &record["logoUrlDark"] });
        let mut icon = json!({"_tag":"themed-logo","logoUrl":logo});
        if let Some(dark) = dark {
            icon["logoUrlDark"] = json!(dark);
        }
        return Some(icon);
    }
    None
}

/// `mcpToolPresentation` of `McpToolPresentation.ts`.
fn integration(server: &Value, tool: &Value, title: &Value, server_display: &Value, source: &Value, icon_url: Option<&str>, icon_url_dark: Option<&str>) -> Map<String, Value> {
    let qualified = tool.as_str().and_then(|name| {
        let rest = name.strip_prefix("mcp__").or_else(|| name.strip_prefix("MCP__"))?;
        let split = rest.find("__").filter(|split| *split > 0 && split + 2 < rest.len())?;
        Some((rest[..split].to_owned(), rest[split + 2..].to_owned()))
    });
    let server_name = display(server).or_else(|| qualified.as_ref().and_then(|(server, _)| display(&json!(server)))).or_else(|| display(server_display));
    let tool_name = qualified.as_ref().and_then(|(_, tool)| display(&json!(tool))).or_else(|| display(tool));
    let mut out = Map::new();
    if let (Some(server), Some(tool)) = (&server_name, &tool_name) {
        if t3_mcp_tool(server, tool) {
            return out;
        }
    }
    let title = display(title).or_else(|| match (&server_name, &tool_name) {
        (Some(_), Some(tool)) => squash(tool),
        _ => None,
    });
    let Some(server) = server_name else {
        if let Some(title) = title {
            out.insert("title".into(), json!(title));
        }
        return out;
    };
    let name = display(server_display).or_else(|| display(&source["name"])).or_else(|| squash(&server)).unwrap_or_else(|| server.clone());
    let logo = icon_url.map(str::to_owned).or_else(|| http_url(&source["logoUrl"]));
    let logo_dark = icon_url_dark.map(str::to_owned).or_else(|| http_url(&source["logoUrlDark"]));
    let icon = logo.map(|logo| {
        let mut icon = json!({"_tag":"themed-logo","logoUrl":logo});
        if let Some(dark) = logo_dark {
            icon["logoUrlDark"] = json!(dark);
        }
        icon
    });
    if let Some(title) = title {
        out.insert("title".into(), json!(title));
    }
    if let Some(icon) = &icon {
        out.insert("toolIcon".into(), icon.clone());
    }
    let mut source_value = json!({"key":format!("mcp:{}", server.to_lowercase()),"name":name,"kind":"integration"});
    if let Some(icon) = icon {
        source_value["icon"] = icon;
    }
    out.insert("toolSource".into(), source_value);
    out
}

fn browser_display(value: &Value) -> Option<String> {
    let name = display(value)?;
    let lower = name.to_lowercase();
    Some(if lower.contains("chrome") || lower == "chromium" {
        "Chrome".into()
    } else if lower.contains("edge") {
        "Microsoft Edge".into()
    } else if lower.contains("firefox") {
        "Firefox".into()
    } else if lower.contains("safari") {
        "Safari".into()
    } else if lower.contains("arc") {
        "Arc".into()
    } else if lower == "iab" || lower.contains("in-app") {
        "Browser".into()
    } else {
        name
    })
}
fn browser_native_app(name: &str) -> Option<Value> {
    match name {
        "Chrome" => Some(json!({"_tag":"display-name","displayName":"Google Chrome"})),
        "Microsoft Edge" | "Firefox" | "Safari" | "Arc" => Some(json!({"_tag":"display-name","displayName":name})),
        _ => None,
    }
}
fn app_name_from_id(app_id: &str) -> Option<&'static str> {
    match app_id.to_lowercase().as_str() {
        "com.apple.finder" => Some("Finder"),
        "com.apple.safari" => Some("Safari"),
        "com.google.chrome" => Some("Chrome"),
        "com.microsoft.edgemac" => Some("Microsoft Edge"),
        "org.mozilla.firefox" => Some("Firefox"),
        "company.thebrowser.browser" => Some("Arc"),
        _ => None,
    }
}
fn native_app(value: &Value) -> Option<Value> {
    match value["kind"].as_str() {
        Some("appId") => {
            let id = value["appId"].as_str()?.trim();
            (!id.is_empty() && id.len() <= 512 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))).then(|| json!({"_tag":"app-id","appId":id}))
        }
        Some("displayName") => display(&value["displayName"]).map(|name| json!({"_tag":"display-name","displayName":name})),
        _ => None,
    }
}
/// SHA-256 is not needed for ids that fit: the original hashes only a key longer than 512 characters.
fn native_app_key(app_id: &str) -> String {
    let key = format!("native-app:{}", app_id.to_lowercase());
    key.chars().take(512).collect()
}

/// The presentation fields (`title`, `toolSurface`, `toolIcon`, `toolSource`) of an
/// `mcpToolCall` item.
pub(crate) fn mcp_presentation(item: &Value) -> Map<String, Value> {
    let metadata = &item["result"]["_meta"];
    let surface = &metadata["codex/toolSurface"];
    let source_metadata = &metadata["source"];
    let app_context = &item["appContext"];
    let source_logo = themed_logo(&[surface, source_metadata, app_context]);
    let mut out = Map::new();
    if surface["kind"] == "browserUse" {
        let screenshot = &surface["screenshot"];
        let browser_use = &metadata["browser_use"];
        let latest_tab = surface["openTabs"].as_array().into_iter().flatten().rev().find(|tab| http_url(&tab["url"]).is_some());
        let selected = [(screenshot, &screenshot["pageUrl"]), (browser_use, &browser_use["url"]), (latest_tab.unwrap_or(&Value::Null), latest_tab.map_or(&Value::Null, |tab| &tab["url"]))]
            .into_iter()
            .find_map(|(record, url)| http_url(url).map(|page| (record, page)));
        let name = browser_display(&app_context["appName"]).or_else(|| browser_display(&surface["browserFamily"])).or_else(|| browser_display(&surface["backend"])).unwrap_or_else(|| "Browser".into());
        let native = browser_native_app(&name).map(|app| json!({"_tag":"native-app","app":app}));
        let source_icon = source_logo.clone().or(native);
        out.insert("toolSurface".into(), json!("browser"));
        if let Some((record, page)) = selected {
            let mut icon = json!({"_tag":"website","pageUrl":page});
            if let Some(favicon) = image_url(if record["faviconUrl"].is_null() { &record["favIconUrl"] } else { &record["faviconUrl"] }) {
                icon["faviconUrl"] = json!(favicon);
            }
            if let Some(dark) = image_url(if record["faviconUrlDark"].is_null() { &record["favIconUrlDark"] } else { &record["faviconUrlDark"] }) {
                icon["faviconUrlDark"] = json!(dark);
            }
            out.insert("toolIcon".into(), icon);
        }
        let mut source = json!({"key":format!("browser-use:{}", name.trim().to_lowercase()),"name":name,"kind":if name == "Browser" {"browser"} else {"integration"}});
        if let Some(icon) = source_icon {
            source["icon"] = icon;
        }
        out.insert("toolSource".into(), source);
        return out;
    }
    if surface["kind"] == "computerUse" {
        let app = native_app(&surface["app"]);
        let arguments = &item["arguments"];
        let argument_name = display(&arguments["appName"]).or_else(|| display(&arguments["application"])).or_else(|| display(&arguments["app"]));
        let name = display(&app_context["appName"])
            .or(argument_name)
            .or_else(|| app.as_ref().filter(|app| app["_tag"] == "display-name").and_then(|app| app["displayName"].as_str().map(str::to_owned)))
            .or_else(|| app.as_ref().filter(|app| app["_tag"] == "app-id").and_then(|app| app_name_from_id(app["appId"].as_str().unwrap_or("")).map(str::to_owned)))
            .unwrap_or_else(|| "Computer Use".into());
        let source_icon = source_logo.clone().or_else(|| app.clone().map(|app| json!({"_tag":"native-app","app":app})));
        let key = match &app {
            Some(app) if app["_tag"] == "app-id" => native_app_key(app["appId"].as_str().unwrap_or("")),
            Some(app) => format!("native-app-name:{}", app["displayName"].as_str().unwrap_or("").trim().to_lowercase()),
            None => "computer-use".into(),
        };
        out.insert("toolSurface".into(), json!("computer"));
        if let Some(app) = &app {
            out.insert("toolIcon".into(), json!({"_tag":"native-app","app":app}));
        }
        let mut source = json!({"key":key,"name":name,"kind":"computer"});
        if let Some(icon) = source_icon {
            source["icon"] = icon;
        }
        out.insert("toolSource".into(), source);
        return out;
    }
    let server = if app_context["connectorId"].is_null() { &item["server"] } else { &app_context["connectorId"] };
    let (logo, logo_dark) = match &source_logo {
        Some(icon) => (icon["logoUrl"].as_str(), icon["logoUrlDark"].as_str()),
        None => (None, None),
    };
    integration(server, &item["tool"], &app_context["actionName"], &app_context["appName"], source_metadata, logo, logo_dark)
}

/// `dynamicToolTitle`: the activity heading a dynamic tool derives from its input.
pub(crate) fn dynamic_tool_title(tool_name: &str, input: &Value) -> Option<String> {
    (tool_name == "cua_repl.js").then(|| input["title"].as_str().map(str::trim).filter(|title| !title.is_empty()).map(str::to_owned)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_integration_names_its_source_and_a_t3_tool_has_no_integration_chrome() {
        let item = json!({"type":"mcpToolCall","server":"github_app","tool":"create_issue","arguments":{}});
        let presentation = mcp_presentation(&item);
        assert_eq!(presentation["title"], "create issue");
        assert_eq!(presentation["toolSource"], json!({"key":"mcp:github_app","name":"github app","kind":"integration"}));
        let tool = serde_json::from_str::<Vec<String>>(include_str!("claude-t3-mcp-tools.json")).unwrap().remove(0);
        assert!(mcp_presentation(&json!({"server":"t3-code","tool":tool,"arguments":{}})).is_empty());
    }

    #[test]
    fn a_browser_surface_picks_the_page_the_tool_reported() {
        let item = json!({"type":"mcpToolCall","server":"browser","tool":"navigate","arguments":{},"appContext":{"appName":"Google Chrome"},"result":{"_meta":{"codex/toolSurface":{"kind":"browserUse","openTabs":[{"url":"ftp://x"},{"url":"https://example.com/a"}]}}}});
        let presentation = mcp_presentation(&item);
        assert_eq!(presentation["toolSurface"], "browser");
        assert_eq!(presentation["toolIcon"], json!({"_tag":"website","pageUrl":"https://example.com/a"}));
        assert_eq!(presentation["toolSource"]["key"], "browser-use:chrome");
        assert_eq!(presentation["toolSource"]["icon"], json!({"_tag":"native-app","app":{"_tag":"display-name","displayName":"Google Chrome"}}));
    }

    #[test]
    fn a_computer_surface_names_the_app_it_drives() {
        let item = json!({"type":"mcpToolCall","server":"cua","tool":"click","arguments":{},"result":{"_meta":{"codex/toolSurface":{"kind":"computerUse","app":{"kind":"appId","appId":"com.apple.Finder"}}}}});
        let presentation = mcp_presentation(&item);
        assert_eq!((presentation["toolSource"]["name"].as_str(), presentation["toolSource"]["key"].as_str()), (Some("Finder"), Some("native-app:com.apple.finder")));
        assert_eq!(dynamic_tool_title("cua_repl.js", &json!({"title":" Open Finder "})).as_deref(), Some("Open Finder"));
        assert_eq!(dynamic_tool_title("shell", &json!({"title":"x"})), None);
    }
}
