//! Choosing a folder on the server by typing its path: the folders under the part of the path already typed, filtered by what has been
//! typed of the next name. Port of the browse helpers of `packages/client-runtime/src/state/projects.ts` and
//! `state/filesystem.ts` (`getFilesystemBrowsePath`, `filterFilesystemBrowseEntries`) that the Add project dialog uses with
//! `filesystem.browse`. Paths are read as the server's own: a drive path or a UNC path is a Windows one, anything else a Unix one.
use serde_json::Value;

pub const METHOD: &str = "filesystem.browse";

fn is_drive_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')
}
fn is_unc_path(value: &str) -> bool {
    value.starts_with("\\\\")
}
fn is_unix_absolute(value: &str) -> bool {
    value.starts_with('/')
}

/// `isFilesystemBrowseQuery`: a path rather than a name or a search: it starts at the root, the home folder, or this folder or its parent.
/// A Windows path counts only where the server is a Windows one.
pub fn is_browse_query(value: &str, windows_server: bool) -> bool {
    value.starts_with("./") || value.starts_with("../") || value.starts_with(".\\") || value.starts_with("..\\") || value.starts_with('/') || value.starts_with("~/") || (windows_server && (is_drive_path(value) || is_unc_path(value)))
}

fn separator_of(value: &str) -> char {
    if is_drive_path(value) || is_unc_path(value) {
        '\\'
    } else if is_unix_absolute(value) {
        '/'
    } else if value.contains('\\') {
        '\\'
    } else {
        '/'
    }
}
fn has_trailing_separator(value: &str) -> bool {
    if is_unix_absolute(value) { value.ends_with('/') } else { value.ends_with('/') || value.ends_with('\\') }
}
fn last_separator(value: &str) -> Option<usize> {
    if is_unix_absolute(value) { value.rfind('/') } else { value.rfind(['/', '\\']) }
}

/// `getBrowseDirectoryPath`: the folder whose entries are listed: the path up to and including its last separator.
pub fn directory_path(current: &str) -> String {
    if has_trailing_separator(current) {
        return current.to_owned();
    }
    match last_separator(current) {
        Some(index) => current[..=index].to_owned(),
        None => current.to_owned(),
    }
}

/// `getBrowseLeafPathSegment`: what has been typed of the next name.
pub fn leaf(current: &str) -> String {
    match last_separator(current) {
        Some(index) => current[index + 1..].to_owned(),
        None => current.to_owned(),
    }
}

/// `getBrowseParentPath`: the folder above a directory path, or none at the root.
pub fn parent_path(current: &str) -> Option<String> {
    let trimmed = current.trim();
    let separator = separator_of(trimmed);
    let (root, rest) = if is_drive_path(trimmed) {
        (trimmed[..3].replace('/', "\\"), &trimmed[3..])
    } else if is_unix_absolute(trimmed) {
        ("/".to_owned(), &trimmed[1..])
    } else {
        let cut = last_separator(trimmed.trim_end_matches(['/', '\\']))?;
        return Some(trimmed[..=cut].to_owned());
    };
    let segments: Vec<&str> = rest.split(['/', '\\']).filter(|part| !part.is_empty()).collect();
    match segments.len() {
        0 => None,
        1 => Some(root),
        count => Some(format!("{root}{}{separator}", segments[..count - 1].join(&separator.to_string()))),
    }
}

/// `canNavigateUp`.
pub fn can_go_up(directory: &str) -> bool {
    has_trailing_separator(directory) && parent_path(directory).is_some()
}

/// `appendBrowsePathSegment`: the path after a folder is chosen: its name added to the directory, and a separator after it.
pub fn append_segment(current: &str, segment: &str) -> String {
    format!("{}{segment}{}", directory_path(current), separator_of(current))
}

/// Where a typed path points: whether it is being browsed, which folder to list, and what to filter its entries by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browse {
    pub directory: String,
    pub filter: String,
    pub can_go_up: bool,
    pub parent: Option<String>,
}

/// `getFilesystemBrowsePath`: none unless the text is a path.
pub fn browse_of(query: &str, windows_server: bool) -> Option<Browse> {
    if !is_browse_query(query, windows_server) {
        return None;
    }
    let directory = directory_path(query);
    Some(Browse {
        filter: if has_trailing_separator(query) { String::new() } else { leaf(query) },
        parent: parent_path(&directory),
        can_go_up: can_go_up(&directory),
        directory,
    })
}

/// `filterFilesystemBrowseEntries`: the entries whose names begin with the filter; hidden ones only when the filter begins with a dot.
pub fn visible_entries(entries: &[Value], filter: &str) -> Vec<Value> {
    let lower = filter.to_lowercase();
    let show_hidden = filter.starts_with('.');
    entries
        .iter()
        .filter(|entry| entry["name"].as_str().is_some_and(|name| name.to_lowercase().starts_with(&lower) && (show_hidden || !name.starts_with('.'))))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_path_is_browsed() {
        for path in ["/", "/home/me", "~/code", "./sub", "../up", ".\\sub"] {
            assert!(is_browse_query(path, false), "{path}");
        }
        for text in ["my-project", "C:\\work", "work/sub", ""] {
            assert!(!is_browse_query(text, false), "{text}");
        }
        assert!(is_browse_query("C:\\work", true) && is_browse_query("\\\\server\\share", true));
    }

    #[test]
    fn the_listed_folder_the_filter_and_the_way_up_follow_what_is_typed() {
        let typed = browse_of("/home/me/co", false).unwrap();
        assert_eq!((typed.directory.as_str(), typed.filter.as_str(), typed.parent.as_deref(), typed.can_go_up), ("/home/me/", "co", Some("/home/"), true));
        let finished = browse_of("/home/me/", false).unwrap();
        assert_eq!((finished.directory.as_str(), finished.filter.as_str()), ("/home/me/", ""));
        let root = browse_of("/", false).unwrap();
        assert_eq!((root.parent, root.can_go_up), (None, false));
        let top = browse_of("/home/", false).unwrap();
        assert_eq!(top.parent.as_deref(), Some("/"));
        assert_eq!(browse_of("plain", false), None);
        assert_eq!(parent_path("C:\\work\\code\\").as_deref(), Some("C:\\work\\"));
        assert_eq!(parent_path("C:\\work\\").as_deref(), Some("C:\\"));
        assert_eq!(parent_path("./sub/").as_deref(), Some("./"));
    }

    #[test]
    fn choosing_a_folder_adds_its_name_to_the_directory_and_ends_with_a_separator() {
        assert_eq!(append_segment("/home/me/co", "code"), "/home/me/code/");
        assert_eq!(append_segment("/home/me/", "code"), "/home/me/code/");
        assert_eq!(append_segment("C:\\work\\co", "code"), "C:\\work\\code\\");
    }

    #[test]
    fn entries_begin_with_the_filter_and_hidden_ones_show_only_for_a_dot() {
        let entries = [json!({"name": "code", "fullPath": "/h/code"}), json!({"name": "Config", "fullPath": "/h/Config"}), json!({"name": ".cache", "fullPath": "/h/.cache"}), json!({"name": "docs", "fullPath": "/h/docs"})];
        let names = |filter: &str| visible_entries(&entries, filter).iter().map(|entry| entry["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!(names(""), ["code", "Config", "docs"]);
        assert_eq!(names("co"), ["code", "Config"], "the filter ignores case");
        assert_eq!(names("."), [".cache"]);
        assert_eq!(names("x"), Vec::<String>::new());
    }
}
