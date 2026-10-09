//! Provider errors are bounded and stripped of credentials before entering projections.
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use regex::{Captures, Regex};
use serde_json::{Value, json};
use std::sync::LazyLock;

const DEFAULT_MESSAGE: &str = "Provider turn failed.";
const JS_SPACE: &str =
    r"\x09-\x0D\x20\xA0\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";
static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r#"(?i)https?://[^{JS_SPACE}<>"']+"#)).unwrap());
static AUTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?i)(Bearer|Basic)[{JS_SPACE}]+[^{JS_SPACE},;]+")).unwrap()
});
static QUOTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r#"(?i)(["'](?:access[_-]?token|api[_-]?key|authorization|credential|password|secret|token)["'][{JS_SPACE}]*:[{JS_SPACE}]*["'])[^"']*(["'])"#)).unwrap()
});
static KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r#"(?i)((access[_-]?token|api[_-]?key|authorization|credential|password|secret|token)[{JS_SPACE}]*[:=][{JS_SPACE}]*)(?:"[^"]*"|'[^']*'|[^{JS_SPACE},;]+)"#)).unwrap()
});
static SECRET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"sk-[A-Za-z0-9_-]{16,}").unwrap());

fn word(character: char, insensitive: bool) -> bool {
    character.is_ascii_alphanumeric()
        || character == '_'
        || (insensitive && matches!(character, '\u{017f}' | '\u{212a}'))
}
fn boundary(value: &str, position: usize, insensitive: bool) -> bool {
    value[..position]
        .chars()
        .next_back()
        .is_some_and(|c| word(c, insensitive))
        != value[position..]
            .chars()
            .next()
            .is_some_and(|c| word(c, insensitive))
}
// Explicit JS word boundaries avoid Rust's broader Unicode \b. Invalid candidates
// advance by one character so a later valid token inside their span is retained.
fn replace(
    value: &str,
    regex: &Regex,
    accept: impl Fn(&Captures<'_>) -> bool,
    replacement: impl Fn(&Captures<'_>) -> String,
) -> String {
    let mut result = String::new();
    let mut cursor = 0;
    let mut copied = 0;
    while let Some(captures) = regex.captures_at(value, cursor) {
        let matched = captures.get(0).unwrap();
        if accept(&captures) {
            result.push_str(&value[copied..matched.start()]);
            result.push_str(&replacement(&captures));
            copied = matched.end();
            cursor = matched.end();
        } else {
            cursor = matched.start() + value[matched.start()..].chars().next().unwrap().len_utf8();
        }
    }
    result.push_str(&value[copied..]);
    result
}
fn redact_url(value: &str) -> String {
    let candidate = value.trim_end_matches([')', ',', '.', ';', '!', '?']);
    let trailing = &value[candidate.len()..];
    let Ok(mut url) = url::Url::parse(candidate) else {
        return "[REDACTED_URL]".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    format!("{url}{trailing}")
}
fn bounded(value: &str, maximum: usize) -> String {
    let value: String = value
        .chars()
        .map(|c| {
            if matches!(c, '\u{0}'..='\u{8}' | '\u{B}'..='\u{C}' | '\u{E}'..='\u{1F}' | '\u{7F}') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let value = replace(
        &value,
        &URL,
        |c| boundary(&value, c.get(0).unwrap().start(), true),
        |c| redact_url(&c[0]),
    );
    let value = replace(
        &value,
        &AUTH,
        |c| boundary(&value, c.get(0).unwrap().start(), true),
        |c| format!("{} [REDACTED]", &c[1]),
    );
    let value = replace(
        &value,
        &QUOTED,
        |_| true,
        |c| format!("{}[REDACTED]{}", &c[1], &c[2]),
    );
    let value = replace(
        &value,
        &KEY,
        |c| {
            boundary(&value, c.get(0).unwrap().start(), true)
                && boundary(&value, c.get(2).unwrap().end(), true)
        },
        |c| format!("{}[REDACTED]", &c[1]),
    );
    let value = replace(
        &value,
        &SECRET,
        |c| {
            let m = c.get(0).unwrap();
            boundary(&value, m.start(), false) && boundary(&value, m.end(), false)
        },
        |_| "[REDACTED]".into(),
    );
    let value = t3_contracts::trim_wire_string(&value);
    if value.encode_utf16().count() <= maximum {
        return value.to_owned();
    }
    let mut units = 0;
    let mut result = String::new();
    for character in value.chars() {
        if units + character.len_utf16() > maximum - 1 {
            break;
        }
        result.push(character);
        units += character.len_utf16();
    }
    result.push('…');
    result
}
fn cause_message(mut cause: &Value) -> Option<&str> {
    let mut message = None;
    for _ in 0..16 {
        if !cause.is_object() {
            break;
        }
        match cause["_tag"].as_str() {
            Some("ContextHandoffBudgetError") => {
                return Some(
                    "Insufficient context allowance for the provider handoff. Compact the target conversation or use a larger-context model; the current request has not been truncated.",
                );
            }
            Some("ClaudeBackgroundWorkBlocksQueryReplacementError") => {
                return cause["message"].as_str();
            }
            Some("ContextHandoffDeliveryUncertainError") => {
                return Some(
                    "T3 could not confirm whether conversation history reached the provider. Retry the turn to recover the session.",
                );
            }
            Some("ProviderAdapterTurnStartError") => {
                message = Some(
                    "The provider could not start this turn. Retry the turn; if it keeps failing, check the provider setup and server logs.",
                )
            }
            Some("ProviderAdapterEventStreamError") => {
                message = Some(
                    "The provider event stream closed unexpectedly. Retry the turn; if it keeps failing, check the provider and server logs.",
                )
            }
            Some("ProviderAdapterOpenSessionError") => {
                message = Some(
                    "The provider session could not be opened. Check that the provider is installed and signed in, then retry the turn.",
                )
            }
            Some("ProviderAdapterResumeThreadError") => {
                message = Some(
                    "The provider conversation could not be resumed. Retry the turn; if it keeps failing, check the provider and server logs.",
                )
            }
            _ => (),
        }
        cause = &cause["cause"];
    }
    message
}
static ISO_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^([+-][0-9]{6}|[0-9]{4})(?:-([0-9]{2})(?:-([0-9]{2}))?)?(?:T([0-9]{2}):([0-9]{2})(?::([0-9]{2})(?:\.([0-9]+))?)?(Z|[+-][0-9]{2}:[0-9]{2})?)?$").unwrap()
});
fn format_date(date: DateTime<Utc>) -> String {
    // JS Date keeps milliseconds, discarding additional fractional precision.
    let date = DateTime::from_timestamp_millis(date.timestamp_millis()).unwrap();
    let year = chrono::Datelike::year(&date);
    if (0..=9999).contains(&year) {
        date.to_rfc3339_opts(SecondsFormat::Millis, true)
    } else {
        format!("{year:+07}{}", date.format("-%m-%dT%H:%M:%S%.3fZ"))
    }
}
fn reset_at<T: TimeZone>(value: &str, timezone: &T) -> Option<String> {
    if let Some(parts) = ISO_DATE.captures(value) {
        let number = |index, fallback| {
            parts
                .get(index)
                .map_or(Some(fallback), |m| m.as_str().parse::<u32>().ok())
        };
        if &parts[1] == "-000000" || (parts.get(4).is_some() && parts.get(3).is_none()) {
            return None;
        }
        let year: i32 = parts[1].parse().ok()?;
        let month = number(2, 1)?;
        let day = number(3, 1)?;
        if !(1..=31).contains(&day) {
            return None;
        }
        let date = NaiveDate::from_ymd_opt(year, month, 1)?
            .checked_add_days(chrono::Days::new((day - 1).into()))?;
        let hour = number(4, 0)?;
        let minute = number(5, 0)?;
        let second = number(6, 0)?;
        let fraction = parts.get(7).map_or("", |m| m.as_str());
        let millis: u32 = format!("{fraction:0<3}")[..3].parse().ok()?;
        if hour > 24
            || minute > 59
            || second > 59
            || (hour == 24 && (minute != 0 || second != 0 || fraction.bytes().any(|b| b != b'0')))
        {
            return None;
        }
        let date = date.and_hms_milli_opt(hour.min(23), minute, second, millis)?;
        let date = if hour == 24 {
            date.checked_add_signed(chrono::Duration::hours(1))?
        } else {
            date
        };
        let date = if let Some(zone) = parts.get(8) {
            let zone = zone.as_str();
            let offset = if zone.eq_ignore_ascii_case("z") {
                0
            } else {
                let hours: i32 = zone[1..3].parse().ok()?;
                let minutes: i32 = zone[4..].parse().ok()?;
                if hours > 23 || minutes > 59 {
                    return None;
                }
                (hours * 3600 + minutes * 60) * if zone.starts_with('-') { -1 } else { 1 }
            };
            date.and_utc()
                .checked_sub_signed(chrono::Duration::seconds(offset.into()))?
        } else if parts.get(4).is_some() {
            timezone
                .from_local_datetime(&date)
                .earliest()?
                .with_timezone(&Utc)
        } else {
            date.and_utc()
        };
        return Some(format_date(date));
    }
    if let Ok(date) = DateTime::parse_from_rfc2822(value) {
        return Some(format_date(date.with_timezone(&Utc)));
    }
    // Legacy slash calendar forms accepted by the original Node runtime use
    // local time; ISO date-only forms above deliberately use UTC instead.
    for format in [
        "%m/%d/%Y %H:%M:%S",
        "%m/%d/%Y",
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d",
        "%Y-%m-%d %H:%M:%S",
    ] {
        let date = NaiveDateTime::parse_from_str(value, format)
            .ok()
            .or_else(|| {
                NaiveDate::parse_from_str(value, format)
                    .ok()?
                    .and_hms_opt(0, 0, 0)
            });
        if let Some(date) = date {
            return Some(format_date(
                timezone
                    .from_local_datetime(&date)
                    .earliest()?
                    .with_timezone(&Utc),
            ));
        }
    }
    None
}

/// The cause is a structured category, never an arbitrary defect's display text.
/// Adapter-specific callers supply explicit class/code/retryable only after
/// interpreting their native error protocol.
pub(crate) fn make(input: &Value) -> Value {
    make_with_timezone(input, &Local)
}
fn make_with_timezone<T: TimeZone>(input: &Value, timezone: &T) -> Value {
    let message = bounded(
        input["message"]
            .as_str()
            .or_else(|| cause_message(&input["cause"]))
            .unwrap_or(DEFAULT_MESSAGE),
        4096,
    );
    let code = input["code"]
        .as_str()
        .or_else(|| input["cause"]["code"].as_str())
        .map(|s| bounded(s, 128))
        .filter(|s| !s.is_empty());
    let mut result = json!({"class":input["class"].as_str().unwrap_or("unknown"),"message":if message.is_empty(){DEFAULT_MESSAGE.to_owned()}else{message},"code":code,"retryable":input["retryable"].as_bool()});
    if input["class"] == "usage_limit" {
        if let Some(reset) = input["resetAt"]
            .as_str()
            .and_then(|value| reset_at(value, timezone))
        {
            result["resetAt"] = json!(reset)
        }
    }
    result
}
fn codex_error_code(info: &Value) -> Option<String> {
    if let Some(code) = info.as_str() {
        return Some(code.into());
    }
    if info.as_array().is_some_and(|items| !items.is_empty()) {
        return Some("0".into());
    }
    let object = info.as_object()?;
    let numeric = object.keys().filter_map(|key| {
        key.parse::<u32>()
            .ok()
            .filter(|number| *number < u32::MAX && number.to_string().as_str() == key.as_str())
    });
    numeric
        .min()
        .map(|number| number.to_string())
        .or_else(|| object.keys().next().cloned())
}
pub(crate) fn codex_error(error: &Value, will_retry: bool) -> Value {
    let code = codex_error_code(&error["codexErrorInfo"]);
    let details = error["additionalDetails"]
        .as_str()
        .map(t3_contracts::trim_wire_string)
        .filter(|value| !value.is_empty());
    let class = if code
        .as_deref()
        .is_some_and(|code| matches!(code, "usageLimitExceeded" | "rateLimitExceeded"))
    {
        "usage_limit"
    } else if will_retry
        && code
            .as_deref()
            .is_some_and(|code| code.starts_with("http") || code.starts_with("responseStream"))
    {
        "transport_error"
    } else {
        "provider_error"
    };
    make(
        &json!({"message":details.or_else(||error["message"].as_str()),"code":code,"class":class,"retryable":will_retry.then_some(true)}),
    )
}
pub(crate) fn codex_terminal(error: &Value, previous: Option<&(String, Value)>) -> Value {
    let code = codex_error_code(&error["codexErrorInfo"]);
    if let Some((native_message, failure)) = previous {
        if error["message"]
            .as_str()
            .is_none_or(|message| message == native_message)
            && (error.is_null() || failure["code"] == json!(code))
        {
            return failure.clone();
        }
    }
    make(
        &json!({"message":error["message"],"code":code,"class":if code.as_deref().is_some_and(|code|matches!(code,"usageLimitExceeded"|"rateLimitExceeded")){"usage_limit"}else{"provider_error"}}),
    )
}
pub(crate) fn acp_prompt(agent_id: &str, cause: &t3_acp::AcpError) -> Value {
    let request = match cause {
        t3_acp::AcpError::Failure(failure) => match failure.as_ref() {
            t3_acp::errors::Failure::Request(error) => {
                Some((error.code, error.error_message.as_str()))
            }
            _ => None,
        },
        t3_acp::AcpError::ResponseError { error, .. } | t3_acp::AcpError::Request(error) => {
            Some((error.code, error.message.as_str()))
        }
        _ => None,
    };
    if let Some((code, message)) = request {
        make(
            &json!({"message":message,"code":code.to_string(),"class":if agent_id=="mistral-vibe"&&code== -31001{"usage_limit"}else{"provider_error"}}),
        )
    } else {
        make(&json!({"class":"provider_error"}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_unchanged_native_failure_classifiers() {
        let rows: Vec<Value> = serde_json::from_str(include_str!(
            "../tests/fixtures/provider-native-failures.json"
        ))
        .unwrap();
        for (index, row) in rows.iter().enumerate() {
            let input = &row["input"];
            let actual = match row["operation"].as_str().unwrap() {
                "codexError" => codex_error(&input["error"], input["willRetry"] == true),
                "codexTerminal" => {
                    let previous = input.get("previous").map(|previous| {
                        (
                            previous["nativeMessage"].as_str().unwrap().to_string(),
                            previous["failure"].clone(),
                        )
                    });
                    codex_terminal(&input["error"], previous.as_ref())
                }
                "acpPrompt" => {
                    let cause = if let Some(request) = input.get("request") {
                        t3_acp::errors::RequestError::new(
                            request["code"].as_i64().unwrap(),
                            request["message"].as_str().unwrap(),
                            None,
                        )
                        .into()
                    } else {
                        t3_acp::AcpError::Transport("arbitrary private defect".into())
                    };
                    acp_prompt(input["agentId"].as_str().unwrap(), &cause)
                }
                _ => panic!("Unknown native failure operation"),
            };
            assert_eq!(
                actual, row["output"],
                "native source row {index}: {}",
                row["operation"]
            );
        }
    }
    #[test]
    fn matches_unchanged_provider_failure_source() {
        let rows: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/provider-failures.json")).unwrap();
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(
                make_with_timezone(&row["input"], &Utc),
                row["output"],
                "source row {index}: {}",
                row["name"]
            );
        }
    }
}
