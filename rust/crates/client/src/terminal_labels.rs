//! Shared source terminalLabels.ts naming and client-owned ID allocation.
use std::{collections::BTreeSet, sync::LazyLock};
static NUMBER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)^term(?:inal)?-([0-9]+)(?:-[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})?$",
    )
    .unwrap()
});
fn number(id: &str) -> Option<&str> {
    NUMBER
        .captures(id)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}
pub fn label(id: &str, summary: Option<&str>) -> String {
    let summary=summary.map(|s|s.trim_matches(|ch|matches!(ch,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')));
    if let Some(s) = summary.filter(|s| !s.is_empty()) {
        return s.to_owned();
    }
    number(id)
        .map(|n| format!("Terminal {n}"))
        .unwrap_or_else(|| id.to_owned())
}
pub fn next_id(ids: &[String], suffix: Option<&str>) -> String {
    let used = ids
        .iter()
        .filter_map(|id| number(id))
        .collect::<BTreeSet<_>>();
    let mut next = 1;
    while used.contains(next.to_string().as_str()) {
        next += 1
    }
    let tail = suffix.map(|s| format!("-{s}")).unwrap_or_default();
    format!("term-{next}{tail}")
}
