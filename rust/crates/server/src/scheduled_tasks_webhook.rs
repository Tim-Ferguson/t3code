//! The pure parts of a webhook-triggered scheduled task: the prompt template, credential redaction for
//! the delivery log, and HMAC-SHA256 signature verification (port of `webhookTemplate.ts` and
//! `webhookVerification.ts`).
//!
//! The syntax is deliberately just `{{path}}` lookups, with no conditionals, defaults, filters or
//! escaping, so it never grows into a template language:
//! `{{body.a.b.0}}` a field of a JSON or form-encoded body, `{{headers.name}}` a request header
//! (case-insensitive), `{{query.name}}` a URL query parameter, `{{body}}` the raw body text and
//! `{{request}}` method, path, query, headers and body. Strings and numbers render as text, objects and
//! arrays as JSON. A path with no value renders empty and is reported in `missing`. Credential-looking
//! headers and query parameters are redacted wherever the whole set renders; naming one gives its raw
//! value.
use hmac::{Hmac, Mac};
use indexmap::IndexMap;
use serde_json::Value;
use sha2::{Digest, Sha256};
use t3_contracts::{ScheduledTaskWebhookSignature, WebhookSignatureEncoding, trim_wire_string};

pub struct WebhookRequest {
    pub method: String,
    pub path: String,
    /// Raw query string without the leading `?`.
    pub query: String,
    /// Lowercased header names, in arrival order.
    pub headers: IndexMap<String, String>,
    pub body_text: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedWebhookPrompt {
    pub prompt: String,
    pub missing: Vec<String>,
}

const REDACTED: &str = "[redacted]";

/// `/^(authorization|proxy-authorization|cookie|set-cookie)$|token|secret|signature|key|password|auth/i`.
fn looks_like_credential(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(name.as_str(), "authorization" | "proxy-authorization" | "cookie" | "set-cookie")
        || ["token", "secret", "signature", "key", "password", "auth"].iter().any(|word| name.contains(word))
}

pub fn redact_headers(headers: &IndexMap<String, String>) -> IndexMap<String, String> {
    headers.iter().map(|(name, value)| (name.clone(), if looks_like_credential(name) { REDACTED.into() } else { value.clone() })).collect()
}

/// `decodeURIComponent`, which refuses malformed escapes and invalid UTF-8.
fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok().filter(|_| hex.bytes().all(|b| b.is_ascii_hexdigit()))?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Redacts credential-named values in a raw query string, keeping the rest as sent.
pub fn redact_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    query
        .split('&')
        .map(|part| match part.split_once('=') {
            None => part.to_owned(),
            Some((name, _)) => {
                // A malformed escape is matched as sent.
                let decoded = decode_uri_component(&name.replace('+', " ")).unwrap_or_else(|| name.to_owned());
                if looks_like_credential(&decoded) { format!("{name}={REDACTED}") } else { part.to_owned() }
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn form_pairs(text: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(text.as_bytes()).map(|(name, value)| (name.into_owned(), value.into_owned())).collect()
}
/// `Object.fromEntries`: a repeated name keeps its first position and takes the last value.
fn pairs_object(pairs: Vec<(String, String)>) -> Value {
    let mut object = serde_json::Map::new();
    for (name, value) in pairs {
        object.insert(name, Value::String(value));
    }
    Value::Object(object)
}

fn parse_body(request: &WebhookRequest) -> Option<Value> {
    let content_type = request.headers.get("content-type").map(|value| value.to_lowercase()).unwrap_or_default();
    if content_type.contains("application/x-www-form-urlencoded") {
        return Some(pairs_object(form_pairs(&request.body_text)));
    }
    // Senders are inconsistent about content types, so any body that parses as JSON is addressable.
    serde_json::from_str(&request.body_text).ok()
}

fn is_array_index(key: &str) -> Option<u64> {
    let index = key.parse::<u64>().ok()?;
    (index < u32::MAX as u64 && index.to_string() == key).then_some(index)
}

/// `Object.hasOwn` plus the property read, for the values JSON can produce.
fn own_property<'a>(current: &'a Value, segment: &str) -> Option<std::borrow::Cow<'a, Value>> {
    match current {
        Value::Object(object) => object.get(segment).map(std::borrow::Cow::Borrowed),
        Value::Array(items) => {
            if segment == "length" {
                return Some(std::borrow::Cow::Owned(Value::from(items.len())));
            }
            is_array_index(segment).and_then(|index| items.get(index as usize)).map(std::borrow::Cow::Borrowed)
        }
        _ => None,
    }
}

fn lookup(root: Option<&Value>, segments: &[&str]) -> Option<Value> {
    let mut current = std::borrow::Cow::Borrowed(root?);
    for segment in segments {
        let next = own_property(current.as_ref(), segment)?.into_owned();
        current = std::borrow::Cow::Owned(next);
    }
    Some(current.into_owned())
}

/// `Number.prototype.toString` for a finite number.
pub fn js_number(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-Infinity".into() } else { "Infinity".into() };
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap();
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let (k, n) = (digits.len() as i32, exponent.parse::<i32>().unwrap() + 1);
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let exponent = format!("e{}{}", if e < 0 { '-' } else { '+' }, e.abs());
        if k == 1 { format!("{digits}{exponent}") } else { format!("{}.{}{exponent}", &digits[..1], &digits[1..]) }
    };
    format!("{sign}{body}")
}

fn js_string(value: &str, out: &mut String) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `JSON.stringify(value, null, 2)`: integer-like keys first, in order, then the others as inserted.
fn js_json(value: &Value, depth: usize, out: &mut String) {
    let pad = |depth: usize| "  ".repeat(depth);
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            let number = number.as_f64().unwrap_or(f64::NAN);
            out.push_str(&if number.is_finite() { js_number(number) } else { "null".into() });
        }
        Value::String(text) => js_string(text, out),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push_str("[\n");
            for (index, item) in items.iter().enumerate() {
                out.push_str(&pad(depth + 1));
                js_json(item, depth + 1, out);
                out.push_str(if index + 1 < items.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(depth));
            out.push(']');
        }
        Value::Object(object) if object.is_empty() => out.push_str("{}"),
        Value::Object(object) => {
            let mut entries: Vec<(&String, &Value)> = object.iter().collect();
            entries.sort_by_key(|(key, _)| is_array_index(key).map_or((1, 0), |index| (0, index)));
            out.push_str("{\n");
            for (index, (key, item)) in entries.iter().enumerate() {
                out.push_str(&pad(depth + 1));
                js_string(key, out);
                out.push_str(": ");
                js_json(item, depth + 1, out);
                out.push_str(if index + 1 < entries.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(depth));
            out.push('}');
        }
    }
}

fn stringify(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => js_number(number.as_f64().unwrap_or(f64::NAN)),
        Value::Bool(flag) => flag.to_string(),
        other => {
            let mut out = String::new();
            js_json(other, 0, &mut out);
            out
        }
    }
}

fn format_webhook_request(request: &WebhookRequest) -> String {
    let query = redact_query(&request.query);
    let mut lines = vec![format!("{} {}{}", request.method, request.path, if query.is_empty() { String::new() } else { format!("?{query}") })];
    lines.extend(redact_headers(&request.headers).iter().map(|(name, value)| format!("{name}: {value}")));
    lines.push(String::new());
    lines.push(request.body_text.clone());
    lines.join("\n")
}

fn headers_value(headers: &IndexMap<String, String>) -> Value {
    Value::Object(headers.iter().map(|(name, value)| (name.clone(), Value::String(value.clone()))).collect())
}

pub fn render_webhook_prompt(template: &str, request: &WebhookRequest) -> RenderedWebhookPrompt {
    let mut missing: Vec<String> = Vec::new();
    let mut body: Option<Option<Value>> = None;
    let mut resolve = |expression: &str| -> Option<Value> {
        let mut parts = expression.split('.');
        let root = parts.next().unwrap_or_default();
        let segments: Vec<&str> = parts.collect();
        match root {
            "request" => segments.is_empty().then(|| Value::String(format_webhook_request(request))),
            "body" if segments.is_empty() => Some(Value::String(request.body_text.clone())),
            "body" => lookup(body.get_or_insert_with(|| parse_body(request)).as_ref(), &segments),
            "headers" if segments.is_empty() => Some(headers_value(&redact_headers(&request.headers))),
            "headers" => request.headers.get(&segments.join(".").to_lowercase()).map(|value| Value::String(value.clone())),
            "query" if segments.is_empty() => Some(pairs_object(form_pairs(&redact_query(&request.query)))),
            "query" => {
                let name = segments.join(".");
                form_pairs(&request.query).into_iter().find(|(candidate, _)| *candidate == name).map(|(_, value)| Value::String(value))
            }
            _ => None,
        }
    };
    // `/\{\{\s*([^{}]*?)\s*\}\}/g`: a placeholder holds no braces, and its expression is trimmed.
    let mut prompt = String::new();
    let mut index = 0;
    while index < template.len() {
        let rest = &template[index..];
        let matched = rest.strip_prefix("{{").and_then(|after| {
            let end = after.find(['{', '}'])?;
            after[end..].starts_with("}}").then(|| (trim_wire_string(&after[..end]), 2 + end + 2))
        });
        match matched {
            Some((expression, length)) => {
                match resolve(expression) {
                    None | Some(Value::Null) => {
                        if !missing.iter().any(|seen| seen == expression) {
                            missing.push(expression.to_owned());
                        }
                    }
                    Some(value) => prompt.push_str(&stringify(&value)),
                }
                index += length;
            }
            None => {
                let character = rest.chars().next().unwrap();
                prompt.push(character);
                index += character.len_utf8();
            }
        }
    }
    RenderedWebhookPrompt { prompt, missing }
}

// ---- signatures --------------------------------------------------------------------------------

/// Constant-time string comparison that does not leak length through timing.
pub fn constant_time_equals(a: &str, b: &str) -> bool {
    let (a, b) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    a.iter().zip(b.iter()).fold(0u8, |difference, (left, right)| difference | (left ^ right)) == 0
}

/// Checks an HMAC-SHA256 signature over the raw body bytes, as GitHub, Linear, Shopify and most other
/// signing senders do. The header value is `<prefix><digest>`, with the digest hex- or base64-encoded.
pub fn verify_webhook_signature(signature: &ScheduledTaskWebhookSignature, secret: &str, headers: &IndexMap<String, String>, body: &[u8]) -> bool {
    use base64::Engine;
    let Some(received) = headers.get(&signature.header.as_str().to_lowercase()) else { return false };
    let value = trim_wire_string(received);
    let prefix = &signature.prefix;
    if !prefix.is_empty() && !value.to_lowercase().starts_with(&prefix.to_lowercase()) {
        return false;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let hex = signature.encoding == WebhookSignatureEncoding::Hex;
    let expected = if hex { digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>() } else { base64::engine::general_purpose::STANDARD.encode(digest) };
    // `value.slice(prefix.length)` counts UTF-16 code units.
    let units: Vec<u16> = value.encode_utf16().collect();
    let candidate = String::from_utf16_lossy(units.get(prefix.encode_utf16().count()..).unwrap_or(&[]));
    constant_time_equals(&if hex { candidate.to_lowercase() } else { candidate }, &expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn fixture() -> Vec<Value> {
        include_str!("../tests/fixtures/scheduled-task-schedule.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
    fn headers(value: &Value) -> IndexMap<String, String> {
        value.as_object().unwrap().iter().map(|(name, value)| (name.clone(), value.as_str().unwrap().to_owned())).collect()
    }

    #[test]
    fn prompt_templates_render_like_the_original() {
        let mut checked = 0;
        for row in fixture().iter().filter(|row| row["type"] == "template") {
            let request = &row["request"];
            let request = WebhookRequest {
                method: request["method"].as_str().unwrap().into(),
                path: request["path"].as_str().unwrap().into(),
                query: request["query"].as_str().unwrap().into(),
                headers: headers(&request["headers"]),
                body_text: request["bodyText"].as_str().unwrap().into(),
            };
            let rendered = render_webhook_prompt(row["template"].as_str().unwrap(), &request);
            assert_eq!(rendered.prompt, row["rendered"]["prompt"].as_str().unwrap(), "{}", row["template"]);
            let missing: Vec<&str> = row["rendered"]["missing"].as_array().unwrap().iter().map(|name| name.as_str().unwrap()).collect();
            assert_eq!(rendered.missing, missing, "{}", row["template"]);
            checked += 1;
        }
        assert_eq!(checked, 29);
    }

    #[test]
    fn credentials_are_redacted_and_signatures_verified_like_the_original() {
        let (mut redactions, mut verifications) = (0, 0);
        for row in fixture() {
            match row["type"].as_str().unwrap() {
                "redact-headers" => {
                    let redacted = redact_headers(&headers(&row["headers"]));
                    assert_eq!(serde_json::to_value(&redacted).unwrap(), row["redacted"]);
                    redactions += 1;
                }
                "redact-query" => {
                    assert_eq!(redact_query(row["query"].as_str().unwrap()), row["redacted"].as_str().unwrap(), "{}", row["query"]);
                    redactions += 1;
                }
                "verify" => {
                    let signature: ScheduledTaskWebhookSignature = serde_json::from_value(row["signature"].clone()).unwrap();
                    let body = base64::engine::general_purpose::STANDARD.decode(row["bodyBase64"].as_str().unwrap()).unwrap();
                    assert_eq!(verify_webhook_signature(&signature, row["secret"].as_str().unwrap(), &headers(&row["headers"]), &body), row["verified"], "{row}");
                    verifications += 1;
                }
                "equals" => assert_eq!(constant_time_equals(row["a"].as_str().unwrap(), row["b"].as_str().unwrap()), row["equal"]),
                _ => {}
            }
        }
        assert_eq!((redactions, verifications), (15, 15));
    }

    #[test]
    fn numbers_print_the_way_javascript_prints_them() {
        for (value, text) in [
            (0.0, "0"), (-0.0, "0"), (1.0, "1"), (1.5, "1.5"), (-2.25, "-2.25"), (1e21, "1e+21"), (1e20, "100000000000000000000"), (1.5e-7, "1.5e-7"),
            (0.000001, "0.000001"), (123456789012345680000.0, "123456789012345680000"), (12345678901234567890.0, "12345678901234567000"), (0.1 + 0.2, "0.30000000000000004"),
            (f64::MAX, "1.7976931348623157e+308"), (5e-324, "5e-324"),
        ] {
            assert_eq!(js_number(value), text);
        }
    }
}
