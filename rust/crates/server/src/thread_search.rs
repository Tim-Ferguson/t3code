//! `orchestration.searchThreads` (port of `orchestration-v2/ThreadSearch.ts`): the finished user and
//! assistant messages of active threads in active projects, one best match per thread.
//!
//! The Rust store keeps one JSON document per thread, so the search reads `rust_projections`
//! directly with SQLite's JSON functions instead of the original's message table. No schema or
//! index changes: `LIKE '%query%'` could not use an index in the original either, and existing
//! databases are searched as they are.
use crate::persistence::{Store, StoreError};
use rusqlite::{params, types::Value as SqlValue};

const SNIPPET_LIMIT: usize = 240;
const SNIPPET_BODY: usize = 236;
const SNIPPET_LEAD: isize = 72;
pub const DEFAULT_LIMIT: u32 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadMatch {
    pub thread_id: String,
    pub project_id: String,
    pub source: String,
    pub snippet: String,
    pub message_created_at: Option<String>,
}

/// Carries no query text: search input is user content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    Query(String),
    Decode(String),
}

impl SearchError {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Query(_) => "query",
            Self::Decode(_) => "decode",
        }
    }
    /// `OrchestrationSearchThreadsError`, as the original's handler wraps a `ThreadSearchError`.
    pub fn wire(&self) -> serde_json::Value {
        let (Self::Query(detail) | Self::Decode(detail)) = self;
        serde_json::json!({
            "_tag":"OrchestrationSearchThreadsError",
            "message":"Failed to search threads",
            "cause":{
                "name":"ThreadSearchError",
                "message":format!("Thread search {} failed.", self.operation()),
                "cause":{"name":"Error","message":detail},
            },
        })
    }
}

impl From<StoreError> for SearchError {
    fn from(error: StoreError) -> Self {
        Self::Query(error.to_string())
    }
}
impl From<rusqlite::Error> for SearchError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Query(error.to_string())
    }
}

/// The `OrchestrationSearchThreadsResult` of a search: what the socket method and the MCP tool both return.
pub fn result_json(matches: &[ThreadMatch]) -> serde_json::Value {
    serde_json::json!({"matches":matches.iter().map(|found| serde_json::json!({
        "threadId":found.thread_id,"projectId":found.project_id,"source":found.source,
        "snippet":found.snippet,"messageCreatedAt":found.message_created_at,
    })).collect::<Vec<_>>()})
}

/// `LIKE` wildcards in the query match literally, escaped with `!`.
pub fn escape_like_pattern(value: &str) -> String {
    value.replace('!', "!!").replace('%', "!%").replace('_', "!_")
}

// One best match per thread: user messages outrank assistant ones, then the newest message wins
// (message id breaks ties). Threads order by match kind, then recency, then id. A thread's recency
// is the time of its last event, which the projection keeps at the top level.
const SEARCH: &str = "
WITH candidate AS (
  SELECT
    t.aggregate_id AS thread_id,
    json_extract(t.state_json, '$.thread.projectId') AS project_id,
    json_extract(m.value, '$.role') AS role,
    json_extract(m.value, '$.text') AS match_text,
    json_extract(m.value, '$.createdAt') AS message_created_at,
    json_extract(m.value, '$.id') AS message_id,
    json_extract(t.state_json, '$.updatedAt') AS thread_updated_at
  FROM rust_projections AS t
  INNER JOIN rust_projections AS p
    ON p.aggregate_kind = 'project'
   AND p.aggregate_id = json_extract(t.state_json, '$.thread.projectId')
  , json_each(t.state_json, '$.messages') AS m
  WHERE t.aggregate_kind = 'thread'
    AND (?3 = 0 OR t.state_json LIKE ?1 ESCAPE '!')
    AND json_extract(t.state_json, '$.thread.deletedAt') IS NULL
    AND json_extract(t.state_json, '$.thread.archivedAt') IS NULL
    AND json_extract(p.state_json, '$.deletedAt') IS NULL
    AND json_extract(m.value, '$.streaming') = 0
    AND json_extract(m.value, '$.role') IN ('user', 'assistant')
    AND json_extract(m.value, '$.text') LIKE ?1 ESCAPE '!'
),
ranked AS (
  SELECT
    thread_id, project_id, role AS source, match_text, message_created_at,
    CASE role WHEN 'user' THEN 0 ELSE 1 END AS match_rank,
    thread_updated_at,
    ROW_NUMBER() OVER (
      PARTITION BY thread_id
      ORDER BY CASE role WHEN 'user' THEN 0 ELSE 1 END ASC, message_created_at DESC, message_id ASC
    ) AS thread_match_rank
  FROM candidate
)
SELECT thread_id, project_id, source, match_text, message_created_at
FROM ranked
WHERE thread_match_rank = 1
ORDER BY match_rank ASC, thread_updated_at DESC, thread_id ASC
LIMIT ?2";

/// Whether the query text appears verbatim in a thread's stored JSON whenever it appears in a
/// message: true for ASCII without quotes, backslashes or control characters, which JSON would
/// escape. Then threads that cannot match are skipped before their messages are parsed.
fn can_prefilter_raw_json(query: &str) -> bool {
    query.chars().all(|c| c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\')
}

/// Searches with `query` as given: the socket handler has already trimmed it to 2..=200 characters.
pub fn search(store: &Store, query: &str, limit: Option<u32>) -> Result<Vec<ThreadMatch>, SearchError> {
    let pattern = format!("%{}%", escape_like_pattern(query));
    let limit = i64::from(limit.unwrap_or(DEFAULT_LIMIT));
    let prefilter = can_prefilter_raw_json(query);
    let rows = store.read(|connection| {
        let mut statement = connection.prepare_cached(SEARCH)?;
        let rows = statement
            .query_map(params![pattern, limit, i64::from(prefilter)], |row| {
                Ok((
                    row.get::<_, SqlValue>(0)?,
                    row.get::<_, SqlValue>(1)?,
                    row.get::<_, SqlValue>(2)?,
                    row.get::<_, SqlValue>(3)?,
                    row.get::<_, SqlValue>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    let text = |value: SqlValue, what: &str| match value {
        SqlValue::Text(text) => Ok(text),
        _ => Err(SearchError::Decode(format!("{what} is not a string"))),
    };
    rows.into_iter()
        .map(|(thread_id, project_id, source, match_text, created_at)| {
            Ok(ThreadMatch {
                thread_id: text(thread_id, "thread id")?,
                project_id: text(project_id, "project id")?,
                source: text(source, "source")?,
                snippet: build_snippet(&text(match_text, "message text")?, query),
                message_created_at: match created_at {
                    SqlValue::Null => None,
                    other => Some(text(other, "message time")?),
                },
            })
        })
        .collect()
}

// ---- snippets --------------------------------------------------------------------------------

/// JavaScript `\s`, which is also what `String.prototype.trim` strips.
fn is_js_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// `text.replace(/\s+/g, " ").trim()`.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if is_js_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out.trim_matches(' ').to_owned()
}

/// Only A-Z fold: the search itself is ASCII case-insensitive and nothing wider.
fn fold_ascii_case(units: &[u16]) -> Vec<u16> {
    units
        .iter()
        .map(|&unit| if (0x41..=0x5a).contains(&unit) { unit + 32 } else { unit })
        .collect()
}

fn index_of(haystack: &[u16], needle: &[u16]) -> isize {
    if needle.is_empty() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
        .map_or(-1, |index| index as isize)
}

/// At most 240 characters, centred near the first match. Positions are UTF-16 code units, as in
/// the original; a cut that falls inside a surrogate pair leaves a replacement character where the
/// original would leave a lone surrogate, which a Rust string cannot hold.
pub fn build_snippet(text: &str, query: &str) -> String {
    let normalized: Vec<u16> = collapse_whitespace(text).encode_utf16().collect();
    if normalized.len() <= SNIPPET_LIMIT {
        return String::from_utf16_lossy(&normalized);
    }
    let query: Vec<u16> = collapse_whitespace(query).encode_utf16().collect();
    let match_index = index_of(&fold_ascii_case(&normalized), &fold_ascii_case(&query));
    let ideal_start = (match_index - SNIPPET_LEAD).max(0) as usize;
    let start = ideal_start.min(normalized.len() - SNIPPET_BODY);
    let end = normalized.len().min(start + SNIPPET_BODY);
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        String::from_utf16_lossy(&normalized[start..end]),
        if end < normalized.len() { "…" } else { "" }
    )
}

#[cfg(test)]
#[path = "thread_search_tests.rs"]
mod tests;
