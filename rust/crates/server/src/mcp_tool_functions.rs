//! The code the original's tool schemas hold: refinements written by hand and transformations.
//! `tools/generate_mcp_tool_schemas.mjs` records each by the hash of its source text, so a function
//! the original changes is a hash this file does not know, and `every_function_the_schemas_hold_is_ported`
//! fails instead of the port drifting. Each arm names the function it ports.
use crate::mcp_tool_input::decoder::{Js, trim};
use serde_json::Value;

/// What a hand-written refinement says: it holds, fails with the library's generic text, or fails with its own.
pub(crate) enum Outcome {
    Pass,
    Fail,
    Message(String),
}
use Outcome::*;

fn verdict(holds: bool, message: &str) -> Outcome {
    if holds { Pass } else { Message(message.into()) }
}

/// `PREVIEW_VIEWPORT_MAX_AREA` in packages/contracts/src/preview.ts: 3840 * 2160.
const PREVIEW_VIEWPORT_MAX_AREA: f64 = 3840.0 * 2160.0;

#[cfg(test)]
pub(crate) const FILTERS: &[&str] = &[
    "8b97d718578572c0",
    "4f74f84e3956da24",
    "0b8ed7bac8b5f0db",
    "c4b917f5c7692df9",
    "31e57149c0b88ed7",
    "62f5a93a0f08b95d",
    "31e68401a07bce67",
    "09d7e5a7be8a7c27",
    "21ac5f0442b289fe",
    "89c49dac99285426",
    "6ae0b8dddb5fb9c2",
    "dd14fe9cc8819e52",
];
#[cfg(test)]
pub(crate) const TRANSFORMS: &[&str] = &["b408d0ec4c4bd5df", "5f01fd5a87b30eb8", "f0d07c845ca6b4bb", "7cf952ddbfe5e80b", "70519281ba9e2967"];

pub(crate) fn filter(hash: &str, value: &Js) -> Outcome {
    let has = |key: &str| value.defined(key);
    let number = |key: &str| match value.get(key) {
        Some(Js::Num(number)) => number.as_f64().unwrap_or(f64::NAN),
        _ => f64::NAN,
    };
    let is = |key: &str, literal: &str| matches!(value.get(key), Some(Js::Str(found)) if found == literal);
    match (hash, value) {
        // isNonBlank: (value) => value.trim().length > 0
        ("8b97d718578572c0", Js::Str(text)) => if trim(text).is_empty() { Fail } else { Pass },
        // ThreadMetadataMcpPullRequestUrl: isHttpUrl(value)
        ("4f74f84e3956da24", Js::Str(text)) => verdict(url::Url::parse(text).is_ok_and(|url| matches!(url.scheme(), "http" | "https")), "Pull request URL must be a well-formed HTTP(S) URL."),
        // ThreadMetadataClientRequestId: hasOnlyPairedSurrogates(value). A Rust string holds no lone surrogate.
        ("0b8ed7bac8b5f0db", Js::Str(_)) => Pass,
        // ThreadMetadataMcpUpdateInput: the action decides which of title and pullRequest it takes.
        ("c4b917f5c7692df9", _) => {
            let action = match value.get("action") {
                Some(Js::Str(action)) => action.as_str(),
                _ => "",
            };
            match action {
                "rename" => verdict(has("title") && !has("pullRequest"), "rename requires title and does not accept pullRequest."),
                "link_pull_request" => verdict(has("pullRequest") && !has("title"), "link_pull_request requires pullRequest and does not accept title."),
                _ => verdict(!has("title") && !has("pullRequest"), &format!("{action} does not accept title or pullRequest.")),
            }
        }
        // preview_open: tabId cannot be combined with reuseExistingTab=false.
        ("31e57149c0b88ed7", _) => verdict(!(has("tabId") && matches!(value.get("reuseExistingTab"), Some(Js::Bool(false)))), "tabId cannot be combined with reuseExistingTab=false."),
        ("62f5a93a0f08b95d", _) => verdict(has("url") as u8 + has("target") as u8 == 1, "Provide exactly one of url or target."),
        // preview_resize
        ("31e68401a07bce67", _) => {
            let (preset, width, height) = (has("preset"), has("width"), has("height"));
            if width != height {
                return Message("Custom dimensions require both width and height.".into());
            }
            if is("mode", "fill") {
                return verdict(!preset && !width && !has("orientation"), "Fill mode does not accept a preset, dimensions, or orientation.");
            }
            if is("mode", "freeform") {
                if !width || !height || preset || has("orientation") {
                    return Message("Freeform mode requires width and height and does not accept a preset or orientation.".into());
                }
            } else if !preset || width || height {
                return Message("Preset mode requires a preset and does not accept custom dimensions.".into());
            }
            if width && height && number("width") * number("height") > PREVIEW_VIEWPORT_MAX_AREA {
                return Message(format!("Custom viewport area must not exceed {} pixels.", PREVIEW_VIEWPORT_MAX_AREA as u64));
            }
            Pass
        }
        // A pointer tool's target: a selector, a locator or a coordinate pair, exactly one.
        ("09d7e5a7be8a7c27", _) => {
            let selectors = has("selector") as u8 + has("locator") as u8;
            let (x, y) = (has("x"), has("y"));
            if x != y {
                return Message("Coordinates require both x and y.".into());
            }
            verdict(selectors + (x && y) as u8 == 1, "Provide exactly one target.")
        }
        ("21ac5f0442b289fe", _) => verdict(!(has("selector") && has("locator")), "Provide at most one of selector or locator."),
        ("89c49dac99285426", _) => verdict(has("selector") as u8 + has("locator") as u8 == 1, "Provide exactly one of selector or locator."),
        // preview_scroll
        ("6ae0b8dddb5fb9c2", _) => {
            if has("selector") && has("locator") {
                return Message("Provide at most one of selector or locator.".into());
            }
            verdict(has("deltaX") || has("deltaY"), "Provide deltaX or deltaY.")
        }
        // preview_wait_for
        ("dd14fe9cc8819e52", _) => {
            if has("selector") && has("locator") {
                return Message("Provide at most one of selector or locator.".into());
            }
            verdict(has("selector") || has("locator") || has("text") || has("urlIncludes"), "Provide at least one wait condition.")
        }
        _ => panic!("refinement {hash} is not ported"),
    }
}

/// Runs a transformation's decode. A refusal is the text after "Expected" in the issue it raises.
pub(crate) fn transform(hash: &str, value: Js) -> Result<Js, String> {
    let entries = |entries: Vec<(String, Js)>| Js::Arr(entries.into_iter().map(|(id, value)| Js::Obj(vec![("id".into(), Js::Str(id)), ("value".into(), value)])).collect());
    match (hash, value) {
        // TrimmedString: (value) => Effect.succeed(value.trim())
        ("b408d0ec4c4bd5df", Js::Str(text)) => Ok(Js::Str(trim(&text).to_owned())),
        // Object.entries(record).map(([id, value]) => ({ id, value }))
        ("5f01fd5a87b30eb8", Js::Obj(record)) => Ok(entries(record)),
        // Schema.fromJsonString: JSON.parse(input)
        ("f0d07c845ca6b4bb", Js::Str(text)) => serde_json::from_str::<Value>(&text).map(|parsed| Js::from_json(&parsed)).map_err(|_| "a valid JSON string".into()),
        // coerceLegacyOptionsObjectToArray: keys and strings trimmed, blanks dropped, non-strings and non-booleans dropped.
        ("7cf952ddbfe5e80b", Js::Obj(record)) => Ok(Js::Arr(
            record
                .into_iter()
                .filter_map(|(key, value)| {
                    let id = trim(&key).to_owned();
                    let value = match value {
                        Js::Str(text) if !trim(&text).is_empty() => Js::Str(trim(&text).to_owned()),
                        Js::Bool(flag) => Js::Bool(flag),
                        _ => return None,
                    };
                    (!id.is_empty()).then(|| Js::Obj(vec![("id".into(), Js::Str(id)), ("value".into(), value)]))
                })
                .collect(),
        )),
        // The legacy model selection: `provider` stands in for a missing `instanceId`.
        ("70519281ba9e2967", raw @ Js::Obj(_)) => {
            let source = match (raw.get("instanceId"), raw.get("provider")) {
                (Some(found), _) if *found != Js::Undefined => found.clone(),
                (_, Some(provider @ Js::Str(_))) => provider.clone(),
                _ => Js::Undefined,
            };
            let mut base = vec![("instanceId".to_owned(), source), ("model".to_owned(), raw.get("model").cloned().unwrap_or(Js::Undefined))];
            if let Some(options) = raw.get("options").filter(|options| **options != Js::Undefined) {
                base.push(("options".into(), options.clone()));
            }
            Ok(Js::Obj(base))
        }
        _ => panic!("transformation {hash} is not ported"),
    }
}
