//! Unified patches of two texts: the subset of the `diff` package (`structuredPatch` over `diffLines`,
//! `formatPatch` with file headers only) the ACP adapter renders v1 diff content with.

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
/// `Diff.tokenize` for lines: each line keeps its newline (`\n` or `\r\n`); empty tokens are dropped.
fn lines(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = vec![];
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            tokens.push(text[start..=index].to_owned());
            start = index + 1;
        }
        index += 1;
    }
    if start < text.len() {
        tokens.push(text[start..].to_owned());
    }
    tokens
}
/// `Diff.diffWithOptionsObj`: the shortest edit script, `None` when it needs more than `max_edits` edits.
/// Each element is `(value, added, removed)`.
fn diff_lines(old: &str, new: &str, max_edits: Option<usize>) -> Option<Vec<(String, bool, bool, Vec<String>)>> {
    let (old_tokens, new_tokens) = (lines(old), lines(new));
    let (new_len, old_len) = (new_tokens.len() as i64, old_tokens.len() as i64);
    let mut arena: Vec<Component> = vec![];
    let mut edit_length: i64 = 1;
    let mut max_edit_length = new_len + old_len;
    if let Some(limit) = max_edits {
        max_edit_length = max_edit_length.min(limit as i64);
    }
    let mut best: std::collections::HashMap<i64, Path> = std::collections::HashMap::new();
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
        return Some(build(&arena, first.last, &new_tokens, &old_tokens));
    }
    best.insert(0, first);
    let (mut min_diagonal, mut max_diagonal) = (i64::MIN, i64::MAX);
    while edit_length <= max_edit_length {
        let mut diagonal = min_diagonal.max(-edit_length);
        while diagonal <= max_diagonal.min(edit_length) {
            let remove_path = best.remove(&(diagonal - 1));
            let add_path = best.get(&(diagonal + 1)).copied();
            let mut can_add = false;
            if let Some(add) = add_path {
                let add_new_pos = add.old_pos - diagonal;
                can_add = 0 <= add_new_pos && add_new_pos < new_len;
            }
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
                return Some(build(&arena, base.last, &new_tokens, &old_tokens));
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
fn add_to_path(arena: &mut Vec<Component>, path: Path, added: bool, removed: bool, old_pos_inc: i64) -> Path {
    let last = path.last;
    if let Some(index) = last.filter(|index| arena[*index].added == added && arena[*index].removed == removed) {
        let component = Component { count: arena[index].count + 1, added, removed, previous: arena[index].previous };
        arena.push(component);
        return Path { old_pos: path.old_pos + old_pos_inc, last: Some(arena.len() - 1) };
    }
    arena.push(Component { count: 1, added, removed, previous: last });
    Path { old_pos: path.old_pos + old_pos_inc, last: Some(arena.len() - 1) }
}
/// `buildValues`: the components in order, each with the lines it covers.
fn build(arena: &[Component], last: Option<usize>, new_tokens: &[String], old_tokens: &[String]) -> Vec<(String, bool, bool, Vec<String>)> {
    let mut components = vec![];
    let mut cursor = last;
    while let Some(index) = cursor {
        components.push(arena[index].clone());
        cursor = arena[index].previous;
    }
    components.reverse();
    let (mut new_pos, mut old_pos) = (0, 0);
    let mut out = vec![];
    for component in components {
        let tokens: Vec<String> = if !component.removed {
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
        out.push((tokens.concat(), component.added, component.removed, tokens));
    }
    out
}
struct Hunk {
    old_start: i64,
    old_lines: i64,
    new_start: i64,
    new_lines: i64,
    lines: Vec<String>,
}
/// `structuredPatch(…, {context})` as unified-diff text with file headers only, for one file; `None` when the
/// diff needed more than `max_edits` edits, and an empty hunk list when the texts are equal.
fn structured(old_name: &str, new_name: &str, old: &str, new: &str, context: usize, max_edits: Option<usize>) -> Option<(String, String, Vec<Hunk>)> {
    let mut diff = diff_lines(old, new, max_edits)?;
    diff.push((String::new(), false, false, vec![]));
    let mut hunks: Vec<Hunk> = vec![];
    let (mut old_range_start, mut new_range_start) = (0i64, 0i64);
    let mut current_range: Vec<String> = vec![];
    let (mut old_line, mut new_line) = (1i64, 1i64);
    let context_lines = |lines: &[String]| -> Vec<String> { lines.iter().map(|line| format!(" {line}")).collect() };
    for index in 0..diff.len() {
        let (_, added, removed, lines) = diff[index].clone();
        if added || removed {
            if old_range_start == 0 {
                old_range_start = old_line;
                new_range_start = new_line;
                if index > 0 {
                    let previous = &diff[index - 1].3;
                    let tail = &previous[previous.len().saturating_sub(context)..];
                    current_range = if context > 0 { context_lines(tail) } else { vec![] };
                    old_range_start -= current_range.len() as i64;
                    new_range_start -= current_range.len() as i64;
                }
            }
            for line in &lines {
                current_range.push(format!("{}{line}", if added { '+' } else { '-' }));
            }
            if added {
                new_line += lines.len() as i64;
            } else {
                old_line += lines.len() as i64;
            }
        } else {
            if old_range_start != 0 {
                if lines.len() <= context * 2 && index < diff.len() - 2 {
                    current_range.extend(context_lines(&lines));
                } else {
                    let size = lines.len().min(context);
                    current_range.extend(context_lines(&lines[..size]));
                    hunks.push(Hunk { old_start: old_range_start, old_lines: old_line - old_range_start + size as i64, new_start: new_range_start, new_lines: new_line - new_range_start + size as i64, lines: std::mem::take(&mut current_range) });
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
                hunk.lines.insert(index + 1, "\\ No newline at end of file".into());
                index += 1;
            }
            index += 1;
        }
    }
    Some((old_name.to_owned(), new_name.to_owned(), hunks))
}
/// `formatPatch(patch, FILE_HEADERS_ONLY)` of the patches that have hunks; `None` when there are none.
pub(crate) fn unified_patches(files: &[(&str, &str, &str, &str, Option<usize>)]) -> Option<String> {
    let mut patches = vec![];
    for (old_name, new_name, old, new, max_edits) in files {
        let Some((old_name, new_name, hunks)) = structured(old_name, new_name, old, new, 3, *max_edits) else { continue };
        if !hunks.is_empty() {
            patches.push((old_name, new_name, hunks));
        }
    }
    if patches.is_empty() {
        return None;
    }
    let formatted: Vec<String> = patches
        .into_iter()
        .map(|(old_name, new_name, mut hunks)| {
            let mut out = vec![format!("--- {old_name}"), format!("+++ {new_name}")];
            for hunk in &mut hunks {
                // Unified diff quirk: a range of size 0 starts one line lower than one would expect.
                if hunk.old_lines == 0 {
                    hunk.old_start -= 1;
                }
                if hunk.new_lines == 0 {
                    hunk.new_start -= 1;
                }
                out.push(format!("@@ -{},{} +{},{} @@", hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines));
                out.extend(hunk.lines.iter().cloned());
            }
            out.join("\n") + "\n"
        })
        .collect();
    Some(formatted.join("\n"))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    #[test]
    fn diff_content_renders_the_patch_the_source_renders() {
        let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/text-diff.json")).unwrap();
        assert!(rows.len() > 10);
        for row in rows {
            assert_eq!(crate::acp_tools::diff_patch(&row["content"]), row["patch"].as_str().map(str::to_owned), "{}", row["content"]);
        }
    }
}
