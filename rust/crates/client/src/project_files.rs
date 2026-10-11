//! Finding a file of the open project by name or by what it contains, and the file viewer those
//! results open. Port of `ProjectFilePicker.logic.ts`, `useProjectPathSearch` /
//! `useProjectContentSearch` (`state/queries.ts`) and the grouping of `ProjectContentSearchDialog.tsx`.
use serde_json::{Value, json};

/// Rows the file picker draws (`PROJECT_FILE_PICKER_RESULT_LIMIT`).
pub const FILE_LIMIT: usize = 200;
/// The pause after the last keystroke before the server is asked, for names and for contents.
pub const PATH_DEBOUNCE_MS: u64 = 120;
pub const CONTENT_DEBOUNCE_MS: u64 = 120;
/// Matches a content search asks for, and how many rows are drawn at first and after each step.
pub const CONTENT_LIMIT: usize = 500;
pub const VISIBLE_WINDOW: usize = 100;
pub const SEARCH_ENTRIES: &str = "projects.searchEntries";
pub const SEARCH_CONTENTS: &str = "projects.searchContents";
pub const READ_FILE: &str = "projects.readFile";

/// `normalizeSearchQuery` with the picker's leading `@./` trimmed, whitespace removed.
pub fn normalize_query(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed.trim_start_matches(['@', '.', '/']);
    stripped.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect()
}

/// The first ordered subsequence of `query` in `value`, as character positions. None when `value`
/// does not hold it (the row still shows, the server matched it, just without marks).
pub fn match_indices(value: &str, query: &str) -> Option<Vec<usize>> {
    if query.is_empty() {
        return Some(vec![]);
    }
    let mut wanted = query.chars();
    let mut next = wanted.next();
    let mut found = vec![];
    for (position, character) in value.chars().enumerate() {
        let Some(want) = next else { break };
        // The original lowercases the value as a whole; per character is the same for what people type.
        if character.to_lowercase().next() == Some(want) {
            found.push(position);
            next = wanted.next();
            if next.is_none() {
                return Some(found);
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub is_file: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryResult {
    pub entries: Vec<Entry>,
    pub truncated: bool,
}
pub fn entries_input(cwd: &str, query: &str) -> Value {
    json!({"cwd":cwd,"query":query.trim(),"limit":FILE_LIMIT,"kind":"file"})
}
pub fn decode_entries(result: &Value) -> Result<EntryResult, String> {
    let rows = result["entries"].as_array().ok_or("The search reply was not understood.")?;
    let entries = rows
        .iter()
        .map(|row| Some(Entry { path: row["path"].as_str()?.to_owned(), is_file: row["kind"].as_str()? == "file" }))
        .collect::<Option<Vec<_>>>()
        .ok_or("The search reply was not understood.")?;
    Ok(EntryResult { entries, truncated: result["truncated"].as_bool().unwrap_or(false) })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatch {
    pub name: String,
    pub name_indices: Vec<usize>,
    pub path: String,
    pub path_indices: Vec<usize>,
}
/// The picker's rows: files only, the server's order, marks computed with the server's normalization.
pub fn file_matches(entries: &[Entry], raw_query: &str, limit: usize) -> Vec<FileMatch> {
    let query = normalize_query(raw_query);
    entries
        .iter()
        .filter(|entry| entry.is_file)
        .take(limit)
        .map(|entry| {
            let name = entry.path.rsplit('/').next().unwrap_or(&entry.path).to_owned();
            FileMatch { name_indices: match_indices(&name, &query).unwrap_or_default(), path_indices: match_indices(&entry.path, &query).unwrap_or_default(), name, path: entry.path.clone() }
        })
        .collect()
}
/// `value` cut into runs, each marked when its character is one of `indices` (sorted).
pub fn marked_runs(value: &str, indices: &[usize]) -> Vec<(String, bool)> {
    let mut runs: Vec<(String, bool)> = vec![];
    for (position, character) in value.chars().enumerate() {
        let marked = indices.binary_search(&position).is_ok();
        match runs.last_mut() {
            Some((text, last)) if *last == marked => text.push(character),
            _ => runs.push((character.to_string(), marked)),
        }
    }
    runs
}
/// What the picker says in place of rows (`getEmptyStateMessage`).
pub fn empty_message(query: &str, error: Option<&str>, pending: bool) -> String {
    if let Some(error) = error {
        return error.to_owned();
    }
    let searching = !query.trim().is_empty();
    match (pending, searching) {
        (true, true) => "Searching workspace files\u{2026}".into(),
        (true, false) => "Indexing workspace files\u{2026}".into(),
        (false, true) => "No matching files.".into(),
        (false, false) => "No files found.".into(),
    }
}

/// One line of a file that matched, with the character ranges (not UTF-16 units) to mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentMatch {
    pub path: String,
    pub line: u64,
    pub text: String,
    pub ranges: Vec<(usize, usize)>,
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContentResult {
    pub matches: Vec<ContentMatch>,
    pub truncated: bool,
    /// The server fell back to a plain search because the pattern would not compile.
    pub invalid_regex: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}
pub fn contents_input(cwd: &str, query: &str, options: Options) -> Value {
    json!({"cwd":cwd,"query":query,"limit":CONTENT_LIMIT,"caseSensitive":options.case_sensitive,"wholeWord":options.whole_word,"useRegex":options.regex})
}
/// Whether a typed content query is sent: whitespace counts in a query, but a blank one is not one.
pub fn content_query_ready(query: &str) -> bool {
    !query.trim().is_empty()
}
/// The server counts positions in UTF-16 units, as the original does.
fn units_to_chars(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (position, character) in text.chars().enumerate() {
        if seen >= units {
            return position;
        }
        seen += character.len_utf16();
    }
    text.chars().count()
}
pub fn decode_contents(result: &Value, regex_requested: bool) -> Result<ContentResult, String> {
    let rows = result["matches"].as_array().ok_or("The search reply was not understood.")?;
    let matches = rows
        .iter()
        .map(|row| {
            let text = row["lineContent"].as_str()?.to_owned();
            let total = text.chars().count();
            let mut ranges: Vec<(usize, usize)> = row["matchRanges"]
                .as_array()?
                .iter()
                .filter_map(|range| {
                    let start = units_to_chars(&text, range["start"].as_u64()? as usize).min(total);
                    let end = units_to_chars(&text, range["end"].as_u64()? as usize).min(total);
                    (end > start).then_some((start, end))
                })
                .collect();
            ranges.sort_unstable();
            // Overlapping or touching ranges are one mark.
            let mut merged: Vec<(usize, usize)> = vec![];
            for range in ranges {
                match merged.last_mut() {
                    Some(last) if range.0 <= last.1 => last.1 = last.1.max(range.1),
                    _ => merged.push(range),
                }
            }
            Some(ContentMatch { path: row["path"].as_str()?.to_owned(), line: row["lineNumber"].as_u64()?, text, ranges: merged })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or("The search reply was not understood.")?;
    Ok(ContentResult { matches, truncated: result["truncated"].as_bool().unwrap_or(false), invalid_regex: regex_requested && result.get("regexFallbackError").is_some_and(|error| !error.is_null()) })
}

/// A file's matches, shown under one heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub path: String,
    pub name: String,
    pub directory: String,
    /// The position of each row among all matches, which the selection and Enter use.
    pub rows: Vec<usize>,
}
/// Groups the first `visible` matches by file, in the order files first appear.
pub fn group(matches: &[ContentMatch], visible: usize) -> Vec<Group> {
    let mut groups: Vec<Group> = vec![];
    for (index, found) in matches.iter().take(visible).enumerate() {
        match groups.iter_mut().rev().find(|group| group.path == found.path) {
            Some(group) => group.rows.push(index),
            None => {
                let (directory, name) = found.path.rsplit_once('/').map(|(directory, name)| (directory.to_owned(), name.to_owned())).unwrap_or((String::new(), found.path.clone()));
                groups.push(Group { path: found.path.clone(), name, directory, rows: vec![index] });
            }
        }
    }
    groups
}
/// `12,345` as `toLocaleString` writes it in English.
pub fn grouped_digits(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}
/// The line above the results: how many, in how many files.
pub fn summary(matches: &[ContentMatch], truncated: bool) -> String {
    let files = matches.iter().map(|found| found.path.as_str()).collect::<std::collections::BTreeSet<_>>().len();
    format!("{}{} results in {} files", grouped_digits(matches.len()), if truncated { "+" } else { "" }, grouped_digits(files))
}

/// A file's lines, as the viewer draws a window of them.
pub const VIEW_WINDOW: usize = 400;
pub const VIEW_STEP: usize = 500;
/// The first line of the window that shows `reveal` with room above it.
pub fn view_start(reveal: Option<u64>) -> usize {
    reveal.map_or(0, |line| (line as usize).saturating_sub(1).saturating_sub(VIEW_WINDOW / 4))
}
/// Splits file contents into lines the way an editor counts them: a final newline ends the last
/// line instead of starting another.
pub fn lines(contents: &str) -> Vec<&str> {
    let mut rows: Vec<&str> = contents.split('\n').map(|row| row.strip_suffix('\r').unwrap_or(row)).collect();
    if rows.last() == Some(&"") {
        rows.pop();
    }
    rows
}
/// Why a file cannot be shown, from the server's failure tag.
pub fn read_failure_message(error: &str) -> String {
    let lowered = error.to_lowercase();
    if lowered.contains("binary") {
        "This file is binary, so it cannot be shown as text.".into()
    } else if lowered.contains("not a regular file") || lowered.contains("path_not_file") {
        "This path is not a file.".into()
    } else {
        error.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<Entry> {
        paths.iter().map(|path| Entry { path: (*path).into(), is_file: !path.ends_with('/') }).collect()
    }

    #[test]
    fn the_leading_at_dot_and_slash_are_not_part_of_the_query() {
        assert_eq!(normalize_query("  @/Src Index "), "srcindex");
        assert_eq!(normalize_query("./a b"), "ab");
        assert_eq!(normalize_query(""), "");
    }

    #[test]
    fn marks_follow_the_first_ordered_subsequence() {
        assert_eq!(match_indices("aabba", "aba"), Some(vec![0, 2, 4]));
        assert_eq!(match_indices("TestFlags.tsx", "tsfl"), Some(vec![0, 2, 4, 5]));
        assert_eq!(match_indices("abc", "z"), None);
        assert_eq!(match_indices("abc", ""), Some(vec![]));
        let rows = file_matches(&files(&["src/TestFlags.tsx"]), "tsfl", 10);
        assert_eq!((rows[0].name_indices.clone(), rows[0].path_indices.clone()), (vec![0, 2, 4, 5], vec![4, 6, 8, 9]));
    }

    #[test]
    fn rows_are_files_only_in_the_servers_order_and_limited() {
        let entries = files(&["apps/web/src/", "apps/web/src/index.ts", "packages/index.ts", "README.md"]);
        let rows = file_matches(&entries, "", 2);
        assert_eq!(rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(), ["apps/web/src/index.ts", "packages/index.ts"]);
        assert_eq!(rows[0].name, "index.ts");
        assert!(rows[0].name_indices.is_empty(), "an empty query marks nothing");
        let path_with_prefix = file_matches(&files(&["src/index.ts"]), "@/src", 5);
        assert_eq!(path_with_prefix[0].path_indices, vec![0, 1, 2]);
    }

    #[test]
    fn marked_runs_join_neighbours() {
        let runs = marked_runs("index.ts", &[0, 1, 5]);
        let expected = [("in", true), ("dex", false), (".", true), ("ts", false)];
        assert_eq!(runs.iter().map(|(text, marked)| (text.as_str(), *marked)).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn the_empty_messages_are_the_originals() {
        assert_eq!(empty_message("", None, true), "Indexing workspace files\u{2026}");
        assert_eq!(empty_message("x", None, true), "Searching workspace files\u{2026}");
        assert_eq!(empty_message("x", None, false), "No matching files.");
        assert_eq!(empty_message("  ", None, false), "No files found.");
        assert_eq!(empty_message("x", Some("boom"), true), "boom");
    }

    #[test]
    fn content_ranges_are_utf16_units_made_into_characters_and_merged() {
        let reply = json!({"matches":[{"path":"a/b.rs","lineNumber":3,"lineContent":"\u{1F600}x foo foo","matchRanges":[{"start":3,"end":4},{"start":4,"end":6},{"start":7,"end":10}]}],"truncated":true});
        let result = decode_contents(&reply, false).unwrap();
        assert_eq!(result.matches[0].ranges, vec![(2, 5), (6, 9)]);
        assert!(result.truncated);
        let bad = decode_contents(&json!({"matches":[],"truncated":false,"regexFallbackError":"nope"}), true).unwrap();
        assert!(bad.invalid_regex);
        assert!(!decode_contents(&json!({"matches":[],"truncated":false,"regexFallbackError":"nope"}), false).unwrap().invalid_regex);
        assert!(decode_contents(&json!({"nothing":1}), false).is_err());
    }

    #[test]
    fn matches_group_by_file_and_the_summary_counts_files() {
        let one = |path: &str, line: u64| ContentMatch { path: path.into(), line, text: String::new(), ranges: vec![] };
        let all = vec![one("src/a.rs", 1), one("src/a.rs", 9), one("b.md", 2), one("src/a.rs", 20)];
        let groups = group(&all, 3);
        assert_eq!(groups.len(), 2);
        assert_eq!((groups[0].name.as_str(), groups[0].directory.as_str(), groups[0].rows.clone()), ("a.rs", "src", vec![0, 1]));
        assert_eq!((groups[1].name.as_str(), groups[1].directory.as_str()), ("b.md", ""));
        assert_eq!(summary(&all, true), "4+ results in 2 files");
        assert_eq!(grouped_digits(1234567), "1,234,567");
    }

    #[test]
    fn the_viewer_window_leaves_room_above_the_revealed_line() {
        assert_eq!(view_start(None), 0);
        assert_eq!(view_start(Some(5)), 0);
        assert_eq!(view_start(Some(1000)), 899);
        assert_eq!(lines("a\r\nb\n"), ["a", "b"]);
        assert_eq!(lines("a\n\n"), ["a", ""]);
        assert!(lines("").is_empty());
    }
}

/// A path the panel shows as a picture (`isWorkspaceImagePreviewPath`).
pub fn is_image_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    [".avif", ".gif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".webp"].iter().any(|extension| lower.ends_with(extension))
}

/// A path whose contents the panel can show rendered (`isMarkdownPreviewFile`).
pub fn is_markdown_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".md") || lower.ends_with(".mdx")
}
