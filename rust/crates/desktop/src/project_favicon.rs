//! Choosing an image file of this computer for a project's icon (`pickProjectFavicon` of `ipc/methods/window.ts`, and
//! `canPickExternalProjectFavicon` and `getLocalFileManagerName` of the web app): what the dialog filters on, when the choice is offered
//! and what the button that opens it is called. The dialog itself is the window toolkit's.
use std::path::{Path, PathBuf};

/// `WORKSPACE_IMAGE_PREVIEW_EXTENSIONS`, without the dot: what the dialog's "Images" filter lets through.
pub const IMAGE_EXTENSIONS: [&str; 8] = ["avif", "gif", "ico", "jpeg", "jpg", "png", "svg", "webp"];

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with("\\\\") || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes.len() == 2 || bytes[2] == b'/' || bytes[2] == b'\\'))
}

/// `canPickExternalProjectFavicon`: a file of this computer can name an icon of a project whose folder is on this computer. On Windows
/// that means a folder with a Windows path; a project of a WSL distribution has a Linux path this dialog's files do not match.
/// `os` is `std::env::consts::OS`.
pub fn can_pick_external(workspace_root: &str, os: &str) -> bool {
    os != "windows" || is_windows_absolute(workspace_root)
}

/// `getLocalFileManagerName`: what the button that opens the dialog calls the program that shows files.
pub fn file_manager_name(os: &str) -> &'static str {
    match os {
        "macos" => "Finder",
        "windows" => "File Explorer",
        _ => "Files",
    }
}

/// Where the dialog starts: the project's folder, when there is one on this computer.
pub fn start_directory(workspace_root: &str) -> Option<PathBuf> {
    let path = Path::new(workspace_root.trim());
    (path.is_absolute() && path.is_dir()).then(|| path.to_path_buf())
}

/// Whether a path the dialog returned is an image the filter allows (a person can still type a name past the filter on some systems).
pub fn is_image(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| IMAGE_EXTENSIONS.iter().any(|known| known.eq_ignore_ascii_case(extension)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_machine_offers_the_choice_only_for_a_folder_with_a_windows_path() {
        assert!(can_pick_external("/home/me/project", "macos"));
        assert!(can_pick_external("/home/me/project", "linux"));
        assert!(can_pick_external("C:\\work\\project", "windows"));
        assert!(can_pick_external("c:/work/project", "windows"));
        assert!(can_pick_external("\\\\server\\share\\project", "windows"));
        assert!(!can_pick_external("/home/me/project", "windows"), "a WSL distribution's folder");
        assert!(!can_pick_external("C:project", "windows"), "a drive-relative path is not absolute");
    }

    #[test]
    fn the_file_manager_is_named_as_the_operating_system_names_it() {
        assert_eq!((file_manager_name("macos"), file_manager_name("windows"), file_manager_name("linux")), ("Finder", "File Explorer", "Files"));
    }

    #[test]
    fn the_dialog_starts_in_the_project_folder_and_lets_images_through() {
        let folder = tempfile::tempdir().unwrap();
        assert_eq!(start_directory(&folder.path().to_string_lossy()).as_deref(), Some(folder.path()));
        assert_eq!(start_directory("/definitely/not/here"), None);
        assert_eq!(start_directory("relative/path"), None);
        assert!(is_image(Path::new("/a/logo.PNG")) && is_image(Path::new("icon.svg")));
        assert!(!is_image(Path::new("/a/notes.txt")) && !is_image(Path::new("/a/png")));
    }
}
