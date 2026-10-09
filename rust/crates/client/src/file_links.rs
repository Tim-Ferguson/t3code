//! Links in a message that name a file of the workspace: what the target is, whether it can be
//! previewed in the page, and the path an editor is asked to open. A link with a scheme of its own
//! (`https:`, `mailto:`) is not one. Nothing here builds a URL: a previewed file is shown through a
//! signed asset URL the server issues for the exact resource.

/// What a file link points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTarget {
    /// The path as written, without its `file://` prefix, hash or `:line:column` suffix.
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// The kinds the page can show without an editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preview {
    Image,
    Pdf,
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
    String::from_utf8_lossy(&out).into_owned()
}

/// A link as a file target, or `None` for a web link, a mail link, an in-page anchor or nothing.
pub fn target(href: &str) -> Option<FileTarget> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let rest = match href.strip_prefix("file://") {
        Some(rest) => rest.strip_prefix("localhost").unwrap_or(rest),
        None => {
            // A scheme of its own makes it something else; a Windows drive is not one.
            let drive = href.as_bytes().get(1) == Some(&b':') && href.as_bytes()[0].is_ascii_alphabetic();
            if !drive && href.split(['/', '?', '#']).next().is_some_and(|first| first.contains(':') && first.split(':').next().is_some_and(|scheme| scheme.len() > 1 && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))) && !looks_like_line_suffix(href) {
                return None;
            }
            href
        }
    };
    let rest = rest.split('?').next().unwrap_or(rest);
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let mut path = percent_decode(rest);
    let (mut line, mut column) = (None, None);
    // `path:12` or `path:12:3`, and `#L12` / `#L12C3`.
    for _ in 0..2 {
        if let Some((head, tail)) = path.rsplit_once(':') {
            if let Ok(number) = tail.parse::<u32>() {
                if column.is_none() && line.is_none() && head.rsplit_once(':').is_some_and(|(_, middle)| middle.parse::<u32>().is_ok()) {
                    column = Some(number);
                } else if line.is_none() {
                    line = Some(number);
                } else {
                    break;
                }
                path = head.to_owned();
                continue;
            }
        }
        break;
    }
    if (line, column) == (None, None) {
        if let Some(position) = fragment.strip_prefix('L') {
            let mut numbers = position.split(|c: char| !c.is_ascii_digit()).filter(|part| !part.is_empty());
            line = numbers.next().and_then(|n| n.parse().ok());
            column = if position.contains('C') { numbers.next().and_then(|n| n.parse().ok()) } else { None };
        }
    }
    // With a column and no line the order was line:column.
    if let (Some(first), None) = (column, line) {
        line = Some(first);
        column = None;
    }
    (!path.trim().is_empty()).then_some(FileTarget { path: path.trim().to_owned(), line, column })
}
fn looks_like_line_suffix(href: &str) -> bool {
    // `src/a.rs:12` has a colon in its first segment's end only after a slash; `a.rs:12` has none to confuse with a scheme.
    href.split(':').next().is_some_and(|head| head.contains('.') || head.contains('/'))
}

/// Whether the page can show this path itself.
pub fn previewable(path: &str) -> Option<Preview> {
    let extension = path.rsplit(['/', '\\']).next()?.rsplit_once('.')?.1.to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif" | "svg" => Some(Preview::Image),
        "pdf" => Some(Preview::Pdf),
        _ => None,
    }
}

fn absolute(path: &str) -> bool {
    path.starts_with('/') || path.starts_with('\\') || (path.as_bytes().get(1) == Some(&b':') && path.as_bytes()[0].is_ascii_alphabetic())
}
/// The path inside the workspace, when the target is under `cwd` or relative to it.
pub fn workspace_relative(path: &str, cwd: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    let relative = if absolute(&path) {
        let root = cwd.replace('\\', "/");
        let root = root.trim_end_matches('/');
        path.strip_prefix(&format!("{root}/"))?.to_owned()
    } else {
        path.trim_start_matches("./").to_owned()
    };
    // Climbing out of the workspace is not inside it.
    if relative.is_empty() || relative.split('/').any(|part| part == "..") { None } else { Some(relative) }
}
/// What an editor is asked to open: the path joined to the workspace, with `:line:column` when known.
pub fn editor_path(target: &FileTarget, cwd: &str) -> String {
    let base = if absolute(&target.path) { target.path.clone() } else { format!("{}/{}", cwd.trim_end_matches(['/', '\\']), target.path.trim_start_matches("./")) };
    match (target.line, target.column) {
        (Some(line), Some(column)) => format!("{base}:{line}:{column}"),
        (Some(line), None) => format!("{base}:{line}"),
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(path: &str, line: Option<u32>, column: Option<u32>) -> Option<FileTarget> {
        Some(FileTarget { path: path.into(), line, column })
    }
    #[test]
    fn a_link_is_a_file_unless_it_goes_somewhere_else() {
        assert_eq!(target("src/lib.rs"), at("src/lib.rs", None, None));
        assert_eq!(target("src/lib.rs:42"), at("src/lib.rs", Some(42), None));
        assert_eq!(target("src/lib.rs:42:7"), at("src/lib.rs", Some(42), Some(7)));
        assert_eq!(target("./a/b%20c.png"), at("./a/b c.png", None, None));
        assert_eq!(target("file:///work/x/y.rs#L9"), at("/work/x/y.rs", Some(9), None));
        assert_eq!(target("/work/x/y.rs#L9C2"), at("/work/x/y.rs", Some(9), Some(2)));
        assert_eq!(target("C:\\work\\y.rs:3"), at("C:\\work\\y.rs", Some(3), None));
        for other in ["https://example.com/a.pdf", "http://x", "mailto:a@b.c", "javascript:alert(1)", "#section", "", "t3-context://v1/thread/x"] {
            assert_eq!(target(other), None, "{other}");
        }
    }
    #[test]
    fn only_images_and_pdfs_are_shown_in_the_page_and_only_inside_the_workspace() {
        assert_eq!(previewable("a/b.PNG"), Some(Preview::Image));
        assert_eq!(previewable("spec.pdf"), Some(Preview::Pdf));
        assert_eq!(previewable("notes.html"), None);
        assert_eq!(previewable("Makefile"), None);
        assert_eq!(workspace_relative("/work/p/docs/a.png", "/work/p"), Some("docs/a.png".into()));
        assert_eq!(workspace_relative("./docs/a.png", "/work/p"), Some("docs/a.png".into()));
        assert_eq!(workspace_relative("/etc/passwd", "/work/p"), None);
        assert_eq!(workspace_relative("../outside.png", "/work/p"), None);
        assert_eq!(workspace_relative("/work/p/../q/a.png", "/work/p"), None);
    }
    #[test]
    fn an_editor_is_given_the_joined_path_with_its_position() {
        let file = FileTarget { path: "src/a.rs".into(), line: Some(12), column: Some(3) };
        assert_eq!(editor_path(&file, "/work/p/"), "/work/p/src/a.rs:12:3");
        assert_eq!(editor_path(&FileTarget { path: "/abs/b.rs".into(), line: None, column: None }, "/work/p"), "/abs/b.rs");
    }
}
