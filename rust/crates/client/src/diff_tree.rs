//! `lib/turnDiffTree.ts`: the changed-file list grouped into folders with summed line counts,
//! shared by the timeline's changed-files card and the diff panel's file tree.
use crate::{checkpoints::ChangedFile, diff::Stat};
use std::{cmp::Ordering, collections::BTreeMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Directory {
        name: String,
        path: String,
        stat: Stat,
        children: Vec<Node>,
    },
    File {
        name: String,
        path: String,
        stat: Option<Stat>,
    },
}

impl Node {
    pub fn path(&self) -> &str {
        match self {
            Node::Directory { path, .. } | Node::File { path, .. } => path,
        }
    }
}

fn stat_of(file: &ChangedFile) -> Option<Stat> {
    Some(Stat {
        additions: file.additions?,
        deletions: file.deletions?,
    })
}

/// `summarizeTurnDiffStats`: only files that report both counts contribute.
pub fn summarize(files: &[ChangedFile]) -> Stat {
    files.iter().filter_map(stat_of).fold(Stat::default(), |total, stat| Stat {
        additions: total.additions + stat.additions,
        deletions: total.deletions + stat.deletions,
    })
}

/// `localeCompare(..., {numeric: true, sensitivity: "base"})` for the names a repository has:
/// digit runs compare by value and case does not matter.
fn natural(left: &str, right: &str) -> Ordering {
    let (mut left, mut right) = (left.chars().peekable(), right.chars().peekable());
    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) if a.is_ascii_digit() && b.is_ascii_digit() => {
                let digits = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut run = String::new();
                    while let Some(digit) = chars.next_if(char::is_ascii_digit) {
                        run.push(digit);
                    }
                    run.trim_start_matches('0').to_owned()
                };
                let (a, b) = (digits(&mut left), digits(&mut right));
                match a.len().cmp(&b.len()).then_with(|| a.cmp(&b)) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(a), Some(b)) => {
                left.next();
                right.next();
                let (a, b) = (a.to_lowercase().next().unwrap_or(a), b.to_lowercase().next().unwrap_or(b));
                match a.cmp(&b) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
        }
    }
}

#[derive(Default)]
struct Folder {
    name: String,
    path: String,
    stat: Stat,
    folders: BTreeMap<String, Folder>,
    files: Vec<Node>,
}

fn nodes(folder: Folder) -> Vec<Node> {
    let mut folders: Vec<Folder> = folder.folders.into_values().collect();
    folders.sort_by(|a, b| natural(&a.name, &b.name));
    let mut files = folder.files;
    files.sort_by(|a, b| match (a, b) {
        (Node::File { name: a, .. }, Node::File { name: b, .. }) => natural(a, b),
        _ => Ordering::Equal,
    });
    folders
        .into_iter()
        .map(|folder| {
            let (name, path, stat) = (folder.name.clone(), folder.path.clone(), folder.stat);
            compact(Node::Directory {
                name,
                path,
                stat,
                children: nodes(folder),
            })
        })
        .chain(files)
        .collect()
}

/// A folder holding only one folder shows as `a/b/c`, until something branches.
fn compact(node: Node) -> Node {
    let Node::Directory { name, path, stat, mut children } = node else {
        return node;
    };
    let (mut name, mut path, mut stat) = (name, path, stat);
    while let [Node::Directory { .. }] = children.as_slice() {
        let Some(Node::Directory { name: child, path: child_path, stat: child_stat, children: grandchildren }) = children.pop() else {
            break;
        };
        name = format!("{name}/{child}");
        path = child_path;
        stat = child_stat;
        children = grandchildren;
    }
    Node::Directory { name, path, stat, children }
}

/// `buildTurnDiffTree`: folders first, then files, each sorted by name; separators of either
/// kind split paths and empty segments are dropped.
pub fn build(files: &[ChangedFile]) -> Vec<Node> {
    let mut root = Folder::default();
    for file in files {
        let normalized = file.path.replace('\\', "/");
        let segments: Vec<&str> = normalized.split('/').filter(|segment| !segment.is_empty()).collect();
        let Some((name, directories)) = segments.split_last() else {
            continue;
        };
        let stat = stat_of(file);
        let mut folder = &mut root;
        let mut prefix = String::new();
        folder.stat = add(folder.stat, stat);
        for segment in directories {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            folder = folder.folders.entry((*segment).to_owned()).or_insert_with(|| Folder {
                name: (*segment).to_owned(),
                path: prefix.clone(),
                ..Folder::default()
            });
            folder.stat = add(folder.stat, stat);
        }
        folder.files.push(Node::File {
            name: (*name).to_owned(),
            path: segments.join("/"),
            stat,
        });
    }
    nodes(root)
}

fn add(total: Stat, stat: Option<Stat>) -> Stat {
    stat.map_or(total, |stat| Stat {
        additions: total.additions + stat.additions,
        deletions: total.deletions + stat.deletions,
    })
}

/// Every folder path in the tree, so a view can start with all of them open or closed.
pub fn directory_paths(nodes: &[Node]) -> Vec<String> {
    let mut paths = Vec::new();
    for node in nodes {
        if let Node::Directory { path, children, .. } = node {
            paths.push(path.clone());
            paths.extend(directory_paths(children));
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, additions: u64, deletions: u64) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            kind: "modified".into(),
            additions: Some(additions),
            deletions: Some(deletions),
        }
    }
    fn stat(additions: u64, deletions: u64) -> Stat {
        Stat { additions, deletions }
    }
    fn leaf(name: &str, path: &str, additions: u64, deletions: u64) -> Node {
        Node::File { name: name.into(), path: path.into(), stat: Some(stat(additions, deletions)) }
    }
    fn folder(name: &str, path: &str, total: Stat, children: Vec<Node>) -> Node {
        Node::Directory { name: name.into(), path: path.into(), stat: total, children }
    }

    #[test]
    fn summaries_count_only_files_with_both_numbers() {
        let mut unknown = file("odd", 9, 9);
        unknown.deletions = None;
        assert_eq!(summarize(&[file("README.md", 3, 1), file("docs/notes.md", 0, 0), file("src/index.ts", 5, 2), unknown]), stat(8, 3));
    }

    #[test]
    fn nested_directories_aggregate_stats_and_list_folders_before_files() {
        let tree = build(&[file("src/index.ts", 2, 1), file("src/components/Button.tsx", 4, 2), file("README.md", 1, 0)]);
        assert_eq!(
            tree,
            [
                folder("src", "src", stat(6, 3), vec![folder("components", "src/components", stat(4, 2), vec![leaf("Button.tsx", "src/components/Button.tsx", 4, 2)]), leaf("index.ts", "src/index.ts", 2, 1)]),
                leaf("README.md", "README.md", 1, 0),
            ]
        );
    }

    #[test]
    fn zero_valued_stats_are_kept() {
        let tree = build(&[file("docs/notes.md", 0, 0), file("docs/todo.md", 1, 1)]);
        assert_eq!(tree, [folder("docs", "docs", stat(1, 1), vec![leaf("notes.md", "docs/notes.md", 0, 0), leaf("todo.md", "docs/todo.md", 1, 1)])]);
    }

    #[test]
    fn windows_separators_are_normalized() {
        let tree = build(&[file("apps\\web\\src\\index.ts", 2, 1)]);
        assert_eq!(tree, [folder("apps/web/src", "apps/web/src", stat(2, 1), vec![leaf("index.ts", "apps/web/src/index.ts", 2, 1)])]);
    }

    #[test]
    fn only_single_folder_chains_compact() {
        let tree = build(&[file("apps/server/src/index.ts", 2, 1), file("apps/server/main.ts", 4, 0)]);
        assert_eq!(
            tree,
            [folder("apps/server", "apps/server", stat(6, 1), vec![folder("src", "apps/server/src", stat(2, 1), vec![leaf("index.ts", "apps/server/src/index.ts", 2, 1)]), leaf("main.ts", "apps/server/main.ts", 4, 0)])]
        );
    }

    #[test]
    fn padding_in_path_segments_is_preserved() {
        let tree = build(&[file("a/file.ts", 1, 0), file(" a/file.ts", 2, 0)]);
        let mut names: Vec<_> = tree.iter().map(|node| node.path().to_owned()).collect();
        names.sort();
        assert_eq!(names, [" a", "a"]);
    }

    #[test]
    fn names_sort_naturally_and_ignore_case() {
        let tree = build(&[file("v10.txt", 1, 0), file("v2.txt", 1, 0), file("B.txt", 1, 0), file("a.txt", 1, 0)]);
        assert_eq!(tree.iter().map(|node| node.path()).collect::<Vec<_>>(), ["a.txt", "B.txt", "v2.txt", "v10.txt"]);
    }

    #[test]
    fn files_without_counts_have_no_stat_and_empty_paths_are_skipped() {
        let mut unknown = file("x.bin", 0, 0);
        unknown.additions = None;
        assert_eq!(build(&[unknown, file("//", 1, 1)]), [Node::File { name: "x.bin".into(), path: "x.bin".into(), stat: None }]);
    }

    #[test]
    fn directory_paths_list_every_folder() {
        let tree = build(&[file("a/b/c.rs", 1, 0), file("a/d.rs", 1, 0), file("e/f/g.rs", 1, 0)]);
        assert_eq!(directory_paths(&tree), ["a", "a/b", "e/f"]);
    }
}
