//! A message with its context records, in the shape a server that does not advertise `inlineMessageContext` understands.
//! Port of `packages/shared/src/composerContextLegacySend.ts`: such a server drops the records and forwards the inline
//! reference links as literal text, so the payload has to travel inside the message, in the format the original's
//! `upgradeLegacyContextMessage` parses back.
use regex::Regex;
use serde_json::Value;
use std::{collections::HashSet, sync::OnceLock};

fn link() -> &'static Regex {
    static LINK: OnceLock<Regex> = OnceLock::new();
    LINK.get_or_init(|| Regex::new(r"(!?)\[([^\]\n]{0,512})\]\((t3-context://v1/[^\s)]{1,200})\)").expect("a valid pattern"))
}

/// `(context id, start, end)` of each inline reference whose href parses (`parseComposerContextHref`).
fn references(text: &str) -> Vec<(String, usize, usize)> {
    if !text.contains("](t3-context:") {
        return vec![];
    }
    link()
        .captures_iter(text)
        .filter_map(|captures| {
            let whole = captures.get(0)?;
            let rest = captures.get(3)?.as_str().strip_prefix("t3-context://v1/")?;
            let (kind, id) = rest.split_once('/')?;
            let kind_ok = kind.len() <= 40 && kind.starts_with(|c: char| c.is_ascii_lowercase()) && kind.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            let id_ok = (1..=128).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            (kind_ok && id_ok).then(|| (id.to_owned(), whole.start(), whole.end()))
        })
        .collect()
}

fn text_of<'a>(record: &'a Value, key: &str) -> &'a str {
    record[key].as_str().unwrap_or_default()
}

fn inline_label(record: &Value) -> String {
    if record["kind"] == "terminal" && record.get("terminalLabel").is_some() {
        let slug = text_of(record, "terminalLabel").trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join("-");
        let (start, end) = (record["lineStart"].as_u64().unwrap_or(0), record["lineEnd"].as_u64().unwrap_or(0));
        let range = if start == end { start.to_string() } else { format!("{start}-{end}") };
        return format!("@{slug}:{range}");
    }
    text_of(record, "label").to_owned()
}

fn render_block(tag: &str, entries: Vec<String>) -> String {
    let body = entries.into_iter().filter(|entry| !entry.is_empty()).collect::<Vec<_>>().join("\n");
    if body.is_empty() { String::new() } else { format!("<{tag}>\n{body}\n</{tag}>") }
}

fn indent(value: &str) -> String {
    value.split('\n').map(|line| if line.is_empty() { String::new() } else { format!("  {line}") }).collect::<Vec<_>>().join("\n")
}

fn terminal_entry(record: &Value) -> String {
    if record.get("terminalLabel").is_none() {
        return String::new();
    }
    let (start, end) = (record["lineStart"].as_u64().unwrap_or(0), record["lineEnd"].as_u64().unwrap_or(0));
    let range = if start == end { format!("line {start}") } else { format!("lines {start}-{end}") };
    // A trailing newline would otherwise number a line past the range the label declares.
    let body = text_of(record, "text").split('\n').take((end + 1).saturating_sub(start) as usize).enumerate().map(|(index, line)| format!("{} | {line}", start + index as u64)).collect::<Vec<_>>().join("\n");
    format!("- {} {range}:\n{}", text_of(record, "terminalLabel"), indent(&body))
}

fn element_entry(record: &Value) -> String {
    if record.get("tagName").is_none() {
        return String::new();
    }
    element_details_entry(record)
}

fn element_details_entry(record: &Value) -> String {
    let mut lines = vec![];
    let page_url = text_of(record, "pageUrl");
    if !page_url.is_empty() {
        lines.push(format!("url: {page_url}"));
    }
    let selector = text_of(record, "selector");
    if !selector.is_empty() {
        lines.push(format!("selector: {selector}"));
    }
    let source = &record["source"];
    if let Some(file) = source["fileName"].as_str().filter(|file| !file.is_empty()) {
        let mut parts = vec![file.to_owned()];
        parts.extend(["lineNumber", "columnNumber"].iter().filter_map(|key| source[*key].as_u64().map(|number| number.to_string())));
        lines.push(format!("source: {}", parts.join(":")));
    }
    for (key, label) in [("htmlPreview", "html"), ("styles", "styles")] {
        let value = text_of(record, key);
        if !value.is_empty() {
            lines.push(format!("{label}:\n{}", indent(value)));
        }
    }
    let name = record["componentName"].as_str().filter(|name| !name.is_empty()).unwrap_or_else(|| text_of(record, "tagName"));
    format!("- <{name}>:\n{}", indent(&lines.join("\n")))
}

fn preview_body(record: &Value) -> String {
    if record.get("annotationId").is_none() {
        return String::new();
    }
    let page = record["pageUrl"].as_str().filter(|url| !url.is_empty()).or_else(|| record["pageTitle"].as_str()).unwrap_or_default();
    let mut lines = vec![format!("Id: {}", text_of(record, "annotationId")), format!("Page: {page}")];
    for (key, label) in [("comment", "Comment"), ("targetSummary", "Targets")] {
        let value = text_of(record, key);
        if !value.is_empty() {
            lines.push(format!("{label}: {value}"));
        }
    }
    if let Some(changes) = record["styleChanges"].as_array().filter(|changes| !changes.is_empty()) {
        lines.push("Requested visual changes:".into());
        lines.extend(changes.iter().filter_map(Value::as_str).map(|change| format!("- {change}")));
    }
    if let Some(elements) = record["elements"].as_array().filter(|elements| !elements.is_empty()) {
        lines.push("<element_context>".into());
        lines.push(elements.iter().map(element_details_entry).collect::<Vec<_>>().join("\n"));
        lines.push("</element_context>".into());
    }
    lines.join("\n")
}

fn escape_attribute(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn review_comment(record: &Value) -> String {
    if record.get("sectionId").is_none() {
        return text_of(record, "label").to_owned();
    }
    let attributes = [
        format!("sectionId=\"{}\"", escape_attribute(text_of(record, "sectionId"))),
        format!("sectionTitle=\"{}\"", escape_attribute(text_of(record, "sectionTitle"))),
        format!("filePath=\"{}\"", escape_attribute(text_of(record, "filePath"))),
        format!("rangeLabel=\"{}\"", escape_attribute(text_of(record, "rangeLabel"))),
        format!("startIndex=\"{}\"", record["startIndex"]),
        format!("endIndex=\"{}\"", record["endIndex"]),
    ]
    .join(" ");
    let diff = text_of(record, "diff");
    // The fence must be longer than any run of backticks inside the diff so the body stays intact.
    let (mut longest, mut run) = (0, 0);
    for character in diff.chars() {
        run = if character == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(3.max(longest + 1));
    let body = if diff.is_empty() {
        text_of(record, "text").to_owned()
    } else {
        format!("{}\n\n{fence}{}\n{diff}\n{fence}", text_of(record, "text"), record["fenceLanguage"].as_str().unwrap_or("diff"))
    };
    format!("<review_comment {attributes}>\n{body}\n</review_comment>")
}

/// `serializeLegacyContextMessage`.
pub fn serialize(text: &str, records: &[Value]) -> String {
    let by_id: std::collections::HashMap<&str, &Value> = records.iter().filter_map(|record| Some((record["contextId"].as_str()?, record))).collect();
    let mut used = HashSet::new();
    // Inline references become the plain label the old composer wrote; the payload follows in a trailing block, which
    // is where the legacy format carried it.
    let mut text = text.to_owned();
    for (id, start, end) in references(&text.clone()).into_iter().rev() {
        let Some(record) = by_id.get(id.as_str()) else { continue };
        used.insert(id);
        let replacement = if record["kind"] == "review-comment" { review_comment(record) } else { inline_label(record) };
        text.replace_range(start..end, &replacement);
    }
    let of_kind = |kind: &'static str| records.iter().filter(move |record| record["kind"] == kind);
    let mut blocks = vec![
        render_block("terminal_context", of_kind("terminal").map(terminal_entry).collect()),
        render_block("element_context", of_kind("element").map(element_entry).collect()),
    ];
    blocks.extend(of_kind("preview-annotation").map(|record| format!("<preview_annotation>\n{}\n</preview_annotation>", preview_body(record))));
    // Review comments already inlined their payload above; anything else unreferenced still ships its block so no
    // context is silently dropped.
    blocks.extend(of_kind("review-comment").filter(|record| !record["contextId"].as_str().is_some_and(|id| used.contains(id))).map(review_comment));
    let mut parts = vec![text.trim_end().to_owned()];
    parts.extend(blocks.into_iter().filter(|block| !block.is_empty()));
    parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn references_become_their_plain_label_and_terminal_selections_follow_as_a_block() {
        let terminal = crate::composer_context::terminal_record("a1", "t1", "Terminal 1", 3, 4, "first\nsecond").expect("a record");
        let id = terminal["contextId"].as_str().unwrap().to_owned();
        let (text, context) = crate::composer_context::compose("look at this", std::slice::from_ref(&terminal));
        assert!(context.is_some() && text.contains("t3-context://v1/terminal/"));
        let legacy = serialize(&text, &[terminal]);
        assert!(!legacy.contains("t3-context://"), "{legacy}");
        assert!(legacy.starts_with("look at this"), "{legacy}");
        assert!(legacy.contains("@terminal-1:3-4"), "{legacy}");
        assert!(legacy.contains("<terminal_context>\n- Terminal 1 lines 3-4:\n  3 | first\n  4 | second\n</terminal_context>"), "{legacy}");
        assert!(!id.is_empty());
    }

    #[test]
    fn a_review_comment_travels_where_it_was_referenced_and_once() {
        let record = json!({"kind":"review-comment","contextId":"c1","label":"a.rs:3","sectionId":"s","sectionTitle":"Changes","filePath":"a.rs","rangeLabel":"line 3","startIndex":0,"endIndex":0,"text":"why?","diff":"+x ``` y","fenceLanguage":"diff"});
        let inline = serialize("see [a.rs:3](t3-context://v1/review-comment/c1) please", std::slice::from_ref(&record));
        assert_eq!(inline.matches("<review_comment").count(), 1, "{inline}");
        assert!(inline.starts_with("see <review_comment sectionId=\"s\""), "{inline}");
        assert!(inline.contains("\n````diff\n+x ``` y\n````\n"), "the fence outgrows the diff's own: {inline}");
        let unreferenced = serialize("see this", &[record]);
        assert!(unreferenced.starts_with("see this\n\n<review_comment"), "{unreferenced}");
    }

    #[test]
    fn text_without_records_is_only_trimmed() {
        assert_eq!(serialize("hello  \n", &[]), "hello");
        assert_eq!(serialize("a [x](t3-context://v1/thread/missing) b", &[]), "a [x](t3-context://v1/thread/missing) b");
    }
}
