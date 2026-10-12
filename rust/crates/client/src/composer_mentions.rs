//! The files a prompt names: ports of `collectMentionTokens` (`composerInlineTokens.ts`), the file chip's kind of entry
//! (`inferEntryKindFromPath`, `pierre-icons.ts`) and the removal of a chip from the text.
//!
//! The original's composer does not write a context record when an `@` path is chosen: it writes the canonical file link, `[name](path)`
//! (`serializeComposerFileLink`, here `file_tree::mention`), and the editor draws a link like that, and a bare `@path` before a
//! space, as a chip. A sent message draws a file link as a chip too (`FileTagChip`). The chips this app's composer draws are the ones
//! `mentions` finds in the text.
use regex::Regex;
use std::sync::OnceLock;

/// A file named in a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// The path the text names, decoded.
    pub path: String,
    /// The text that names it, as typed.
    pub source: String,
    pub start: usize,
    pub end: usize,
}

/// The longest label a file link is searched for, so that a run of `[` cannot make the search quadratic (`MAX_FILE_LINK_LABEL_LENGTH`).
const MAX_LABEL: usize = 512;

fn file_link() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(&format!(r"(^|\s)\[((?:\\.|[^\]\\]){{0,{MAX_LABEL}}})\]\(([^)\s]+)\)")).expect("file link pattern"))
}
fn at_mention() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r#"(^|\s)@(?:"((?:\\.|[^"\\])*)"|([^\s@"]+))"#).expect("mention pattern"))
}
fn scoped_package() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r#"^[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*(?:/[^\s@"]+)*$"#).expect("package pattern"))
}

/// Drops the backslash that escapes the character after it (`replace(/\\(.)/g, "$1")`).
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            if let Some(value) = std::str::from_utf8(&bytes[at + 1..at + 3]).ok().and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(value);
                at += 3;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    // `decodeURIComponent` throws on text that is not UTF-8; the original then keeps the source as it is.
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

/// The last component of a path, either separator.
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Whether the text starts with a URI scheme that is not a Windows drive (`https:`, `mailto:`).
fn has_external_scheme(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\');
    let scheme = path.split_once(':').is_some_and(|(scheme, _)| scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')));
    scheme && !drive
}

/// `collectMentionTokens`: the file links whose label is the file's own name, then the `@path` and `@"quoted path"` words, each followed
/// by whitespace (a half-typed one is still prose); in the order they stand, and where two overlap the first.
pub fn mentions(text: &str) -> Vec<Mention> {
    let mut found: Vec<Mention> = vec![];
    let followed = |end: usize| text[end..].chars().next().is_some_and(char::is_whitespace);
    for captures in file_link().captures_iter(text) {
        let whole = captures.get(0).expect("whole match");
        let prefix = captures.get(1).map_or(0, |prefix| prefix.len());
        if !followed(whole.end()) {
            continue;
        }
        let label = unescape(captures.get(2).map_or("", |label| label.as_str()));
        let path = percent_decode(captures.get(3).map_or("", |path| path.as_str()));
        if path.is_empty() || has_external_scheme(&path) || label != basename(&path) {
            continue;
        }
        let start = whole.start() + prefix;
        found.push(Mention { path, source: text[start..whole.end()].to_owned(), start, end: whole.end() });
    }
    for captures in at_mention().captures_iter(text) {
        let whole = captures.get(0).expect("whole match");
        let prefix = captures.get(1).map_or(0, |prefix| prefix.len());
        if !followed(whole.end()) {
            continue;
        }
        let quoted = captures.get(2).map(|quoted| unescape(quoted.as_str()));
        let path = quoted.clone().unwrap_or_else(|| captures.get(3).map_or(String::new(), |path| path.as_str().to_owned()));
        if path.is_empty() || (quoted.is_none() && scoped_package().is_match(&path)) {
            continue;
        }
        let start = whole.start() + prefix;
        found.push(Mention { path, source: text[start..whole.end()].to_owned(), start, end: whole.end() });
    }
    found.sort_by_key(|mention| mention.start);
    let mut kept: Vec<Mention> = vec![];
    for mention in found {
        if kept.last().is_none_or(|last| mention.start >= last.end) {
            kept.push(mention);
        }
    }
    kept
}

/// The prompt without a chip: the text that names it, and the single space after it when the text before is a boundary too, so that
/// removing a chip does not leave a doubled space.
pub fn remove(prompt: &str, mention: &Mention) -> String {
    let before = &prompt[..mention.start];
    let mut after = &prompt[mention.end..];
    if (before.is_empty() || before.ends_with(char::is_whitespace)) && after.starts_with(' ') {
        after = &after[1..];
    }
    format!("{before}{after}")
}

/// `inferEntryKindFromPath`: a name that starts with a dot and has no other, or has no dot at all, is taken for a folder.
pub fn is_folder(path: &str) -> bool {
    let base = basename(path);
    if base.starts_with('.') && !base[1..].contains('.') {
        return true;
    }
    !base.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(text: &str) -> Vec<String> {
        mentions(text).into_iter().map(|mention| mention.path).collect()
    }

    #[test]
    fn a_file_link_with_the_files_own_name_and_a_bare_at_word_before_a_space_are_mentions() {
        assert_eq!(paths("look at [App.tsx](src/App.tsx) and @README.md now"), ["src/App.tsx", "README.md"]);
        let text = "[App.tsx](src/App.tsx) ";
        let found = &mentions(text)[0];
        assert_eq!((found.start, found.end, found.source.as_str()), (0, 22, "[App.tsx](src/App.tsx)"));
        // The canonical link the picker writes, with the characters it escapes.
        assert_eq!(paths("see [a b.txt](dir/a%20b.txt) "), ["dir/a b.txt"]);
        assert_eq!(paths(r#"see [x\[1\].md](docs/x%5B1%5D.md) "#), ["docs/x[1].md"]);
        // A quoted word names a path with spaces.
        assert_eq!(paths(r#"open @"my dir/a file.rs" please"#), ["my dir/a file.rs"]);
    }

    #[test]
    fn prose_that_only_looks_like_a_mention_is_not_one() {
        // Half typed: no whitespace after it yet.
        assert!(mentions("look at @src/ma").is_empty() && mentions("[App.tsx](src/App.tsx)").is_empty());
        // A label that is not the file's name, a web link, a mail address, a scoped package.
        assert!(mentions("see [the app](src/App.tsx) ").is_empty());
        assert!(mentions("see [docs](https://example.com/docs) ").is_empty());
        assert!(mentions("ask a@b.com now").is_empty());
        assert!(mentions("install @types/node now").is_empty() && mentions("install @scope/pkg/deep now").is_empty());
        assert_eq!(paths("install @\"@types/node\" now"), ["@types/node"], "quoted, it is a path");
        // A Windows drive is not a scheme.
        assert_eq!(paths(r"[a.rs](C:\work\a.rs) "), [r"C:\work\a.rs"]);
        assert_eq!(paths("a @x.rs b @y.rs c"), ["x.rs", "y.rs"]);
    }

    #[test]
    fn removing_a_chip_takes_its_text_and_one_space_and_never_doubles_one() {
        let text = "read [a.rs](a.rs) then @b.rs now";
        let first = &mentions(text)[0];
        assert_eq!(remove(text, first), "read then @b.rs now");
        let second = &mentions(text)[1];
        assert_eq!(remove(text, second), "read [a.rs](a.rs) then now");
        assert_eq!(remove("@a.rs hello", &mentions("@a.rs hello")[0]), "hello");
        assert_eq!(remove("x@a.rs hello", &Mention { path: "a.rs".into(), source: "@a.rs".into(), start: 1, end: 6 }), "x hello", "after text it only closes the gap");
    }

    #[test]
    fn a_name_with_no_dot_is_taken_for_a_folder() {
        assert!(is_folder("src/components") && is_folder(".github") && is_folder("Makefile"));
        assert!(!is_folder("src/App.tsx") && !is_folder(".env.local") && !is_folder("a/.eslintrc.json"));
    }
}
