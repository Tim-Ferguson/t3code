//! Where a project started from just a name will be made, shown before the server makes it.
//! Port of `newProjectFolderName` (`packages/shared/src/path.ts`) and `getNewProjectPathPreview`.
use icu_normalizer::DecomposingNormalizer;
use std::sync::OnceLock;

/// "Pinball Stats" becomes "pinball-stats": the name decomposed, stripped of accents, lowercased,
/// every run of other characters turned into one dash, cut to 64, and kept off Windows' reserved
/// device names. The server uses the same rule, so this is the folder it will make (it adds `-2`,
/// `-3`, ... when that folder is taken).
pub fn new_project_folder_name(name: &str) -> String {
    let decomposed = DecomposingNormalizer::new_nfkd().normalize(name);
    let lowered = decomposed.chars().filter(|c| !('\u{300}'..='\u{36f}').contains(c)).collect::<String>().to_lowercase();
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c);
        } else {
            pending_dash = true;
        }
    }
    slug.truncate(64);
    let slug = slug.trim_end_matches('-').to_owned();
    if slug.is_empty() {
        return "project".into();
    }
    static RESERVED: OnceLock<regex::Regex> = OnceLock::new();
    if RESERVED.get_or_init(|| regex::Regex::new(r"^(con|prn|aux|nul|com[1-9]|lpt[1-9])$").expect("reserved names")).is_match(&slug) { format!("{slug}-project") } else { slug }
}

fn windows_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes.len() == 2 || matches!(bytes[2], b'/' | b'\\'))) || value.starts_with("\\\\")
}
fn separator(value: &str) -> char {
    if windows_path(value) {
        '\\'
    } else if value.starts_with('/') || !value.contains('\\') {
        '/'
    } else {
        '\\'
    }
}
/// `ensureBrowseDirectoryPath`: a directory path ending in its separator.
pub fn directory(path: &str) -> String {
    let trimmed = path.trim();
    let trailing = if value_is_unix(trimmed) { trimmed.ends_with('/') } else { trimmed.ends_with('/') || trimmed.ends_with('\\') };
    if trimmed.is_empty() || trailing { trimmed.to_owned() } else { format!("{trimmed}{}", separator(trimmed)) }
}
fn value_is_unix(value: &str) -> bool {
    !windows_path(value) && value.starts_with('/')
}

/// `getNewProjectPathPreview`: the folder under the environment's new-projects root.
pub fn path_preview(root: &str, name: &str) -> String {
    let folder = new_project_folder_name(name);
    if folder.trim().is_empty() { root.to_owned() } else { format!("{}{folder}", directory(root)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn folder_names_and_previews_match_the_original() {
        let mut checked = 0;
        for line in include_str!("../tests/fixtures/new-project.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            match row["type"].as_str().unwrap() {
                "folder" => assert_eq!(new_project_folder_name(row["name"].as_str().unwrap()), row["folder"].as_str().unwrap(), "{row}"),
                "preview" => assert_eq!(path_preview(row["root"].as_str().unwrap(), row["name"].as_str().unwrap()), row["path"].as_str().unwrap(), "{row}"),
                other => panic!("unknown fixture row {other}"),
            }
            checked += 1;
        }
        assert!(checked > 100);
    }
}
