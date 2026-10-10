//! What the head has of each file, read off a unified patch (port of `bitbucketDiffRevisions.ts`).
//! Bitbucket states no blob id for a file anywhere but a patch's `index <before>..<after>` line,
//! so the pull request's own patch is where the versions that viewed-file marks are kept against
//! come from.
use std::collections::BTreeMap;

const ENTRY: &str = "diff --git ";

struct Entry {
    old_path: Option<String>,
    new_path: Option<String>,
    deleted: bool,
    revision: Option<String>,
    /// Past the first hunk header every line is content, and content can start like a header.
    in_body: bool,
}

/// One header's name token as the name it stands for (`unquoteGitPatchPath`): the escapes are
/// undone whether the quotes are still there or not, because patch parsers disagree about how much
/// of the quoting they hand back. The escapes are per byte, so a name in another alphabet arrives
/// as a run of octal and only reads back as itself once those bytes are rejoined and decoded.
pub fn unquote_git_patch_path(token: &str) -> String {
    let body = if token.len() >= 2 && token.starts_with('"') && token.ends_with('"') { &token[1..token.len() - 1] } else { token };
    if !body.contains('\\') {
        return body.to_string();
    }
    let characters: Vec<char> = body.chars().collect();
    let mut bytes: Vec<u8> = vec![];
    let mut at = 0;
    while at < characters.len() {
        let character = characters[at];
        if character != '\\' {
            let mut buffer = [0; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            at += 1;
            continue;
        }
        let Some(&escaped) = characters.get(at + 1) else {
            bytes.push(b'\\');
            break;
        };
        let named = match escaped {
            '"' => Some(0x22),
            '\\' => Some(0x5c),
            'a' => Some(0x07),
            'b' => Some(0x08),
            'f' => Some(0x0c),
            'n' => Some(0x0a),
            'r' => Some(0x0d),
            't' => Some(0x09),
            'v' => Some(0x0b),
            _ => None,
        };
        if let Some(byte) = named {
            bytes.push(byte);
            at += 2;
            continue;
        }
        let octal: String = characters[at + 1..characters.len().min(at + 4)].iter().collect();
        if octal.len() == 3 && octal.bytes().all(|digit| (b'0'..=b'7').contains(&digit)) {
            bytes.push(u8::from_str_radix(&octal, 8).unwrap_or(0));
            at += 4;
            continue;
        }
        let mut buffer = [0; 4];
        bytes.extend_from_slice(escaped.encode_utf8(&mut buffer).as_bytes());
        at += 2;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Where a quoted name closes, given git escapes every quote the name itself holds.
fn quoted_end(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut at = 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 1,
            b'"' => return Some(at),
            _ => {}
        }
        at += 1;
    }
    None
}

/// `a/x`/`b/x` on a `---`/`+++` line; `/dev/null` marks the side that has no file. Git ends the
/// name with a tab when it holds a space, and a name with a tab of its own arrives quoted with that
/// tab escaped, so the first literal tab is never part of what the file is called.
fn side_path(rest: &str, prefix: &str) -> Option<String> {
    let token = rest.find('\t').map_or(rest, |tab| &rest[..tab]);
    if token == "/dev/null" {
        return None;
    }
    let path = unquote_git_patch_path(token);
    Some(path.strip_prefix(prefix).map_or_else(|| path.clone(), str::to_string))
}

fn header_side(token: &str, prefix: &str) -> Option<String> {
    unquote_git_patch_path(token).strip_prefix(prefix).map(str::to_string)
}

/// The two names on a `diff --git` line, written with no delimiter between them. `a/one two b/one
/// two` can split in more than one place, so the split leaving both sides equal wins; a rename (the
/// only case where sides differ) states its names on separate lines instead. A quoted name ends at
/// its own closing quote and needs none of that guessing.
fn header_paths(rest: &str) -> (Option<String>, Option<String>) {
    if rest.starts_with('"') {
        let Some(end) = quoted_end(rest) else { return (None, None) };
        if rest.as_bytes().get(end + 1) != Some(&b' ') {
            return (None, None);
        }
        return (header_side(&rest[..=end], "a/"), header_side(&rest[end + 2..], "b/"));
    }
    if rest.ends_with('"') {
        let Some(opens) = rest.find('"').filter(|opens| *opens >= 1 && rest.as_bytes()[opens - 1] == b' ') else { return (None, None) };
        return (header_side(&rest[..opens - 1], "a/"), header_side(&rest[opens..], "b/"));
    }
    if !rest.starts_with("a/") {
        return (None, None);
    }
    let splits: Vec<usize> = rest.match_indices(" b/").map(|(at, _)| at).filter(|at| *at >= 2).collect();
    let chosen = splits.iter().copied().find(|at| rest[2..*at] == rest[at + 3..]).or_else(|| (splits.len() == 1).then(|| splits[0]));
    match chosen {
        None => (None, None),
        Some(at) => (Some(rest[2..at].to_string()), Some(rest[at + 3..].to_string())),
    }
}

/// The right-hand id of `index <before>..<after> <mode>`.
fn head_revision(rest: &str) -> Option<String> {
    let gap = rest.find("..")?;
    let after = &rest[gap + 2..];
    let head = after.find(' ').map_or(after, |end| &after[..end]);
    (!head.is_empty()).then(|| head.to_string())
}

/// What the head has of each file, as the blob ids from a unified patch's `index <before>..<after>`
/// line. Keyed by the head's name, except for a deletion where only the old name exists. An entry
/// with no `index` line (most often one Bitbucket excluded by pattern) is left out.
pub fn parse_diff_file_revisions(patch: &str) -> BTreeMap<String, String> {
    let mut revisions = BTreeMap::new();
    let mut entry: Option<Entry> = None;
    let close = |entry: &mut Option<Entry>, revisions: &mut BTreeMap<String, String>| {
        let Some(closed) = entry.take() else { return };
        let path = if closed.deleted { closed.old_path } else { closed.new_path.or(closed.old_path) };
        if let (Some(path), Some(revision)) = (path, closed.revision)
            && !path.is_empty()
        {
            revisions.insert(path, revision);
        }
    };
    for line in patch.split('\n') {
        if let Some(rest) = line.strip_prefix(ENTRY) {
            close(&mut entry, &mut revisions);
            let (old_path, new_path) = header_paths(rest);
            entry = Some(Entry { old_path, new_path, deleted: false, revision: None, in_body: false });
            continue;
        }
        let Some(current) = entry.as_mut() else { continue };
        if current.in_body {
            continue;
        }
        if line.starts_with("@@") {
            current.in_body = true;
        } else if let Some(rest) = line.strip_prefix("index ") {
            current.revision = head_revision(rest);
        } else if line.starts_with("deleted file mode") {
            current.deleted = true;
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            current.old_path = Some(unquote_git_patch_path(rest));
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            current.new_path = Some(unquote_git_patch_path(rest));
        } else if let Some(rest) = line.strip_prefix("--- ") {
            current.old_path = side_path(rest, "a/");
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let side = side_path(rest, "b/");
            if side.is_none() {
                current.deleted = true;
            }
            current.new_path = side;
        }
    }
    close(&mut entry, &mut revisions);
    revisions
}
