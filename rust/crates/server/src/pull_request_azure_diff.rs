//! The patch Azure DevOps does not serve, built here (port of `azureDevOpsDiff.ts`). Azure's diff
//! routes name the files an iteration changed and the contents are a separate read per side, so a
//! unified patch is made out of both sides of every changed file. The line diff is the one the
//! original gets from the `diff` package (`structuredPatch` over `diffLines`, v8), reproduced here
//! with the same edit ceiling, so a change is sliced the same way as in the original. The
//! original also has a two second wall-clock backstop for a slow machine; it is left out, because
//! a limit that fires on a busy host would slice the same change differently from one run to the
//! next, and the edit ceiling alone already bounds the work.
use crate::{pull_request_azure_json::{ChangeEntry, ChangeKind}, pull_request_decode::quote_git_patch_path};
use std::collections::HashMap;

/// Beyond this a file is shown as changed without its hunks.
const MAX_FILE_BYTES: usize = 512 * 1024;
/// Git's own default, and what the hunks from this repo's other hosts are already cut to.
const PATCH_CONTEXT_LINES: usize = 3;
/// How far apart one file's two sides may be before it is listed without its hunks.
pub const MAX_FILE_DIFF_EDITS: usize = 2_000;
/// How much diff work one slice does before the rest is left for the next request.
pub const MAX_DIFF_SLICE_EDITS: usize = 6_000;
/// How much patch one slice carries before the rest is left for the next request.
pub const MAX_DIFF_SLICE_BYTES: usize = 256 * 1024;
/// How many files one slice carries however little each one weighs.
pub const MAX_DIFF_SLICE_FILES: usize = 300;
const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file";

// ---- the cursor -------------------------------------------------------------------------------------

/// How far a diff read got, and which push it was reading: a push landing mid-read would renumber
/// the files under a cursor that did not also pin the iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffCursor {
    pub iteration_id: i64,
    pub file_index: usize,
}

pub fn format_cursor(cursor: DiffCursor) -> String {
    format!("{}:{}", cursor.iteration_id, cursor.file_index)
}

/// Plain decimal only, so a foreign cursor's `0x3` is not read as three.
fn decimal(component: &str) -> Option<i64> {
    if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number: f64 = component.parse().ok()?;
    (number <= 9_007_199_254_740_991.0).then_some(number as i64)
}

/// `None` for anything this did not write, which starts the read from the top rather than failing.
pub fn parse_cursor(raw: Option<&str>) -> Option<DiffCursor> {
    let mut parts = raw?.split(':');
    let (iteration, file, rest) = (parts.next()?, parts.next()?, parts.next());
    if rest.is_some() {
        return None;
    }
    let (iteration_id, file_index) = (decimal(iteration)?, decimal(file)?);
    (iteration_id > 0).then_some(DiffCursor { iteration_id, file_index: file_index as usize })
}

// ---- the line diff ----------------------------------------------------------------------------------

#[derive(Clone)]
struct Component {
    count: usize,
    added: bool,
    removed: bool,
    previous: Option<usize>,
}
#[derive(Clone, Copy)]
struct Path {
    old_pos: i64,
    last: Option<usize>,
}

/// `Diff.tokenize` for lines: each line keeps its newline (`\n` or `\r\n`).
fn tokens(text: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            out.push(&text[start..=index]);
            start = index + 1;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn add_to_path(arena: &mut Vec<Component>, path: Path, added: bool, removed: bool, old_pos_inc: i64) -> Path {
    let last = path.last;
    let component = match last.filter(|index| arena[*index].added == added && arena[*index].removed == removed) {
        Some(index) => Component { count: arena[index].count + 1, added, removed, previous: arena[index].previous },
        None => Component { count: 1, added, removed, previous: last },
    };
    arena.push(component);
    Path { old_pos: path.old_pos + old_pos_inc, last: Some(arena.len() - 1) }
}

/// One run of the edit script: whether it was added or removed (neither: common), and its lines.
type Run<'a> = (bool, bool, Vec<&'a str>);

fn build<'a>(arena: &[Component], last: Option<usize>, new_tokens: &[&'a str], old_tokens: &[&'a str]) -> Vec<Run<'a>> {
    let mut components = vec![];
    let mut cursor = last;
    while let Some(index) = cursor {
        components.push(arena[index].clone());
        cursor = arena[index].previous;
    }
    components.reverse();
    let (mut new_pos, mut old_pos) = (0, 0);
    components
        .into_iter()
        .map(|component| {
            let lines = if !component.removed {
                let slice = new_tokens[new_pos..new_pos + component.count].to_vec();
                new_pos += component.count;
                if !component.added {
                    old_pos += component.count;
                }
                slice
            } else {
                let slice = old_tokens[old_pos..old_pos + component.count].to_vec();
                old_pos += component.count;
                slice
            };
            (component.added, component.removed, lines)
        })
        .collect()
}

/// `Diff.diff` over lines: the shortest edit script, or `None` when it needs more than `max_edits`
/// edits.
fn diff_lines<'a>(old_tokens: &[&'a str], new_tokens: &[&'a str], max_edits: usize) -> Option<Vec<Run<'a>>> {
    let (new_len, old_len) = (new_tokens.len() as i64, old_tokens.len() as i64);
    let mut arena: Vec<Component> = vec![];
    let max_edit_length = (new_len + old_len).min(max_edits as i64);
    let mut best: HashMap<i64, Path> = HashMap::new();
    let extract = |arena: &mut Vec<Component>, path: &mut Path, diagonal: i64| -> i64 {
        let mut old_pos = path.old_pos;
        let mut new_pos = old_pos - diagonal;
        let mut common = 0;
        while new_pos + 1 < new_len && old_pos + 1 < old_len && old_tokens[(old_pos + 1) as usize] == new_tokens[(new_pos + 1) as usize] {
            new_pos += 1;
            old_pos += 1;
            common += 1;
        }
        if common > 0 {
            arena.push(Component { count: common, added: false, removed: false, previous: path.last });
            path.last = Some(arena.len() - 1);
        }
        path.old_pos = old_pos;
        new_pos
    };
    let mut first = Path { old_pos: -1, last: None };
    let new_pos = extract(&mut arena, &mut first, 0);
    if first.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
        return Some(build(&arena, first.last, new_tokens, old_tokens));
    }
    best.insert(0, first);
    let (mut min_diagonal, mut max_diagonal) = (i64::MIN, i64::MAX);
    let mut edit_length = 1;
    while edit_length <= max_edit_length {
        let mut diagonal = min_diagonal.max(-edit_length);
        while diagonal <= max_diagonal.min(edit_length) {
            let remove_path = best.remove(&(diagonal - 1));
            let add_path = best.get(&(diagonal + 1)).copied();
            let can_add = add_path.is_some_and(|add| {
                let add_new_pos = add.old_pos - diagonal;
                0 <= add_new_pos && add_new_pos < new_len
            });
            let can_remove = remove_path.is_some_and(|remove| remove.old_pos + 1 < old_len);
            if !can_add && !can_remove {
                best.remove(&diagonal);
                diagonal += 2;
                continue;
            }
            let mut base = if !can_remove || (can_add && remove_path.unwrap().old_pos < add_path.unwrap().old_pos) {
                add_to_path(&mut arena, add_path.unwrap(), true, false, 0)
            } else {
                add_to_path(&mut arena, remove_path.unwrap(), false, true, 1)
            };
            let new_pos = extract(&mut arena, &mut base, diagonal);
            if base.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                return Some(build(&arena, base.last, new_tokens, old_tokens));
            }
            best.insert(diagonal, base);
            if base.old_pos + 1 >= old_len {
                max_diagonal = max_diagonal.min(diagonal - 1);
            }
            if new_pos + 1 >= new_len {
                min_diagonal = min_diagonal.max(diagonal + 1);
            }
            diagonal += 2;
        }
        edit_length += 1;
    }
    None
}

struct Hunk {
    old_start: i64,
    old_lines: i64,
    new_start: i64,
    new_lines: i64,
    lines: Vec<String>,
}

/// `structuredPatch(…, {context})`: the hunks of a line diff, `None` when it gave up.
fn structured_hunks(old: &str, new: &str, context: usize, max_edits: usize) -> Option<Vec<Hunk>> {
    let (old_tokens, new_tokens) = (tokens(old), tokens(new));
    let mut diff = diff_lines(&old_tokens, &new_tokens, max_edits)?;
    diff.push((false, false, vec![]));
    let mut hunks: Vec<Hunk> = vec![];
    let (mut old_range_start, mut new_range_start) = (0i64, 0i64);
    let mut current: Vec<String> = vec![];
    let (mut old_line, mut new_line) = (1i64, 1i64);
    let context_lines = |lines: &[&str]| -> Vec<String> { lines.iter().map(|line| format!(" {line}")).collect() };
    for index in 0..diff.len() {
        let (added, removed, lines) = (diff[index].0, diff[index].1, diff[index].2.clone());
        if added || removed {
            if old_range_start == 0 {
                old_range_start = old_line;
                new_range_start = new_line;
                if index > 0 {
                    let previous = &diff[index - 1].2;
                    current = if context > 0 { context_lines(&previous[previous.len().saturating_sub(context)..]) } else { vec![] };
                    old_range_start -= current.len() as i64;
                    new_range_start -= current.len() as i64;
                }
            }
            for line in &lines {
                current.push(format!("{}{line}", if added { '+' } else { '-' }));
            }
            if added {
                new_line += lines.len() as i64;
            } else {
                old_line += lines.len() as i64;
            }
        } else {
            if old_range_start != 0 {
                if lines.len() <= context * 2 && index < diff.len() - 2 {
                    current.extend(context_lines(&lines));
                } else {
                    let size = lines.len().min(context);
                    current.extend(context_lines(&lines[..size]));
                    hunks.push(Hunk {
                        old_start: old_range_start,
                        old_lines: old_line - old_range_start + size as i64,
                        new_start: new_range_start,
                        new_lines: new_line - new_range_start + size as i64,
                        lines: std::mem::take(&mut current),
                    });
                    old_range_start = 0;
                    new_range_start = 0;
                }
            }
            old_line += lines.len() as i64;
            new_line += lines.len() as i64;
        }
    }
    for hunk in &mut hunks {
        let mut index = 0;
        while index < hunk.lines.len() {
            if hunk.lines[index].ends_with('\n') {
                hunk.lines[index].pop();
            } else {
                hunk.lines.insert(index + 1, NO_NEWLINE_MARKER.into());
                index += 1;
            }
            index += 1;
        }
    }
    Some(hunks)
}

// ---- one file's section ---------------------------------------------------------------------------

/// The two texts of one changed file, empty on whichever side the change does not have.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileTexts {
    pub old_contents: String,
    pub new_contents: String,
    /// Azure's own flag for a file it hands back base64-encoded instead of as text.
    pub binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub section: String,
    /// The file changed but its hunks are not in the section, so the patch has a hole in it.
    pub truncated: bool,
    /// The diff was given up on partway, so a caller reading a run of files stops here.
    pub abandoned: bool,
    /// Lines added or removed, which is what a slice's diff budget is spent in.
    pub edits: usize,
}

/// A text's lines, without the empty one that a trailing newline leaves behind a split.
fn content_lines(contents: &str) -> Vec<&str> {
    if contents.is_empty() {
        return vec![];
    }
    let mut lines: Vec<&str> = contents.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// Git points an empty range at the line before it and writes a single line as its number alone.
fn hunk_range(start: i64, lines: i64) -> String {
    match lines {
        0 => format!("{},0", start - 1),
        1 => start.to_string(),
        _ => format!("{start},{lines}"),
    }
}

/// The `diff --git` preamble a viewer reads a file's identity and fate from. Names are quoted the
/// way git quotes them, prefix inside the quoting.
fn patch_header(change: &ChangeEntry) -> String {
    let old_side = quote_git_patch_path(&format!("a/{}", change.old_path));
    let new_side = quote_git_patch_path(&format!("b/{}", change.path));
    let mut lines = vec![format!("diff --git {old_side} {new_side}")];
    match change.change_kind {
        ChangeKind::New => lines.push("new file mode 100644".into()),
        ChangeKind::Deleted => lines.push("deleted file mode 100644".into()),
        ChangeKind::RenamePure | ChangeKind::RenameChanged => {
            lines.push(format!("rename from {}", quote_git_patch_path(&change.old_path)));
            lines.push(format!("rename to {}", quote_git_patch_path(&change.path)));
        }
        ChangeKind::Change => {}
    }
    lines.push(format!("--- {}", if change.change_kind == ChangeKind::New { "/dev/null" } else { &old_side }));
    lines.push(format!("+++ {}", if change.change_kind == ChangeKind::Deleted { "/dev/null" } else { &new_side }));
    lines.join("\n")
}

/// A file written out as wholly replaced: every old line gone, every new line arrived, in one hunk.
fn replacement_section(header: &str, texts: &FileTexts) -> String {
    let (old_lines, new_lines) = (content_lines(&texts.old_contents), content_lines(&texts.new_contents));
    let no_newline = |contents: &str, lines: &[&str]| -> Vec<String> { if !lines.is_empty() && !contents.ends_with('\n') { vec![NO_NEWLINE_MARKER.into()] } else { vec![] } };
    let mut out = vec![header.to_string(), format!("@@ -{} +{} @@", hunk_range(1, old_lines.len() as i64), hunk_range(1, new_lines.len() as i64))];
    out.extend(old_lines.iter().map(|line| format!("-{line}")));
    out.extend(no_newline(&texts.old_contents, &old_lines));
    out.extend(new_lines.iter().map(|line| format!("+{line}")));
    out.extend(no_newline(&texts.new_contents, &new_lines));
    out.push(String::new());
    out.join("\n")
}

fn header_only(header: &str, truncated: bool, abandoned: bool, edits: usize) -> FilePatch {
    FilePatch { section: format!("{header}\n"), truncated, abandoned, edits }
}

/// One file's section of a unified patch.
pub fn file_patch(change: &ChangeEntry, texts: &FileTexts) -> FilePatch {
    let header = patch_header(change);
    let (old, new) = (texts.old_contents.as_str(), texts.new_contents.as_str());
    if texts.binary || old.contains('\0') || new.contains('\0') {
        let binary = format!("Binary files {} and {} differ", quote_git_patch_path(&format!("a/{}", change.old_path)), quote_git_patch_path(&format!("b/{}", change.path)));
        return FilePatch { section: format!("{header}\n{binary}\n"), truncated: true, abandoned: false, edits: 0 };
    }
    if old.len() > MAX_FILE_BYTES || new.len() > MAX_FILE_BYTES {
        return header_only(&header, true, false, 0);
    }
    // A creation or deletion has nothing on one side, so writing the other in full is already the
    // minimal patch and needs no diff search.
    let created = old.is_empty() && !new.is_empty();
    let deleted = new.is_empty() && !old.is_empty();
    if created || deleted {
        let contents = if created { new } else { old };
        let lines = content_lines(contents).len();
        // A `+`/`-` marker on every line can put a side that just fits the size ceiling over it.
        if contents.len() + lines > MAX_FILE_BYTES {
            return header_only(&header, true, false, lines);
        }
        let section = replacement_section(&header, texts);
        if section.len() > MAX_FILE_BYTES {
            return header_only(&header, true, false, lines);
        }
        return FilePatch { section, truncated: false, abandoned: false, edits: lines };
    }
    let Some(hunks) = structured_hunks(old, new, PATCH_CONTEXT_LINES, MAX_FILE_DIFF_EDITS) else {
        // The ceiling lists the file without hunks rather than falling back to a full replacement.
        return header_only(&header, true, true, MAX_FILE_DIFF_EDITS);
    };
    let edits = hunks.iter().flat_map(|hunk| &hunk.lines).filter(|line| line.starts_with('+') || line.starts_with('-')).count();
    let rendered: Vec<String> = hunks
        .iter()
        .map(|hunk| {
            let mut out = vec![format!("@@ -{} +{} @@", hunk_range(hunk.old_start, hunk.old_lines), hunk_range(hunk.new_start, hunk.new_lines))];
            out.extend(hunk.lines.iter().cloned());
            out.join("\n")
        })
        .collect();
    // A pure rename has no hunks to give but is still listed.
    let section = if rendered.is_empty() { format!("{header}\n") } else { format!("{header}\n{}\n", rendered.join("\n")) };
    if section.len() > MAX_FILE_BYTES {
        return header_only(&header, true, false, edits);
    }
    FilePatch { section, truncated: false, abandoned: false, edits }
}

/// A file listed without its hunks, for when the host would not hand one of its two sides over.
pub fn unreadable_file_patch(change: &ChangeEntry) -> FilePatch {
    header_only(&patch_header(change), true, false, 0)
}
