//! Original composer-store recovery. Platform storage and mutable UI drafts
//! are separate; the original serialized document is never rewritten here.
use regex::Regex;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

pub const SOURCE_KEY: &str = "t3code:composer-drafts:v1";
pub const SOURCE_VERSION: u64 = 9;
pub const SIDECAR_KEY: &str = "t3code:rust-composer-drafts:v1";
pub const DEBOUNCE_MS: u64 = 300;

fn entries(value: &Value) -> Vec<(String, &Value)> {
    let mut result: Vec<_> = match value {
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| (key.clone(), value))
            .collect(),
        Value::Array(array) => array
            .iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), value))
            .collect(),
        _ => vec![],
    };
    // JavaScript enumerates integer property names before the remaining keys.
    fn integer(key: &str) -> Option<u32> {
        let number = key.parse::<u32>().ok()?;
        (number != u32::MAX && number.to_string() == key).then_some(number)
    }
    result.sort_by(|(a, _), (b, _)| match (integer(a), integer(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    result
}
fn object_like(value: &Value) -> bool {
    matches!(value, Value::Object(_) | Value::Array(_))
}
fn text(value: &Value) -> Option<&str> {
    value.as_str()
}
fn nonempty(value: &Value) -> Option<&str> {
    text(value).filter(|text| !text.is_empty())
}
fn coalesce<'a>(value: &'a Value, keys: &[&str]) -> &'a Value {
    keys.iter()
        .map(|key| &value[*key])
        .find(|value| !value.is_null())
        .unwrap_or(&Value::Null)
}
pub fn scoped_key(environment: &str, local: &str) -> String {
    format!("{environment}:{local}")
}
pub fn parse_scoped_key(key: &str) -> Option<(&str, &str)> {
    let (environment, local) = key.split_once(':')?;
    (!environment.is_empty() && !local.is_empty()).then_some((environment, local))
}
fn storage_key(key: &str, environment: Option<&str>) -> String {
    parse_scoped_key(key)
        .map(|(environment, local)| scoped_key(environment, local))
        .or_else(|| environment.map(|environment| scoped_key(environment, key)))
        .unwrap_or_else(|| key.into())
}
fn runtime(value: &Value) -> Option<&str> {
    text(value).filter(|mode| {
        matches!(
            *mode,
            "full-access" | "approval-required" | "auto-accept-edits" | "auto"
        )
    })
}
fn interaction(value: &Value) -> Option<&str> {
    text(value).filter(|mode| matches!(*mode, "default" | "plan"))
}
fn instance(value: &Value) -> Option<&str> {
    text(value).filter(|id| {
        let bytes = id.as_bytes();
        (1..=64).contains(&bytes.len())
            && bytes[0].is_ascii_alphabetic()
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-'))
    })
}
fn driver(value: &Value) -> Option<&str> {
    text(value).filter(|driver| {
        matches!(
            *driver,
            "codex"
                | "claudeAgent"
                | "cursor"
                | "grok"
                | "pi"
                | "opencode"
                | "antigravity"
                | "acpRegistry"
        )
    })
}
fn default_model(driver: &str) -> &str {
    match driver {
        "claudeAgent" => "claude-fable-5-1",
        "cursor" => "auto",
        "grok" => "grok-build",
        "pi" | "acpRegistry" => "default",
        "opencode" => "openai/gpt-5",
        "antigravity" => crate::models::ANTIGRAVITY_DEFAULT_MODEL,
        _ => "gpt-6-astra",
    }
}
fn coerce_options(value: &Value) -> Vec<Value> {
    if let Some(array) = value.as_array() {
        return array
            .iter()
            .filter_map(|option| {
                let id = nonempty(&option["id"])?;
                let value = &option["value"];
                (value.is_string() || value.is_boolean()).then(|| json!({"id":id,"value":value}))
            })
            .collect();
    }
    entries(value)
        .into_iter()
        .filter(|(_, value)| value.is_string() || value.is_boolean())
        .map(|(id, value)| json!({"id":id,"value":value}))
        .collect()
}
fn option_bag(value: &Value, provider: Option<&str>, legacy: &Value) -> Map<String, Value> {
    let mut result = Map::new();
    for provider in ["codex", "claudeAgent", "cursor", "opencode"] {
        let options = coerce_options(&value[provider]);
        if !options.is_empty() {
            result.insert(provider.into(), json!(options));
        }
    }
    if provider == Some("codex") {
        let mut extras = vec![];
        if let Some(effort) = nonempty(&legacy["effort"]) {
            extras.push(json!({"id":"reasoningEffort","value":effort}));
        }
        if legacy["codexFastMode"] == true || legacy["serviceTier"] == "fast" {
            extras.push(json!({"id":"fastMode","value":true}));
        }
        if !extras.is_empty() {
            let options = result
                .entry("codex".to_owned())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap();
            for extra in extras {
                if !options.iter().any(|option| option["id"] == extra["id"]) {
                    options.push(extra);
                }
            }
        }
    }
    result
}
fn selection(instance: &str, model: &str, options: &[Value]) -> Value {
    let mut value = json!({"instanceId":instance,"model":model});
    if !options.is_empty() {
        value["options"] = json!(options);
    }
    value
}
fn normalize_selection(
    value: &Value,
    legacy: &Value,
    legacy_options: &Value,
    legacy_codex: &Value,
) -> Option<Value> {
    let instance_value = coalesce(value, &["instanceId", "provider"]);
    let instance = instance(if instance_value.is_null() {
        &legacy["provider"]
    } else {
        instance_value
    })?;
    let model_value = &value["model"];
    let model = text(if model_value.is_null() {
        &legacy["model"]
    } else {
        model_value
    })?;
    let driver_value = &value["provider"];
    let kind = driver(if driver_value.is_null() {
        &legacy["provider"]
    } else {
        driver_value
    })
    .unwrap_or("codex");
    let model = crate::models::normalize_model_slug(kind, model)?;
    let options = if value["options"].is_array() {
        coerce_options(&value["options"])
    } else if let Some(kind) = driver(&json!(instance)) {
        let wrapped = if !value["options"].is_null() {
            json!({kind:value["options"]})
        } else {
            legacy_options.clone()
        };
        option_bag(
            &wrapped,
            Some(kind),
            if kind == "codex" {
                legacy_codex
            } else {
                &Value::Null
            },
        )
        .remove(kind)
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
    } else {
        vec![]
    };
    Some(selection(instance, &model, &options))
}
fn legacy_models(
    value: &Value,
    legacy: &Value,
    raw_options: &Value,
    legacy_codex: &Value,
) -> Map<String, Value> {
    let mut bag = option_bag(raw_options, None, legacy_codex);
    let current = normalize_selection(value, legacy, raw_options, legacy_codex);
    if let Some(current) = current.as_ref() {
        if let Some(kind) = driver(&current["instanceId"]) {
            if let Some(options) = current["options"]
                .as_array()
                .filter(|options| !options.is_empty())
            {
                bag.insert(kind.into(), json!(options));
            }
        }
    }
    let current = current.map(|current| {
        let instance = current["instanceId"].as_str().unwrap();
        let options = driver(&current["instanceId"])
            .and_then(|kind| bag.get(kind))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        selection(instance, current["model"].as_str().unwrap(), &options)
    });
    let mut result = Map::new();
    for kind in ["codex", "claudeAgent", "cursor", "opencode"] {
        if let Some(options) = bag
            .get(kind)
            .and_then(Value::as_array)
            .filter(|options| !options.is_empty())
        {
            let model = current
                .as_ref()
                .filter(|selection| selection["instanceId"] == kind)
                .and_then(|selection| selection["model"].as_str())
                .unwrap_or(default_model(kind));
            result.insert(kind.into(), selection(kind, model, options));
        }
    }
    if let Some(current) = current {
        result.insert(current["instanceId"].as_str().unwrap().into(), current);
    }
    result
}

fn normalize_sessions(state: &Value, now: &str) -> (Map<String, Value>, Map<String, Value>) {
    let raw_sessions = coalesce(
        state,
        &["draftThreadsByThreadKey", "draftThreadsByThreadId"],
    );
    let raw_mapping = coalesce(
        state,
        &[
            "logicalProjectDraftThreadKeyByLogicalProjectKey",
            "projectDraftThreadKeyByProjectKey",
            "projectDraftThreadIdByProjectKey",
            "projectDraftThreadIdByProjectId",
        ],
    );
    let mut environments = Map::new();
    for (project, key) in entries(raw_mapping) {
        if let (Some((environment, _)), Some(key)) = (parse_scoped_key(&project), nonempty(key)) {
            if let Some((environment, thread)) = parse_scoped_key(key) {
                environments.insert(thread.into(), json!(environment));
            } else {
                environments.insert(key.into(), json!(environment));
            }
        }
    }
    let mut sessions = Map::new();
    for (key, raw) in entries(raw_sessions) {
        if key.is_empty() || !object_like(raw) {
            continue;
        }
        let parsed = parse_scoped_key(&key);
        let thread = parsed
            .map(|(_, thread)| thread)
            .or_else(|| nonempty(&raw["threadId"]))
            .unwrap_or(&key);
        let environment = parsed
            .map(|(environment, _)| environment)
            .or_else(|| nonempty(&raw["environmentId"]))
            .or_else(|| environments.get(&key).and_then(Value::as_str));
        let Some(project) = nonempty(&raw["projectId"]) else {
            continue;
        };
        let Some(environment) = environment else {
            continue;
        };
        let worktree = text(&raw["worktreePath"]);
        let logical = nonempty(&raw["logicalProjectKey"])
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if parsed.is_some() {
                    scoped_key(environment, project)
                } else {
                    key.clone()
                }
            });
        let promoted = if nonempty(&raw["promotedTo"]["environmentId"]).is_some()
            && nonempty(&raw["promotedTo"]["threadId"]).is_some()
        {
            json!({"environmentId":raw["promotedTo"]["environmentId"],"threadId":raw["promotedTo"]["threadId"]})
        } else {
            Value::Null
        };
        let mut session = json!({"threadId":thread,"environmentId":environment,"projectId":project,"logicalProjectKey":logical,"createdAt":nonempty(&raw["createdAt"]).unwrap_or(now),"runtimeMode":runtime(&raw["runtimeMode"]).unwrap_or("full-access"),"interactionMode":interaction(&raw["interactionMode"]).unwrap_or("default"),"branch":text(&raw["branch"]),"worktreePath":worktree,"envMode":text(&raw["envMode"]).filter(|mode|matches!(*mode,"local"|"worktree")).unwrap_or(if worktree.is_some_and(|path|!path.is_empty()){"worktree"}else{"local"}),"startFromOrigin":raw["startFromOrigin"]==true,"promotedTo":promoted});
        if matches!(
            raw["environmentSelection"].as_str(),
            Some("auto" | "manual")
        ) {
            session["environmentSelection"] = raw["environmentSelection"].clone();
        }
        if nonempty(&raw["loadBalancedEnvironmentId"]).is_some()
            || raw
                .as_object()
                .is_some_and(|raw| raw.get("loadBalancedEnvironmentId") == Some(&Value::Null))
        {
            session["loadBalancedEnvironmentId"] = raw["loadBalancedEnvironmentId"].clone();
        }
        sessions.insert(storage_key(&key, None), session);
    }
    let mut mapping = Map::new();
    for (logical, key) in entries(raw_mapping) {
        let Some(key) = nonempty(key) else {
            continue;
        };
        let key = storage_key(key, None);
        mapping.insert(logical.clone(), json!(key));
        let project = parse_scoped_key(&logical);
        let parsed = parse_scoped_key(&key);
        if sessions
            .get(&key)
            .is_some_and(|session| session["logicalProjectKey"] == logical)
        {
            continue;
        }
        if let Some((environment, project)) = project {
            if let Some(session) = sessions.get_mut(&key) {
                if session["environmentId"] != environment || session["projectId"] != project {
                    session["environmentId"] = json!(environment);
                    session["projectId"] = json!(project);
                    session["logicalProjectKey"] = json!(logical);
                }
            } else {
                sessions.insert(key.clone(),json!({"threadId":parsed.map(|(_,thread)|thread).unwrap_or(&key),"environmentId":environment,"projectId":project,"logicalProjectKey":logical,"createdAt":now,"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"envMode":"local","startFromOrigin":false,"promotedTo":null}));
            }
        } else if let Some(session) = sessions.get_mut(&key) {
            session["logicalProjectKey"] = json!(logical);
        }
    }
    (sessions, mapping)
}

// Source Schema.is accepts the codec input, including transformable strings;
// keep the saved record rather than replacing it with the decoded DTO.
fn schema_is<T: DeserializeOwned>(raw: &Value) -> bool {
    serde_json::from_value::<T>(raw.clone()).is_ok()
}
fn js_space(c: char) -> bool {
    matches!(c,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')
}
fn js_trim(value: &str) -> &str {
    value.trim_matches(js_space)
}
fn valid_file(value: &Value) -> bool {
    object_like(value)
        && ["id", "name", "mimeType"]
            .iter()
            .all(|key| value[*key].is_string())
        && value["sizeBytes"].is_number()
        && value.get("attachmentId").is_none_or(Value::is_string)
        && value
            .get("environmentId")
            .is_none_or(schema_is::<t3_contracts::EnvironmentId>)
        && value
            .get("source")
            .is_none_or(|source| schema_is::<t3_contracts::PastedTextAttachmentSource>(source))
}
fn valid_review(value: &Value) -> bool {
    [
        "id",
        "sectionId",
        "sectionTitle",
        "filePath",
        "rangeLabel",
        "text",
        "diff",
    ]
    .iter()
    .all(|key| value[*key].is_string())
        && ["startIndex", "endIndex"]
            .iter()
            .all(|key| value[*key].is_number())
        && value.get("fenceLanguage").is_none_or(Value::is_string)
        && value.get("selection").is_none_or(|selection| {
            ["start", "end"]
                .iter()
                .all(|key| selection[*key].is_number())
                && ["side", "endSide"]
                    .iter()
                    .all(|key| matches!(selection[*key].as_str(), Some("additions" | "deletions")))
        })
        && value
            .get("pullRequest")
            .is_none_or(|value| schema_is::<t3_contracts::PullRequestContextMetadata>(value))
}
fn image(value: &Value) -> Option<Value> {
    let id = nonempty(&value["id"])?;
    let data = nonempty(&value["dataUrl"])?;
    let name = text(&value["name"])?;
    let mime = text(&value["mimeType"])?;
    value["sizeBytes"].as_f64()?;
    let mut image =
        json!({"id":id,"name":name,"mimeType":mime,"sizeBytes":value["sizeBytes"],"dataUrl":data});
    if let Some(source) = value
        .get("source")
        .filter(|source| schema_is::<t3_contracts::SnapShotSource>(source))
    {
        image["source"] = source.clone();
    }
    Some(image)
}
fn terminal(value: &Value) -> Option<Value> {
    let id = nonempty(&value["id"])?;
    let thread = nonempty(&value["threadId"])?;
    let created = nonempty(&value["createdAt"])?;
    let terminal = js_trim(text(&value["terminalId"])?);
    let label = js_trim(text(&value["terminalLabel"])?);
    if terminal.is_empty() || label.is_empty() {
        return None;
    }
    let start = value["lineStart"].as_f64()?.floor().max(1.0);
    let end = value["lineEnd"].as_f64()?.floor().max(start);
    let mut terminal = json!({"id":context_id(id),"threadId":thread,"createdAt":created,"terminalId":terminal,"terminalLabel":label,"lineStart":json_number(start),"lineEnd":json_number(end)});
    if let Some(text) = text(&value["text"]) {
        terminal["text"] = json!(text);
    }
    Some(terminal)
}
fn content(draft: &Value) -> bool {
    !draft["prompt"]
        .as_str()
        .unwrap_or_default()
        .trim_matches(js_space)
        .is_empty()
        || [
            "attachments",
            "files",
            "terminalContexts",
            "previewAnnotations",
            "reviewComments",
            "threadContexts",
        ]
        .iter()
        .any(|key| {
            draft[*key]
                .as_array()
                .is_some_and(|array| !array.is_empty())
        })
}
pub fn has_unrendered_content(draft: &Value) -> bool {
    [
        "attachments",
        "files",
        "terminalContexts",
        "previewAnnotations",
        "reviewComments",
        "threadContexts",
        "elementContexts",
    ]
    .iter()
    .any(|key| {
        draft[*key]
            .as_array()
            .is_some_and(|array| !array.is_empty())
    })
}

/// Context IDs use the original pair of FNV-1a passes over UTF-16 units.
pub fn context_id(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return value.into();
    }
    let units: Vec<_> = value.encode_utf16().collect();
    let (mut forward, mut reverse) = (0x811c9dc5u32, 0x9dc5811cu32);
    for (index, unit) in units.iter().enumerate() {
        forward = (forward ^ u32::from(*unit)).wrapping_mul(0x01000193);
        reverse = (reverse ^ u32::from(units[units.len() - 1 - index])).wrapping_mul(0x01000193);
    }
    let mut slug = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            separator = false;
            slug.push(character);
        } else {
            separator = true;
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(48).collect();
    format!(
        "{}-{forward:08x}{reverse:08x}",
        if slug.is_empty() { "ctx" } else { &slug }
    )
}
fn scoped_context_id(kind: &str, id: &str) -> String {
    context_id(&format!("{kind}_{id}"))
}
fn reference(kind: &str, id: &str, label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|character| {
            if matches!(character, '[' | ']' | '\\' | '\r' | '\n') {
                ' '
            } else {
                character
            }
        })
        .collect();
    let label = cleaned
        .split(js_space)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let label = if label.is_empty() { kind } else { &label };
    let label = String::from_utf16_lossy(&label.encode_utf16().take(200).collect::<Vec<_>>());
    format!(
        "{}[{label}](t3-context://v1/{kind}/{id})",
        if kind == "image" { "!" } else { "" }
    )
}
fn append_reference(prompt: &mut String, kind: &str, id: &str, label: &str) {
    static LINKS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"!?\[([^\]\n]{0,512})\]\(t3-context://v1/([a-z][a-z0-9-]{0,39})/([a-zA-Z0-9_-]{1,128})\)").unwrap()
    });
    if LINKS
        .captures_iter(prompt)
        .any(|capture| capture.get(3).is_some_and(|value| value.as_str() == id))
    {
        return;
    }
    if prompt
        .chars()
        .last()
        .is_some_and(|character| !matches!(character, ' ' | '\n' | '\t' | '\r'))
    {
        prompt.push(' ');
    }
    prompt.push_str(&reference(kind, id, label));
    prompt.push(' ');
}
fn normalize_context_prompt(prompt: &str, draft: &Value) -> String {
    static LEGACY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"!?\[([^\]\r\n]*)\]\(t3-context://v1/([a-z-]+)/([^/()\r\n]+)\)").unwrap()
    });
    let mut ids = std::collections::HashMap::new();
    for (kind, key) in [
        ("image", "attachments"),
        ("file", "files"),
        ("terminal", "terminalContexts"),
        ("review-comment", "reviewComments"),
        ("preview-annotation", "previewAnnotations"),
    ] {
        for entry in draft[key].as_array().into_iter().flatten() {
            let id = entry["id"].as_str().unwrap_or_default();
            let scoped = scoped_context_id(kind, id);
            ids.insert(format!("{kind}/{id}"), scoped.clone());
            ids.insert(format!("{kind}/{}", context_id(id)), scoped.clone());
            if kind == "preview-annotation" {
                ids.insert(
                    format!("{kind}/{}", context_id(&format!("annotation-{id}"))),
                    scoped,
                );
            }
        }
        for entry in draft[key].as_array().into_iter().flatten() {
            let scoped = scoped_context_id(kind, entry["id"].as_str().unwrap_or_default());
            ids.insert(format!("{kind}/{scoped}"), scoped);
        }
    }
    let migrated = LEGACY.replace_all(prompt, |captures: &regex::Captures<'_>| {
        let kind = &captures[2];
        ids.get(&format!("{kind}/{}", &captures[3]))
            .map(|id| reference(kind, id, &captures[1]))
            .unwrap_or_else(|| captures[0].into())
    });
    let contexts = draft["terminalContexts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut result = String::new();
    for (index, part) in migrated.split('\u{fffc}').enumerate() {
        if index > 0 {
            if let Some(context) = contexts.get(index - 1) {
                result.push_str(&terminal_reference(context));
            }
        }
        result.push_str(part);
    }
    for context in contexts {
        let label = terminal_label(context);
        append_reference(
            &mut result,
            "terminal",
            &scoped_context_id("terminal", context["id"].as_str().unwrap_or_default()),
            &label,
        );
    }
    result
}
fn json_number(number: f64) -> Value {
    if number >= i64::MIN as f64 && number < (i64::MAX as f64) {
        json!(number as i64)
    } else {
        json!(number)
    }
}
fn js_number(number: f64) -> String {
    let digits = number.to_string();
    if number.abs() < 1e21 {
        return digits;
    }
    let encoded = serde_json::to_string(&number).unwrap();
    if let Some((mantissa, exponent)) = encoded.split_once('e') {
        let mantissa = mantissa.strip_suffix(".0").unwrap_or(mantissa);
        let exponent = exponent.parse::<i32>().unwrap();
        format!(
            "{mantissa}e{}{exponent}",
            if exponent >= 0 { "+" } else { "" }
        )
    } else {
        digits
    }
}
fn terminal_label(context: &Value) -> String {
    let start = context["lineStart"].as_f64().unwrap_or(1.0);
    let end = context["lineEnd"].as_f64().unwrap_or(start);
    let first = js_number(start);
    let last = js_number(end);
    format!(
        "{} {}",
        context["terminalLabel"].as_str().unwrap_or_default(),
        if start == end {
            format!("line {first}")
        } else {
            format!("lines {first}-{last}")
        }
    )
}
fn terminal_reference(context: &Value) -> String {
    reference(
        "terminal",
        &scoped_context_id("terminal", context["id"].as_str().unwrap_or_default()),
        &terminal_label(context),
    )
}

/// Source migration normalizes supported fields without consulting a live
/// destination. Callers keep the source bytes and any unported data separately.
pub fn recover_state(state: &Value, version: u64, now: &str) -> Result<Value, String> {
    let (sessions, mapping) = normalize_sessions(state, now);
    let mut environments = Map::new();
    for (key, session) in &sessions {
        if let Some((_, thread)) = parse_scoped_key(key) {
            environments.insert(thread.into(), session["environmentId"].clone());
        }
    }
    let mut drafts = Map::new();
    for (key, raw) in entries(coalesce(state, &["draftsByThreadKey", "draftsByThreadId"])) {
        if key.is_empty() || !object_like(raw) {
            continue;
        }
        let mut draft = json!({"prompt":text(&raw["prompt"]).unwrap_or_default(),"attachments":raw["attachments"].as_array().into_iter().flatten().filter_map(image).collect::<Vec<_>>()});
        for field in [
            "files",
            "terminalContexts",
            "reviewComments",
            "threadContexts",
        ] {
            let array: Vec<_> = raw[field]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|value| match field {
                    "files" => valid_file(value).then(|| value.clone()),
                    "terminalContexts" => terminal(value),
                    "reviewComments" => valid_review(value).then(|| value.clone()),
                    _ => {
                        schema_is::<t3_contracts::ThreadContextRecord>(value).then(|| value.clone())
                    }
                })
                .collect();
            if !array.is_empty() {
                draft[field] = json!(array);
            }
        }
        // Raw preview capture validation is supplied by the shared IPC contract.
        let mut preview: Vec<_> = raw["previewAnnotations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|value| valid_preview(value))
            .cloned()
            .collect();
        for element in raw["elementContexts"].as_array().into_iter().flatten() {
            if !schema_is::<t3_contracts::ElementContextDetails>(element)
                || !element["id"].is_string()
                || !element["pickedAt"].is_string()
                || preview
                    .iter()
                    .any(|annotation| annotation["id"] == element["id"])
            {
                continue;
            }
            let mut picked = element.clone();
            picked["stack"] = json!([]);
            preview.push(json!({"id":element["id"],"pageUrl":element["pageUrl"],"pageTitle":element["pageTitle"],"comment":"","elements":[{"id":element["id"],"element":picked,"rect":{"x":0,"y":0,"width":0,"height":0}}],"regions":[],"strokes":[],"styleChanges":[],"screenshot":null,"createdAt":element["pickedAt"]}));
        }
        if !preview.is_empty() {
            draft["previewAnnotations"] = json!(preview);
        }
        let modern = object_like(&raw["modelSelectionByProvider"]);
        let models = if modern {
            let mut result = Map::new();
            for (provider, selection) in entries(&raw["modelSelectionByProvider"]) {
                if selection.is_null()
                    || selection.get("options").is_some_and(|value| {
                        !value.is_null() && *value != false && *value != "" && !value.is_array()
                    })
                {
                    return Err("Invalid saved model selection.".into());
                }
                result.insert(provider, selection.clone());
            }
            result
        } else {
            let bag = option_bag(&raw["modelOptions"], None, raw);
            legacy_models(
                &raw["modelSelection"],
                raw,
                &if bag.is_empty() {
                    raw["modelOptions"].clone()
                } else {
                    json!(bag)
                },
                raw,
            )
        };
        let active = if modern {
            instance(&raw["activeProvider"]).map(str::to_owned)
        } else {
            let bag = option_bag(&raw["modelOptions"], None, raw);
            normalize_selection(
                &raw["modelSelection"],
                raw,
                &if bag.is_empty() {
                    raw["modelOptions"].clone()
                } else {
                    json!(bag)
                },
                raw,
            )
            .and_then(|value| value["instanceId"].as_str().map(str::to_owned))
        };
        let has_model = !models.is_empty() || active.is_some();
        let has_runtime = runtime(&raw["runtimeMode"]);
        let has_interaction = interaction(&raw["interactionMode"]);
        if !content(&draft)
            && draft["prompt"].as_str().is_none_or(str::is_empty)
            && !has_model
            && has_runtime.is_none()
            && has_interaction.is_none()
        {
            continue;
        }
        if has_model {
            draft["modelSelectionByProvider"] = json!(models);
            draft["activeProvider"] = json!(active);
            if modern && raw["modelSelectionExplicit"] == true {
                draft["modelSelectionExplicit"] = json!(true);
            }
        }
        if let Some(mode) = has_runtime {
            draft["runtimeMode"] = json!(mode);
        }
        if let Some(mode) = has_interaction {
            draft["interactionMode"] = json!(mode);
        }
        draft["prompt"] = json!(normalize_context_prompt(
            draft["prompt"].as_str().unwrap_or_default(),
            &draft
        ));
        let normalized = if parse_scoped_key(&key).is_some() || sessions.contains_key(&key) {
            storage_key(&key, None)
        } else {
            storage_key(&key, environments.get(&key).and_then(Value::as_str))
        };
        if version != SOURCE_VERSION
            && sessions.contains_key(&normalized)
            && draft["modelSelectionExplicit"] != true
            && !content(&draft)
        {
            let object = draft.as_object_mut().unwrap();
            for field in [
                "activeProvider",
                "modelSelectionByProvider",
                "modelSelectionExplicit",
            ] {
                object.remove(field);
            }
            if !object.contains_key("runtimeMode") && !object.contains_key("interactionMode") {
                continue;
            }
        }
        drafts.insert(normalized, draft);
    }
    let (sticky, active) = if object_like(&state["stickyModelSelectionByProvider"]) {
        let mut map = Map::new();
        for (key, value) in entries(&state["stickyModelSelectionByProvider"]) {
            if value.is_null()
                || value.get("options").is_some_and(|value| {
                    value.is_null() || (*value != false && *value != "" && !value.is_array())
                })
            {
                return Err("Invalid saved sticky selection.".into());
            }
            map.insert(key, value.clone());
        }
        (
            map,
            instance(&state["stickyActiveProvider"]).map(str::to_owned),
        )
    } else {
        let legacy = json!({"provider":coalesce(state,&["stickyProvider"]).as_str().unwrap_or("codex"),"model":state["stickyModel"]});
        (
            legacy_models(
                &state["stickyModelSelection"],
                &legacy,
                &state["stickyModelOptions"],
                &Value::Null,
            ),
            instance(&state["stickyProvider"]).map(str::to_owned),
        )
    };
    let mut memory = Map::new();
    if let Some(raw) = state.get("stickyOptionsByModelByProvider") {
        for (instance, models) in entries(raw) {
            for (model, options) in entries(models) {
                if options
                    .as_array()
                    .is_some_and(|options| !options.is_empty())
                {
                    memory.entry(instance.clone()).or_insert_with(|| json!({}))[model] =
                        options.clone();
                }
            }
        }
    } else {
        for (instance, selection) in &sticky {
            if selection.get("options").is_some_and(|options| {
                !options.as_array().is_some_and(Vec::is_empty) && options.as_str() != Some("")
            }) {
                if let Some(model) = selection["model"].as_str() {
                    memory.insert(instance.clone(), json!({model:selection["options"]}));
                }
            }
        }
    }
    Ok(
        json!({"draftsByThreadKey":drafts,"draftThreadsByThreadKey":sessions,"logicalProjectDraftThreadKeyByLogicalProjectKey":mapping,"stickyModelSelectionByProvider":sticky,"stickyOptionsByModelByProvider":memory,"stickyActiveProvider":active}),
    )
}
fn valid_preview(value: &Value) -> bool {
    schema_is::<t3_contracts::PreviewAnnotationPayload>(value)
}
