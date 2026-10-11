//! The file tree of the file panel: which rows it draws from the folders read so far, and what a
//! folder row stands for. Port of `FileBrowserPanel.tsx` (the tree model it feeds `@pierre/trees`:
//! folders first, a folder holding one folder drawn as one row `a/b/c` (`flattenEmptyDirectories`),
//! search results shown with their parents), `useDirectoryEntries.ts`, `fileTreeExpansion.ts` and
//! `serializeComposerFileLink` (`composerTrigger.ts`), the text "Copy mention" copies.
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

pub const LIST_ENTRIES: &str = "projects.listEntries";
/// The most a search names (`useProjectPathSearch` asks for a query of at most 256 characters).
pub const SEARCH_QUERY_LIMIT: usize = 256;
pub const SEARCH_LIMIT: usize = 200;
/// Folders read at once (`running.current >= 4`).
pub const PARALLEL_READS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    pub directory: bool,
    pub ignored: bool,
}
impl Entry {
    pub fn from_json(value: &Value) -> Option<Self> {
        Some(Self { path: value["path"].as_str()?.to_owned(), directory: value["kind"] == "directory", ignored: value["ignored"] == true })
    }
}

pub fn list_input(cwd: &str, directory: &str) -> Value {
    json!({"cwd":cwd,"directoryPath":directory})
}
pub fn search_input(cwd: &str, query: &str) -> Value {
    json!({"cwd":cwd,"query":query.chars().take(SEARCH_QUERY_LIMIT).collect::<String>(),"limit":SEARCH_LIMIT})
}

fn parent_of(path: &str) -> &str {
    path.rfind('/').map_or("", |at| &path[..at])
}
/// What a folder read keeps: the entries directly inside it (`entry.path`'s parent is the folder).
pub fn children_of(directory: &str, entries: &[Value]) -> Vec<Entry> {
    entries.iter().filter_map(Entry::from_json).filter(|entry| parent_of(&entry.path) == directory).collect()
}

/// The folders above a path, outermost first.
pub fn ancestors(path: &str) -> Vec<String> {
    let mut result = Vec::new();
    for (at, _) in path.match_indices('/') {
        result.push(path[..at].to_owned());
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The path the row stands for: for a chain of single folders, the innermost.
    pub path: String,
    /// Every folder the row stands for, outermost first; a file's is its own path. A chain is open when any of it is, which
    /// keeps a folder open when reading it shows it holds one folder only.
    pub chain: Vec<String>,
    pub label: String,
    pub depth: usize,
    pub directory: bool,
    pub ignored: bool,
    pub expanded: bool,
    /// A folder whose contents were read; an unread one shows what a click will fetch.
    pub loaded: bool,
}

fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
fn sort(entries: &mut [&Entry]) {
    entries.sort_by(|left, right| right.directory.cmp(&left.directory).then_with(|| name_of(&left.path).to_lowercase().cmp(&name_of(&right.path).to_lowercase())).then_with(|| left.path.cmp(&right.path)));
}

/// The rows to draw: the root's entries, and under each expanded folder its entries.
pub fn rows(directories: &BTreeMap<String, Vec<Entry>>, expanded: &BTreeSet<String>) -> Vec<Row> {
    let mut out = Vec::new();
    visit("", 0, directories, expanded, &mut out);
    out
}
fn visit(directory: &str, depth: usize, directories: &BTreeMap<String, Vec<Entry>>, expanded: &BTreeSet<String>, out: &mut Vec<Row>) {
    let Some(entries) = directories.get(directory) else { return };
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sort(&mut sorted);
    for entry in sorted {
        if !entry.directory {
            out.push(Row { path: entry.path.clone(), chain: vec![entry.path.clone()], label: name_of(&entry.path).to_owned(), depth, directory: false, ignored: entry.ignored, expanded: false, loaded: true });
            continue;
        }
        // A folder holding one folder is one row.
        let mut path = entry.path.clone();
        let mut label = name_of(&path).to_owned();
        let mut ignored = entry.ignored;
        let mut chain = vec![path.clone()];
        while let Some([only]) = directories.get(&path).map(Vec::as_slice) {
            if !only.directory {
                break;
            }
            label = format!("{label}/{}", name_of(&only.path));
            ignored = ignored || only.ignored;
            path = only.path.clone();
            chain.push(path.clone());
        }
        let open = chain.iter().any(|folder| expanded.contains(folder));
        out.push(Row { path: path.clone(), chain, label, depth, directory: true, ignored, expanded: open, loaded: directories.contains_key(&path) });
        if open {
            visit(&path, depth + 1, directories, expanded, out);
        }
    }
}

/// The open folders drawn that have not been asked for yet: what opening a row, or a chain growing under it, has to read.
pub fn to_read(directories: &BTreeMap<String, Vec<Entry>>, expanded: &BTreeSet<String>, requested: &BTreeSet<String>) -> Vec<String> {
    rows(directories, expanded).into_iter().filter(|row| row.directory && row.expanded && !requested.contains(&row.path)).map(|row| row.path).collect()
}

/// Search results as a tree: each match under its folders, all of them open (`hide-non-matches`).
pub fn search_tree(matches: &[Entry]) -> (BTreeMap<String, Vec<Entry>>, BTreeSet<String>) {
    let mut by_path: BTreeMap<String, Entry> = BTreeMap::new();
    for entry in matches {
        by_path.entry(entry.path.clone()).or_insert_with(|| entry.clone());
        for folder in ancestors(&entry.path) {
            by_path.entry(folder.clone()).or_insert(Entry { path: folder, directory: true, ignored: false });
        }
    }
    let mut directories: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for entry in by_path.values() {
        directories.entry(parent_of(&entry.path).to_owned()).or_default().push(entry.clone());
        if entry.directory {
            directories.entry(entry.path.clone()).or_default();
        }
    }
    let expanded = directories.keys().filter(|path| !path.is_empty()).cloned().collect();
    (directories, expanded)
}

/// Every folder the tree knows (`directoryPaths`), which "Expand all folders" opens.
pub fn known_directories(directories: &BTreeMap<String, Vec<Entry>>) -> BTreeSet<String> {
    directories.values().flatten().filter(|entry| entry.directory).map(|entry| entry.path.clone()).collect()
}
/// `areAllDirectoriesExpanded`.
pub fn all_expanded(directories: &BTreeMap<String, Vec<Entry>>, expanded: &BTreeSet<String>) -> bool {
    let known = known_directories(directories);
    !known.is_empty() && known.iter().all(|path| expanded.contains(path))
}

/// The folders to read next after the ones just opened: a chosen file's folders, outermost first, that are not read yet.
pub fn unread_ancestors(path: &str, directories: &BTreeMap<String, Vec<Entry>>) -> Vec<String> {
    std::iter::once(String::new()).chain(ancestors(path)).filter(|folder| !directories.contains_key(folder)).collect()
}

fn encode_destination(path: &str) -> String {
    let mut out = String::new();
    for byte in path.bytes() {
        // `encodeURI` leaves these alone; `(`, `)`, `#`, `?` and `\` are then encoded by `encodeMarkdownLinkDestination`.
        if byte.is_ascii_alphanumeric() || b";,/:@&=+$-_.!~*'".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
/// `serializeComposerFileLink`: `[name](path)` with the label and the destination escaped for Markdown.
pub fn mention(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let label = name.replace('\\', "\\\\").replace('[', "\\[").replace(']', "\\]");
    format!("[{label}]({})", encode_destination(path))
}

/// The prompt after "Add to chat": the mention at the end, a space before it unless one is there (`ensureLeadingBoundary`).
/// This composer names a file as `@path`, as its own `@` menu does.
pub fn add_to_prompt(prompt: &str, path: &str) -> String {
    let boundary = if prompt.is_empty() || prompt.ends_with(char::is_whitespace) { "" } else { " " };
    format!("{prompt}{boundary}@{path} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, directory: bool) -> Entry {
        Entry { path: path.into(), directory, ignored: false }
    }
    fn tree(read: &[(&str, Vec<Entry>)]) -> BTreeMap<String, Vec<Entry>> {
        read.iter().map(|(directory, entries)| ((*directory).to_owned(), entries.clone())).collect()
    }
    fn shown(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|row| format!("{}{}{}", "  ".repeat(row.depth), row.label, if row.directory { "/" } else { "" })).collect()
    }

    #[test]
    fn folders_come_first_and_only_an_open_folder_shows_what_it_holds() {
        let read = tree(&[("", vec![entry("zeta.txt", false), entry("src", true), entry("Alpha.md", false), entry("docs", true)]), ("src", vec![entry("src/main.rs", false), entry("src/util", true)])]);
        assert_eq!(shown(&rows(&read, &BTreeSet::new())), vec!["docs/", "src/", "Alpha.md", "zeta.txt"]);
        let open = BTreeSet::from(["src".to_owned()]);
        assert_eq!(shown(&rows(&read, &open)), vec!["docs/", "src/", "  util/", "  main.rs", "Alpha.md", "zeta.txt"]);
        let docs = rows(&read, &BTreeSet::new()).into_iter().find(|row| row.path == "docs").unwrap();
        assert!(!docs.loaded, "a folder not read yet is fetched by opening it");
    }

    #[test]
    fn a_folder_holding_one_folder_is_one_row() {
        let read = tree(&[("", vec![entry("a", true)]), ("a", vec![entry("a/b", true)]), ("a/b", vec![entry("a/b/c", true)]), ("a/b/c", vec![entry("a/b/c/x.txt", false), entry("a/b/c/y.txt", false)])]);
        let closed = rows(&read, &BTreeSet::new());
        assert_eq!(shown(&closed), vec!["a/b/c/"]);
        assert_eq!(closed[0].path, "a/b/c");
        let open = BTreeSet::from(["a/b/c".to_owned()]);
        assert_eq!(shown(&rows(&read, &open)), vec!["a/b/c/", "  x.txt", "  y.txt"]);
        // A folder with two entries is a row of its own.
        let two = tree(&[("", vec![entry("a", true)]), ("a", vec![entry("a/b", true), entry("a/f", false)])]);
        assert_eq!(shown(&rows(&two, &BTreeSet::new())), vec!["a/"]);
    }

    #[test]
    fn a_folder_opened_before_it_was_read_stays_open_when_it_turns_out_to_hold_one_folder() {
        let mut read = tree(&[("", vec![entry("src", true)])]);
        let open = BTreeSet::from(["src".to_owned()]);
        assert_eq!(to_read(&read, &open, &BTreeSet::from(["".to_owned()])), vec!["src".to_owned()]);
        read.insert("src".into(), vec![entry("src/ui", true)]);
        let requested = BTreeSet::from(["".to_owned(), "src".to_owned()]);
        let drawn = rows(&read, &open);
        assert_eq!((shown(&drawn), drawn[0].expanded, drawn[0].chain.clone()), (vec!["src/ui/".to_owned()], true, vec!["src".to_owned(), "src/ui".to_owned()]));
        assert_eq!(to_read(&read, &open, &requested), vec!["src/ui".to_owned()], "the folder it grew into is read next");
        assert!(to_read(&read, &open, &BTreeSet::from(["".to_owned(), "src".to_owned(), "src/ui".to_owned()])).is_empty());
    }

    #[test]
    fn a_read_keeps_only_the_entries_directly_inside() {
        let answer = vec![json!({"path":"src/a.rs","kind":"file"}), json!({"path":"src/deep/b.rs","kind":"file"}), json!({"path":"src/deep","kind":"directory","ignored":true})];
        assert_eq!(children_of("src", &answer), vec![entry("src/a.rs", false), Entry { path: "src/deep".into(), directory: true, ignored: true }]);
        assert_eq!(children_of("", &[json!({"path":"top.md","kind":"file"})]), vec![entry("top.md", false)]);
    }

    #[test]
    fn search_results_show_with_their_folders_all_open() {
        let (directories, open) = search_tree(&[entry("src/ui/button.rs", false), entry("src/ui", true), entry("README.md", false)]);
        assert_eq!(shown(&rows(&directories, &open)), vec!["src/ui/", "  button.rs", "README.md"]);
    }

    #[test]
    fn opening_a_file_names_the_folders_still_to_read() {
        let read = tree(&[("", vec![entry("src", true)]), ("src", vec![entry("src/ui", true)])]);
        assert_eq!(unread_ancestors("src/ui/button.rs", &read), vec!["src/ui".to_owned()]);
        assert_eq!(unread_ancestors("top.md", &BTreeMap::new()), vec![String::new()]);
        assert_eq!(ancestors("a/b/c.txt"), vec!["a".to_owned(), "a/b".to_owned()]);
    }

    #[test]
    fn expanding_all_means_every_known_folder_is_open() {
        let read = tree(&[("", vec![entry("a", true), entry("f", false)]), ("a", vec![entry("a/b", true)])]);
        assert!(!all_expanded(&read, &BTreeSet::from(["a".to_owned()])));
        assert!(all_expanded(&read, &known_directories(&read)));
        assert!(!all_expanded(&BTreeMap::new(), &BTreeSet::new()));
    }

    #[test]
    fn a_mention_is_the_original_composer_link() {
        assert_eq!(mention("src/lib/a b.ts"), "[a b.ts](src/lib/a%20b.ts)");
        assert_eq!(mention("docs/notes (v2) #1.md"), "[notes (v2) #1.md](docs/notes%20%28v2%29%20%231.md)");
        assert_eq!(mention("a/[x].md"), "[\\[x\\].md](a/%5Bx%5D.md)");
        assert_eq!(mention("é.txt"), "[é.txt](%C3%A9.txt)");
    }

    #[test]
    fn add_to_chat_leaves_a_boundary_before_the_mention() {
        assert_eq!(add_to_prompt("", "a.rs"), "@a.rs ");
        assert_eq!(add_to_prompt("look at", "a.rs"), "look at @a.rs ");
        assert_eq!(add_to_prompt("look at ", "a.rs"), "look at @a.rs ");
    }
}
