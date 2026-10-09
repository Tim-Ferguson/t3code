//! What a thread title is generated from (port of `ThreadTitleContext.ts` and the plain-text view of
//! assistant quotes in `assistantCitations.ts`): a bounded transcript that reserves room for what the
//! user asked before the assistant's findings, in conversation order.
use crate::text_generation::{js_trim, limit_title_message, utf16_len};
use serde_json::Value;
use std::{collections::HashMap, sync::OnceLock};

const MAX_CONTEXT: i64 = 8_000;
const MAX_MESSAGE: i64 = 2_000;
const OMITTED: &str = "[Earlier content truncated]\n\n";
const TRUNCATED_LEN: i64 = "\n[Content truncated]\n".len() as i64;

pub struct ContextMessage<'a> {
    pub role: &'a str,
    pub text: &'a str,
    pub attachments: &'a [Value],
}

#[derive(Debug, Clone, PartialEq)]
pub struct TitleContext {
    pub message: String,
    pub attachments: Vec<Value>,
}

struct Section<'a> {
    index: usize,
    message: &'a ContextMessage<'a>,
    prefix: String,
}

/// `formatThreadTitleContext`. Thinking traces and system messages are working notes, not what the
/// thread is about, and are left out.
pub fn format_thread_title_context(messages: &[ContextMessage]) -> TitleContext {
    let sections: Vec<Section> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| !(matches!(message.role, "system" | "reasoning") || (js_trim(message.text).is_empty() && message.attachments.is_empty())))
        .map(|(index, message)| Section { index, message, prefix: format!("{}:\n", message.role.to_uppercase()) })
        .collect();
    let contents: HashMap<usize, String> = sections
        .iter()
        .map(|section| {
            let text = js_trim(&assistant_citations_to_plain_text(section.message.text)).to_string();
            let names = section.message.attachments.iter().map(|attachment| attachment["name"].as_str().unwrap_or("")).collect::<Vec<_>>().join(", ");
            let joined = [text, if section.message.attachments.is_empty() { String::new() } else { format!("[Attachments: {names}]") }].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n");
            (section.index, joined)
        })
        .collect();
    let mut selected: HashMap<usize, String> = HashMap::new();
    let mut remaining = MAX_CONTEXT - utf16_len(OMITTED) as i64;
    fn add(section: &Section, budget: i64, contents: &HashMap<usize, String>, selected: &mut HashMap<usize, String>, remaining: &mut i64) {
        if selected.contains_key(&section.index) {
            return;
        }
        let limit = budget.min(*remaining) - utf16_len(&section.prefix) as i64 - 2;
        if limit <= TRUNCATED_LEN {
            return;
        }
        let limited = limit_title_message(&contents[&section.index], limit as usize);
        if limited.is_empty() {
            return;
        }
        let text = format!("{}{limited}", section.prefix);
        *remaining -= utf16_len(&text) as i64 + 2;
        selected.insert(section.index, text);
    }

    let first_user = sections.iter().find(|section| section.message.role == "user");
    if let Some(first) = first_user {
        add(first, MAX_MESSAGE, &contents, &mut selected, &mut remaining);
    }
    // Up to 6,000 characters go to user messages. Assistant output cannot evict them.
    for section in sections.iter().rev().filter(|section| section.message.role == "user") {
        add(section, MAX_MESSAGE.min(remaining - 2_000), &contents, &mut selected, &mut remaining);
    }
    for section in sections.iter().rev().filter(|section| section.message.role == "assistant") {
        add(section, MAX_MESSAGE, &contents, &mut selected, &mut remaining);
    }
    // Use spare space when the conversation has only a few messages.
    for role in ["user", "assistant"] {
        for section in sections.iter().rev() {
            let Some(previous) = selected.get(&section.index).cloned() else { continue };
            if section.message.role != role {
                continue;
            }
            let budget = (utf16_len(&previous) as i64 + remaining - utf16_len(&section.prefix) as i64).max(0) as usize;
            let expanded = format!("{}{}", section.prefix, limit_title_message(&contents[&section.index], budget));
            remaining -= utf16_len(&expanded) as i64 - utf16_len(&previous) as i64;
            selected.insert(section.index, expanded);
        }
    }
    let retained: Vec<&Section> = sections.iter().filter(|section| selected.contains_key(&section.index)).collect();
    let truncated = retained.iter().any(|section| selected[&section.index] != format!("{}{}", section.prefix, contents[&section.index]));
    let attachments: Vec<&Value> = retained.iter().flat_map(|section| section.message.attachments.iter()).collect();
    let first_attachment = first_user.and_then(|section| section.message.attachments.first());
    let recent: Vec<&Value> = attachments.into_iter().filter(|attachment| first_attachment.is_none_or(|first| attachment["id"] != first["id"])).collect();
    let keep = if first_attachment.is_some() { 3 } else { 4 };
    let tail = &recent[recent.len().saturating_sub(keep)..];
    TitleContext {
        message: format!(
            "{}{}",
            if truncated || retained.len() < sections.len() { OMITTED } else { "" },
            retained.iter().map(|section| selected[&section.index].as_str()).collect::<Vec<_>>().join("\n\n")
        ),
        attachments: first_attachment.into_iter().chain(tail.iter().copied()).cloned().collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// Assistant quotes
// ---------------------------------------------------------------------------------------------

const CITATION_HREF_PREFIX: &str = "t3-citation://v1/";
const MAX_CITATION_HREF_LENGTH: usize = 9 * (8_000 + 8_000) + 16_000;

struct Citation {
    text: String,
    comment: Option<String>,
}

/// `decodeURIComponent`, strict about malformed escapes and invalid UTF-8.
fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = value.get(at + 1..at + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok().filter(|_| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `parseAssistantCitationHref`: the quote a link carries, if it is a well-formed one.
fn parse_citation_href(href: &str) -> Option<Citation> {
    if !href.starts_with(CITATION_HREF_PREFIX) || href.len() > MAX_CITATION_HREF_LENGTH {
        return None;
    }
    let url = url::Url::parse(href).ok()?;
    let parts: Vec<&str> = url.path().strip_prefix('/')?.split('/').collect();
    if url.scheme() != "t3-citation" || url.host_str() != Some("v1") || parts.len() != 3 || !url.username().is_empty() || url.password().is_some() || url.port().is_some() || url.fragment().is_some_and(|fragment| !fragment.is_empty()) {
        return None;
    }
    let pairs: Vec<(String, String)> = url.query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
    let once = |key: &str| {
        let mut found = pairs.iter().filter(|(name, _)| name == key);
        let first = found.next()?;
        found.next().is_none().then(|| first.1.clone())
    };
    let comment = pairs.iter().find(|(name, _)| name == "comment").map(|(_, value)| value.clone());
    if pairs.len() != 5 + usize::from(comment.is_some()) || ["text", "start", "end", "prefix", "suffix"].iter().any(|key| once(key).is_none()) {
        return None;
    }
    let number = |key: &str| {
        let value = once(key)?;
        (!value.is_empty() && value.len() <= 16 && value.bytes().all(|byte| byte.is_ascii_digit())).then(|| value.parse::<u64>().ok()).flatten()
    };
    let (start, end) = (number("start")?, number("end")?);
    let (text, prefix, suffix) = (once("text")?, once("prefix")?, once("suffix")?);
    for part in &parts[..3] {
        let id = decode_uri_component(part)?;
        if js_trim(&id).is_empty() || utf16_len(js_trim(&id)) > 512 {
            return None;
        }
    }
    let valid = !text.is_empty()
        && utf16_len(&text) <= 8_000
        && comment.as_ref().is_none_or(|comment| utf16_len(comment) <= 8_000)
        && start <= (1u64 << 53) - 1
        && end <= (1u64 << 53) - 1
        && utf16_len(&prefix) <= 32
        && utf16_len(&suffix) <= 32
        && end > start
        && !js_trim(&text).is_empty();
    valid.then_some(Citation { text, comment })
}

/// `assistantCitationsToPlainText`: titles and previews carry the quoted text and the user's comment
/// without Markdown escaping.
pub fn assistant_citations_to_plain_text(prompt: &str) -> String {
    static LINK: OnceLock<regex::Regex> = OnceLock::new();
    let link = LINK.get_or_init(|| regex::Regex::new(&format!(r"\[Assistant quote\]\(({}[^\s)]+)\)", regex::escape(CITATION_HREF_PREFIX))).unwrap());
    link.replace_all(prompt, |captures: &regex::Captures| match parse_citation_href(&captures[1]) {
        None => captures[0].to_string(),
        Some(Citation { text, comment: None }) => text,
        Some(Citation { text, comment: Some(comment) }) => format!("{text}\nComment: {comment}"),
    })
    .into_owned()
}
