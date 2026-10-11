//! Links in terminal output: the URLs and file paths a line of text holds, which of them lies under a
//! cell, and the path an editor is asked to open. Port of `terminal-links.ts`. A wrapped line is one
//! line for this purpose, so a link that wraps is found whole.
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Url,
    Path,
}
/// A link in a line: `start..end` are character positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub kind: Kind,
    pub text: String,
    pub start: usize,
    pub end: usize,
}

static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)https?://[^\s"'`<>]+"#).expect("url pattern"));
static PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:~/|\.{1,2}/|/|[A-Za-z]:[\\/]|\\\\)[^\s"'`<>]+|[A-Za-z0-9._-]+(?:/[A-Za-z0-9._-]+)+(?::\d+){0,2}"#).expect("path pattern"));

/// Sentence punctuation after a link is not part of it; a path also drops the colon compilers end
/// `file:line:col:` with. A closing bracket with no opening one before it is dropped too.
fn trim_closing(value: &str, kind: Kind) -> String {
    let mut output = value.trim_end_matches(if kind == Kind::Path { &['.', ',', ';', ':', '!', '?'][..] } else { &['.', ',', ';', '!', '?'][..] }).to_owned();
    for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
        while output.ends_with(close) {
            let (opens, closes) = (output.matches(open).count(), output.matches(close).count());
            if opens >= closes {
                break;
            }
            output.pop();
        }
    }
    output
}

fn collect(line: &str, kind: Kind, pattern: &Regex, existing: &[Match]) -> Vec<Match> {
    let mut found: Vec<Match> = vec![];
    for raw in pattern.find_iter(line) {
        let trimmed = trim_closing(raw.as_str(), kind);
        if trimmed.is_empty() || (kind == Kind::Path && is_url(&trimmed)) {
            continue;
        }
        let start = line[..raw.start()].chars().count();
        let candidate = Match { kind, end: start + trimmed.chars().count(), text: trimmed, start };
        if existing.iter().chain(found.iter()).any(|other| candidate.start < other.end && other.start < candidate.end) {
            continue;
        }
        found.push(candidate);
    }
    found
}

/// `extractTerminalLinks`: URLs first, then paths that do not overlap them, in line order.
pub fn extract(line: &str) -> Vec<Match> {
    let urls = collect(line, Kind::Url, &URL, &[]);
    let paths = collect(line, Kind::Path, &PATH, &urls);
    let mut all: Vec<Match> = urls.into_iter().chain(paths).collect();
    all.sort_by_key(|link| link.start);
    all
}
pub fn is_url(text: &str) -> bool {
    let lowered = text.to_lowercase();
    lowered.starts_with("http://") || lowered.starts_with("https://")
}
/// A click opens a link only with the platform's modifier held: Cmd on a Mac, Ctrl elsewhere.
pub fn is_activation(meta: bool, ctrl: bool, mac: bool) -> bool {
    if mac { meta && !ctrl } else { ctrl && !meta }
}

fn windows_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')) || value.starts_with("\\\\")
}
fn absolute(value: &str) -> bool {
    value.starts_with('/') || windows_absolute(value)
}
fn windows_style(value: &str) -> bool {
    windows_absolute(value) || value.as_bytes().windows(3).any(|window| window[0].is_ascii_alphabetic() && window[1] == b':' && window[2] == b'\\')
}
fn join(base: &str, next: &str, separator: char) -> String {
    let base = base.trim_end_matches(['/', '\\']);
    if separator == '\\' { format!("{base}\\{}", next.replace('/', "\\")) } else { format!("{base}/{}", next.trim_start_matches('/')) }
}
fn home_of(cwd: &str) -> Option<String> {
    for prefix in ["/Users/", "/home/"] {
        if let Some(rest) = cwd.strip_prefix(prefix) {
            let user = rest.split('/').next().filter(|user| !user.is_empty())?;
            return Some(format!("{prefix}{user}"));
        }
    }
    let rest = cwd.get(1..)?.strip_prefix(":\\Users\\")?;
    let user = rest.split('\\').next().filter(|user| !user.is_empty())?;
    Some(format!("{}:\\Users\\{user}", &cwd[..1]))
}
/// The path and the `:line:column` that follows it, if any.
fn split_position(raw: &str) -> (&str, &str) {
    let mut end = raw.len();
    for _ in 0..2 {
        match raw[..end].rfind(':') {
            Some(at) if at > 0 && !raw[at + 1..end].is_empty() && raw[at + 1..end].bytes().all(|byte| byte.is_ascii_digit()) && !(at == 1 && raw.as_bytes()[0].is_ascii_alphabetic() && raw[..end].len() > 2 && matches!(raw.as_bytes().get(2), Some(b'\\' | b'/'))) => end = at,
            _ => break,
        }
    }
    (&raw[..end], &raw[end..])
}
/// `resolvePathLinkTarget`: the path against the terminal's directory (`~/` against the home it implies), with its position kept.
pub fn resolve_path_target(raw: &str, cwd: &str) -> String {
    let (path, position) = split_position(raw);
    let resolved = if let Some(rest) = path.strip_prefix("~/") {
        match home_of(cwd) {
            Some(home) => join(&home, rest, if windows_style(&home) { '\\' } else { '/' }),
            None => path.to_owned(),
        }
    } else if !absolute(path) {
        join(cwd, path, if windows_style(cwd) { '\\' } else { '/' })
    } else {
        path.to_owned()
    };
    format!("{resolved}{position}")
}

/// One cell of a terminal row, for finding what lies under it.
pub trait Cells {
    fn rows(&self) -> usize;
    /// The text of each cell of the row (empty for the second half of a wide character).
    fn cells(&self, row: usize) -> Vec<&str>;
    fn continues_previous(&self, row: usize) -> bool;
    fn continues_next(&self, row: usize) -> bool;
}
/// A link and the cells it covers, first to last, as (column, row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub kind: Kind,
    pub text: String,
    pub start: (usize, usize),
    pub end: (usize, usize),
}
/// The link under `(column, row)`, reading the whole wrapped line the row belongs to.
pub fn link_at(grid: &impl Cells, column: usize, row: usize) -> Option<Hit> {
    if row >= grid.rows() {
        return None;
    }
    let mut first = row;
    while first > 0 && grid.continues_previous(first) {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < grid.rows() && grid.continues_next(last) {
        last += 1;
    }
    let mut text = String::new();
    // Where each character of `text` sits on the screen.
    let mut places: Vec<(usize, usize)> = vec![];
    for current in first..=last {
        for (at, cell) in grid.cells(current).into_iter().enumerate() {
            if cell.is_empty() {
                text.push(' ');
                places.push((at, current));
            } else {
                for character in cell.chars() {
                    text.push(character);
                    places.push((at, current));
                }
            }
        }
    }
    let under = places.iter().position(|place| *place == (column, row))?;
    let link = extract(&text).into_iter().find(|link| link.start <= under && under < link.end)?;
    Some(Hit { kind: link.kind, start: places[link.start], end: places[link.end - 1], text: link.text })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &str) -> Vec<(Kind, String)> {
        extract(line).into_iter().map(|link| (link.kind, link.text)).collect()
    }

    #[test]
    fn urls_and_paths_are_found_without_the_punctuation_around_them() {
        assert_eq!(texts("see https://example.com/a?b=1, then (https://x.dev/y)."), [(Kind::Url, "https://example.com/a?b=1".into()), (Kind::Url, "https://x.dev/y".into())]);
        assert_eq!(texts("error at src/main.rs:12:5: oops"), [(Kind::Path, "src/main.rs:12:5".into())]);
        assert_eq!(texts("open ./a/b.txt and ~/notes.md and /etc/hosts."), [(Kind::Path, "./a/b.txt".into()), (Kind::Path, "~/notes.md".into()), (Kind::Path, "/etc/hosts".into())]);
        assert_eq!(texts(r"C:\Users\me\file.txt next"), [(Kind::Path, r"C:\Users\me\file.txt".into())]);
        // The path inside a URL is the URL's.
        assert_eq!(texts("https://example.com/a/b/c.js"), [(Kind::Url, "https://example.com/a/b/c.js".into())]);
        assert!(texts("no links here").is_empty());
    }

    #[test]
    fn positions_count_characters_not_bytes() {
        let links = extract("é→ https://x.dev/é");
        assert_eq!((links[0].start, links[0].end), (3, 18));
    }

    #[test]
    fn a_path_resolves_against_the_terminals_directory_and_keeps_its_position() {
        assert_eq!(resolve_path_target("src/a.rs:3:4", "/work/app"), "/work/app/src/a.rs:3:4");
        assert_eq!(resolve_path_target("/abs/a.rs:9", "/work/app"), "/abs/a.rs:9");
        assert_eq!(resolve_path_target("~/n.md", "/Users/sam/code/app"), "/Users/sam/n.md");
        assert_eq!(resolve_path_target("~/n.md", "/srv/app"), "~/n.md");
        assert_eq!(resolve_path_target(r"src\a.rs", r"C:\Users\sam\app"), r"C:\Users\sam\app\src\a.rs");
        assert_eq!(resolve_path_target("../b.rs", "/w/a/"), "/w/a/../b.rs");
    }

    #[test]
    fn only_the_platforms_modifier_activates_a_link() {
        assert!(is_activation(true, false, true) && !is_activation(false, true, true));
        assert!(is_activation(false, true, false) && !is_activation(true, false, false) && !is_activation(false, false, false));
    }

    struct Screen(Vec<(Vec<String>, bool)>);
    impl Cells for Screen {
        fn rows(&self) -> usize {
            self.0.len()
        }
        fn cells(&self, row: usize) -> Vec<&str> {
            self.0[row].0.iter().map(String::as_str).collect()
        }
        fn continues_previous(&self, row: usize) -> bool {
            row > 0 && self.0[row - 1].1
        }
        fn continues_next(&self, row: usize) -> bool {
            self.0[row].1
        }
    }
    fn line(text: &str, width: usize, wraps: bool) -> (Vec<String>, bool) {
        let mut cells: Vec<String> = text.chars().map(|character| character.to_string()).collect();
        cells.resize(width, String::new());
        (cells, wraps)
    }

    #[test]
    fn a_link_that_wraps_is_found_whole_from_either_row() {
        let screen = Screen(vec![line("open https://exa", 16, true), line("mple.com/path now", 17, false)]);
        for (column, row) in [(10, 0), (3, 1)] {
            let hit = link_at(&screen, column, row).unwrap();
            assert_eq!((hit.kind, hit.text.as_str(), hit.start, hit.end), (Kind::Url, "https://example.com/path", (5, 0), (12, 1)));
        }
        assert!(link_at(&screen, 1, 0).is_none(), "the word before it is not a link");
        assert!(link_at(&screen, 14, 1).is_none());
        assert!(link_at(&screen, 0, 5).is_none());
    }
}
