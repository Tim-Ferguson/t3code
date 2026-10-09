//! How a subagent reads in the conversation: its title, state counts, the one line of detail under
//! it, the model it ran and how long it has worked. Port of `subagent-display.ts`, the subagent
//! row of `V2LifecycleRow.tsx`, `AgentElapsed.tsx` and `deriveSubagentElapsedMs`.
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

fn pattern(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("subagent pattern"))
}

pub fn is_active(status: &str) -> bool {
    matches!(status, "pending" | "running" | "waiting")
}
fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "interrupted")
}

/// `subagentGroupSummary`: the group's label, whether any member is working, and whether any failed.
pub fn group_summary(statuses: &[&str]) -> (String, bool, bool) {
    let active = statuses.iter().any(|status| is_active(status));
    let noun = if statuses.len() == 1 { "subagent" } else { "subagents" };
    (format!("{} {} {noun}", if active { "Kicked off" } else { "Ran" }, statuses.len()), active, statuses.contains(&"failed"))
}
/// `summarizeSubagentStatuses`: what is still running first, then the outcomes.
pub fn status_summary(statuses: &[&str]) -> String {
    let mut counts = [("working", 0), ("done", 0), ("failed", 0), ("stopped", 0), ("idle", 0)];
    for status in statuses {
        let index = match *status {
            status if is_active(status) => 0,
            "completed" => 1,
            "failed" => 2,
            "idle" => 4,
            _ => 3,
        };
        counts[index].1 += 1;
    }
    counts.iter().filter(|(_, count)| *count > 0).map(|(name, count)| format!("{count} {name}")).collect::<Vec<_>>().join(" \u{b7} ")
}
/// `formatSubagentDisplayTitle`: a Codex task path shown as its last part, in words.
pub fn display_title(title: &str) -> String {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    static PATH: OnceLock<Regex> = OnceLock::new();
    let display = pattern(&PREFIX, r"(?i)^Subagent:\s*").replace(title, "").into_owned();
    let Some(captures) = pattern(&PATH, r"^/root/(?:[^/]+/)*([^/]+)/?$").captures(&display) else { return display };
    let name = captures[1].split(|c: char| c == '_' || c.is_whitespace()).filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ");
    let mut titled = String::new();
    let mut start = true;
    for c in name.chars() {
        if start && !c.is_whitespace() { titled.extend(c.to_uppercase()) } else { titled.push(c) }
        start = c.is_whitespace();
    }
    if titled.is_empty() { display } else { titled }
}
/// `plainDetail`: one line of a markdown result, without bullets, code ticks or link targets.
pub fn plain_detail(text: &str) -> String {
    static LINK: OnceLock<Regex> = OnceLock::new();
    static BULLET: OnceLock<Regex> = OnceLock::new();
    let text = pattern(&LINK, r"\[([^\]]+)\]\([^)]*\)").replace_all(text, "$1").replace('`', "");
    let text = pattern(&BULLET, r"(?m)^[ \t]*[-*][ \t]+").replace_all(&text, "").into_owned();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
/// The line under a subagent's title: progress while it works, its result once it has finished. The
/// server's placeholder for a child that ended without output says nothing the state does not.
pub fn row_detail(status: &str, result: Option<&str>, progress: Option<&str>) -> Option<String> {
    static GENERIC: OnceLock<Regex> = OnceLock::new();
    let (result, progress) = (result.map(str::trim).filter(|text| !text.is_empty()), progress.map(str::trim).filter(|text| !text.is_empty()));
    let detail = if is_terminal(status) { result.or(progress) } else { progress.or(result) }?;
    (!pattern(&GENERIC, r"(?i)^Child task ended with status\b").is_match(detail)).then(|| plain_detail(detail)).filter(|detail| !detail.is_empty())
}
/// `subagentDetailPreview`: the hover card's line, cut at 280 characters.
pub fn detail_preview(status: &str, result: Option<&str>, progress: Option<&str>) -> Option<String> {
    let (result, progress) = (result.map(str::trim).unwrap_or_default(), progress.map(str::trim).unwrap_or_default());
    let detail = if is_terminal(status) { if result.is_empty() { progress } else { result } } else if progress.is_empty() { result } else { progress };
    let compact = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = compact.chars().collect();
    if chars.len() > 280 { Some(format!("{}\u{2026}", chars[..280].iter().collect::<String>().trim_end())) } else { Some(compact).filter(|text| !text.is_empty()) }
}
/// `status` as the row words it (`STATUS_VISUALS`).
pub fn status_label(status: &str) -> &'static str {
    match status {
        "pending" => "Queued",
        "running" => "Running",
        "waiting" => "Waiting",
        "idle" => "Idle \u{b7} resumable",
        "completed" => "Completed",
        "failed" => "Failed",
        "cancelled" | "interrupted" => "Stopped",
        _ => "",
    }
}

/// `deriveSubagentElapsedMs`; settled work with no end time has no duration.
pub fn elapsed_ms(status: &str, started_at: Option<&str>, completed_at: Option<&str>, now_ms: i64) -> Option<i64> {
    let start = chrono::DateTime::parse_from_rfc3339(started_at?).ok()?.timestamp_millis();
    let end = if is_active(status) { now_ms } else { chrono::DateTime::parse_from_rfc3339(completed_at?).ok()?.timestamp_millis() };
    Some((end - start).max(0))
}
/// `formatElapsedSeconds` of `AgentElapsed`.
pub fn format_elapsed(seconds: f64) -> String {
    let seconds = seconds.max(0.0).floor() as u64;
    let (minutes, hours) = (seconds / 60, seconds / 3600);
    if minutes == 0 {
        format!("{seconds}s")
    } else if hours == 0 {
        format!("{minutes}m {:02}s", seconds % 60)
    } else {
        format!("{hours}h {:02}m", minutes % 60)
    }
}

/// A model name as `formatModelSlugName` writes it.
pub fn model_slug_name(slug: &str) -> String {
    static GPT: OnceLock<Regex> = OnceLock::new();
    static KNOWN: OnceLock<Regex> = OnceLock::new();
    static DOT: OnceLock<Regex> = OnceLock::new();
    static DASH: OnceLock<Regex> = OnceLock::new();
    static GPT_HEAD: OnceLock<Regex> = OnceLock::new();
    let separator = slug.rfind('/').map_or(0, |index| index + 1);
    let (prefix, name) = slug.split_at(separator);
    if pattern(&GPT, r"(?i)^gpt-\d").is_match(name) {
        let head = pattern(&GPT_HEAD, r"(?i)^gpt").replace(name, "GPT").into_owned();
        let upper = pattern(&DASH, r"-([a-z])").replace_all(&head, |captures: &regex::Captures| format!("-{}", captures[1].to_uppercase())).into_owned();
        return format!("{prefix}{upper}");
    }
    if !pattern(&KNOWN, r"(?i)^(claude-(opus|sonnet|haiku|fable)|gemini|grok|composer)-\d").is_match(name) {
        return slug.to_owned();
    }
    let dotted = pattern(&DOT, r"(?i)^(claude-[a-z]+-\d+)-(\d{1,2})(?:-|\[|$)").captures(name).map(|captures| {
        let head = captures.get(1).expect("head");
        let digits = captures.get(2).expect("digits");
        format!("{}.{}{}", head.as_str(), digits.as_str(), &name[digits.end()..])
    });
    let words = dotted.unwrap_or_else(|| name.to_owned()).split('-').map(|part| {
        let mut chars = part.chars();
        chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
    }).collect::<Vec<_>>().join(" ");
    format!("{prefix}{words}")
}

/// The model label the hover card shows: the catalog's name for the model, else its slug in words;
/// "Not reported" when the provider named none (`resolveSubagentMetadata`).
pub fn model_label(model: Option<&str>, catalog: &[Value]) -> String {
    let model = model.map(str::trim).filter(|model| !model.is_empty());
    let found = model.and_then(|model| {
        catalog.iter().find(|entry| entry["slug"] == model).or_else(|| catalog.iter().find(|entry| entry["name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(model)))).or_else(|| catalog.iter().find(|entry| entry["aliases"].as_array().is_some_and(|aliases| aliases.iter().any(|alias| alias.as_str().is_some_and(|alias| alias.eq_ignore_ascii_case(model))))))
    });
    let reported = match (found, model) {
        (Some(entry), _) => entry["shortName"].as_str().filter(|name| !name.is_empty()).or_else(|| entry["name"].as_str()).unwrap_or_default().to_owned(),
        (None, Some(model)) => model_slug_name(model),
        (None, None) => return "Not reported".into(),
    };
    let Some(qualifier) = found.and_then(|entry| entry["subProvider"].as_str()).map(str::trim).filter(|qualifier| !qualifier.is_empty()) else { return reported };
    let stripped = Regex::new(&format!(r"(?iu)^{}(?:\s*[.:/-]\s*|\s+)", regex::escape(qualifier))).map(|re| re.replace(&reported, "").trim().to_owned()).unwrap_or_default();
    if stripped.is_empty() { reported } else { stripped }
}
