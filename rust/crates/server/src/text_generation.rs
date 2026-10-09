//! Text generation (port of `textGeneration/` in the original): the short pieces of text a model
//! writes on the user's behalf. This module is the part that does not depend on a provider: the
//! prompts (templates generated from the original by `tools/generate_text_generation_prompts.mjs`,
//! never typed here), the shape of what a model must answer, and what is done to the answer before
//! anyone sees it (single line, length caps, fallbacks, branch-name slugs).
//!
//! Lengths in the original are JavaScript string lengths, UTF-16 units; every cap here counts the
//! same way and never cuts a character in half (the original can leave half of an astral
//! character at a cut, which no one wants in a commit subject).
use crate::source_control_bitbucket::sanitize_branch_fragment;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------------------------
// Errors and requests
// ---------------------------------------------------------------------------------------------

/// `TextGenerationError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextGenerationError {
    pub operation: &'static str,
    pub detail: String,
}

impl TextGenerationError {
    pub fn new(operation: Operation, detail: impl Into<String>) -> Self {
        Self { operation: operation.name(), detail: detail.into() }
    }

    /// `error.message` of the original.
    pub fn message(&self) -> String {
        format!("Text generation failed in {}: {}", self.operation, self.detail)
    }

    pub fn wire(&self) -> Value {
        json!({"_tag":"TextGenerationError","operation":self.operation,"detail":self.detail})
    }
}

impl std::fmt::Display for TextGenerationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for TextGenerationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    CommitMessage,
    PrContent,
    BranchName,
    ThreadTitle,
}

impl Operation {
    /// The operation's name in errors.
    pub fn name(self) -> &'static str {
        match self {
            Self::CommitMessage => "generateCommitMessage",
            Self::PrContent => "generatePrContent",
            Self::BranchName => "generateBranchName",
            Self::ThreadTitle => "generateThreadTitle",
        }
    }
}

/// What a model must answer, and so the schema it is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Commit,
    CommitWithBranch,
    ChangeRequest,
    Branch,
    ThreadTitle,
}

impl Shape {
    /// The structured output schema, as the original encodes it (`toJsonSchemaObject`).
    pub fn schema(self) -> &'static Value {
        let data = data();
        match self {
            Self::Commit => &data["commit"]["plain"]["schema"],
            Self::CommitWithBranch => &data["commit"]["branch"]["schema"],
            Self::ChangeRequest => &data["changeRequest"]["plain"]["schema"],
            Self::Branch => &data["branch"]["static"]["schema"],
            Self::ThreadTitle => &data["threadTitle"]["schema"],
        }
    }

    /// Decodes an answer as the original's output schema does: every named field present and the
    /// right type, anything else ignored, `needsRefinement` defaulting to false. The result holds
    /// only the schema's fields.
    pub fn decode(self, answer: &Value) -> Option<Value> {
        let object = answer.as_object()?;
        let text = |key: &str| object.get(key)?.as_str().map(|text| json!(text));
        Some(match self {
            Self::Commit => json!({"subject":text("subject")?,"body":text("body")?}),
            Self::CommitWithBranch => json!({"subject":text("subject")?,"body":text("body")?,"branch":text("branch")?}),
            Self::ChangeRequest => json!({"title":text("title")?,"body":text("body")?}),
            Self::Branch => json!({"branch":text("branch")?}),
            Self::ThreadTitle => {
                let refinement = match object.get("needsRefinement") {
                    None => false,
                    Some(Value::Bool(flag)) => *flag,
                    Some(_) => return None,
                };
                json!({"title":text("title")?,"needsRefinement":refinement})
            }
        })
    }
}

/// One prompt for a provider to run.
#[derive(Debug, Clone)]
pub struct Request {
    pub operation: Operation,
    pub cwd: String,
    pub prompt: String,
    pub shape: Shape,
    pub model_selection: Value,
    /// The user's attachments, already listed in the prompt. Providers that accept images send them.
    pub attachments: Vec<Value>,
}

/// Runs one request and returns the answer decoded as its shape.
pub trait Runner: Send + Sync {
    fn run<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Value, TextGenerationError>>;
}

// ---------------------------------------------------------------------------------------------
// Generated data and text helpers
// ---------------------------------------------------------------------------------------------

/// The generated prompts, schemas, presets and defaults.
pub fn data() -> &'static Value {
    static DATA: OnceLock<Value> = OnceLock::new();
    DATA.get_or_init(|| serde_json::from_str(include_str!("text_generation_prompts.json")).expect("generated text generation data"))
}

/// Fills a template's `{{slot}}` holes in one pass, so a value is never read as a slot.
pub fn fill(template: &str, slots: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + slots.iter().map(|(_, value)| value.len()).sum::<usize>());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(length) = rest[start + 2..].find("}}") else { break };
        out.push_str(&rest[..start]);
        let name = &rest[start + 2..start + 2 + length];
        out.push_str(slots.iter().find(|(slot, _)| *slot == name).map(|(_, value)| *value).unwrap_or_else(|| panic!("template slot {name} has no value")));
        rest = &rest[start + 2 + length + 2..];
    }
    out.push_str(rest);
    out
}

/// What JavaScript's `String.prototype.trim` strips.
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_space)
}

fn js_trim_end(value: &str) -> &str {
    value.trim_end_matches(is_js_space)
}

fn is_js_space(character: char) -> bool {
    match character {
        '\u{85}' => false,
        '\u{FEFF}' => true,
        other => other.is_whitespace(),
    }
}

pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// The longest start of `value` within `units` UTF-16 units.
pub fn utf16_prefix(value: &str, units: usize) -> &str {
    let mut used = 0;
    for (index, character) in value.char_indices() {
        used += character.len_utf16();
        if used > units {
            return &value[..index];
        }
    }
    value
}

/// The longest end of `value` within `units` UTF-16 units (`slice(-units)`).
pub fn utf16_suffix(value: &str, units: usize) -> &str {
    let mut used = 0;
    for (index, character) in value.char_indices().rev() {
        used += character.len_utf16();
        if used > units {
            return &value[index + character.len_utf8()..];
        }
    }
    value
}

/// `limitSection`: cut to `max_chars`, with a `[truncated]` marker when something was cut.
pub fn limit_section(value: &str, max_chars: usize) -> String {
    if utf16_len(value) <= max_chars {
        return value.to_string();
    }
    format!("{}\n\n[truncated]", utf16_prefix(value, max_chars))
}

/// The first line, split as `/\r?\n/` does.
fn first_line(value: &str) -> &str {
    match value.find('\n') {
        Some(at) => value[..at].strip_suffix('\r').unwrap_or(&value[..at]),
        None => value,
    }
}

// ---------------------------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------------------------

/// `policyInstruction`: extra instructions, trimmed and bounded, or nothing.
fn policy_text(instruction: Option<&str>) -> Option<String> {
    let trimmed = js_trim(instruction?);
    (!trimmed.is_empty()).then(|| limit_section(trimmed, 20_000))
}

pub struct CommitPrompt<'a> {
    pub branch: Option<&'a str>,
    pub staged_summary: &'a str,
    pub staged_patch: &'a str,
    pub include_branch: bool,
    /// The style policy's commit instructions.
    pub instructions: Option<&'a str>,
}

pub fn commit_message_prompt(input: &CommitPrompt) -> String {
    let policy = policy_text(input.instructions);
    let key = format!("{}{}", if input.include_branch { "branch" } else { "plain" }, if policy.is_some() { "+policy" } else { "" });
    let (summary, patch) = (limit_section(input.staged_summary, 6_000), limit_section(input.staged_patch, 40_000));
    fill(
        data()["commit"][key.as_str()]["template"].as_str().expect("commit template"),
        &[("policy", policy.as_deref().unwrap_or("")), ("branch", input.branch.unwrap_or("(detached)")), ("staged_summary", &summary), ("staged_patch", &patch)],
    )
}

pub struct ChangeRequestPrompt<'a> {
    pub base_branch: &'a str,
    pub head_branch: &'a str,
    pub commit_summary: &'a str,
    pub diff_summary: &'a str,
    pub diff_patch: &'a str,
    pub template: Option<&'a str>,
    /// The style policy's change request instructions.
    pub instructions: Option<&'a str>,
}

pub fn change_request_prompt(input: &ChangeRequestPrompt) -> String {
    let template = input.template.map(js_trim).filter(|template| !template.is_empty());
    let policy = policy_text(input.instructions);
    let key = format!("{}{}", if template.is_some() { "template" } else { "plain" }, if policy.is_some() { "+policy" } else { "" });
    let template = template.map(|template| limit_section(template, 8_000));
    let (commits, stat, patch) = (limit_section(input.commit_summary, 12_000), limit_section(input.diff_summary, 12_000), limit_section(input.diff_patch, 40_000));
    fill(
        data()["changeRequest"][key.as_str()]["template"].as_str().expect("change request template"),
        &[
            ("policy", policy.as_deref().unwrap_or("")),
            ("template", template.as_deref().unwrap_or("")),
            ("base_branch", input.base_branch),
            ("head_branch", input.head_branch),
            ("commit_summary", &commits),
            ("diff_summary", &stat),
            ("diff_patch", &patch),
        ],
    )
}

/// `BranchNamingOptions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Naming {
    /// `static`, `semantic` or `custom`.
    pub mode: String,
    pub prefix: String,
    pub instructions: String,
}

/// `- name (mime, size bytes)` for each attachment, as the prompts list them.
pub fn attachment_lines(attachments: &[Value]) -> String {
    attachments
        .iter()
        .map(|attachment| format!("- {} ({}, {} bytes)", js_text(&attachment["name"]), js_text(&attachment["mimeType"]), js_text(&attachment["sizeBytes"])))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A value as a template literal prints it.
fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

pub fn branch_name_prompt(message: &str, attachments: &[Value], naming: &Naming) -> String {
    let instructions = (naming.mode == "custom").then(|| policy_text(Some(&naming.instructions))).flatten();
    let mode = match naming.mode.as_str() {
        "custom" => "custom",
        "semantic" => "semantic",
        _ => "static",
    };
    let key = format!("{mode}{}{}", if instructions.is_some() { "+instructions" } else { "" }, if attachments.is_empty() { "" } else { "+attachments" });
    let lines = limit_section(&attachment_lines(attachments), 4_000);
    fill(
        data()["branch"][key.as_str()]["template"].as_str().expect("branch template"),
        &[("message", &limit_section(message, 8_000)), ("instructions", instructions.as_deref().unwrap_or("")), ("attachments", &lines)],
    )
}

const EARLIER_CONTENT_TRUNCATION_MARKER: &str = "[Earlier content truncated]\n\n";
const CONTENT_TRUNCATED: &str = "\n[Content truncated]\n";

/// `limitTitleMessage`: keeps the start of the request and its final constraints.
pub fn limit_title_message(text: &str, budget: usize) -> String {
    if utf16_len(text) <= budget {
        return text.to_string();
    }
    let marker = utf16_len(CONTENT_TRUNCATED);
    if budget <= marker {
        return String::new();
    }
    let available = budget - marker;
    let head = available.div_ceil(2);
    let tail = available - head;
    format!("{}{CONTENT_TRUNCATED}{}", utf16_prefix(text, head), if tail > 0 { utf16_suffix(text, tail) } else { "" })
}

fn preserve_message_end(message: &str) -> String {
    let already = message.starts_with(EARLIER_CONTENT_TRUNCATION_MARKER);
    let contents = if already { &message[EARLIER_CONTENT_TRUNCATION_MARKER.len()..] } else { message };
    if !already && utf16_len(contents) <= 8_000 {
        return contents.to_string();
    }
    format!("{EARLIER_CONTENT_TRUNCATION_MARKER}{}", utf16_suffix(contents, 8_000))
}

pub struct TitlePrompt<'a> {
    pub message: &'a str,
    /// Present when replacing an existing title from the thread's history.
    pub previous_title: Option<&'a str>,
    pub linked_context: Option<&'a str>,
    pub attachments: &'a [Value],
}

pub fn thread_title_prompt(input: &TitlePrompt) -> String {
    let title = &data()["threadTitle"];
    let text = |key: &str| title[key].as_str().expect("title template");
    let mut prompt = match input.previous_title {
        None => format!("{}{}{}", text("initialHead"), text("initialLabel"), limit_title_message(input.message, 8_000)),
        Some(previous) => format!(
            "{}{}{}",
            fill(text("regenerateHead"), &[("previous_title_json", &serde_json::to_string(previous).expect("a string encodes"))]),
            text("regenerateLabel"),
            preserve_message_end(input.message)
        ),
    };
    if let Some(linked) = input.linked_context.filter(|linked| !linked.is_empty()) {
        prompt.push_str(&fill(text("linked"), &[("linked", linked)]));
    }
    if !input.attachments.is_empty() {
        prompt.push_str(&fill(text("attachments"), &[("attachments", &limit_section(&attachment_lines(input.attachments), 4_000))]));
    }
    prompt
}

// ---------------------------------------------------------------------------------------------
// What is done to an answer
// ---------------------------------------------------------------------------------------------

/// `sanitizeCommitSubject`: imperative-length, one line, no trailing period.
pub fn sanitize_commit_subject(raw: &str) -> String {
    let line = js_trim(first_line(js_trim(raw)));
    let line = js_trim(line.trim_end_matches('.'));
    if line.is_empty() {
        return "Update project files".into();
    }
    if utf16_len(line) <= 72 {
        return line.into();
    }
    js_trim_end(utf16_prefix(line, 72)).into()
}

/// `sanitizePrTitle`: one line, with a fallback.
pub fn sanitize_pr_title(raw: &str) -> String {
    let line = js_trim(first_line(js_trim(raw)));
    if line.is_empty() { "Update project changes".into() } else { line.into() }
}

const MAX_THREAD_TITLE_CHARS: usize = 120;

/// `sanitizeThreadTitle`: one line without wrapping quotes, bounded; a title the model answered as
/// a JSON object is unwrapped first.
pub fn sanitize_thread_title(raw: &str) -> String {
    static SPACES: OnceLock<regex::Regex> = OnceLock::new();
    let spaces = SPACES.get_or_init(|| regex::Regex::new(r"[\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]+").unwrap());
    let decoded = serde_json::from_str::<Value>(raw).ok().and_then(|value| value.get("title").and_then(Value::as_str).map(str::to_string));
    let title = decoded.as_deref().unwrap_or(raw);
    let line = js_trim(first_line(js_trim(title)));
    let unquoted = js_trim(line.trim_start_matches(['\'', '"', '`']).trim_end_matches(['\'', '"', '`']));
    let normalized = spaces.replace_all(unquoted, " ");
    if js_trim(&normalized).is_empty() {
        return "New thread".into();
    }
    if utf16_len(&normalized) <= MAX_THREAD_TITLE_CHARS {
        return normalized.into_owned();
    }
    format!("{}...", js_trim_end(utf16_prefix(&normalized, MAX_THREAD_TITLE_CHARS - 3)))
}

/// `formatGeneratedBranchName`: the model's name as a ref name, with the configured prefix when
/// naming is static; custom naming keeps the model's complete ref, which git validates on rename.
pub fn format_generated_branch_name(raw: &str, naming: &Naming) -> String {
    if naming.mode == "custom" {
        return js_trim(raw).into();
    }
    let branch = sanitize_branch_fragment(raw);
    if naming.mode != "static" {
        return branch;
    }
    static UNSAFE: OnceLock<regex::Regex> = OnceLock::new();
    static DASHES: OnceLock<regex::Regex> = OnceLock::new();
    let (unsafe_run, dashes) = (UNSAFE.get_or_init(|| regex::Regex::new(r"[^a-zA-Z0-9_-]+").unwrap()), DASHES.get_or_init(|| regex::Regex::new(r"-+").unwrap()));
    let prefix = naming
        .prefix
        .split('/')
        .map(|part| {
            let part = unsafe_run.replace_all(part, "-");
            dashes.replace_all(&part, "-").trim_matches('-').to_string()
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if prefix.is_empty() { branch } else { format!("{prefix}/{branch}") }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMessage {
    pub subject: String,
    pub body: String,
    /// Only present when the model was asked for a branch name.
    pub branch: Option<String>,
}

/// An answer to the commit message prompt, finished.
pub fn finish_commit_message(answer: &Value, include_branch: bool) -> CommitMessage {
    CommitMessage {
        subject: sanitize_commit_subject(answer["subject"].as_str().unwrap_or("")),
        body: js_trim(answer["body"].as_str().unwrap_or("")).into(),
        branch: if include_branch { answer["branch"].as_str().map(crate::git_workflow_stacked::sanitize_feature_branch_name) } else { None },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequestContent {
    pub title: String,
    pub body: String,
}

pub fn finish_change_request(answer: &Value) -> ChangeRequestContent {
    ChangeRequestContent { title: sanitize_pr_title(answer["title"].as_str().unwrap_or("")), body: js_trim(answer["body"].as_str().unwrap_or("")).into() }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadTitle {
    pub title: String,
    pub needs_refinement: bool,
}

pub fn finish_thread_title(answer: &Value) -> ThreadTitle {
    ThreadTitle { title: sanitize_thread_title(answer["title"].as_str().unwrap_or("")), needs_refinement: answer["needsRefinement"] == true }
}
