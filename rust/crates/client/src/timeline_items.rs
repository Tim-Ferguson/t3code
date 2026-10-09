//! The words and rules the conversation timeline shows for turn items, ported from the original
//! (`session-logic.ts`, `proposedPlan.ts`, `lib/contextWindow.ts`, the context meter and the
//! work-log presentation). Replayed against the original's answers in `tests/timeline_parity.rs`
//! (`rust/tools/generate_client_timeline_fixtures.mjs`).
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::sync::OnceLock;

/// ECMAScript `toFixed`: halfway values round away from zero where Rust's formatter rounds to even.
pub fn to_fixed(value: f64, digits: usize) -> String {
    let scaled = value * 10f64.powi(digits as i32);
    if scaled.fract().abs() == 0.5 {
        let rounded = (scaled.abs() + 0.5).floor().copysign(scaled) / 10f64.powi(digits as i32);
        return format!("{rounded:.digits$}");
    }
    let text = format!("{value:.digits$}");
    // `-0.00` prints as `0.00` in ECMAScript only for exact zero; a negative that rounds to zero keeps its sign.
    text
}
fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}
fn strip_zero_fraction(text: String) -> String {
    match text.split_once('.') {
        Some((whole, fraction)) if fraction.bytes().all(|byte| byte == b'0') => whole.to_owned(),
        _ => text,
    }
}
fn group(whole: i64) -> String {
    let digits = whole.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    if whole < 0 { format!("-{out}") } else { out }
}

/// `formatTokens` of `packages/shared/src/usageFormat.ts`: three significant figures and a unit.
pub fn format_tokens(value: f64) -> String {
    fn trim(value: f64) -> String {
        let abs = value.abs();
        let digits = if abs >= 100. { 0 } else if abs >= 10. { 1 } else { 2 };
        strip_zero_fraction(to_fixed(value, digits))
    }
    let abs = value.abs();
    if abs >= 1e12 {
        format!("{}T", trim(value / 1e12))
    } else if abs >= 1e9 {
        format!("{}B", trim(value / 1e9))
    } else if abs >= 1e6 {
        format!("{}M", trim(value / 1e6))
    } else if abs >= 1e3 {
        format!("{}K", trim(value / 1e3))
    } else {
        group(js_round(value) as i64)
    }
}

/// `formatContextWindowTokens` of `lib/contextWindow.ts`: the meter's compact count.
pub fn format_context_tokens(value: Option<f64>) -> String {
    let Some(value) = value.filter(|value| value.is_finite()) else {
        return "0".into();
    };
    if value < 1_000. {
        format!("{}", js_round(value))
    } else if value < 10_000. {
        format!("{}k", strip_zero_fraction(to_fixed(value / 1_000., 1)))
    } else if value < 1_000_000. {
        format!("{}k", js_round(value / 1_000.))
    } else {
        format!("{}m", strip_zero_fraction(to_fixed(value / 1_000_000., 1)))
    }
}

/// The meter's percentage, or `None` when there is no finite value.
pub fn format_percentage(value: Option<f64>) -> Option<String> {
    let value = value.filter(|value| value.is_finite())?;
    Some(if value < 10. {
        format!("{}%", strip_zero_fraction(to_fixed(value, 1)))
    } else {
        format!("{}%", js_round(value))
    })
}

pub fn format_context_cost(amount: f64, currency: &str) -> String {
    let digits = if amount.abs() > 0. && amount.abs() < 0.01 { 4 } else { 2 };
    format!("{currency} {}", to_fixed(amount, digits))
}

pub fn format_compaction_message(model: Option<&str>, threshold: Option<f64>) -> String {
    if let Some(threshold) = threshold.filter(|threshold| *threshold > 0.) {
        return format!("Compacts automatically at {} tokens.", group(threshold.round() as i64));
    }
    match model {
        Some(model) if !model.is_empty() => format!("Context for {model} compacts automatically when needed."),
        _ => "Context compacts automatically when needed.".into(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
}
/// What the context meter shows (`ContextWindowSnapshot`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextWindow {
    pub used_tokens: f64,
    pub total_processed_tokens: Option<f64>,
    pub max_tokens: Option<f64>,
    pub remaining_tokens: Option<f64>,
    pub used_percentage: Option<f64>,
    pub remaining_percentage: Option<f64>,
    pub input_tokens: Option<f64>,
    pub cached_input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    pub reasoning_output_tokens: Option<f64>,
    pub last_used_tokens: Option<f64>,
    pub last_input_tokens: Option<f64>,
    pub last_cached_input_tokens: Option<f64>,
    pub last_output_tokens: Option<f64>,
    pub last_reasoning_output_tokens: Option<f64>,
    pub tool_uses: Option<f64>,
    pub duration_ms: Option<f64>,
    pub compacts_automatically: Option<bool>,
    pub auto_compact_threshold: Option<f64>,
    pub cost: Option<Cost>,
    pub updated_at: String,
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|value| value.is_finite())
}
fn percentages(used: f64, max: Option<f64>) -> (Option<f64>, Option<f64>, Option<f64>) {
    let percentage = max.filter(|max| *max > 0.).map(|max| (used / max * 100.).min(100.));
    (
        max.map(|max| (max - used).max(0.).round()),
        percentage,
        percentage.map(|percentage| (100. - percentage).max(0.)),
    )
}

/// `deriveLatestContextWindowSnapshot`: the provider's live report wins, then the provider
/// thread's, then the last compaction. `items` are the visible turn items, oldest first.
pub fn derive_context_window(items: &[&Value], live: Option<&Value>, provider_thread: Option<&Value>) -> Option<ContextWindow> {
    let blank = |used_tokens: f64, max_tokens: Option<f64>, updated_at: String| {
        let (remaining_tokens, used_percentage, remaining_percentage) = percentages(used_tokens, max_tokens);
        ContextWindow {
            used_tokens,
            total_processed_tokens: None,
            max_tokens,
            remaining_tokens,
            used_percentage,
            remaining_percentage,
            input_tokens: None,
            cached_input_tokens: None,
            output_tokens: None,
            reasoning_output_tokens: None,
            last_used_tokens: None,
            last_input_tokens: None,
            last_cached_input_tokens: None,
            last_output_tokens: None,
            last_reasoning_output_tokens: None,
            tool_uses: None,
            duration_ms: None,
            compacts_automatically: Some(true),
            auto_compact_threshold: None,
            cost: None,
            updated_at,
        }
    };
    if let Some(live) = live.filter(|live| live.is_object()) {
        let used = live["usedTokens"].as_f64().unwrap_or(0.).max(0.);
        let mut window = blank(used, live["maxTokens"].as_f64(), live["updatedAt"].as_str().unwrap_or_default().to_owned());
        window.input_tokens = number(&live["inputTokens"]);
        window.cached_input_tokens = number(&live["cachedInputTokens"]);
        window.output_tokens = number(&live["outputTokens"]);
        window.reasoning_output_tokens = number(&live["reasoningOutputTokens"]);
        return Some(window);
    }
    if let Some((usage, updated_at)) = provider_thread.and_then(|thread| Some((thread.get("contextUsage").filter(|usage| usage.is_object())?, thread["updatedAt"].as_str()?))) {
        let used = usage["usedTokens"].as_f64().unwrap_or(0.);
        let mut window = blank(used, number(&usage["maxTokens"]), updated_at.to_owned());
        window.total_processed_tokens = number(&usage["totalProcessedTokens"]);
        window.input_tokens = number(&usage["inputTokens"]);
        window.cached_input_tokens = number(&usage["cachedInputTokens"]);
        window.output_tokens = number(&usage["outputTokens"]);
        window.reasoning_output_tokens = number(&usage["reasoningOutputTokens"]);
        window.last_used_tokens = number(&usage["lastUsedTokens"]);
        window.last_input_tokens = number(&usage["lastInputTokens"]);
        window.last_cached_input_tokens = number(&usage["lastCachedInputTokens"]);
        window.last_output_tokens = number(&usage["lastOutputTokens"]);
        window.last_reasoning_output_tokens = number(&usage["lastReasoningOutputTokens"]);
        window.tool_uses = number(&usage["toolUses"]);
        window.duration_ms = number(&usage["durationMs"]);
        window.compacts_automatically = usage["compactsAutomatically"].as_bool();
        window.auto_compact_threshold = usage["autoCompactThreshold"].as_f64();
        window.cost = usage.get("cost").filter(|cost| cost.is_object()).and_then(|cost| Some(Cost { amount: cost["amount"].as_f64()?, currency: cost["currency"].as_str()?.to_owned() }));
        return Some(window);
    }
    items.iter().rev().filter(|item| item["type"] == "compaction").find_map(|item| {
        let used = number(&item["afterTokenCount"]).filter(|used| *used >= 0.)?;
        let updated = item["startedAt"].as_str().or_else(|| item["updatedAt"].as_str()).unwrap_or_default();
        let mut window = blank(used, None, updated.to_owned());
        window.total_processed_tokens = number(&item["beforeTokenCount"]);
        Some(window)
    })
}

/// The newest provider-reported usage: the latest provider turn that carries any (`tokenUsage`).
pub fn latest_live_usage(projection: &Value) -> Option<&Value> {
    projection["providerTurns"].as_array()?.iter().rev().find_map(|turn| turn.get("tokenUsage").filter(|usage| usage.is_object()))
}

/// The context window of a thread projection (`activeContextWindow` of the original's chat view).
pub fn projection_context_window(projection: &Value) -> Option<ContextWindow> {
    let items: Vec<&Value> = projection["visibleTurnItems"].as_array().map(|rows| rows.iter().map(|row| &row["item"]).collect()).unwrap_or_default();
    let active = projection["thread"]["activeProviderThreadId"].as_str();
    let thread = projection["providerThreads"].as_array().and_then(|threads| threads.iter().find(|thread| thread["id"].as_str() == active && active.is_some()));
    derive_context_window(&items, latest_live_usage(projection), thread)
}

/// `contextCompactionLabel`: the divider's words.
pub fn compaction_label(item: &Value) -> String {
    if item["status"] == "running" {
        return "Compacting context".into();
    }
    match (number(&item["beforeTokenCount"]), number(&item["afterTokenCount"])) {
        (Some(before), Some(after)) => format!("Context compacted {} → {} tokens", format_tokens(before), format_tokens(after)),
        _ => "Context compacted".into(),
    }
}

/// `providerErrorPresentation`: the heading and the message of an error item.
pub fn error_presentation(item: &Value) -> (String, String) {
    let message = item["failure"]["message"].as_str().unwrap_or_default();
    let usage_limit = item["failure"]["class"] == "usage_limit";
    let retry = item.get("retry").filter(|retry| retry.is_object());
    let Some(retry) = retry else {
        let title = item["title"].as_str().map(str::trim).filter(|title| !title.is_empty());
        let label = if usage_limit { "Usage limit reached" } else { title.unwrap_or("Provider error") };
        return (label.to_owned(), message.to_owned());
    };
    let attempt = retry["attempt"].as_u64().unwrap_or(0);
    let progress = match retry["maxAttempts"].as_u64() {
        Some(max) => format!("{attempt}/{max}"),
        None => attempt.to_string(),
    };
    let status = item["status"].as_str().unwrap_or_default();
    let label = match status {
        "running" => format!("Retrying provider ({progress})"),
        "completed" => format!("Provider recovered ({progress} retries)"),
        "failed" => format!("{} after {progress} retries", if usage_limit { "Usage limit reached" } else { "Provider error" }),
        _ => format!("Provider retry stopped ({progress})"),
    };
    let delay = retry["retryDelayMs"].as_f64().filter(|delay| status == "running" && *delay > 0.);
    let delay = match delay {
        Some(ms) if ms < 1_000. => format!(" Retrying in {}ms.", ms as u64),
        Some(ms) => format!(" Retrying in {}s.", strip_zero_fraction(to_fixed(ms / 1_000., 1))),
        None => String::new(),
    };
    (label, format!("{message}{delay}"))
}

/// How an error item is toned in the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warning,
    Danger,
}
pub fn error_tone(item: &Value) -> Tone {
    if item["status"] == "completed" {
        Tone::Success
    } else if item["status"] == "running" || item["failure"]["class"] == "usage_limit" {
        Tone::Warning
    } else {
        Tone::Danger
    }
}

fn pattern(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("timeline pattern"))
}

const HEADING: &str = r"^\s{0,3}#{1,6}\s+";
/// `proposedPlanTitle`: the plan's first heading.
pub fn plan_title(markdown: &str) -> Option<String> {
    static TITLE: OnceLock<Regex> = OnceLock::new();
    let heading = pattern(&TITLE, r"(?m)^\s{0,3}#{1,6}\s+(.+)$").captures(markdown)?.get(1)?.as_str().trim();
    (!heading.is_empty()).then(|| heading.to_owned())
}

fn lines_of(text: &str) -> Vec<&str> {
    text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect()
}
/// `stripDisplayedPlanMarkdown`: the plan without the heading the card already shows.
pub fn plan_body(markdown: &str) -> String {
    static LEADING: OnceLock<Regex> = OnceLock::new();
    static FIRST: OnceLock<Regex> = OnceLock::new();
    let leading = pattern(&LEADING, HEADING);
    let first = pattern(&FIRST, r"^\s{0,3}#{1,6}\s+(.+)$");
    let lines = lines_of(markdown.trim_end());
    let mut source: Vec<&str> = if !lines[0].is_empty() && leading.is_match(lines[0]) { lines[1..].to_vec() } else { lines };
    while source.first().is_some_and(|line| line.trim().is_empty()) {
        source.remove(0);
    }
    if source.first().and_then(|line| first.captures(line)).is_some_and(|captures| captures[1].trim().to_lowercase() == "summary") {
        source.remove(0);
        while source.first().is_some_and(|line| line.trim().is_empty()) {
            source.remove(0);
        }
    }
    source.join("\n")
}

/// `buildCollapsedProposedPlanPreviewMarkdown`.
pub fn plan_preview(markdown: &str, max_lines: usize) -> String {
    let body = plan_body(markdown);
    let mut preview: Vec<String> = Vec::new();
    let (mut visible, mut more) = (0, false);
    for line in lines_of(body.trim_end()) {
        let line = line.trim_end();
        let is_visible = !line.trim().is_empty();
        if is_visible && visible >= max_lines {
            more = true;
            break;
        }
        preview.push(line.to_owned());
        if is_visible {
            visible += 1;
        }
    }
    while preview.last().is_some_and(|line| line.trim().is_empty()) {
        preview.pop();
    }
    if preview.is_empty() {
        return plan_title(markdown).unwrap_or_else(|| "Plan preview unavailable.".into());
    }
    if more {
        preview.push(String::new());
        preview.push("...".into());
    }
    preview.join("\n")
}

/// Whether the card offers to collapse: long by characters or lines.
pub fn plan_can_collapse(markdown: &str) -> bool {
    markdown.chars().map(char::len_utf16).sum::<usize>() > 900 || markdown.split('\n').count() > 20
}

pub fn plan_filename(markdown: &str) -> String {
    let title = plan_title(markdown).unwrap_or_else(|| "plan".into());
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in title.to_lowercase().chars().filter(|c| !"`'\".,!?()[]{}".contains(*c)) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c);
        } else {
            pending_dash = true;
        }
    }
    format!("{}.md", if slug.is_empty() { "plan" } else { &slug })
}
pub fn plan_export(markdown: &str) -> String {
    format!("{}\n", markdown.trim_end())
}
/// The message the app sends when the user approves a plan.
pub const PLAN_IMPLEMENTATION_PROMPT_PREFIX: &str = "PLEASE IMPLEMENT THIS PLAN:\n";
pub fn plan_implementation_prompt(markdown: &str) -> String {
    format!("{PLAN_IMPLEMENTATION_PROMPT_PREFIX}{}", markdown.trim())
}
pub fn plan_thread_title(markdown: &str) -> String {
    plan_title(markdown).map_or_else(|| "Implement plan".into(), |title| format!("Implement {title}"))
}
/// `resolvePlanFollowUpSubmission`: feedback refines the plan; an empty draft implements it.
pub fn plan_follow_up(draft: &str, markdown: &str) -> (String, &'static str) {
    let draft = draft.trim();
    if draft.is_empty() { (plan_implementation_prompt(markdown), "default") } else { (draft.to_owned(), "plan") }
}

/// The plan the follow-up banner and the Implement action are about: the newest proposed plan of
/// the latest run (`findLatestProposedPlan`), with its status from the projection's `plans`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProposedPlan {
    pub id: String,
    pub thread_id: String,
    pub run_id: Option<String>,
    pub markdown: String,
    pub status: String,
}
pub fn latest_proposed_plan(projection: &Value, latest_run: Option<&str>) -> Option<ProposedPlan> {
    let plans: Vec<&Value> = projection["plans"].as_array()?.iter().filter(|plan| plan["kind"] == "proposed_plan").collect();
    let in_run: Vec<&Value> = plans.iter().copied().filter(|plan| latest_run.is_some() && plan["runId"].as_str() == latest_run).collect();
    let candidates = if in_run.is_empty() { plans } else { in_run };
    let time = |plan: &Value| {
        projection["turnItems"]
            .as_array()
            .and_then(|items| items.iter().rev().find(|item| matches!(item["type"].as_str(), Some("proposed_plan" | "todo_list")) && item["planId"] == plan["id"]))
            .and_then(|item| item["updatedAt"].as_str())
            .or_else(|| projection["updatedAt"].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let plan = candidates.into_iter().max_by(|left, right| {
        time(left).cmp(&time(right)).then_with(|| left["id"].as_str().unwrap_or_default().cmp(right["id"].as_str().unwrap_or_default()))
    })?;
    Some(ProposedPlan {
        id: plan["id"].as_str()?.to_owned(),
        thread_id: plan["threadId"].as_str().or_else(|| projection["thread"]["id"].as_str())?.to_owned(),
        run_id: plan["runId"].as_str().map(str::to_owned),
        markdown: plan["markdown"].as_str().unwrap_or_default().to_owned(),
        status: plan["status"].as_str().unwrap_or_default().to_owned(),
    })
}
/// `shouldShowPlanFollowUpPrompt`.
pub fn show_plan_follow_up(pending_user_inputs: usize, interaction_mode: &str, latest_run_settled: bool, plan: Option<&ProposedPlan>, has_attachments: bool) -> bool {
    pending_user_inputs == 0 && interaction_mode == "plan" && latest_run_settled && plan.is_some_and(|plan| plan.status == "active") && !has_attachments
}

/// `liveThoughtLine`: the one plain line a thought shows while it is collapsed.
pub fn live_thought_line(markdown: &str) -> String {
    static HEADING_LINE: OnceLock<Regex> = OnceLock::new();
    static LINKS: OnceLock<Regex> = OnceLock::new();
    static BULLETS: OnceLock<Regex> = OnceLock::new();
    static MARKS: OnceLock<Regex> = OnceLock::new();
    static SPACE: OnceLock<Regex> = OnceLock::new();
    let heading = pattern(&HEADING_LINE, r"^\s*\*\*([^*\r\n]+)\*\*[ \t]*\r?(?:\n|\z)").captures(markdown).map(|captures| captures[1].to_owned());
    let source = heading.as_deref().unwrap_or(markdown);
    let text = pattern(&LINKS, r"!?\[([^\]]*)\]\([^)]*\)").replace_all(source, "$1");
    let text = pattern(&BULLETS, r"(?m)^[ \t]*(?:#{1,6}|[-*+]|[0-9]+\.)[ \t]+").replace_all(&text, "");
    let text = pattern(&MARKS, r"`+|\*\*|~~").replace_all(&text, "");
    let text = unwrap_emphasis(&text);
    let text = pattern(&SPACE, r"\s+").replace_all(&text, " ").trim().to_owned();
    if heading.is_some() {
        return text;
    }
    // Cut after the first . ? or ! (plus a closing quote or paren) that whitespace follows.
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (at, (index, c)) in chars.iter().enumerate() {
        if !matches!(c, '.' | '?' | '!') {
            continue;
        }
        let mut end = index + c.len_utf8();
        let mut next = at + 1;
        if let Some((_, quote)) = chars.get(next).filter(|(_, quote)| matches!(quote, '"' | '\'' | '”' | '’' | ')')) {
            if chars.get(next + 1).is_some_and(|(_, after)| after.is_whitespace()) {
                end += quote.len_utf8();
                next += 1;
            }
        }
        if chars.get(next).is_some_and(|(_, after)| after.is_whitespace()) {
            return text[..end].to_owned();
        }
    }
    text
}
/// `/(^|[^\w*])[*_]([^*_\n]+)[*_](?![\w*])/g` replaced by `$1$2`.
fn unwrap_emphasis(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let marker = |c: char| c == '*' || c == '_';
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let mut matched = None;
        // `^` first, then one character that is neither a word character nor `*`.
        let starts: &[(usize, bool)] = if i == 0 { &[(0, false), (1, true)] } else { &[(1, true)] };
        for &(skip, keep_lead) in starts {
            let open = i + skip;
            if keep_lead && !chars.get(i).is_some_and(|c| !word(*c) && *c != '*') {
                continue;
            }
            if !chars.get(open).copied().is_some_and(marker) {
                continue;
            }
            let mut end = open + 1;
            while chars.get(end).is_some_and(|c| !marker(*c) && *c != '\n') {
                end += 1;
            }
            if end == open + 1 || !chars.get(end).copied().is_some_and(marker) {
                continue;
            }
            if chars.get(end + 1).is_some_and(|c| word(*c) || *c == '*') {
                continue;
            }
            matched = Some((keep_lead.then(|| chars[i]), open + 1..end, end + 1));
            break;
        }
        match matched {
            Some((lead, body, next)) => {
                out.extend(lead);
                out.extend(&chars[body]);
                i = next;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// The panel heading and the detail's accessible name for a pending approval
/// (`ComposerPendingApprovalPanel`).
pub fn approval_labels(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "mcp-elicitation" => ("App access approval", "App access request"),
        "command" => ("Command approval", "Command"),
        "file-read" => ("File read approval", "File to read"),
        "permission" => ("App permission approval", "Permission request"),
        _ => ("File change approval", "File change"),
    }
}
/// The decisions the original shows as buttons; the rest sit in the "More approval options" menu.
pub fn approval_is_primary(decision: &str) -> bool {
    matches!(decision, "decline" | "accept")
}
/// Options when the item carries none (`DEFAULT_APPROVAL_OPTIONS`).
pub fn default_approval_options() -> Vec<Value> {
    use serde_json::json;
    vec![
        json!({"decision":"cancel","label":"Cancel"}),
        json!({"decision":"decline","label":"Decline"}),
        json!({"decision":"acceptForSession","label":"Always allow this session"}),
        json!({"decision":"accept","label":"Approve"}),
    ]
}

/// A message waiting behind the active run (`deriveThreadQueueWorkflowState`'s `queuedRuns`).
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedRun {
    pub run_id: String,
    pub message_id: String,
    pub text: String,
    pub attachments: Vec<Value>,
}
/// The user's queue, in delivery order. Automatic completion and notification deliveries are not
/// the user's messages and are left out (`getUserQueuedThreadRuns`).
pub fn queued_runs(projection: &Value) -> Vec<QueuedRun> {
    let messages = projection["messages"].as_array().map(Vec::as_slice).unwrap_or_default();
    let mut runs: Vec<&Value> = projection["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|run| run["status"] == "queued")
        .filter(|run| {
            !messages.iter().any(|message| message["id"] == run["userMessageId"] && (!message["delegatedCompletion"].is_null() || !message["notification"].is_null()))
        })
        .collect();
    let key = |run: &Value| (run["queuePosition"].as_i64().or_else(|| run["ordinal"].as_i64()).unwrap_or(0), run["ordinal"].as_i64().unwrap_or(0));
    runs.sort_by_key(|run| key(run));
    runs.into_iter()
        .map(|run| {
            let message = messages.iter().find(|message| message["id"] == run["userMessageId"]);
            QueuedRun {
                run_id: run["id"].as_str().unwrap_or_default().to_owned(),
                message_id: run["userMessageId"].as_str().unwrap_or_default().to_owned(),
                text: message.and_then(|message| message["text"].as_str()).unwrap_or("Queued message").to_owned(),
                attachments: message.and_then(|message| message["attachments"].as_array().cloned()).unwrap_or_default(),
            }
        })
        .collect()
}

/// What a settled thread still runs, for the strip above the composer
/// (`presentPendingBackgroundWork`).
#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundWork {
    pub title: String,
    /// True when the work will wake the agent (subagents, monitors); false when only commands remain.
    pub waiting: bool,
    pub items: Vec<BackgroundItem>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundItem {
    pub task_id: String,
    pub label: String,
    pub child_thread_id: Option<String>,
}
pub fn background_work(tasks: &[Value]) -> Option<BackgroundWork> {
    fn kind(kind: &str) -> (u8, &'static str, &'static str) {
        match kind {
            "subagent" => (0, "subagent", "subagents"),
            "command" => (1, "command", "commands"),
            "monitor" => (2, "monitor", "monitors"),
            _ => (3, "background task", "background tasks"),
        }
    }
    if tasks.is_empty() {
        return None;
    }
    let waiting = tasks.iter().any(|task| task["kind"] != "command");
    let mut items: Vec<(u8, &str, BackgroundItem)> = tasks
        .iter()
        .map(|task| {
            let name = task["kind"].as_str().unwrap_or("background_task");
            let (order, noun, _) = kind(name);
            let description = task["description"].as_str().map(str::trim).filter(|text| !text.is_empty());
            BackgroundItem {
                task_id: task["taskId"].as_str().unwrap_or_default().to_owned(),
                label: description.unwrap_or(noun).to_owned(),
                child_thread_id: task["childThreadId"].as_str().filter(|_| name == "subagent").map(str::to_owned),
            }
            .pipe(|item| (order, name, item))
        })
        .collect();
    items.sort_by_key(|(order, _, _)| *order);
    if let [(_, name, only)] = items.as_slice() {
        let noun = kind(name).1;
        let named = only.label != noun;
        let title = match (waiting, named) {
            (true, true) => format!("Waiting on {noun} {}", only.label),
            (true, false) => format!("Waiting on a {noun}"),
            (false, true) => format!("Running: {}", only.label),
            (false, false) => format!("Running a {noun}"),
        };
        return Some(BackgroundWork { title, waiting, items: items.into_iter().map(|(_, _, item)| item).collect() });
    }
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for (_, name, _) in &items {
        match counts.iter_mut().find(|(seen, _)| seen == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    let groups: Vec<String> = counts.iter().map(|(name, count)| format!("{count} {}", if *count == 1 { kind(name).1 } else { kind(name).2 })).collect();
    let joined = match groups.as_slice() {
        [] | [_] => groups.join(""),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    };
    Some(BackgroundWork { title: format!("{} {joined}", if waiting { "Waiting on" } else { "Running" }), waiting, items: items.into_iter().map(|(_, _, item)| item).collect() })
}
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

/// The elapsed time the working row shows (`formatWorkingTimer`): seconds, then minutes and hours.
pub fn working_elapsed(started: &str, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let started = chrono::DateTime::parse_from_rfc3339(started).ok()?;
    let seconds = (now - started.with_timezone(&chrono::Utc)).num_seconds().max(0);
    if seconds < 60 {
        return Some(format!("{seconds}s"));
    }
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    let parts: Vec<String> = [(hours, "h"), (minutes, "m"), (seconds, "s")].into_iter().filter(|(count, _)| *count > 0).map(|(count, unit)| format!("{count}{unit}")).collect();
    Some(parts.join(" "))
}

/// `formatUpcomingTimestamp` for the default (12-hour) clock: the time today, "tomorrow at …", or
/// the numeric date and time. `offset_minutes` is the viewer's zone east of UTC.
pub fn upcoming_timestamp(iso: &str, offset_minutes: i32, now: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::{Datelike, Duration, Timelike};
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let shift = Duration::minutes(offset_minutes as i64);
    let (local, here) = (at.with_timezone(&chrono::Utc) + shift, now + shift);
    let hour = if local.hour() % 12 == 0 { 12 } else { local.hour() % 12 };
    let time = format!("{hour}:{:02} {}", local.minute(), if local.hour() < 12 { "AM" } else { "PM" });
    match (local.date_naive() - here.date_naive()).num_days() {
        0 => time,
        1 => format!("tomorrow at {time}"),
        _ if local.year() == here.year() => format!("{}/{} {time}", local.month(), local.day()),
        _ => format!("{}/{}/{} {time}", local.month(), local.day(), local.year()),
    }
}

/// `resolveComposerDispatchMode`: how a send reaches a thread. A thread with nothing running just
/// starts; a running one gets the configured follow-up, or the other one on the alternate send.
pub fn dispatch_mode(running: bool, alternate: bool, queue_by_default: bool) -> &'static str {
    match (running, alternate == queue_by_default) {
        (false, _) => "auto",
        (true, true) => "steer",
        (true, false) => "queue",
    }
}

// ---- notifications ------------------------------------------------------------------------------

/// What a notification reports on (`OrchestrationV2NotificationSource`), with the server's two
/// spellings of a background command and of a background subagent read as the original decodes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    Subagent,
    DelegatedTask,
    Command,
    Monitor,
    BackgroundTask,
}
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationView {
    pub kind: NotificationKind,
    /// `notificationChildThreadId`: the thread to open, for a subagent or delegated task.
    pub child_thread_id: Option<String>,
    pub failed: bool,
    /// The glyph name `workEntryIconName` picks.
    pub icon: &'static str,
}
pub fn notification_view(item: &Value) -> NotificationView {
    let source = &item["source"];
    let kind = match (source["kind"].as_str(), source["work"].as_str()) {
        (Some("subagent"), _) | (Some("background_task"), Some("subagent")) => NotificationKind::Subagent,
        (Some("delegated_task"), _) => NotificationKind::DelegatedTask,
        (Some("command" | "background_command"), _) => NotificationKind::Command,
        (Some("monitor"), _) => NotificationKind::Monitor,
        _ => NotificationKind::BackgroundTask,
    };
    let child_thread_id = matches!(kind, NotificationKind::Subagent | NotificationKind::DelegatedTask).then(|| source["childThreadId"].as_str().map(str::to_owned)).flatten();
    let failed = item["outcome"] == "failed";
    let icon = if failed {
        "circle-alert"
    } else {
        match kind {
            NotificationKind::Subagent | NotificationKind::DelegatedTask => "bot",
            NotificationKind::Command => "terminal",
            NotificationKind::Monitor => "eye",
            NotificationKind::BackgroundTask => "zap",
        }
    };
    NotificationView { kind, child_thread_id, failed, icon }
}
/// `NOTIFICATION_OUTCOME_STATUS` and `NOTIFICATION_OUTCOME_LABEL` of `V2LifecycleRow.tsx`: the
/// status a subagent card shows for a report, frozen at the time it arrived, and its word.
pub fn notification_outcome(outcome: &str) -> (Option<&'static str>, &'static str) {
    match outcome {
        "completed" => (Some("completed"), "Finished"),
        "failed" => (Some("failed"), "Failed"),
        "cancelled" => (Some("cancelled"), "Stopped"),
        "updated" => (None, "Updated"),
        _ => (None, "Finished"),
    }
}
/// The word a tool row carries when the provider reported it was not run: its own kind of
/// non-execution (`toolNonExecutionKind`), read as words.
pub fn non_execution_word(kind: Option<&str>) -> Option<&'static str> {
    match kind?.trim().to_lowercase().replace(['_', '-'], " ").as_str() {
        "" => None,
        "denied" | "rejected" | "permission denied" => Some("denied"),
        "cancelled" | "canceled" => Some("cancelled"),
        "interrupted" => Some("interrupted"),
        "blocked" => Some("blocked"),
        "skipped" => Some("skipped"),
        _ => Some("not run"),
    }
}
/// `formatShortTimestamp` for the default 12-hour clock: `3:07 PM` in the viewer's zone.
pub fn short_timestamp(iso: &str, offset_minutes: i32) -> String {
    use chrono::{Duration, Timelike};
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let local = at.with_timezone(&chrono::Utc) + Duration::minutes(offset_minutes as i64);
    let hour = if local.hour() % 12 == 0 { 12 } else { local.hour() % 12 };
    format!("{hour}:{:02} {}", local.minute(), if local.hour() < 12 { "AM" } else { "PM" })
}

/// `formatDayAwareTimestamp` for the default 12-hour clock: the time today, `yesterday at …`, or
/// the numeric date and time, with the year once the calendar year differs. `offset_minutes` is the
/// viewer's zone east of UTC; boundaries are local calendar days.
pub fn day_aware_timestamp(iso: &str, offset_minutes: i32, now: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::{Datelike, Duration};
    let time = short_timestamp(iso, offset_minutes);
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let shift = Duration::minutes(offset_minutes as i64);
    let (local, here) = (at.with_timezone(&chrono::Utc) + shift, now + shift);
    match (here.date_naive() - local.date_naive()).num_days() {
        ..=0 => time,
        1 => format!("yesterday at {time}"),
        _ if local.year() == here.year() => format!("{}/{} {time}", local.month(), local.day()),
        _ => format!("{}/{}/{} {time}", local.month(), local.day(), local.year()),
    }
}
/// `formatChatTimestampTooltip`: `12:04 PM, 4th June 2026`.
pub fn timestamp_tooltip(iso: &str, offset_minutes: i32) -> String {
    use chrono::{Datelike, Duration};
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let local = at.with_timezone(&chrono::Utc) + Duration::minutes(offset_minutes as i64);
    let day = local.day();
    let suffix = match (day % 100, day % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{}, {day}{suffix} {} {}", short_timestamp(iso, offset_minutes), local.format("%B"), local.year())
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn both_spellings_of_a_source_read_as_the_original_decodes_them() {
        let view = |source: Value, outcome: &str| notification_view(&json!({"type":"notification","source":source,"outcome":outcome,"summary":"x"}));
        assert_eq!(view(json!({"kind":"background_command"}), "completed").icon, "terminal");
        assert_eq!(view(json!({"kind":"command"}), "completed").kind, NotificationKind::Command);
        assert_eq!(view(json!({"kind":"monitor"}), "updated").icon, "eye");
        assert_eq!(view(json!({"kind":"background_task"}), "completed").icon, "zap");
        let subagent = view(json!({"kind":"background_task","work":"subagent","childThreadId":"t2"}), "completed");
        assert_eq!((subagent.kind, subagent.child_thread_id.as_deref(), subagent.icon), (NotificationKind::Subagent, Some("t2"), "bot"));
        assert_eq!(view(json!({"kind":"subagent","childThreadId":"t3"}), "completed").child_thread_id.as_deref(), Some("t3"));
        // A command never opens a thread, and a failure takes the alert glyph from any source.
        assert_eq!(view(json!({"kind":"background_command","childThreadId":"t9"}), "completed").child_thread_id, None);
        assert_eq!(view(json!({"kind":"background_task","work":"subagent"}), "failed").icon, "circle-alert");
        // An unknown kind falls back to a background task, as `kindUnionWithFallback` does.
        assert_eq!(view(json!({"kind":"something_new"}), "unknown").kind, NotificationKind::BackgroundTask);
    }

    #[test]
    fn outcomes_and_times_read_as_the_original() {
        assert_eq!(notification_outcome("cancelled"), (Some("cancelled"), "Stopped"));
        assert_eq!(notification_outcome("updated"), (None, "Updated"));
        assert_eq!(notification_outcome("unknown"), (None, "Finished"));
        assert_eq!(short_timestamp("2026-10-01T15:07:00Z", 0), "3:07 PM");
        assert_eq!(short_timestamp("2026-10-01T00:05:00Z", 0), "12:05 AM");
        assert_eq!(short_timestamp("2026-10-01T23:30:00Z", 120), "1:30 AM");
        assert_eq!(short_timestamp("nonsense", 0), "");
    }

    #[test]
    fn a_footer_time_names_the_day_once_it_is_not_today() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-09T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        assert_eq!(day_aware_timestamp("2026-10-09T08:05:00Z", 0, now), "8:05 AM");
        assert_eq!(day_aware_timestamp("2026-10-08T23:59:00Z", 0, now), "yesterday at 11:59 PM");
        assert_eq!(day_aware_timestamp("2026-08-13T15:30:00Z", 0, now), "8/13 3:30 PM");
        assert_eq!(day_aware_timestamp("2025-08-13T15:30:00Z", 0, now), "8/13/2025 3:30 PM");
        // Days are the viewer's, not UTC's.
        assert_eq!(day_aware_timestamp("2026-10-08T23:30:00Z", 120, now), "1:30 AM");
        assert_eq!(timestamp_tooltip("2026-06-04T12:04:00Z", 0), "12:04 PM, 4th June 2026");
        assert_eq!(timestamp_tooltip("2026-06-22T00:04:00Z", 0), "12:04 AM, 22nd June 2026");
        assert_eq!(timestamp_tooltip("2026-06-11T00:04:00Z", 0), "12:04 AM, 11th June 2026");
    }
}
