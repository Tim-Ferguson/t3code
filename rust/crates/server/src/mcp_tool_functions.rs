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
    "21891774abfb8b35",
    "f0abab71e3b687de",
    "564eaf2d15a1eaed",
    "d47c6f04fdc3cb77",
    "fe592ca0722cf7ea",
    "9d17f3fe6e0abee4",
    "545f41b3d1af89b4",
    "1d47e7d3fad08a52",
    "ac7ed779287c1dab",
    "4ecdd66ab1d79947",
    "a5df8ab7baddd8a3",
    "49b70fbab8e755cb",
    "a5ea1ee6718d26c0",
];
#[cfg(test)]
pub(crate) const TRANSFORMS: &[&str] = &[
    "b408d0ec4c4bd5df",
    "5f01fd5a87b30eb8",
    "f0d07c845ca6b4bb",
    "7cf952ddbfe5e80b",
    "70519281ba9e2967",
    "aaad838e39369837",
    "37d2fe1cb246a09b",
    "55c74d9b368f5dfe",
    "79acca51f7d9da1b",
    "23115dae1df5b4a6",
    "c873fba597093fe5",
];

pub(crate) fn filter(hash: &str, value: &Js, rejects: Option<&[String]>) -> Outcome {
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
        // The local host id is reserved.
        ("21891774abfb8b35", Js::Str(id)) => verdict(id != "local", "The local host id is reserved."),
        ("21891774abfb8b35", _) => Pass,
        // Device host ids are unique.
        ("f0abab71e3b687de", Js::Arr(hosts)) => verdict(distinct(hosts.iter().map(|host| host.get("id").unwrap_or(&Js::Undefined))), "Device host ids must be unique."),
        // A page snapshot's accessibility tree: at most 10,000 nodes and 32,000 serialized characters.
        ("564eaf2d15a1eaed", _) => {
            if is("format", "flat-text") {
                return Pass;
            }
            let mut stack = vec![value.get("root")];
            let mut nodes = 0;
            while let Some(node) = stack.pop() {
                nodes += 1;
                if nodes > 10_000 {
                    return Message("Accessibility trees must not exceed 10000 nodes.".into());
                }
                if let Some(Js::Arr(children)) = node.and_then(|node| node.get("children")) {
                    stack.extend(children.iter().map(Some));
                }
            }
            verdict(json_length(value) <= Some(32_000), "Accessibility trees must not exceed 32000 serialized characters.")
        }
        ("d47c6f04fdc3cb77", _) => verdict(number("width") * number("height") <= PREVIEW_VIEWPORT_MAX_AREA, &format!("Viewport area must not exceed {} pixels.", PREVIEW_VIEWPORT_MAX_AREA as u64)),
        ("fe592ca0722cf7ea", _) => verdict(has("autoResume") || has("snooze"), "A recovery update must include autoResume or snooze."),
        // A known kind must decode as its own member: the strings the union's other members are keyed by.
        ("9d17f3fe6e0abee4", Js::Str(kind)) => verdict(!rejects.unwrap_or_default().contains(kind), "A known kind must decode in full."),
        ("545f41b3d1af89b4", _) => if number("lineEnd") >= number("lineStart") { Pass } else { Fail },
        ("1d47e7d3fad08a52", _) => if number("endIndex") >= number("startIndex") { Pass } else { Fail },
        // An unknown context record's payload: JSON of at most 64,000 characters.
        ("ac7ed779287c1dab", _) => if json_length(value).is_some_and(|length| length <= 64_000) { Pass } else { Fail },
        // No holes in an array.
        ("4ecdd66ab1d79947", Js::Arr(values)) => if values.contains(&Js::Undefined) { Fail } else { Pass },
        ("a5df8ab7baddd8a3", Js::Arr(records)) => if distinct(records.iter().map(|record| record.get("contextId").unwrap_or(&Js::Undefined))) { Pass } else { Fail },
        ("49b70fbab8e755cb", _) => if json_length(value).is_some_and(|length| length <= 16_000_000) { Pass } else { Fail },
        ("a5ea1ee6718d26c0", _) => verdict(number("fromTurnCount") <= number("toTurnCount"), "fromTurnCount must be less than or equal to toTurnCount"),
        _ => panic!("refinement {hash} is not ported"),
    }
}

/// Whether the values are pairwise different, as a `Set` of them has as many members as there are values.
fn distinct<'a>(mut values: impl Iterator<Item = &'a Js>) -> bool {
    let mut seen = std::collections::HashSet::new();
    values.all(|value| seen.insert(match value {
        Js::Str(text) => format!("s{text}"),
        other => format!("{other:?}"),
    }))
}

/// The length of `JSON.stringify(value)` in UTF-16 units, or none where it throws or returns undefined.
fn json_length(value: &Js) -> Option<usize> {
    fn units(text: &str) -> usize {
        2 + text.chars().map(|c| match c {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf16(),
        }).sum::<usize>()
    }
    Some(match value {
        Js::Undefined => return None,
        Js::Null | Js::NonFinite(_) => 4,
        Js::Bool(flag) => if *flag { 4 } else { 5 },
        Js::Num(number) => js_number(number.as_f64()?).len(),
        Js::Str(text) => units(text),
        Js::Arr(items) => 2 + items.len().saturating_sub(1) + items.iter().map(|item| json_length(item).unwrap_or(4)).sum::<usize>(),
        Js::Obj(entries) => {
            let kept: Vec<usize> = entries.iter().filter_map(|(key, found)| json_length(found).map(|length| units(key) + 1 + length)).collect();
            2 + kept.len().saturating_sub(1) + kept.iter().sum::<usize>()
        }
    })
}

/// `Number.prototype.toString` for a finite number.
fn js_number(number: f64) -> String {
    if number == 0.0 {
        return "0".into();
    }
    let sign = if number < 0.0 { "-" } else { "" };
    let scientific = format!("{:e}", number.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let (k, n) = (digits.len() as i32, exponent.parse::<i32>().unwrap_or(0) + 1);
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let power = format!("e{}{}", if n - 1 < 0 { "-" } else { "+" }, (n - 1).abs());
        if k == 1 { format!("{digits}{power}") } else { format!("{}.{}{power}", &digits[..1], &digits[1..]) }
    };
    format!("{sign}{body}")
}

/// `DateTime.make(string)`: a string without a zone is read as UTC, and the rest as V8's `new Date(text)` reads it. The ISO forms
/// and a lone number are ported; V8's other legacy spellings ("Jan 1 2024", "12/25/2024") are not.
fn date_time(text: &str) -> Option<()> {
    use std::sync::OnceLock;
    static ZONE: OnceLock<regex::Regex> = OnceLock::new();
    static ISO: OnceLock<regex::Regex> = OnceLock::new();
    let zone = ZONE.get_or_init(|| regex::Regex::new(r"Z|GMT|[+-][0-9]{2}$|[+-][0-9]{2}:?[0-9]{2}$|\]$").expect("zone pattern"));
    let iso = ISO.get_or_init(|| regex::Regex::new(r"^([+-][0-9]{6}|[0-9]{4})(?:-([0-9]{2})(?:-([0-9]{2}))?)?(?:[Tt]([0-9]{2}):([0-9]{2})(?::([0-9]{2})(?:\.([0-9]+))?)?)?(?:Z|[+-][0-9]{2}:?[0-9]{2})?$").expect("ISO date pattern"));
    let full = if zone.is_match(text) { text.to_owned() } else { format!("{text}Z") };
    let Some(parts) = iso.captures(&full) else {
        // A lone number reads as a month (1 to 12) or a two-digit year.
        let digits = full.strip_suffix('Z').filter(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))?;
        return digits.parse::<u64>().ok().filter(|number| !(13..=31).contains(number)).map(|_| ());
    };
    let number = |at: usize| parts.get(at).map(|found| found.as_str().parse::<i64>().unwrap_or(0));
    let year = parts[1].parse::<i64>().ok()?;
    if &parts[1] == "-000000" {
        return None;
    }
    let (month, day) = (number(2).unwrap_or(1), number(3).unwrap_or(1));
    let (hour, minute, second) = (number(4).unwrap_or(0), number(5).unwrap_or(0), number(6).unwrap_or(0));
    let midnight_end = hour == 24 && minute == 0 && second == 0 && parts.get(7).is_none_or(|found| found.as_str().chars().all(|c| c == '0'));
    let in_range = year.abs() < 275_760 || (year == 275_760 && (month < 9 || (month == 9 && (day < 13 || (day == 13 && hour + minute + second == 0 && parts.get(7).is_none_or(|found| found.as_str().chars().all(|c| c == '0'))))))) && year >= -271_820;
    ((1..=12).contains(&month) && (1..=31).contains(&day) && (hour < 24 || midnight_end) && minute < 60 && second < 60 && in_range).then_some(())
}

/// Runs a transformation's decode. A refusal is the text after "Expected" in the issue it raises.
pub(crate) fn transform(hash: &str, value: Js) -> Result<Js, String> {
    let fields = |record: &Js| record.get("childThreadId").filter(|found| **found != Js::Undefined).cloned();
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
        // Number(value): the codec's way back from "NaN", "Infinity" and "-Infinity".
        ("aaad838e39369837", Js::Str(text)) => Ok(Js::NonFinite(match text.as_str() {
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            _ => f64::NAN,
        })),
        ("aaad838e39369837", number) => Ok(number),
        // Duration.millis(i): the milliseconds stand for the Duration they make.
        ("37d2fe1cb246a09b", number) => Ok(number),
        // self.toLowerCase()
        ("55c74d9b368f5dfe", Js::Str(text)) => Ok(Js::Str(text.to_lowercase())),
        // DateTime.make(s) as a UTC DateTime: the text stands for it.
        ("79acca51f7d9da1b", Js::Str(text)) => date_time(&text).map(|_| Js::Str(text)).ok_or_else(|| "a valid UTC DateTime string".to_owned()),
        // A subagent source: `childThreadId` kept when present.
        ("23115dae1df5b4a6", record @ Js::Obj(_)) => Ok(Js::Obj([("kind".to_owned(), Js::Str("subagent".into()))].into_iter().chain(fields(&record).map(|id| ("childThreadId".to_owned(), id))).collect())),
        // values.filter((value) => value !== undefined)
        ("c873fba597093fe5", Js::Arr(values)) => Ok(Js::Arr(values.into_iter().filter(|value| *value != Js::Undefined).collect())),
        _ => panic!("transformation {hash} is not ported"),
    }
}
