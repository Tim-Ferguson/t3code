//! Composer context: the chips attached to a message (a terminal selection, a review comment on
//! diff lines, another thread), the records they become on the wire and the references that
//! place them in the text. Port of `composerContextRecords.ts`, `terminalContext.ts` and
//! `composerContextReferences.ts`, replayed against them by `tests/composer_context_parity.rs`.
//!
//! A chip's payload is its record; the text names it with `[label](t3-context://v1/<kind>/<id>)`.
//! Chips here are kept beside the prompt, so the references that tie them to it are added after
//! the prose when the message is sent, unless a saved prompt already holds them.
use crate::drafts::context_id;
use serde_json::{Value, json};

pub const LABEL_MAX_CHARS: usize = 200;
pub const REVIEW_TEXT_MAX_CHARS: usize = 16_000;
pub const REVIEW_DIFF_MAX_CHARS: usize = 32_000;
const TRUNCATION_MARKER: &str = "\n\u{2026} truncated \u{2026}";

/// Raw producer IDs are always scoped, even when they already start with the kind name.
pub fn scoped_id(kind: &str, producer_id: &str) -> String {
    context_id(&format!("{kind}_{producer_id}"))
}

fn js_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}
/// A label that survives a Markdown link: no brackets or line breaks, bounded, never empty.
pub fn sanitize_label(label: &str, kind: &str) -> String {
    let replaced: String = label.chars().map(|c| if matches!(c, '[' | ']' | '\\' | '\r' | '\n') { ' ' } else { c }).collect();
    let mut collapsed = String::new();
    let mut in_space = false;
    for c in replaced.chars() {
        if js_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    let cleaned: String = collapsed.trim_matches(js_space).chars().take(LABEL_MAX_CHARS).collect();
    if cleaned.is_empty() { kind.to_owned() } else { cleaned }
}
pub fn href(kind: &str, id: &str) -> String {
    format!("t3-context://v1/{kind}/{id}")
}
/// `[label](t3-context://v1/<kind>/<contextId>)`.
pub fn reference(kind: &str, id: &str, label: &str) -> String {
    format!("{}[{}]({})", if kind == "image" { "!" } else { "" }, sanitize_label(label, kind), href(kind, id))
}

fn clamp(value: &str, max: usize) -> String {
    let count = value.chars().count();
    if count <= max {
        return value.to_owned();
    }
    let keep = max.saturating_sub(TRUNCATION_MARKER.chars().count());
    format!("{}{TRUNCATION_MARKER}", value.chars().take(keep).collect::<String>())
}

// ---- terminal selections --------------------------------------------------------------

pub fn normalize_terminal_text(text: &str) -> String {
    text.replace("\r\n", "\n").trim_matches('\n').to_owned()
}
fn terminal_range(start: u64, end: u64) -> String {
    if start == end { format!("line {start}") } else { format!("lines {start}-{end}") }
}
pub fn terminal_label(label: &str, start: u64, end: u64) -> String {
    format!("{label} {}", terminal_range(start, end))
}
/// The lines a terminal selection covers, from the position its surface reports
/// (`{"start":{"x":0,"y":12},"end":{"x":5,"y":14}}`, rows from 0 in the whole scrollback): the
/// first line is the row plus one and the last is not before it (`terminalSelectionLineRange`).
/// None when the surface reports none, and the chip counts its own lines instead.
pub fn terminal_selection_lines(position: &str) -> Option<(u64, u64)> {
    let position: Value = serde_json::from_str(position).ok()?;
    let start = position["start"]["y"].as_u64()? + 1;
    let end = position["end"]["y"].as_u64()? + 1;
    Some((start, end.max(start)))
}

/// What a selection in a terminal becomes: `None` when nothing is selected.
pub fn terminal_record(id: &str, terminal_id: &str, terminal_label_text: &str, line_start: u64, line_end: u64, text: &str) -> Option<Value> {
    let text = normalize_terminal_text(text);
    let (terminal_id, terminal_label_text) = (terminal_id.trim(), terminal_label_text.trim());
    if text.is_empty() || terminal_id.is_empty() || terminal_label_text.is_empty() {
        return None;
    }
    let line_start = line_start.max(1);
    let line_end = line_end.max(line_start);
    Some(json!({
        "version":1,
        "contextId":scoped_id("terminal", id),
        "kind":"terminal",
        "label":sanitize_label(&terminal_label(terminal_label_text, line_start, line_end), "terminal"),
        "terminalId":terminal_id,
        "terminalLabel":terminal_label_text,
        "lineStart":line_start,
        "lineEnd":line_end,
        "text":text,
    }))
}

// ---- review comments ---------------------------------------------------------------------

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}
fn pull_request_number(comment: &Value) -> Option<u64> {
    if let Some(number) = comment["pullRequest"]["number"].as_u64() {
        return Some(number);
    }
    comment["filePath"].as_str()?.strip_prefix("PR #")?.parse().ok()
}
fn is_pull_request_summary(comment: &Value) -> bool {
    if comment["pullRequest"].is_object() {
        return true;
    }
    comment["sectionId"].as_str().is_some_and(|section| section.starts_with("pull-request:"))
        && comment["diff"].as_str().is_some_and(|diff| diff.trim().is_empty())
        && comment["filePath"].as_str().is_some_and(|path| path.strip_prefix("PR #").is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())))
}
/// `reviewCommentContextLabel`: `a.ts L12 to L14`, `README.md L3 (before)`, or `#9` for a pull request.
pub fn review_label(comment: &Value) -> String {
    if let (true, Some(number)) = (is_pull_request_summary(comment), pull_request_number(comment)) {
        return format!("#{number}");
    }
    let range = comment["rangeLabel"].as_str().unwrap_or("");
    let label = match diff_range(range) {
        Some((sign, first, second)) => format!("L{first}{}{}", second.map(|second| format!(" to L{second}")).unwrap_or_default(), if sign == '-' { " (before)" } else { "" }),
        None => range.to_owned(),
    };
    format!("{} {label}", basename(comment["filePath"].as_str().unwrap_or("")))
}
/// `^([+-])(\d+)(?: to \1(\d+))?$`
fn diff_range(range: &str) -> Option<(char, &str, Option<&str>)> {
    let sign = range.chars().next().filter(|c| matches!(c, '+' | '-'))?;
    let rest = &range[1..];
    let digits = |text: &str| !text.is_empty() && text.chars().all(|c| c.is_ascii_digit());
    match rest.split_once(" to ") {
        None => digits(rest).then_some((sign, rest, None)),
        Some((first, second)) => {
            let second = second.strip_prefix(sign)?;
            (digits(first) && digits(second)).then_some((sign, first, Some(second)))
        }
    }
}
pub fn review_record(comment: &Value) -> Value {
    let mut record = json!({
        "version":1,
        "contextId":scoped_id("review-comment", comment["id"].as_str().unwrap_or("")),
        "kind":"review-comment",
        "label":sanitize_label(&review_label(comment), "review-comment"),
        "sectionId":comment["sectionId"],
        "sectionTitle":comment["sectionTitle"],
        "filePath":comment["filePath"],
        "startIndex":comment["startIndex"],
        "endIndex":comment["endIndex"],
        "rangeLabel":comment["rangeLabel"],
        "text":clamp(comment["text"].as_str().unwrap_or(""), REVIEW_TEXT_MAX_CHARS),
        "diff":clamp(comment["diff"].as_str().unwrap_or(""), REVIEW_DIFF_MAX_CHARS),
    });
    if let Some(language) = comment.get("fenceLanguage").filter(|value| !value.is_null()) {
        record["fenceLanguage"] = language.clone();
    }
    if let Some(pull_request) = comment.get("pullRequest").filter(|value| !value.is_null()) {
        record["pullRequest"] = pull_request.clone();
    }
    record
}

// ---- thread references ---------------------------------------------------------------------

/// Another thread on the same server, attached so the agent can read its history. One record per
/// thread: attaching it twice reuses the chip.
pub fn thread_record(environment_id: &str, thread_id: &str, title: &str) -> Value {
    let label = sanitize_label(title, "thread");
    json!({"version":1,"kind":"thread","contextId":scoped_id("thread", thread_id),"label":label,"environmentId":environment_id,"threadId":thread_id,"title":label})
}

// ---- drafts and messages ---------------------------------------------------------------------

/// The records a draft saved by the original app holds, as wire records: terminal selections
/// without text have expired and are left out, as the original leaves them out.
pub fn records_from_draft(draft: &Value) -> Vec<Value> {
    let mut records = Vec::new();
    for entry in draft["terminalContexts"].as_array().into_iter().flatten() {
        let (Some(id), Some(terminal_id), Some(label)) = (entry["id"].as_str(), entry["terminalId"].as_str(), entry["terminalLabel"].as_str()) else { continue };
        let (start, end) = (entry["lineStart"].as_f64().unwrap_or(1.0).max(0.0) as u64, entry["lineEnd"].as_f64().unwrap_or(1.0).max(0.0) as u64);
        if let Some(record) = terminal_record(id, terminal_id, label, start, end, entry["text"].as_str().unwrap_or("")) {
            records.push(record);
        }
    }
    for comment in draft["reviewComments"].as_array().into_iter().flatten().filter(|comment| comment["id"].is_string()) {
        records.push(review_record(comment));
    }
    for record in draft["threadContexts"].as_array().into_iter().flatten().filter(|record| record["kind"] == "thread") {
        records.push(record.clone());
    }
    records
}
/// Whether a saved draft holds context this build cannot show or send (annotations and picked
/// page elements, which need a preview panel).
pub fn holds_unsupported(draft: &Value) -> bool {
    ["previewAnnotations", "elementContexts"].iter().any(|key| draft[*key].as_array().is_some_and(|list| !list.is_empty()))
}

fn referenced_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("](t3-context://v1/") {
        let tail = &rest[at + 18..];
        let end = tail.find(|c: char| c == ')' || c.is_whitespace()).unwrap_or(tail.len());
        if let Some((_, id)) = tail[..end].split_once('/') {
            ids.push(id.to_owned());
        }
        rest = &tail[end..];
    }
    ids
}
/// The text and context of a message: the prompt, then a reference to each chip it does not
/// already name, with every chip's record in order.
pub fn compose(prompt: &str, records: &[Value]) -> (String, Option<Value>) {
    if records.is_empty() {
        return (prompt.to_owned(), None);
    }
    let named = referenced_ids(prompt);
    let mut text = prompt.trim_end().to_owned();
    for record in records {
        let (Some(kind), Some(id)) = (record["kind"].as_str(), record["contextId"].as_str()) else { continue };
        if named.iter().any(|named| named == id) {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text += &reference(kind, id, record["label"].as_str().unwrap_or(""));
    }
    (text, Some(json!({"version":1,"records":records})))
}
/// How a chip is introduced: the kind in words.
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "terminal" => "Terminal",
        "review-comment" => "Review comment",
        "thread" => "Thread",
        _ => "Context",
    }
}

/// `inferReviewCommentFenceLanguage`: the extension, a dotfile's name, else `text`.
pub fn fence_language(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or("").to_lowercase();
    match name.rfind('.') {
        Some(at) if at > 0 && at + 1 < name.len() => name[at + 1..].to_owned(),
        Some(0) if name.len() > 1 => name[1..].to_owned(),
        _ => "text".to_owned(),
    }
}
/// A comment on one line of a diff: `sign` is `+` for an added or unchanged line (its new
/// number), `-` for a removed one (its old number).
pub fn line_comment(id: &str, section_id: &str, section_title: &str, file_path: &str, sign: char, number: u32, line_text: &str, text: &str) -> Value {
    let marker = if sign == '-' { '-' } else { '+' };
    json!({
        "id":id,"sectionId":section_id,"sectionTitle":section_title,"filePath":file_path,
        "startIndex":number.saturating_sub(1),"endIndex":number.saturating_sub(1),
        "rangeLabel":format!("{sign}{number}"),"text":text.trim(),"diff":format!("{marker}{line_text}"),
        "fenceLanguage":fence_language(file_path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_comment_on_a_line_reads_as_a_review_comment_of_the_original() {
        assert_eq!(fence_language("src/lib/a.ts"), "ts");
        assert_eq!(fence_language(".gitignore"), "gitignore");
        assert_eq!(fence_language("Makefile"), "text");
        assert_eq!(fence_language("dir\\x.RS"), "rs");
        let comment = line_comment("c1", "diff:changes", "Changes", "src/a.ts", '-', 3, "const a = 1;", "  why?  ");
        let record = review_record(&comment);
        assert_eq!(record["label"], "a.ts L3 (before)");
        assert_eq!((record["rangeLabel"].as_str(), record["diff"].as_str(), record["text"].as_str(), record["startIndex"].as_u64()), (Some("-3"), Some("-const a = 1;"), Some("why?"), Some(2)));
        assert_eq!(review_label(&line_comment("c2", "s", "t", "README.md", '+', 12, "x", "y")), "README.md L12");
    }

    #[test]
    fn a_terminal_selection_is_named_by_the_lines_its_surface_reports() {
        assert_eq!(terminal_selection_lines(r#"{"start":{"x":4,"y":11},"end":{"x":0,"y":13}}"#), Some((12, 14)));
        // One row, or an end before the start, is one line.
        assert_eq!(terminal_selection_lines(r#"{"start":{"x":0,"y":0},"end":{"x":9,"y":0}}"#), Some((1, 1)));
        assert_eq!(terminal_selection_lines(r#"{"start":{"x":0,"y":5},"end":{"x":0,"y":2}}"#), Some((6, 6)));
        // A surface that reports none, or garbage, leaves the chip to count its own.
        assert_eq!(terminal_selection_lines(""), None);
        assert_eq!(terminal_selection_lines("{"), None);
    }
}
