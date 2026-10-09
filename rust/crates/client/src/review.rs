//! The "Changes" and "Uncommitted" diff scopes: the server's `review.getDiffPreview` answer, the
//! requests that read one file's patch or its unchanged lines, the base-ref choices, and the
//! expansion of unchanged lines between hunks. Port of the review half of `DiffPanel.tsx`,
//! `baseRefChoices.ts`, `useReviewFilePatches.ts` and `diffFileContents.ts`.
use crate::{
    diff::{DiffLine, FileDiff, FileStatus, Hunk, LineKind},
    vcs::Ref,
};
use serde_json::{Value, json};
use std::{cmp::Ordering, collections::BTreeMap};

pub const DIFF_PREVIEW: &str = "review.getDiffPreview";
pub const DIFF_FILE_CONTENTS: &str = "review.getDiffFileContents";
/// Files whose patch is read when a truncated preview first opens, and each step after.
pub const FILE_BATCH: usize = 4;
/// Unchanged lines one click reveals, as the original's diff viewer does.
pub const EXPAND_STEP: usize = 100;
/// What the server says when the thread's directory is not under the workspace it serves.
pub const OUTSIDE_WORKSPACE: &str = "configured workspace root";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceKind {
    /// "Uncommitted".
    WorkingTree,
    /// "Changes".
    BranchRange,
}
impl SourceKind {
    pub fn wire(self) -> &'static str {
        match self {
            Self::WorkingTree => "working-tree",
            Self::BranchRange => "branch-range",
        }
    }
    fn parse(text: &str) -> Option<Self> {
        match text {
            "working-tree" => Some(Self::WorkingTree),
            "branch-range" => Some(Self::BranchRange),
            _ => None,
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::WorkingTree => "Uncommitted",
            Self::BranchRange => "Changes",
        }
    }
    /// The panel's loading line for this scope.
    pub fn loading(self) -> &'static str {
        match self {
            Self::WorkingTree => "Loading uncommitted changes...",
            Self::BranchRange => "Loading changes...",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    pub previous_path: Option<String>,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub kind: SourceKind,
    pub base_ref: Option<String>,
    pub head_ref: Option<String>,
    pub diff: String,
    /// Names the content: an unchanged diff has an unchanged hash, so nothing is drawn again.
    pub diff_hash: String,
    pub truncated: bool,
    /// Complete statistics, independent of the patch limit. Absent on older servers.
    pub files: Option<Vec<FileStat>>,
}
impl Source {
    /// A truncated preview with statistics reads each file's patch separately.
    pub fn lazy(&self) -> bool {
        self.truncated && self.files.is_some()
    }
    /// The files of a lazy source in the original's order.
    pub fn sorted_files(&self) -> Vec<FileStat> {
        let mut files = self.files.clone().unwrap_or_default();
        files.sort_by(|a, b| natural_cmp(&a.path, &b.path));
        files
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Preview {
    pub cwd: String,
    pub generated_at: String,
    pub sources: Vec<Source>,
}
impl Preview {
    pub fn source(&self, kind: SourceKind) -> Option<&Source> {
        self.sources.iter().find(|source| source.kind == kind)
    }
    /// Whether a refetch changed nothing the panel shows, so it can keep what it has.
    pub fn same_content(&self, other: &Preview) -> bool {
        self.cwd == other.cwd
            && self.sources.len() == other.sources.len()
            && self.sources.iter().all(|source| other.source(source.kind).is_some_and(|theirs| theirs.diff_hash == source.diff_hash && theirs.truncated == source.truncated && theirs.base_ref == source.base_ref && theirs.head_ref == source.head_ref))
    }
}

pub fn decode(value: &Value) -> Result<Preview, String> {
    let rows = value["sources"].as_array().ok_or("The diff answer has no sources.")?;
    let mut sources = Vec::new();
    for row in rows {
        let Some(kind) = row["kind"].as_str().and_then(SourceKind::parse) else { continue };
        let files = row["files"].as_array().map(|files| {
            files
                .iter()
                .filter_map(|file| {
                    Some(FileStat {
                        path: file["path"].as_str()?.to_owned(),
                        previous_path: file["previousPath"].as_str().map(str::to_owned),
                        additions: file["additions"].as_u64().unwrap_or(0),
                        deletions: file["deletions"].as_u64().unwrap_or(0),
                    })
                })
                .collect()
        });
        sources.push(Source {
            kind,
            base_ref: row["baseRef"].as_str().map(str::to_owned),
            head_ref: row["headRef"].as_str().map(str::to_owned),
            diff: row["diff"].as_str().unwrap_or_default().to_owned(),
            diff_hash: row["diffHash"].as_str().ok_or("A diff source has no hash.")?.to_owned(),
            truncated: row["truncated"].as_bool().unwrap_or(false),
            files,
        });
    }
    Ok(Preview { cwd: value["cwd"].as_str().ok_or("The diff answer has no directory.")?.to_owned(), generated_at: value["generatedAt"].as_str().unwrap_or_default().to_owned(), sources })
}

/// The request for the preview, or for one file's patch of one source.
pub fn preview_payload(cwd: &str, base_ref: Option<&str>, ignore_whitespace: bool, file: Option<(&FileStat, SourceKind)>) -> Value {
    let mut payload = json!({"cwd":cwd,"ignoreWhitespace":ignore_whitespace});
    if let Some(base_ref) = base_ref.filter(|base_ref| !base_ref.is_empty()) {
        payload["baseRef"] = json!(base_ref);
    }
    if let Some((file, kind)) = file {
        payload["file"] = json!({"path":file.path,"previousPath":file.previous_path,"sourceKind":kind.wire()});
    }
    payload
}

/// The request for a file's two sides, for expanding its unchanged lines.
pub fn contents_payload(cwd: &str, kind: SourceKind, base_ref: Option<&str>, head_ref: Option<&str>, file: &FileDiff) -> Value {
    let new_path = file.path();
    let old_path = file.old_path.as_deref().unwrap_or(new_path);
    let change = match file.status {
        FileStatus::Added => "new",
        FileStatus::Deleted => "deleted",
        FileStatus::Renamed if file.hunks.is_empty() => "rename-pure",
        FileStatus::Renamed => "rename-changed",
        _ => "change",
    };
    json!({"cwd":cwd,"sourceKind":kind.wire(),"changeType":change,"baseRef":base_ref,"headRef":head_ref,"oldPath":old_path,"newPath":new_path})
}

/// Whether a failed preview should be asked again at the server's own directory. The original
/// does so when a thread's directory is not under the workspace the server serves.
pub fn retry_at_server_cwd(error: &str, tried: &str, server_cwd: Option<&str>) -> Option<String> {
    server_cwd.filter(|cwd| error.contains(OUTSIDE_WORKSPACE) && *cwd != tried).map(str::to_owned)
}

/// `localeCompare(…, {numeric: true, sensitivity: "base"})`, close enough for file paths: case is
/// ignored and runs of digits compare as numbers.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let run = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut digits = String::new();
                    while let Some(c) = chars.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        chars.next();
                    }
                    digits.trim_start_matches('0').to_owned()
                };
                let (left, right) = (run(&mut a), run(&mut b));
                let order = left.len().cmp(&right.len()).then_with(|| left.cmp(&right));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// `workspaceMutationId`: names the latest settled point after which files on disk may have
/// changed, the last finished command or file change and the newest checkpoint. A new value is a
/// reason to read the checkout again; the same value is not.
pub fn workspace_mutation(projection: Option<&Value>, thread_id: &str) -> Option<String> {
    let projection = projection.filter(|projection| projection["thread"]["id"] == thread_id)?;
    let item = projection["visibleTurnItems"].as_array().into_iter().flatten().rev().map(|row| row.get("item").unwrap_or(row)).find(|item| {
        matches!(item["type"].as_str(), Some("command_execution" | "file_change")) && !matches!(item["status"].as_str(), Some("pending" | "running" | "waiting"))
    });
    let captured = projection["checkpoints"].as_array().into_iter().flatten().filter_map(|checkpoint| checkpoint["capturedAt"].as_str()).max();
    let id = item.and_then(|item| item["id"].as_str());
    (id.is_some() || captured.is_some()).then(|| json!([id, captured]).to_string())
}

/// One entry of the base-ref picker: a branch, with its remote twin when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseChoice {
    pub label: String,
    pub local: Option<String>,
    pub remote: Option<String>,
}
impl BaseChoice {
    /// The ref the choice means when the reader has not flipped it to its remote version.
    pub fn default_value(&self) -> &str {
        self.local.as_deref().or(self.remote.as_deref()).unwrap_or(&self.label)
    }
    /// The ref to compare against, given the one selected now: a selected remote twin stays.
    pub fn value(&self, selected: Option<&str>) -> &str {
        match (selected, self.remote.as_deref()) {
            (Some(selected), Some(remote)) if selected == remote => remote,
            _ => self.default_value(),
        }
    }
}

/// `buildBaseRefChoices`: each local branch paired with its remote of the same name (preferring
/// `origin`), then the remote-only branches.
pub fn base_choices(local: &[Ref], remote: &[Ref]) -> Vec<BaseChoice> {
    let mut unused: Vec<bool> = vec![true; remote.len()];
    let mut choices: Vec<BaseChoice> = Vec::new();
    for local in local {
        let matches: Vec<usize> = remote.iter().enumerate().filter(|(index, candidate)| unused[*index] && crate::vcs::local_name_of_remote(&candidate.name) == local.name).map(|(index, _)| index).collect();
        let pick = matches.iter().copied().find(|index| remote[*index].name.starts_with("origin/")).or(matches.first().copied());
        if let Some(index) = pick {
            unused[index] = false;
        }
        choices.push(BaseChoice { label: local.name.clone(), local: Some(local.name.clone()), remote: pick.map(|index| remote[index].name.clone()) });
    }
    for (index, remote) in remote.iter().enumerate() {
        if unused[index] {
            choices.push(BaseChoice { label: remote.name.clone(), local: None, remote: Some(remote.name.clone()) });
        }
    }
    choices
}

pub fn filter_choices(choices: &[BaseChoice], query: &str) -> Vec<BaseChoice> {
    let query = query.trim().to_lowercase();
    choices
        .iter()
        .filter(|choice| query.is_empty() || [Some(choice.label.as_str()), choice.local.as_deref(), choice.remote.as_deref()].into_iter().flatten().any(|name| name.to_lowercase().contains(&query)))
        .cloned()
        .collect()
}

/// Unchanged lines revealed in one gap between hunks: from the end of the hunk above, and from
/// the start of the hunk below.
pub type Reveals = BTreeMap<usize, (usize, usize)>;

/// A run of unchanged lines the reader has not seen, drawn between two hunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// Which gap of the patch it is, to reveal lines in it.
    pub index: usize,
    /// The hunk of the returned file it sits before; the hunk count for the end of the file.
    pub before: usize,
    pub hidden: usize,
    /// After the last hunk: lines can only be revealed downward from it.
    pub trailing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    /// The file with the revealed lines merged in; `None` when nothing was revealed.
    pub file: Option<FileDiff>,
    pub gaps: Vec<Gap>,
}

/// The first and one past the last line a hunk covers on each side. A count of zero names the
/// line before the change, so nothing is covered.
fn span(start: u32, count: u32) -> (usize, usize) {
    let first = if count == 0 { start as usize + 1 } else { start as usize };
    (first, first + count as usize)
}

/// Text lines of a file's new side, `\r` kept as the patch parser keeps it.
pub fn text_lines(contents: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = contents.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

fn context_line(new_number: usize, delta: isize, text: &str) -> DiffLine {
    DiffLine { kind: LineKind::Context, old_number: Some((new_number as isize + delta) as u32), new_number: Some(new_number as u32), text: text.to_owned(), no_newline: false }
}

fn rebuilt(mut hunk: Hunk) -> Hunk {
    let old = hunk.lines.iter().filter(|line| line.kind != LineKind::Addition).count() as u32;
    let new = hunk.lines.iter().filter(|line| line.kind != LineKind::Deletion).count() as u32;
    if let Some(first) = hunk.lines.iter().find_map(|line| line.old_number) {
        hunk.old_start = first;
    }
    if let Some(first) = hunk.lines.iter().find_map(|line| line.new_number) {
        hunk.new_start = first;
    }
    hunk.old_count = old;
    hunk.new_count = new;
    hunk
}

/// The file's gaps, and the file with `reveals` merged in. `new_contents` is the new side; until
/// it has been read, only the gaps the patch itself shows can be named, and nothing is revealed.
/// Added, deleted and binary files have nothing unchanged to reveal.
pub fn expand(file: &FileDiff, new_contents: Option<&[&str]>, reveals: &Reveals) -> Expanded {
    if file.hunks.is_empty() || file.binary || matches!(file.status, FileStatus::Added | FileStatus::Deleted) {
        return Expanded { file: None, gaps: Vec::new() };
    }
    let total = new_contents.map(<[&str]>::len);
    let hunks = &file.hunks;
    let mut out: Vec<Hunk> = Vec::new();
    let mut gaps = Vec::new();
    let mut current: Option<Hunk> = None;
    for k in 0..=hunks.len() {
        // The gap above hunk `k`: new lines `from..to`, whose old twin is `delta` lines away.
        let (from, delta) = match k {
            0 => (1, 0),
            _ => {
                let (_, end) = span(hunks[k - 1].new_start, hunks[k - 1].new_count);
                let old_end = span(hunks[k - 1].old_start, hunks[k - 1].old_count).1;
                (end, old_end as isize - end as isize)
            }
        };
        let (to, delta) = match hunks.get(k) {
            Some(next) => {
                let (first, _) = span(next.new_start, next.new_count);
                let (old_first, _) = span(next.old_start, next.old_count);
                (first, if k == 0 { old_first as isize - first as isize } else { delta })
            }
            None => (total.map_or(from, |total| total + 1), delta),
        };
        let hidden = to.saturating_sub(from);
        let asked = if new_contents.is_some() { reveals.get(&k).copied().unwrap_or((0, 0)) } else { (0, 0) };
        let (top, bottom) = if k == 0 { (0, asked.1.min(hidden)) } else if k == hunks.len() { (asked.0.min(hidden), 0) } else { (asked.0.min(hidden), asked.1.min(hidden - asked.0.min(hidden))) };
        let seen = top + bottom;
        let lines_of = |range: std::ops::Range<usize>| -> Vec<DiffLine> {
            new_contents.map_or_else(Vec::new, |contents| range.filter_map(|number| contents.get(number - 1).map(|text| context_line(number, delta, text))).collect())
        };
        if k == 0 {
            let mut start = hunks[0].clone();
            if bottom > 0 {
                let mut lines = lines_of(to - bottom..to);
                lines.append(&mut start.lines);
                start.lines = lines;
            }
            if hidden > seen {
                gaps.push(Gap { index: 0, before: 0, hidden: hidden - seen, trailing: false });
            }
            current = Some(rebuilt(start));
            continue;
        }
        let mut open = current.take().expect("a hunk is open");
        if top > 0 {
            open.lines.extend(lines_of(from..from + top));
        }
        if k == hunks.len() {
            if hidden > seen && total.is_some() {
                gaps.push(Gap { index: k, before: out.len() + 1, hidden: hidden - seen, trailing: true });
            }
            out.push(rebuilt(open));
            break;
        }
        let mut next = hunks[k].clone();
        if hidden <= seen {
            // Nothing is hidden any more: the two hunks are one.
            open.lines.extend(lines_of(from + top..to));
            open.lines.append(&mut next.lines);
            open.section = if open.section.is_empty() { next.section } else { open.section };
            current = Some(rebuilt(open));
        } else {
            if bottom > 0 {
                let mut lines = lines_of(to - bottom..to);
                lines.append(&mut next.lines);
                next.lines = lines;
            }
            out.push(rebuilt(open));
            gaps.push(Gap { index: k, before: out.len(), hidden: hidden - seen, trailing: false });
            current = Some(rebuilt(next));
        }
    }
    let changed = reveals.values().any(|(top, bottom)| top + bottom > 0) && new_contents.is_some();
    Expanded { file: changed.then(|| FileDiff { hunks: out, ..file.clone() }), gaps }
}

/// What one click on a gap reveals. `all` takes everything left.
pub fn reveal(reveals: &mut Reveals, gap: &Gap, from_top: bool, all: bool) {
    let entry = reveals.entry(gap.index).or_insert((0, 0));
    let step = if all { gap.hidden } else { EXPAND_STEP.min(gap.hidden) };
    // The first gap only grows toward the hunk below it, the last only away from the hunk above.
    if gap.index == 0 || !gap.trailing && !from_top {
        entry.1 += step;
    } else {
        entry.0 += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff;

    fn file(patch: &str) -> FileDiff {
        diff::parse(patch).remove(0)
    }

    #[test]
    fn a_finished_command_or_a_new_checkpoint_is_a_workspace_mutation() {
        let projection = |items: Value, checkpoints: Value| json!({"thread":{"id":"t"},"visibleTurnItems":items,"checkpoints":checkpoints});
        assert_eq!(workspace_mutation(None, "t"), None);
        assert_eq!(workspace_mutation(Some(&projection(json!([]), json!([]))), "t"), None);
        let running = json!([{"item":{"id":"a","type":"command_execution","status":"running"}}]);
        let done = json!([{"item":{"id":"a","type":"command_execution","status":"completed"}},{"item":{"id":"m","type":"assistant_message","status":"completed"}}]);
        assert_eq!(workspace_mutation(Some(&projection(running, json!([]))), "t"), None, "a command still running changes nothing yet");
        let first = workspace_mutation(Some(&projection(done.clone(), json!([]))), "t");
        assert!(first.is_some());
        assert_eq!(first, workspace_mutation(Some(&projection(done.clone(), json!([]))), "t"));
        assert_ne!(first, workspace_mutation(Some(&projection(done, json!([{"capturedAt":"2026-01-01T00:00:00Z"}]))), "t"));
        assert_eq!(workspace_mutation(Some(&projection(json!([]), json!([]))), "other"), None);
    }

    #[test]
    fn numbers_compare_as_numbers_and_case_is_ignored() {
        assert_eq!(natural_cmp("file2.rs", "file10.rs"), Ordering::Less);
        assert_eq!(natural_cmp("A.rs", "a.rs"), Ordering::Equal);
        assert_eq!(natural_cmp("src/b", "src/a/z"), Ordering::Greater);
        assert_eq!(natural_cmp("a", "ab"), Ordering::Less);
    }

    #[test]
    fn decoding_keeps_both_sources_and_their_statistics() {
        let value = json!({"cwd":"/r","generatedAt":"2026-01-01T00:00:00Z","sources":[
            {"id":"w","kind":"working-tree","title":"Uncommitted","baseRef":null,"headRef":"f","diff":"","diffHash":"h1","truncated":false},
            {"id":"b","kind":"branch-range","title":"Changes","baseRef":"main","headRef":"f","diff":"x","diffHash":"h2","truncated":true,"files":[{"path":"b","previousPath":null,"additions":3,"deletions":1},{"path":"a","previousPath":"o","additions":0,"deletions":0}]}]});
        let preview = decode(&value).unwrap();
        assert_eq!(preview.sources.len(), 2);
        let changes = preview.source(SourceKind::BranchRange).unwrap();
        assert!(changes.lazy() && !preview.source(SourceKind::WorkingTree).unwrap().lazy());
        assert_eq!(changes.sorted_files()[0].path, "a");
        assert!(preview.same_content(&decode(&value).unwrap()));
        let mut moved = value.clone();
        moved["sources"][1]["diffHash"] = json!("h3");
        assert!(!preview.same_content(&decode(&moved).unwrap()));
        assert!(decode(&json!({"cwd":"/r","sources":[{"kind":"working-tree"}]})).is_err());
    }

    #[test]
    fn requests_name_the_base_the_file_and_the_change_type() {
        let stat = FileStat { path: "b".into(), previous_path: Some("a".into()), additions: 0, deletions: 0 };
        assert_eq!(preview_payload("/r", None, false, None), json!({"cwd":"/r","ignoreWhitespace":false}));
        assert_eq!(preview_payload("/r", Some("main"), true, Some((&stat, SourceKind::BranchRange)))["file"], json!({"path":"b","previousPath":"a","sourceKind":"branch-range"}));
        assert_eq!(preview_payload("/r", Some(""), true, None).get("baseRef"), None);
        let patch = file("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n");
        let payload = contents_payload("/r", SourceKind::WorkingTree, None, None, &patch);
        assert_eq!((payload["changeType"].as_str(), payload["oldPath"].as_str(), payload["sourceKind"].as_str()), (Some("change"), Some("x"), Some("working-tree")));
        assert_eq!(retry_at_server_cwd("path must stay within the configured workspace root", "/a", Some("/b")), Some("/b".into()));
        assert_eq!(retry_at_server_cwd("path must stay within the configured workspace root", "/b", Some("/b")), None);
        assert_eq!(retry_at_server_cwd("other", "/a", Some("/b")), None);
    }

    fn reference(name: &str, remote: bool) -> Ref {
        Ref { name: name.into(), is_remote: remote, current: false, is_default: false, worktree_path: None }
    }

    #[test]
    fn base_choices_pair_local_branches_with_their_remote_twin() {
        let choices = base_choices(&[reference("main", false), reference("dev", false)], &[reference("upstream/main", true), reference("origin/main", true), reference("origin/old", true)]);
        assert_eq!(choices.iter().map(|choice| (choice.label.as_str(), choice.remote.as_deref())).collect::<Vec<_>>(), [("main", Some("origin/main")), ("dev", None), ("upstream/main", Some("upstream/main")), ("origin/old", Some("origin/old"))]);
        assert_eq!(choices[0].value(None), "main");
        assert_eq!(choices[0].value(Some("origin/main")), "origin/main");
        assert_eq!(filter_choices(&choices, " OLD ").len(), 1);
        assert_eq!(filter_choices(&choices, "").len(), 4);
    }

    const MIDDLE: &str = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -2,3 +2,3 @@\n l2\n-l3\n+L3\n l4\n@@ -20,3 +20,3 @@\n l20\n-l21\n+L21\n l22\n";

    fn contents(count: usize) -> String {
        (1..=count).map(|number| match number {
            3 => "L3".to_owned(),
            21 => "L21".to_owned(),
            _ => format!("l{number}"),
        }).collect::<Vec<_>>().join("\n") + "\n"
    }

    #[test]
    fn gaps_are_named_from_the_patch_and_the_end_of_the_file_once_it_is_read() {
        let patch = file(MIDDLE);
        let before = expand(&patch, None, &Reveals::new());
        assert_eq!(before.file, None);
        assert_eq!(before.gaps, [Gap { index: 0, before: 0, hidden: 1, trailing: false }, Gap { index: 1, before: 1, hidden: 15, trailing: false }]);
        let text = contents(30);
        let lines = text_lines(&text);
        let read = expand(&patch, Some(&lines), &Reveals::new());
        assert_eq!(read.gaps.last(), Some(&Gap { index: 2, before: 2, hidden: 8, trailing: true }));
    }

    #[test]
    fn revealing_lines_extends_the_hunks_and_closing_a_gap_joins_them() {
        let patch = file(MIDDLE);
        let text = contents(30);
        let lines = text_lines(&text);
        let mut reveals = Reveals::new();
        // Two lines below the first hunk and three above the second.
        reveals.insert(1, (2, 3));
        let expanded = expand(&patch, Some(&lines), &reveals);
        let shown = expanded.file.unwrap();
        assert_eq!(shown.hunks.len(), 2);
        assert_eq!((shown.hunks[0].new_start, shown.hunks[0].new_count), (2, 5));
        assert_eq!((shown.hunks[1].new_start, shown.hunks[1].new_count, shown.hunks[1].old_start), (17, 6, 17));
        assert_eq!(shown.hunks[0].lines.last().map(|line| (line.text.as_str(), line.old_number, line.new_number)), Some(("l6", Some(6), Some(6))));
        assert_eq!(expanded.gaps.iter().find(|gap| gap.index == 1).map(|gap| gap.hidden), Some(10));
        // The rest of the gap: one hunk, with every line numbered in order.
        reveals.insert(1, (2, 13));
        let joined = expand(&patch, Some(&lines), &reveals).file.unwrap();
        assert_eq!(joined.hunks.len(), 1);
        let numbers: Vec<u32> = joined.hunks[0].lines.iter().filter_map(|line| line.new_number).collect();
        assert_eq!(numbers, (2..=22).collect::<Vec<_>>());
        assert_eq!((joined.hunks[0].old_start, joined.hunks[0].old_count, joined.hunks[0].new_count), (2, 21, 21));
        // The line above the first hunk and the lines after the last.
        reveals.insert(0, (0, 1));
        reveals.insert(2, (3, 0));
        let ends = expand(&patch, Some(&lines), &reveals).file.unwrap();
        assert_eq!(ends.hunks[0].lines.first().map(|line| line.text.as_str()), Some("l1"));
        assert_eq!(ends.hunks[0].lines.last().map(|line| line.text.as_str()), Some("l25"));
    }

    #[test]
    fn lines_revealed_across_a_deletion_keep_their_old_numbers() {
        // A deleted line shifts the old numbering by one below the first hunk.
        let patch = file("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1 @@\n-x\n k\n@@ -9,2 +8,2 @@\n a\n-b\n+B\n");
        let text = "k\n2\n3\n4\n5\n6\n7\na\nB\n".to_owned();
        let lines = text_lines(&text);
        let mut reveals = Reveals::new();
        reveals.insert(1, (1, 0));
        let shown = expand(&patch, Some(&lines), &reveals).file.unwrap();
        let line = shown.hunks[0].lines.last().unwrap();
        assert_eq!((line.new_number, line.old_number, line.text.as_str()), (Some(2), Some(3), "2"));
    }

    #[test]
    fn files_with_nothing_unchanged_to_reveal_have_no_gaps() {
        let added = file("diff --git a/n b/n\nnew file mode 100644\n--- /dev/null\n+++ b/n\n@@ -0,0 +1,2 @@\n+a\n+b\n");
        assert_eq!(expand(&added, None, &Reveals::new()).gaps, []);
    }

    #[test]
    fn one_click_reveals_a_step_and_all_takes_the_rest() {
        let mut reveals = Reveals::new();
        let middle = Gap { index: 1, before: 1, hidden: 250, trailing: false };
        reveal(&mut reveals, &middle, true, false);
        reveal(&mut reveals, &middle, false, false);
        assert_eq!(reveals[&1], (100, 100));
        reveal(&mut reveals, &Gap { index: 0, before: 0, hidden: 40, trailing: false }, true, true);
        assert_eq!(reveals[&0], (0, 40));
    }
}
