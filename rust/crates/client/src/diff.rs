//! Unified git patch parsing for the diff view. The original delegates this to a JS library
//! (`@pierre/diffs`); the panel only needs files, hunks and numbered lines, so the parser is
//! small and tolerant: unknown header lines are skipped and a patch it cannot read as files
//! falls back to raw text, as `getRenderablePatch` does.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Addition,
    Deletion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_number: Option<u32>,
    pub new_number: Option<u32>,
    /// The line without its marker. A trailing `\r` is kept, so CRLF files render faithfully.
    pub text: String,
    /// The `\ No newline at end of file` marker followed this line.
    pub no_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// The text after the closing `@@`, usually the enclosing function.
    pub section: String,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    pub fn header(&self) -> String {
        let range = |start: u32, count: u32| {
            if count == 1 {
                start.to_string()
            } else {
                format!("{start},{count}")
            }
        };
        let mut header = format!(
            "@@ -{} +{} @@",
            range(self.old_start, self.old_count),
            range(self.new_start, self.new_count)
        );
        if !self.section.is_empty() {
            header.push(' ');
            header.push_str(&self.section);
        }
        header
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    /// `(old mode, new mode)` when the file mode changed.
    pub mode_change: Option<(String, String)>,
    /// Rename or copy similarity, in percent.
    pub similarity: Option<u8>,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
}

impl FileDiff {
    fn empty() -> Self {
        Self {
            old_path: None,
            new_path: None,
            status: FileStatus::Modified,
            binary: false,
            mode_change: None,
            similarity: None,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        }
    }

    /// The path the file is known by after the change; a deleted file keeps its old path.
    pub fn path(&self) -> &str {
        match self.status {
            FileStatus::Deleted => self.old_path.as_deref().or(self.new_path.as_deref()),
            _ => self.new_path.as_deref().or(self.old_path.as_deref()),
        }
        .unwrap_or("")
    }

    /// Where a renamed or copied file came from.
    pub fn previous_path(&self) -> Option<&str> {
        match self.status {
            FileStatus::Renamed | FileStatus::Copied => self
                .old_path
                .as_deref()
                .filter(|old| Some(*old) != self.new_path.as_deref()),
            _ => None,
        }
    }

    /// Rows the file takes in the stacked view: one per line plus one header per hunk.
    pub fn row_count(&self) -> usize {
        self.hunks.iter().map(|hunk| hunk.lines.len() + 1).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stat {
    pub additions: u64,
    pub deletions: u64,
}

pub fn total_stat(files: &[FileDiff]) -> Stat {
    files.iter().fold(Stat::default(), |total, file| Stat {
        additions: total.additions + u64::from(file.additions),
        deletions: total.deletions + u64::from(file.deletions),
    })
}

/// `formatCompactDiffCount`: 999, 1.2k, 12k, 3.4m.
pub fn compact_count(value: u64) -> String {
    fn scaled(value: u64, unit: u64, suffix: &str) -> String {
        let scaled = value as f64 / unit as f64;
        if scaled < 10.0 {
            let text = format!("{scaled:.1}");
            format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text))
        } else {
            format!("{}{suffix}", scaled.round() as u64)
        }
    }
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => scaled(value, 1_000, "k"),
        1_000_000..=999_999_999 => scaled(value, 1_000_000, "m"),
        _ => scaled(value, 1_000_000_000, "b"),
    }
}

/// What the panel shows for a non-empty patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Renderable {
    Files(Vec<FileDiff>),
    Raw { text: String, reason: &'static str },
}

/// `getRenderablePatch`: `None` for an empty patch, files when any parse, otherwise the text.
pub fn renderable(patch: &str) -> Option<Renderable> {
    let trimmed = patch.trim();
    if trimmed.is_empty() {
        return None;
    }
    let files = parse(trimmed);
    Some(if files.is_empty() {
        Renderable::Raw {
            text: trimmed.to_owned(),
            reason: "Unsupported diff format. Showing raw patch.",
        }
    } else {
        Renderable::Files(files)
    })
}

#[derive(Default)]
struct Parser {
    files: Vec<FileDiff>,
    file: Option<FileDiff>,
    /// The file came from a `diff --git` header, so `a/` and `b/` prefixes are git's.
    git: bool,
    saw_old_header: bool,
    hunk: Option<Hunk>,
    old_left: u32,
    new_left: u32,
    old_number: u32,
    new_number: u32,
    binary_patch: bool,
}

/// Parses a unified patch, in `git diff` form or plain `diff -u` form, into files.
pub fn parse(patch: &str) -> Vec<FileDiff> {
    let mut parser = Parser::default();
    for raw in patch.split('\n') {
        parser.line(raw);
    }
    parser.finish_file();
    parser.files
}

impl Parser {
    fn line(&mut self, raw: &str) {
        if self.binary_patch {
            if !raw.starts_with("diff --git ") {
                return;
            }
            self.binary_patch = false;
        }
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            self.finish_file();
            let mut file = FileDiff::empty();
            (file.old_path, file.new_path) = git_header_paths(rest.trim_end_matches('\r'));
            self.file = Some(file);
            self.git = true;
            return;
        }
        if self.hunk.is_some() && (self.old_left > 0 || self.new_left > 0) {
            self.content(raw);
            return;
        }
        let line = raw.trim_end_matches('\r');
        if line.starts_with("@@ ") {
            self.start_hunk(line);
        } else if line.starts_with('\\') {
            if let Some(last) = self.hunk.as_mut().and_then(|hunk| hunk.lines.last_mut()) {
                last.no_newline = true;
            }
        } else {
            self.header(line);
        }
    }

    fn content(&mut self, raw: &str) {
        let hunk = self.hunk.as_mut().expect("content follows a hunk header");
        if raw.starts_with('\\') {
            if let Some(last) = hunk.lines.last_mut() {
                last.no_newline = true;
            }
            return;
        }
        let (kind, text) = match raw.as_bytes().first() {
            Some(b'+') => (LineKind::Addition, &raw[1..]),
            Some(b'-') => (LineKind::Deletion, &raw[1..]),
            Some(b' ') => (LineKind::Context, &raw[1..]),
            // Editors and mailers strip the single space of an empty context line.
            _ => (LineKind::Context, raw),
        };
        let old_number = (kind != LineKind::Addition).then(|| {
            let number = self.old_number;
            self.old_number += 1;
            number
        });
        let new_number = (kind != LineKind::Deletion).then(|| {
            let number = self.new_number;
            self.new_number += 1;
            number
        });
        if kind != LineKind::Addition {
            self.old_left = self.old_left.saturating_sub(1);
        }
        if kind != LineKind::Deletion {
            self.new_left = self.new_left.saturating_sub(1);
        }
        hunk.lines.push(DiffLine {
            kind,
            old_number,
            new_number,
            text: text.to_owned(),
            no_newline: false,
        });
    }

    fn start_hunk(&mut self, line: &str) {
        let Some((old, new, section)) = parse_hunk_header(line) else {
            return;
        };
        self.push_hunk();
        if self.file.is_none() {
            // A hunk with no file header at all is still worth showing.
            self.file = Some(FileDiff::empty());
        }
        self.old_left = old.1;
        self.new_left = new.1;
        self.old_number = old.0;
        self.new_number = new.0;
        self.hunk = Some(Hunk {
            old_start: old.0,
            old_count: old.1,
            new_start: new.0,
            new_count: new.1,
            section,
            lines: Vec::new(),
        });
    }

    fn push_hunk(&mut self) {
        if let (Some(hunk), Some(file)) = (self.hunk.take(), self.file.as_mut()) {
            file.hunks.push(hunk);
        }
        self.old_left = 0;
        self.new_left = 0;
    }

    fn finish_file(&mut self) {
        self.push_hunk();
        let Some(mut file) = self.file.take() else {
            return;
        };
        self.saw_old_header = false;
        match file.status {
            FileStatus::Added => file.old_path = None,
            FileStatus::Deleted => file.new_path = None,
            _ => {}
        }
        for hunk in &file.hunks {
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Addition => file.additions += 1,
                    LineKind::Deletion => file.deletions += 1,
                    LineKind::Context => {}
                }
            }
        }
        // Text between `diff --git` and the next one that says nothing about a file is noise.
        if file.old_path.is_some() || file.new_path.is_some() || !file.hunks.is_empty() {
            self.files.push(file);
        }
    }

    fn header(&mut self, line: &str) {
        if let Some(path) = line.strip_prefix("--- ") {
            // Plain `diff -u` output has no `diff --git` line, so this starts the next file.
            if self.file.as_ref().is_none_or(|file| {
                !file.hunks.is_empty() || self.hunk.is_some() || self.saw_old_header
            }) {
                self.finish_file();
                self.file = Some(FileDiff::empty());
                self.git = false;
            }
            self.saw_old_header = true;
            let old = header_path(path, self.git, "a/");
            let file = self.file.as_mut().expect("file started above");
            if old.is_none() && file.new_path.is_some() && self.git {
                file.status = FileStatus::Added;
            }
            if self.git || old.is_some() {
                file.old_path = old;
            }
            return;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            let Some(file) = self.file.as_mut() else {
                return;
            };
            let new = header_path(path, self.git, "b/");
            if new.is_none() {
                file.status = FileStatus::Deleted;
            } else if file.old_path.is_none() && file.status == FileStatus::Modified {
                file.status = FileStatus::Added;
            }
            if self.git || new.is_some() {
                file.new_path = new;
            }
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if line.starts_with("new file mode ") {
            file.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode ") {
            file.status = FileStatus::Deleted;
        } else if let Some(mode) = line.strip_prefix("old mode ") {
            file.mode_change = Some((mode.trim().to_owned(), String::new()));
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            if let Some((_, new)) = file.mode_change.as_mut() {
                *new = mode.trim().to_owned();
            }
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = header_path(path, false, "");
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.status = FileStatus::Renamed;
            file.new_path = header_path(path, false, "");
        } else if let Some(path) = line.strip_prefix("copy from ") {
            file.status = FileStatus::Copied;
            file.old_path = header_path(path, false, "");
        } else if let Some(path) = line.strip_prefix("copy to ") {
            file.status = FileStatus::Copied;
            file.new_path = header_path(path, false, "");
        } else if let Some(percent) = line.strip_prefix("similarity index ") {
            file.similarity = percent.trim().trim_end_matches('%').parse().ok();
        } else if line.starts_with("GIT binary patch") {
            file.binary = true;
            self.binary_patch = true;
        } else if line.starts_with("Binary files ") && line.ends_with(" differ") {
            file.binary = true;
        }
    }
}

/// `@@ -a[,b] +c[,d] @@ section`; an omitted count is one.
fn parse_hunk_header(line: &str) -> Option<((u32, u32), (u32, u32), String)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, section) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |text: &str| -> Option<(u32, u32)> {
        match text.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((text.parse().ok()?, 1)),
        }
    };
    Some((range(old)?, range(new)?, section.trim().to_owned()))
}

/// A path from a `---`, `+++`, `rename` or `copy` line. `/dev/null` is no path.
fn header_path(text: &str, git: bool, prefix: &str) -> Option<String> {
    // Plain `diff -u` appends a tab and a timestamp.
    let text = text.split('\t').next().unwrap_or(text);
    let text = if text.starts_with('"') {
        unquote(text).0
    } else {
        text.to_owned()
    };
    if text == "/dev/null" {
        return None;
    }
    if !prefix.is_empty() {
        if let Some(stripped) = text.strip_prefix(prefix) {
            if git || !stripped.is_empty() {
                return Some(stripped.to_owned());
            }
        }
    }
    Some(text)
}

/// The two paths of `diff --git a/x b/y`. They are ambiguous when a name holds a space, so an
/// unquoted pair is split where both halves name the same file; renames are corrected by the
/// `rename from` and `rename to` lines that follow.
fn git_header_paths(rest: &str) -> (Option<String>, Option<String>) {
    let strip = |path: &str, prefix: &str| path.strip_prefix(prefix).unwrap_or(path).to_owned();
    if rest.starts_with('"') {
        let (first, remainder) = unquote(rest);
        let remainder = remainder.trim_start();
        let second = if remainder.starts_with('"') {
            unquote(remainder).0
        } else {
            remainder.to_owned()
        };
        return (Some(strip(&first, "a/")), Some(strip(&second, "b/")));
    }
    for (index, _) in rest.match_indices(' ') {
        let (left, right) = (&rest[..index], &rest[index + 1..]);
        if let (Some(old), Some(new)) = (left.strip_prefix("a/"), right.strip_prefix("b/")) {
            if old == new {
                return (Some(old.to_owned()), Some(new.to_owned()));
            }
        }
    }
    match rest.find(" b/") {
        Some(index) => (
            Some(strip(&rest[..index], "a/")),
            Some(strip(&rest[index + 1..], "b/")),
        ),
        None => (None, None),
    }
}

/// Git's C-style quoting: `"caf\303\251"`, `\t`, `\n`, `\"`, `\\`. Returns the decoded text and
/// what follows the closing quote.
fn unquote(text: &str) -> (String, &str) {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index += 1;
                break;
            }
            b'\\' if index + 1 < bytes.len() => {
                index += 1;
                match bytes[index] {
                    b'n' => decoded.push(b'\n'),
                    b't' => decoded.push(b'\t'),
                    b'r' => decoded.push(b'\r'),
                    b'a' => decoded.push(7),
                    b'b' => decoded.push(8),
                    b'f' => decoded.push(12),
                    b'v' => decoded.push(11),
                    digit @ b'0'..=b'7' => {
                        let mut value = u32::from(digit - b'0');
                        let mut taken = 1;
                        while taken < 3 && index + 1 < bytes.len() {
                            match bytes[index + 1] {
                                next @ b'0'..=b'7' => {
                                    value = value * 8 + u32::from(next - b'0');
                                    index += 1;
                                    taken += 1;
                                }
                                _ => break,
                            }
                        }
                        decoded.push(value as u8);
                    }
                    other => decoded.push(other),
                }
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    (
        String::from_utf8_lossy(&decoded).into_owned(),
        &text[index.min(text.len())..],
    )
}

/// One row of the split view: the old side and the new side of the same position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitRow<'a> {
    pub old: Option<&'a DiffLine>,
    pub new: Option<&'a DiffLine>,
}

/// Pairs a hunk's lines for side-by-side display: context on both sides, and each run of
/// deletions beside the additions that follow it.
pub fn split_rows(lines: &[DiffLine]) -> Vec<SplitRow<'_>> {
    let mut rows = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        let line = &lines[index];
        if line.kind == LineKind::Context {
            rows.push(SplitRow {
                old: Some(line),
                new: Some(line),
            });
            index += 1;
            continue;
        }
        let deletions_end = index
            + lines[index..]
                .iter()
                .take_while(|line| line.kind == LineKind::Deletion)
                .count();
        let additions_end = deletions_end
            + lines[deletions_end..]
                .iter()
                .take_while(|line| line.kind == LineKind::Addition)
                .count();
        let (deletions, additions) = (&lines[index..deletions_end], &lines[deletions_end..additions_end]);
        for position in 0..deletions.len().max(additions.len()) {
            rows.push(SplitRow {
                old: deletions.get(position),
                new: additions.get(position),
            });
        }
        index = additions_end;
    }
    rows
}

/// A row of the visible window, in the layout being drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row<'a> {
    Line(&'a DiffLine),
    Pair(SplitRow<'a>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkRows<'a> {
    pub index: usize,
    pub hunk: &'a Hunk,
    pub rows: Vec<Row<'a>>,
    /// How many of the hunk's lines the rows cover (a split row can cover two).
    pub lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window<'a> {
    pub hunks: Vec<HunkRows<'a>>,
    /// Rows drawn, hunk headers included, and rows in the whole file.
    pub shown: usize,
    pub total: usize,
}

/// The first `limit` rows of a file in the stacked or split layout. Large files are drawn a
/// window at a time, so the cost of showing a file follows what is on screen, not its size.
pub fn window(file: &FileDiff, split: bool, limit: usize) -> Window<'_> {
    let mut hunks = Vec::new();
    let mut shown = 0;
    let mut total = 0;
    for (index, hunk) in file.hunks.iter().enumerate() {
        if shown >= limit {
            // Past the window only the size matters, and the stacked size needs no pairing.
            total += 1 + if split { split_rows(&hunk.lines).len() } else { hunk.lines.len() };
            continue;
        }
        let room = limit - shown - 1;
        let mut rows: Vec<Row<'_>> = if split {
            split_rows(&hunk.lines).into_iter().map(Row::Pair).collect()
        } else {
            hunk.lines.iter().map(Row::Line).collect()
        };
        total += 1 + rows.len();
        rows.truncate(room);
        shown += 1 + rows.len();
        let lines = rows
            .iter()
            .map(|row| match row {
                Row::Line(_) => 1,
                Row::Pair(pair) => usize::from(pair.old.is_some()) + usize::from(pair.new.is_some()) - usize::from(pair.old.is_some_and(|line| line.kind == LineKind::Context)),
            })
            .sum();
        hunks.push(HunkRows { index, hunk, rows, lines });
    }
    Window { hunks, shown, total }
}

/// The text the highlighter reads for a file: the old side and the new side as two documents,
/// hunks joined in order. Only the first `budget` lines of the file are included, so the
/// tokenizer's work is bounded however large the diff is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Documents {
    pub old: String,
    pub new: String,
    old_lines: usize,
    new_lines: usize,
    hunks: Vec<HunkStart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HunkStart {
    old_offset: usize,
    new_offset: usize,
    old_start: u32,
    new_start: u32,
}

impl Documents {
    pub fn build(file: &FileDiff, hunks: usize, budget: usize) -> Self {
        let mut documents = Self::default();
        let mut remaining = budget;
        for hunk in file.hunks.iter().take(hunks) {
            documents.hunks.push(HunkStart {
                old_offset: documents.old_lines,
                new_offset: documents.new_lines,
                old_start: hunk.old_start,
                new_start: hunk.new_start,
            });
            for line in hunk.lines.iter().take(remaining) {
                if line.kind != LineKind::Addition {
                    documents.old.push_str(&line.text);
                    documents.old.push('\n');
                    documents.old_lines += 1;
                }
                if line.kind != LineKind::Deletion {
                    documents.new.push_str(&line.text);
                    documents.new.push('\n');
                    documents.new_lines += 1;
                }
            }
            remaining = remaining.saturating_sub(hunk.lines.len());
        }
        documents
    }

    /// Where a line sits in the old document; `None` when the budget did not reach it.
    pub fn old_index(&self, hunk: usize, line: &DiffLine) -> Option<usize> {
        let start = self.hunks.get(hunk)?;
        let index = start.old_offset + line.old_number?.checked_sub(start.old_start)? as usize;
        (index < self.old_lines).then_some(index)
    }

    pub fn new_index(&self, hunk: usize, line: &DiffLine) -> Option<usize> {
        let start = self.hunks.get(hunk)?;
        let index = start.new_offset + line.new_number?.checked_sub(start.new_start)? as usize;
        (index < self.new_lines).then_some(index)
    }
}

/// The grammar name the syntax highlighter knows a path by: its extension, or a few well-known
/// file names. Unknown names simply highlight as plain text.
pub fn language_for_path(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        "Dockerfile" => return "dockerfile".into(),
        "Makefile" | "makefile" | "GNUmakefile" => return "make".into(),
        _ => {}
    }
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension.to_ascii_lowercase(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(patch: &str) -> FileDiff {
        let mut files = parse(patch);
        assert_eq!(files.len(), 1, "{files:#?}");
        files.remove(0)
    }

    const MODIFIED: &str = "diff --git a/src/lib.rs b/src/lib.rs\nindex 83db48f..bf2a7c1 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,4 @@ fn main\n keep\n-old\n+new\n+extra\n tail\n@@ -20 +21 @@\n-x\n+y\n";

    #[test]
    fn a_modified_file_has_numbered_hunks_and_counts() {
        let file = one(MODIFIED);
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!((file.path(), file.previous_path()), ("src/lib.rs", None));
        assert_eq!((file.additions, file.deletions), (3, 2));
        assert_eq!(file.hunks.len(), 2);
        let first = &file.hunks[0];
        assert_eq!((first.old_start, first.old_count, first.new_start, first.new_count), (1, 3, 1, 4));
        assert_eq!(first.section, "fn main");
        assert_eq!(first.header(), "@@ -1,3 +1,4 @@ fn main");
        let numbers: Vec<_> = first
            .lines
            .iter()
            .map(|line| (line.kind, line.old_number, line.new_number, line.text.as_str()))
            .collect();
        assert_eq!(
            numbers,
            [
                (LineKind::Context, Some(1), Some(1), "keep"),
                (LineKind::Deletion, Some(2), None, "old"),
                (LineKind::Addition, None, Some(2), "new"),
                (LineKind::Addition, None, Some(3), "extra"),
                (LineKind::Context, Some(3), Some(4), "tail"),
            ]
        );
        // An omitted count means one line, and the header prints it back the same way.
        assert_eq!((file.hunks[1].old_count, file.hunks[1].new_count), (1, 1));
        assert_eq!(file.hunks[1].header(), "@@ -20 +21 @@");
        assert_eq!(file.row_count(), 5 + 1 + 2 + 1);
    }

    #[test]
    fn new_and_deleted_files_drop_the_missing_side() {
        let added = one("diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..e69de29\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n");
        assert_eq!((added.status, added.old_path.as_deref(), added.path()), (FileStatus::Added, None, "new.txt"));
        assert_eq!((added.additions, added.deletions), (2, 0));
        assert_eq!(added.hunks[0].lines[1].new_number, Some(2));

        let deleted = one("diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\nindex e69de29..0000000\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n");
        assert_eq!((deleted.status, deleted.new_path.as_deref(), deleted.path()), (FileStatus::Deleted, None, "gone.txt"));
        assert_eq!((deleted.additions, deleted.deletions), (0, 1));
    }

    #[test]
    fn empty_new_and_deleted_files_have_no_hunks() {
        let files = parse("diff --git a/empty b/empty\nnew file mode 100644\nindex 0000000..e69de29\ndiff --git a/old b/old\ndeleted file mode 100644\nindex e69de29..0000000\n");
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].status, files[0].path(), files[0].hunks.len()), (FileStatus::Added, "empty", 0));
        assert_eq!((files[1].status, files[1].path(), files[1].hunks.len()), (FileStatus::Deleted, "old", 0));
    }

    #[test]
    fn renames_keep_both_paths_with_or_without_content_changes() {
        let pure = one("diff --git a/before.rs b/after.rs\nsimilarity index 100%\nrename from before.rs\nrename to after.rs\n");
        assert_eq!((pure.status, pure.path(), pure.previous_path(), pure.similarity), (FileStatus::Renamed, "after.rs", Some("before.rs"), Some(100)));
        assert!(pure.hunks.is_empty());

        let changed = one("diff --git a/dir/a.rs b/dir/b.rs\nsimilarity index 80%\nrename from dir/a.rs\nrename to dir/b.rs\nindex 111..222 100644\n--- a/dir/a.rs\n+++ b/dir/b.rs\n@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!((changed.status, changed.path(), changed.previous_path()), (FileStatus::Renamed, "dir/b.rs", Some("dir/a.rs")));
        assert_eq!((changed.additions, changed.deletions), (1, 1));
    }

    #[test]
    fn copies_and_mode_changes() {
        let copy = one("diff --git a/a b/b\nsimilarity index 100%\ncopy from a\ncopy to b\n");
        assert_eq!((copy.status, copy.path(), copy.previous_path()), (FileStatus::Copied, "b", Some("a")));
        let mode = one("diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n");
        assert_eq!(mode.status, FileStatus::Modified);
        assert_eq!(mode.mode_change, Some(("100644".into(), "100755".into())));
        assert!(mode.hunks.is_empty());
    }

    #[test]
    fn binary_files_are_marked_and_never_swallow_the_next_file() {
        let files = parse("diff --git a/logo.png b/logo.png\nindex 111..222 100644\nBinary files a/logo.png and b/logo.png differ\ndiff --git a/data.bin b/data.bin\nnew file mode 100644\nindex 0000000..abc\nGIT binary patch\nliteral 12\nzc$@(6wlpDiff --git x\n\nliteral 0\nHcmV?d00001\n\ndiff --git a/after.txt b/after.txt\n--- a/after.txt\n+++ b/after.txt\n@@ -1 +1 @@\n-1\n+2\n");
        assert_eq!(files.iter().map(|file| (file.path(), file.binary, file.hunks.len())).collect::<Vec<_>>(), [("logo.png", true, 0), ("data.bin", true, 0), ("after.txt", false, 1)]);
        assert_eq!(files[1].status, FileStatus::Added);
        assert_eq!((files[2].additions, files[2].deletions), (1, 1));
    }

    #[test]
    fn the_no_newline_marker_attaches_to_the_line_before_it() {
        let file = one("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n same\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n");
        let lines = &file.hunks[0].lines;
        assert_eq!(lines.iter().map(|line| line.no_newline).collect::<Vec<_>>(), [false, true, true]);
        assert_eq!((file.additions, file.deletions), (1, 1));
    }

    #[test]
    fn content_that_looks_like_headers_stays_content() {
        // A deleted line "-- a/x" is "--- a/x" on the wire, and an added "++ b/x" is "+++ b/x".
        let patch = "diff --git a/f.sql b/f.sql\n--- a/f.sql\n+++ b/f.sql\n@@ -1 +1 @@\n--- a/x\n+++ b/x\n";
        let file = one(patch);
        assert_eq!(file.path(), "f.sql");
        assert_eq!(file.hunks[0].lines.iter().map(|line| (line.kind, line.text.as_str())).collect::<Vec<_>>(), [(LineKind::Deletion, "-- a/x"), (LineKind::Addition, "++ b/x")]);
        // A line that merely starts like a file header cannot end the hunk early either.
        let file = one("diff --git a/g b/g\n--- a/g\n+++ b/g\n@@ -1,3 +1,3 @@\n a\n-diff --git a/z b/z\n+@@ -9 +9 @@\n b\n");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines.len(), 4);
    }

    #[test]
    fn blank_context_lines_survive_editors_that_strip_the_space() {
        let file = one("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n");
        let lines = &file.hunks[0].lines;
        assert_eq!((lines[1].kind, lines[1].text.as_str(), lines[1].old_number, lines[1].new_number), (LineKind::Context, "", Some(2), Some(2)));
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn crlf_is_kept_in_content_and_ignored_in_headers() {
        let file = one("diff --git a/f b/f\r\n--- a/f\r\n+++ b/f\r\n@@ -1 +1 @@\r\n-a\r\n+b\r\n");
        assert_eq!(file.path(), "f");
        assert_eq!(file.hunks[0].lines[0].text, "a\r");
    }

    #[test]
    fn quoted_and_spaced_paths_decode() {
        let spaced = one("diff --git a/my docs/read me.md b/my docs/read me.md\n--- a/my docs/read me.md\n+++ b/my docs/read me.md\n@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(spaced.path(), "my docs/read me.md");
        let quoted = one("diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\nnew file mode 100644\n--- /dev/null\n+++ \"b/caf\\303\\251.txt\"\n@@ -0,0 +1 @@\n+x\n");
        assert_eq!(quoted.path(), "café.txt");
        // Without hunks the header is all there is, and a rename's own lines correct it.
        let renamed = one("diff --git a/a b.txt b/c d.txt\nsimilarity index 100%\nrename from a b.txt\nrename to c d.txt\n");
        assert_eq!((renamed.previous_path(), renamed.path()), (Some("a b.txt"), "c d.txt"));
        assert_eq!(unquote("\"a\\tb\\\"c\\\\\" rest"), ("a\tb\"c\\".to_owned(), " rest"));
    }

    #[test]
    fn plain_unified_diffs_without_git_headers_parse_as_several_files() {
        let files = parse("--- old/a.txt\t2020-01-01 00:00:00\n+++ new/a.txt\t2020-01-02 00:00:00\n@@ -1 +1 @@\n-a\n+b\n--- /dev/null\n+++ new/b.txt\n@@ -0,0 +1 @@\n+c\n");
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].path(), files[0].status), ("new/a.txt", FileStatus::Modified));
        assert_eq!((files[1].path(), files[1].status, files[1].old_path.as_deref()), ("new/b.txt", FileStatus::Added, None));
    }

    #[test]
    fn a_lone_hunk_still_renders_and_prose_does_not() {
        assert_eq!(parse("@@ -1 +1 @@\n-a\n+b\n").len(), 1);
        assert!(parse("this is not a patch\nat all\n").is_empty());
        assert!(parse("").is_empty());
    }

    #[test]
    fn a_truncated_hunk_ends_at_the_next_file() {
        let files = parse("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,10 +1,10 @@\n-x\n+y\ndiff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-1\n+2\n");
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].additions, files[1].additions), (1, 1));
    }

    #[test]
    fn renderable_distinguishes_empty_files_and_raw_text() {
        assert_eq!(renderable(" \n\n"), None);
        assert!(matches!(renderable(MODIFIED), Some(Renderable::Files(files)) if files.len() == 1));
        assert_eq!(
            renderable("not a patch"),
            Some(Renderable::Raw { text: "not a patch".into(), reason: "Unsupported diff format. Showing raw patch." })
        );
    }

    #[test]
    fn stats_sum_across_files_and_compact_like_the_original() {
        let files = parse(&format!("{MODIFIED}diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-1\n+2\n"));
        assert_eq!(total_stat(&files), Stat { additions: 4, deletions: 3 });
        assert_eq!([0, 999, 1000, 1234, 9999, 10_000, 12_499, 999_999, 1_000_000, 2_500_000, 3_000_000_000].map(compact_count), ["0", "999", "1k", "1.2k", "10k", "10k", "12k", "1000k", "1m", "2.5m", "3b"]);
    }

    #[test]
    fn split_rows_pair_deletions_with_the_additions_after_them() {
        let file = one("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,5 +1,5 @@\n ctx\n-a\n-b\n-c\n+x\n+y\n tail\n+only\n");
        let rows = split_rows(&file.hunks[0].lines);
        fn text(line: Option<&DiffLine>) -> Option<&str> {
            line.map(|line| line.text.as_str())
        }
        assert_eq!(
            rows.iter().map(|row| (text(row.old), text(row.new))).collect::<Vec<_>>(),
            [(Some("ctx"), Some("ctx")), (Some("a"), Some("x")), (Some("b"), Some("y")), (Some("c"), None), (Some("tail"), Some("tail")), (None, Some("only"))]
        );
        assert!(split_rows(&[]).is_empty());
    }

    #[test]
    fn languages_come_from_extensions_and_a_few_names() {
        assert_eq!(language_for_path("src/Main.RS"), "rs");
        assert_eq!(language_for_path("a/b/Dockerfile"), "dockerfile");
        assert_eq!(language_for_path("Makefile"), "make");
        assert_eq!(language_for_path(".gitignore"), "");
        assert_eq!(language_for_path("LICENSE"), "");
    }

    /// A file whose one hunk deletes and re-adds `count` lines.
    fn numbered(count: usize) -> FileDiff {
        let mut patch = format!("diff --git a/big b/big\n--- a/big\n+++ b/big\n@@ -1,{count} +1,{count} @@\n");
        for line in 0..count {
            patch.push_str(&format!("-old {line}\n+new {line}\n"));
        }
        one(&patch)
    }

    #[test]
    fn a_window_draws_only_the_first_rows_and_knows_the_whole_size() {
        let file = one(MODIFIED);
        let all = window(&file, false, 100);
        assert_eq!((all.shown, all.total, all.hunks.len()), (9, 9, 2));
        let first = window(&file, false, 4);
        assert_eq!((first.shown, first.total, first.hunks.len()), (4, 9, 1));
        assert_eq!((first.hunks[0].rows.len(), first.hunks[0].lines), (3, 3));
        // The header of the second hunk fits only once there is room for a row after it.
        let edge = window(&file, false, 6);
        assert_eq!((edge.shown, edge.hunks.len()), (6, 1));
        let more = window(&file, false, 7);
        assert_eq!((more.shown, more.hunks.len(), more.hunks[1].rows.len()), (7, 2, 0));
        assert_eq!(window(&file, false, 0).hunks.len(), 0);
    }

    #[test]
    fn a_huge_file_is_windowed_without_walking_every_row_into_the_result() {
        let file = numbered(30_000);
        let window = window(&file, false, 400);
        assert_eq!((window.shown, window.total), (400, 60_001));
        assert_eq!(window.hunks[0].rows.len(), 399);
        let split = super::window(&file, true, 400);
        assert_eq!((split.shown, split.total), (400, 30_001));
        assert_eq!(file.row_count(), 60_001);
    }

    #[test]
    fn split_windows_count_paired_rows() {
        let file = one(MODIFIED);
        let split = window(&file, true, 100);
        // keep | old->new | extra | tail, then the second hunk's pair.
        assert_eq!((split.shown, split.total), (1 + 4 + 1 + 1, 1 + 4 + 1 + 1));
        // Five lines in the first hunk fill four rows, and the pairing does not lose or repeat any.
        assert_eq!((split.hunks[0].lines, split.hunks[1].lines), (5, 2));
    }

    #[test]
    fn highlight_documents_hold_each_side_and_find_lines_by_number() {
        let file = one(MODIFIED);
        let documents = Documents::build(&file, 2, 100);
        assert_eq!(documents.old, "keep\nold\ntail\nx\n");
        assert_eq!(documents.new, "keep\nnew\nextra\ntail\ny\n");
        let lines = &file.hunks[0].lines;
        assert_eq!(documents.old_index(0, &lines[1]), Some(1));
        assert_eq!(documents.new_index(0, &lines[3]), Some(2));
        assert_eq!(documents.new_index(0, &lines[1]), None);
        let second = &file.hunks[1].lines;
        assert_eq!((documents.old_index(1, &second[0]), documents.new_index(1, &second[1])), (Some(3), Some(4)));
    }

    #[test]
    fn highlight_budget_bounds_the_documents_and_leaves_the_rest_plain() {
        let file = numbered(5_000);
        let documents = Documents::build(&file, 1, 100);
        assert_eq!(documents.old.lines().count() + documents.new.lines().count(), 100);
        let late = &file.hunks[0].lines[9_000];
        assert_eq!((documents.old_index(0, late), documents.new_index(0, late)), (None, None));
        let early = &file.hunks[0].lines[1];
        assert_eq!(documents.new_index(0, early), Some(0));
        // Hunks the budget never reached have lines but no positions.
        let several = one(MODIFIED);
        let starved = Documents::build(&several, 2, 5);
        assert_eq!(starved.new_index(1, &several.hunks[1].lines[1]), None);
    }
}
