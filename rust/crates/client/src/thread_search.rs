//! Searching threads from the sidebar and the palette: when a query may go to the server, how an
//! answer is accepted or dropped, and how the server's message matches combine with title
//! matches. Port of `useThreadSearch`, `threadSearch.ts` (client-runtime) and
//! `searchSidebarThreads`.
use serde_json::Value;
use std::collections::BTreeMap;
use t3_contracts::trim_wire_string;

/// The wait after the last keystroke before the server is asked; every call is a full scan there.
pub const DEBOUNCE_MS: u64 = 200;
pub const MIN_QUERY_UNITS: usize = 2;
pub const MAX_QUERY_UNITS: usize = 200;
pub const STATUS_SEARCHING: &str = "Searching thread messages\u{2026}";
pub const STATUS_NONE: &str = "No threads found";

/// The text to send for a typed query, or None when it is too short or too long for the server to
/// accept (trimmed, 2 to 200 UTF-16 code units). Title matches still work for those.
pub fn request_query(raw: &str) -> Option<String> {
    let trimmed = trim_wire_string(raw);
    (MIN_QUERY_UNITS..=MAX_QUERY_UNITS).contains(&trimmed.encode_utf16().count()).then(|| trimmed.to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub thread_id: String,
    pub project_id: String,
    pub user: bool,
    pub snippet: String,
    pub created_at: Option<String>,
}
/// Decodes `{matches:[...]}`. Err is what to show for a reply that is not the contract's.
pub fn decode_matches(result: &Value) -> Result<Vec<Match>, String> {
    let rows = result["matches"].as_array().ok_or("The search reply was not understood.")?;
    rows.iter()
        .map(|row| {
            Some(Match {
                thread_id: row["threadId"].as_str()?.to_owned(),
                project_id: row["projectId"].as_str()?.to_owned(),
                user: match row["source"].as_str()? {
                    "user" => true,
                    "assistant" => false,
                    _ => return None,
                },
                snippet: row["snippet"].as_str()?.to_owned(),
                created_at: row["messageCreatedAt"].as_str().map(str::to_owned),
            })
        })
        .collect::<Option<_>>()
        .ok_or_else(|| "The search reply was not understood.".to_owned())
}

/// What one search was asked of: the environment's connection, the typed query as sent, and a
/// number that grows with every request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ask {
    pub owner: String,
    pub query: String,
    pub generation: u64,
}
/// An answer is kept only while it answers the latest question: same connection, same query,
/// nothing asked since.
pub fn answers_current(asked: &Ask, current: &Ask) -> bool {
    asked == current
}

/// What the page knows about a thread, for combining with the server's matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub project_id: String,
    pub title: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub thread: Candidate,
    pub found: Option<Match>,
}
/// Threads whose title contains the query, in list order, then threads whose messages the server
/// matched, in list order. A match for a thread that is not in the list (deleted, archived, or
/// from before the list changed) is dropped.
pub fn combine(threads: &[Candidate], query: &str, matches: &[Match]) -> Vec<Hit> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return vec![];
    }
    let by_thread: BTreeMap<&str, &Match> = matches.iter().map(|found| (found.thread_id.as_str(), found)).collect();
    let (mut titled, mut messaged) = (vec![], vec![]);
    for thread in threads {
        let found = by_thread.get(thread.id.as_str()).map(|found| (*found).clone());
        if thread.title.to_lowercase().contains(&query) {
            titled.push(Hit { thread: thread.clone(), found });
        } else if found.is_some() {
            messaged.push(Hit { thread: thread.clone(), found });
        }
    }
    titled.extend(messaged);
    titled
}

/// Which row the keyboard has highlighted after a move, wrapping at both ends.
pub fn step(current: usize, count: usize, forward: bool) -> usize {
    if count == 0 {
        0
    } else if forward {
        (current + 1) % count
    } else {
        (current + count - 1) % count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate(id: &str, title: &str) -> Candidate {
        Candidate { id: id.into(), project_id: "p".into(), title: title.into() }
    }
    fn found(id: &str, snippet: &str) -> Match {
        Match { thread_id: id.into(), project_id: "p".into(), user: true, snippet: snippet.into(), created_at: None }
    }

    #[test]
    fn only_queries_the_server_accepts_are_sent() {
        assert_eq!(request_query("  ab  "), Some("ab".into()));
        assert_eq!(request_query("a"), None);
        assert_eq!(request_query("  a "), None);
        assert_eq!(request_query(""), None);
        assert_eq!(request_query(&"x".repeat(200)).map(|query| query.len()), Some(200));
        assert_eq!(request_query(&"x".repeat(201)), None);
        // Two UTF-16 units: one astral character counts as two, as the contract counts.
        assert_eq!(request_query("\u{1F600}"), Some("\u{1F600}".into()));
        assert_eq!(request_query("é"), None);
    }

    #[test]
    fn replies_decode_and_a_malformed_one_is_refused() {
        let reply = json!({"matches":[
            {"threadId":"t1","projectId":"p","source":"assistant","snippet":"the fix","messageCreatedAt":"2026-01-01T00:00:00Z"},
            {"threadId":"t2","projectId":"p","source":"user","snippet":"why","messageCreatedAt":null}]});
        let matches = decode_matches(&reply).unwrap();
        assert_eq!((matches[0].user, matches[0].created_at.as_deref()), (false, Some("2026-01-01T00:00:00Z")));
        assert_eq!((matches[1].user, matches[1].created_at.clone()), (true, None));
        assert!(decode_matches(&json!({"matches":[{"threadId":"t","projectId":"p","source":"system","snippet":"","messageCreatedAt":null}]})).is_err());
        assert!(decode_matches(&json!({})).is_err());
    }

    #[test]
    fn title_matches_come_first_then_message_matches_in_list_order() {
        let threads = [candidate("t1", "Deploy notes"), candidate("t2", "Login fix"), candidate("t3", "Deploy script"), candidate("t4", "Other")];
        let matches = [found("t4", "we should deploy"), found("t1", "deploy it"), found("gone", "deploy")];
        let hits = combine(&threads, "Deploy", &matches);
        assert_eq!(hits.iter().map(|hit| hit.thread.id.as_str()).collect::<Vec<_>>(), ["t1", "t3", "t4"]);
        assert_eq!(hits[0].found.as_ref().unwrap().snippet, "deploy it", "a title match also shows its message excerpt");
        assert!(hits[1].found.is_none());
        assert_eq!(hits[2].found.as_ref().unwrap().snippet, "we should deploy");
        // A match for a thread the list no longer has is dropped; no query finds nothing.
        assert!(combine(&threads, "  ", &matches).is_empty());
        assert!(combine(&[], "deploy", &matches).is_empty());
    }

    #[test]
    fn an_answer_is_kept_only_for_the_latest_question_on_the_same_connection() {
        let latest = Ask { owner: "env-a".into(), query: "login".into(), generation: 4 };
        assert!(answers_current(&latest.clone(), &latest));
        assert!(!answers_current(&Ask { generation: 3, ..latest.clone() }, &latest), "an older request");
        assert!(!answers_current(&Ask { query: "logi".into(), ..latest.clone() }, &latest), "an older query");
        assert!(!answers_current(&Ask { owner: "env-b".into(), ..latest.clone() }, &latest), "another connection");
    }

    #[test]
    fn the_highlight_wraps() {
        assert_eq!((step(0, 3, true), step(2, 3, true), step(0, 3, false), step(1, 3, false), step(0, 0, true)), (1, 0, 2, 0, 0));
    }
}
