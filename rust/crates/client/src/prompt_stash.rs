//! The prompt stash: a prompt put aside with what travels with it (its attachments and context
//! chips) and brought back into whatever composer is open. Port of `promptStashStore.ts` and the
//! stash half of `ChatComposer.tsx`. The stash is the browser's own, not a thread's or a
//! provider's: an entry is kept across threads and reloads, newest first, at most twenty, and
//! restoring one adds to the open draft without touching its model or mode.
//!
//! Attachments here are uploads the server holds (the draft keeps references, not bytes), so an
//! entry keeps the references; the server sweeps an upload after 24 hours and a restore verifies
//! each one first, as a draft reopened after a reload does.
use crate::{attachments::{Kind, Persisted}, composer_context};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where the stash is kept (the original's is `t3code:prompt-stash:v2`, with images as data
/// URLs; this one holds upload references, so it does not read or write that one).
pub const STORAGE_KEY: &str = "t3code:rust-prompt-stash:v1";
pub const MAX_ENTRIES: usize = 20;
const VERSION: u64 = 1;
const SNIPPET_MAX_CHARS: usize = 90;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub created_at: String,
    /// The environment that holds the attachments' uploads.
    pub environment: String,
    pub prompt: String,
    #[serde(default)]
    pub attachments: Vec<Persisted>,
    /// The payloads behind the prompt's context links.
    #[serde(default)]
    pub records: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stash {
    /// Newest first.
    pub entries: Vec<Entry>,
}
impl Stash {
    /// What storage held: an unreadable stash is an empty one, as in the original.
    pub fn parse(raw: Option<&str>) -> Self {
        let entries = raw
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| value.get("entries").cloned())
            .and_then(|entries| entries.as_array().cloned())
            .map(|entries| entries.into_iter().filter_map(|entry| serde_json::from_value::<Entry>(entry).ok()).collect())
            .unwrap_or_default();
        Self { entries }
    }
    pub fn to_json(&self) -> String {
        serde_json::json!({"version":VERSION,"entries":self.entries}).to_string()
    }
    /// The stash with `entry` first, and the oldest entry it pushed out past the cap.
    pub fn with(&self, entry: Entry) -> (Stash, Option<Entry>) {
        let mut entries = self.entries.clone();
        entries.insert(0, entry);
        let evicted = if entries.len() > MAX_ENTRIES { entries.pop() } else { None };
        (Stash { entries }, evicted)
    }
    /// The stash without entry `id`, and that entry.
    pub fn without(&self, id: &str) -> (Stash, Option<Entry>) {
        let mut entries = self.entries.clone();
        let taken = entries.iter().position(|entry| entry.id == id).map(|at| entries.remove(at));
        (Stash { entries }, taken)
    }
}

fn js_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}
/// The words a menu row shows for an entry: the prompt on one line, else what it holds.
pub fn snippet(entry: &Entry) -> String {
    let mut line = String::new();
    let mut in_space = false;
    for c in entry.prompt.trim_matches(js_space).chars() {
        if js_space(c) {
            if !in_space {
                line.push(' ');
            }
            in_space = true;
        } else {
            line.push(c);
            in_space = false;
        }
    }
    if !line.is_empty() {
        // Lengths are the original's, in UTF-16 units; a character that would be cut in half is left out.
        if line.encode_utf16().count() <= SNIPPET_MAX_CHARS {
            return line;
        }
        let mut units = 0;
        let kept: String = line.chars().take_while(|c| { units += c.len_utf16(); units <= SNIPPET_MAX_CHARS }).collect();
        return format!("{kept}\u{2026}");
    }
    let images = entry.attachments.iter().filter(|attachment| attachment.r#type == Kind::Image).count();
    let files = entry.attachments.len() - images;
    let count = images + files;
    if count == 0 {
        return "(empty)".into();
    }
    let label = if images > 0 && files > 0 { "attachment" } else if files > 0 { "file" } else { "image" };
    format!("({count} {label}{})", if count == 1 { "" } else { "s" })
}

/// A restored prompt added to what is already written: after it, a blank line apart; an
/// attachment-only entry leaves the text alone.
pub fn merge_prompt(current: &str, restored: &str) -> String {
    if restored.is_empty() {
        current.to_owned()
    } else if current.trim_matches(js_space).is_empty() {
        restored.to_owned()
    } else {
        format!("{}\n\n{restored}", current.trim_end_matches(js_space))
    }
}

fn payload(record: &Value) -> Value {
    let mut copy = record.clone();
    if let Some(object) = copy.as_object_mut() {
        object.remove("contextId");
    }
    copy
}
/// The chips an entry brings into a draft that already holds `held`, and its prompt with their links
/// following. A chip the draft already holds, payload and all, is not added twice; one that shares
/// its id but not its payload takes a fresh id from `fresh`; a thread of another environment is
/// left behind, since the agent can only read threads of its own server.
pub fn bring_in(prompt: &str, records: &[Value], held: &[Value], environment: &str, mut fresh: impl FnMut() -> String) -> (String, Vec<Value>) {
    let mut text = prompt.to_owned();
    let mut added = Vec::new();
    for record in records {
        let (Some(kind), Some(id)) = (record["kind"].as_str(), record["contextId"].as_str()) else { continue };
        if kind == "thread" && record["environmentId"].as_str().is_some_and(|source| source != environment) {
            continue;
        }
        match held.iter().find(|existing| existing["contextId"] == record["contextId"]) {
            Some(existing) if payload(existing) == payload(record) => continue,
            Some(_) => {
                let new_id = composer_context::scoped_id(kind, &fresh());
                text = text.replace(&composer_context::href(kind, id), &composer_context::href(kind, &new_id));
                let mut renamed = record.clone();
                renamed["contextId"] = Value::String(new_id);
                added.push(renamed);
            }
            None => added.push(record.clone()),
        }
    }
    (text, added)
}

/// What stands in the way of stashing, in the original's words.
pub fn blocker(uploading: bool, needs_reattach: bool, failed: bool) -> Option<&'static str> {
    if needs_reattach {
        Some("Attach dropped files again or remove them before stashing")
    } else if uploading || failed {
        Some("Wait for file uploads before stashing this prompt")
    } else {
        None
    }
}

/// `formatRelativeTimeLabel`: "just now", "5m ago", "3h ago", "2d ago".
pub fn relative_label(created_at: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(created_at) else { return String::new() };
    let seconds = (now - at.with_timezone(&chrono::Utc)).num_seconds();
    if seconds < 60 {
        return "just now".into();
    }
    let (minutes, hours) = (seconds / 60, seconds / 3600);
    if minutes < 60 {
        format!("{minutes}m ago")
    } else if hours < 24 {
        format!("{hours}h ago")
    } else {
        format!("{}d ago", hours / 24)
    }
}
